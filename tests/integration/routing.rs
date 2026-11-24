use std::time::{Duration, Instant};

use axum::http::StatusCode;
use serde_json::Value;
use tower::ServiceExt;

use umsp::config::Backend;
use umsp::route::UsageWindow;
use umsp::server::{AppState, build_router};

use crate::common::*;

#[tokio::test]
async fn fallback_triggers_on_retryable_statuses() {
    for status in [500u16, 429, 402, 503] {
        let dir = temp_dir(&format!("fb-{status}"));
        let (app, fake) = m4b_gateway(&dir, true).await;
        fake.queue("upstream-model", &[status]);

        let response = app
            .oneshot(chat_model("upstream-model", false))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "upstream {status} must fall back to the same protocol"
        );
        assert!(
            !response.headers().contains_key("x-umsp-provider"),
            "provider header must not leak to clients"
        );
        assert!(
            !response.headers().contains_key("x-umsp-upstream-model"),
            "upstream model header must not leak"
        );
        let value = body_json(response).await;
        assert_eq!(value["_served_by"], "fallback-model");
        assert_eq!(
            fake.hits(),
            vec!["upstream-model".to_string(), "fallback-model".to_string()]
        );

        // The ledger records the provider that actually answered, one hop.
        let records = wait_records(&dir, 1).await;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].status, 200);
        assert_eq!(records[0].provider, "deepseek");
        assert_eq!(records[0].upstream_model, "fallback-model");
        assert_eq!(records[0].fallbacks, 1);
        assert!(
            records[0]
                .quirks
                .iter()
                .any(|quirk| quirk == "fallback_from:od"),
            "missing fallback_from quirk: {:?}",
            records[0].quirks
        );
    }
}

#[tokio::test]
async fn fallback_triggers_on_connection_error() {
    let dir = temp_dir("fb-conn");
    // Nothing listens on port 1: the primary attempt fails before headers.
    let (app, fake) = m4b_gateway_with(&dir, Some("http://127.0.0.1:1/v1".into()), true).await;

    let response = app
        .oneshot(chat_model("upstream-model", false))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        !response.headers().contains_key("x-umsp-provider"),
        "provider header must not leak to clients"
    );
    assert_eq!(fake.hits(), vec!["fallback-model".to_string()]);
}

#[tokio::test]
async fn client_errors_do_not_fallback() {
    for status in [400u16, 404, 422] {
        let dir = temp_dir(&format!("fb-no-{status}"));
        let (app, fake) = m4b_gateway(&dir, true).await;
        fake.queue("upstream-model", &[status]);

        let response = app
            .oneshot(chat_model("upstream-model", false))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::from_u16(status).unwrap());
        let value = body_json(response).await;
        assert_eq!(
            value["error"]["message"],
            format!("upstream-model says {status}")
        );
        assert_eq!(
            fake.hits(),
            vec!["upstream-model".to_string()],
            "upstream {status} must not trigger a fallback"
        );
    }
}

#[tokio::test]
async fn failed_fallback_returns_the_last_upstream_error() {
    let dir = temp_dir("fb-both-fail");
    let (app, fake) = m4b_gateway(&dir, true).await;
    fake.queue("upstream-model", &[500]);
    fake.queue("fallback-model", &[502]);

    let response = app
        .oneshot(chat_model("upstream-model", false))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert!(
        !response.headers().contains_key("x-umsp-provider"),
        "provider header must not leak to clients"
    );
    let value = body_json(response).await;
    assert_eq!(
        value["error"]["message"],
        "the upstream service failed to handle the request"
    );
    assert_eq!(value["error"]["code"], "upstream_error");

    let records = wait_records(&dir, 1).await;
    assert_eq!(records[0].status, 502);
    assert_eq!(records[0].upstream_status, Some(502));
    assert_eq!(records[0].provider, "deepseek");
    assert_eq!(records[0].error_code.as_deref(), Some("upstream_502"));
    assert!(
        records[0]
            .upstream_body
            .as_deref()
            .unwrap_or("")
            .contains("fallback-model says 502"),
        "the raw upstream body stays available in the ledger"
    );
    assert_eq!(records[0].fallbacks, 1);
}

#[tokio::test]
async fn stream_failure_after_200_does_not_fallback() {
    let dir = temp_dir("fb-stream");
    let (app, fake) = m4b_gateway(&dir, true).await;
    fake.die_in_stream();

    let response = app
        .oneshot(chat_model("upstream-model", true))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let _ = body_bytes(response).await;
    assert_eq!(
        fake.hits(),
        vec!["upstream-model".to_string()],
        "a stream that already sent 200 must not switch provider"
    );
}

#[tokio::test]
async fn five_failures_open_the_circuit_and_reject_with_503() {
    let dir = temp_dir("circuit-open");
    let (app, fake) = m4b_gateway(&dir, false).await;
    fake.queue("upstream-model", &[500; 5]);

    for _ in 0..5 {
        let response = app
            .clone()
            .oneshot(chat_model("upstream-model", false))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    }

    let health = body_json(app.clone().oneshot(healthz_request()).await.unwrap()).await;
    assert_eq!(health["status"], "degraded");
    assert_eq!(health["providers"]["od"]["circuit"], "open");
    assert_eq!(health["providers"]["od"]["failures"], 5);
    assert!(health["providers"]["od"]["opened_at"].is_string());
    assert_eq!(health["providers"]["od"]["reason"], "http_500");

    let response = app
        .oneshot(chat_model("upstream-model", false))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.headers()["x-umsp-code"], "temporarily_unavailable");
    assert!(
        !response.headers().contains_key("x-umsp-provider"),
        "provider header must not leak to clients"
    );
    let value = body_json(response).await;
    assert_eq!(value["error"]["code"], "temporarily_unavailable");
    assert_eq!(
        fake.hits_of("upstream-model"),
        5,
        "an open circuit must not touch the upstream"
    );
}

#[tokio::test]
async fn not_found_never_opens_the_circuit() {
    let dir = temp_dir("circuit-404");
    let (app, fake) = m4b_gateway(&dir, false).await;
    fake.queue("upstream-model", &[404; 5]);

    for _ in 0..5 {
        let response = app
            .clone()
            .oneshot(chat_model("upstream-model", false))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
    let health = body_json(app.oneshot(healthz_request()).await.unwrap()).await;
    assert_eq!(health["providers"]["od"]["circuit"], "closed");
    assert_eq!(health["providers"]["od"]["failures"], 0);
}

#[tokio::test]
async fn a_spent_usage_window_skips_the_only_provider() {
    let dir = temp_dir("usage-spent");
    let (upstream, rec) = start_upstream().await;
    let cfg = config(&dir, upstream, 52_428_800);
    let state = AppState::new(cfg, env_file(&dir)).unwrap();
    let now = Instant::now();
    state.note_provider_usage(
        "od",
        UsageWindow {
            duration: Duration::from_secs(7 * 86400),
            remaining_fraction: 0.0,
            reset_at: Some(now + Duration::from_secs(3600)),
            observed_at: now,
        },
    );
    let app = build_router(state);

    let response = app
        .clone()
        .oneshot(chat_model("upstream-model", false))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let response = app
        .oneshot(chat_model("upstream-model", true))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        rec.seen.lock().unwrap().is_empty(),
        "a spent provider must not be called"
    );
}

#[tokio::test]
async fn half_open_probe_client_error_does_not_wedge_the_circuit() {
    let dir = temp_dir("circuit-probe-404");
    let (root, fake) = start_scripted().await;
    let cfg = m4b_config(&dir, format!("{root}/v1"), root.clone(), false);
    let state = AppState::new(cfg, env_file(&dir)).unwrap();
    let app = build_router(state.clone());

    // Open, then wait out the (shortened) cooldown so the next request probes.
    state
        .circuits()
        .force_open("od", "http_500", Duration::from_millis(40));
    tokio::time::sleep(Duration::from_millis(60)).await;
    fake.queue("upstream-model", &[404, 200]);

    // The probe reaches the upstream and gets a client error: relayed verbatim,
    // never counted as a circuit failure.
    let response = app
        .clone()
        .oneshot(chat_model("upstream-model", false))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(fake.hits_of("upstream-model"), 1);

    // The provider answered, so the probe slot was released and the circuit
    // closed instead of staying half_open with a leaked probe forever.
    let health = body_json(app.clone().oneshot(healthz_request()).await.unwrap()).await;
    assert_eq!(health["providers"]["od"]["circuit"], "closed");
    assert_eq!(health["providers"]["od"]["failures"], 0);

    // The next request is served, not a permanent 503.
    let response = app
        .oneshot(chat_model("upstream-model", false))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(fake.hits_of("upstream-model"), 2);
}

#[tokio::test]
async fn cancelled_half_open_probe_releases_the_slot() {
    let dir = temp_dir("circuit-probe-cancel");
    let (root, fake) = start_scripted().await;
    let cfg = m4b_config(&dir, format!("{root}/v1"), root.clone(), false);
    let state = AppState::new(cfg, env_file(&dir)).unwrap();
    let app = build_router(state.clone());

    state
        .circuits()
        .force_open("od", "http_500", Duration::from_millis(40));
    tokio::time::sleep(Duration::from_millis(60)).await;

    // The half-open probe hangs upstream; the client disconnects (the request
    // future is dropped) before any result is recorded.
    fake.set_delay_ms(300);
    let cancelled = tokio::time::timeout(
        Duration::from_millis(50),
        app.clone().oneshot(chat_model("upstream-model", false)),
    )
    .await;
    assert!(cancelled.is_err(), "the probe request must be cancelled");

    // The probe slot was released: the next request is a fresh probe and is
    // served instead of a permanent 503.
    fake.set_delay_ms(0);
    let response = app
        .clone()
        .oneshot(chat_model("upstream-model", false))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let health = body_json(app.oneshot(healthz_request()).await.unwrap()).await;
    assert_eq!(health["providers"]["od"]["circuit"], "closed");
    assert_eq!(health["providers"]["od"]["failures"], 0);
}

#[tokio::test]
async fn a_successful_provider_sticks_until_it_fails() {
    let dir = temp_dir("route-pin");
    let (app, fake) = m4b_gateway(&dir, true).await;
    fake.queue("upstream-model", &[500, 200]);
    fake.queue("fallback-model", &[200, 500]);

    // Primary fails, fallback answers, and the model pins the fallback.
    let response = app
        .clone()
        .oneshot(chat_model("upstream-model", false))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        !response.headers().contains_key("x-umsp-provider"),
        "provider header must not leak to clients"
    );

    // The pin fails, so the request selects again and the primary answers.
    let response = app
        .clone()
        .oneshot(chat_model("upstream-model", false))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // The new pin is the primary. The fallback is not called.
    let response = app
        .oneshot(chat_model("upstream-model", false))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        fake.hits(),
        vec![
            "upstream-model".to_string(),
            "fallback-model".to_string(),
            "fallback-model".to_string(),
            "upstream-model".to_string(),
            "upstream-model".to_string(),
        ]
    );
    let records = wait_records(&dir, 3).await;
    assert_eq!(records[0].provider, "deepseek");
    assert_eq!(records[0].fallbacks, 1);
    assert!(
        records[0]
            .quirks
            .iter()
            .any(|quirk| quirk == "fallback_from:od")
    );
    assert_eq!(records[1].provider, "od");
    assert_eq!(records[1].fallbacks, 1);
    assert!(
        records[1]
            .quirks
            .iter()
            .any(|quirk| quirk == "fallback_from:deepseek")
    );
    assert_eq!(records[2].provider, "od");
    assert_eq!(records[2].fallbacks, 0);
}

#[tokio::test]
async fn open_circuit_still_uses_a_closed_fallback() {
    let dir = temp_dir("circuit-fb");
    let (root, fake) = start_scripted().await;
    let cfg = m4b_config(&dir, format!("{root}/v1"), root, true);
    let state = AppState::new(cfg, env_file(&dir)).unwrap();
    assert!(
        state
            .circuits()
            .force_open("od", "test", Duration::from_secs(60))
    );
    let app = build_router(state);
    fake.queue("fallback-model", &[200, 200]);

    let response = app
        .clone()
        .oneshot(chat_model("upstream-model", false))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        fake.hits_of("upstream-model"),
        0,
        "an open circuit must not be called"
    );

    // The fallback is now pinned, so the next request skips the open provider again.
    let response = app
        .oneshot(chat_model("upstream-model", false))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(fake.hits_of("upstream-model"), 0);
    assert_eq!(fake.hits_of("fallback-model"), 2);
}

#[tokio::test]
async fn a_stream_that_dies_after_200_unpins_the_provider() {
    let dir = temp_dir("route-stream-unpin");
    let (app, fake) = m4b_gateway(&dir, true).await;

    // First request: primary fails, fallback succeeds, so the model pins deepseek.
    fake.queue("upstream-model", &[500]);
    fake.queue("fallback-model", &[200]);
    let response = app
        .clone()
        .oneshot(chat_model("upstream-model", false))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // The pinned stream starts with 200 and then dies: the pump must unpin it.
    fake.die_in_stream();
    let response = app
        .clone()
        .oneshot(chat_model("upstream-model", true))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let _ = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    fake.heal_stream();

    // With the pin gone, selection falls back to configuration order (od first).
    fake.queue("upstream-model", &[200]);
    let response = app
        .oneshot(chat_model("upstream-model", false))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        fake.hits(),
        vec![
            "upstream-model".to_string(),
            "fallback-model".to_string(),
            "fallback-model".to_string(),
            "upstream-model".to_string(),
        ]
    );
}

#[tokio::test]
async fn a_retryable_failure_tries_the_same_providers_next_upstream() {
    let dir = temp_dir("route-second-upstream");
    let (root, fake) = start_scripted().await;
    let mut cfg = m4b_config(&dir, format!("{root}/v1"), root, false);
    cfg.models[0].backends.push(Backend {
        provider: "od".into(),
        upstream: "second-model".into(),
    });
    let state = AppState::new(cfg, env_file(&dir)).unwrap();
    let app = build_router(state);
    fake.queue("upstream-model", &[503]);
    fake.queue("second-model", &[200]);

    let response = app
        .oneshot(chat_model("upstream-model", false))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        fake.hits(),
        vec!["upstream-model".to_string(), "second-model".to_string()]
    );
}

#[tokio::test]
async fn deepseek_402_opens_the_circuit_immediately() {
    let dir = temp_dir("ds-402");
    let (app, fake) = m4b_gateway(&dir, false).await;
    fake.queue("fallback-model", &[402]);

    let response = app
        .clone()
        .oneshot(chat_model("deepseek-flash", false))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let value = body_json(response).await;
    assert_eq!(value["error"]["code"], "upstream_quota_exhausted");

    let health = body_json(app.clone().oneshot(healthz_request()).await.unwrap()).await;
    assert_eq!(health["providers"]["deepseek"]["circuit"], "open");
    assert_eq!(
        health["providers"]["deepseek"]["reason"],
        "insufficient_balance"
    );
    assert!(health["providers"]["deepseek"]["opened_at"].is_string());

    let response = app
        .oneshot(chat_model("deepseek-flash", false))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        fake.hits_of("fallback-model"),
        1,
        "a balance-open circuit must reject without an upstream call"
    );
}

#[tokio::test]
async fn od_401_opens_the_circuit_and_stays_opaque() {
    let dir = temp_dir("od-401");
    let (app, fake) = m4b_gateway(&dir, false).await;
    fake.queue("upstream-model", &[401]);

    let response = app
        .clone()
        .oneshot(chat_model("upstream-model", false))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let value = body_json(response).await;
    assert_eq!(value["error"]["code"], "upstream_auth_failed");

    let health = body_json(app.clone().oneshot(healthz_request()).await.unwrap()).await;
    assert_eq!(health["providers"]["od"]["circuit"], "open");
    assert_eq!(health["providers"]["od"]["reason"], "auth_failed");

    let response = app
        .oneshot(chat_model("upstream-model", false))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(fake.hits_of("upstream-model"), 1);
}

#[tokio::test]
async fn circuit_open_uses_the_anthropic_error_shape_for_messages() {
    let dir = temp_dir("circuit-messages");
    let (app, fake) = m4b_gateway(&dir, false).await;
    fake.queue("upstream-model", &[401]);

    let response = app
        .clone()
        .oneshot(messages_model("upstream-model"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);

    let response = app.oneshot(messages_model("upstream-model")).await.unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.headers()["x-umsp-code"], "temporarily_unavailable");
    let value = body_json(response).await;
    assert_eq!(value["type"], "error");
    assert_eq!(value["error"]["type"], "api_error");
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("temporarily unavailable")
    );
}

#[tokio::test]
async fn healthz_circuit_fields_never_leak_keys() {
    let dir = temp_dir("healthz-keys");
    let (app, _fake) = m4b_gateway(&dir, false).await;

    let body = body_bytes(app.oneshot(healthz_request()).await.unwrap()).await;
    let text = String::from_utf8(body).unwrap();
    assert!(!text.contains("sk-test"), "healthz leaked an upstream key");
    assert!(
        !text.contains("sk-ds-test"),
        "healthz leaked an upstream key"
    );
    let value: Value = serde_json::from_str(&text).unwrap();
    for provider in ["od", "deepseek"] {
        assert!(value["providers"][provider]["circuit"].is_string());
        assert!(value["providers"][provider].get("failures").is_some());
        assert!(value["providers"][provider].get("opened_at").is_some());
        assert!(value["providers"][provider].get("reason").is_some());
    }
}

#[tokio::test]
async fn success_resets_the_failure_window() {
    let dir = temp_dir("circuit-reset");
    let (app, fake) = m4b_gateway(&dir, false).await;
    fake.queue("upstream-model", &[500, 500, 500, 500, 200]);

    for _ in 0..4 {
        let response = app
            .clone()
            .oneshot(chat_model("upstream-model", false))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    }
    let response = app
        .clone()
        .oneshot(chat_model("upstream-model", false))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let health = body_json(app.oneshot(healthz_request()).await.unwrap()).await;
    assert_eq!(health["providers"]["od"]["circuit"], "closed");
    assert_eq!(health["providers"]["od"]["failures"], 0);
}

#[tokio::test]
async fn mid_stream_failures_open_the_circuit() {
    let dir = temp_dir("circuit-midstream");
    let (app, _, rec) = gateway(&dir, 50 * 1024 * 1024).await;

    // Every attempt gets a 200 and then the upstream connection dies.
    for _ in 0..5 {
        let response = app
            .clone()
            .oneshot(stream_request("sse-error"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        // Reading to the end waits for the pump, which records the outcome.
        let text = String::from_utf8(body_bytes(response).await).unwrap();
        assert!(text.contains("upstream_stream_error"), "{text:?}");
    }
    let health = body_json(app.clone().oneshot(healthz_request()).await.unwrap()).await;
    assert_eq!(health["providers"]["od"]["circuit"], "open", "{health}");

    let calls = rec.seen.lock().unwrap().len();
    let response = app.oneshot(stream_request("sse-error")).await.unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        rec.seen.lock().unwrap().len(),
        calls,
        "an open circuit must not reach the upstream"
    );
}

#[tokio::test]
async fn completed_streams_keep_the_circuit_closed() {
    let dir = temp_dir("circuit-stream-ok");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    for _ in 0..6 {
        let response = app.clone().oneshot(stream_request("sse")).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        body_bytes(response).await;
    }
    let health = body_json(app.oneshot(healthz_request()).await.unwrap()).await;
    assert_eq!(health["providers"]["od"]["circuit"], "closed");
    assert_eq!(health["providers"]["od"]["failures"], 0);
}
