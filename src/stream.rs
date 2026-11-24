//! SSE side-scanning for streaming Chat Completions, Responses, and Messages.
//!
//! The gateway forwards upstream bytes verbatim (spec 2.0: no buffering, no
//! aggregation). This module only *observes* a copy of those bytes so the
//! request ledger can record usage and so the gateway knows whether the
//! upstream already sent the terminal marker.
//!
//! Framing delegates to [`crate::sse::LineFramer`] so multi-byte UTF-8
//! characters and CRLF/CR/LF line endings split across chunk boundaries are
//! handled consistently everywhere.

use serde_json::json;

use crate::sse::{self, LineFramer, LinePiece, MAX_EVENT_BYTES, MAX_OBSERVER_LINE_BYTES};
use crate::usage::Usage;

/// SSE comment sent while the upstream is quiet (spec 2.0, idle keep-alive).
pub const KEEPALIVE_EVENT: &[u8] = b": keep-alive\n\n";

/// Anthropic keepalive: a real `ping` event, not a comment (spec 2.0 / 2.3).
pub const MESSAGES_PING_EVENT: &[u8] = b"event: ping\ndata: {\"type\":\"ping\"}\n\n";

/// Terminal event of an OpenAI Chat Completions stream.
pub const DONE_EVENT: &[u8] = b"data: [DONE]\n\n";

/// Build a protocol-native Chat error event for a stream that broke after the
/// 200 was already sent (spec 2.1 / 8.3).
pub fn error_event(message: &str) -> Vec<u8> {
    let payload = json!({
        "error": {
            "message": message,
            "type": "upstream_error",
            "code": "upstream_stream_error"
        }
    });
    let body = serde_json::to_string(&payload).unwrap_or_default();
    format!("data: {body}\n\n").into_bytes()
}

/// Payload of a Responses stream `error` event. The Responses wire protocol
/// defines it flat (`type`, `code`, `message`, `param`, `sequence_number`),
/// unlike the nested Chat and Anthropic shapes. The caller adds
/// `sequence_number`.
pub fn responses_error_payload(message: &str) -> serde_json::Value {
    json!({
        "type": "error",
        "code": "upstream_stream_error",
        "message": message,
        "param": null
    })
}

/// Build a protocol-native Responses error event for a stream that broke after
/// the 200 was already sent (spec 2.2 / 8.3): `event: error` + the flat
/// payload, numbered after the last upstream event when it carried one.
pub fn responses_error_event(message: &str, sequence_number: Option<u64>) -> Vec<u8> {
    let mut payload = responses_error_payload(message);
    if let (Some(seq), Some(obj)) = (sequence_number, payload.as_object_mut()) {
        obj.insert("sequence_number".into(), json!(seq));
    }
    let body = serde_json::to_string(&payload).unwrap_or_default();
    format!("event: error\ndata: {body}\n\n").into_bytes()
}

/// Build a protocol-native Anthropic error event for a stream that broke after
/// the 200 was already sent (spec 2.3 / 8.3). `message_stop` is never faked.
pub fn messages_error_event(message: &str) -> Vec<u8> {
    let payload = json!({
        "type": "error",
        "error": {
            "type": "upstream_error",
            "message": message
        }
    });
    let body = serde_json::to_string(&payload).unwrap_or_default();
    format!("event: error\ndata: {body}\n\n").into_bytes()
}

/// Incremental SSE observer for Chat Completions streams.
#[derive(Debug)]
pub struct SseScanner {
    framer: LineFramer,
    saw_data: bool,
    /// Upstream sent `data: [DONE]`.
    pub done: bool,
    /// Last `usage` object seen on a `data:` event.
    pub usage: Option<Usage>,
}

impl Default for SseScanner {
    fn default() -> Self {
        Self {
            framer: LineFramer::new(MAX_OBSERVER_LINE_BYTES),
            saw_data: false,
            done: false,
            usage: None,
        }
    }
}

impl SseScanner {
    /// Feed one raw chunk. Returns `true` exactly once: on the first chunk that
    /// carries a JSON `data:` event (the caller uses that moment as TTFT).
    pub fn feed(&mut self, chunk: &[u8]) -> bool {
        let mut first_data = false;
        let saw_data = &mut self.saw_data;
        let done = &mut self.done;
        let usage = &mut self.usage;
        self.framer.push(chunk, |piece| {
            if let LinePiece::Line(line) = piece {
                if handle_chat_line(line, *saw_data, done, usage) && !*saw_data {
                    *saw_data = true;
                    first_data = true;
                }
            }
        });
        first_data
    }
}

fn handle_chat_line(
    line: &[u8],
    saw_data: bool,
    done: &mut bool,
    usage: &mut Option<Usage>,
) -> bool {
    let content = sse::trim_eol(line);
    let Some((b"data", val)) = sse::parse_field(content) else {
        return false;
    };
    let payload = trim_ascii(val);
    if payload.is_empty() {
        return false;
    }
    if payload == b"[DONE]" {
        *done = true;
        return false;
    }
    // After TTFT is recorded, only events mentioning `"usage"` (or a JSON
    // unicode escape that could spell it) can update scanner state.
    if saw_data && !contains_bytes(payload, b"\"usage\"") && !contains_bytes(payload, b"\\u") {
        return true;
    }
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(payload) else {
        return false;
    };
    if let Some(found) = Usage::from_chat(&value) {
        *usage = Some(found);
    }
    true
}

/// OpenAI Responses stream patcher (spec 2.2).
///
/// OpenDesign only sends `data:` lines, while the Responses SSE spec pairs each
/// event with an `event: <type>` line. This patcher forwards every upstream byte
/// unchanged except for inserting the missing `event:` line in front of a
/// `data:` line whose JSON carries a `type`. It also observes usage and the
/// terminal `[DONE]` for the ledger.
///
/// Rules:
/// - an `event:` line already present in the same event block is never duplicated;
/// - `[DONE]`, comments, blank lines, other fields and their order are untouched;
/// - wiring `emit_event_lines = false` makes this a pure pass-through.
#[derive(Debug)]
pub struct ResponsesPatcher {
    emit_event_lines: bool,
    framer: LineFramer,
    /// Seen an `event:` line in the current event block (reset by a blank line).
    saw_event_line: bool,
    saw_data: bool,
    /// Upstream sent `data: [DONE]`.
    pub done: bool,
    /// Last `usage` object seen (non-streaming shape or `response.completed`).
    pub usage: Option<Usage>,
    /// Highest upstream `sequence_number`, so a synthesized error event
    /// continues the numbering.
    pub last_sequence: Option<u64>,
}

impl ResponsesPatcher {
    pub fn new(emit_event_lines: bool) -> Self {
        Self {
            emit_event_lines,
            framer: LineFramer::new(MAX_EVENT_BYTES),
            saw_event_line: false,
            saw_data: false,
            done: false,
            usage: None,
            last_sequence: None,
        }
    }

    /// Feed one raw upstream chunk. Appends the bytes to send to `out` and
    /// returns `true` exactly once: on the first JSON `data:` event (TTFT).
    pub fn feed(&mut self, chunk: &[u8], out: &mut Vec<u8>) -> bool {
        let mut first_data = false;
        let emit_event_lines = self.emit_event_lines;
        let saw_event_line = &mut self.saw_event_line;
        let saw_data = &mut self.saw_data;
        let done = &mut self.done;
        let usage = &mut self.usage;
        let last_sequence = &mut self.last_sequence;
        self.framer.push(chunk, |piece| match piece {
            LinePiece::Line(line) => {
                if handle_responses_line(
                    line,
                    emit_event_lines,
                    saw_event_line,
                    *saw_data,
                    done,
                    usage,
                    last_sequence,
                    out,
                ) && !*saw_data
                {
                    *saw_data = true;
                    first_data = true;
                }
            }
            LinePiece::Overflow(overflow) => {
                *saw_event_line = false;
                out.extend_from_slice(overflow);
            }
            LinePiece::Partial(_) => {}
        });
        first_data
    }

    /// Flush a trailing partial line (upstream closed mid-line) verbatim.
    pub fn finish(&mut self, out: &mut Vec<u8>) {
        let emit_event_lines = self.emit_event_lines;
        let saw_event_line = &mut self.saw_event_line;
        let saw_data = &mut self.saw_data;
        let done = &mut self.done;
        let usage = &mut self.usage;
        let last_sequence = &mut self.last_sequence;
        self.framer.finish(|piece| match piece {
            LinePiece::Line(line) => {
                if handle_responses_line(
                    line,
                    emit_event_lines,
                    saw_event_line,
                    *saw_data,
                    done,
                    usage,
                    last_sequence,
                    out,
                ) && !*saw_data
                {
                    *saw_data = true;
                }
            }
            LinePiece::Partial(partial) | LinePiece::Overflow(partial) => {
                out.extend_from_slice(partial);
            }
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_responses_line(
    line: &[u8],
    emit_event_lines: bool,
    saw_event_line: &mut bool,
    saw_data: bool,
    done: &mut bool,
    usage: &mut Option<Usage>,
    last_sequence: &mut Option<u64>,
    out: &mut Vec<u8>,
) -> bool {
    let content = sse::trim_eol(line);
    if content.is_empty() {
        *saw_event_line = false;
        out.extend_from_slice(line);
        return false;
    }
    let Some((field, val)) = sse::parse_field(content) else {
        out.extend_from_slice(line);
        return false;
    };
    if field == b"event" {
        *saw_event_line = true;
        out.extend_from_slice(line);
        return false;
    }
    if field != b"data" {
        out.extend_from_slice(line);
        return false;
    }
    let payload = trim_ascii(val);
    if payload.is_empty() {
        out.extend_from_slice(line);
        return false;
    }
    if payload == b"[DONE]" {
        *done = true;
        out.extend_from_slice(line);
        return false;
    }
    let needs_event_line = emit_event_lines && !*saw_event_line;
    if saw_data
        && !needs_event_line
        && !contains_bytes(payload, b"\"usage\"")
        && !contains_bytes(payload, b"\"sequence_number\"")
        && !contains_bytes(payload, b"\\u")
    {
        out.extend_from_slice(line);
        return true;
    }
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(payload) else {
        out.extend_from_slice(line);
        return false;
    };
    if needs_event_line {
        if let Some(kind) = value.get("type").and_then(|v| v.as_str()) {
            // Event names are ASCII tokens; refuse to synthesize anything else.
            let printable = !kind.is_empty()
                && kind
                    .bytes()
                    .all(|b| b.is_ascii_graphic() || b == b' ' || b == b'.' || b == b'_');
            if printable {
                out.extend_from_slice(b"event: ");
                out.extend_from_slice(kind.as_bytes());
                out.push(b'\n');
            }
        }
    }
    if let Some(found) = Usage::from_responses(&value) {
        *usage = Some(found);
    }
    if let Some(seq) = value.get("sequence_number").and_then(|v| v.as_u64()) {
        *last_sequence = Some(last_sequence.map_or(seq, |last| last.max(seq)));
    }
    out.extend_from_slice(line);
    true
}

/// Anthropic Messages stream observer (spec 2.3).
///
/// Bytes are forwarded verbatim — Anthropic SSE already carries `event:` lines,
/// and the gateway never rewrites, reorders or synthesizes protocol events. The
/// observer only reads a copy to learn: TTFT (first `data:` event), usage
/// (`message_start.message.usage` + `message_delta.usage`), and whether the
/// stream reached a terminal event (`message_stop`, or an upstream `error`).
#[derive(Debug)]
pub struct MessagesPatcher {
    framer: LineFramer,
    saw_data: bool,
    /// Upstream sent `message_stop` or a terminal `error` event.
    pub done: bool,
    usage: crate::usage::MessagesUsageFields,
}

impl Default for MessagesPatcher {
    fn default() -> Self {
        Self {
            framer: LineFramer::new(MAX_OBSERVER_LINE_BYTES),
            saw_data: false,
            done: false,
            usage: crate::usage::MessagesUsageFields::default(),
        }
    }
}

impl MessagesPatcher {
    /// Feed one raw upstream chunk. Returns `true` exactly once: on the first
    /// JSON `data:` event (TTFT).
    pub fn feed(&mut self, chunk: &[u8]) -> bool {
        let mut first_data = false;
        let saw_data = &mut self.saw_data;
        let done = &mut self.done;
        let usage = &mut self.usage;
        self.framer.push(chunk, |piece| {
            if let LinePiece::Line(line) = piece {
                if handle_messages_line(line, *saw_data, done, usage) && !*saw_data {
                    *saw_data = true;
                    first_data = true;
                }
            }
        });
        first_data
    }

    pub fn usage(&self) -> Option<Usage> {
        self.usage.to_usage()
    }
}

fn handle_messages_line(
    line: &[u8],
    saw_data: bool,
    done: &mut bool,
    usage: &mut crate::usage::MessagesUsageFields,
) -> bool {
    let content = sse::trim_eol(line);
    let Some((b"data", val)) = sse::parse_field(content) else {
        return false;
    };
    let payload = trim_ascii(val);
    if payload.is_empty() {
        return false;
    }
    // After TTFT is recorded, skip parsing hot content_block_* delta lines
    // that cannot carry message lifecycle or error events.
    if saw_data
        && !contains_bytes(payload, b"message_")
        && !contains_bytes(payload, b"error")
        && !contains_bytes(payload, b"\\u")
    {
        return true;
    }
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(payload) else {
        return false;
    };
    match value.get("type").and_then(|v| v.as_str()) {
        Some("message_start") => {
            if let Some(found) = value.pointer("/message/usage") {
                usage.merge(crate::usage::MessagesUsageFields::parse(found));
            }
        }
        Some("message_delta") => {
            if let Some(found) = value.get("usage") {
                usage.merge(crate::usage::MessagesUsageFields::parse(found));
            }
        }
        Some("message_stop") => *done = true,
        // An upstream `error` event terminates the stream; its body was
        // already forwarded verbatim, so the gateway adds nothing.
        Some("error") => *done = true,
        _ => {}
    }
    true
}

/// Side observer for one SSE stream: chat forwards bytes untouched, Responses
/// patches event lines. Both answer the same questions for the ledger: TTFT,
/// usage, terminal marker, and the protocol-native error event.
#[derive(Debug)]
pub enum Observer {
    Chat(SseScanner),
    Responses(ResponsesPatcher),
    Messages(MessagesPatcher),
}

impl Observer {
    pub fn chat() -> Self {
        Self::Chat(SseScanner::default())
    }

    pub fn responses(emit_event_lines: bool) -> Self {
        Self::Responses(ResponsesPatcher::new(emit_event_lines))
    }

    pub fn messages() -> Self {
        Self::Messages(MessagesPatcher::default())
    }

    /// Feed one upstream chunk; returns `true` on the first data event.
    pub fn transform(&mut self, chunk: &[u8], out: &mut Vec<u8>) -> bool {
        match self {
            Observer::Chat(scanner) => {
                let first = scanner.feed(chunk);
                out.extend_from_slice(chunk);
                first
            }
            Observer::Responses(patcher) => patcher.feed(chunk, out),
            Observer::Messages(patcher) => {
                let first = patcher.feed(chunk);
                out.extend_from_slice(chunk);
                first
            }
        }
    }

    /// Flush a trailing partial line (Responses only; chat forwards as it goes).
    pub fn finish(&mut self, out: &mut Vec<u8>) {
        if let Observer::Responses(patcher) = self {
            patcher.finish(out);
        }
    }

    pub fn done(&self) -> bool {
        match self {
            Observer::Chat(scanner) => scanner.done,
            Observer::Responses(patcher) => patcher.done,
            Observer::Messages(patcher) => patcher.done,
        }
    }

    pub fn usage(&self) -> Option<Usage> {
        match self {
            Observer::Chat(scanner) => scanner.usage,
            Observer::Responses(patcher) => patcher.usage,
            Observer::Messages(patcher) => patcher.usage(),
        }
    }

    /// Keepalive frame while the upstream is silent: an SSE comment for
    /// OpenAI-shaped streams, a real `ping` event for Anthropic.
    pub fn keepalive(&self) -> &'static [u8] {
        match self {
            Observer::Messages(_) => MESSAGES_PING_EVENT,
            _ => KEEPALIVE_EVENT,
        }
    }

    pub fn error_event(&self, message: &str) -> Vec<u8> {
        match self {
            Observer::Chat(_) => error_event(message),
            Observer::Responses(patcher) => {
                responses_error_event(message, patcher.last_sequence.map(|seq| seq + 1))
            }
            Observer::Messages(_) => messages_error_event(message),
        }
    }
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn trim_ascii(mut bytes: &[u8]) -> &[u8] {
    while let [b' ' | b'\t' | b'\n' | b'\r', rest @ ..] = bytes {
        bytes = rest;
    }
    while let [rest @ .., b' ' | b'\t' | b'\n' | b'\r'] = bytes {
        bytes = rest;
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_done_and_usage() {
        let mut scanner = SseScanner::default();
        // First JSON event is the TTFT moment.
        assert!(
            scanner.feed(b"data: {\"id\":\"\",\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n")
        );
        assert!(scanner.usage.is_none());
        assert!(!scanner.feed(
            b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":4,\"prompt_tokens_details\":{\"cached_tokens\":1},\"completion_tokens_details\":{\"reasoning_tokens\":2}}}\n\n",
        ));
        let usage = scanner.usage.unwrap();
        assert_eq!(usage.input_tokens, 3);
        assert_eq!(usage.output_tokens, 4);
        assert_eq!(usage.cached_tokens, 1);
        assert_eq!(usage.reasoning_tokens, 2);
        assert!(!scanner.done);
        scanner.feed(b"data: [DONE]\n\n");
        assert!(scanner.done);
    }

    #[test]
    fn done_split_across_chunks() {
        let mut scanner = SseScanner::default();
        scanner.feed(b"data: {\"choices\":[]}\n\ndata: [DO");
        assert!(!scanner.done);
        scanner.feed(b"NE]\n\n");
        assert!(scanner.done);
    }

    #[test]
    fn comments_and_empty_deltas_do_not_count_as_data() {
        let mut scanner = SseScanner::default();
        assert!(!scanner.feed(b": keep-alive\n\n"));
        // An empty-delta chunk is still a JSON event -> it is the first data
        // event we see (TTFT), even though it carries no content.
        assert!(scanner.feed(b"data: {\"id\":\"\",\"choices\":[{\"index\":0,\"delta\":{}}]}\n\n"));
        // A second data event is not "first" any more.
        assert!(!scanner.feed(
            b"data: {\"id\":\"\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning\":\"x\"}}]}\n\n"
        ));
    }

    #[test]
    fn multibyte_character_split_across_chunks() {
        let mut scanner = SseScanner::default();
        let event = "data: {\"choices\":[{\"delta\":{\"content\":\"€\"}}],\"usage\":null}\n\n";
        let bytes = event.as_bytes();
        // Split inside the 3-byte UTF-8 sequence of the euro sign.
        let cut = bytes
            .windows(3)
            .position(|w| w == "€".as_bytes())
            .expect("event contains the character");
        let (head, tail) = bytes.split_at(cut + 1);
        assert!(!scanner.feed(head));
        assert!(
            scanner.feed(tail),
            "a split UTF-8 char must not lose the event"
        );
    }

    #[test]
    fn usage_null_is_ignored() {
        let mut scanner = SseScanner::default();
        scanner.feed(b"data: {\"choices\":[],\"usage\":null}\n\n");
        assert!(scanner.usage.is_none());
    }

    #[test]
    fn error_event_is_native_chat_shape() {
        let event = error_event("boom");
        let text = String::from_utf8(event).unwrap();
        assert!(text.starts_with("data: "));
        assert!(text.ends_with("\n\n"));
        let payload: serde_json::Value =
            serde_json::from_str(text.trim_start_matches("data: ").trim()).unwrap();
        assert_eq!(payload["error"]["type"], "upstream_error");
        assert_eq!(payload["error"]["code"], "upstream_stream_error");
        assert_eq!(payload["error"]["message"], "boom");
    }

    const RESPONSES_DATA_ONLY: &str = concat!(
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n",
        "data: {\"type\":\"response.in_progress\",\"response\":{\"id\":\"resp_1\"}}\n\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":11,\"output_tokens\":22,\"input_tokens_details\":{\"cached_tokens\":3},\"output_tokens_details\":{\"reasoning_tokens\":5}}}}\n\n",
        "data: [DONE]\n\n",
    );

    fn patch_all(chunks: &[&[u8]], emit: bool) -> (Vec<u8>, ResponsesPatcher) {
        let mut patcher = ResponsesPatcher::new(emit);
        let mut out = Vec::new();
        for chunk in chunks {
            patcher.feed(chunk, &mut out);
        }
        patcher.finish(&mut out);
        (out, patcher)
    }

    #[test]
    fn responses_patcher_inserts_missing_event_lines() {
        let (out, patcher) = patch_all(&[RESPONSES_DATA_ONLY.as_bytes()], true);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("event: response.created\ndata: {\"type\":\"response.created\""));
        assert!(text.contains("event: response.in_progress\n"));
        assert!(text.contains("event: response.output_text.delta\n"));
        assert!(text.contains("event: response.completed\n"));
        // [DONE] has no `type` and must never gain an event line.
        assert!(text.ends_with("data: [DONE]\n\n"));
        assert!(!text.contains("event: [DONE]"));
        assert!(patcher.done);

        let usage = patcher.usage.expect("usage from response.completed");
        assert_eq!(usage.input_tokens, 11);
        assert_eq!(usage.output_tokens, 22);
        assert_eq!(usage.cached_tokens, 3);
        assert_eq!(usage.reasoning_tokens, 5);
    }

    #[test]
    fn responses_patcher_preserves_existing_event_lines() {
        let upstream = concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\"}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n",
            "data: [DONE]\n\n",
        );
        let (out, _) = patch_all(&[upstream.as_bytes()], true);
        let text = String::from_utf8(out).unwrap();
        // The existing event line is kept once; only the bare data event gains one.
        assert_eq!(text.matches("event: response.created").count(), 1);
        assert_eq!(text.matches("event: response.output_text.delta").count(), 1);
        assert_eq!(
            text,
            concat!(
                "event: response.created\n",
                "data: {\"type\":\"response.created\"}\n\n",
                "event: response.output_text.delta\n",
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n",
                "data: [DONE]\n\n",
            )
        );
    }

    #[test]
    fn responses_patcher_disabled_is_byte_identical() {
        let (out, patcher) = patch_all(&[RESPONSES_DATA_ONLY.as_bytes()], false);
        assert_eq!(out, RESPONSES_DATA_ONLY.as_bytes());
        assert!(patcher.done);
        assert!(patcher.usage.is_some());
    }

    #[test]
    fn responses_patcher_handles_lines_split_across_chunks() {
        let mut patcher = ResponsesPatcher::new(true);
        let mut out = Vec::new();
        let bytes = RESPONSES_DATA_ONLY.as_bytes();
        // Cut inside the first JSON payload and inside `[DONE]`.
        let mut first = 0;
        for cut in [17usize, 19, bytes.len() - 6] {
            let mut end = cut.min(bytes.len());
            while end > first && !RESPONSES_DATA_ONLY.is_char_boundary(end) {
                end -= 1;
            }
            patcher.feed(&bytes[first..end], &mut out);
            first = end;
        }
        patcher.feed(&bytes[first..], &mut out);
        patcher.finish(&mut out);
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("event: response.created\ndata: "));
        assert!(text.ends_with("data: [DONE]\n\n"));
        assert_eq!(text.matches("event: ").count(), 4);
        assert!(patcher.done);
    }

    #[test]
    fn responses_patcher_leaves_untyped_data_and_comments_alone() {
        let upstream = concat!(
            ": keep-alive\n\n",
            "data: {\"no_type\":true}\n\n",
            "data: not-json\n\n",
            "data: [DONE]\n\n",
        );
        let (out, _) = patch_all(&[upstream.as_bytes()], true);
        assert_eq!(String::from_utf8(out).unwrap(), upstream);
    }

    #[test]
    fn responses_observer_error_event_is_native_shape() {
        let observer = Observer::responses(true);
        let text = String::from_utf8(observer.error_event("boom")).unwrap();
        assert!(text.starts_with("event: error\ndata: "));
        let payload: serde_json::Value =
            serde_json::from_str(text.lines().nth(1).unwrap().trim_start_matches("data: "))
                .unwrap();
        // Flat, as the Responses wire protocol defines the `error` event.
        assert_eq!(payload["type"], "error");
        assert_eq!(payload["code"], "upstream_stream_error");
        assert_eq!(payload["message"], "boom");
        assert!(payload["param"].is_null());
        assert!(payload.get("error").is_none());
        assert!(
            payload.get("sequence_number").is_none(),
            "no upstream numbering"
        );
    }

    #[test]
    fn responses_observer_error_event_continues_upstream_numbering() {
        let mut observer = Observer::responses(true);
        let mut out = Vec::new();
        observer.transform(
            b"data: {\"type\":\"response.created\",\"sequence_number\":0}\n\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\",\"sequence_number\":4}\n\n",
            &mut out,
        );
        let text = String::from_utf8(observer.error_event("boom")).unwrap();
        let payload: serde_json::Value =
            serde_json::from_str(text.lines().nth(1).unwrap().trim_start_matches("data: "))
                .unwrap();
        assert_eq!(payload["sequence_number"], 5);
    }

    #[test]
    fn chat_observer_still_forwards_verbatim() {
        let mut observer = Observer::chat();
        let mut out = Vec::new();
        assert!(observer.transform(b"data: {\"choices\":[]}\n\n", &mut out));
        assert_eq!(out, b"data: {\"choices\":[]}\n\n");
        assert!(!observer.done());
        observer.finish(&mut out);
        assert_eq!(out.len(), 22);
    }

    const MESSAGES_SSE: &str = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"m\",\"usage\":{\"input_tokens\":21,\"output_tokens\":1,\"cache_read_input_tokens\":4,\"cache_creation\":{\"ephemeral_5m_input_tokens\":3}}}}\n\n",
        "event: ping\n",
        "data: {\"type\":\"ping\"}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"why\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"Bash\",\"input\":{}}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"cmd\\\":\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":17}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );

    #[test]
    fn messages_observer_forwards_verbatim_and_reads_usage() {
        let mut observer = Observer::messages();
        let mut out = Vec::new();
        // Split mid-event to prove the byte stream is untouched.
        let bytes = MESSAGES_SSE.as_bytes();
        let cut = MESSAGES_SSE.find("event: ping").expect("fixture has ping") + 5;
        assert!(observer.transform(&bytes[..cut], &mut out));
        assert!(!observer.transform(&bytes[cut..], &mut out));
        assert_eq!(out, bytes, "Messages SSE must be forwarded verbatim");
        assert!(observer.done());
        let usage = observer.usage().expect("usage from message_start/delta");
        assert_eq!(usage.input_tokens, 21);
        assert_eq!(usage.output_tokens, 17);
        assert_eq!(usage.cached_tokens, 4);
        assert_eq!(usage.cache_creation_tokens, 3);
    }

    #[test]
    fn messages_observer_reports_truncation_without_message_stop() {
        let mut observer = Observer::messages();
        let mut out = Vec::new();
        observer.transform(
            b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5}}}\n\n",
            &mut out,
        );
        assert!(!observer.done(), "no message_stop yet");
    }

    #[test]
    fn messages_observer_marks_upstream_error_terminal() {
        let mut observer = Observer::messages();
        let mut out = Vec::new();
        let error = b"event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n";
        assert!(observer.transform(error, &mut out));
        assert_eq!(out, error);
        assert!(observer.done(), "upstream error event ends the stream");
    }

    #[test]
    fn messages_keepalive_is_a_ping_event_and_error_is_anthropic_shaped() {
        let observer = Observer::messages();
        assert_eq!(
            observer.keepalive(),
            b"event: ping\ndata: {\"type\":\"ping\"}\n\n"
        );
        let text = String::from_utf8(observer.error_event("boom")).unwrap();
        assert!(text.starts_with("event: error\ndata: "));
        let payload: serde_json::Value =
            serde_json::from_str(text.lines().nth(1).unwrap().trim_start_matches("data: "))
                .unwrap();
        assert_eq!(payload["type"], "error");
        assert_eq!(payload["error"]["type"], "upstream_error");
        assert_eq!(payload["error"]["message"], "boom");
    }
}
