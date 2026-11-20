//! Cross-protocol translation used only when a provider does not speak the
//! protocol the client called.
//!
//! Same-protocol attempts never enter this module: the gateway still forwards
//! those bodies and streams unchanged. Translation covers text and ordinary
//! tool calls. Thinking signatures, cache breakpoints, `previous_response_id`,
//! and `count_tokens` are not represented.

use serde_json::{Map, Value, json};

use crate::stream;
use crate::usage::Usage;

const DEFAULT_MESSAGES_MAX_TOKENS: u64 = 8192;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Proto {
    Chat,
    Responses,
    Messages,
}

impl Proto {
    fn parse(name: &str) -> Result<Self, String> {
        match name {
            "chat" => Ok(Self::Chat),
            "responses" => Ok(Self::Responses),
            "messages" => Ok(Self::Messages),
            other => Err(format!("unknown protocol '{other}'")),
        }
    }
}

/// Protocol this attempt should speak.
///
/// The client's protocol wins when the provider lists it. Otherwise the first
/// `chat` / `responses` / `messages` entry in the provider's own list is used,
/// which keeps the choice stable and local to that provider.
pub fn select_upstream_protocol<'a>(
    provider_protocols: &'a [String],
    client_protocol: &str,
) -> Option<&'a str> {
    if provider_protocols
        .iter()
        .any(|protocol| protocol == client_protocol)
    {
        return provider_protocols
            .iter()
            .find(|protocol| protocol.as_str() == client_protocol)
            .map(String::as_str);
    }
    provider_protocols
        .iter()
        .find(|protocol| matches!(protocol.as_str(), "chat" | "responses" | "messages"))
        .map(String::as_str)
}

/// A request rewritten into `to`, plus ledger notes such as a defaulted budget.
pub struct RequestTranslation {
    pub body: Value,
    pub notes: Vec<String>,
}

pub fn translate_request(body: &Value, from: &str, to: &str) -> Result<RequestTranslation, String> {
    translate_request_with_budget(body, from, to, None)
}

/// Translate with the logical model's output limit available. When translating
/// to Messages, a request that omitted its budget uses the model limit instead
/// of a small hard-coded default.
pub fn translate_request_with_budget(
    body: &Value,
    from: &str,
    to: &str,
    model_max_output: Option<u64>,
) -> Result<RequestTranslation, String> {
    let from = Proto::parse(from)?;
    let to = Proto::parse(to)?;
    if from == to {
        return Ok(RequestTranslation {
            body: body.clone(),
            notes: Vec::new(),
        });
    }
    let canon = parse_request(body, from)?;
    let mut notes = canon.notes.clone();
    let emitted = emit_request(&canon, to, &mut notes, model_max_output);
    Ok(RequestTranslation {
        body: emitted,
        notes,
    })
}

/// Rewrite a successful upstream JSON body into the client's protocol.
/// `model_id` is the logical model the client asked for.
pub fn translate_response(
    bytes: &[u8],
    from: &str,
    to: &str,
    model_id: &str,
) -> Result<Vec<u8>, String> {
    let from = Proto::parse(from)?;
    let to = Proto::parse(to)?;
    if from == to {
        return Ok(bytes.to_vec());
    }
    let value: Value =
        serde_json::from_slice(bytes).map_err(|_| "upstream response is not JSON".to_string())?;
    let canon = parse_response(&value, from)?;
    let emitted = emit_response(&canon, to, model_id);
    serde_json::to_vec(&emitted)
        .map_err(|err| format!("cannot serialize translated response: {err}"))
}

/// Best-effort message from an OpenAI or Anthropic error envelope.
pub fn error_message(bytes: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(bytes).ok()?;
    value
        .pointer("/error/message")
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|text| !text.is_empty())
}

// ---------------------------------------------------------------------------
// Canonical request / response
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum Part {
    Text(String),
    ImageUrl(String),
}

#[derive(Clone, Debug)]
struct ToolCall {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Clone, Debug)]
enum Turn {
    User {
        parts: Vec<Part>,
    },
    Assistant {
        parts: Vec<Part>,
        calls: Vec<ToolCall>,
    },
    Tool {
        id: String,
        output: String,
    },
}

#[derive(Clone, Debug)]
struct ToolDef {
    name: String,
    description: Option<String>,
    parameters: Value,
}

#[derive(Clone, Debug)]
enum ToolChoice {
    Auto,
    None,
    Required,
    Named(String),
}

#[derive(Clone, Debug)]
struct CanonRequest {
    system: Vec<String>,
    turns: Vec<Turn>,
    tools: Vec<ToolDef>,
    tool_choice: Option<ToolChoice>,
    max_tokens: Option<u64>,
    temperature: Option<Value>,
    top_p: Option<Value>,
    stream: Option<bool>,
    stop: Vec<String>,
    response_format: Option<Value>,
    notes: Vec<String>,
}

#[derive(Clone, Debug)]
struct CanonResponse {
    id: String,
    text: String,
    calls: Vec<ToolCall>,
    /// `stop`, `tool_calls`, or `length`.
    finish: &'static str,
    usage: Option<Usage>,
}

fn parse_request(body: &Value, from: Proto) -> Result<CanonRequest, String> {
    let obj = body
        .as_object()
        .ok_or_else(|| "request body must be a JSON object".to_string())?;
    let mut canon = CanonRequest {
        system: Vec::new(),
        turns: Vec::new(),
        tools: Vec::new(),
        tool_choice: None,
        max_tokens: None,
        temperature: number_field(obj, "temperature"),
        top_p: number_field(obj, "top_p"),
        stream: obj.get("stream").and_then(Value::as_bool),
        stop: Vec::new(),
        response_format: None,
        notes: Vec::new(),
    };
    match from {
        Proto::Chat => parse_chat_request(obj, &mut canon)?,
        Proto::Responses => parse_responses_request(obj, &mut canon)?,
        Proto::Messages => parse_messages_request(obj, &mut canon)?,
    }
    if canon.turns.is_empty() && canon.system.is_empty() {
        return Err("request has no messages".into());
    }
    if let Some(ToolChoice::Named(name)) = &canon.tool_choice {
        if !canon.tools.iter().any(|tool| &tool.name == name) {
            let name = name.clone();
            canon.tool_choice = None;
            canon.notes.push(format!("dropped_tool_choice:{name}"));
        }
    }
    Ok(canon)
}

fn parse_chat_request(obj: &Map<String, Value>, canon: &mut CanonRequest) -> Result<(), String> {
    canon.max_tokens = obj
        .get("max_tokens")
        .and_then(Value::as_u64)
        .or_else(|| obj.get("max_completion_tokens").and_then(Value::as_u64));
    canon.stop = stop_list(obj.get("stop"));
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

fn parse_responses_request(
    obj: &Map<String, Value>,
    canon: &mut CanonRequest,
) -> Result<(), String> {
    canon.max_tokens = obj.get("max_output_tokens").and_then(Value::as_u64);
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

fn parse_messages_request(
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

fn emit_request(
    canon: &CanonRequest,
    to: Proto,
    notes: &mut Vec<String>,
    model_max_output: Option<u64>,
) -> Value {
    let mut obj = Map::new();
    if let Some(stream) = canon.stream {
        obj.insert("stream".into(), Value::Bool(stream));
    }
    for (name, value) in [
        ("temperature", canon.temperature.as_ref()),
        ("top_p", canon.top_p.as_ref()),
    ] {
        let Some(value) = value else {
            continue;
        };
        if to == Proto::Messages {
            if let Some(number) = value.as_f64() {
                if !(0.0..=1.0).contains(&number) {
                    obj.insert(name.into(), json!(number.clamp(0.0, 1.0)));
                    notes.push(format!("clamped_{name}"));
                    continue;
                }
            }
        }
        obj.insert(name.into(), value.clone());
    }
    match to {
        Proto::Chat => emit_chat_request(canon, &mut obj, notes),
        Proto::Responses => emit_responses_request(canon, &mut obj, notes),
        Proto::Messages => emit_messages_request(canon, &mut obj, notes, model_max_output),
    }
    Value::Object(obj)
}

fn emit_chat_request(canon: &CanonRequest, obj: &mut Map<String, Value>, notes: &mut Vec<String>) {
    if canon.stream == Some(true) {
        obj.insert("stream_options".into(), json!({"include_usage": true}));
    }
    if let Some(format) = &canon.response_format {
        obj.insert("response_format".into(), chat_response_format(format));
    }
    if let Some(max) = canon.max_tokens {
        obj.insert("max_tokens".into(), json!(max));
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

fn emit_responses_request(
    canon: &CanonRequest,
    obj: &mut Map<String, Value>,
    notes: &mut Vec<String>,
) {
    if let Some(max) = canon.max_tokens {
        obj.insert("max_output_tokens".into(), json!(max));
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

fn emit_messages_request(
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

fn parse_response(body: &Value, from: Proto) -> Result<CanonResponse, String> {
    let obj = body
        .as_object()
        .ok_or_else(|| "upstream response is not a JSON object".to_string())?;
    let id = obj
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("umsp")
        .to_string();
    let usage = match from {
        Proto::Chat => Usage::from_chat(body),
        Proto::Responses => Usage::from_responses(body),
        Proto::Messages => Usage::from_messages(body),
    };
    let (text, calls, finish) = match from {
        Proto::Chat => parse_chat_response(obj),
        Proto::Responses => parse_responses_response(obj),
        Proto::Messages => parse_messages_response(obj),
    };
    Ok(CanonResponse {
        id,
        text,
        calls,
        finish,
        usage,
    })
}

fn parse_chat_response(obj: &Map<String, Value>) -> (String, Vec<ToolCall>, &'static str) {
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

fn parse_responses_response(obj: &Map<String, Value>) -> (String, Vec<ToolCall>, &'static str) {
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
    (text, calls, finish)
}

fn parse_messages_response(obj: &Map<String, Value>) -> (String, Vec<ToolCall>, &'static str) {
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

fn emit_response(canon: &CanonResponse, to: Proto, model_id: &str) -> Value {
    match to {
        Proto::Chat => json!({
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
        }),
        Proto::Responses => {
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
        Proto::Messages => {
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
    }
}

// ---------------------------------------------------------------------------
// Streaming bridge
// ---------------------------------------------------------------------------

/// Turns an upstream SSE byte stream into the client's protocol.
///
/// The bridge is used only for a mismatched attempt. Passthrough streams keep
/// the existing observers and stay byte-identical.
pub struct StreamBridge {
    from: Proto,
    to: Proto,
    model_id: String,
    emit_event_lines: bool,
    carry: Vec<u8>,
    done: bool,
    saw_output: bool,
    usage: Option<Usage>,
    response_id: String,
    created: i64,
    started: bool,
    seq: u64,
    text_open: bool,
    text_buf: String,
    text_item_id: String,
    tool_open: bool,
    tool_id: String,
    tool_name: String,
    tool_item_id: String,
    args_buf: String,
    tool_seq: u32,
    tools_seen: u32,
    block_index: u32,
    output_index: u32,
    item_seq: u32,
    output_items: Vec<Value>,
    finish: Option<&'static str>,
    terminal_sent: bool,
    /// Chat upstreams interleave parallel tool calls by `index`; buffer them and
    /// flush at the terminal so Messages/Responses blocks stay contiguous.
    pending_tools: std::collections::BTreeMap<u64, PendingTool>,
}

#[derive(Default)]
struct PendingTool {
    id: String,
    name: String,
    arguments: String,
}

impl StreamBridge {
    pub fn new(
        from: &str,
        to: &str,
        model_id: &str,
        emit_event_lines: bool,
    ) -> Result<Self, String> {
        Ok(Self {
            from: Proto::parse(from)?,
            to: Proto::parse(to)?,
            model_id: model_id.to_string(),
            emit_event_lines,
            carry: Vec::new(),
            done: false,
            saw_output: false,
            usage: None,
            response_id: "umsp".into(),
            created: chrono::Utc::now().timestamp(),
            started: false,
            seq: 0,
            text_open: false,
            text_buf: String::new(),
            text_item_id: String::new(),
            tool_open: false,
            tool_id: String::new(),
            tool_name: String::new(),
            tool_item_id: String::new(),
            args_buf: String::new(),
            tool_seq: 0,
            tools_seen: 0,
            block_index: 0,
            output_index: 0,
            item_seq: 0,
            output_items: Vec::new(),
            finish: None,
            terminal_sent: false,
            pending_tools: std::collections::BTreeMap::new(),
        })
    }

    /// Feed one upstream chunk. Returns `true` the first time content reaches
    /// the client, which the pump records as time to first byte.
    pub fn push(&mut self, chunk: &[u8], out: &mut Vec<u8>) -> bool {
        if self.done {
            return false;
        }
        self.carry.extend_from_slice(chunk);
        let mut produced = false;
        while let Some((pos, len)) = split_event(&self.carry) {
            let event: Vec<u8> = self.carry.drain(..pos + len).collect();
            let mut piece = String::new();
            self.handle_event(&event, &mut piece);
            if !piece.is_empty() {
                produced = true;
                out.extend(piece.into_bytes());
            }
        }
        if self.carry.len() > 8 * 1024 * 1024 {
            self.carry.clear();
        }
        produced && self.saw_output
    }

    /// Parse a trailing event that arrived without a blank line, then give
    /// Chat/Responses clients a terminal event when the upstream closed cleanly.
    /// Messages never gets a synthesized `message_stop` (the pump sends an
    /// error event instead).
    pub fn finish(&mut self, out: &mut Vec<u8>) {
        if self.done {
            return;
        }
        if !self.carry.iter().all(|byte| byte.is_ascii_whitespace()) {
            let event = std::mem::take(&mut self.carry);
            let mut piece = String::new();
            self.handle_event(&event, &mut piece);
            out.extend(piece.into_bytes());
        }
        if !self.done && self.to != Proto::Messages {
            let mut piece = String::new();
            self.emit_terminal(&mut piece);
            out.extend(piece.into_bytes());
        }
    }

    pub fn done(&self) -> bool {
        self.done
    }

    pub fn usage(&self) -> Option<Usage> {
        self.usage
    }

    pub fn keepalive(&self) -> &'static [u8] {
        if self.to == Proto::Messages {
            stream::MESSAGES_PING_EVENT
        } else {
            stream::KEEPALIVE_EVENT
        }
    }

    pub fn error_event(&mut self, message: &str) -> Vec<u8> {
        self.terminal_sent = true;
        self.done = true;
        match self.to {
            Proto::Messages => stream::messages_error_event(message),
            Proto::Responses => self
                .responses_event(
                    "error",
                    &json!({
                        "type": "error",
                        "message": message,
                        "code": "upstream_stream_error",
                    }),
                )
                .into_bytes(),
            Proto::Chat => stream::error_event(message),
        }
    }

    fn handle_event(&mut self, raw: &[u8], out: &mut String) {
        let Some(event) = parse_sse(raw) else {
            return;
        };
        if event.data.trim() == "[DONE]" {
            self.emit_terminal(out);
            return;
        }
        let Ok(value) = serde_json::from_str::<Value>(&event.data) else {
            return;
        };
        self.note_usage(&value);
        self.note_id(&value);
        let kind = event
            .name
            .as_deref()
            .or_else(|| value.get("type").and_then(Value::as_str))
            .unwrap_or("");
        match self.from {
            Proto::Chat => self.on_chat(&value, out),
            Proto::Responses => self.on_responses(kind, &value, out),
            Proto::Messages => self.on_messages(kind, &value, out),
        }
    }

    fn note_usage(&mut self, value: &Value) {
        let usage = match self.from {
            Proto::Chat => Usage::from_chat(value),
            Proto::Responses => Usage::from_responses(value),
            Proto::Messages => Usage::from_messages(value).or_else(|| {
                value
                    .pointer("/message/usage")
                    .and_then(|usage| Usage::from_messages(&json!({"usage": usage})))
            }),
        };
        if let Some(new) = usage {
            self.usage = Some(match self.usage {
                Some(old) => merge_usage(old, new),
                None => new,
            });
        }
        if self.from == Proto::Messages {
            if let Some(output) = value
                .pointer("/usage/output_tokens")
                .and_then(Value::as_u64)
            {
                let mut merged = self.usage.unwrap_or_default();
                merged.output_tokens = output;
                if merged.total_tokens == 0 {
                    merged.total_tokens = merged.input_tokens + output;
                }
                self.usage = Some(merged);
            }
        }
    }

    fn note_id(&mut self, value: &Value) {
        let id = value
            .get("id")
            .or_else(|| value.pointer("/response/id"))
            .or_else(|| value.pointer("/message/id"))
            .and_then(Value::as_str);
        if let Some(id) = id.filter(|id| !id.is_empty()) {
            self.response_id = id.to_string();
        }
    }

    fn on_chat(&mut self, value: &Value, out: &mut String) {
        if let Some(message) = value
            .get("error")
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str)
        {
            let bytes = self.error_event(message);
            out.push_str(&String::from_utf8_lossy(&bytes));
            return;
        }
        let choice = value.pointer("/choices/0").and_then(Value::as_object);
        let Some(choice) = choice else {
            return;
        };
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish = Some(map_finish(reason, self.tools_seen > 0));
        }
        let Some(delta) = choice.get("delta").and_then(Value::as_object) else {
            return;
        };
        if let Some(text) = delta
            .get("content")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
        {
            self.emit_text(text, out);
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                self.on_chat_tool(call);
            }
        }
    }

    fn on_chat_tool(&mut self, call: &Value) {
        let Some(call) = call.as_object() else {
            return;
        };
        let index = call.get("index").and_then(Value::as_u64).unwrap_or(0);
        let entry = self.pending_tools.entry(index).or_default();
        if let Some(id) = call
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        {
            entry.id = id.to_string();
        }
        if let Some(name) = call
            .get("function")
            .and_then(Value::as_object)
            .and_then(|function| function.get("name"))
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
        {
            entry.name = name.to_string();
        }
        if let Some(arguments) = call
            .get("function")
            .and_then(Value::as_object)
            .and_then(|function| function.get("arguments"))
            .and_then(Value::as_str)
            .filter(|arguments| !arguments.is_empty())
        {
            entry.arguments.push_str(arguments);
        }
    }

    fn flush_pending_tools(&mut self, out: &mut String) {
        let pending = std::mem::take(&mut self.pending_tools);
        for (_, tool) in pending {
            if tool.name.is_empty() {
                continue;
            }
            let id = if tool.id.is_empty() {
                format!("call_{}", self.tool_seq)
            } else {
                tool.id
            };
            self.begin_tool(&id, &tool.name, out);
            if !tool.arguments.is_empty() {
                self.emit_args(&tool.arguments, out);
            }
            self.close_tool_block(out);
        }
    }

    fn on_responses(&mut self, kind: &str, value: &Value, out: &mut String) {
        match kind {
            "response.output_text.delta" => {
                if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                    self.emit_text(delta, out);
                }
            }
            "response.output_text.done" => {
                if self.text_buf.is_empty() {
                    if let Some(text) = value.get("text").and_then(Value::as_str) {
                        self.emit_text(text, out);
                    }
                }
            }
            "response.output_item.added" => {
                if let Some(item) = value.get("item").and_then(Value::as_object) {
                    if item.get("type").and_then(Value::as_str) == Some("function_call") {
                        self.begin_tool(
                            item.get("call_id")
                                .or_else(|| item.get("id"))
                                .and_then(Value::as_str)
                                .unwrap_or("call_unknown"),
                            item.get("name")
                                .and_then(Value::as_str)
                                .unwrap_or("function"),
                            out,
                        );
                    }
                }
            }
            "response.function_call_arguments.delta" => {
                if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                    self.emit_args(delta, out);
                }
            }
            "response.output_item.done" => self.on_responses_item_done(value, out),
            "response.completed" => self.emit_terminal(out),
            "response.incomplete" => {
                self.finish = Some("length");
                self.emit_terminal(out);
            }
            "response.failed" | "response.error" | "error" => {
                let message = value
                    .pointer("/response/error/message")
                    .or_else(|| value.pointer("/error/message"))
                    .or_else(|| value.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("upstream response failed");
                let bytes = self.error_event(message);
                out.push_str(&String::from_utf8_lossy(&bytes));
            }
            _ => {}
        }
    }

    fn on_responses_item_done(&mut self, value: &Value, out: &mut String) {
        let Some(item) = value.get("item").and_then(Value::as_object) else {
            return;
        };
        match item.get("type").and_then(Value::as_str) {
            Some("message") if self.text_buf.is_empty() => {
                let text = text_of(item.get("content"));
                if !text.is_empty() {
                    self.emit_text(&text, out);
                }
            }
            Some("function_call") => {
                let arguments = json_string(item.get("arguments"));
                let call_id = item
                    .get("call_id")
                    .or_else(|| item.get("id"))
                    .and_then(Value::as_str);
                if self.tool_open && call_id == Some(self.tool_id.as_str()) {
                    if let Some(rest) = arguments.strip_prefix(&self.args_buf) {
                        if !rest.is_empty() {
                            self.emit_args(rest, out);
                        }
                    }
                } else if !arguments.is_empty() || item.get("name").is_some() {
                    self.begin_tool(
                        call_id.unwrap_or("call_unknown"),
                        item.get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("function"),
                        out,
                    );
                    if !arguments.is_empty() {
                        self.emit_args(&arguments, out);
                    }
                }
            }
            _ => {}
        }
    }

    fn on_messages(&mut self, kind: &str, value: &Value, out: &mut String) {
        match kind {
            "content_block_start" => {
                let Some(block) = value.get("content_block").and_then(Value::as_object) else {
                    return;
                };
                if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                    self.begin_tool(
                        block
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or("toolu_unknown"),
                        block
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("function"),
                        out,
                    );
                }
            }
            "content_block_delta" => {
                let delta = value.get("delta").and_then(Value::as_object);
                let Some(delta) = delta else {
                    return;
                };
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        if let Some(text) = delta.get("text").and_then(Value::as_str) {
                            self.emit_text(text, out);
                        }
                    }
                    Some("input_json_delta") => {
                        if let Some(partial) = delta.get("partial_json").and_then(Value::as_str) {
                            self.emit_args(partial, out);
                        }
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(reason) = value.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.finish = Some(map_finish(reason, self.tools_seen > 0));
                }
            }
            "message_stop" => self.emit_terminal(out),
            "error" => {
                let message = value
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("upstream stream error");
                let bytes = self.error_event(message);
                out.push_str(&String::from_utf8_lossy(&bytes));
            }
            _ => {}
        }
    }

    fn emit_text(&mut self, text: &str, out: &mut String) {
        if text.is_empty() || self.terminal_sent {
            return;
        }
        self.ensure_started(out);
        self.close_tool_block(out);
        match self.to {
            Proto::Chat => {
                let delta = if !self.saw_output {
                    json!({"role": "assistant", "content": text})
                } else {
                    json!({"content": text})
                };
                out.push_str(&chat_chunk(
                    &self.response_id,
                    self.created,
                    &self.model_id,
                    delta,
                    None,
                    None,
                ));
            }
            Proto::Messages => {
                if !self.text_open {
                    out.push_str(&messages_event(
                        "content_block_start",
                        &json!({
                            "type": "content_block_start",
                            "index": self.block_index,
                            "content_block": {"type": "text", "text": ""},
                        }),
                    ));
                    self.text_open = true;
                }
                out.push_str(&messages_event(
                    "content_block_delta",
                    &json!({
                        "type": "content_block_delta",
                        "index": self.block_index,
                        "delta": {"type": "text_delta", "text": text},
                    }),
                ));
            }
            Proto::Responses => {
                if !self.text_open {
                    self.text_open = true;
                    self.text_buf.clear();
                    self.item_seq += 1;
                    self.text_item_id = format!("msg_{}_{}", self.response_id, self.item_seq);
                    let item_id = self.text_item_id.clone();
                    out.push_str(&self.responses_event(
                        "response.output_item.added",
                        &json!({
                            "type": "response.output_item.added",
                            "output_index": self.output_index,
                            "item": {
                                "type": "message",
                                "id": item_id,
                                "role": "assistant",
                                "status": "in_progress",
                                "content": [],
                            },
                        }),
                    ));
                    out.push_str(&self.responses_event(
                        "response.content_part.added",
                        &json!({
                            "type": "response.content_part.added",
                            "item_id": self.text_item_id,
                            "output_index": self.output_index,
                            "content_index": 0,
                            "part": {"type": "output_text", "text": "", "annotations": []},
                        }),
                    ));
                }
                out.push_str(&self.responses_event(
                    "response.output_text.delta",
                    &json!({
                        "type": "response.output_text.delta",
                        "item_id": self.text_item_id,
                        "output_index": self.output_index,
                        "content_index": 0,
                        "delta": text,
                        "logprobs": [],
                    }),
                ));
                self.text_buf.push_str(text);
            }
        }
        self.saw_output = true;
        self.text_open = true;
    }

    fn begin_tool(&mut self, id: &str, name: &str, out: &mut String) {
        if self.terminal_sent {
            return;
        }
        self.ensure_started(out);
        self.close_text_block(out);
        self.close_tool_block(out);
        self.tool_id = id.to_string();
        self.tool_name = name.to_string();
        self.args_buf.clear();
        self.tool_open = true;
        self.tools_seen += 1;
        let index = self.tool_seq;
        self.tool_seq += 1;
        match self.to {
            Proto::Chat => {
                let delta = if !self.saw_output {
                    json!({
                        "role": "assistant",
                        "tool_calls": [{
                            "index": index,
                            "id": self.tool_id,
                            "type": "function",
                            "function": {"name": self.tool_name, "arguments": ""},
                        }],
                    })
                } else {
                    json!({
                        "tool_calls": [{
                            "index": index,
                            "id": self.tool_id,
                            "type": "function",
                            "function": {"name": self.tool_name, "arguments": ""},
                        }],
                    })
                };
                out.push_str(&chat_chunk(
                    &self.response_id,
                    self.created,
                    &self.model_id,
                    delta,
                    None,
                    None,
                ));
            }
            Proto::Messages => {
                out.push_str(&messages_event(
                    "content_block_start",
                    &json!({
                        "type": "content_block_start",
                        "index": self.block_index,
                        "content_block": {
                            "type": "tool_use",
                            "id": self.tool_id,
                            "name": self.tool_name,
                            "input": {},
                        },
                    }),
                ));
            }
            Proto::Responses => {
                self.item_seq += 1;
                self.tool_item_id = format!("fc_{}_{}", self.response_id, self.item_seq);
                out.push_str(&self.responses_event(
                    "response.output_item.added",
                    &json!({
                        "type": "response.output_item.added",
                        "output_index": self.output_index,
                        "item": {
                            "type": "function_call",
                            "id": self.tool_item_id,
                            "call_id": self.tool_id,
                            "name": self.tool_name,
                            "arguments": "",
                            "status": "in_progress",
                        },
                    }),
                ));
            }
        }
        self.saw_output = true;
    }

    fn emit_args(&mut self, arguments: &str, out: &mut String) {
        if arguments.is_empty() || !self.tool_open || self.terminal_sent {
            return;
        }
        self.args_buf.push_str(arguments);
        match self.to {
            Proto::Chat => {
                let index = self.tool_seq.saturating_sub(1);
                let delta = json!({
                    "tool_calls": [{
                        "index": index,
                        "function": {"arguments": arguments},
                    }],
                });
                out.push_str(&chat_chunk(
                    &self.response_id,
                    self.created,
                    &self.model_id,
                    delta,
                    None,
                    None,
                ));
            }
            Proto::Messages => {
                out.push_str(&messages_event(
                    "content_block_delta",
                    &json!({
                        "type": "content_block_delta",
                        "index": self.block_index,
                        "delta": {"type": "input_json_delta", "partial_json": arguments},
                    }),
                ));
            }
            Proto::Responses => {
                out.push_str(&self.responses_event(
                    "response.function_call_arguments.delta",
                    &json!({
                        "type": "response.function_call_arguments.delta",
                        "item_id": self.tool_item_id,
                        "output_index": self.output_index,
                        "delta": arguments,
                    }),
                ));
            }
        }
        self.saw_output = true;
    }

    fn ensure_started(&mut self, out: &mut String) {
        if self.started {
            return;
        }
        self.started = true;
        if self.to == Proto::Messages {
            let usage = messages_usage(self.usage);
            out.push_str(&messages_event(
                "message_start",
                &json!({
                    "type": "message_start",
                    "message": {
                        "id": format!("msg_{}", self.response_id),
                        "type": "message",
                        "role": "assistant",
                        "model": self.model_id,
                        "content": [],
                        "stop_reason": null,
                        "stop_sequence": null,
                        "usage": usage,
                    },
                }),
            ));
        } else if self.to == Proto::Responses {
            let response = self.responses_envelope("in_progress", Vec::new(), None);
            out.push_str(&self.responses_event(
                "response.created",
                &json!({"type": "response.created", "response": response}),
            ));
        }
    }

    fn close_text_block(&mut self, out: &mut String) {
        if !self.text_open {
            return;
        }
        self.text_open = false;
        match self.to {
            Proto::Messages => {
                out.push_str(&messages_event(
                    "content_block_stop",
                    &json!({"type": "content_block_stop", "index": self.block_index}),
                ));
                self.block_index += 1;
            }
            Proto::Responses => {
                let item_id = self.text_item_id.clone();
                let text = std::mem::take(&mut self.text_buf);
                out.push_str(&self.responses_event(
                    "response.output_text.done",
                    &json!({
                        "type": "response.output_text.done",
                        "item_id": item_id,
                        "output_index": self.output_index,
                        "content_index": 0,
                        "text": text,
                        "logprobs": [],
                    }),
                ));
                let item = json!({
                    "type": "message",
                    "id": item_id,
                    "role": "assistant",
                    "status": "completed",
                    "content": [{"type": "output_text", "text": text, "annotations": []}],
                });
                out.push_str(&self.responses_event(
                    "response.output_item.done",
                    &json!({
                        "type": "response.output_item.done",
                        "output_index": self.output_index,
                        "item": item,
                    }),
                ));
                self.output_items.push(item);
                self.output_index += 1;
            }
            Proto::Chat => {}
        }
    }

    fn close_tool_block(&mut self, out: &mut String) {
        if !self.tool_open {
            return;
        }
        self.tool_open = false;
        match self.to {
            Proto::Messages => {
                out.push_str(&messages_event(
                    "content_block_stop",
                    &json!({"type": "content_block_stop", "index": self.block_index}),
                ));
                self.block_index += 1;
            }
            Proto::Responses => {
                let arguments = std::mem::take(&mut self.args_buf);
                out.push_str(&self.responses_event(
                    "response.function_call_arguments.done",
                    &json!({
                        "type": "response.function_call_arguments.done",
                        "item_id": self.tool_item_id,
                        "output_index": self.output_index,
                        "name": self.tool_name,
                        "arguments": arguments,
                    }),
                ));
                let item = json!({
                    "type": "function_call",
                    "id": self.tool_item_id,
                    "call_id": self.tool_id,
                    "name": self.tool_name,
                    "arguments": arguments,
                    "status": "completed",
                });
                out.push_str(&self.responses_event(
                    "response.output_item.done",
                    &json!({
                        "type": "response.output_item.done",
                        "output_index": self.output_index,
                        "item": item,
                    }),
                ));
                self.output_items.push(item);
                self.output_index += 1;
            }
            Proto::Chat => {}
        }
    }

    fn emit_terminal(&mut self, out: &mut String) {
        if self.terminal_sent {
            return;
        }
        if self.from == Proto::Chat {
            self.flush_pending_tools(out);
        }
        self.ensure_started(out);
        self.close_text_block(out);
        self.close_tool_block(out);
        let has_tools = self.tools_seen > 0;
        let finish = self
            .finish
            .unwrap_or(if has_tools { "tool_calls" } else { "stop" });
        match self.to {
            Proto::Chat => {
                out.push_str(&chat_chunk(
                    &self.response_id,
                    self.created,
                    &self.model_id,
                    json!({}),
                    Some(chat_finish(finish)),
                    self.usage,
                ));
                out.push_str("data: [DONE]\n\n");
            }
            Proto::Messages => {
                let stop = messages_stop(finish);
                let usage = messages_usage(self.usage);
                out.push_str(&messages_event(
                    "message_delta",
                    &json!({
                        "type": "message_delta",
                        "delta": {"stop_reason": stop, "stop_sequence": null},
                        "usage": {"output_tokens": usage["output_tokens"]},
                    }),
                ));
                out.push_str(&messages_event(
                    "message_stop",
                    &json!({"type": "message_stop"}),
                ));
            }
            Proto::Responses => {
                let status = if finish == "length" || finish == "content_filter" {
                    "incomplete"
                } else {
                    "completed"
                };
                let mut response =
                    self.responses_envelope(status, self.output_items.clone(), self.usage);
                if status == "incomplete" {
                    let reason = if finish == "length" {
                        "max_output_tokens"
                    } else {
                        "content_filter"
                    };
                    response["incomplete_details"] = json!({"reason": reason});
                }
                out.push_str(&self.responses_event(
                    "response.completed",
                    &json!({"type": "response.completed", "response": response}),
                ));
            }
        }
        self.terminal_sent = true;
        self.done = true;
        self.saw_output = true;
    }

    fn responses_envelope(&self, status: &str, output: Vec<Value>, usage: Option<Usage>) -> Value {
        json!({
            "id": format!("resp_{}", self.response_id),
            "object": "response",
            "created_at": self.created,
            "status": status,
            "model": self.model_id,
            "output": output,
            "parallel_tool_calls": false,
            "tool_choice": "auto",
            "tools": [],
            "usage": responses_usage(usage),
        })
    }

    fn responses_event(&mut self, event: &str, payload: &Value) -> String {
        let mut payload = payload.clone();
        if let Some(obj) = payload.as_object_mut() {
            obj.insert("sequence_number".into(), json!(self.seq));
        }
        self.seq += 1;
        sse_event(event, &payload, self.emit_event_lines)
    }
}

fn merge_usage(old: Usage, new: Usage) -> Usage {
    let mut merged = Usage {
        input_tokens: if new.input_tokens > 0 {
            new.input_tokens
        } else {
            old.input_tokens
        },
        output_tokens: if new.output_tokens > 0 {
            new.output_tokens
        } else {
            old.output_tokens
        },
        cached_tokens: new.cached_tokens.max(old.cached_tokens),
        reasoning_tokens: new.reasoning_tokens.max(old.reasoning_tokens),
        cache_creation_tokens: new.cache_creation_tokens.max(old.cache_creation_tokens),
        total_tokens: new.total_tokens.max(old.total_tokens),
    };
    if merged.total_tokens == 0 {
        merged.total_tokens = merged.input_tokens + merged.output_tokens;
    }
    merged
}

// ---------------------------------------------------------------------------
// Small JSON helpers
// ---------------------------------------------------------------------------

fn number_field(obj: &Map<String, Value>, key: &str) -> Option<Value> {
    obj.get(key).filter(|value| value.is_number()).cloned()
}

fn push_text(out: &mut Vec<String>, text: &str) {
    if !text.is_empty() {
        out.push(text.to_string());
    }
}

fn tool_result_text(value: Option<&Value>) -> String {
    let Some(Value::Array(blocks)) = value else {
        return text_of(value);
    };
    let mut text = String::new();
    let mut images = 0;
    for block in blocks {
        let Some(block) = block.as_object() else {
            continue;
        };
        match block.get("type").and_then(Value::as_str) {
            Some("text") => text.push_str(block.get("text").and_then(Value::as_str).unwrap_or("")),
            Some("image") => images += 1,
            _ => {}
        }
    }
    if images > 0 {
        text.push_str(&format!(" [{images} image(s) omitted]"));
    }
    text
}

fn text_of(value: Option<&Value>) -> String {
    let Some(value) = value else {
        return String::new();
    };
    match value {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| match part {
                Value::String(text) => Some(text.clone()),
                Value::Object(obj) => match obj.get("type").and_then(Value::as_str) {
                    Some("text" | "output_text" | "input_text") => {
                        obj.get("text").and_then(Value::as_str).map(str::to_string)
                    }
                    _ => None,
                },
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

fn parts_of(value: Option<&Value>, from: Proto) -> Vec<Part> {
    let Some(value) = value else {
        return Vec::new();
    };
    match value {
        Value::String(text) => vec![Part::Text(text.clone())],
        Value::Array(items) => items
            .iter()
            .filter_map(|item| part_of(item, from))
            .collect(),
        _ => Vec::new(),
    }
}

fn part_of(item: &Value, from: Proto) -> Option<Part> {
    if let Some(text) = item.as_str() {
        return Some(Part::Text(text.to_string()));
    }
    let obj = item.as_object()?;
    match obj.get("type").and_then(Value::as_str) {
        Some("text" | "output_text" | "input_text") => obj
            .get("text")
            .and_then(Value::as_str)
            .map(|text| Part::Text(text.to_string())),
        Some("image_url") => image_url_value(obj.get("image_url")).map(Part::ImageUrl),
        Some("input_image") => obj
            .get("image_url")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| image_url_value(obj.get("image_url")))
            .map(Part::ImageUrl),
        Some("image") if from == Proto::Messages => messages_image(obj).map(Part::ImageUrl),
        _ => None,
    }
}

fn image_url_value(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(url) => Some(url.clone()),
        Value::Object(obj) => obj.get("url").and_then(Value::as_str).map(str::to_string),
        _ => None,
    }
}

fn messages_image(obj: &Map<String, Value>) -> Option<String> {
    let source = obj.get("source")?.as_object()?;
    match source.get("type").and_then(Value::as_str) {
        Some("url") => source
            .get("url")
            .and_then(Value::as_str)
            .map(str::to_string),
        Some("base64") => {
            let media = source
                .get("media_type")
                .and_then(Value::as_str)
                .unwrap_or("application/octet-stream");
            let data = source.get("data").and_then(Value::as_str)?;
            Some(format!("data:{media};base64,{data}"))
        }
        _ => None,
    }
}

fn stop_list(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::String(text)) if !text.is_empty() => vec![text.clone()],
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

fn json_string(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(other) => serde_json::to_string(other).unwrap_or_else(|_| "{}".into()),
        None => String::new(),
    }
}

fn parse_args(arguments: &str) -> Value {
    if arguments.is_empty() {
        return json!({});
    }
    match serde_json::from_str::<Value>(arguments) {
        Ok(value @ Value::Object(_)) => value,
        _ => json!({}),
    }
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

fn named_choice(choice: &str) -> Option<ToolChoice> {
    match choice {
        "auto" => Some(ToolChoice::Auto),
        "none" => Some(ToolChoice::None),
        "required" => Some(ToolChoice::Required),
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

fn normalize_schema(value: Option<&Value>) -> Value {
    match value {
        Some(Value::Object(map)) => Value::Object(map.clone()),
        _ => json!({"type": "object"}),
    }
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

fn chat_tool_choice(choice: &ToolChoice) -> Value {
    match choice {
        ToolChoice::Auto => json!("auto"),
        ToolChoice::None => json!("none"),
        ToolChoice::Required => json!("required"),
        ToolChoice::Named(name) => json!({"type": "function", "function": {"name": name}}),
    }
}

fn responses_tool_choice(choice: &ToolChoice) -> Value {
    match choice {
        ToolChoice::Auto => json!("auto"),
        ToolChoice::None => json!("none"),
        ToolChoice::Required => json!("required"),
        ToolChoice::Named(name) => json!({"type": "function", "name": name}),
    }
}

fn messages_tool_choice(choice: &ToolChoice) -> Value {
    match choice {
        ToolChoice::Auto => json!({"type": "auto"}),
        ToolChoice::None => json!({"type": "none"}),
        ToolChoice::Required => json!({"type": "any"}),
        ToolChoice::Named(name) => json!({"type": "tool", "name": name}),
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

fn map_finish(raw: &str, has_tools: bool) -> &'static str {
    match raw {
        "length" | "max_tokens" => "length",
        "tool_calls" | "tool_use" => "tool_calls",
        "content_filter" | "refusal" => "content_filter",
        _ if has_tools => "tool_calls",
        _ => "stop",
    }
}

fn chat_finish(finish: &str) -> &'static str {
    match finish {
        "length" => "length",
        "tool_calls" => "tool_calls",
        "content_filter" => "content_filter",
        _ => "stop",
    }
}

fn messages_stop(finish: &str) -> &'static str {
    match finish {
        "length" => "max_tokens",
        "tool_calls" => "tool_use",
        "content_filter" => "refusal",
        _ => "end_turn",
    }
}

fn chat_usage(usage: Option<Usage>) -> Value {
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

fn responses_usage(usage: Option<Usage>) -> Value {
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

fn messages_usage(usage: Option<Usage>) -> Value {
    let Some(usage) = usage else {
        return json!({"input_tokens": 0, "output_tokens": 0});
    };
    json!({
        "input_tokens": usage.input_tokens,
        "output_tokens": usage.output_tokens,
    })
}

fn split_event(bytes: &[u8]) -> Option<(usize, usize)> {
    let lf = bytes
        .windows(2)
        .position(|pair| pair == b"\n\n")
        .map(|pos| (pos, 2));
    let crlf = bytes
        .windows(4)
        .position(|pair| pair == b"\r\n\r\n")
        .map(|pos| (pos, 4));
    match (lf, crlf) {
        (Some(lf), Some(crlf)) if crlf.0 < lf.0 => Some(crlf),
        (Some(lf), _) => Some(lf),
        (None, Some(crlf)) => Some(crlf),
        (None, None) => None,
    }
}

struct SseFields {
    name: Option<String>,
    data: String,
}

fn parse_sse(raw: &[u8]) -> Option<SseFields> {
    let text = String::from_utf8_lossy(raw);
    let mut name = None;
    let mut data = Vec::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix("data:") {
            data.push(rest.trim_start().to_string());
        } else if let Some(rest) = line.strip_prefix("event:") {
            name = Some(rest.trim().to_string());
        }
    }
    if data.is_empty() {
        return None;
    }
    Some(SseFields {
        name,
        data: data.join("\n"),
    })
}

fn sse_event(event: &str, payload: &Value, with_event_line: bool) -> String {
    let data = serde_json::to_string(payload).unwrap_or_else(|_| "{}".into());
    if with_event_line {
        format!("event: {event}\ndata: {data}\n\n")
    } else {
        format!("data: {data}\n\n")
    }
}

fn messages_event(event: &str, payload: &Value) -> String {
    sse_event(event, payload, true)
}

fn chat_chunk(
    id: &str,
    created: i64,
    model: &str,
    delta: Value,
    finish: Option<&str>,
    usage: Option<Usage>,
) -> String {
    let mut payload = json!({
        "id": format!("chatcmpl-{id}"),
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": delta,
            "finish_reason": finish,
        }],
    });
    if let Some(usage) = usage {
        payload["usage"] = chat_usage(Some(usage));
    }
    format!(
        "data: {}\n\n",
        serde_json::to_string(&payload).unwrap_or_else(|_| "{}".into())
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn protocols(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_string()).collect()
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
        let events: Vec<Value> = text
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
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
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
        );
        let mut bridge = StreamBridge::new("messages", "chat", "logical", false).unwrap();
        let mut out = Vec::new();
        bridge.push(raw.as_bytes(), &mut out);
        let usage = bridge.usage().unwrap();
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.output_tokens, 5);
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
}
