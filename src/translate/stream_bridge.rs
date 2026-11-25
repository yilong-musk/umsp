//! Streaming cross-protocol bridge built around an explicit [`StreamEvent`] IR.
//!
//! Pipeline:
//!   raw upstream bytes -> `sse::EventFramer` -> `StreamDecoder` -> `StreamEvent`
//!                      -> `StreamEncoder` (Chat / Responses / Messages) -> bytes
//!
//! Keeping decoding and encoding decoupled avoids N*N branching, makes each
//! side independently testable, and writes hot-path delta frames directly into
//! `&mut Vec<u8>` without intermediate `serde_json::Value` clones.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::{Value, json};

use crate::sse::{EventFramer, MAX_EVENT_BYTES, SseEvent};
use crate::stream;
use crate::usage::{MessagesUsageFields, Usage};

use super::Proto;
use super::canon::{json_string, map_finish, text_of};
use super::chat::{chat_finish, chat_usage};
use super::messages::{messages_stop, messages_usage};
use super::responses::responses_usage;

/// Canonical streaming event passed from [`StreamDecoder`] to [`StreamEncoder`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum StreamEvent {
    ResponseId(String),
    TextDelta(String),
    ToolStart { id: String, name: String },
    ToolArgs(String),
    ToolEnd,
    Usage(Usage),
    Finish(&'static str),
    Terminal,
    Error(String),
}

/// Turns an upstream SSE byte stream into the client's protocol.
///
/// Used only for a mismatched attempt. Passthrough streams keep the native
/// observers in [`crate::stream`] and stay byte-identical.
pub struct StreamBridge {
    framer: EventFramer,
    decoder: StreamDecoder,
    encoder: StreamEncoder,
}

impl StreamBridge {
    pub fn new(
        from: &str,
        to: &str,
        model_id: &str,
        emit_event_lines: bool,
    ) -> Result<Self, String> {
        let from = Proto::parse(from)?;
        let to = Proto::parse(to)?;
        Ok(Self {
            framer: EventFramer::new(MAX_EVENT_BYTES),
            decoder: StreamDecoder::new(from),
            encoder: StreamEncoder::new(to, model_id, emit_event_lines),
        })
    }

    /// Feed one upstream chunk. Returns `true` the first time content reaches
    /// the client, which the pump records as time to first byte.
    pub fn push(&mut self, chunk: &[u8], out: &mut Vec<u8>) -> bool {
        if self.encoder.done() {
            return false;
        }
        let before = out.len();
        let mut events = Vec::new();
        let decoder = &mut self.decoder;
        let encoder = &mut self.encoder;
        let framed = self.framer.push(chunk, |sse| {
            if encoder.done() {
                return;
            }
            events.clear();
            decoder.decode(sse, &mut events);
            for event in events.drain(..) {
                encoder.encode(event, out);
                if encoder.done() {
                    break;
                }
            }
        });
        if framed.is_err() {
            let bytes = self.error_event("upstream sent an oversized stream event");
            out.extend_from_slice(&bytes);
            return true;
        }
        out.len() > before && self.encoder.saw_output()
    }

    /// Parse a trailing event that arrived without a blank line, then give
    /// Chat/Responses clients a terminal event when the upstream closed cleanly.
    /// Messages never gets a synthesized `message_stop` (the pump sends an
    /// error event instead).
    pub fn finish(&mut self, out: &mut Vec<u8>) {
        if self.encoder.done() {
            return;
        }
        let mut events = Vec::new();
        let decoder = &mut self.decoder;
        let encoder = &mut self.encoder;
        let framed = self.framer.finish(|sse| {
            if encoder.done() {
                return;
            }
            events.clear();
            decoder.decode(sse, &mut events);
            for event in events.drain(..) {
                encoder.encode(event, out);
                if encoder.done() {
                    break;
                }
            }
        });
        if framed.is_err() {
            let bytes = self.error_event("upstream sent an oversized stream event");
            out.extend_from_slice(&bytes);
            return;
        }
        if !self.encoder.done() && self.encoder.target() != Proto::Messages {
            events.clear();
            self.decoder.flush_pending_tools(&mut events);
            events.push(StreamEvent::Terminal);
            for event in events {
                self.encoder.encode(event, out);
            }
        }
    }

    pub fn done(&self) -> bool {
        self.encoder.done()
    }

    pub fn usage(&self) -> Option<Usage> {
        self.decoder.usage()
    }

    pub fn keepalive(&self) -> &'static [u8] {
        if self.encoder.target() == Proto::Messages {
            stream::MESSAGES_PING_EVENT
        } else {
            stream::KEEPALIVE_EVENT
        }
    }

    pub fn error_event(&mut self, message: &str) -> Vec<u8> {
        let mut out = Vec::new();
        self.encoder.emit_error(message, &mut out);
        out
    }
}

// ---------------------------------------------------------------------------
// Upstream decoder -> StreamEvent IR
// ---------------------------------------------------------------------------

#[derive(Default)]
struct PendingTool {
    id: String,
    name: String,
    arguments: String,
}

struct StreamDecoder {
    from: Proto,
    usage: Option<Usage>,
    messages_usage: MessagesUsageFields,
    /// Chat upstreams interleave parallel tool calls by `index`; buffer them
    /// and flush in index order at the terminal so Messages/Responses blocks
    /// stay contiguous.
    pending_tools: BTreeMap<u64, PendingTool>,
    next_tool_seq: u32,
    /// Tracks whether Responses already emitted text deltas so `.done`
    /// fallbacks do not duplicate streamed text.
    responses_text_seen: bool,
    /// Active Responses function call `(call_id, streamed_arguments)` used to
    /// emit only the missing suffix on `response.output_item.done`.
    responses_open_tool: Option<(String, String)>,
    /// Index of the currently open Messages `tool_use` content block, if any.
    messages_open_tool_index: Option<u64>,
}

impl StreamDecoder {
    fn new(from: Proto) -> Self {
        Self {
            from,
            usage: None,
            messages_usage: MessagesUsageFields::default(),
            pending_tools: BTreeMap::new(),
            next_tool_seq: 0,
            responses_text_seen: false,
            responses_open_tool: None,
            messages_open_tool_index: None,
        }
    }

    fn usage(&self) -> Option<Usage> {
        self.usage
    }

    fn decode(&mut self, event: SseEvent, out: &mut Vec<StreamEvent>) {
        if event.data.trim() == "[DONE]" {
            self.flush_pending_tools(out);
            out.push(StreamEvent::Terminal);
            return;
        }
        let Ok(value) = serde_json::from_str::<Value>(&event.data) else {
            return;
        };
        if let Some(usage) = self.note_usage(&value) {
            out.push(StreamEvent::Usage(usage));
        }
        if let Some(id) = extract_response_id(&value) {
            out.push(StreamEvent::ResponseId(id));
        }
        let kind = event
            .name
            .as_deref()
            .or_else(|| value.get("type").and_then(Value::as_str))
            .unwrap_or("");
        match self.from {
            Proto::Chat => self.decode_chat(&value, out),
            Proto::Responses => self.decode_responses(kind, &value, out),
            Proto::Messages => self.decode_messages(kind, &value, out),
        }
    }

    fn note_usage(&mut self, value: &Value) -> Option<Usage> {
        match self.from {
            Proto::Messages => {
                let mut changed = false;
                if let Some(u) = value.pointer("/message/usage") {
                    self.messages_usage.merge(MessagesUsageFields::parse(u));
                    changed = true;
                }
                if let Some(u) = value.get("usage") {
                    self.messages_usage.merge(MessagesUsageFields::parse(u));
                    changed = true;
                }
                if changed {
                    self.usage = self.messages_usage.to_usage();
                    self.usage
                } else {
                    None
                }
            }
            Proto::Chat => {
                let found = Usage::from_chat(value)?;
                let merged = self.usage.map_or(found, |old| merge_usage(old, found));
                self.usage = Some(merged);
                Some(merged)
            }
            Proto::Responses => {
                let found = Usage::from_responses(value)?;
                let merged = self.usage.map_or(found, |old| merge_usage(old, found));
                self.usage = Some(merged);
                Some(merged)
            }
        }
    }

    fn decode_chat(&mut self, value: &Value, out: &mut Vec<StreamEvent>) {
        if let Some(message) = value
            .get("error")
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str)
        {
            out.push(StreamEvent::Error(message.to_string()));
            return;
        }
        let Some(choice) = value.pointer("/choices/0").and_then(Value::as_object) else {
            return;
        };
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            out.push(StreamEvent::Finish(map_finish(reason, false)));
        }
        let Some(delta) = choice.get("delta").and_then(Value::as_object) else {
            return;
        };
        if let Some(text) = delta
            .get("content")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
        {
            out.push(StreamEvent::TextDelta(text.to_string()));
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                self.record_chat_tool(call);
            }
        }
    }

    fn record_chat_tool(&mut self, call: &Value) {
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
        if let Some(function) = call.get("function").and_then(Value::as_object) {
            if let Some(name) = function
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
            {
                entry.name = name.to_string();
            }
            if let Some(arguments) = function
                .get("arguments")
                .and_then(Value::as_str)
                .filter(|arguments| !arguments.is_empty())
            {
                entry.arguments.push_str(arguments);
            }
        }
    }

    fn flush_pending_tools(&mut self, out: &mut Vec<StreamEvent>) {
        if self.from != Proto::Chat {
            return;
        }
        let pending = std::mem::take(&mut self.pending_tools);
        for (_, tool) in pending {
            if tool.name.is_empty() {
                continue;
            }
            let seq = self.next_tool_seq;
            self.next_tool_seq += 1;
            let id = if tool.id.is_empty() {
                format!("call_{seq}")
            } else {
                tool.id
            };
            out.push(StreamEvent::ToolStart {
                id,
                name: tool.name,
            });
            if !tool.arguments.is_empty() {
                out.push(StreamEvent::ToolArgs(tool.arguments));
            }
            out.push(StreamEvent::ToolEnd);
        }
    }

    fn decode_responses(&mut self, kind: &str, value: &Value, out: &mut Vec<StreamEvent>) {
        match kind {
            "response.output_text.delta" => {
                if let Some(delta) = value
                    .get("delta")
                    .and_then(Value::as_str)
                    .filter(|d| !d.is_empty())
                {
                    self.responses_text_seen = true;
                    out.push(StreamEvent::TextDelta(delta.to_string()));
                }
            }
            "response.output_text.done" => {
                if !self.responses_text_seen {
                    if let Some(text) = value
                        .get("text")
                        .and_then(Value::as_str)
                        .filter(|t| !t.is_empty())
                    {
                        self.responses_text_seen = true;
                        out.push(StreamEvent::TextDelta(text.to_string()));
                    }
                }
            }
            "response.output_item.added" => {
                if let Some(item) = value.get("item").and_then(Value::as_object) {
                    if item.get("type").and_then(Value::as_str) == Some("function_call") {
                        let id = item
                            .get("call_id")
                            .or_else(|| item.get("id"))
                            .and_then(Value::as_str)
                            .unwrap_or("call_unknown")
                            .to_string();
                        let name = item
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("function")
                            .to_string();
                        self.responses_open_tool = Some((id.clone(), String::new()));
                        out.push(StreamEvent::ToolStart { id, name });
                    }
                }
            }
            "response.function_call_arguments.delta" => {
                if let Some(delta) = value
                    .get("delta")
                    .and_then(Value::as_str)
                    .filter(|d| !d.is_empty())
                {
                    if let Some((_, buf)) = self.responses_open_tool.as_mut() {
                        buf.push_str(delta);
                    }
                    out.push(StreamEvent::ToolArgs(delta.to_string()));
                }
            }
            "response.output_item.done" => self.decode_responses_item_done(value, out),
            "response.completed" => out.push(StreamEvent::Terminal),
            "response.incomplete" => {
                let reason = value
                    .pointer("/response/incomplete_details/reason")
                    .and_then(Value::as_str);
                out.push(StreamEvent::Finish(if reason == Some("content_filter") {
                    "content_filter"
                } else {
                    "length"
                }));
                out.push(StreamEvent::Terminal);
            }
            "response.failed" | "response.error" | "error" => {
                let message = value
                    .pointer("/response/error/message")
                    .or_else(|| value.pointer("/error/message"))
                    .or_else(|| value.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("upstream response failed");
                out.push(StreamEvent::Error(message.to_string()));
            }
            _ => {}
        }
    }

    fn decode_responses_item_done(&mut self, value: &Value, out: &mut Vec<StreamEvent>) {
        let Some(item) = value.get("item").and_then(Value::as_object) else {
            return;
        };
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                if !self.responses_text_seen {
                    let text = text_of(item.get("content"));
                    if !text.is_empty() {
                        self.responses_text_seen = true;
                        out.push(StreamEvent::TextDelta(text));
                    }
                }
                // Reset so a subsequent message item (e.g. text -> tool -> text)
                // can still fall back to `.done` if its deltas were omitted.
                self.responses_text_seen = false;
            }
            Some("function_call") => {
                let arguments = json_string(item.get("arguments"));
                let call_id = item
                    .get("call_id")
                    .or_else(|| item.get("id"))
                    .and_then(Value::as_str);
                let matched = self
                    .responses_open_tool
                    .as_ref()
                    .is_some_and(|(open_id, _)| call_id == Some(open_id.as_str()));
                if matched {
                    if let Some((_, streamed)) = self.responses_open_tool.take() {
                        if let Some(rest) = arguments.strip_prefix(&streamed) {
                            if !rest.is_empty() {
                                out.push(StreamEvent::ToolArgs(rest.to_string()));
                            }
                        }
                    }
                    out.push(StreamEvent::ToolEnd);
                } else if !arguments.is_empty() || item.get("name").is_some() {
                    out.push(StreamEvent::ToolStart {
                        id: call_id.unwrap_or("call_unknown").to_string(),
                        name: item
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("function")
                            .to_string(),
                    });
                    if !arguments.is_empty() {
                        out.push(StreamEvent::ToolArgs(arguments));
                    }
                    out.push(StreamEvent::ToolEnd);
                }
            }
            _ => {}
        }
    }

    fn decode_messages(&mut self, kind: &str, value: &Value, out: &mut Vec<StreamEvent>) {
        match kind {
            "content_block_start" => {
                let Some(block) = value.get("content_block").and_then(Value::as_object) else {
                    return;
                };
                if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                    self.messages_open_tool_index = value.get("index").and_then(Value::as_u64);
                    out.push(StreamEvent::ToolStart {
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
                    });
                }
            }
            "content_block_delta" => {
                let Some(delta) = value.get("delta").and_then(Value::as_object) else {
                    return;
                };
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        if let Some(text) = delta
                            .get("text")
                            .and_then(Value::as_str)
                            .filter(|t| !t.is_empty())
                        {
                            out.push(StreamEvent::TextDelta(text.to_string()));
                        }
                    }
                    Some("input_json_delta") => {
                        if let Some(partial) = delta
                            .get("partial_json")
                            .and_then(Value::as_str)
                            .filter(|p| !p.is_empty())
                        {
                            out.push(StreamEvent::ToolArgs(partial.to_string()));
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let index = value.get("index").and_then(Value::as_u64);
                if self.messages_open_tool_index.is_some() && self.messages_open_tool_index == index
                {
                    self.messages_open_tool_index = None;
                    out.push(StreamEvent::ToolEnd);
                }
            }
            "message_delta" => {
                if let Some(reason) = value.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    out.push(StreamEvent::Finish(map_finish(reason, false)));
                }
            }
            "message_stop" => out.push(StreamEvent::Terminal),
            "error" => {
                let message = value
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("upstream stream error");
                out.push(StreamEvent::Error(message.to_string()));
            }
            _ => {}
        }
    }
}

fn extract_response_id(value: &Value) -> Option<String> {
    value
        .get("id")
        .or_else(|| value.pointer("/response/id"))
        .or_else(|| value.pointer("/message/id"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
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
// Per-target StreamEncoder
// ---------------------------------------------------------------------------

struct EncoderCommon {
    model_id: String,
    response_id: String,
    created: i64,
    usage: Option<Usage>,
    finish: Option<&'static str>,
    tools_seen: u32,
    saw_output: bool,
    done: bool,
}

impl EncoderCommon {
    fn new(model_id: &str) -> Self {
        Self {
            model_id: model_id.to_string(),
            response_id: "umsp".into(),
            created: chrono::Utc::now().timestamp(),
            usage: None,
            finish: None,
            tools_seen: 0,
            saw_output: false,
            done: false,
        }
    }

    /// Resolve the effective finish reason at terminal time. Tool-bearing turns
    /// that an upstream closed with plain `"stop"` still report `"tool_calls"`.
    fn resolved_finish(&self) -> &'static str {
        let has_tools = self.tools_seen > 0;
        match self.finish {
            Some("stop") | None if has_tools => "tool_calls",
            Some(finish) => finish,
            None => "stop",
        }
    }
}

struct StreamEncoder {
    common: EncoderCommon,
    target: TargetEncoder,
}

enum TargetEncoder {
    Chat(ChatEncoder),
    Responses(ResponsesEncoder),
    Messages(MessagesEncoder),
}

impl StreamEncoder {
    fn new(to: Proto, model_id: &str, emit_event_lines: bool) -> Self {
        let target = match to {
            Proto::Chat => TargetEncoder::Chat(ChatEncoder::default()),
            Proto::Responses => TargetEncoder::Responses(ResponsesEncoder::new(emit_event_lines)),
            Proto::Messages => TargetEncoder::Messages(MessagesEncoder::default()),
        };
        Self {
            common: EncoderCommon::new(model_id),
            target,
        }
    }

    fn target(&self) -> Proto {
        match self.target {
            TargetEncoder::Chat(_) => Proto::Chat,
            TargetEncoder::Responses(_) => Proto::Responses,
            TargetEncoder::Messages(_) => Proto::Messages,
        }
    }

    fn done(&self) -> bool {
        self.common.done
    }

    fn saw_output(&self) -> bool {
        self.common.saw_output
    }

    fn encode(&mut self, event: StreamEvent, out: &mut Vec<u8>) {
        if self.common.done {
            return;
        }
        match event {
            StreamEvent::ResponseId(id) => self.common.response_id = id,
            StreamEvent::Usage(usage) => self.common.usage = Some(usage),
            StreamEvent::Finish(reason) => self.common.finish = Some(reason),
            StreamEvent::TextDelta(text) => {
                if text.is_empty() {
                    return;
                }
                match &mut self.target {
                    TargetEncoder::Chat(enc) => enc.emit_text(&self.common, &text, out),
                    TargetEncoder::Responses(enc) => enc.emit_text(&self.common, &text, out),
                    TargetEncoder::Messages(enc) => enc.emit_text(&self.common, &text, out),
                }
                self.common.saw_output = true;
            }
            StreamEvent::ToolStart { id, name } => {
                self.common.tools_seen += 1;
                match &mut self.target {
                    TargetEncoder::Chat(enc) => enc.begin_tool(&self.common, &id, &name, out),
                    TargetEncoder::Responses(enc) => enc.begin_tool(&self.common, &id, &name, out),
                    TargetEncoder::Messages(enc) => enc.begin_tool(&self.common, &id, &name, out),
                }
                self.common.saw_output = true;
            }
            StreamEvent::ToolArgs(args) => {
                if args.is_empty() {
                    return;
                }
                let emitted = match &mut self.target {
                    TargetEncoder::Chat(enc) => enc.emit_args(&self.common, &args, out),
                    TargetEncoder::Responses(enc) => enc.emit_args(&args, out),
                    TargetEncoder::Messages(enc) => enc.emit_args(&args, out),
                };
                if emitted {
                    self.common.saw_output = true;
                }
            }
            StreamEvent::ToolEnd => match &mut self.target {
                TargetEncoder::Chat(enc) => enc.close_tool(),
                TargetEncoder::Responses(enc) => enc.close_tool(out),
                TargetEncoder::Messages(enc) => enc.close_tool(out),
            },
            StreamEvent::Terminal => {
                match &mut self.target {
                    TargetEncoder::Chat(enc) => enc.emit_terminal(&self.common, out),
                    TargetEncoder::Responses(enc) => enc.emit_terminal(&self.common, out),
                    TargetEncoder::Messages(enc) => enc.emit_terminal(&self.common, out),
                }
                self.common.done = true;
                self.common.saw_output = true;
            }
            StreamEvent::Error(message) => {
                self.emit_error(&message, out);
            }
        }
    }

    fn emit_error(&mut self, message: &str, out: &mut Vec<u8>) {
        self.common.done = true;
        match &mut self.target {
            TargetEncoder::Messages(_) => {
                out.extend_from_slice(&stream::messages_error_event(message));
            }
            TargetEncoder::Responses(enc) => {
                let payload = stream::responses_error_payload(message);
                enc.write_value_event("error", payload, out);
            }
            TargetEncoder::Chat(_) => {
                out.extend_from_slice(&stream::error_event(message));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Chat target encoder
// ---------------------------------------------------------------------------

#[derive(Default)]
struct ChatEncoder {
    tool_open: bool,
    tool_seq: u32,
}

#[derive(Serialize)]
struct ChatChunkFrame<'a, D: Serialize> {
    id: &'a str,
    object: &'static str,
    created: i64,
    model: &'a str,
    choices: [ChatChoiceFrame<D>; 1],
}

#[derive(Serialize)]
struct ChatChoiceFrame<D: Serialize> {
    index: u32,
    delta: D,
    finish_reason: Option<&'static str>,
}

#[derive(Serialize)]
struct ChatTextDelta<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<&'static str>,
    content: &'a str,
}

#[derive(Serialize)]
struct ChatToolArgsDelta<'a> {
    tool_calls: [ChatToolArgsItem<'a>; 1],
}

#[derive(Serialize)]
struct ChatToolArgsItem<'a> {
    index: u32,
    function: ChatToolArgsFn<'a>,
}

#[derive(Serialize)]
struct ChatToolArgsFn<'a> {
    arguments: &'a str,
}

impl ChatEncoder {
    fn emit_text(&mut self, common: &EncoderCommon, text: &str, out: &mut Vec<u8>) {
        self.tool_open = false;
        let id = format!("chatcmpl-{}", common.response_id);
        let frame = ChatChunkFrame {
            id: &id,
            object: "chat.completion.chunk",
            created: common.created,
            model: &common.model_id,
            choices: [ChatChoiceFrame {
                index: 0,
                delta: ChatTextDelta {
                    role: (!common.saw_output).then_some("assistant"),
                    content: text,
                },
                finish_reason: None,
            }],
        };
        write_data_json(&frame, out);
    }

    fn begin_tool(&mut self, common: &EncoderCommon, id: &str, name: &str, out: &mut Vec<u8>) {
        self.tool_open = true;
        let index = self.tool_seq;
        self.tool_seq += 1;
        let delta = if !common.saw_output {
            json!({
                "role": "assistant",
                "tool_calls": [{
                    "index": index,
                    "id": id,
                    "type": "function",
                    "function": {"name": name, "arguments": ""},
                }],
            })
        } else {
            json!({
                "tool_calls": [{
                    "index": index,
                    "id": id,
                    "type": "function",
                    "function": {"name": name, "arguments": ""},
                }],
            })
        };
        write_chat_value_chunk(
            &common.response_id,
            common.created,
            &common.model_id,
            delta,
            None,
            None,
            out,
        );
    }

    fn emit_args(&mut self, common: &EncoderCommon, arguments: &str, out: &mut Vec<u8>) -> bool {
        if !self.tool_open {
            return false;
        }
        let id = format!("chatcmpl-{}", common.response_id);
        let frame = ChatChunkFrame {
            id: &id,
            object: "chat.completion.chunk",
            created: common.created,
            model: &common.model_id,
            choices: [ChatChoiceFrame {
                index: 0,
                delta: ChatToolArgsDelta {
                    tool_calls: [ChatToolArgsItem {
                        index: self.tool_seq.saturating_sub(1),
                        function: ChatToolArgsFn { arguments },
                    }],
                },
                finish_reason: None,
            }],
        };
        write_data_json(&frame, out);
        true
    }

    fn close_tool(&mut self) {
        self.tool_open = false;
    }

    fn emit_terminal(&mut self, common: &EncoderCommon, out: &mut Vec<u8>) {
        self.tool_open = false;
        let finish = common.resolved_finish();
        write_chat_value_chunk(
            &common.response_id,
            common.created,
            &common.model_id,
            json!({}),
            Some(chat_finish(finish)),
            common.usage,
            out,
        );
        out.extend_from_slice(stream::DONE_EVENT);
    }
}

fn write_chat_value_chunk(
    id: &str,
    created: i64,
    model: &str,
    delta: Value,
    finish: Option<&str>,
    usage: Option<Usage>,
    out: &mut Vec<u8>,
) {
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
    write_data_json(&payload, out);
}

// ---------------------------------------------------------------------------
// Messages target encoder
// ---------------------------------------------------------------------------

#[derive(Default)]
struct MessagesEncoder {
    started: bool,
    text_open: bool,
    tool_open: bool,
    block_index: u32,
}

#[derive(Serialize)]
struct MessagesTextDeltaFrame<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    index: u32,
    delta: MessagesTextDelta<'a>,
}

#[derive(Serialize)]
struct MessagesTextDelta<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    text: &'a str,
}

#[derive(Serialize)]
struct MessagesInputJsonDeltaFrame<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    index: u32,
    delta: MessagesInputJsonDelta<'a>,
}

#[derive(Serialize)]
struct MessagesInputJsonDelta<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    partial_json: &'a str,
}

impl MessagesEncoder {
    fn ensure_started(&mut self, common: &EncoderCommon, out: &mut Vec<u8>) {
        if self.started {
            return;
        }
        self.started = true;
        let usage = messages_usage(common.usage);
        write_sse_json(
            "message_start",
            &json!({
                "type": "message_start",
                "message": {
                    "id": format!("msg_{}", common.response_id),
                    "type": "message",
                    "role": "assistant",
                    "model": common.model_id,
                    "content": [],
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": usage,
                },
            }),
            true,
            out,
        );
    }

    fn emit_text(&mut self, common: &EncoderCommon, text: &str, out: &mut Vec<u8>) {
        self.ensure_started(common, out);
        self.close_tool(out);
        if !self.text_open {
            write_sse_json(
                "content_block_start",
                &json!({
                    "type": "content_block_start",
                    "index": self.block_index,
                    "content_block": {"type": "text", "text": ""},
                }),
                true,
                out,
            );
            self.text_open = true;
        }
        let frame = MessagesTextDeltaFrame {
            kind: "content_block_delta",
            index: self.block_index,
            delta: MessagesTextDelta {
                kind: "text_delta",
                text,
            },
        };
        write_sse_json("content_block_delta", &frame, true, out);
    }

    fn begin_tool(&mut self, common: &EncoderCommon, id: &str, name: &str, out: &mut Vec<u8>) {
        self.ensure_started(common, out);
        self.close_text(out);
        self.close_tool(out);
        self.tool_open = true;
        write_sse_json(
            "content_block_start",
            &json!({
                "type": "content_block_start",
                "index": self.block_index,
                "content_block": {
                    "type": "tool_use",
                    "id": id,
                    "name": name,
                    "input": {},
                },
            }),
            true,
            out,
        );
    }

    fn emit_args(&mut self, arguments: &str, out: &mut Vec<u8>) -> bool {
        if !self.tool_open {
            return false;
        }
        let frame = MessagesInputJsonDeltaFrame {
            kind: "content_block_delta",
            index: self.block_index,
            delta: MessagesInputJsonDelta {
                kind: "input_json_delta",
                partial_json: arguments,
            },
        };
        write_sse_json("content_block_delta", &frame, true, out);
        true
    }

    fn close_text(&mut self, out: &mut Vec<u8>) {
        if !self.text_open {
            return;
        }
        self.text_open = false;
        write_sse_json(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": self.block_index}),
            true,
            out,
        );
        self.block_index += 1;
    }

    fn close_tool(&mut self, out: &mut Vec<u8>) {
        if !self.tool_open {
            return;
        }
        self.tool_open = false;
        write_sse_json(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": self.block_index}),
            true,
            out,
        );
        self.block_index += 1;
    }

    fn emit_terminal(&mut self, common: &EncoderCommon, out: &mut Vec<u8>) {
        self.ensure_started(common, out);
        self.close_text(out);
        self.close_tool(out);
        let stop = messages_stop(common.resolved_finish());
        let usage = messages_usage(common.usage);
        write_sse_json(
            "message_delta",
            &json!({
                "type": "message_delta",
                "delta": {"stop_reason": stop, "stop_sequence": null},
                "usage": {"output_tokens": usage["output_tokens"]},
            }),
            true,
            out,
        );
        write_sse_json("message_stop", &json!({"type": "message_stop"}), true, out);
    }
}

// ---------------------------------------------------------------------------
// Responses target encoder
// ---------------------------------------------------------------------------

struct ResponsesEncoder {
    emit_event_lines: bool,
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
    output_index: u32,
    item_seq: u32,
    output_items: Vec<Value>,
}

#[derive(Serialize)]
struct ResponsesTextDeltaFrame<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    item_id: &'a str,
    output_index: u32,
    content_index: u32,
    delta: &'a str,
    logprobs: [(); 0],
    sequence_number: u64,
}

#[derive(Serialize)]
struct ResponsesToolArgsDeltaFrame<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    item_id: &'a str,
    output_index: u32,
    delta: &'a str,
    sequence_number: u64,
}

impl ResponsesEncoder {
    fn new(emit_event_lines: bool) -> Self {
        Self {
            emit_event_lines,
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
            output_index: 0,
            item_seq: 0,
            output_items: Vec::new(),
        }
    }

    fn ensure_started(&mut self, common: &EncoderCommon, out: &mut Vec<u8>) {
        if self.started {
            return;
        }
        self.started = true;
        let response = responses_envelope(common, "in_progress", Vec::new(), None);
        self.write_value_event(
            "response.created",
            json!({"type": "response.created", "response": response}),
            out,
        );
    }

    fn emit_text(&mut self, common: &EncoderCommon, text: &str, out: &mut Vec<u8>) {
        self.ensure_started(common, out);
        self.close_tool(out);
        if !self.text_open {
            self.text_open = true;
            self.text_buf.clear();
            self.item_seq += 1;
            self.text_item_id = format!("msg_{}_{}", common.response_id, self.item_seq);
            let item_id = self.text_item_id.clone();
            self.write_value_event(
                "response.output_item.added",
                json!({
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
                out,
            );
            self.write_value_event(
                "response.content_part.added",
                json!({
                    "type": "response.content_part.added",
                    "item_id": self.text_item_id,
                    "output_index": self.output_index,
                    "content_index": 0,
                    "part": {"type": "output_text", "text": "", "annotations": []},
                }),
                out,
            );
        }
        let seq = self.next_seq();
        let frame = ResponsesTextDeltaFrame {
            kind: "response.output_text.delta",
            item_id: &self.text_item_id,
            output_index: self.output_index,
            content_index: 0,
            delta: text,
            logprobs: [],
            sequence_number: seq,
        };
        write_sse_json(
            "response.output_text.delta",
            &frame,
            self.emit_event_lines,
            out,
        );
        self.text_buf.push_str(text);
    }

    fn begin_tool(&mut self, common: &EncoderCommon, id: &str, name: &str, out: &mut Vec<u8>) {
        self.ensure_started(common, out);
        self.close_text(out);
        self.close_tool(out);
        self.tool_id = id.to_string();
        self.tool_name = name.to_string();
        self.args_buf.clear();
        self.tool_open = true;
        self.item_seq += 1;
        self.tool_item_id = format!("fc_{}_{}", common.response_id, self.item_seq);
        self.write_value_event(
            "response.output_item.added",
            json!({
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
            out,
        );
    }

    fn emit_args(&mut self, arguments: &str, out: &mut Vec<u8>) -> bool {
        if !self.tool_open {
            return false;
        }
        self.args_buf.push_str(arguments);
        let seq = self.next_seq();
        let frame = ResponsesToolArgsDeltaFrame {
            kind: "response.function_call_arguments.delta",
            item_id: &self.tool_item_id,
            output_index: self.output_index,
            delta: arguments,
            sequence_number: seq,
        };
        write_sse_json(
            "response.function_call_arguments.delta",
            &frame,
            self.emit_event_lines,
            out,
        );
        true
    }

    fn close_text(&mut self, out: &mut Vec<u8>) {
        if !self.text_open {
            return;
        }
        self.text_open = false;
        let item_id = self.text_item_id.clone();
        let text = std::mem::take(&mut self.text_buf);
        self.write_value_event(
            "response.output_text.done",
            json!({
                "type": "response.output_text.done",
                "item_id": item_id,
                "output_index": self.output_index,
                "content_index": 0,
                "text": text,
                "logprobs": [],
            }),
            out,
        );
        let item = json!({
            "type": "message",
            "id": item_id,
            "role": "assistant",
            "status": "completed",
            "content": [{"type": "output_text", "text": text, "annotations": []}],
        });
        self.write_value_event(
            "response.output_item.done",
            json!({
                "type": "response.output_item.done",
                "output_index": self.output_index,
                "item": item,
            }),
            out,
        );
        self.output_items.push(item);
        self.output_index += 1;
    }

    fn close_tool(&mut self, out: &mut Vec<u8>) {
        if !self.tool_open {
            return;
        }
        self.tool_open = false;
        let arguments = std::mem::take(&mut self.args_buf);
        self.write_value_event(
            "response.function_call_arguments.done",
            json!({
                "type": "response.function_call_arguments.done",
                "item_id": self.tool_item_id,
                "output_index": self.output_index,
                "name": self.tool_name,
                "arguments": arguments,
            }),
            out,
        );
        let item = json!({
            "type": "function_call",
            "id": self.tool_item_id,
            "call_id": self.tool_id,
            "name": self.tool_name,
            "arguments": arguments,
            "status": "completed",
        });
        self.write_value_event(
            "response.output_item.done",
            json!({
                "type": "response.output_item.done",
                "output_index": self.output_index,
                "item": item,
            }),
            out,
        );
        self.output_items.push(item);
        self.output_index += 1;
    }

    fn emit_terminal(&mut self, common: &EncoderCommon, out: &mut Vec<u8>) {
        self.ensure_started(common, out);
        self.close_text(out);
        self.close_tool(out);
        let finish = common.resolved_finish();
        let status = if finish == "length" || finish == "content_filter" {
            "incomplete"
        } else {
            "completed"
        };
        let output = std::mem::take(&mut self.output_items);
        let mut response = responses_envelope(common, status, output, common.usage);
        if status == "incomplete" {
            let reason = if finish == "length" {
                "max_output_tokens"
            } else {
                "content_filter"
            };
            response["incomplete_details"] = json!({"reason": reason});
        }
        // The Responses wire protocol ends an incomplete response with
        // `response.incomplete`; SDK clients dispatch on the event type.
        let event = if status == "incomplete" {
            "response.incomplete"
        } else {
            "response.completed"
        };
        self.write_value_event(event, json!({"type": event, "response": response}), out);
    }

    fn next_seq(&mut self) -> u64 {
        let seq = self.seq;
        self.seq += 1;
        seq
    }

    fn write_value_event(&mut self, event: &str, mut payload: Value, out: &mut Vec<u8>) {
        let seq = self.next_seq();
        if let Some(obj) = payload.as_object_mut() {
            obj.insert("sequence_number".into(), json!(seq));
        }
        write_sse_json(event, &payload, self.emit_event_lines, out);
    }
}

fn responses_envelope(
    common: &EncoderCommon,
    status: &str,
    output: Vec<Value>,
    usage: Option<Usage>,
) -> Value {
    json!({
        "id": format!("resp_{}", common.response_id),
        "object": "response",
        "created_at": common.created,
        "status": status,
        "model": common.model_id,
        "output": output,
        "parallel_tool_calls": false,
        "tool_choice": "auto",
        "tools": [],
        "usage": responses_usage(usage),
    })
}

fn write_data_json<T: Serialize>(payload: &T, out: &mut Vec<u8>) {
    out.extend_from_slice(b"data: ");
    if serde_json::to_writer(&mut *out, payload).is_err() {
        out.extend_from_slice(b"{}");
    }
    out.extend_from_slice(b"\n\n");
}

fn write_sse_json<T: Serialize>(
    event: &str,
    payload: &T,
    with_event_line: bool,
    out: &mut Vec<u8>,
) {
    if with_event_line {
        out.extend_from_slice(b"event: ");
        out.extend_from_slice(event.as_bytes());
        out.push(b'\n');
    }
    write_data_json(payload, out);
}
