//! Provider quirks. M0 implements the OpenDesign Chat Completions fixes that
//! were verified against the live upstream (spec 2.1).
//!
//! Everything here is *additive*: we never rewrite fields we do not own,
//! `reasoning_details` and unknown fields pass through untouched.

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::util::hex_encode;

/// Request-side quirks: OD returns `choices: []` when both `max_tokens` and
/// `max_completion_tokens` are present, so drop the newer alias and keep
/// `max_tokens`. Only applies when both are present.
pub fn apply_request_quirks(body: &mut Value) -> Vec<String> {
    let mut applied = Vec::new();
    let Some(obj) = body.as_object_mut() else {
        return applied;
    };
    let has_max_tokens = obj.get("max_tokens").is_some_and(|v| !v.is_null());
    let has_max_completion = obj
        .get("max_completion_tokens")
        .is_some_and(|v| !v.is_null());
    if has_max_tokens && has_max_completion {
        obj.remove("max_completion_tokens");
        applied.push("dropped_max_completion_tokens".to_string());
    }
    applied
}

/// The effective output budget the client asked for (for finish_reason inference).
pub fn effective_max_tokens(body: &Value) -> Option<u64> {
    body.get("max_tokens")
        .and_then(Value::as_u64)
        .or_else(|| body.get("max_completion_tokens").and_then(Value::as_u64))
}

/// DeepSeek Chat request quirks (spec 2.1, DeepSeek upstream).
///
/// - With `tools` in play, every historical assistant message must carry a
///   non-empty `reasoning_content` or DeepSeek rejects the request with 400.
///   Missing text is backfilled from `reasoning`, then from a single space.
/// - `reasoning_details` is an OpenDesign extension (it carries encrypted
///   blobs); DeepSeek only understands the plain text, so it is dropped.
/// - `user_id` pins KVCache isolation per client. An explicit client value wins.
/// - `thinking` / `reasoning_effort` are never defaulted: they pass through.
pub fn apply_deepseek_request_quirks(body: &mut Value, client_name: &str) -> Vec<String> {
    let mut applied = Vec::new();
    let Some(obj) = body.as_object_mut() else {
        return applied;
    };
    let has_tools = obj
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| !tools.is_empty());

    if let Some(messages) = obj.get_mut("messages").and_then(Value::as_array_mut) {
        for message in messages.iter_mut() {
            let Some(message) = message.as_object_mut() else {
                continue;
            };
            if message.remove("reasoning_details").is_some() {
                push_once(&mut applied, "dropped_reasoning_details");
            }
            if !has_tools || message.get("role").and_then(Value::as_str) != Some("assistant") {
                continue;
            }
            let satisfied = message
                .get("reasoning_content")
                .and_then(Value::as_str)
                .is_some_and(|text| !text.is_empty());
            if satisfied {
                continue;
            }
            let reasoning = message
                .get("reasoning")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .map(str::to_string);
            match reasoning {
                Some(text) => {
                    message.insert("reasoning_content".to_string(), Value::String(text));
                    push_once(&mut applied, "filled_reasoning_content");
                }
                None => {
                    message.insert("reasoning_content".to_string(), Value::String(" ".into()));
                    push_once(&mut applied, "filled_reasoning_placeholder");
                }
            }
        }
    }

    if obj.get("user_id").is_none_or(Value::is_null) {
        obj.insert(
            "user_id".to_string(),
            Value::String(stable_user_id(client_name)),
        );
        push_once(&mut applied, "injected_user_id");
    }
    applied
}

/// Stable per-client `user_id` for DeepSeek KVCache isolation. The client name
/// is the identity the gateway authenticates; hashing keeps the wire value
/// opaque and stable across restarts.
pub fn stable_user_id(client_name: &str) -> String {
    let digest = Sha256::digest(client_name.as_bytes());
    format!("umsp-{}", &hex_encode(&digest)[..16])
}

fn push_once(applied: &mut Vec<String>, label: &str) {
    if !applied.iter().any(|existing| existing == label) {
        applied.push(label.to_string());
    }
}

/// Response-side quirks for a non-streaming Chat Completion.
pub fn apply_chat_response_quirks(resp: &mut Value, effective_max: Option<u64>) -> Vec<String> {
    let mut applied = Vec::new();
    let completion_tokens = resp
        .get("usage")
        .and_then(|u| u.get("completion_tokens"))
        .and_then(Value::as_u64);

    let Some(obj) = resp.as_object_mut() else {
        return applied;
    };

    // `choices: []` when reasoning ate the whole budget -> synthesize an empty
    // assistant message with finish_reason "length".
    let empty_choices = obj
        .get("choices")
        .and_then(Value::as_array)
        .is_some_and(|c| c.is_empty());
    if empty_choices {
        obj.insert(
            "choices".to_string(),
            serde_json::json!([{
                "index": 0,
                "message": { "role": "assistant", "content": "" },
                "finish_reason": "length"
            }]),
        );
        applied.push("synthesized_empty_choices".to_string());
    }

    if let Some(choices) = obj.get_mut("choices").and_then(Value::as_array_mut) {
        for choice in choices.iter_mut() {
            let Some(choice_obj) = choice.as_object_mut() else {
                continue;
            };

            // Missing finish_reason: OD truncation drops it. Infer from the
            // budget: if completion_tokens hit the max, it was a length stop.
            let needs_finish = match choice_obj.get("finish_reason") {
                None | Some(Value::Null) => true,
                Some(_) => false,
            };
            if needs_finish {
                let reason = match (completion_tokens, effective_max) {
                    (Some(used), Some(max)) if max > 0 && used >= max => "length",
                    _ => "stop",
                };
                choice_obj.insert(
                    "finish_reason".to_string(),
                    Value::String(reason.to_string()),
                );
                applied.push("filled_finish_reason".to_string());
            }

            // Non-standard `message.tool_calls[].index`: strip it for non-stream.
            if let Some(tool_calls) = choice_obj
                .get_mut("message")
                .and_then(|m| m.get_mut("tool_calls"))
                .and_then(Value::as_array_mut)
            {
                let mut stripped = false;
                for call in tool_calls.iter_mut() {
                    if let Some(call_obj) = call.as_object_mut() {
                        if call_obj.remove("index").is_some() {
                            stripped = true;
                        }
                    }
                }
                if stripped {
                    applied.push("stripped_tool_call_index".to_string());
                }
            }
        }
    }

    applied
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn drops_max_completion_when_both_present() {
        let mut body = json!({"model": "m", "max_tokens": 10, "max_completion_tokens": 20});
        let applied = apply_request_quirks(&mut body);
        assert_eq!(applied, vec!["dropped_max_completion_tokens"]);
        assert_eq!(body["max_tokens"], 10);
        assert!(body.get("max_completion_tokens").is_none());
    }

    #[test]
    fn keeps_max_completion_when_alone() {
        let mut body = json!({"model": "m", "max_completion_tokens": 20});
        let applied = apply_request_quirks(&mut body);
        assert!(applied.is_empty());
        assert_eq!(body["max_completion_tokens"], 20);
    }

    #[test]
    fn fills_missing_finish_reason_as_length() {
        let mut resp = json!({
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "x"}}],
            "usage": {"completion_tokens": 128}
        });
        let applied = apply_chat_response_quirks(&mut resp, Some(128));
        assert_eq!(resp["choices"][0]["finish_reason"], "length");
        assert!(applied.contains(&"filled_finish_reason".to_string()));
    }

    #[test]
    fn fills_missing_finish_reason_as_stop() {
        let mut resp = json!({
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "x"}}],
            "usage": {"completion_tokens": 5}
        });
        apply_chat_response_quirks(&mut resp, Some(128));
        assert_eq!(resp["choices"][0]["finish_reason"], "stop");
    }

    #[test]
    fn keeps_existing_finish_reason() {
        let mut resp = json!({
            "choices": [{"index": 0, "message": {"content": "x"}, "finish_reason": "tool_calls"}],
            "usage": {"completion_tokens": 5}
        });
        let applied = apply_chat_response_quirks(&mut resp, Some(128));
        assert_eq!(resp["choices"][0]["finish_reason"], "tool_calls");
        assert!(applied.is_empty());
    }

    #[test]
    fn synthesizes_empty_choices() {
        let mut resp = json!({"choices": [], "usage": {"completion_tokens": 900}});
        let applied = apply_chat_response_quirks(&mut resp, Some(900));
        assert_eq!(resp["choices"][0]["message"]["role"], "assistant");
        assert_eq!(resp["choices"][0]["message"]["content"], "");
        assert_eq!(resp["choices"][0]["finish_reason"], "length");
        assert!(applied.contains(&"synthesized_empty_choices".to_string()));
    }

    #[test]
    fn strips_tool_call_index_and_keeps_reasoning_details() {
        let mut resp = json!({
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": null,
                    "reasoning_details": [{"type": "reasoning.summary", "summary": "s"}],
                    "tool_calls": [
                        {"index": 0, "id": "call_1", "type": "function",
                         "function": {"name": "f", "arguments": "{}"}}
                    ]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"completion_tokens": 3}
        });
        apply_chat_response_quirks(&mut resp, None);
        assert!(
            resp["choices"][0]["message"]["tool_calls"][0]
                .get("index")
                .is_none()
        );
        assert_eq!(
            resp["choices"][0]["message"]["reasoning_details"][0]["type"],
            "reasoning.summary"
        );
        assert_eq!(
            resp["choices"][0]["message"]["tool_calls"][0]["id"],
            "call_1"
        );
    }

    #[test]
    fn effective_budget_prefers_max_tokens() {
        assert_eq!(
            effective_max_tokens(&json!({"max_tokens": 5, "max_completion_tokens": 9})),
            Some(5)
        );
        assert_eq!(
            effective_max_tokens(&json!({"max_completion_tokens": 9})),
            Some(9)
        );
        assert_eq!(effective_max_tokens(&json!({})), None);
    }

    // -----------------------------------------------------------------------
    // DeepSeek Chat (M4a)
    // -----------------------------------------------------------------------

    fn ds_body() -> Value {
        json!({
            "model": "deepseek-flash",
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": "calling", "reasoning": "why",
                 "reasoning_details": [{"type": "reasoning.encrypted", "data": "xyz"}],
                 "tool_calls": [{"id": "call_1", "type": "function",
                                  "function": {"name": "f", "arguments": "{}"}}]},
                {"role": "tool", "tool_call_id": "call_1", "content": "ok"},
                {"role": "assistant", "content": null}
            ],
            "tools": [{"type": "function", "function": {"name": "f", "parameters": {}}}],
            "thinking": {"type": "enabled"},
            "reasoning_effort": "max",
            "custom_unknown_field": {"keep": true}
        })
    }

    #[test]
    fn ds_fills_reasoning_content_from_reasoning_and_placeholder() {
        let mut body = ds_body();
        let applied = apply_deepseek_request_quirks(&mut body, "tester");
        // First assistant message: `reasoning` text becomes reasoning_content.
        assert_eq!(body["messages"][1]["reasoning_content"], "why");
        // Second assistant message has neither -> a single space keeps DS happy.
        assert_eq!(body["messages"][3]["reasoning_content"], " ");
        assert!(applied.contains(&"filled_reasoning_content".to_string()));
        assert!(applied.contains(&"filled_reasoning_placeholder".to_string()));
        // The plain text survives; only the OD-only blob is dropped.
        assert_eq!(body["messages"][1]["reasoning"], "why");
    }

    #[test]
    fn ds_drops_reasoning_details_and_keeps_unknown_fields() {
        let mut body = ds_body();
        apply_deepseek_request_quirks(&mut body, "tester");
        assert!(body["messages"][1].get("reasoning_details").is_none());
        assert_eq!(body["custom_unknown_field"]["keep"], true);
        // thinking / reasoning_effort are passed through, never defaulted.
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["reasoning_effort"], "max");
    }

    #[test]
    fn ds_does_not_backfill_reasoning_without_tools() {
        let mut body = ds_body();
        body.as_object_mut().unwrap().remove("tools");
        apply_deepseek_request_quirks(&mut body, "tester");
        assert!(body["messages"][1].get("reasoning_content").is_none());
        assert!(body["messages"][3].get("reasoning_content").is_none());
        // reasoning_details is OD-only regardless of tools.
        assert!(body["messages"][1].get("reasoning_details").is_none());
    }

    #[test]
    fn ds_keeps_an_existing_non_empty_reasoning_content() {
        let mut body = json!({
            "messages": [{"role": "assistant", "content": "x", "reasoning_content": "real", "reasoning": "other"}],
            "tools": [{"type": "function", "function": {"name": "f"}}]
        });
        let applied = apply_deepseek_request_quirks(&mut body, "tester");
        assert_eq!(body["messages"][0]["reasoning_content"], "real");
        assert!(!applied.contains(&"filled_reasoning_content".to_string()));
    }

    #[test]
    fn ds_injects_stable_user_id_unless_the_client_set_one() {
        let mut body = json!({"messages": []});
        apply_deepseek_request_quirks(&mut body, "tester");
        let injected = body["user_id"].as_str().unwrap().to_string();
        assert!(injected.starts_with("umsp-"));
        assert_eq!(injected.len(), "umsp-".len() + 16);

        // Stable across calls, different across clients.
        let mut again = json!({"messages": []});
        apply_deepseek_request_quirks(&mut again, "tester");
        assert_eq!(again["user_id"], injected);
        let mut other = json!({"messages": []});
        apply_deepseek_request_quirks(&mut other, "someone-else");
        assert_ne!(other["user_id"], injected);

        // An explicit client value is respected, null counts as absent.
        let mut explicit = json!({"messages": [], "user_id": "client-picked"});
        apply_deepseek_request_quirks(&mut explicit, "tester");
        assert_eq!(explicit["user_id"], "client-picked");
        let mut nulled = json!({"messages": [], "user_id": null});
        apply_deepseek_request_quirks(&mut nulled, "tester");
        assert_eq!(nulled["user_id"], injected);
    }
}
