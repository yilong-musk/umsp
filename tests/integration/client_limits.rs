use std::path::Path;

use axum::Router;
use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

use umsp::config::{Client, Config};
use umsp::server::{AppState, build_router};

use crate::common::*;

// ---------------------------------------------------------------------------
// Per-client limits: key expiry
// ---------------------------------------------------------------------------

/// Gateway whose only client is reshaped by `edit` before the router starts.
async fn gateway_with_client(dir: &Path, edit: impl FnOnce(&mut Client)) -> (Router, Recorder) {
    let (upstream, rec) = start_upstream().await;
    let mut cfg: Config = config(dir, upstream, 52_428_800);
    edit(&mut cfg.clients[0]);
    let state = AppState::new(cfg, env_file(dir)).unwrap();
    (build_router(state), rec)
}

#[tokio::test]
async fn expired_key_is_rejected_before_binding() {
    let dir = temp_dir("limits-expired");
    let (app, rec) = gateway_with_client(&dir, |c| {
        c.expires_at = Some("2020-01-01T00:00:00Z".into());
    })
    .await;

    let response = app.clone().oneshot(chat_request("normal")).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        response.headers().get("x-umsp-code").unwrap(),
        "key_expired"
    );
    let value = body_json(response).await;
    assert_eq!(value["error"]["code"], "key_expired");

    // Messages callers get Anthropic's envelope.
    let body = json!({
        "model": "upstream-model",
        "max_tokens": 16,
        "messages": [{"role": "user", "content": "hi"}]
    });
    let response = app
        .clone()
        .oneshot(post_to("/v1/messages", body, "normal"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let value = body_json(response).await;
    assert_eq!(value["type"], "error");
    assert_eq!(value["error"]["type"], "authentication_error");

    // Never reached the upstream, never claimed a device binding.
    assert!(rec.seen.lock().unwrap().is_empty());
    assert!(
        umsp::bindings::BindingStore::load(&dir)
            .bound_ip("tester")
            .is_none()
    );
}

#[tokio::test]
async fn key_with_future_expiry_is_served() {
    let dir = temp_dir("limits-future");
    let (app, _) = gateway_with_client(&dir, |c| {
        c.expires_at = Some("2999-01-01T00:00:00Z".into());
    })
    .await;
    let response = app.oneshot(chat_request("normal")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn malformed_expiry_fails_closed() {
    let dir = temp_dir("limits-malformed");
    let (app, _) = gateway_with_client(&dir, |c| {
        c.expires_at = Some("next tuesday".into());
    })
    .await;
    let response = app.oneshot(chat_request("normal")).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// Per-client limits: concurrency
// ---------------------------------------------------------------------------

#[tokio::test]
async fn concurrency_limit_rejects_the_second_buffered_call() {
    let dir = temp_dir("limits-conc-buffered");
    let (app, _) = gateway_with_client(&dir, |c| c.max_concurrency = Some(1)).await;

    // The "slow" upstream holds the first call for ~400ms.
    let first = tokio::spawn(app.clone().oneshot(chat_request("slow")));
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let response = app.clone().oneshot(chat_request("normal")).await.unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.headers().get("retry-after").unwrap(), "1");
    let value = body_json(response).await;
    assert_eq!(value["error"]["code"], "concurrency_limit");

    // Metadata calls are exempt while the slot is taken.
    let models = axum::http::Request::builder()
        .uri("/v1/models")
        .header("authorization", format!("Bearer {TEST_KEY}"))
        .body(axum::body::Body::empty())
        .unwrap();
    let response = app.clone().oneshot(models).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let response = first.await.unwrap().unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    body_bytes(response).await;

    // The slot is free again.
    let response = app.oneshot(chat_request("normal")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn a_stream_holds_its_slot_until_the_body_is_dropped() {
    let dir = temp_dir("limits-conc-stream");
    let (app, _) = gateway_with_client(&dir, |c| c.max_concurrency = Some(1)).await;

    // Headers arrive at once; the upstream then goes quiet for 1.3s.
    let streaming = app
        .clone()
        .oneshot(stream_request("sse-slow"))
        .await
        .unwrap();
    assert_eq!(streaming.status(), StatusCode::OK);

    let response = app.clone().oneshot(chat_request("normal")).await.unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);

    // The client walks away mid-stream: dropping the body frees the slot.
    drop(streaming);
    let response = app.oneshot(chat_request("normal")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn concurrency_limit_uses_the_messages_error_shape() {
    let dir = temp_dir("limits-conc-messages");
    let (app, _) = gateway_with_client(&dir, |c| c.max_concurrency = Some(1)).await;
    let streaming = app
        .clone()
        .oneshot(stream_request("sse-slow"))
        .await
        .unwrap();
    assert_eq!(streaming.status(), StatusCode::OK);

    let body = json!({
        "model": "upstream-model",
        "max_tokens": 16,
        "messages": [{"role": "user", "content": "hi"}]
    });
    let response = app
        .oneshot(post_to("/v1/messages", body, "normal"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let value = body_json(response).await;
    assert_eq!(value["error"]["type"], "rate_limit_error");
    drop(streaming);
}

#[tokio::test]
async fn unlimited_clients_are_not_throttled() {
    let dir = temp_dir("limits-conc-none");
    let (app, _) = gateway_with_client(&dir, |_| {}).await;
    let streaming = app
        .clone()
        .oneshot(stream_request("sse-slow"))
        .await
        .unwrap();
    let response = app.oneshot(chat_request("normal")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    drop(streaming);
}

// ---------------------------------------------------------------------------
// Per-client limits: token budget
// ---------------------------------------------------------------------------

fn capped_chat(max_tokens: u64) -> axum::http::Request<axum::body::Body> {
    post_body(
        json!({
            "model": "upstream-model",
            "messages": [{"role": "user", "content": "hello"}],
            "max_tokens": max_tokens
        }),
        "normal",
    )
}

fn used_of(dir: &Path) -> u64 {
    umsp::limits::BudgetStore::load(dir).used("tester")
}

#[tokio::test]
async fn budget_charges_real_usage_and_then_refuses() {
    let dir = temp_dir("limits-budget");
    let (app, rec) = gateway_with_client(&dir, |c| c.budget_tokens = Some(65)).await;

    // The fake upstream reports 10 + 20 = 30 tokens per buffered chat call.
    // Bodies are drained: a reservation lives until its response body is done.
    let response = app.clone().oneshot(capped_chat(10)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    body_bytes(response).await;
    assert_eq!(used_of(&dir), 30);

    // 35 left: an output cap of 36 does not fit, 35 does.
    let response = app.clone().oneshot(capped_chat(36)).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let value = body_json(response).await;
    assert_eq!(value["error"]["code"], "budget_exhausted");
    let message = value["error"]["message"].as_str().unwrap().to_string();
    assert!(message.contains("35 tokens left"), "{value}");
    let response = app.clone().oneshot(capped_chat(35)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    body_bytes(response).await;

    // 5 left. Input is never reserved, so an uncapped call is still admitted
    // and overshoots the budget by its own usage; after that nothing is.
    assert_eq!(used_of(&dir), 60);
    let uncapped = post_body(
        json!({"model": "upstream-model", "messages": [{"role": "user", "content": "hi"}]}),
        "normal",
    );
    let response = app.clone().oneshot(uncapped).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    body_bytes(response).await;
    assert_eq!(used_of(&dir), 90);

    let response = app.clone().oneshot(capped_chat(1)).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        rec.seen.lock().unwrap().len(),
        3,
        "refusals never reach the upstream"
    );

    // Refusals are ledger rows with the error code.
    let rows = wait_records(&dir, 5).await;
    assert!(
        rows.iter()
            .filter(|r| r.error_code.as_deref() == Some("budget_exhausted"))
            .count()
            >= 2
    );
}

#[tokio::test]
async fn budget_counts_streams_and_skips_calls_without_usage() {
    let dir = temp_dir("limits-budget-stream");
    let (app, _) = gateway_with_client(&dir, |c| c.budget_tokens = Some(1000)).await;

    let response = app.clone().oneshot(stream_request("sse")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    body_bytes(response).await;
    // The stream's trailing usage chunk reports 11 + 22.
    for _ in 0..100 {
        if used_of(&dir) > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(used_of(&dir), 33);

    // No usage reported: not charged.
    let response = app
        .clone()
        .oneshot(responses_post(responses_body(false), "nousage"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    body_bytes(response).await;
    assert_eq!(used_of(&dir), 33);
}

#[tokio::test]
async fn budget_overrides_include_usage_false() {
    let dir = temp_dir("limits-budget-force-usage");
    let (app, rec) = gateway_with_client(&dir, |c| c.budget_tokens = Some(1000)).await;
    for stream_options in [json!({"include_usage": false}), json!("off")] {
        let request = post_body(
            json!({
                "model": "upstream-model",
                "messages": [{"role": "user", "content": "hello"}],
                "stream": true,
                "stream_options": stream_options
            }),
            "sse",
        );
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        body_bytes(response).await;
        assert_eq!(rec.last()["stream_options"]["include_usage"], true);
    }
    let rows = wait_records(&dir, 2).await;
    assert!(
        rows.iter()
            .all(|r| r.quirks.iter().any(|q| q == "forced_include_usage")),
        "{rows:?}"
    );
}

#[tokio::test]
async fn budget_refusal_uses_the_messages_error_shape() {
    let dir = temp_dir("limits-budget-messages");
    let (app, _) = gateway_with_client(&dir, |c| c.budget_tokens = Some(10)).await;
    let body = json!({
        "model": "upstream-model",
        "max_tokens": 11,
        "messages": [{"role": "user", "content": "hi"}]
    });
    let response = app
        .oneshot(post_to("/v1/messages", body, "normal"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let value = body_json(response).await;
    assert_eq!(value["type"], "error");
    assert_eq!(value["error"]["type"], "permission_error");
}

#[tokio::test]
async fn clients_without_a_budget_are_not_tracked() {
    let dir = temp_dir("limits-budget-none");
    let (app, _) = gateway_with_client(&dir, |_| {}).await;
    let response = app.oneshot(capped_chat(10)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!dir.join(umsp::limits::USAGE_FILE).exists());
}

#[tokio::test]
async fn an_unfinished_body_keeps_its_reservation() {
    let dir = temp_dir("limits-budget-held");
    let (app, _) = gateway_with_client(&dir, |c| c.budget_tokens = Some(100)).await;
    let held = app.clone().oneshot(capped_chat(40)).await.unwrap();
    assert_eq!(held.status(), StatusCode::OK);
    // Charged 30, still reserving 40 until the body is done: 30 left.
    let response = app.clone().oneshot(capped_chat(31)).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    drop(held);
    let response = app.oneshot(capped_chat(31)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}
