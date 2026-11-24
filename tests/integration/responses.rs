use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;

use umsp::server::{AppState, build_router};

use crate::common::*;

// ---------------------------------------------------------------------------
// Responses (M2)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn responses_non_stream_relays_body_verbatim_and_records_usage() {
    let dir = temp_dir("resp-nonstream");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let response = app
        .oneshot(responses_post(responses_body(false), "normal"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
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
    assert_eq!(value["id"], "resp_test");
    assert_eq!(value["usage"]["input_tokens"], 11);
    // Only `model` is rewritten; every other field reaches the upstream as sent.
    let received = &value["_received"];
    assert_eq!(received["model"], "upstream-model");
    assert_eq!(received["instructions"], "be brief");
    assert_eq!(received["max_output_tokens"], 64);
    assert_eq!(received["reasoning"]["effort"], "low");
    assert_eq!(received["store"], false);
    assert_eq!(received["metadata"]["k"], "v");
    assert_eq!(received["include"][0], "reasoning.encrypted_content");
    assert_eq!(received["truncation"], "auto");
    assert_eq!(received["tool_choice"], "auto");
    assert_eq!(received["parallel_tool_calls"], true);
    assert_eq!(received["custom_unknown_field"]["keep"], true);
    // No defaults are injected.
    assert!(received.get("temperature").is_none());
    assert!(received.get("top_p").is_none());

    let records = wait_records(&dir, 1).await;
    assert_eq!(records.len(), 1);
    let r = &records[0];
    assert_eq!(r.protocol, "responses");
    assert_eq!(r.status, 200);
    assert!(!r.stream);
    assert_eq!(r.prompt_tokens, 11);
    assert_eq!(r.completion_tokens, 22);
    assert_eq!(r.cached_tokens, 3);
    assert_eq!(r.reasoning_tokens, 5);
    assert!(!r.quirks.iter().any(|q| q == "usage_missing"));
}

#[tokio::test]
async fn responses_non_stream_without_usage_records_usage_missing() {
    let dir = temp_dir("resp-nousage");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let response = app
        .oneshot(responses_post(responses_body(false), "nousage"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let value = body_json(response).await;
    assert_eq!(value["status"], "completed");

    let r = wait_until(&dir, |_| true, std::time::Duration::from_secs(5))
        .await
        .expect("ledger row");
    assert_eq!(r.protocol, "responses");
    assert_eq!(r.prompt_tokens, 0);
    assert!(r.quirks.iter().any(|q| q == "usage_missing"));
}

#[tokio::test]
async fn responses_non_stream_failed_body_is_passed_through() {
    let dir = temp_dir("resp-failed");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let response = app
        .oneshot(responses_post(responses_body(false), "failed"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let value = body_json(response).await;
    assert_eq!(value["status"], "failed");
    assert_eq!(value["error"]["code"], "server_error");
    assert!(value.get("choices").is_none());
}

#[tokio::test]
async fn responses_stream_adds_missing_event_lines() {
    let dir = temp_dir("resp-sse");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let response = app
        .oneshot(responses_post(responses_body(true), "sse"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "text/event-stream"
    );
    assert!(response.headers().get("content-length").is_none());
    assert!(
        response.headers().get("x-umsp-provider").is_none(),
        "provider header must not leak to clients"
    );

    let text = String::from_utf8(body_bytes(response).await).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        if line.starts_with("data: {") {
            let prev = if i == 0 { "" } else { lines[i - 1] };
            assert!(
                prev.starts_with("event: "),
                "data line without a preceding event line: {line}"
            );
        }
    }
    assert_eq!(text.matches("event: ").count(), 8);
    assert_eq!(text.matches("data: [DONE]").count(), 1);
    assert!(text.ends_with("data: [DONE]\n\n"));
    // Removing the inserted event lines gives back the upstream bytes exactly.
    let stripped: String = lines
        .iter()
        .filter(|line| !line.starts_with("event: "))
        .map(|line| format!("{line}\n"))
        .collect();
    assert_eq!(stripped, RESPONSES_SSE_DATA_ONLY);

    let r = wait_until(&dir, |_| true, std::time::Duration::from_secs(5))
        .await
        .expect("ledger row");
    assert_eq!(r.protocol, "responses");
    assert!(r.stream);
    assert_eq!(r.status, 200);
    assert_eq!(r.prompt_tokens, 11);
    assert_eq!(r.completion_tokens, 22);
    assert_eq!(r.cached_tokens, 3);
    assert_eq!(r.reasoning_tokens, 5);
    assert!(r.ttft_ms.is_some(), "ttft_ms must be recorded");
    assert_eq!(r.bytes_out, text.len() as u64);
    assert!(!r.client_disconnected);
    assert!(r.error_code.is_none());
}

#[tokio::test]
async fn responses_stream_keeps_existing_event_lines() {
    let dir = temp_dir("resp-sse-events");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let text = String::from_utf8(
        body_bytes(
            app.oneshot(responses_post(responses_body(true), "sse-events"))
                .await
                .unwrap(),
        )
        .await,
    )
    .unwrap();
    assert_eq!(text, RESPONSES_SSE_WITH_EVENTS);
}

#[tokio::test]
async fn responses_event_lines_can_be_disabled() {
    let dir = temp_dir("resp-sse-off");
    let (app, _, _) = gateway_with(&dir, 50 * 1024 * 1024, false).await;
    let text = String::from_utf8(
        body_bytes(
            app.oneshot(responses_post(responses_body(true), "sse"))
                .await
                .unwrap(),
        )
        .await,
    )
    .unwrap();
    assert_eq!(text, RESPONSES_SSE_DATA_ONLY);
    assert!(!text.contains("event: "));
}

#[tokio::test]
async fn responses_stream_keepalive_fills_upstream_silence() {
    let dir = temp_dir("resp-sse-ka");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let text = String::from_utf8(
        body_bytes(
            app.oneshot(responses_post(responses_body(true), "sse-slow"))
                .await
                .unwrap(),
        )
        .await,
    )
    .unwrap();
    assert!(
        text.matches(": keep-alive\n\n").count() >= 1,
        "expected keep-alive comments: {text:?}"
    );
    assert_eq!(
        text.replace(": keep-alive\n\n", ""),
        RESPONSES_SSE_WITH_EVENTS
    );
}

#[tokio::test]
async fn responses_stream_upstream_error_after_headers_emits_error_event() {
    let dir = temp_dir("resp-sse-err");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let response = app
        .oneshot(responses_post(responses_body(true), "sse-error"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let text = String::from_utf8(body_bytes(response).await).unwrap();
    assert!(
        text.starts_with("event: response.created\ndata: {\"type\":\"response.created\""),
        "partial upstream event must be relayed: {text:?}"
    );
    assert!(text.contains("event: error\ndata: {\"type\":\"error\""));
    assert!(text.contains("\"code\":\"upstream_stream_error\""));
    assert!(text.ends_with("data: [DONE]\n\n"));
    // No forged success.
    assert!(!text.contains("response.completed"));

    let r = wait_until(&dir, |_| true, std::time::Duration::from_secs(5))
        .await
        .expect("ledger row");
    assert_eq!(r.protocol, "responses");
    assert_eq!(r.status, 200);
    assert_eq!(r.error_code.as_deref(), Some("upstream_stream_error"));
    assert!(r.ttft_ms.is_some());
}

#[tokio::test]
async fn responses_stream_upstream_error_before_headers_is_relayed_verbatim() {
    let dir = temp_dir("resp-401");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let response = app
        .oneshot(responses_post(responses_body(true), "error401"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "application/json"
    );
    assert_eq!(
        response.headers().get("x-umsp-code").unwrap(),
        "upstream_auth_failed"
    );
    let value = body_json(response).await;
    assert_eq!(
        value["error"]["message"],
        "the gateway could not authenticate with the upstream service"
    );

    let r = wait_until(&dir, |_| true, std::time::Duration::from_secs(5))
        .await
        .expect("ledger row");
    assert_eq!(r.protocol, "responses");
    assert!(r.stream);
    assert_eq!(r.status, 502);
    assert_eq!(r.upstream_status, Some(401));
    assert_eq!(r.error_code.as_deref(), Some("upstream_401"));
}

#[tokio::test]
async fn responses_previous_response_id_is_400_unsupported_parameter() {
    let dir = temp_dir("resp-prev");
    let (app, _, rec) = gateway(&dir, 50 * 1024 * 1024).await;
    let body = json!({
        "model": "upstream-model",
        "input": "hello",
        "previous_response_id": "resp_does_not_exist"
    });
    let response = app.oneshot(responses_post(body, "normal")).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        response.headers().get("x-umsp-code").unwrap(),
        "unsupported_parameter"
    );
    let value = body_json(response).await;
    assert_eq!(value["error"]["code"], "unsupported_parameter");
    assert_eq!(value["error"]["type"], "invalid_request_error");
    assert_eq!(
        value["error"]["message"],
        "previous_response_id is not supported by this gateway; send the full input history"
    );
    // Rejected before any upstream call.
    assert!(rec.last().is_null());

    let r = wait_until(&dir, |_| true, std::time::Duration::from_secs(5))
        .await
        .expect("ledger row");
    assert_eq!(r.protocol, "responses");
    assert_eq!(r.status, 400);
    assert_eq!(r.error_code.as_deref(), Some("unsupported_parameter"));
}

#[tokio::test]
async fn responses_unsupported_endpoints_are_501_and_chat_survives() {
    let dir = temp_dir("resp-501");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;

    for (method, uri) in [
        ("POST", "/v1/responses/input_tokens"),
        ("GET", "/v1/responses/resp_123"),
        ("POST", "/v1/responses/resp_123/cancel"),
    ] {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("Bearer {TEST_KEY}"))
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::NOT_IMPLEMENTED,
            "{method} {uri} must be 501"
        );
        assert_eq!(
            response.headers().get("x-umsp-code").unwrap(),
            "unsupported_endpoint"
        );
        let value = body_json(response).await;
        assert_eq!(value["error"]["code"], "unsupported_endpoint");
        assert_eq!(value["error"]["type"], "invalid_request_error");
    }

    // The wildcard must not swallow the real Responses endpoint or Chat.
    let response = app
        .clone()
        .oneshot(responses_post(responses_body(false), "normal"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let response = app.oneshot(chat_request("normal")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn responses_oversized_body_is_413() {
    let dir = temp_dir("resp-413");
    let (app, _, _) = gateway(&dir, 64).await;
    let body = json!({"model": "upstream-model", "input": "x".repeat(4096)});
    let response = app.oneshot(responses_post(body, "normal")).await.unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        response.headers().get("x-umsp-code").unwrap(),
        "request_too_large"
    );
}

#[tokio::test]
async fn responses_streaming_client_disconnect_is_recorded() {
    let dir = temp_dir("resp-disc");
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
    let body = serde_json::to_string(&json!({
        "model": "upstream-model",
        "input": "hi",
        "stream": true
    }))
    .unwrap();
    let request = format!(
        "POST /v1/responses HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {TEST_KEY}\r\nContent-Type: application/json\r\nx-test-mode: sse-endless\r\nContent-Length: {}\r\n\r\n{body}",
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
    assert_eq!(record.protocol, "responses");
    assert_eq!(record.status, 200);
    assert!(record.stream);
}
