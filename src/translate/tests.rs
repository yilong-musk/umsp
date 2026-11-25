use serde_json::{Value, json};

use crate::sse::MAX_EVENT_BYTES;
use crate::stream;

use super::*;

fn protocols(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_string()).collect()
}

fn parse_sse_json_events(bytes: &[u8]) -> Vec<Value> {
    String::from_utf8(bytes.to_vec())
        .expect("utf-8 sse output")
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|line| *line != "[DONE]")
        .map(|line| serde_json::from_str::<Value>(line).expect("valid sse json"))
        .collect()
}

#[test]
fn native_protocol_wins_over_list_order() {
    let listed = protocols(&["responses", "chat", "messages"]);
    assert_eq!(select_upstream_protocol(&listed, "chat"), Some("chat"));
    assert_eq!(
        select_upstream_protocol(&protocols(&["responses"]), "messages"),
        Some("responses")
    );
    assert_eq!(select_upstream_protocol(&protocols(&[]), "chat"), None);
}

#[test]
fn chat_tool_turn_becomes_responses_items() {
    let body = json!({
        "model": "client-model",
        "messages": [
            {"role": "system", "content": "be brief"},
            {"role": "user", "content": "weather?"},
            {"role": "assistant", "content": null, "tool_calls": [{
                "id": "call_1",
                "type": "function",
                "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}
            }]},
            {"role": "tool", "tool_call_id": "call_1", "content": "sunny"}
        ],
        "tools": [{
            "type": "function",
            "function": {
                "name": "get_weather",
                "description": "lookup",
                "parameters": {"type": "object"}
            }
        }],
        "tool_choice": {"type": "function", "function": {"name": "get_weather"}},
        "max_tokens": 32,
        "stream": false
    });
    let translated = translate_request(&body, "chat", "responses").unwrap();
    assert_eq!(translated.body["instructions"], "be brief");
    assert_eq!(translated.body["max_output_tokens"], 32);
    assert_eq!(translated.body["tool_choice"]["name"], "get_weather");
    assert_eq!(translated.body["tools"][0]["name"], "get_weather");
    let input = translated.body["input"].as_array().unwrap();
    assert_eq!(input[0]["role"], "user");
    assert_eq!(input[1]["type"], "function_call");
    assert_eq!(input[1]["arguments"], "{\"city\":\"Paris\"}");
    assert_eq!(input[2]["type"], "function_call_output");
    assert_eq!(input[2]["output"], "sunny");
    assert!(translated.body.get("messages").is_none());
}

#[test]
fn messages_request_becomes_chat_and_defaults_are_noted() {
    let body = json!({
        "system": "be brief",
        "messages": [
            {"role": "user", "content": [{"type": "text", "text": "hi"}]},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "secret", "signature": "sig"},
                {"type": "tool_use", "id": "toolu_1", "name": "get_weather", "input": {"city": "Paris"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": "sunny"},
                {"type": "text", "text": "thanks"}
            ]}
        ],
        "tools": [{"name": "get_weather", "input_schema": {"type": "object"}}],
        "tool_choice": {"type": "any"}
    });
    let to_chat = translate_request(&body, "messages", "chat").unwrap();
    assert_eq!(to_chat.body["messages"][0]["role"], "system");
    assert_eq!(to_chat.body["messages"][0]["content"], "be brief");
    assert_eq!(to_chat.body["tool_choice"], "required");
    assert_eq!(
        to_chat.body["messages"][2]["tool_calls"][0]["id"],
        "toolu_1"
    );
    assert!(
        !to_chat.body.to_string().contains("signature"),
        "thinking signatures stay out of the translated body"
    );
    let chat_without_budget = json!({
        "messages": [{"role": "user", "content": "hi"}]
    });
    let to_messages = translate_request(&chat_without_budget, "chat", "messages").unwrap();
    assert!(
        to_messages
            .notes
            .iter()
            .any(|note| note == "defaulted_max_tokens"),
        "{:?}",
        to_messages.notes
    );
    assert_eq!(to_messages.body["max_tokens"], DEFAULT_MESSAGES_MAX_TOKENS);
}

#[test]
fn responses_body_becomes_chat_and_messages() {
    let upstream = json!({
        "id": "resp_1",
        "object": "response",
        "status": "completed",
        "output": [
            {"type": "message", "role": "assistant", "content": [
                {"type": "output_text", "text": "hi"}
            ]},
            {"type": "function_call", "call_id": "call_1", "name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}
        ],
        "usage": {"input_tokens": 11, "output_tokens": 22, "total_tokens": 33}
    });
    let bytes = serde_json::to_vec(&upstream).unwrap();
    let chat: Value = serde_json::from_slice(
        &translate_response(&bytes, "responses", "chat", "logical").unwrap(),
    )
    .unwrap();
    assert_eq!(chat["object"], "chat.completion");
    assert_eq!(chat["model"], "logical");
    assert_eq!(chat["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(chat["choices"][0]["message"]["content"], "hi");
    assert_eq!(
        chat["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
        "get_weather"
    );
    assert_eq!(chat["usage"]["prompt_tokens"], 11);
    let messages: Value = serde_json::from_slice(
        &translate_response(&bytes, "responses", "messages", "logical").unwrap(),
    )
    .unwrap();
    assert_eq!(messages["type"], "message");
    assert_eq!(messages["stop_reason"], "tool_use");
    assert_eq!(messages["content"][0]["text"], "hi");
    assert_eq!(messages["content"][1]["input"]["city"], "Paris");
    assert_eq!(messages["usage"]["output_tokens"], 22);
}

#[test]
fn responses_stream_becomes_chat_chunks_across_splits() {
    let raw = concat!(
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n",
        "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"get_weather\"}}\n\n",
        "data: {\"type\":\"response.function_call_arguments.delta\",\"delta\":\"{\\\"city\\\":",
        "\\\"Paris\\\"}\"}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"usage\":{\"input_tokens\":11,\"output_tokens\":22,\"total_tokens\":33}}}\n\n",
    );
    let mut bridge = StreamBridge::new("responses", "chat", "logical", true).unwrap();
    let mut out = Vec::new();
    let mut first = false;
    for chunk in raw.as_bytes().chunks(17) {
        if bridge.push(chunk, &mut out) {
            first = true;
        }
    }
    bridge.finish(&mut out);
    assert!(first);
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("\"content\":\"hi\""), "{text}");
    assert!(text.contains("get_weather"), "{text}");
    assert!(text.contains("Paris"), "{text}");
    assert!(text.contains("\"finish_reason\":\"tool_calls\""), "{text}");
    assert!(text.contains("data: [DONE]"), "{text}");
    assert_eq!(bridge.usage().unwrap().input_tokens, 11);
    assert!(bridge.done());
}

#[test]
fn chat_stream_tool_calls_finished_with_stop_still_end_as_tool_use() {
    let same_chunk = concat!(
        "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"{}\"}}]},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    let later_chunk = concat!(
        "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"{}\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    for raw in [same_chunk, later_chunk] {
        let mut bridge = StreamBridge::new("chat", "messages", "logical", false).unwrap();
        let mut out = Vec::new();
        bridge.push(raw.as_bytes(), &mut out);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("\"type\":\"tool_use\""), "{text}");
        assert!(text.contains("\"stop_reason\":\"tool_use\""), "{text}");
        assert!(!text.contains("end_turn"), "{text}");

        let mut bridge = StreamBridge::new("chat", "responses", "logical", false).unwrap();
        let mut out = Vec::new();
        bridge.push(raw.as_bytes(), &mut out);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("\"status\":\"completed\""), "{text}");
        assert!(text.contains("\"type\":\"function_call\""), "{text}");
    }
}

#[test]
fn chat_stream_becomes_messages_events() {
    let raw = concat!(
        "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"hi\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2,\"total_tokens\":5}}\n\n",
        "data: [DONE]\n\n",
    );
    let mut bridge = StreamBridge::new("chat", "messages", "logical", false).unwrap();
    let mut out = Vec::new();
    bridge.push(raw.as_bytes(), &mut out);
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("event: message_start"), "{text}");
    assert!(text.contains("\"text\":\"hi\""), "{text}");
    assert!(text.contains("\"stop_reason\":\"end_turn\""), "{text}");
    assert!(text.contains("event: message_stop"), "{text}");
    assert!(!text.contains("[DONE]"));
    assert!(bridge.done());
}

#[test]
fn responses_envelope_carries_the_fields_sdk_clients_require() {
    let upstream = json!({
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "created": 1700000000,
        "model": "upstream-model",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "hi"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}
    });
    let bytes = serde_json::to_vec(&upstream).unwrap();
    let translated = translate_response(&bytes, "chat", "responses", "logical").unwrap();
    let body: Value = serde_json::from_slice(&translated).unwrap();
    for key in [
        "id",
        "object",
        "created_at",
        "status",
        "model",
        "output",
        "parallel_tool_calls",
        "tool_choice",
        "tools",
        "usage",
    ] {
        assert!(body.get(key).is_some(), "missing {key}: {body}");
    }
    assert_eq!(body["output"][0]["status"], "completed");
    assert_eq!(body["output"][0]["content"][0]["annotations"], json!([]));
}

#[test]
fn chat_stream_translates_to_a_complete_responses_event_sequence() {
    let raw = concat!(
        "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"hi\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"f\",\"arguments\":\"{\\\"a\\\":\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"1}\"}}]},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2,\"total_tokens\":5}}\n\n",
        "data: [DONE]\n\n"
    );
    let mut bridge = StreamBridge::new("chat", "responses", "logical", true).unwrap();
    let mut out = Vec::new();
    bridge.push(raw.as_bytes(), &mut out);
    bridge.finish(&mut out);
    let text = String::from_utf8(out).unwrap();
    assert!(!text.contains("data: [DONE]"), "{text}");
    let events = parse_sse_json_events(text.as_bytes());
    assert!(events.len() >= 7, "{text}");
    for event in &events {
        assert!(event.get("sequence_number").is_some(), "{event}");
    }
    let types: Vec<&str> = events
        .iter()
        .filter_map(|event| event["type"].as_str())
        .collect();
    assert!(
        types.contains(&"response.content_part.added"),
        "missing content_part.added: {text}"
    );
    assert!(
        types.contains(&"response.output_text.done"),
        "missing output_text.done: {text}"
    );
    let delta = events
        .iter()
        .find(|event| event["type"] == "response.output_text.delta")
        .unwrap();
    assert_eq!(delta["content_index"], 0);
    assert_eq!(delta["logprobs"], json!([]));
    assert!(delta["item_id"].as_str().unwrap().starts_with("msg_"));
    let completed = events
        .iter()
        .find(|event| event["type"] == "response.completed")
        .unwrap();
    let output = completed["response"]["output"].as_array().unwrap();
    assert_eq!(output.len(), 2, "{completed}");
    assert_eq!(output[0]["content"][0]["text"], "hi");
    assert_eq!(output[1]["arguments"], "{\"a\":1}");
}

#[test]
fn truncated_chat_stream_ends_responses_clients_with_response_incomplete() {
    let raw = concat!(
        "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"hi\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    let mut bridge = StreamBridge::new("chat", "responses", "logical", true).unwrap();
    let mut out = Vec::new();
    bridge.push(raw.as_bytes(), &mut out);
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("event: response.incomplete\n"), "{text}");
    assert!(!text.contains("response.completed"), "{text}");
    let terminal: Value = parse_sse_json_events(text.as_bytes())
        .into_iter()
        .find(|event| event["type"] == "response.incomplete")
        .expect("response.incomplete event");
    assert_eq!(terminal["response"]["status"], "incomplete");
    assert_eq!(
        terminal["response"]["incomplete_details"]["reason"],
        "max_output_tokens"
    );
    assert!(bridge.done());
}

#[test]
fn upstream_response_incomplete_keeps_a_content_filter_reason() {
    let raw = concat!(
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n",
        "data: {\"type\":\"response.incomplete\",\"response\":{\"id\":\"resp_1\",\"status\":\"incomplete\",\"incomplete_details\":{\"reason\":\"content_filter\"}}}\n\n",
    );
    let mut bridge = StreamBridge::new("responses", "chat", "logical", false).unwrap();
    let mut out = Vec::new();
    bridge.push(raw.as_bytes(), &mut out);
    let text = String::from_utf8(out).unwrap();
    assert!(
        text.contains("\"finish_reason\":\"content_filter\""),
        "{text}"
    );

    let mut bridge = StreamBridge::new("responses", "messages", "logical", false).unwrap();
    let mut out = Vec::new();
    bridge.push(raw.as_bytes(), &mut out);
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("\"stop_reason\":\"refusal\""), "{text}");
}

#[test]
fn bridge_and_native_responses_error_events_share_one_shape() {
    let mut bridge = StreamBridge::new("chat", "responses", "logical", true).unwrap();
    let mut out = Vec::new();
    bridge.push(
        b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
        &mut out,
    );
    let parse = |bytes: Vec<u8>| -> Value {
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.starts_with("event: error\n"), "{text}");
        let data = text
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .unwrap();
        serde_json::from_str(data).unwrap()
    };
    let bridged = parse(bridge.error_event("boom"));
    let native = parse(stream::Observer::responses(true).error_event("boom"));
    for event in [&bridged, &native] {
        assert_eq!(event["type"], "error", "{event}");
        assert_eq!(event["code"], "upstream_stream_error", "{event}");
        assert_eq!(event["message"], "boom", "{event}");
        assert!(event["param"].is_null(), "{event}");
        assert!(event.get("error").is_none(), "{event}");
    }
    assert!(
        bridged["sequence_number"].as_u64().unwrap() > 0,
        "{bridged}"
    );
}

#[test]
fn responses_item_done_arguments_that_do_not_match_deltas_do_not_panic() {
    let raw = concat!(
        "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"f\"}}\n\n",
        "data: {\"type\":\"response.function_call_arguments.delta\",\"delta\":\"€€\"}\n\n",
        "data: {\"type\":\"response.function_call_arguments.delta\",\"delta\":\"a\"}\n\n",
        "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"f\",\"arguments\":\"{\\\"x\\\":\\\"€€\\\"}\"}}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\"}}\n\n"
    );
    let mut bridge = StreamBridge::new("responses", "chat", "logical", false).unwrap();
    let mut out = Vec::new();
    bridge.push(raw.as_bytes(), &mut out);
    bridge.finish(&mut out);
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("€€"), "{text}");
    assert!(text.contains("\"arguments\":\"a\""), "{text}");
    assert!(bridge.done());
}

#[test]
fn messages_usage_keeps_input_tokens_across_message_delta() {
    let raw = concat!(
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{\"input_tokens\":10,\"output_tokens\":1,\"cache_read_input_tokens\":4,\"cache_creation\":{\"ephemeral_5m_input_tokens\":3}}}}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
    );
    let mut bridge = StreamBridge::new("messages", "chat", "logical", false).unwrap();
    let mut out = Vec::new();
    bridge.push(raw.as_bytes(), &mut out);
    let usage = bridge.usage().unwrap();
    assert_eq!(usage.input_tokens, 10);
    assert_eq!(usage.output_tokens, 5);
    assert_eq!(usage.cached_tokens, 4);
    assert_eq!(usage.cache_creation_tokens, 3);
}

#[test]
fn response_failed_becomes_an_error_instead_of_a_completed_terminal() {
    let raw = concat!(
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n",
        "data: {\"type\":\"response.failed\",\"response\":{\"id\":\"resp_1\",\"error\":{\"message\":\"boom\"}}}\n\n"
    );
    let mut bridge = StreamBridge::new("responses", "chat", "logical", false).unwrap();
    let mut out = Vec::new();
    bridge.push(raw.as_bytes(), &mut out);
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("boom"), "{text}");
    assert!(!text.contains("\"finish_reason\":\"stop\""), "{text}");
    assert!(bridge.done());
}

#[test]
fn a_clean_eof_without_a_terminal_event_still_terminates_chat_streams() {
    let raw = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n";
    let mut bridge = StreamBridge::new("responses", "chat", "logical", false).unwrap();
    let mut out = Vec::new();
    bridge.push(raw.as_bytes(), &mut out);
    bridge.finish(&mut out);
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("\"finish_reason\":\"stop\""), "{text}");
    assert!(text.contains("data: [DONE]"), "{text}");
}

#[test]
fn parallel_chat_tool_calls_stay_separate_when_translated() {
    let raw = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_a\",\"function\":{\"name\":\"fa\",\"arguments\":\"{\\\"a\\\":\"}},{\"index\":1,\"id\":\"call_b\",\"function\":{\"name\":\"fb\",\"arguments\":\"{\\\"b\\\":\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"function\":{\"arguments\":\"2}\"}},{\"index\":0,\"function\":{\"arguments\":\"1}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let mut bridge = StreamBridge::new("chat", "messages", "logical", true).unwrap();
    let mut out = Vec::new();
    bridge.push(raw.as_bytes(), &mut out);
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("\"id\":\"call_a\""), "{text}");
    assert!(text.contains("\"id\":\"call_b\""), "{text}");
    assert!(text.contains("{\\\"a\\\":1}"), "{text}");
    assert!(text.contains("{\\\"b\\\":2}"), "{text}");
    assert_eq!(bridge.usage(), None);
}

#[test]
fn sampling_parameters_are_clamped_for_messages() {
    let body = json!({
        "messages": [{"role": "user", "content": "hi"}],
        "temperature": 1.6,
        "top_p": 1.2,
        "max_tokens": 16
    });
    let translated = translate_request(&body, "chat", "messages").unwrap();
    assert_eq!(translated.body["temperature"], 1.0);
    assert_eq!(translated.body["top_p"], 1.0);
    assert!(
        translated
            .notes
            .iter()
            .any(|note| note == "clamped_temperature")
    );
    assert!(translated.notes.iter().any(|note| note == "clamped_top_p"));
}

#[test]
fn messages_budget_uses_the_model_limit_when_the_request_omits_it() {
    let body = json!({"messages": [{"role": "user", "content": "hi"}]});
    let translated =
        translate_request_with_budget(&body, "chat", "messages", Some(64_000)).unwrap();
    assert_eq!(translated.body["max_tokens"], 64_000);
    assert!(
        translated
            .notes
            .iter()
            .any(|note| note == "defaulted_max_tokens")
    );
}

#[test]
fn server_tools_are_dropped_with_a_note_instead_of_promoted() {
    let body = json!({
        "messages": [{"role": "user", "content": "search"}],
        "tools": [
            {"name": "local", "input_schema": {"type": "object"}},
            {"type": "web_search_20250305", "name": "web_search", "max_uses": 3}
        ]
    });
    let translated = translate_request(&body, "messages", "chat").unwrap();
    let tools = translated.body["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["function"]["name"], "local");
    assert!(
        translated
            .notes
            .iter()
            .any(|note| note == "dropped_server_tool:web_search")
    );
}

#[test]
fn responses_custom_tools_are_dropped_with_a_note() {
    let body = json!({
        "input": "hi",
        "tools": [
            {"type": "function", "name": "f", "parameters": {"type": "object"}},
            {"type": "custom", "name": "freeform"}
        ]
    });
    let translated = translate_request(&body, "responses", "chat").unwrap();
    assert_eq!(translated.body["tools"].as_array().unwrap().len(), 1);
    assert!(
        translated
            .notes
            .iter()
            .any(|note| note == "dropped_server_tool:freeform")
    );
}

#[test]
fn response_format_survives_chat_to_responses_and_is_noted_for_messages() {
    let schema = json!({"type": "object", "properties": {"city": {"type": "string"}}});
    let body = json!({
        "messages": [{"role": "user", "content": "weather"}],
        "response_format": {
            "type": "json_schema",
            "json_schema": {"name": "weather", "schema": schema, "strict": true}
        }
    });
    let to_responses = translate_request(&body, "chat", "responses").unwrap();
    assert_eq!(to_responses.body["text"]["format"]["type"], "json_schema");
    assert_eq!(to_responses.body["text"]["format"]["name"], "weather");
    assert_eq!(to_responses.body["text"]["format"]["schema"], schema);
    let to_messages = translate_request(&body, "chat", "messages").unwrap();
    assert!(
        to_messages
            .notes
            .iter()
            .any(|note| note == "dropped_response_format")
    );
}

#[test]
fn responses_text_format_becomes_chat_response_format() {
    let body = json!({
        "input": "weather",
        "text": {"format": {
            "type": "json_schema",
            "name": "weather",
            "schema": {"type": "object"}
        }}
    });
    let translated = translate_request(&body, "responses", "chat").unwrap();
    assert_eq!(translated.body["response_format"]["type"], "json_schema");
    assert_eq!(
        translated.body["response_format"]["json_schema"]["name"],
        "weather"
    );
}

#[test]
fn empty_user_parts_and_assistant_images_do_not_emit_invalid_blocks() {
    let empty = json!({"messages": [{"role": "user", "content": []}]});
    let to_responses = translate_request(&empty, "chat", "responses").unwrap();
    assert_eq!(
        to_responses.body["input"][0]["content"],
        json!([{"type": "input_text", "text": ""}])
    );

    let assistant_image = json!({
        "messages": [
            {"role": "user", "content": "look"},
            {"role": "assistant", "content": [
                {"type": "image_url", "image_url": {"url": "https://example.test/a.png"}}
            ]}
        ]
    });
    let translated = translate_request(&assistant_image, "chat", "responses").unwrap();
    assert!(
        translated
            .notes
            .iter()
            .any(|note| note == "dropped_assistant_image")
    );
    let input = translated.body["input"].as_array().unwrap();
    assert!(
        !input.iter().any(|item| item["role"] == "assistant"),
        "{}",
        translated.body
    );
}

#[test]
fn tool_result_errors_and_images_are_made_explicit() {
    let body = json!({
        "messages": [{
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": "toolu_1",
                "is_error": true,
                "content": [
                    {"type": "text", "text": "boom"},
                    {"type": "image", "source": {
                        "type": "base64", "media_type": "image/png", "data": "AAAA"
                    }}
                ]
            }]
        }]
    });
    let translated = translate_request(&body, "messages", "chat").unwrap();
    assert_eq!(
        translated.body["messages"][0]["content"],
        "error: boom [1 image(s) omitted]"
    );
}

#[test]
fn content_filter_finish_reasons_survive_translation() {
    let refusal = json!({
        "id": "msg_1",
        "content": [{"type": "text", "text": "no"}],
        "stop_reason": "refusal",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    });
    let to_chat = translate_response(
        &serde_json::to_vec(&refusal).unwrap(),
        "messages",
        "chat",
        "m",
    )
    .unwrap();
    let chat: Value = serde_json::from_slice(&to_chat).unwrap();
    assert_eq!(chat["choices"][0]["finish_reason"], "content_filter");

    let filtered = json!({
        "id": "chatcmpl-1",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "no"},
            "finish_reason": "content_filter"
        }]
    });
    let to_messages = translate_response(
        &serde_json::to_vec(&filtered).unwrap(),
        "chat",
        "messages",
        "m",
    )
    .unwrap();
    let messages: Value = serde_json::from_slice(&to_messages).unwrap();
    assert_eq!(messages["stop_reason"], "refusal");
}

#[test]
fn object_tool_arguments_are_preserved_and_empty_descriptions_omitted() {
    let body = json!({
        "messages": [
            {"role": "user", "content": "weather"},
            {"role": "assistant", "content": null, "tool_calls": [{
                "id": "call_1",
                "type": "function",
                "function": {"name": "f", "arguments": {"city": "Paris"}}
            }]}
        ],
        "tools": [{"type": "function", "function": {"name": "f", "parameters": null}}]
    });
    let translated = translate_request(&body, "chat", "messages").unwrap();
    assert_eq!(
        translated.body["messages"][1]["content"][0]["input"]["city"],
        "Paris"
    );
    let tool = &translated.body["tools"][0];
    assert!(tool.get("description").is_none(), "{tool}");
    assert_eq!(tool["input_schema"], json!({"type": "object"}));
}

#[test]
fn unknown_tool_choice_shapes_are_noted_and_named_choices_follow_dropped_tools() {
    let unknown = json!({
        "messages": [{"role": "user", "content": "hi"}],
        "tools": [{"name": "f", "input_schema": {"type": "object"}}],
        "tool_choice": {"type": "allowed_tools", "mode": "auto"}
    });
    let translated = translate_request(&unknown, "messages", "chat").unwrap();
    assert!(
        translated
            .notes
            .iter()
            .any(|note| note == "dropped_tool_choice")
    );

    let named = json!({
        "messages": [{"role": "user", "content": "hi"}],
        "tools": [{"type": "web_search_20250305", "name": "web_search"}],
        "tool_choice": {"type": "tool", "name": "web_search"}
    });
    let translated = translate_request(&named, "messages", "chat").unwrap();
    assert!(
        translated
            .notes
            .iter()
            .any(|note| note == "dropped_tool_choice:web_search")
    );
    assert!(translated.body.get("tool_choice").is_none());
}

#[test]
fn n_and_parallel_tool_calls_are_mapped_or_noted() {
    let body = json!({
        "messages": [{"role": "user", "content": "hi"}],
        "n": 3,
        "parallel_tool_calls": false
    });
    let to_responses = translate_request(&body, "chat", "responses").unwrap();
    assert_eq!(to_responses.body["parallel_tool_calls"], false);
    assert!(to_responses.notes.iter().any(|note| note == "dropped_n"));

    let to_messages = translate_request(&body, "chat", "messages").unwrap();
    assert!(to_messages.notes.iter().any(|note| note == "dropped_n"));
    assert!(
        to_messages
            .notes
            .iter()
            .any(|note| note == "dropped_parallel_tool_calls")
    );
}

#[test]
fn failed_responses_are_errors_instead_of_truncated_successes() {
    let failed = json!({
        "id": "resp_1",
        "status": "failed",
        "error": {"message": "boom"},
        "output": []
    });
    let bytes = serde_json::to_vec(&failed).unwrap();
    assert!(translate_response(&bytes, "responses", "chat", "logical").is_err());
}

#[test]
fn oversized_stream_events_end_the_stream_with_an_error() {
    let mut bridge = StreamBridge::new("responses", "messages", "logical", false).unwrap();
    let mut out = Vec::new();
    bridge.push(&vec![b'x'; MAX_EVENT_BYTES + 1], &mut out);
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("oversized"), "{text}");
    assert!(bridge.done());
}

// ---------------------------------------------------------------------------
// Extended streaming coverage: Messages <-> Responses, tool_use, text->tool->text,
// split UTF-8, and chunk-boundary invariance.
// ---------------------------------------------------------------------------

const MESSAGES_TOOL_STREAM: &str = concat!(
    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_42\",\"usage\":{\"input_tokens\":14,\"output_tokens\":1}}}\n\n",
    "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Let me check \"}}\n\n",
    "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_9\",\"name\":\"lookup\",\"input\":{}}}\n\n",
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"q\\\":\"}}\n\n",
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"€\\\"}\"}}\n\n",
    "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
    "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"text_delta\",\"text\":\"done.\"}}\n\n",
    "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":2}\n\n",
    "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":23}}\n\n",
    "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
);

const RESPONSES_TEXT_TOOL_TEXT_STREAM: &str = concat!(
    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"first \"}\n\n",
    "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"first \"}]}}\n\n",
    "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_7\",\"name\":\"calc\"}}\n\n",
    "data: {\"type\":\"response.function_call_arguments.delta\",\"delta\":\"{\\\"n\\\":1}\"}\n\n",
    "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_7\",\"name\":\"calc\",\"arguments\":\"{\\\"n\\\":1}\"}}\n\n",
    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"after €\"}\n\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_7\",\"usage\":{\"input_tokens\":8,\"output_tokens\":12,\"total_tokens\":20}}}\n\n",
);

#[test]
fn messages_stream_becomes_responses_preserving_text_tool_text_order() {
    let mut bridge = StreamBridge::new("messages", "responses", "logical", true).unwrap();
    let mut out = Vec::new();
    bridge.push(MESSAGES_TOOL_STREAM.as_bytes(), &mut out);
    bridge.finish(&mut out);

    let events = parse_sse_json_events(&out);
    let completed = events
        .iter()
        .find(|e| e["type"] == "response.completed")
        .expect("response.completed");
    let output = completed["response"]["output"].as_array().unwrap();
    assert_eq!(output.len(), 3, "{completed}");
    assert_eq!(output[0]["type"], "message");
    assert_eq!(output[0]["content"][0]["text"], "Let me check ");
    assert_eq!(output[1]["type"], "function_call");
    assert_eq!(output[1]["call_id"], "toolu_9");
    assert_eq!(output[1]["name"], "lookup");
    assert_eq!(output[1]["arguments"], "{\"q\":\"€\"}");
    assert_eq!(output[2]["type"], "message");
    assert_eq!(output[2]["content"][0]["text"], "done.");
    assert_eq!(completed["response"]["usage"]["input_tokens"], 14);
    assert_eq!(completed["response"]["usage"]["output_tokens"], 23);
    // Sequence numbers must be strictly increasing from 0.
    for (idx, event) in events.iter().enumerate() {
        assert_eq!(event["sequence_number"], idx as u64, "{event}");
    }
    assert!(bridge.done());
}

#[test]
fn messages_stream_tool_use_becomes_chat() {
    let mut bridge = StreamBridge::new("messages", "chat", "logical", false).unwrap();
    let mut out = Vec::new();
    bridge.push(MESSAGES_TOOL_STREAM.as_bytes(), &mut out);
    bridge.finish(&mut out);

    let text = String::from_utf8(out).unwrap();
    assert!(text.ends_with("data: [DONE]\n\n"), "{text}");
    let events = parse_sse_json_events(text.as_bytes());
    let mut content = String::new();
    let mut args = String::new();
    let mut tool_name = None;
    let mut finish_reason = None;
    for event in &events {
        let choice = &event["choices"][0];
        if let Some(c) = choice["delta"]["content"].as_str() {
            content.push_str(c);
        }
        if let Some(calls) = choice["delta"]["tool_calls"].as_array() {
            for call in calls {
                if let Some(name) = call["function"]["name"].as_str() {
                    tool_name = Some(name.to_string());
                }
                if let Some(a) = call["function"]["arguments"].as_str() {
                    args.push_str(a);
                }
            }
        }
        if let Some(reason) = choice["finish_reason"].as_str() {
            finish_reason = Some(reason.to_string());
        }
    }
    assert_eq!(content, "Let me check done.");
    assert_eq!(tool_name.as_deref(), Some("lookup"));
    assert_eq!(args, "{\"q\":\"€\"}");
    assert_eq!(finish_reason.as_deref(), Some("tool_calls"));
    assert!(bridge.done());
}

#[test]
fn responses_stream_becomes_messages_preserving_text_tool_text_blocks() {
    let mut bridge = StreamBridge::new("responses", "messages", "logical", true).unwrap();
    let mut out = Vec::new();
    bridge.push(RESPONSES_TEXT_TOOL_TEXT_STREAM.as_bytes(), &mut out);
    bridge.finish(&mut out);

    let events = parse_sse_json_events(&out);
    let starts: Vec<(u64, &str)> = events
        .iter()
        .filter(|e| e["type"] == "content_block_start")
        .map(|e| {
            (
                e["index"].as_u64().unwrap(),
                e["content_block"]["type"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(starts, vec![(0, "text"), (1, "tool_use"), (2, "text")]);
    let stops: Vec<u64> = events
        .iter()
        .filter(|e| e["type"] == "content_block_stop")
        .map(|e| e["index"].as_u64().unwrap())
        .collect();
    assert_eq!(stops, vec![0, 1, 2]);
    let delta = events
        .iter()
        .find(|e| e["type"] == "message_delta")
        .expect("message_delta");
    assert_eq!(delta["delta"]["stop_reason"], "tool_use");
    assert_eq!(delta["usage"]["output_tokens"], 12);
    assert!(bridge.done());
}

#[test]
fn multibyte_utf8_split_byte_by_byte_survives_stream_bridge() {
    // 2-byte (`é`), 3-byte (`€`), and 4-byte (`🚀`) UTF-8 sequences split across chunks.
    let raw = "data: {\"choices\":[{\"delta\":{\"content\":\"é€🚀\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
    let mut bridge = StreamBridge::new("chat", "responses", "logical", true).unwrap();
    let mut out = Vec::new();
    for byte in raw.as_bytes().chunks(1) {
        bridge.push(byte, &mut out);
    }
    bridge.finish(&mut out);
    let events = parse_sse_json_events(&out);
    let completed = events
        .iter()
        .find(|e| e["type"] == "response.completed")
        .expect("completed");
    assert_eq!(
        completed["response"]["output"][0]["content"][0]["text"],
        "é€🚀"
    );
    assert!(bridge.done());
}

#[test]
fn stream_bridge_output_is_invariant_to_chunk_boundaries() {
    const CHAT_STREAM: &str = concat!(
        "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"hello €\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"f\",\"arguments\":\"{\\\"k\\\":\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"v\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":7,\"total_tokens\":12}}\n\n",
        "data: [DONE]\n\n",
    );

    let cases = [
        ("chat", "responses", CHAT_STREAM),
        ("chat", "messages", CHAT_STREAM),
        ("responses", "chat", RESPONSES_TEXT_TOOL_TEXT_STREAM),
        ("responses", "messages", RESPONSES_TEXT_TOOL_TEXT_STREAM),
        ("messages", "chat", MESSAGES_TOOL_STREAM),
        ("messages", "responses", MESSAGES_TOOL_STREAM),
    ];

    let run = |from: &str, to: &str, raw: &[u8], mut seed: u64, fixed: Option<usize>| {
        let mut bridge = StreamBridge::new(from, to, "logical", true).unwrap();
        let mut out = Vec::new();
        let mut pos = 0;
        while pos < raw.len() {
            let step = match fixed {
                Some(n) => n,
                None => {
                    // Deterministic xorshift64 chunk size in 1..=23 bytes.
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    ((seed % 23) + 1) as usize
                }
            };
            let end = (pos + step).min(raw.len());
            bridge.push(&raw[pos..end], &mut out);
            pos = end;
        }
        bridge.finish(&mut out);
        // Strip wall-clock timestamps (`created` / `created_at`) before comparing.
        let mut events = parse_sse_json_events(&out);
        for event in &mut events {
            if let Some(obj) = event.as_object_mut() {
                obj.remove("created");
                if let Some(resp) = obj.get_mut("response").and_then(Value::as_object_mut) {
                    resp.remove("created_at");
                }
            }
        }
        (events, bridge.usage(), bridge.done())
    };

    for (from, to, raw) in cases {
        let baseline = run(from, to, raw.as_bytes(), 0, Some(raw.len()));
        assert!(baseline.2, "{from}->{to} must finish done");
        for fixed in [Some(1), Some(2), Some(7)] {
            let split = run(from, to, raw.as_bytes(), 0, fixed);
            assert_eq!(split, baseline, "{from}->{to} fixed={fixed:?}");
        }
        for seed in [1u64, 42, 9999, 0xdead_beef_cafe] {
            let split = run(from, to, raw.as_bytes(), seed, None);
            assert_eq!(split, baseline, "{from}->{to} seed={seed}");
        }
    }
}
