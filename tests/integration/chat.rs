use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;

use umsp::server::{AppState, build_router};

use crate::common::*;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn chat_proxy_applies_od_quirks_and_counts_usage() {
    let dir = temp_dir("chat");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;

    let response = app.clone().oneshot(chat_request("normal")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let value = body_json(response).await;

    // Request rewriting: only `model` plus the max_tokens quirk.
    assert_eq!(value["_received"]["model"], "upstream-model");
    assert_eq!(value["_received"]["max_tokens"], 20);
    assert!(value["_received"].get("max_completion_tokens").is_none());
    assert_eq!(value["_received"]["custom_unknown_field"]["keep"], true);
    assert_eq!(
        value["_received"]["reasoning_details"][0]["type"],
        "reasoning.encrypted"
    );

    // Response quirks: finish_reason inferred, tool_call index stripped,
    // reasoning_details preserved.
    assert_eq!(value["choices"][0]["finish_reason"], "length");
    assert!(
        value["choices"][0]["message"]["tool_calls"][0]
            .get("index")
            .is_none()
    );
    assert_eq!(
        value["choices"][0]["message"]["reasoning_details"][0]["summary"],
        "because"
    );

    // Ledger recorded exactly one request with usage.
    let records = wait_records(&dir, 1).await;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].status, 200);
    assert_eq!(records[0].prompt_tokens, 10);
    assert_eq!(records[0].completion_tokens, 20);
    assert_eq!(records[0].cached_tokens, 4);
    assert_eq!(records[0].reasoning_tokens, 6);
    assert_eq!(records[0].requested_model, "upstream-model");
    assert_eq!(records[0].upstream_model, "upstream-model");
    assert!(!records[0].quirks.is_empty());
}

#[tokio::test]
async fn empty_choices_are_synthesized() {
    let dir = temp_dir("empty");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let response = app.oneshot(chat_request("empty")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let value = body_json(response).await;
    assert_eq!(value["choices"][0]["message"]["role"], "assistant");
    assert_eq!(value["choices"][0]["finish_reason"], "length");
}

#[tokio::test]
async fn missing_key_is_401_with_umsp_code() {
    let dir = temp_dir("auth");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"model":"upstream-model","messages":[]}"#))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        response.headers().get("x-umsp-code").unwrap(),
        "unauthorized"
    );
    let value = body_json(response).await;
    assert_eq!(value["error"]["code"], "unauthorized");
    assert_eq!(value["error"]["type"], "authentication_error");
}

#[tokio::test]
async fn x_api_key_header_is_accepted() {
    let dir = temp_dir("xapikey");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let request = Request::builder()
        .method("GET")
        .uri("/v1/models")
        .header("x-api-key", TEST_KEY)
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn unknown_model_is_404() {
    let dir = temp_dir("model");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let body = json!({"model": "od/nope", "messages": []});
    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("authorization", format!("Bearer {TEST_KEY}"))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        response.headers().get("x-umsp-code").unwrap(),
        "model_not_found"
    );
}

#[tokio::test]
async fn oversized_body_is_413() {
    let dir = temp_dir("413");
    let (app, _, _) = gateway(&dir, 64).await;
    let big = format!(
        r#"{{"model":"upstream-model","messages":[],"pad":"{}"}}"#,
        "x".repeat(4096)
    );
    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("authorization", format!("Bearer {TEST_KEY}"))
        .header("content-type", "application/json")
        .body(Body::from(big))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        response.headers().get("x-umsp-code").unwrap(),
        "request_too_large"
    );
}

#[tokio::test]
async fn streaming_forwards_bytes_verbatim_and_records_usage() {
    let dir = temp_dir("sse");
    let (app, _, rec) = gateway(&dir, 50 * 1024 * 1024).await;

    let response = app.oneshot(stream_request("sse")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "text/event-stream"
    );
    assert!(response.headers().get("content-length").is_none());
    assert_eq!(response.headers().get("cache-control").unwrap(), "no-cache");
    assert_eq!(response.headers().get("x-accel-buffering").unwrap(), "no");
    assert!(response.headers().get("x-umsp-request-id").is_some());
    assert!(
        response.headers().get("x-umsp-provider").is_none(),
        "provider header must not leak to clients"
    );
    assert!(
        response.headers().get("x-umsp-upstream-model").is_none(),
        "upstream model header must not leak"
    );
    // No upstream header is relayed: vendor trace ids would reveal the backend.
    assert!(response.headers().get("x-request-id").is_none());
    assert!(response.headers().get("x-upstream-echo").is_none());

    // Byte-for-byte: comments, empty deltas, empty id, tool deltas, [DONE].
    let bytes = body_bytes(response).await;
    assert_eq!(
        bytes,
        SSE_NORMAL.as_bytes(),
        "SSE must be forwarded verbatim"
    );

    // Accounting: include_usage was injected because the client omitted it.
    assert_eq!(rec.last()["stream_options"]["include_usage"], true);
    assert_eq!(rec.last()["model"], "upstream-model");

    // Upstream request headers: streaming accept, no compression, exactly one
    // content-type, and the provider key (never the client key).
    let up_headers = rec.last_headers();
    assert_eq!(up_headers.get("accept").unwrap(), "text/event-stream");
    assert_eq!(up_headers.get("accept-encoding").unwrap(), "identity");
    assert_eq!(up_headers.get("authorization").unwrap(), "Bearer sk-test");
    assert_eq!(up_headers.get_all("content-type").iter().count(), 1);

    let records = wait_records(&dir, 1).await;
    assert_eq!(records.len(), 1);
    let r = &records[0];
    assert!(r.stream);
    assert_eq!(r.status, 200);
    assert_eq!(r.upstream_status, Some(200));
    assert_eq!(r.prompt_tokens, 11);
    assert_eq!(r.completion_tokens, 22);
    assert_eq!(r.cached_tokens, 3);
    assert_eq!(r.reasoning_tokens, 5);
    assert!(r.ttft_ms.is_some(), "ttft_ms must be recorded");
    assert_eq!(r.bytes_out, SSE_NORMAL.len() as u64);
    assert!(!r.client_disconnected);
    assert!(r.error_code.is_none());
    assert!(r.quirks.iter().any(|q| q == "injected_include_usage"));
}

#[tokio::test]
async fn streaming_appends_done_when_upstream_omits_it() {
    let dir = temp_dir("sse-nodone");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let bytes = body_bytes(app.oneshot(stream_request("sse-nodone")).await.unwrap()).await;
    let expected = format!("{SSE_NO_DONE}data: [DONE]\n\n");
    assert_eq!(String::from_utf8(bytes).unwrap(), expected);

    let r = wait_until(&dir, |_| true, std::time::Duration::from_secs(5))
        .await
        .expect("ledger row");
    assert!(r.stream);
    assert_eq!(r.status, 200);
    // No usage chunk from upstream -> recorded as missing, never estimated.
    assert_eq!(r.completion_tokens, 0);
    assert!(r.quirks.iter().any(|q| q == "usage_missing"));
}

#[tokio::test]
async fn streaming_respects_explicit_include_usage_false() {
    let dir = temp_dir("sse-nousage");
    let (app, _, rec) = gateway(&dir, 50 * 1024 * 1024).await;
    let body = json!({
        "model": "upstream-model",
        "messages": [],
        "stream": true,
        "stream_options": {"include_usage": false}
    });
    let bytes = body_bytes(app.oneshot(post_body(body, "sse-nousage")).await.unwrap()).await;
    assert_eq!(String::from_utf8(bytes).unwrap(), SSE_NO_USAGE);
    assert_eq!(rec.last()["stream_options"]["include_usage"], false);

    let r = wait_until(&dir, |_| true, std::time::Duration::from_secs(5))
        .await
        .expect("ledger row");
    assert!(r.quirks.iter().any(|q| q == "usage_missing"));
    assert!(!r.quirks.iter().any(|q| q == "injected_include_usage"));
}

#[tokio::test]
async fn streaming_keepalive_fills_upstream_silence() {
    let dir = temp_dir("sse-ka");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let bytes = body_bytes(app.oneshot(stream_request("sse-slow")).await.unwrap()).await;
    let text = String::from_utf8(bytes).unwrap();
    let keepalives = text.matches(": keep-alive\n\n").count();
    assert!(keepalives >= 1, "expected keep-alive comments: {text:?}");
    assert_eq!(
        text.replace(": keep-alive\n\n", ""),
        format!("{SSE_NO_DONE}data: [DONE]\n\n")
    );
}

#[tokio::test]
async fn streaming_idle_timeout_fires_despite_keepalives() {
    let dir = temp_dir("sse-idle");
    let (upstream, _) = start_upstream().await;
    let mut cfg = config(&dir, upstream, 50 * 1024 * 1024);
    // Keepalive fires several times inside one idle window; it must not
    // extend the window.
    cfg.server.stream_idle_timeout_s = 2;
    cfg.server.stream_keepalive_s = 1;
    let app = build_router(AppState::new(cfg, env_file(&dir)).unwrap());

    let started = Instant::now();
    let response = app.oneshot(stream_request("sse-stall")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = tokio::time::timeout(Duration::from_secs(10), body_bytes(response))
        .await
        .expect("stream must end at the idle timeout, not hang");
    let elapsed = started.elapsed();
    let text = String::from_utf8(bytes).unwrap();
    assert!(text.contains(": keep-alive\n\n"), "{text:?}");
    // The wire code stays `upstream_stream_error` (docs/protocols.md); the
    // message and the ledger row carry the timeout.
    assert!(text.contains("upstream stream idle timeout"), "{text:?}");
    assert!(text.ends_with("data: [DONE]\n\n"));
    assert!(elapsed >= Duration::from_secs(2), "{elapsed:?}");

    let r = wait_until(&dir, |_| true, Duration::from_secs(5))
        .await
        .expect("ledger row");
    assert_eq!(r.error_code.as_deref(), Some("upstream_stream_timeout"));
}

#[tokio::test]
async fn streaming_upstream_error_after_headers_emits_error_then_done() {
    let dir = temp_dir("sse-err");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let response = app.oneshot(stream_request("sse-error")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let text = String::from_utf8(body_bytes(response).await).unwrap();
    assert!(
        text.starts_with(
            "data: {\"id\":\"\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"part\"}}]}\n\n"
        ),
        "partial upstream event must be relayed: {text:?}"
    );
    assert!(text.contains("\"type\":\"upstream_error\""));
    assert!(text.contains("\"code\":\"upstream_stream_error\""));
    assert!(text.ends_with("data: [DONE]\n\n"));
    // No forged success: no finish_reason, no usage numbers.
    assert!(!text.contains("finish_reason"));

    let r = wait_until(&dir, |_| true, std::time::Duration::from_secs(5))
        .await
        .expect("ledger row");
    assert_eq!(r.status, 200);
    assert_eq!(r.error_code.as_deref(), Some("upstream_stream_error"));
    assert_eq!(r.completion_tokens, 0);
    assert!(r.ttft_ms.is_some());
}

#[tokio::test]
async fn streaming_upstream_error_before_headers_is_relayed_verbatim() {
    let dir = temp_dir("sse-401");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let response = app.oneshot(stream_request("error401")).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    // Not an SSE response: gateway errors are plain JSON.
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "application/json"
    );
    assert_eq!(
        response.headers().get("x-umsp-code").unwrap(),
        "upstream_auth_failed"
    );
    assert!(
        response.headers().get("x-request-id").is_none(),
        "upstream headers must not leak"
    );
    let value = body_json(response).await;
    assert_eq!(
        value["error"]["message"],
        "the gateway could not authenticate with the upstream service"
    );

    let r = wait_until(&dir, |_| true, std::time::Duration::from_secs(5))
        .await
        .expect("ledger row");
    assert!(r.stream);
    assert_eq!(r.status, 502);
    assert_eq!(r.upstream_status, Some(401));
    assert_eq!(r.error_code.as_deref(), Some("upstream_401"));
}

#[tokio::test]
async fn streaming_disconnect_before_headers_still_logs() {
    let dir = temp_dir("sse-disc-early");
    let (upstream, _rec) = start_upstream().await;
    let cfg = config(&dir, upstream, 50 * 1024 * 1024);
    let state = AppState::new(cfg, env_file(&dir)).unwrap();
    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    use tokio::io::AsyncWriteExt;
    let body = serde_json::to_string(&json!({
        "model": "upstream-model",
        "messages": [],
        "stream": true
    }))
    .unwrap();
    let request = format!(
        "POST /v1/chat/completions HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {TEST_KEY}\r\nContent-Type: application/json\r\nx-test-mode: sse-delay\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let mut sock = tokio::net::TcpStream::connect(addr).await.unwrap();
    sock.write_all(request.as_bytes()).await.unwrap();
    drop(sock); // gone before upstream headers arrive

    let record = wait_until(
        &dir,
        |r| r.client_disconnected,
        std::time::Duration::from_secs(10),
    )
    .await
    .expect("a 499 row must be written instead of losing the request");
    assert_eq!(record.status, 499);
    assert_eq!(record.error_code.as_deref(), Some("client_disconnected"));
    assert!(record.stream);
}

#[tokio::test]
async fn cancelled_buffered_request_still_writes_a_499_row() {
    let dir = temp_dir("buffered-cancel");
    let (app, _, rec) = gateway(&dir, 50 * 1024 * 1024).await;

    // The client gives up while the upstream call is in flight: dropping the
    // handler future is what a disconnect does to the request task.
    let gave_up = tokio::time::timeout(
        Duration::from_millis(100),
        app.clone().oneshot(chat_request("slow")),
    )
    .await;
    assert!(gave_up.is_err(), "the request must still be in flight");

    let record = wait_until(&dir, |r| r.client_disconnected, Duration::from_secs(5))
        .await
        .expect("a 499 row must be written instead of losing the request");
    assert_eq!(record.status, 499);
    assert_eq!(record.error_code.as_deref(), Some("client_disconnected"));
    assert!(!record.stream);
    assert_eq!(record.provider, "od");
    assert_eq!(record.requested_model, "upstream-model");
    assert_eq!(rec.seen.lock().unwrap().len(), 1);

    // A completed request still writes exactly one row of its own.
    let response = app.oneshot(chat_request("normal")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let records = wait_records(&dir, 2).await;
    assert_eq!(records.len(), 2, "{records:?}");
    assert_eq!(records.iter().filter(|r| r.status == 499).count(), 1);
}

#[tokio::test]
async fn non_boolean_stream_field_is_handled_as_false() {
    let dir = temp_dir("stream-nonbool");
    let (app, _, rec) = gateway(&dir, 50 * 1024 * 1024).await;
    let body = json!({
        "model": "upstream-model",
        "messages": [],
        "stream": "yes"
    });
    let response = app.oneshot(post_body(body, "normal")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "application/json"
    );
    // Normalized on the wire: strict upstreams reject a non-boolean `stream`.
    assert_eq!(rec.last()["stream"], false);
    assert!(rec.last().get("stream_options").is_none());
    let value = body_json(response).await;
    assert_eq!(value["choices"][0]["message"]["content"], "hi");
    let records = wait_records(&dir, 1).await;
    assert!(records[0].quirks.iter().any(|q| q == "stream_non_boolean"));
    assert!(!records[0].stream);
}

#[tokio::test]
async fn streaming_client_disconnect_is_recorded_and_server_survives() {
    let dir = temp_dir("sse-disc");
    let (upstream, _rec) = start_upstream().await;
    let cfg = config(&dir, upstream, 50 * 1024 * 1024);
    let state = AppState::new(cfg, env_file(&dir)).unwrap();
    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let body = json!({
        "model": "upstream-model",
        "messages": [],
        "stream": true
    });
    let body = serde_json::to_string(&body).unwrap();
    let request = format!(
        "POST /v1/chat/completions HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {TEST_KEY}\r\nContent-Type: application/json\r\nx-test-mode: sse-endless\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let mut sock = tokio::net::TcpStream::connect(addr).await.unwrap();
    sock.write_all(request.as_bytes()).await.unwrap();
    let mut buf = [0u8; 1024];
    let n = tokio::time::timeout(std::time::Duration::from_secs(5), sock.read(&mut buf))
        .await
        .expect("first bytes")
        .expect("read");
    assert!(n > 0);
    drop(sock);

    let record = wait_until(
        &dir,
        |r| r.client_disconnected,
        std::time::Duration::from_secs(10),
    )
    .await
    .expect("ledger row with client_disconnected=true");
    assert_eq!(record.status, 200);
    assert!(record.stream);

    // Port still serves after the disconnect.
    let mut sock = tokio::net::TcpStream::connect(addr).await.unwrap();
    sock.write_all(
        format!("GET /healthz HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n").as_bytes(),
    )
    .await
    .unwrap();
    let mut raw = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        sock.read_to_end(&mut raw),
    )
    .await
    .expect("healthz response")
    .expect("read");
    let text = String::from_utf8_lossy(&raw);
    assert!(
        text.starts_with("HTTP/1.1 200"),
        "healthz still up: {text:?}"
    );
}

#[tokio::test]
async fn models_list_is_flat_and_filtered_by_alias_resolution() {
    let dir = temp_dir("models");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let request = Request::builder()
        .method("GET")
        .uri("/v1/models")
        .header("authorization", format!("Bearer {TEST_KEY}"))
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let value = body_json(response).await;
    assert_eq!(value["object"], "list");
    let model = &value["data"][0];
    assert_eq!(model["id"], "upstream-model");
    assert_eq!(model["context_length"], 1048000);
    assert_eq!(model["max_completion_tokens"], 128000);
    assert_eq!(model["capabilities"]["tools"], true);
    assert_eq!(model["capabilities"]["vision"], false);
    assert!(model.get("metadata").is_none());

    // Gateway metadata envelope (cc-switch style consumers): same card data
    // under the names those parsers read, no provider data, flat fields intact.
    let meta = &model["meta"];
    assert_eq!(meta["context_length"], 1048000);
    assert_eq!(meta["max_output_tokens"], 128000);
    assert_eq!(meta["display_name"], "Test Model");
    assert_eq!(meta["tool_call"], true);
    assert_eq!(meta["reasoning"], true);
    assert_eq!(meta["capabilities"]["vision"], false);
    assert!(meta["capabilities"].get("vision_source").is_none());
    assert_eq!(meta["input_modalities"], json!(["text"]));
    assert_eq!(meta["output_modalities"], json!(["text"]));
    for key in ["provider", "upstream", "upstream_model", "backends"] {
        assert!(meta.get(key).is_none(), "meta must not expose {key}");
    }

    // Alias also resolves through the chat endpoint.
    let body = json!({"model": "short", "messages": []});
    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("authorization", format!("Bearer {TEST_KEY}"))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn health_and_hello() {
    let dir = temp_dir("health");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;

    let request = Request::builder()
        .method("GET")
        .uri("/healthz")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let value = body_json(response).await;
    assert_eq!(value["status"], "ok");
    assert_eq!(value["version"], env!("CARGO_PKG_VERSION"));
    assert!(value["providers"]["od"].is_object());

    let request = Request::builder()
        .method("HEAD")
        .uri("/api/hello")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn upstream_error_body_is_passed_through() {
    let dir = temp_dir("upstream-err");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let response = app.oneshot(chat_request("error")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK); // fake returns 200 with an error body
    let value = body_json(response).await;
    assert_eq!(value["error"]["message"], "upstream says no");
    assert!(value.get("choices").is_none());
}

#[tokio::test]
async fn upstream_401_becomes_a_neutral_gateway_error() {
    let dir = temp_dir("upstream-401");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let response = app.oneshot(chat_request("error401")).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(
        response.headers().get("x-umsp-code").unwrap(),
        "upstream_auth_failed"
    );
    assert!(response.headers().get("x-umsp-request-id").is_some());
    let value = body_json(response).await;
    assert_eq!(
        value["error"]["message"],
        "the gateway could not authenticate with the upstream service"
    );
    assert_eq!(value["error"]["code"], "upstream_auth_failed");
}

#[tokio::test]
async fn upstream_426_becomes_a_neutral_gateway_error() {
    let dir = temp_dir("upstream-426");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let response = app.oneshot(chat_request("error426")).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(
        response.headers().get("x-umsp-code").unwrap(),
        "upstream_client_outdated"
    );
    let value = body_json(response).await;
    assert_eq!(value["error"]["code"], "upstream_client_outdated");
    assert!(
        !value.to_string().to_lowercase().contains("grok"),
        "vendor client details must not leak: {value}"
    );
}

#[tokio::test]
async fn upstream_400_body_is_scrubbed_before_reaching_the_client() {
    let dir = temp_dir("scrub-400");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let response = app.oneshot(chat_request("error400-leak")).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "application/json"
    );
    let text = String::from_utf8(body_bytes(response).await).unwrap();
    assert!(
        text.contains("on provider umsp"),
        "provider id must be replaced: {text}"
    );
    assert!(!text.contains(" od"), "provider id leaked: {text}");
    assert!(
        text.contains("upstream-model"),
        "the message itself must survive: {text}"
    );

    let r = wait_until(&dir, |_| true, std::time::Duration::from_secs(5))
        .await
        .expect("ledger row");
    assert_eq!(r.status, 400);
    assert_eq!(r.upstream_status, Some(400));
    assert!(
        r.upstream_body.unwrap_or_default().contains("provider od"),
        "the raw body stays in the ledger"
    );
}
