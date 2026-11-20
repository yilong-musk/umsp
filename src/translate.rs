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

const DEFAULT_MESSAGES_MAX_TOKENS: u64 = 4096;

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
    let emitted = emit_request(&canon, to, &mut notes);
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
    Ok(canon)
}

fn parse_chat_request(obj: &Map<String, Value>, canon: &mut CanonRequest) -> Result<(), String> {
    canon.max_tokens = obj
        .get("max_tokens")
        .and_then(Value::as_u64)
        .or_else(|| obj.get("max_completion_tokens").and_then(Value::as_u64));
    canon.stop = stop_list(obj.get("stop"));
    canon.tools = parse_chat_tools(obj.get("tools"));
    canon.tool_choice = parse_chat_tool_choice(obj.get("tool_choice"));
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
    canon.tools = parse_responses_tools(obj.get("tools"));
    canon.tool_choice = parse_responses_tool_choice(obj.get("tool_choice"));
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
    canon.tools = parse_messages_tools(obj.get("tools"));
    canon.tool_choice = parse_messages_tool_choice(obj.get("tool_choice"));
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

fn emit_request(canon: &CanonRequest, to: Proto, notes: &mut Vec<String>) -> Value {
    let mut obj = Map::new();
    if let Some(stream) = canon.stream {
        obj.insert("stream".into(), Value::Bool(stream));
    }
    if let Some(temperature) = &canon.temperature {
        obj.insert("temperature".into(), temperature.clone());
    }
    if let Some(top_p) = &canon.top_p {
        obj.insert("top_p".into(), top_p.clone());
    }
    match to {
        Proto::Chat => emit_chat_request(canon, &mut obj),
        Proto::Responses => emit_responses_request(canon, &mut obj, notes),
        Proto::Messages => emit_messages_request(canon, &mut obj, notes),
    }
    Value::Object(obj)
}

fn emit_chat_request(canon: &CanonRequest, obj: &mut Map<String, Value>) {
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
                let mut message = Map::new();
                message.insert("role".into(), json!("assistant"));
                message.insert("content".into(), chat_content(parts));
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
                if !parts.is_empty() {
                    input.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": responses_content(parts, false),
                    }));
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
) {
    let max = canon.max_tokens.unwrap_or_else(|| {
        notes.push("defaulted_max_tokens".into());
        DEFAULT_MESSAGES_MAX_TOKENS
    });
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
    let finish = if status == "incomplete" {
        "length"
    } else {
        map_finish("stop", !calls.is_empty())
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
                    "content": [{"type": "output_text", "text": canon.text}],
                }));
            }
            for call in &canon.calls {
                output.push(json!({
                    "type": "function_call",
                    "call_id": call.id,
                    "name": call.name,
                    "arguments": call.arguments,
                }));
            }
            json!({
                "id": if canon.id.starts_with("resp_") { canon.id.clone() } else { format!("resp_{}", canon.id) },
                "object": "response",
                "status": if canon.finish == "length" { "incomplete" } else { "completed" },
                "model": model_id,
                "output": output,
                "usage": responses_usage(canon.usage),
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
    text_open: bool,
    saw_text_delta: bool,
    tool_seq: u32,
    tools_seen: u32,
    tool_open: bool,
    tool_id: String,
    tool_name: String,
    args_emitted: usize,
    block_index: u32,
    output_index: u32,
    finish: Option<&'static str>,
    terminal_sent: bool,
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
            text_open: false,
            saw_text_delta: false,
            tool_seq: 0,
            tools_seen: 0,
            tool_open: false,
            tool_id: String::new(),
            tool_name: String::new(),
            args_emitted: 0,
            block_index: 0,
            output_index: 0,
            finish: None,
            terminal_sent: false,
        })
    }

    /// Feed one upstream chunk. Returns `true` the first time client bytes are
    /// produced, which the pump records as time to first byte.
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

    /// Parse a trailing event that arrived without a blank line.
    pub fn finish(&mut self, out: &mut Vec<u8>) {
        if self.done || self.carry.iter().all(|byte| byte.is_ascii_whitespace()) {
            return;
        }
        let event = std::mem::take(&mut self.carry);
        let mut piece = String::new();
        self.handle_event(&event, &mut piece);
        out.extend(piece.into_bytes());
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

    pub fn error_event(&self, message: &str) -> Vec<u8> {
        match self.to {
            Proto::Messages => stream::messages_error_event(message),
            Proto::Responses => stream::responses_error_event(message),
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
        if usage.is_some() {
            self.usage = usage;
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
                self.on_chat_tool(call, out);
            }
        }
    }

    fn on_chat_tool(&mut self, call: &Value, out: &mut String) {
        let Some(call) = call.as_object() else {
            return;
        };
        let function = call.get("function").and_then(Value::as_object);
        let name = function
            .and_then(|function| function.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let id = call.get("id").and_then(Value::as_str).unwrap_or("");
        if !id.is_empty() || !name.is_empty() {
            let id = if id.is_empty() {
                format!("call_{}", self.tool_seq)
            } else {
                id.to_string()
            };
            let name = if name.is_empty() {
                "function".to_string()
            } else {
                name.to_string()
            };
            self.begin_tool(&id, &name, out);
        }
        if let Some(arguments) = function
            .and_then(|function| function.get("arguments"))
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
        {
            self.emit_args(arguments, out);
        }
    }

    fn on_responses(&mut self, kind: &str, value: &Value, out: &mut String) {
        match kind {
            "response.output_text.delta" => {
                if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                    self.saw_text_delta = true;
                    self.emit_text(delta, out);
                }
            }
            "response.output_text.done" => {
                if !self.saw_text_delta {
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
            "response.completed" | "response.incomplete" | "response.failed" => {
                if kind == "response.incomplete" {
                    self.finish = Some("length");
                }
                self.emit_terminal(out);
            }
            _ => {}
        }
    }

    fn on_responses_item_done(&mut self, value: &Value, out: &mut String) {
        let Some(item) = value.get("item").and_then(Value::as_object) else {
            return;
        };
        match item.get("type").and_then(Value::as_str) {
            Some("message") if !self.saw_text_delta => {
                let text = text_of(item.get("content"));
                if !text.is_empty() {
                    self.emit_text(&text, out);
                }
            }
            Some("function_call") => {
                let arguments = json_string(item.get("arguments"));
                if self.tool_open {
                    if arguments.len() > self.args_emitted {
                        let rest = &arguments[self.args_emitted..];
                        self.emit_args(rest, out);
                    }
                } else if !arguments.is_empty() || item.get("name").is_some() {
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
                            self.saw_text_delta = true;
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
                    out.push_str(&self.responses_event(
                        "response.output_item.added",
                        &json!({
                            "type": "response.output_item.added",
                            "output_index": self.output_index,
                            "item": {
                                "type": "message",
                                "id": format!("msg_{}", self.response_id),
                                "role": "assistant",
                                "content": [],
                            },
                        }),
                    ));
                    self.text_open = true;
                }
                out.push_str(&self.responses_event(
                    "response.output_text.delta",
                    &json!({
                        "type": "response.output_text.delta",
                        "output_index": self.output_index,
                        "delta": text,
                    }),
                ));
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
        self.args_emitted = 0;
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
                out.push_str(&self.responses_event(
                    "response.output_item.added",
                    &json!({
                        "type": "response.output_item.added",
                        "output_index": self.output_index,
                        "item": {
                            "type": "function_call",
                            "call_id": self.tool_id,
                            "name": self.tool_name,
                            "arguments": "",
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
        self.args_emitted += arguments.len();
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
                        "usage": {"input_tokens": 0, "output_tokens": 0},
                    },
                }),
            ));
        } else if self.to == Proto::Responses {
            out.push_str(&self.responses_event(
                "response.created",
                &json!({
                    "type": "response.created",
                    "response": {
                        "id": format!("resp_{}", self.response_id),
                        "object": "response",
                        "status": "in_progress",
                        "model": self.model_id,
                        "output": [],
                    },
                }),
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
                out.push_str(&self.responses_event(
                    "response.output_item.done",
                    &json!({
                        "type": "response.output_item.done",
                        "output_index": self.output_index,
                        "item": {"type": "message", "role": "assistant", "content": []},
                    }),
                ));
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
                out.push_str(&self.responses_event(
                    "response.output_item.done",
                    &json!({
                        "type": "response.output_item.done",
                        "output_index": self.output_index,
                        "item": {
                            "type": "function_call",
                            "call_id": self.tool_id,
                            "name": self.tool_name,
                        },
                    }),
                ));
                self.output_index += 1;
            }
            Proto::Chat => {}
        }
    }

    fn emit_terminal(&mut self, out: &mut String) {
        if self.terminal_sent {
            return;
        }
        self.ensure_started(out);
        self.close_text_block(out);
        self.close_tool_block(out);
        let finish = self.finish.unwrap_or(if self.tools_seen > 0 {
            "tool_calls"
        } else {
            "stop"
        });
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
                out.push_str(&self.responses_event(
                    "response.completed",
                    &json!({
                        "type": "response.completed",
                        "response": {
                            "id": format!("resp_{}", self.response_id),
                            "object": "response",
                            "status": if finish == "length" { "incomplete" } else { "completed" },
                            "model": self.model_id,
                            "usage": responses_usage(self.usage),
                        },
                    }),
                ));
                out.push_str("data: [DONE]\n\n");
            }
        }
        self.terminal_sent = true;
        self.done = true;
        self.saw_output = true;
    }

    fn responses_event(&self, event: &str, payload: &Value) -> String {
        sse_event(event, payload, self.emit_event_lines)
    }
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
    serde_json::from_str(arguments).unwrap_or_else(|_| json!({"raw": arguments}))
}

fn parse_chat_tools(value: Option<&Value>) -> Vec<ToolDef> {
    let Some(tools) = value.and_then(Value::as_array) else {
        return Vec::new();
    };
    tools
        .iter()
        .filter_map(|tool| {
            let tool = tool.as_object()?;
            let function = tool
                .get("function")
                .and_then(Value::as_object)
                .unwrap_or(tool);
            let name = function.get("name").and_then(Value::as_str)?;
            Some(ToolDef {
                name: name.to_string(),
                description: function
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                parameters: function
                    .get("parameters")
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object"})),
            })
        })
        .collect()
}

fn parse_responses_tools(value: Option<&Value>) -> Vec<ToolDef> {
    let Some(tools) = value.and_then(Value::as_array) else {
        return Vec::new();
    };
    tools
        .iter()
        .filter_map(|tool| {
            let tool = tool.as_object()?;
            if tool.get("type").and_then(Value::as_str) == Some("function")
                || tool.get("name").is_some()
            {
                let name = tool.get("name").and_then(Value::as_str)?;
                return Some(ToolDef {
                    name: name.to_string(),
                    description: tool
                        .get("description")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    parameters: tool
                        .get("parameters")
                        .cloned()
                        .unwrap_or_else(|| json!({"type": "object"})),
                });
            }
            None
        })
        .collect()
}

fn parse_messages_tools(value: Option<&Value>) -> Vec<ToolDef> {
    let Some(tools) = value.and_then(Value::as_array) else {
        return Vec::new();
    };
    tools
        .iter()
        .filter_map(|tool| {
            let tool = tool.as_object()?;
            let name = tool.get("name").and_then(Value::as_str)?;
            Some(ToolDef {
                name: name.to_string(),
                description: tool
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                parameters: tool
                    .get("input_schema")
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object"})),
            })
        })
        .collect()
}

fn parse_chat_tool_choice(value: Option<&Value>) -> Option<ToolChoice> {
    match value? {
        Value::String(choice) => named_choice(choice),
        Value::Object(obj) => obj
            .get("function")
            .and_then(|function| function.get("name"))
            .or_else(|| obj.get("name"))
            .and_then(Value::as_str)
            .map(|name| ToolChoice::Named(name.to_string()))
            .or(Some(ToolChoice::Auto)),
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
                arguments: function
                    .get("arguments")
                    .and_then(Value::as_str)
                    .unwrap_or("{}")
                    .to_string(),
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
                arguments: json_string(block.get("input")),
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
            turns.push(Turn::Tool {
                id: obj
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .unwrap_or("toolu_unknown")
                    .to_string(),
                output: text_of(obj.get("content")),
            });
        } else if let Some(part) = part_of(block, Proto::Messages) {
            parts.push(part);
        }
    }
    if !parts.is_empty() {
        turns.push(Turn::User { parts });
    }
}

fn chat_tool(tool: &ToolDef) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": tool.name,
            "description": tool.description,
            "parameters": tool.parameters,
        },
    })
}

fn responses_tool(tool: &ToolDef) -> Value {
    json!({
        "type": "function",
        "name": tool.name,
        "description": tool.description,
        "parameters": tool.parameters,
    })
}

fn messages_tool(tool: &ToolDef) -> Value {
    json!({
        "name": tool.name,
        "description": tool.description,
        "input_schema": tool.parameters,
    })
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
    Value::Array(
        parts
            .iter()
            .map(|part| match part {
                Part::Text(text) if input => json!({"type": "input_text", "text": text}),
                Part::Text(text) => json!({"type": "output_text", "text": text}),
                Part::ImageUrl(url) => json!({"type": "input_image", "image_url": url}),
            })
            .collect(),
    )
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
        _ if has_tools => "tool_calls",
        _ => "stop",
    }
}

fn chat_finish(finish: &str) -> &'static str {
    match finish {
        "length" => "length",
        "tool_calls" => "tool_calls",
        _ => "stop",
    }
}

fn messages_stop(finish: &str) -> &'static str {
    match finish {
        "length" => "max_tokens",
        "tool_calls" => "tool_use",
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
}
