//! Anthropic Messages request/response codec for cross-protocol translation.

use serde_json::{Map, Value, json};

use crate::usage::Usage;

use super::canon::{
    CanonRequest, CanonResponse, Part, ToolCall, ToolChoice, ToolDef, Turn, json_string,
    map_finish, normalize_schema, parse_args, part_of, parts_of, push_text, stop_list, text_of,
    tool_result_text,
};
use super::{DEFAULT_MESSAGES_MAX_TOKENS, Proto};

pub(super) fn parse_request(
    obj: &Map<String, Value>,
    canon: &mut CanonRequest,
) -> Result<(), String> {
    canon.max_tokens = obj.get("max_tokens").and_then(Value::as_u64);
    canon.stop = stop_list(obj.get("stop_sequences"));
    let (tools, dropped) = parse_messages_tools(obj.get("tools"));
    canon.tools = tools;
    canon.notes.extend(dropped);
    canon.tool_choice = parse_messages_tool_choice(obj.get("tool_choice"));
    if canon.tool_choice.is_none() && obj.get("tool_choice").is_some_and(Value::is_object) {
        canon.notes.push("dropped_tool_choice".into());
    }
    if obj.get("system").is_some() {
        push_text(&mut canon.system, &text_of(obj.get("system")));
    }
    let messages = obj
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| "messages request requires messages".to_string())?;
    for message in messages {
        let Some(message) = message.as_object() else {
            continue;
        };
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");
        if role == "assistant" {
            let (parts, calls) = split_assistant_blocks(message.get("content"));
            canon.turns.push(Turn::Assistant { parts, calls });
            continue;
        }
        push_user_blocks(message.get("content"), &mut canon.turns);
    }
    Ok(())
}

pub(super) fn emit_request(
    canon: &CanonRequest,
    obj: &mut Map<String, Value>,
    notes: &mut Vec<String>,
    model_max_output: Option<u64>,
) {
    let max = canon.max_tokens.unwrap_or_else(|| {
        notes.push("defaulted_max_tokens".into());
        model_max_output
            .filter(|max| *max > 0)
            .unwrap_or(DEFAULT_MESSAGES_MAX_TOKENS)
    });
    if canon.response_format.is_some() {
        notes.push("dropped_response_format".into());
    }
    if canon.n.is_some() {
        notes.push("dropped_n".into());
    }
    if canon.parallel_tool_calls.is_some() {
        notes.push("dropped_parallel_tool_calls".into());
    }
    obj.insert("max_tokens".into(), json!(max));
    if !canon.stop.is_empty() {
        obj.insert("stop_sequences".into(), json!(canon.stop));
    }
    if !canon.system.is_empty() {
        obj.insert("system".into(), json!(canon.system.join("\n\n")));
    }
    if !canon.tools.is_empty() {
        obj.insert(
            "tools".into(),
            Value::Array(canon.tools.iter().map(messages_tool).collect()),
        );
    }
    if let Some(choice) = &canon.tool_choice {
        obj.insert("tool_choice".into(), messages_tool_choice(choice));
    }
    let mut messages: Vec<Value> = Vec::new();
    for turn in &canon.turns {
        match turn {
            Turn::User { parts } => {
                push_messages_turn(&mut messages, "user", messages_user_blocks(parts))
            }
            Turn::Assistant { parts, calls } => {
                if parts.iter().any(|part| matches!(part, Part::ImageUrl(_))) {
                    notes.push("dropped_assistant_image".into());
                }
                let mut blocks = messages_assistant_text(parts);
                for call in calls {
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": call.id,
                        "name": call.name,
                        "input": parse_args(&call.arguments),
                    }));
                }
                push_messages_turn(&mut messages, "assistant", blocks);
            }
            Turn::Tool { id, output } => push_messages_turn(
                &mut messages,
                "user",
                vec![json!({
                    "type": "tool_result",
                    "tool_use_id": id,
                    "content": output,
                })],
            ),
        }
    }
    obj.insert("messages".into(), Value::Array(messages));
}

pub(super) fn parse_response(obj: &Map<String, Value>) -> (String, Vec<ToolCall>, &'static str) {
    let (parts, calls) = split_assistant_blocks(obj.get("content"));
    let text = parts
        .into_iter()
        .filter_map(|part| match part {
            Part::Text(text) => Some(text),
            Part::ImageUrl(_) => None,
        })
        .collect::<Vec<_>>()
        .join("");
    let finish = map_finish(
        obj.get("stop_reason")
            .and_then(Value::as_str)
            .unwrap_or("end_turn"),
        !calls.is_empty(),
    );
    (text, calls, finish)
}

pub(super) fn emit_response(canon: &CanonResponse, model_id: &str) -> Value {
    let mut content = Vec::new();
    if !canon.text.is_empty() || canon.calls.is_empty() {
        content.push(json!({"type": "text", "text": canon.text}));
    }
    for call in &canon.calls {
        content.push(json!({
            "type": "tool_use",
            "id": call.id,
            "name": call.name,
            "input": parse_args(&call.arguments),
        }));
    }
    json!({
        "id": format!("msg_{}", canon.id),
        "type": "message",
        "role": "assistant",
        "model": model_id,
        "content": content,
        "stop_reason": messages_stop(canon.finish),
        "stop_sequence": null,
        "usage": messages_usage(canon.usage),
    })
}

pub(super) fn messages_stop(finish: &str) -> &'static str {
    match finish {
        "length" => "max_tokens",
        "tool_calls" => "tool_use",
        "content_filter" => "refusal",
        _ => "end_turn",
    }
}

pub(super) fn messages_usage(usage: Option<Usage>) -> Value {
    let Some(usage) = usage else {
        return json!({"input_tokens": 0, "output_tokens": 0});
    };
    json!({
        "input_tokens": usage.input_tokens,
        "output_tokens": usage.output_tokens,
    })
}

fn parse_messages_tools(value: Option<&Value>) -> (Vec<ToolDef>, Vec<String>) {
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
        if !matches!(kind, None | Some("custom")) {
            let name = tool
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("unnamed");
            dropped.push(format!("dropped_server_tool:{name}"));
            continue;
        }
        let Some(name) = tool.get("name").and_then(Value::as_str) else {
            continue;
        };
        parsed.push(ToolDef {
            name: name.to_string(),
            description: tool
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_string),
            parameters: normalize_schema(tool.get("input_schema")),
        });
    }
    (parsed, dropped)
}

fn parse_messages_tool_choice(value: Option<&Value>) -> Option<ToolChoice> {
    let obj = value?.as_object()?;
    match obj.get("type").and_then(Value::as_str)? {
        "auto" => Some(ToolChoice::Auto),
        "none" => Some(ToolChoice::None),
        "any" => Some(ToolChoice::Required),
        "tool" => obj
            .get("name")
            .and_then(Value::as_str)
            .map(|name| ToolChoice::Named(name.to_string())),
        _ => None,
    }
}

fn split_assistant_blocks(content: Option<&Value>) -> (Vec<Part>, Vec<ToolCall>) {
    let Some(Value::Array(blocks)) = content else {
        return (parts_of(content, Proto::Messages), Vec::new());
    };
    let mut parts = Vec::new();
    let mut calls = Vec::new();
    for block in blocks {
        let Some(block) = block.as_object() else {
            continue;
        };
        match block.get("type").and_then(Value::as_str) {
            Some("tool_use") => calls.push(ToolCall {
                id: block
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("toolu_unknown")
                    .to_string(),
                name: block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("function")
                    .to_string(),
                arguments: {
                    let arguments = json_string(block.get("input"));
                    if arguments.is_empty() {
                        "{}".to_string()
                    } else {
                        arguments
                    }
                },
            }),
            Some("thinking" | "redacted_thinking") => {}
            _ => {
                if let Some(part) = part_of(&Value::Object(block.clone()), Proto::Messages) {
                    parts.push(part);
                }
            }
        }
    }
    (parts, calls)
}

fn push_user_blocks(content: Option<&Value>, turns: &mut Vec<Turn>) {
    let Some(Value::Array(blocks)) = content else {
        turns.push(Turn::User {
            parts: parts_of(content, Proto::Messages),
        });
        return;
    };
    let mut parts = Vec::new();
    for block in blocks {
        let Some(obj) = block.as_object() else {
            continue;
        };
        if obj.get("type").and_then(Value::as_str) == Some("tool_result") {
            if !parts.is_empty() {
                turns.push(Turn::User {
                    parts: std::mem::take(&mut parts),
                });
            }
            let mut output = tool_result_text(obj.get("content"));
            if obj.get("is_error").and_then(Value::as_bool) == Some(true) {
                output = if output.is_empty() {
                    "error".to_string()
                } else {
                    format!("error: {output}")
                };
            }
            turns.push(Turn::Tool {
                id: obj
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .unwrap_or("toolu_unknown")
                    .to_string(),
                output,
            });
        } else if let Some(part) = part_of(block, Proto::Messages) {
            parts.push(part);
        }
    }
    if !parts.is_empty() {
        turns.push(Turn::User { parts });
    }
}

fn messages_tool(tool: &ToolDef) -> Value {
    let mut value = json!({
        "name": tool.name,
        "input_schema": normalize_schema(Some(&tool.parameters)),
    });
    if let Some(description) = &tool.description {
        value["description"] = json!(description);
    }
    value
}

fn messages_tool_choice(choice: &ToolChoice) -> Value {
    match choice {
        ToolChoice::Auto => json!({"type": "auto"}),
        ToolChoice::None => json!({"type": "none"}),
        ToolChoice::Required => json!({"type": "any"}),
        ToolChoice::Named(name) => json!({"type": "tool", "name": name}),
    }
}

fn messages_user_blocks(parts: &[Part]) -> Vec<Value> {
    if parts.is_empty() {
        return vec![json!({"type": "text", "text": ""})];
    }
    parts.iter().map(messages_part).collect()
}

fn messages_assistant_text(parts: &[Part]) -> Vec<Value> {
    parts
        .iter()
        .filter_map(|part| match part {
            Part::Text(text) => Some(json!({"type": "text", "text": text})),
            Part::ImageUrl(_) => None,
        })
        .collect()
}

fn messages_part(part: &Part) -> Value {
    match part {
        Part::Text(text) => json!({"type": "text", "text": text}),
        Part::ImageUrl(url) => {
            if let Some(rest) = url.strip_prefix("data:") {
                if let Some((media, data)) = rest.split_once(";base64,") {
                    return json!({
                        "type": "image",
                        "source": {"type": "base64", "media_type": media, "data": data},
                    });
                }
            }
            json!({
                "type": "image",
                "source": {"type": "url", "url": url},
            })
        }
    }
}

fn push_messages_turn(messages: &mut Vec<Value>, role: &str, blocks: Vec<Value>) {
    if blocks.is_empty() {
        return;
    }
    if let Some(Value::Object(last)) = messages.last_mut() {
        if last.get("role").and_then(Value::as_str) == Some(role) {
            if let Some(content) = last.get_mut("content").and_then(Value::as_array_mut) {
                content.extend(blocks);
                return;
            }
        }
    }
    messages.push(json!({"role": role, "content": blocks}));
}
