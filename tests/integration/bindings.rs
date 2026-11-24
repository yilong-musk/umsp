use std::path::Path;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;

use umsp::server::{AppState, build_router};

use crate::common::*;

// ---------------------------------------------------------------------------
// Device binding (TOFU on source IP)
// ---------------------------------------------------------------------------

/// A chat request whose effective source IP is forced via XFF. The in-process
/// router has no socket peer, so the auth layer treats it as loopback and
/// trusts the proxy-supplied header — exactly the public reverse-proxy path.
fn chat_request_from_ip(mode: &str, xff: &str) -> Request<Body> {
    let mut req = chat_request(mode);
    req.headers_mut()
        .insert("x-forwarded-for", xff.parse().unwrap());
    req
}

fn bound_ip_of(dir: &Path, client: &str) -> Option<String> {
    umsp::bindings::BindingStore::load(dir).bound_ip(client)
}

#[tokio::test]
async fn key_binds_on_first_use_and_rejects_other_ips() {
    let dir = temp_dir("bind-first");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;

    // First authenticated request binds the key to the loopback peer.
    let response = app.clone().oneshot(chat_request("normal")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(bound_ip_of(&dir, "tester").as_deref(), Some("127.0.0.1"));

    // Same key from a different device IP: 403, and the error must not leak
    // the bound IP (the caller may hold a leaked key).
    let response = app
        .clone()
        .oneshot(chat_request_from_ip("normal", "9.9.9.9"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        response.headers().get("x-umsp-code").unwrap(),
        "device_mismatch"
    );
    let value = body_json(response).await;
    assert_eq!(value["error"]["code"], "device_mismatch");
    let text = value.to_string();
    assert!(!text.contains("127.0.0.1"), "bound IP leaked: {text}");

    // The rejected request must not move the binding; the bound device still works.
    assert_eq!(bound_ip_of(&dir, "tester").as_deref(), Some("127.0.0.1"));
    let response = app.oneshot(chat_request("normal")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Ledger rows carry the effective client IP.
    let rows = wait_records(&dir, 2).await;
    assert!(rows.len() >= 2);
    for row in &rows {
        assert_eq!(row.client_ip.as_deref(), Some("127.0.0.1"));
    }
}

#[tokio::test]
async fn device_mismatch_uses_messages_error_shape() {
    let dir = temp_dir("bind-msg");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let response = app.clone().oneshot(chat_request("normal")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let mut req = post_to(
        "/v1/messages",
        json!({
            "model": "upstream-model",
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}]
        }),
        "normal",
    );
    req.headers_mut()
        .insert("x-forwarded-for", "9.9.9.9".parse().unwrap());
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let value = body_json(response).await;
    assert_eq!(value["type"], "error");
    assert_eq!(value["error"]["type"], "permission_error");
    assert!(!value.to_string().contains("127.0.0.1"));
}

#[tokio::test]
async fn xff_rightmost_entry_wins_over_spoofed_left() {
    let dir = temp_dir("bind-xff");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;

    // Left entries are client-controlled; the proxy wrote the rightmost one.
    let response = app
        .clone()
        .oneshot(chat_request_from_ip("normal", "1.2.3.4, 9.9.9.9"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(bound_ip_of(&dir, "tester").as_deref(), Some("9.9.9.9"));

    let response = app
        .clone()
        .oneshot(chat_request_from_ip("normal", "9.9.9.9"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Presenting only the spoofed left entry must not pass.
    let response = app
        .oneshot(chat_request_from_ip("normal", "1.2.3.4"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn unbind_then_reload_reaches_live_state() {
    let dir = temp_dir("bind-unbind");
    let (upstream, _rec) = start_upstream().await;
    let cfg = config(&dir, upstream, 50 * 1024 * 1024);
    let state = AppState::new(cfg, env_file(&dir)).unwrap();
    let app = build_router(state.clone());

    let response = app.clone().oneshot(chat_request("normal")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response = app
        .clone()
        .oneshot(chat_request_from_ip("normal", "9.9.9.9"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    // CLI unbind writes the file; SIGHUP reloads it into the live state.
    assert!(
        umsp::bindings::BindingStore::load(&dir)
            .unbind("tester")
            .unwrap()
    );
    state.reload_bindings(&dir);

    // The next request re-binds (TOFU) to the new device IP.
    let response = app
        .clone()
        .oneshot(chat_request_from_ip("normal", "9.9.9.9"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(bound_ip_of(&dir, "tester").as_deref(), Some("9.9.9.9"));
    let response = app
        .oneshot(chat_request_from_ip("normal", "8.8.8.8"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn binding_persists_across_restart() {
    let dir = temp_dir("bind-restart");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let response = app.clone().oneshot(chat_request("normal")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    drop(app);

    // A fresh state over the same state_dir must enforce the stored binding.
    let (app2, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let response = app2
        .clone()
        .oneshot(chat_request_from_ip("normal", "9.9.9.9"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = app2.oneshot(chat_request("normal")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn count_tokens_enforces_binding() {
    let dir = temp_dir("bind-count");
    let (app, _rec) = ds_gateway(&dir).await;
    let response = app.clone().oneshot(chat_request("normal")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let mut req = post_to(
        "/v1/messages/count_tokens",
        json!({
            "model": "deepseek-flash",
            "messages": [{"role": "user", "content": "hi"}]
        }),
        "normal",
    );
    req.headers_mut()
        .insert("x-forwarded-for", "9.9.9.9".parse().unwrap());
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn models_get_binds_and_enforces() {
    let dir = temp_dir("bind-models");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let get = |xff: Option<&str>| {
        let mut builder = Request::builder()
            .method("GET")
            .uri("/v1/models")
            .header("x-api-key", TEST_KEY);
        if let Some(ip) = xff {
            builder = builder.header("x-forwarded-for", ip);
        }
        builder.body(Body::empty()).unwrap()
    };
    let response = app.clone().oneshot(get(None)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(bound_ip_of(&dir, "tester").as_deref(), Some("127.0.0.1"));
    let response = app.oneshot(get(Some("9.9.9.9"))).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}
