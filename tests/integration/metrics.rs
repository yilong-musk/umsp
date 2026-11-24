use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::{Json, Router};
use serde_json::{Value, json};
use tower::ServiceExt;

use umsp::config::{Backend, Model, Provider};
use umsp::server::{AppState, build_router};

use crate::common::*;

// ---------------------------------------------------------------------------
// M5: Prometheus /metrics
// ---------------------------------------------------------------------------

fn metrics_request() -> Request<Body> {
    Request::builder()
        .uri("/metrics")
        .body(Body::empty())
        .unwrap()
}

async fn metrics_text(app: Router) -> String {
    let response = app.oneshot(metrics_request()).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["content-type"],
        "text/plain; version=0.0.4; charset=utf-8"
    );
    String::from_utf8(body_bytes(response).await).unwrap()
}

/// Sum of every series of a counter, e.g. all `umsp_requests_total{...}` lines.
fn counter_sum(text: &str, name: &str) -> u64 {
    let prefix = format!("{name}{{");
    text.lines()
        .filter(|line| line.starts_with(&prefix))
        .filter_map(|line| line.rsplit(' ').next()?.parse::<u64>().ok())
        .sum()
}

#[tokio::test]
async fn metrics_need_no_key_and_match_the_ledger() {
    let dir = temp_dir("metrics-basic");
    let (app, _, rec) = gateway(&dir, 50 * 1024 * 1024).await;

    // No Authorization header: admin endpoints sit outside client auth.
    let before = metrics_text(app.clone()).await;
    assert!(before.contains("# TYPE umsp_requests_total counter"));
    assert!(before.contains("# TYPE umsp_request_duration_seconds histogram"));
    assert!(before.contains("umsp_circuit_open{provider=\"od\"} 0"));
    assert_eq!(counter_sum(&before, "umsp_requests_total"), 0);
    assert!(
        rec.seen.lock().unwrap().is_empty(),
        "/metrics must never call an upstream"
    );

    let response = app.clone().oneshot(chat_request("normal")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let records = wait_records(&dir, 1).await;

    let text = metrics_text(app).await;
    assert!(text.contains(
        "umsp_requests_total{provider=\"od\",model=\"upstream-model\",protocol=\"chat\",status=\"200\",stream=\"false\"} 1"
    ));
    assert!(text.contains(
        "umsp_request_duration_seconds_count{provider=\"od\",model=\"upstream-model\",protocol=\"chat\"} 1"
    ));
    assert!(text.contains(
        "umsp_tokens_total{provider=\"od\",model=\"upstream-model\",direction=\"input\"} 10"
    ));
    assert!(text.contains("direction=\"output\"} 20"));
    assert!(text.contains("direction=\"cached\"} 4"));
    assert!(text.contains("direction=\"reasoning\"} 6"));
    // No client name, client key or request id in the exposition.
    assert!(!text.contains(TEST_KEY));
    assert!(!text.contains("tester"));
    assert!(!text.contains("umsp_testkey"));
    // One finished request = one series increment = one ledger row.
    assert_eq!(
        counter_sum(&text, "umsp_requests_total"),
        records.len() as u64
    );
    assert_eq!(records.len(), 1);
}

#[tokio::test]
async fn metrics_count_a_fallback_once_under_the_serving_provider() {
    let dir = temp_dir("metrics-fallback");
    let (app, fake) = m4b_gateway(&dir, true).await;
    fake.queue("upstream-model", &[502]);

    let response = app
        .clone()
        .oneshot(chat_model("upstream-model", false))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let records = wait_records(&dir, 1).await;

    let text = metrics_text(app).await;
    // One finished request, accounted under the provider that answered.
    assert!(text.contains(
        "umsp_requests_total{provider=\"deepseek\",model=\"upstream-model\",protocol=\"chat\",status=\"200\",stream=\"false\"} 1"
    ));
    assert!(
        !text.contains("umsp_requests_total{provider=\"od\""),
        "the absorbed primary attempt must not count as a second request"
    );
    assert_eq!(
        counter_sum(&text, "umsp_requests_total"),
        records.len() as u64
    );
    // The absorbed 502 still shows up as an upstream error of the primary.
    assert!(text.contains("umsp_upstream_errors_total{provider=\"od\",code=\"upstream_502\"} 1"));
}

#[tokio::test]
async fn metrics_record_stream_ttft_and_return_inflight_to_zero() {
    let dir = temp_dir("metrics-stream");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;

    let response = app.clone().oneshot(stream_request("sse")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    // Draining the body runs the pump to completion, which writes the ledger.
    let body = body_bytes(response).await;
    assert!(body.ends_with(b"[DONE]\n\n"));
    let records = wait_records(&dir, 1).await;
    assert!(records[0].ttft_ms.is_some(), "stream must record a TTFT");

    let text = metrics_text(app).await;
    assert!(text.contains("stream=\"true\""));
    assert!(text.contains(
        "umsp_ttft_seconds_count{provider=\"od\",model=\"upstream-model\",protocol=\"chat\"} 1"
    ));
    assert!(text.contains("umsp_request_duration_seconds_count{provider=\"od\""));
    // The in-flight gauge drops back to zero and its series disappears.
    assert!(
        !text.contains("umsp_inflight_requests{"),
        "inflight gauge leaked: {text}"
    );
}

#[tokio::test]
async fn oauth_accounts_send_independent_session_headers() {
    let dir = temp_dir("oauth-headers");
    let (upstream, rec) = start_upstream().await;
    let mut cfg = config(&dir, upstream, 50 * 1024 * 1024);
    let base = cfg.providers[0].base_url.clone();
    for id in ["grok", "grok-2"] {
        cfg.providers.push(Provider {
            id: id.into(),
            vendor: Some("grok".into()),
            base_url: base.clone(),
            anthropic_base_url: None,
            api_key_env: String::new(),
            auth: "oauth".into(),
            protocols: vec!["responses".into()],
            quirks: None,
        });
    }
    cfg.models.push(Model {
        id: "grok-a".into(),
        backends: vec![Backend {
            provider: "grok".into(),
            upstream: "grok-4.7".into(),
        }],
        aliases: vec![],
        context_length: 256_000,
        context_lengths: Vec::new(),
        max_completion_tokens: 0,
        capabilities: vec!["text".into()],
        display_name: None,
        created: 1_789_026_056,
        description: None,
    });
    cfg.models.push(Model {
        id: "grok-b".into(),
        backends: vec![Backend {
            provider: "grok-2".into(),
            upstream: "grok-4.7".into(),
        }],
        aliases: vec![],
        context_length: 256_000,
        context_lengths: Vec::new(),
        max_completion_tokens: 0,
        capabilities: vec!["text".into()],
        display_name: None,
        created: 1_789_026_056,
        description: None,
    });
    let sessions = dir.join("sessions");
    umsp::sessions::write_session(
        &sessions,
        "grok",
        "token-account-a",
        "refresh-a",
        "2099-01-01T00:00:00Z",
    )
    .unwrap();
    umsp::sessions::write_session(
        &sessions,
        "grok-2",
        "token-account-b",
        "refresh-b",
        "2099-01-01T00:00:00Z",
    )
    .unwrap();
    let state = AppState::with_sessions(cfg, env_file(&dir), sessions).unwrap();
    let app = build_router(state);

    let response = app.clone().oneshot(chat_request("normal")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response = app
        .clone()
        .oneshot(responses_named("grok-a"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response = app.oneshot(responses_named("grok-b")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let seen = rec.headers.lock().unwrap().clone();
    assert_eq!(seen.len(), 3);
    let auth = |i: usize| {
        seen[i]
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    };
    let hdr = |i: usize, name: &str| {
        seen[i]
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    };
    assert_eq!(auth(0), "Bearer sk-test");
    assert!(hdr(0, "x-xai-token-auth").is_empty());
    assert_eq!(auth(1), "Bearer token-account-a");
    assert_eq!(hdr(1, "x-xai-token-auth"), "xai-grok-cli");
    assert_eq!(hdr(1, "x-grok-model-override"), "grok-4.7");
    assert_eq!(
        hdr(1, "x-grok-client-version"),
        umsp::sessions::GROK_CLI_VERSION
    );
    assert_eq!(auth(2), "Bearer token-account-b");
    assert_eq!(hdr(2, "x-xai-token-auth"), "xai-grok-cli");
    assert_eq!(hdr(2, "x-grok-model-override"), "grok-4.7");
    assert_eq!(
        hdr(2, "x-grok-client-version"),
        umsp::sessions::GROK_CLI_VERSION
    );
    assert!(
        !std::fs::read_to_string(dir.join("sessions/grok.json"))
            .unwrap()
            .contains("token-account-b")
    );
}

#[tokio::test]
async fn registered_model_accepts_every_protocol_against_responses_only() {
    let dir = temp_dir("translate-responses-only");
    let (upstream, rec) = start_upstream().await;
    let mut cfg = config(&dir, upstream, 50 * 1024 * 1024);
    cfg.providers[0].protocols = vec!["responses".into()];
    cfg.providers[0].quirks = None;
    let state = AppState::new(cfg, env_file(&dir)).unwrap();
    let app = build_router(state);

    let chat = app.clone().oneshot(chat_request("normal")).await.unwrap();
    assert_eq!(chat.status(), StatusCode::OK);
    let chat_body = body_json(chat).await;
    assert_eq!(chat_body["object"], "chat.completion");
    assert_eq!(chat_body["model"], "upstream-model");
    assert_eq!(chat_body["choices"][0]["message"]["content"], "hi");
    assert_eq!(chat_body["usage"]["prompt_tokens"], 11);
    let seen = rec.last();
    assert!(
        seen.get("input").is_some(),
        "upstream saw a responses body: {seen}"
    );
    assert!(seen.get("messages").is_none(), "{seen}");
    assert_eq!(seen["model"], "upstream-model");

    let messages = app
        .clone()
        .oneshot(post_to(
            "/v1/messages",
            json!({
                "model": "upstream-model",
                "max_tokens": 32,
                "system": "be brief",
                "messages": [{"role": "user", "content": "hello"}],
                "stream": false
            }),
            "normal",
        ))
        .await
        .unwrap();
    assert_eq!(messages.status(), StatusCode::OK);
    let messages_body = body_json(messages).await;
    assert_eq!(messages_body["type"], "message");
    assert_eq!(messages_body["content"][0]["text"], "hi");
    assert_eq!(messages_body["stop_reason"], "end_turn");
    let seen = rec.last();
    assert_eq!(seen["instructions"], "be brief");
    assert_eq!(seen["max_output_tokens"], 32);

    let stream = app.oneshot(stream_request("sse")).await.unwrap();
    assert_eq!(stream.status(), StatusCode::OK);
    let bytes = body_bytes(stream).await;
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("\"content\":\"hi\""), "{text}");
    assert!(text.contains("data: [DONE]"), "{text}");

    let records = wait_records(&dir, 3).await;
    assert!(
        records.iter().all(|record| {
            record
                .quirks
                .iter()
                .any(|quirk| quirk.starts_with("translated:"))
        }),
        "{:?}",
        records
            .iter()
            .map(|record| &record.quirks)
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn higher_priority_responses_provider_is_tried_before_a_native_chat_backend() {
    let dir = temp_dir("translate-priority");
    let (root, fake) = start_scripted().await;
    let mut cfg = config(&dir, format!("{root}/v1"), 50 * 1024 * 1024);
    cfg.providers[0].base_url = format!("{root}/v1");
    cfg.providers.push(Provider {
        id: "responses-only".into(),
        vendor: Some("open-design".into()),
        base_url: format!("{root}/v1"),
        anthropic_base_url: None,
        api_key_env: "TEST_OD_KEY".into(),
        auth: "api_key".into(),
        protocols: vec!["responses".into()],
        quirks: None,
    });
    cfg.models[0].backends = vec![
        Backend {
            provider: "responses-only".into(),
            upstream: "only-model".into(),
        },
        Backend {
            provider: "od".into(),
            upstream: "upstream-model".into(),
        },
    ];
    fake.queue("only-model", &[502]);
    let state = AppState::new(cfg, env_file(&dir)).unwrap();
    let app = build_router(state);
    let response = app.oneshot(chat_request("normal")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["choices"][0]["message"]["content"], "ok");
    assert_eq!(
        fake.hits(),
        vec!["only-model".to_string(), "upstream-model".to_string()]
    );
    let records = wait_records(&dir, 1).await;
    assert_eq!(records[0].provider, "od");
    assert!(
        records[0]
            .quirks
            .iter()
            .any(|quirk| quirk == "fallback_from:responses-only"),
        "{:?}",
        records[0].quirks
    );
}

fn responses_named(model: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/responses")
        .header("authorization", format!("Bearer {TEST_KEY}"))
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "model": model,
                "input": "ping",
                "stream": false
            }))
            .unwrap(),
        ))
        .unwrap()
}

// ---------------------------------------------------------------------------
// Grok weekly billing producer
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct BillingCall {
    path: String,
    authorization: String,
    user_id: Option<String>,
}

#[derive(Clone, Default)]
struct BillingFake {
    seen: Arc<Mutex<Vec<BillingCall>>>,
}

async fn billing_user(
    State(fake): State<BillingFake>,
    headers: axum::http::HeaderMap,
) -> Json<Value> {
    fake.seen.lock().unwrap().push(BillingCall {
        path: "/v1/user".into(),
        authorization: header(&headers, "authorization").unwrap_or_default(),
        user_id: header(&headers, "x-userid"),
    });
    Json(json!({"xUserId": "2011310503512195073"}))
}

async fn billing_credits(
    State(fake): State<BillingFake>,
    headers: axum::http::HeaderMap,
) -> Json<Value> {
    fake.seen.lock().unwrap().push(BillingCall {
        path: "/v1/billing".into(),
        authorization: header(&headers, "authorization").unwrap_or_default(),
        user_id: header(&headers, "x-userid"),
    });
    Json(json!({
        "config": {
            "currentPeriod": {
                "type": "USAGE_PERIOD_TYPE_WEEKLY",
                "start": "2026-11-19T08:33:29.430+00:00",
                "end": "2099-11-26T08:33:29.430+00:00"
            },
            "creditUsagePercent": 44.0,
            "productUsage": [{"product": "GrokBuild", "usagePercent": 44.0}],
            "isUnifiedBillingUser": true
        }
    }))
}

fn header(headers: &axum::http::HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

#[tokio::test]
async fn grok_billing_reads_the_weekly_pool_and_user_id() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let fake = BillingFake::default();
    let app = Router::new()
        .route("/v1/user", axum::routing::get(billing_user))
        .route("/v1/billing", axum::routing::get(billing_credits))
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let dir = temp_dir("grok-billing");
    let sessions_dir = dir.join("sessions");
    std::fs::create_dir_all(&sessions_dir).unwrap();
    umsp::sessions::write_session(
        &sessions_dir,
        "grok",
        "session-token",
        "refresh-token",
        "2099-01-01T00:00:00Z",
    )
    .unwrap();
    let store = umsp::sessions::SessionStore::new(sessions_dir, reqwest::Client::new());
    let provider = Provider {
        id: "grok".into(),
        vendor: Some("grok".into()),
        base_url: format!("http://{addr}/v1"),
        anthropic_base_url: None,
        api_key_env: String::new(),
        auth: "oauth".into(),
        protocols: vec!["responses".into()],
        quirks: None,
    };

    let (usage, user_id) =
        umsp::grok_billing::fetch_usage(&reqwest::Client::new(), &store, &provider, "1.0.46", None)
            .await
            .unwrap();
    assert!((usage.remaining_fraction - 0.56).abs() < 1e-9);
    assert_eq!(usage.used_percent, 44.0);
    assert_eq!(user_id, "2011310503512195073");

    let seen = fake.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].path, "/v1/user");
    assert_eq!(seen[0].user_id, None);
    assert_eq!(seen[1].path, "/v1/billing");
    assert_eq!(seen[1].user_id.as_deref(), Some("2011310503512195073"));
    assert_eq!(seen[1].authorization, "Bearer session-token");
}
