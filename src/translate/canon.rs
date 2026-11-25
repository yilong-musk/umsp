//! Canonical intermediate representation and shared JSON helpers for
//! cross-protocol request/response translation.

use serde_json::{Map, Value, json};

use crate::usage::Usage;

use super::Proto;

#[derive(Clone, Debug)]
pub(super) enum Part {
    Text(String),
    ImageUrl(String),
}

#[derive(Clone, Debug)]
pub(super) struct ToolCall {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) arguments: String,
}

#[derive(Clone, Debug)]
pub(super) enum Turn {
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
pub(super) struct ToolDef {
    pub(super) name: String,
    pub(super) description: Option<String>,
    pub(super) parameters: Value,
}

#[derive(Clone, Debug)]
pub(super) enum ToolChoice {
    Auto,
    None,
    Required,
    Named(String),
}

#[derive(Clone, Debug)]
pub(super) struct CanonRequest {
    pub(super) system: Vec<String>,
    pub(super) turns: Vec<Turn>,
    pub(super) tools: Vec<ToolDef>,
    pub(super) tool_choice: Option<ToolChoice>,
    pub(super) max_tokens: Option<u64>,
    pub(super) temperature: Option<Value>,
    pub(super) top_p: Option<Value>,
    pub(super) stream: Option<bool>,
    pub(super) stop: Vec<String>,
    pub(super) n: Option<u64>,
    pub(super) parallel_tool_calls: Option<bool>,
    pub(super) response_format: Option<Value>,
    pub(super) notes: Vec<String>,
}

#[derive(Clone, Debug)]
pub(super) struct CanonResponse {
    pub(super) id: String,
    pub(super) text: String,
    pub(super) calls: Vec<ToolCall>,
    /// `stop`, `tool_calls`, `length`, or `content_filter`.
    pub(super) finish: &'static str,
    pub(super) usage: Option<Usage>,
}

pub(super) fn number_field(obj: &Map<String, Value>, key: &str) -> Option<Value> {
    obj.get(key).filter(|value| value.is_number()).cloned()
}

pub(super) fn push_text(out: &mut Vec<String>, text: &str) {
    if !text.is_empty() {
        out.push(text.to_string());
    }
}

pub(super) fn tool_result_text(value: Option<&Value>) -> String {
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

pub(super) fn text_of(value: Option<&Value>) -> String {
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

pub(super) fn parts_of(value: Option<&Value>, from: Proto) -> Vec<Part> {
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

pub(super) fn part_of(item: &Value, from: Proto) -> Option<Part> {
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

pub(super) fn stop_list(value: Option<&Value>) -> Vec<String> {
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

pub(super) fn json_string(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(other) => serde_json::to_string(other).unwrap_or_else(|_| "{}".into()),
        None => String::new(),
    }
}

pub(super) fn parse_args(arguments: &str) -> Value {
    if arguments.is_empty() {
        return json!({});
    }
    match serde_json::from_str::<Value>(arguments) {
        Ok(value @ Value::Object(_)) => value,
        _ => json!({}),
    }
}

pub(super) fn normalize_schema(value: Option<&Value>) -> Value {
    match value {
        Some(Value::Object(map)) => Value::Object(map.clone()),
        _ => json!({"type": "object"}),
    }
}

pub(super) fn named_choice(choice: &str) -> Option<ToolChoice> {
    match choice {
        "auto" => Some(ToolChoice::Auto),
        "none" => Some(ToolChoice::None),
        "required" => Some(ToolChoice::Required),
        _ => None,
    }
}

pub(super) fn map_finish(raw: &str, has_tools: bool) -> &'static str {
    match raw {
        "length" | "max_tokens" => "length",
        "tool_calls" | "tool_use" => "tool_calls",
        "content_filter" | "refusal" => "content_filter",
        _ if has_tools => "tool_calls",
        _ => "stop",
    }
}
