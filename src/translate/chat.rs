//! OpenAI Chat Completions request/response codec for cross-protocol
//! translation.

use serde_json::{Map, Value, json};

use crate::usage::Usage;

use super::Proto;
use super::canon::{
    CanonRequest, CanonResponse, Part, ToolCall, ToolChoice, ToolDef, Turn, json_string,
    map_finish, named_choice, normalize_schema, parts_of, push_text, stop_list, text_of,
};

pub(super) fn parse_request(
    obj: &Map<String, Value>,
    canon: &mut CanonRequest,
) -> Result<(), String> {
    canon.max_tokens = obj
        .get("max_tokens")
        .and_then(Value::as_u64)
        .or_else(|| obj.get("max_completion_tokens").and_then(Value::as_u64));
    canon.stop = stop_list(obj.get("stop"));
    canon.n = obj.get("n").and_then(Value::as_u64);
    canon.parallel_tool_calls = obj.get("parallel_tool_calls").and_then(Value::as_bool);
    let (tools, dropped) = parse_chat_tools(obj.get("tools"));
    canon.tools = tools;
    canon.notes.extend(dropped);
    canon.response_format = parse_chat_response_format(obj.get("response_format"));
    canon.tool_choice = parse_chat_tool_choice(obj.get("tool_choice"));
    if canon.tool_choice.is_none() && obj.get("tool_choice").is_some_and(Value::is_object) {
        canon.notes.push("dropped_tool_choice".into());
    }
    let messages = obj
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| "chat request requires messages".to_string())?;
    for message in messages {
        let Some(message) = message.as_object() else {
            continue;
        };
        let role = message.get("role").and_then(Value::as_str).unwrap_or("");
        match role {
            "system" | "developer" => {
                push_text(&mut canon.system, &text_of(message.get("content")))
            }
            "user" => canon.turns.push(Turn::User {
                parts: parts_of(message.get("content"), Proto::Chat),
            }),
            "assistant" => canon.turns.push(Turn::Assistant {
                parts: parts_of(message.get("content"), Proto::Chat),
                calls: parse_chat_calls(message.get("tool_calls")),
            }),
            "tool" => {
                let id = message
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .unwrap_or("call_unknown")
                    .to_string();
                canon.turns.push(Turn::Tool {
                    id,
                    output: text_of(message.get("content")),
                });
            }
            _ => {}
        }
    }
    Ok(())
}

pub(super) fn emit_request(
    canon: &CanonRequest,
    obj: &mut Map<String, Value>,
    notes: &mut Vec<String>,
) {
    if canon.stream == Some(true) {
        obj.insert("stream_options".into(), json!({"include_usage": true}));
    }
    if let Some(format) = &canon.response_format {
        obj.insert("response_format".into(), chat_response_format(format));
    }
    if let Some(max) = canon.max_tokens {
        obj.insert("max_tokens".into(), json!(max));
    }
    if let Some(n) = canon.n {
        obj.insert("n".into(), json!(n));
    }
    if let Some(parallel) = canon.parallel_tool_calls {
        obj.insert("parallel_tool_calls".into(), json!(parallel));
    }
    if !canon.stop.is_empty() {
        obj.insert("stop".into(), json!(canon.stop));
    }
    if !canon.tools.is_empty() {
        obj.insert(
            "tools".into(),
            Value::Array(canon.tools.iter().map(chat_tool).collect()),
        );
    }
    if let Some(choice) = &canon.tool_choice {
        obj.insert("tool_choice".into(), chat_tool_choice(choice));
    }
    let mut messages = Vec::new();
    if !canon.system.is_empty() {
        messages.push(json!({"role": "system", "content": canon.system.join("\n\n")}));
    }
    for turn in &canon.turns {
        match turn {
            Turn::User { parts } => messages.push(json!({
                "role": "user",
                "content": chat_content(parts),
            })),
            Turn::Assistant { parts, calls } => {
                if parts.iter().any(|part| matches!(part, Part::ImageUrl(_))) {
                    notes.push("dropped_assistant_image".into());
                }
                let mut message = Map::new();
                message.insert("role".into(), json!("assistant"));
                message.insert("content".into(), chat_assistant_content(parts));
                if !calls.is_empty() {
                    message.insert(
                        "tool_calls".into(),
                        Value::Array(calls.iter().map(chat_call).collect()),
                    );
                }
                messages.push(Value::Object(message));
            }
            Turn::Tool { id, output } => messages.push(json!({
                "role": "tool",
                "tool_call_id": id,
                "content": output,
            })),
        }
    }
    obj.insert("messages".into(), Value::Array(messages));
}

pub(super) fn parse_response(obj: &Map<String, Value>) -> (String, Vec<ToolCall>, &'static str) {
    let choice = obj
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(Value::as_object);
    let Some(choice) = choice else {
        return (String::new(), Vec::new(), "stop");
    };
    let message = choice.get("message").and_then(Value::as_object);
    let text = message
        .map(|message| text_of(message.get("content")))
        .unwrap_or_default();
    let calls = message
        .map(|message| parse_chat_calls(message.get("tool_calls")))
        .unwrap_or_default();
    let finish = map_finish(
        choice
            .get("finish_reason")
            .and_then(Value::as_str)
            .unwrap_or("stop"),
        !calls.is_empty(),
    );
    (text, calls, finish)
}

pub(super) fn emit_response(canon: &CanonResponse, model_id: &str) -> Value {
    json!({
        "id": format!("chatcmpl-{}", canon.id),
        "object": "chat.completion",
        "created": chrono::Utc::now().timestamp(),
        "model": model_id,
        "choices": [{
            "index": 0,
            "message": chat_assistant(&canon.text, &canon.calls),
            "finish_reason": chat_finish(canon.finish),
        }],
        "usage": chat_usage(canon.usage),
    })
}

pub(super) fn chat_finish(finish: &str) -> &'static str {
    match finish {
        "length" => "length",
        "tool_calls" => "tool_calls",
        "content_filter" => "content_filter",
        _ => "stop",
    }
}

pub(super) fn chat_usage(usage: Option<Usage>) -> Value {
    let Some(usage) = usage else {
        return Value::Null;
    };
    json!({
        "prompt_tokens": usage.input_tokens,
        "completion_tokens": usage.output_tokens,
        "total_tokens": usage.total_tokens,
        "prompt_tokens_details": {"cached_tokens": usage.cached_tokens},
        "completion_tokens_details": {"reasoning_tokens": usage.reasoning_tokens},
    })
}

fn parse_chat_tools(value: Option<&Value>) -> (Vec<ToolDef>, Vec<String>) {
    let Some(tools) = value.and_then(Value::as_array) else {
        return (Vec::new(), Vec::new());
    };
    let mut parsed = Vec::new();
    let mut dropped = Vec::new();
    for tool in tools {
        let Some(tool) = tool.as_object() else {
            continue;
        };
        let kind = tool.get("type").and_then(Value::as_str);
        if !matches!(kind, None | Some("function")) {
            let name = tool
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("unnamed");
            dropped.push(format!("dropped_server_tool:{name}"));
            continue;
        }
        let function = tool
            .get("function")
            .and_then(Value::as_object)
            .unwrap_or(tool);
        let Some(name) = function.get("name").and_then(Value::as_str) else {
            continue;
        };
        parsed.push(ToolDef {
            name: name.to_string(),
            description: function
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_string),
            parameters: normalize_schema(function.get("parameters")),
        });
    }
    (parsed, dropped)
}

fn parse_chat_tool_choice(value: Option<&Value>) -> Option<ToolChoice> {
    match value? {
        Value::String(choice) => named_choice(choice),
        Value::Object(obj) => obj
            .get("function")
            .and_then(|function| function.get("name"))
            .or_else(|| obj.get("name"))
            .and_then(Value::as_str)
            .map(|name| ToolChoice::Named(name.to_string())),
        _ => None,
    }
}

fn parse_chat_calls(value: Option<&Value>) -> Vec<ToolCall> {
    let Some(calls) = value.and_then(Value::as_array) else {
        return Vec::new();
    };
    calls
        .iter()
        .filter_map(|call| {
            let call = call.as_object()?;
            let function = call.get("function").and_then(Value::as_object)?;
            Some(ToolCall {
                id: call
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("call_unknown")
                    .to_string(),
                name: function
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("function")
                    .to_string(),
                arguments: {
                    let arguments = json_string(function.get("arguments"));
                    if arguments.is_empty() {
                        "{}".to_string()
                    } else {
                        arguments
                    }
                },
            })
        })
        .collect()
}

fn parse_chat_response_format(value: Option<&Value>) -> Option<Value> {
    let obj = value?.as_object()?;
    match obj.get("type").and_then(Value::as_str)? {
        "json_object" => Some(json!({"type": "json_object"})),
        "json_schema" => {
            let spec = obj.get("json_schema")?.as_object()?;
            Some(json!({
                "type": "json_schema",
                "name": spec.get("name").and_then(Value::as_str).unwrap_or("response"),
                "schema": spec.get("schema").cloned().unwrap_or_else(|| json!({})),
                "strict": spec.get("strict").and_then(Value::as_bool),
            }))
        }
        _ => None,
    }
}

fn chat_response_format(format: &Value) -> Value {
    match format.get("type").and_then(Value::as_str) {
        Some("json_schema") => {
            let mut spec = Map::new();
            spec.insert(
                "name".into(),
                json!(
                    format
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("response")
                ),
            );
            spec.insert(
                "schema".into(),
                format.get("schema").cloned().unwrap_or_else(|| json!({})),
            );
            if let Some(strict) = format.get("strict").and_then(Value::as_bool) {
                spec.insert("strict".into(), json!(strict));
            }
            json!({"type": "json_schema", "json_schema": spec})
        }
        _ => json!({"type": "json_object"}),
    }
}

fn chat_assistant_content(parts: &[Part]) -> Value {
    let texts: Vec<&str> = parts
        .iter()
        .filter_map(|part| match part {
            Part::Text(text) => Some(text.as_str()),
            Part::ImageUrl(_) => None,
        })
        .collect();
    match texts.as_slice() {
        [] => json!(""),
        [text] => json!(text),
        _ => Value::Array(
            texts
                .iter()
                .map(|text| json!({"type": "text", "text": text}))
                .collect(),
        ),
    }
}

fn chat_tool(tool: &ToolDef) -> Value {
    let mut function = Map::new();
    function.insert("name".into(), json!(tool.name));
    if let Some(description) = &tool.description {
        function.insert("description".into(), json!(description));
    }
    function.insert(
        "parameters".into(),
        normalize_schema(Some(&tool.parameters)),
    );
    json!({"type": "function", "function": function})
}

fn chat_tool_choice(choice: &ToolChoice) -> Value {
    match choice {
        ToolChoice::Auto => json!("auto"),
        ToolChoice::None => json!("none"),
        ToolChoice::Required => json!("required"),
        ToolChoice::Named(name) => json!({"type": "function", "function": {"name": name}}),
    }
}

fn chat_content(parts: &[Part]) -> Value {
    if parts.len() == 1 {
        if let Part::Text(text) = &parts[0] {
            return json!(text);
        }
    }
    if parts.is_empty() {
        return json!("");
    }
    Value::Array(parts.iter().map(chat_part).collect())
}

fn chat_part(part: &Part) -> Value {
    match part {
        Part::Text(text) => json!({"type": "text", "text": text}),
        Part::ImageUrl(url) => json!({"type": "image_url", "image_url": {"url": url}}),
    }
}

fn chat_call(call: &ToolCall) -> Value {
    json!({
        "id": call.id,
        "type": "function",
        "function": {"name": call.name, "arguments": call.arguments},
    })
}

fn chat_assistant(text: &str, calls: &[ToolCall]) -> Value {
    let mut message = Map::new();
    message.insert("role".into(), json!("assistant"));
    message.insert(
        "content".into(),
        if text.is_empty() && !calls.is_empty() {
            Value::Null
        } else {
            json!(text)
        },
    );
    if !calls.is_empty() {
        message.insert(
            "tool_calls".into(),
            Value::Array(calls.iter().map(chat_call).collect()),
        );
    }
    Value::Object(message)
}
