use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;

use umsp::server::{AppState, build_router};

use crate::common::*;

#[tokio::test]
async fn messages_upstream_failures_use_the_anthropic_error_shape() {
    let dir = temp_dir("msg-err-shape");
    let (upstream, _) = start_upstream().await;
    let mut cfg = config(&dir, upstream, 50 * 1024 * 1024);
    // The env file never defines this variable: every attempt fails with
    // `upstream_key_missing` before reaching the upstream.
    cfg.providers[0].api_key_env = "TEST_UNSET_KEY".into();
    let app = build_router(AppState::new(cfg, env_file(&dir)).unwrap());

    for stream in [false, true] {
        let response = app
            .clone()
            .oneshot(messages_post(messages_body(stream), "normal"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            response.headers().get("x-umsp-code").unwrap(),
            "upstream_key_missing"
        );
        let value = body_json(response).await;
        assert_eq!(value["type"], "error", "stream={stream}: {value}");
        assert_eq!(value["error"]["type"], "api_error", "stream={stream}");
        assert!(value["error"].get("param").is_none(), "stream={stream}");
        assert!(value["error"].get("code").is_none(), "stream={stream}");
    }

    // Chat clients keep the OpenAI shape for the same failure.
    let response = app.oneshot(stream_request("sse")).await.unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let value = body_json(response).await;
    assert_eq!(value["error"]["code"], "upstream_key_missing");
    assert_eq!(value["error"]["type"], "server_error");
}

#[tokio::test]
async fn messages_non_stream_relays_body_verbatim_and_records_usage() {
    let dir = temp_dir("msg-nonstream");
    let (app, _, rec) = gateway(&dir, 50 * 1024 * 1024).await;

    let response = app
        .oneshot(messages_post(messages_body(false), "normal"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "application/json"
    );
    assert!(
        response.headers().get("x-umsp-provider").is_none(),
        "provider header must not leak to clients"
    );
    assert!(
        response.headers().get("x-umsp-upstream-model").is_none(),
        "upstream model header must not leak"
    );
    assert!(response.headers().get("x-umsp-request-id").is_some());
    let value = body_json(response).await;

    // Anthropic content blocks survive untouched, including thinking and tool_use.
    let types: Vec<&str> = value["content"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        types,
        ["thinking", "redacted_thinking", "text", "tool_use"],
        "content blocks must be relayed in order"
    );
    assert_eq!(value["stop_reason"], "tool_use");
    assert_eq!(
        value["usage"]["cache_creation"]["ephemeral_5m_input_tokens"],
        3
    );

    // Only `model` changed upstream; everything else is byte-identical in shape.
    let received = &rec.last();
    assert_eq!(received["model"], "upstream-model");
    assert_eq!(received["max_tokens"], 64);
    assert_eq!(received["system"][0]["text"], "be brief");
    assert_eq!(received["system"][0]["cache_control"]["type"], "ephemeral");
    assert_eq!(
        received["messages"][0]["content"][0]["cache_control"]["type"],
        "ephemeral"
    );
    assert_eq!(received["tools"][0]["input_schema"]["type"], "object");
    assert_eq!(received["tool_choice"]["type"], "auto");
    assert_eq!(received["thinking"]["budget_tokens"], 1024);
    assert_eq!(received["metadata"]["user_id"], "u1");
    assert_eq!(received["stop_sequences"][0], "STOP");
    assert_eq!(received["temperature"], 0.3);
    assert_eq!(received["top_p"], 0.9);
    assert_eq!(received["top_k"], 5);
    assert_eq!(received["output_config"]["effort"], "high");
    assert_eq!(received["context_management"]["edits"], json!([]));
    assert_eq!(received["custom_beta_field"]["keep"], true);
    assert_eq!(received["stream"], false);
    assert!(
        received.get("max_output_tokens").is_none(),
        "the gateway must not add defaults"
    );

    let records = wait_records(&dir, 1).await;
    assert_eq!(records.len(), 1);
    let r = &records[0];
    assert_eq!(r.protocol, "messages");
    assert!(!r.stream);
    assert_eq!(r.status, 200);
    assert_eq!(r.upstream_status, Some(200));
    assert_eq!(r.prompt_tokens, 21);
    assert_eq!(r.completion_tokens, 9);
    assert_eq!(r.cached_tokens, 4);
    assert!(r.ttft_ms.is_none());
    assert!(r.bytes_out > 0);
    assert!(r.error_code.is_none());
    assert!(!r.quirks.iter().any(|q| q == "usage_missing"));
}

#[tokio::test]
async fn messages_non_stream_without_usage_records_usage_missing() {
    let dir = temp_dir("msg-nousage");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let response = app
        .oneshot(messages_post(messages_body(false), "nousage"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let value = body_json(response).await;
    assert_eq!(value["type"], "message");
    assert!(value.get("usage").is_none());

    let records = wait_records(&dir, 1).await;
    assert_eq!(records[0].protocol, "messages");
    assert_eq!(records[0].prompt_tokens, 0);
    assert!(records[0].quirks.iter().any(|q| q == "usage_missing"));
}

#[tokio::test]
async fn messages_query_and_headers_are_relayed_with_default_version() {
    let dir = temp_dir("msg-headers");
    let (app, _, rec) = gateway(&dir, 50 * 1024 * 1024).await;

    let request = Request::builder()
        .method("POST")
        .uri("/v1/messages?beta=true")
        .header("authorization", format!("Bearer {TEST_KEY}"))
        .header("x-api-key", TEST_KEY)
        .header("content-type", "application/json")
        .header(
            "anthropic-beta",
            "fine-grained-tool-streaming-2025-05-14,interleaved-thinking-2025-05-14",
        )
        .header("x-stainless-lang", "js")
        .header("x-claude-code-session-id", "sess-42")
        .body(Body::from(
            serde_json::to_vec(&messages_body(false)).unwrap(),
        ))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    assert_eq!(
        rec.last_uri(),
        "/v1/messages?beta=true",
        "query string must survive to the upstream"
    );
    let up = rec.last_headers();
    // Missing on the client side -> the gateway injects the OD-required version.
    assert_eq!(up.get("anthropic-version").unwrap(), "2023-06-01");
    assert_eq!(
        up.get("anthropic-beta").unwrap(),
        "fine-grained-tool-streaming-2025-05-14,interleaved-thinking-2025-05-14",
        "beta values are relayed verbatim, never whitelisted"
    );
    assert_eq!(up.get("x-stainless-lang").unwrap(), "js");
    assert_eq!(up.get("x-claude-code-session-id").unwrap(), "sess-42");
    // Client credentials never leak; the provider key is injected instead.
    assert_eq!(up.get("authorization").unwrap(), "Bearer sk-test");
    assert!(up.get("x-api-key").is_none());
    assert_eq!(up.get_all("content-type").iter().count(), 1);
    assert_eq!(up.get("accept-encoding").unwrap(), "identity");
}

#[tokio::test]
async fn messages_client_anthropic_version_is_not_overwritten() {
    let dir = temp_dir("msg-version");
    let (app, _, rec) = gateway(&dir, 50 * 1024 * 1024).await;
    let request = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("authorization", format!("Bearer {TEST_KEY}"))
        .header("content-type", "application/json")
        .header("anthropic-version", "2023-01-01")
        .body(Body::from(
            serde_json::to_vec(&messages_body(false)).unwrap(),
        ))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        rec.last_headers().get("anthropic-version").unwrap(),
        "2023-01-01"
    );
    assert_eq!(rec.last_uri(), "/v1/messages");
}

#[tokio::test]
async fn messages_stream_relays_event_sequence_verbatim() {
    let dir = temp_dir("msg-sse");
    let (app, _, rec) = gateway(&dir, 50 * 1024 * 1024).await;

    let response = app
        .oneshot(messages_post(messages_body(true), "sse"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "text/event-stream"
    );
    assert!(response.headers().get("content-length").is_none());
    let bytes = body_bytes(response).await;
    assert_eq!(
        String::from_utf8(bytes).unwrap(),
        MESSAGES_SSE,
        "the event sequence must be relayed verbatim, ping included"
    );
    assert_eq!(rec.last()["stream"], true);

    let records = wait_records(&dir, 1).await;
    let r = &records[0];
    assert_eq!(r.protocol, "messages");
    assert!(r.stream);
    assert_eq!(r.status, 200);
    assert_eq!(r.prompt_tokens, 21);
    assert_eq!(r.completion_tokens, 17);
    assert_eq!(r.cached_tokens, 4);
    assert!(r.ttft_ms.is_some(), "ttft_ms from the first data event");
    assert_eq!(r.bytes_out, MESSAGES_SSE.len() as u64);
    assert!(!r.client_disconnected);
    assert!(r.error_code.is_none());
}

#[tokio::test]
async fn messages_stream_truncated_without_message_stop_emits_error_event() {
    let dir = temp_dir("msg-nostop");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let bytes = body_bytes(
        app.oneshot(messages_post(messages_body(true), "sse-nostop"))
            .await
            .unwrap(),
    )
    .await;
    let text = String::from_utf8(bytes).unwrap();
    assert!(text.starts_with(MESSAGES_SSE_NO_STOP));
    assert!(text.contains("event: error\ndata: "));
    assert!(
        !text.contains("message_stop"),
        "the gateway must never fabricate message_stop"
    );
    assert!(!text.contains("[DONE]"), "Messages has no [DONE] sentinel");

    let r = wait_until(&dir, |_| true, std::time::Duration::from_secs(5))
        .await
        .expect("ledger row");
    assert_eq!(r.error_code.as_deref(), Some("upstream_stream_error"));
}

#[tokio::test]
async fn messages_stream_keepalive_is_a_ping_event() {
    let dir = temp_dir("msg-keepalive");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let bytes = body_bytes(
        app.oneshot(messages_post(messages_body(true), "sse-slow"))
            .await
            .unwrap(),
    )
    .await;
    let text = String::from_utf8(bytes).unwrap();
    let pings = text
        .matches("event: ping\ndata: {\"type\":\"ping\"}\n\n")
        .count();
    assert!(pings >= 2, "upstream ping + gateway keepalive: {text:?}");
    let strip = |s: &str| s.replace("event: ping\ndata: {\"type\":\"ping\"}\n\n", "");
    assert_eq!(
        strip(&text),
        strip(MESSAGES_SSE),
        "keepalive must not disturb the relayed sequence"
    );
}

#[tokio::test]
async fn messages_stream_upstream_error_event_is_passed_through() {
    let dir = temp_dir("msg-upstream-error");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let bytes = body_bytes(
        app.oneshot(messages_post(messages_body(true), "sse-error-event"))
            .await
            .unwrap(),
    )
    .await;
    let text = String::from_utf8(bytes).unwrap();
    assert_eq!(text, MESSAGES_SSE_UPSTREAM_ERROR);
    // No extra error event and no forged terminator.
    assert_eq!(text.matches("event: error").count(), 1);
    assert!(!text.contains("message_stop"));
    assert!(!text.contains("[DONE]"));

    let r = wait_until(&dir, |_| true, std::time::Duration::from_secs(5))
        .await
        .expect("ledger row");
    assert!(r.stream);
    assert_eq!(r.status, 200);
}

#[tokio::test]
async fn messages_stream_upstream_connection_error_emits_error_event() {
    let dir = temp_dir("msg-sse-error");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let response = app
        .oneshot(messages_post(messages_body(true), "sse-error"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let text = String::from_utf8(body_bytes(response).await).unwrap();
    assert!(text.starts_with("event: message_start\n"));
    assert!(text.contains("event: error\ndata: "));
    assert!(text.contains("\"type\":\"upstream_error\""));
    assert!(!text.contains("message_stop"));
    assert!(!text.contains("[DONE]"));

    let r = wait_until(&dir, |_| true, std::time::Duration::from_secs(5))
        .await
        .expect("ledger row");
    assert_eq!(r.error_code.as_deref(), Some("upstream_stream_error"));
}

#[tokio::test]
async fn messages_stream_client_disconnect_is_recorded() {
    let dir = temp_dir("msg-disc");
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
    let body = serde_json::to_string(&messages_body(true)).unwrap();
    let request = format!(
        "POST /v1/messages?beta=true HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {TEST_KEY}\r\ncontent-type: application/json\r\nx-test-mode: sse-endless\r\nContent-Length: {}\r\n\r\n{body}",
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
    assert_eq!(record.protocol, "messages");
    assert_eq!(record.status, 200);
    assert!(record.stream);
}

#[tokio::test]
async fn messages_disconnect_before_headers_still_logs_499() {
    let dir = temp_dir("msg-disc-early");
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
    let body = serde_json::to_string(&messages_body(true)).unwrap();
    let request = format!(
        "POST /v1/messages HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {TEST_KEY}\r\ncontent-type: application/json\r\nx-test-mode: sse-delay\r\nContent-Length: {}\r\n\r\n{body}",
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
    assert_eq!(record.protocol, "messages");
    assert_eq!(record.status, 499);
    assert_eq!(record.error_code.as_deref(), Some("client_disconnected"));
    assert!(record.stream);
}

#[tokio::test]
async fn messages_upstream_401_becomes_a_neutral_gateway_error() {
    let dir = temp_dir("msg-401");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let response = app
        .oneshot(messages_post(messages_body(true), "error401"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(
        response.headers().get("x-umsp-code").unwrap(),
        "upstream_auth_failed"
    );
    assert!(
        response.headers().get("x-request-id").is_none(),
        "upstream headers must not leak"
    );
    assert!(response.headers().get("x-should-retry").is_none());
    assert!(response.headers().get("retry-after").is_none());
    let value = body_json(response).await;
    assert_eq!(value["type"], "error");
    assert_eq!(value["error"]["type"], "api_error");
    assert_eq!(
        value["error"]["message"],
        "the gateway could not authenticate with the upstream service"
    );

    let r = wait_until(&dir, |_| true, std::time::Duration::from_secs(5))
        .await
        .expect("ledger row");
    assert_eq!(r.status, 502);
    assert_eq!(r.upstream_status, Some(401));
    assert_eq!(r.error_code.as_deref(), Some("upstream_401"));
}

#[tokio::test]
async fn messages_count_tokens_is_501_not_supported() {
    let dir = temp_dir("msg-count");
    let (app, _, rec) = gateway(&dir, 50 * 1024 * 1024).await;
    let body = json!({
        "model": "upstream-model",
        "messages": [{"role": "user", "content": "hello"}]
    });
    let request = Request::builder()
        .method("POST")
        .uri("/v1/messages/count_tokens")
        .header("authorization", format!("Bearer {TEST_KEY}"))
        .header("x-api-key", TEST_KEY)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    assert_eq!(
        response.headers().get("x-umsp-code").unwrap(),
        "not_supported_error"
    );
    let value = body_json(response).await;
    assert_eq!(
        value,
        json!({
            "type": "error",
            "error": {
                "type": "not_supported_error",
                "message": "count_tokens is not supported by any backend of model 'upstream-model'"
            }
        })
    );
    assert!(
        rec.seen.lock().unwrap().is_empty(),
        "count_tokens must not be forwarded to the OD 404"
    );
}

#[tokio::test]
async fn messages_gateway_errors_use_anthropic_shape() {
    let dir = temp_dir("msg-errors");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;

    // No key -> 401 in Anthropic shape.
    let request = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let value = body_json(response).await;
    assert_eq!(value["type"], "error");
    assert_eq!(value["error"]["type"], "authentication_error");

    // Unknown model -> 404 not_found_error, still Anthropic shape.
    let body = json!({"model": "od/nope", "messages": []});
    let request = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("authorization", format!("Bearer {TEST_KEY}"))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let value = body_json(response).await;
    assert_eq!(value["type"], "error");
    assert_eq!(value["error"]["type"], "not_found_error");
}

#[tokio::test]
async fn messages_upstream_400_body_is_passed_through() {
    let dir = temp_dir("msg-400");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let response = app
        .oneshot(messages_post(messages_body(false), "error400"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let value = body_json(response).await;
    assert_eq!(value["type"], "error");
    assert_eq!(value["error"]["type"], "invalid_request_error");
    assert_eq!(value["error"]["message"], "max_tokens: field required");
}
