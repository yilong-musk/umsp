use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::json;
use tower::ServiceExt;

use umsp::config::{Backend, Provider};
use umsp::envfile::EnvFile;
use umsp::server::{AppState, build_router};
use umsp::usage::LogStore;

use crate::common::*;

#[tokio::test]
async fn reload_swaps_env_keys_logs_timeout_and_models_atomically() {
    let dir1 = temp_dir("reload-dir1");
    let dir2 = temp_dir("reload-dir2");
    let (upstream, rec) = start_upstream().await;

    let mut cfg1 = config(&dir1, upstream.clone(), 50 * 1024 * 1024);
    cfg1.providers[0].api_key_env = "TEST_ROTATED_KEY".into();
    // Env file starts without TEST_ROTATED_KEY so the first call fails.
    let state = AppState::new(cfg1.clone(), env_file(&dir1)).unwrap();
    let app = build_router(state.clone());

    let response = app.clone().oneshot(chat_request("normal")).await.unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        response.headers().get("x-umsp-code").unwrap(),
        "upstream_key_missing"
    );
    let rows1_before = wait_records(&dir1, 1).await;
    assert_eq!(rows1_before.len(), 1);

    // Prepare a reloaded config pointing at dir2, a new request timeout, a new
    // model alias, and a fresh env file carrying the rotated upstream key.
    let mut cfg2 = cfg1;
    cfg2.server.state_dir = dir2.display().to_string();
    cfg2.server.request_timeout_s = 12;
    cfg2.models[0].aliases.push("reloaded-alias".into());
    let env2_path = dir2.join("env");
    std::fs::write(&env2_path, "TEST_ROTATED_KEY=sk-rotated-new\n").unwrap();
    let env2 = EnvFile::load(&env2_path);

    state.reload(cfg2, env2).unwrap();

    // Request using the new alias succeeds with the rotated key and writes its
    // ledger row to dir2, leaving dir1 untouched.
    let response = app
        .clone()
        .oneshot(post_body(
            json!({
                "model": "reloaded-alias",
                "messages": [{"role": "user", "content": "hello"}]
            }),
            "normal",
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        rec.last_headers().get("authorization").unwrap(),
        "Bearer sk-rotated-new"
    );

    state.flush_logs();
    let rows1_after =
        LogStore::new(&dir1).read_since(chrono::Utc::now() - chrono::TimeDelta::minutes(5));
    let rows2 = wait_records(&dir2, 1).await;
    assert_eq!(
        rows1_after.len(),
        1,
        "old state_dir must stop receiving rows"
    );
    assert_eq!(
        rows2.len(),
        1,
        "new state_dir must receive the reloaded row"
    );
    assert_eq!(rows2[0].status, 200);
}

#[tokio::test]
async fn healthz_probe_runs_providers_in_parallel() {
    let dir = temp_dir("healthz-parallel");
    let slow_chat = || async {
        tokio::time::sleep(Duration::from_millis(400)).await;
        Json(json!({
            "id": "chatcmpl-probe",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "ok"},
                "finish_reason": "stop"
            }]
        }))
    };
    let app_a = Router::new().route("/v1/chat/completions", post(slow_chat));
    let app_b = Router::new().route("/v1/chat/completions", post(slow_chat));
    let listener_a = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let listener_b = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr_a = listener_a.local_addr().unwrap();
    let addr_b = listener_b.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener_a, app_a).await;
    });
    tokio::spawn(async move {
        let _ = axum::serve(listener_b, app_b).await;
    });

    let mut cfg = config(&dir, format!("http://{addr_a}/v1"), 50 * 1024 * 1024);
    cfg.providers.push(Provider {
        id: "second".into(),
        vendor: None,
        base_url: format!("http://{addr_b}/v1"),
        anthropic_base_url: None,
        api_key_env: "TEST_OD_KEY".into(),
        auth: "api_key".into(),
        protocols: vec!["chat".into()],
        quirks: None,
    });
    cfg.models[0].backends.push(Backend {
        provider: "second".into(),
        upstream: "upstream-model".into(),
    });
    let app = build_router(AppState::new(cfg, env_file(&dir)).unwrap());

    let started = Instant::now();
    let response = app
        .oneshot(
            Request::builder()
                .uri("/healthz?probe=1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let elapsed = started.elapsed();
    assert_eq!(response.status(), StatusCode::OK);
    let value = body_json(response).await;
    assert_eq!(value["providers"]["od"]["probe"]["status"], "ok", "{value}");
    assert_eq!(
        value["providers"]["second"]["probe"]["status"], "ok",
        "{value}"
    );
    assert!(
        elapsed < Duration::from_millis(750),
        "two 400ms probes ran serially instead of in parallel: {elapsed:?}"
    );
}
