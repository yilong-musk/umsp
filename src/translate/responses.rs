//! OpenAI Responses request/response codec for cross-protocol translation.

use serde_json::{Map, Value, json};

use crate::usage::Usage;

use super::Proto;
use super::canon::{
    CanonRequest, CanonResponse, Part, ToolCall, ToolChoice, ToolDef, Turn, json_string,
    map_finish, named_choice, normalize_schema, parts_of, push_text, text_of,
};

pub(super) fn parse_request(
    obj: &Map<String, Value>,
    canon: &mut CanonRequest,
) -> Result<(), String> {
    canon.max_tokens = obj.get("max_output_tokens").and_then(Value::as_u64);
    canon.parallel_tool_calls = obj.get("parallel_tool_calls").and_then(Value::as_bool);
    if obj.get("n").is_some() {
        canon.notes.push("dropped_n".into());
    }
    let (tools, dropped) = parse_responses_tools(obj.get("tools"));
    canon.tools = tools;
    canon.notes.extend(dropped);
    canon.response_format = parse_responses_text_format(obj.get("text"));
    canon.tool_choice = parse_responses_tool_choice(obj.get("tool_choice"));
    if canon.tool_choice.is_none() && obj.get("tool_choice").is_some_and(Value::is_object) {
        canon.notes.push("dropped_tool_choice".into());
    }
    if obj.get("instructions").is_some() {
        push_text(&mut canon.system, &text_of(obj.get("instructions")));
    }
    match obj.get("input") {
        None => return Err("responses request requires input".into()),
        Some(Value::String(text)) => canon.turns.push(Turn::User {
            parts: vec![Part::Text(text.clone())],
        }),
        Some(Value::Array(items)) => {
            for item in items {
                parse_responses_item(item, canon);
            }
        }
        Some(_) => return Err("responses input must be a string or an array".into()),
    }
    Ok(())
}

pub(super) fn emit_request(
    canon: &CanonRequest,
    obj: &mut Map<String, Value>,
    notes: &mut Vec<String>,
) {
    if let Some(max) = canon.max_tokens {
        obj.insert("max_output_tokens".into(), json!(max));
    }
    if let Some(parallel) = canon.parallel_tool_calls {
        obj.insert("parallel_tool_calls".into(), json!(parallel));
    }
    if canon.n.is_some() {
        notes.push("dropped_n".into());
    }
    if !canon.stop.is_empty() {
        notes.push("dropped_stop".into());
    }
    if let Some(format) = &canon.response_format {
        obj.insert(
            "text".into(),
            json!({"format": responses_response_format(format)}),
        );
    }
    if !canon.system.is_empty() {
        obj.insert("instructions".into(), json!(canon.system.join("\n\n")));
    }
    if !canon.tools.is_empty() {
        obj.insert(
            "tools".into(),
            Value::Array(canon.tools.iter().map(responses_tool).collect()),
        );
    }
    if let Some(choice) = &canon.tool_choice {
        obj.insert("tool_choice".into(), responses_tool_choice(choice));
    }
    let mut input = Vec::new();
    for turn in &canon.turns {
        match turn {
            Turn::User { parts } => input.push(json!({
                "type": "message",
                "role": "user",
                "content": responses_content(parts, true),
            })),
            Turn::Assistant { parts, calls } => {
                let content = responses_content(parts, false);
                let has_blocks = content.as_array().is_some_and(|blocks| !blocks.is_empty());
                if has_blocks {
                    input.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": content,
                    }));
                } else if parts.iter().any(|part| matches!(part, Part::ImageUrl(_))) {
                    notes.push("dropped_assistant_image".into());
                }
                for call in calls {
                    input.push(json!({
                        "type": "function_call",
                        "call_id": call.id,
                        "name": call.name,
                        "arguments": call.arguments,
                    }));
                }
            }
            Turn::Tool { id, output } => input.push(json!({
                "type": "function_call_output",
                "call_id": id,
                "output": output,
            })),
        }
    }
    obj.insert("input".into(), Value::Array(input));
}

pub(super) fn parse_response(
    obj: &Map<String, Value>,
) -> Result<(String, Vec<ToolCall>, &'static str), String> {
    let mut text = String::new();
    let mut calls = Vec::new();
    if let Some(items) = obj.get("output").and_then(Value::as_array) {
        for item in items {
            let Some(item) = item.as_object() else {
                continue;
            };
            match item.get("type").and_then(Value::as_str) {
                Some("function_call") => calls.push(ToolCall {
                    id: item
                        .get("call_id")
                        .or_else(|| item.get("id"))
                        .and_then(Value::as_str)
                        .unwrap_or("call_unknown")
                        .to_string(),
                    name: item
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("function")
                        .to_string(),
                    arguments: json_string(item.get("arguments")),
                }),
                Some("message") | Some("output_text") | None => {
                    let piece = text_of(item.get("content").or_else(|| item.get("text")));
                    if !piece.is_empty() {
                        text.push_str(&piece);
                    }
                }
                _ => {}
            }
        }
    }
    let status = obj
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("completed");
    if matches!(status, "failed" | "cancelled") {
        return Err("upstream response did not complete".into());
    }
    let finish = match status {
        "incomplete" => match obj
            .get("incomplete_details")
            .and_then(|details| details.get("reason"))
            .and_then(Value::as_str)
        {
            Some("content_filter") => "content_filter",
            _ => "length",
        },
        _ => map_finish("stop", !calls.is_empty()),
    };
    Ok((text, calls, finish))
}

pub(super) fn emit_response(canon: &CanonResponse, model_id: &str) -> Value {
    let mut output = Vec::new();
    if !canon.text.is_empty() || canon.calls.is_empty() {
        output.push(json!({
            "type": "message",
            "id": format!("msg_{}", canon.id),
            "role": "assistant",
            "status": "completed",
            "content": [{"type": "output_text", "text": canon.text, "annotations": []}],
        }));
    }
    for call in &canon.calls {
        output.push(json!({
            "type": "function_call",
            "id": format!("fc_{}", call.id),
            "call_id": call.id,
            "name": call.name,
            "arguments": call.arguments,
            "status": "completed",
        }));
    }
    let status = if canon.finish == "length" || canon.finish == "content_filter" {
        "incomplete"
    } else {
        "completed"
    };
    let incomplete_details = match canon.finish {
        "length" => json!({"reason": "max_output_tokens"}),
        "content_filter" => json!({"reason": "content_filter"}),
        _ => Value::Null,
    };
    json!({
        "id": if canon.id.starts_with("resp_") { canon.id.clone() } else { format!("resp_{}", canon.id) },
        "object": "response",
        "created_at": chrono::Utc::now().timestamp(),
        "status": status,
        "model": model_id,
        "output": output,
        "parallel_tool_calls": false,
        "tool_choice": "auto",
        "tools": [],
        "usage": responses_usage(canon.usage),
        "incomplete_details": incomplete_details,
    })
}

pub(super) fn responses_usage(usage: Option<Usage>) -> Value {
    let Some(usage) = usage else {
        return Value::Null;
    };
    json!({
        "input_tokens": usage.input_tokens,
        "output_tokens": usage.output_tokens,
        "total_tokens": usage.total_tokens,
        "input_tokens_details": {"cached_tokens": usage.cached_tokens},
        "output_tokens_details": {"reasoning_tokens": usage.reasoning_tokens},
    })
}

fn parse_responses_item(item: &Value, canon: &mut CanonRequest) {
    let Some(obj) = item.as_object() else {
        return;
    };
    let kind = obj.get("type").and_then(Value::as_str);
    match kind {
        Some("function_call") => {
            let call = ToolCall {
                id: obj
                    .get("call_id")
                    .or_else(|| obj.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or("call_unknown")
                    .to_string(),
                name: obj
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("function")
                    .to_string(),
                arguments: json_string(obj.get("arguments")),
            };
            if let Some(Turn::Assistant { calls, .. }) = canon.turns.last_mut() {
                calls.push(call);
            } else {
                canon.turns.push(Turn::Assistant {
                    parts: Vec::new(),
                    calls: vec![call],
                });
            }
        }
        Some("function_call_output") => canon.turns.push(Turn::Tool {
            id: obj
                .get("call_id")
                .and_then(Value::as_str)
                .unwrap_or("call_unknown")
                .to_string(),
            output: text_of(obj.get("output")),
        }),
        Some("message") | None => {
            let role = obj.get("role").and_then(Value::as_str).unwrap_or("user");
            match role {
                "system" | "developer" => {
                    push_text(&mut canon.system, &text_of(obj.get("content")));
                }
                "assistant" => canon.turns.push(Turn::Assistant {
                    parts: parts_of(obj.get("content"), Proto::Responses),
                    calls: Vec::new(),
                }),
                _ => canon.turns.push(Turn::User {
                    parts: parts_of(obj.get("content"), Proto::Responses),
                }),
            }
        }
        _ => {}
    }
}

fn parse_responses_tools(value: Option<&Value>) -> (Vec<ToolDef>, Vec<String>) {
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
        let Some(name) = tool.get("name").and_then(Value::as_str) else {
            continue;
        };
        parsed.push(ToolDef {
            name: name.to_string(),
            description: tool
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_string),
            parameters: normalize_schema(tool.get("parameters")),
        });
    }
    (parsed, dropped)
}

fn parse_responses_tool_choice(value: Option<&Value>) -> Option<ToolChoice> {
    match value? {
        Value::String(choice) => named_choice(choice),
        Value::Object(obj) => obj
            .get("name")
            .and_then(Value::as_str)
            .map(|name| ToolChoice::Named(name.to_string())),
        _ => None,
    }
}

fn parse_responses_text_format(value: Option<&Value>) -> Option<Value> {
    let format = value?.get("format")?.as_object()?;
    match format.get("type").and_then(Value::as_str)? {
        "json_object" => Some(json!({"type": "json_object"})),
        "json_schema" => Some(json!({
            "type": "json_schema",
            "name": format.get("name").and_then(Value::as_str).unwrap_or("response"),
            "schema": format.get("schema").cloned().unwrap_or_else(|| json!({})),
            "strict": format.get("strict").and_then(Value::as_bool),
        })),
        _ => None,
    }
}

fn responses_response_format(format: &Value) -> Value {
    match format.get("type").and_then(Value::as_str) {
        Some("json_schema") => {
            let mut spec = Map::new();
            spec.insert("type".into(), json!("json_schema"));
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
            Value::Object(spec)
        }
        _ => json!({"type": "json_object"}),
    }
}

fn responses_tool(tool: &ToolDef) -> Value {
    let mut tool = tool.clone();
    tool.parameters = normalize_schema(Some(&tool.parameters));
    let mut value = json!({
        "type": "function",
        "name": tool.name,
        "parameters": tool.parameters,
    });
    if let Some(description) = &tool.description {
        value["description"] = json!(description);
    }
    value
}

fn responses_tool_choice(choice: &ToolChoice) -> Value {
    match choice {
        ToolChoice::Auto => json!("auto"),
        ToolChoice::None => json!("none"),
        ToolChoice::Required => json!("required"),
        ToolChoice::Named(name) => json!({"type": "function", "name": name}),
    }
}

fn responses_content(parts: &[Part], input: bool) -> Value {
    let mut blocks: Vec<Value> = parts
        .iter()
        .filter_map(|part| match part {
            Part::Text(text) if input => Some(json!({"type": "input_text", "text": text})),
            Part::Text(text) => Some(json!({"type": "output_text", "text": text})),
            Part::ImageUrl(url) if input => Some(json!({"type": "input_image", "image_url": url})),
            Part::ImageUrl(_) => None,
        })
        .collect();
    if blocks.is_empty() && input {
        blocks.push(json!({"type": "input_text", "text": ""}));
    }
    Value::Array(blocks)
}
