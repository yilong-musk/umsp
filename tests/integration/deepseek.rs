use axum::http::StatusCode;
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::common::*;

// ---------------------------------------------------------------------------
// M4a: DeepSeek provider
// ---------------------------------------------------------------------------

/// A DS Chat request that replays an OpenDesign-style history: the assistant
/// turn carries `reasoning` + `reasoning_details` but no `reasoning_content`.
fn ds_chat_body() -> Value {
    json!({
        "model": "deepseek-flash",
        "messages": [
            {"role": "user", "content": "run f"},
            {"role": "assistant", "content": null, "reasoning": "I should call f",
             "reasoning_details": [{"type": "reasoning.encrypted", "data": "abc"}],
             "tool_calls": [{"id": "call_1", "type": "function",
                             "function": {"name": "f", "arguments": "{}"}}]},
            {"role": "tool", "tool_call_id": "call_1", "content": "ok"},
            {"role": "assistant", "content": "done"}
        ],
        "tools": [{"type": "function", "function": {"name": "f",
                    "parameters": {"type": "object", "properties": {}}}}],
        "thinking": {"type": "enabled"},
        "reasoning_effort": "max",
        "custom_unknown_field": {"keep": true}
    })
}

#[tokio::test]
async fn ds_chat_backfills_reasoning_content_and_drops_reasoning_details() {
    let dir = temp_dir("ds-chat");
    let (app, rec) = ds_gateway(&dir).await;
    let response = app
        .clone()
        .oneshot(post_to("/v1/chat/completions", ds_chat_body(), "normal"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let seen = rec.last();
    // The alias resolved to the DS model and only the upstream id went out.
    assert_eq!(seen["model"], "deepseek-flash");
    // `reasoning` text backfills the required field; the bare assistant turn
    // gets a single space instead of a 400.
    assert_eq!(seen["messages"][1]["reasoning_content"], "I should call f");
    assert_eq!(seen["messages"][3]["reasoning_content"], " ");
    // OD's encrypted reasoning is OpenDesign-only.
    assert!(seen["messages"][1].get("reasoning_details").is_none());
    // Unknown fields, thinking and reasoning_effort pass through untouched.
    assert_eq!(seen["custom_unknown_field"]["keep"], true);
    assert_eq!(seen["thinking"]["type"], "enabled");
    assert_eq!(seen["reasoning_effort"], "max");
    // A stable per-client user_id is injected for KVCache isolation.
    let user_id = seen["user_id"]
        .as_str()
        .expect("user_id must be injected for DS");
    assert!(user_id.starts_with("umsp-"), "got {user_id}");

    let record = wait_until(
        &dir,
        |r| r.protocol == "chat",
        std::time::Duration::from_secs(10),
    )
    .await
    .expect("chat ledger row");
    assert_eq!(record.provider, "deepseek");
    assert_eq!(record.upstream_model, "deepseek-flash");
    assert_eq!(record.status, 200);
    assert!(
        record
            .quirks
            .contains(&"filled_reasoning_content".to_string())
    );
    assert!(
        record
            .quirks
            .contains(&"filled_reasoning_placeholder".to_string())
    );
    assert!(
        record
            .quirks
            .contains(&"dropped_reasoning_details".to_string())
    );
    assert!(record.quirks.contains(&"injected_user_id".to_string()));
}

#[tokio::test]
async fn ds_chat_user_id_is_stable_and_an_explicit_one_wins() {
    let dir = temp_dir("ds-user-id");
    let (app, rec) = ds_gateway(&dir).await;
    for _ in 0..2 {
        let response = app
            .clone()
            .oneshot(post_to("/v1/chat/completions", ds_chat_body(), "normal"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
    let seen = rec.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0]["user_id"], seen[1]["user_id"], "must be stable");
    let injected = seen[0]["user_id"].clone();

    let mut explicit = ds_chat_body();
    explicit["user_id"] = json!("client-picked");
    let response = app
        .oneshot(post_to("/v1/chat/completions", explicit, "normal"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(rec.last()["user_id"], "client-picked");
    assert_ne!(rec.last()["user_id"], injected);
}

#[tokio::test]
async fn ds_chat_without_tools_does_not_invent_reasoning_content() {
    let dir = temp_dir("ds-no-tools");
    let (app, rec) = ds_gateway(&dir).await;
    let mut body = ds_chat_body();
    body.as_object_mut().unwrap().remove("tools");
    let response = app
        .oneshot(post_to("/v1/chat/completions", body, "normal"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let seen = rec.last();
    assert!(seen["messages"][1].get("reasoning_content").is_none());
    assert!(seen["messages"][3].get("reasoning_content").is_none());
    assert!(seen["messages"][1].get("reasoning_details").is_none());
}

#[tokio::test]
async fn ds_chat_stream_is_forwarded_and_recorded() {
    let dir = temp_dir("ds-chat-stream");
    let (app, rec) = ds_gateway(&dir).await;
    let mut body = ds_chat_body();
    body["stream"] = json!(true);
    let response = app
        .oneshot(post_to("/v1/chat/completions", body, "sse"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response.headers().get("x-umsp-provider").is_none(),
        "provider header must not leak to clients"
    );
    let bytes = body_bytes(response).await;
    let text = String::from_utf8(bytes).unwrap();
    assert!(text.contains("data: [DONE]"));
    // The upstream got the DS quirks on the streaming path too.
    assert_eq!(
        rec.last()["messages"][1]["reasoning_content"],
        "I should call f"
    );
    assert_eq!(rec.last()["stream"], true);

    let record = wait_until(
        &dir,
        |r| r.protocol == "chat" && r.stream,
        std::time::Duration::from_secs(10),
    )
    .await
    .expect("streaming chat ledger row");
    assert_eq!(record.provider, "deepseek");
    assert!(record.ttft_ms.is_some(), "ttft_ms must be recorded");
    assert_eq!(record.prompt_tokens, 11);
    assert_eq!(record.completion_tokens, 22);
}

#[tokio::test]
async fn ds_responses_non_stream_relays_body_and_records_usage() {
    let dir = temp_dir("ds-resp");
    let (app, rec) = ds_gateway(&dir).await;
    let mut body = responses_body(false);
    body["model"] = json!("deepseek-flash");
    let response = app
        .oneshot(post_to("/v1/responses", body, "normal"))
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
    let value = body_json(response).await;
    assert_eq!(value["object"], "response");
    assert_eq!(value["output"][0]["type"], "message");
    // Only `model` is rewritten; everything else is relayed as-is.
    assert_eq!(rec.last()["model"], "deepseek-flash");
    assert_eq!(rec.last()["instructions"], "be brief");
    assert_eq!(rec.last()["custom_unknown_field"]["keep"], true);

    let record = wait_until(
        &dir,
        |r| r.protocol == "responses",
        std::time::Duration::from_secs(10),
    )
    .await
    .expect("responses ledger row");
    assert_eq!(record.provider, "deepseek");
    assert_eq!(record.status, 200);
    assert_eq!(record.prompt_tokens, 11);
    assert_eq!(record.completion_tokens, 22);
    assert_eq!(record.cached_tokens, 3);
    assert_eq!(record.reasoning_tokens, 5);
}

#[tokio::test]
async fn ds_responses_stream_does_not_duplicate_event_lines() {
    let dir = temp_dir("ds-resp-stream");
    let (app, rec) = ds_gateway(&dir).await;
    let mut body = responses_body(true);
    body["model"] = json!("deepseek-flash");
    let response = app
        .oneshot(post_to("/v1/responses", body, "sse-events"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "text/event-stream"
    );
    assert!(
        response.headers().get("x-umsp-provider").is_none(),
        "provider header must not leak to clients"
    );
    let text = String::from_utf8(body_bytes(response).await).unwrap();
    // DeepSeek already sends `event:` lines; the patcher must not add a second one.
    assert_eq!(text.matches("event: response.created").count(), 1);
    assert_eq!(text.matches("event: response.output_text.delta").count(), 1);
    assert_eq!(text.matches("event: response.completed").count(), 1);
    assert_eq!(text.matches("event: ").count(), 3, "text was:\n{text}");
    assert!(text.ends_with("data: [DONE]\n\n"));
    assert_eq!(rec.last()["stream"], true);

    let record = wait_until(
        &dir,
        |r| r.protocol == "responses" && r.stream,
        std::time::Duration::from_secs(10),
    )
    .await
    .expect("streaming responses ledger row");
    assert_eq!(record.provider, "deepseek");
    // usage comes from `response.completed.response.usage`.
    assert_eq!(record.prompt_tokens, 11);
    assert_eq!(record.completion_tokens, 22);
    assert!(record.ttft_ms.is_some());
}

#[tokio::test]
async fn ds_responses_previous_response_id_is_still_400() {
    let dir = temp_dir("ds-resp-prev");
    let (app, _rec) = ds_gateway(&dir).await;
    let mut body = responses_body(false);
    body["model"] = json!("deepseek-flash");
    body["previous_response_id"] = json!("resp_does_not_exist");
    let response = app
        .oneshot(post_to("/v1/responses", body, "normal"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let value = body_json(response).await;
    assert_eq!(value["error"]["code"], "unsupported_parameter");
}

#[tokio::test]
async fn ds_messages_non_stream_relays_body_and_records_usage() {
    let dir = temp_dir("ds-msg");
    let (app, rec) = ds_gateway(&dir).await;
    let mut body = messages_body(false);
    body["model"] = json!("deepseek-flash");
    let response = app
        .oneshot(post_to("/v1/messages", body, "flat-cache"))
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
    let value = body_json(response).await;
    assert_eq!(value["type"], "message");
    // The DS Anthropic path, not the OpenAI one.
    assert_eq!(rec.last_uri(), "/anthropic/v1/messages");
    // Only `model` is rewritten and `anthropic-version` is filled in.
    assert_eq!(rec.last()["model"], "deepseek-flash");
    assert_eq!(rec.last()["thinking"]["type"], "enabled");
    assert_eq!(rec.last()["custom_beta_field"]["keep"], true);
    assert_eq!(
        rec.last_headers()
            .get("anthropic-version")
            .unwrap()
            .to_str()
            .unwrap(),
        "2023-06-01"
    );

    let record = wait_until(
        &dir,
        |r| r.protocol == "messages" && !r.stream,
        std::time::Duration::from_secs(10),
    )
    .await
    .expect("messages ledger row");
    assert_eq!(record.provider, "deepseek");
    assert_eq!(record.upstream_model, "deepseek-flash");
    // Flat `cache_creation_input_tokens` still leaves the shared fields intact.
    assert_eq!(record.prompt_tokens, 21);
    assert_eq!(record.completion_tokens, 9);
    assert_eq!(record.cached_tokens, 2);
}

#[tokio::test]
async fn ds_messages_stream_relays_events_and_reads_flat_usage() {
    let dir = temp_dir("ds-msg-stream");
    let (app, _rec) = ds_gateway(&dir).await;
    let mut body = messages_body(true);
    body["model"] = json!("deepseek-flash");
    let response = app
        .oneshot(post_to("/v1/messages?beta=true", body, "sse-flat"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "text/event-stream"
    );
    let text = String::from_utf8(body_bytes(response).await).unwrap();
    assert!(text.contains("event: message_start"));
    assert!(text.contains("event: message_stop"));
    assert_eq!(text.matches("event: ").count(), 7, "text was:\n{text}");

    let record = wait_until(
        &dir,
        |r| r.protocol == "messages" && r.stream,
        std::time::Duration::from_secs(10),
    )
    .await
    .expect("streaming messages ledger row");
    assert_eq!(record.provider, "deepseek");
    // message_start input/cache + message_delta output merge.
    assert_eq!(record.prompt_tokens, 21);
    assert_eq!(record.completion_tokens, 17);
    assert_eq!(record.cached_tokens, 4);
    assert!(record.ttft_ms.is_some());
}

#[tokio::test]
async fn ds_messages_query_string_is_preserved() {
    let dir = temp_dir("ds-msg-query");
    let (app, rec) = ds_gateway(&dir).await;
    let mut body = messages_body(false);
    body["model"] = json!("deepseek-flash");
    let response = app
        .oneshot(post_to("/v1/messages?beta=true", body, "normal"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(rec.last_uri(), "/anthropic/v1/messages?beta=true");
}

#[tokio::test]
async fn ds_count_tokens_is_proxied_to_the_anthropic_endpoint() {
    let dir = temp_dir("ds-count");
    let (app, rec) = ds_gateway(&dir).await;
    let body = json!({
        "model": "deepseek-flash",
        "messages": [{"role": "user", "content": "hello"}],
        "tools": [{"name": "Bash", "input_schema": {"type": "object"}}]
    });
    let response = app
        .oneshot(post_to("/v1/messages/count_tokens", body, "normal"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response.headers().get("x-umsp-provider").is_none(),
        "provider header must not leak to clients"
    );
    let value = body_json(response).await;
    assert_eq!(value["input_tokens"], 31);
    assert_eq!(rec.last_uri(), "/anthropic/v1/messages/count_tokens");
    // The body is relayed with only `model` rewritten; version defaulted.
    assert_eq!(rec.last()["model"], "deepseek-flash");
    assert_eq!(
        rec.last_headers()
            .get("anthropic-version")
            .unwrap()
            .to_str()
            .unwrap(),
        "2023-06-01"
    );

    let record = wait_until(
        &dir,
        |r| r.quirks.iter().any(|q| q == "count_tokens"),
        std::time::Duration::from_secs(10),
    )
    .await
    .expect("count_tokens ledger row");
    assert_eq!(record.protocol, "messages");
    assert_eq!(record.provider, "deepseek");
    assert_eq!(record.status, 200);
    assert_eq!(record.prompt_tokens, 31);
}

#[tokio::test]
async fn od_count_tokens_never_reaches_the_upstream() {
    let dir = temp_dir("od-count");
    let (app, _upstream, rec) = gateway(&dir, 50 * 1024 * 1024).await;
    let mut body = messages_body(false);
    body["model"] = json!("upstream-model");
    let response = app
        .oneshot(post_to("/v1/messages/count_tokens", body, "normal"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    let value = body_json(response).await;
    assert_eq!(value["error"]["type"], "not_supported_error");
    assert!(
        rec.uris.lock().unwrap().is_empty(),
        "OD count_tokens must not be forwarded"
    );
}

#[tokio::test]
async fn responses_translated_to_deepseek_chat_applies_chat_quirks() {
    let dir = temp_dir("ds-translated-quirks");
    let (upstream, rec) = start_upstream().await;
    let mut cfg = config(&dir, upstream.clone(), 52_428_800);
    add_deepseek(&mut cfg, &upstream);
    // Restrict DeepSeek to `chat` only so a `/v1/responses` call forces
    // translation (`responses -> chat`), after which DeepSeek Chat quirks must
    // still run on the translated body.
    cfg.providers
        .iter_mut()
        .find(|p| p.id == "deepseek")
        .expect("deepseek provider")
        .protocols = vec!["chat".to_string()];
    let state = umsp::server::AppState::new(cfg, env_file(&dir)).unwrap();
    let app = umsp::server::build_router(state);

    let body = json!({
        "model": "deepseek-flash",
        "input": [
            {"type": "message", "role": "user", "content": "run f"},
            {"type": "function_call", "call_id": "call_1", "name": "f", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "call_1", "output": "ok"}
        ],
        "tools": [{"type": "function", "name": "f", "parameters": {"type": "object"}}]
    });
    let response = app
        .oneshot(post_to("/v1/responses", body, "normal"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let seen = rec.last();
    assert_eq!(seen["model"], "deepseek-flash");
    // Post-translation DeepSeek quirks: user_id injected and assistant turn
    // with tools gets a non-empty reasoning_content placeholder.
    let user_id = seen["user_id"]
        .as_str()
        .expect("user_id must be injected after translation");
    assert!(user_id.starts_with("umsp-"), "got {user_id}");
    assert_eq!(seen["messages"][1]["role"], "assistant");
    assert_eq!(seen["messages"][1]["reasoning_content"], " ");

    let record = wait_until(
        &dir,
        |r| r.protocol == "responses",
        std::time::Duration::from_secs(10),
    )
    .await
    .expect("translated responses ledger row");
    assert!(
        record
            .quirks
            .contains(&"translated:responses->chat".to_string())
    );
    assert!(record.quirks.contains(&"injected_user_id".to_string()));
    assert!(
        record
            .quirks
            .contains(&"filled_reasoning_placeholder".to_string())
    );
}
