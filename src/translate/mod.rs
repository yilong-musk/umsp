//! Cross-protocol translation used only when a provider does not speak the
//! protocol the client called.
//!
//! Same-protocol attempts never enter this module: the gateway still forwards
//! those bodies and streams unchanged. Translation covers text and ordinary
//! tool calls. Thinking signatures, cache breakpoints, `previous_response_id`,
//! and `count_tokens` are not represented.

mod canon;
mod chat;
mod messages;
mod responses;
mod stream_bridge;
#[cfg(test)]
mod tests;

use serde_json::{Map, Value, json};

use crate::usage::Usage;

use canon::{CanonRequest, CanonResponse, ToolChoice, number_field};

pub use stream_bridge::StreamBridge;

pub(super) const DEFAULT_MESSAGES_MAX_TOKENS: u64 = 8192;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Proto {
    Chat,
    Responses,
    Messages,
}

impl Proto {
    pub(super) fn parse(name: &str) -> Result<Self, String> {
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
        n: None,
        parallel_tool_calls: None,
        response_format: None,
        notes: Vec::new(),
    };
    match from {
        Proto::Chat => chat::parse_request(obj, &mut canon)?,
        Proto::Responses => responses::parse_request(obj, &mut canon)?,
        Proto::Messages => messages::parse_request(obj, &mut canon)?,
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
        Proto::Chat => chat::emit_request(canon, &mut obj, notes),
        Proto::Responses => responses::emit_request(canon, &mut obj, notes),
        Proto::Messages => messages::emit_request(canon, &mut obj, notes, model_max_output),
    }
    Value::Object(obj)
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
        Proto::Chat => chat::parse_response(obj),
        Proto::Responses => responses::parse_response(obj)?,
        Proto::Messages => messages::parse_response(obj),
    };
    Ok(CanonResponse {
        id,
        text,
        calls,
        finish,
        usage,
    })
}

fn emit_response(canon: &CanonResponse, to: Proto, model_id: &str) -> Value {
    match to {
        Proto::Chat => chat::emit_response(canon, model_id),
        Proto::Responses => responses::emit_response(canon, model_id),
        Proto::Messages => messages::emit_response(canon, model_id),
    }
}
