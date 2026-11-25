//! Byte-level Server-Sent Events (SSE) framing shared by native stream
//! observers and the cross-protocol `StreamBridge`.
//!
//! Framing stays byte-oriented until a full event is assembled so multi-byte
//! UTF-8 sequences split across TCP chunks are never corrupted. Line endings
//! follow the WHATWG SSE specification: CRLF (`\r\n`), LF (`\n`), and CR
//! (`\r`) are all recognized, and a single optional leading ASCII space after
//! `:` is stripped from field values.

/// Shared default upper bound for a single buffered SSE event or patched line
/// (8 MiB). Large enough for Responses `output_item.done` payloads carrying
/// encrypted reasoning or base64 data while still bounding memory.
pub const MAX_EVENT_BYTES: usize = 8 * 1024 * 1024;

/// Upper bound for side-observer partial lines that only extract metadata
/// (TTFT / usage / terminal marker).
pub const MAX_OBSERVER_LINE_BYTES: usize = 64 * 1024;

/// One framed slice produced by [`LineFramer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinePiece<'a> {
    /// A complete line (including its original EOL bytes) within `max_line`.
    Line(&'a [u8]),
    /// Raw bytes belonging to a line that exceeded `max_line` (delivered up
    /// through its terminating EOL, after which normal line framing resumes).
    Overflow(&'a [u8]),
    /// Trailing bytes buffered at EOF with no terminating EOL.
    Partial(&'a [u8]),
}

/// Incremental byte-level SSE line framer.
///
/// Tracks a scan offset so lines arriving in small chunks are scanned once
/// (O(n) total) instead of rescanning the carry buffer on every chunk.
#[derive(Debug)]
pub struct LineFramer {
    buf: Vec<u8>,
    read: usize,
    scan: usize,
    max_line: usize,
    overflow: bool,
    overflow_pending_cr: bool,
}

impl LineFramer {
    pub fn new(max_line: usize) -> Self {
        Self {
            buf: Vec::new(),
            read: 0,
            scan: 0,
            max_line,
            overflow: false,
            overflow_pending_cr: false,
        }
    }

    /// Feed raw bytes and emit framed [`LinePiece`]s to `on_piece`.
    pub fn push(&mut self, chunk: &[u8], mut on_piece: impl FnMut(LinePiece<'_>)) {
        let mut rest = chunk;
        if rest.is_empty() {
            return;
        }

        if self.overflow {
            if self.overflow_pending_cr {
                self.overflow_pending_cr = false;
                if rest[0] == b'\n' {
                    on_piece(LinePiece::Overflow(&rest[..1]));
                    rest = &rest[1..];
                }
                self.overflow = false;
            }
            if self.overflow {
                match rest.iter().position(|&b| b == b'\n' || b == b'\r') {
                    None => {
                        on_piece(LinePiece::Overflow(rest));
                        return;
                    }
                    Some(pos) => {
                        if rest[pos] == b'\n' {
                            on_piece(LinePiece::Overflow(&rest[..=pos]));
                            rest = &rest[pos + 1..];
                            self.overflow = false;
                        } else if pos + 1 < rest.len() {
                            let end = if rest[pos + 1] == b'\n' {
                                pos + 2
                            } else {
                                pos + 1
                            };
                            on_piece(LinePiece::Overflow(&rest[..end]));
                            rest = &rest[end..];
                            self.overflow = false;
                        } else {
                            // Overflow line ended on `\r`; consume a following
                            // `\n` if the next chunk starts with one.
                            on_piece(LinePiece::Overflow(&rest[..=pos]));
                            self.overflow_pending_cr = true;
                            return;
                        }
                    }
                }
            }
        }

        if rest.is_empty() {
            return;
        }

        self.buf.extend_from_slice(rest);
        while self.scan < self.buf.len() {
            let b = self.buf[self.scan];
            if b == b'\n' {
                let end = self.scan + 1;
                let slice = &self.buf[self.read..end];
                if slice.len() > self.max_line {
                    on_piece(LinePiece::Overflow(slice));
                } else {
                    on_piece(LinePiece::Line(slice));
                }
                self.read = end;
                self.scan = end;
            } else if b == b'\r' {
                if self.scan + 1 < self.buf.len() {
                    let end = if self.buf[self.scan + 1] == b'\n' {
                        self.scan + 2
                    } else {
                        self.scan + 1
                    };
                    let slice = &self.buf[self.read..end];
                    if slice.len() > self.max_line {
                        on_piece(LinePiece::Overflow(slice));
                    } else {
                        on_piece(LinePiece::Line(slice));
                    }
                    self.read = end;
                    self.scan = end;
                } else {
                    // Hold a trailing `\r` until the next byte arrives so a
                    // CRLF split across chunks is not emitted as two lines.
                    break;
                }
            } else {
                self.scan += 1;
            }
        }

        let pending = self.buf.len() - self.read;
        if pending > self.max_line {
            let trailing_cr = self.buf.last() == Some(&b'\r');
            on_piece(LinePiece::Overflow(&self.buf[self.read..]));
            self.buf.clear();
            self.read = 0;
            self.scan = 0;
            self.overflow = true;
            self.overflow_pending_cr = trailing_cr;
        } else if self.read > 0 {
            if self.read == self.buf.len() {
                self.buf.clear();
                self.read = 0;
                self.scan = 0;
            } else if self.read >= 4096 || self.read * 2 >= self.buf.len() {
                self.buf.copy_within(self.read.., 0);
                let remaining = self.buf.len() - self.read;
                self.buf.truncate(remaining);
                self.scan -= self.read;
                self.read = 0;
            }
        }
    }

    /// Flush any buffered bytes at EOF.
    ///
    /// A buffered line ending in a held `\r` is delivered as [`LinePiece::Line`];
    /// any other trailing bytes without an EOL are delivered as [`LinePiece::Partial`].
    pub fn finish(&mut self, mut on_piece: impl FnMut(LinePiece<'_>)) {
        if self.overflow {
            self.overflow = false;
            self.overflow_pending_cr = false;
            return;
        }
        if self.read < self.buf.len() {
            let tail = &self.buf[self.read..];
            if tail.ends_with(b"\r") {
                on_piece(LinePiece::Line(tail));
            } else {
                on_piece(LinePiece::Partial(tail));
            }
            self.buf.clear();
            self.read = 0;
            self.scan = 0;
        }
    }

    pub fn clear(&mut self) {
        self.buf.clear();
        self.read = 0;
        self.scan = 0;
        self.overflow = false;
        self.overflow_pending_cr = false;
    }
}

/// Strip one trailing SSE line ending (`\r\n`, `\n`, or `\r`).
pub fn trim_eol(line: &[u8]) -> &[u8] {
    if let Some(rest) = line.strip_suffix(b"\r\n") {
        rest
    } else if let Some(rest) = line.strip_suffix(b"\n") {
        rest
    } else if let Some(rest) = line.strip_suffix(b"\r") {
        rest
    } else {
        line
    }
}

/// Parse one SSE line (with EOL already removed) into `(field, value)`.
///
/// Comment lines (leading `:`) and empty lines return `None`. Per the WHATWG
/// SSE spec, at most one leading ASCII space (`0x20`) after `:` is removed.
pub fn parse_field(line: &[u8]) -> Option<(&[u8], &[u8])> {
    if line.is_empty() || line[0] == b':' {
        return None;
    }
    match line.iter().position(|&b| b == b':') {
        Some(colon) => {
            let field = &line[..colon];
            let raw_val = &line[colon + 1..];
            let val = raw_val.strip_prefix(b" ").unwrap_or(raw_val);
            Some((field, val))
        }
        None => Some((line, b"")),
    }
}

/// One dispatched SSE event (assembled from one or more `data:` lines).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub name: Option<String>,
    pub data: String,
}

/// Returned when a single SSE event exceeds the configured byte limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OversizedEvent;

#[derive(Debug, Default)]
struct EventAcc {
    name: Option<String>,
    data: Vec<u8>,
    has_data: bool,
    event_bytes: usize,
    oversized: bool,
}

impl EventAcc {
    fn feed_line(&mut self, line: &[u8], max_event: usize, on_event: &mut impl FnMut(SseEvent)) {
        if self.oversized {
            return;
        }
        let content = trim_eol(line);
        if content.is_empty() {
            self.dispatch(on_event);
            return;
        }
        self.event_bytes += line.len();
        if self.event_bytes > max_event {
            self.oversized = true;
            return;
        }
        let Some((field, value)) = parse_field(content) else {
            return;
        };
        match field {
            b"data" => {
                if self.has_data {
                    self.data.push(b'\n');
                }
                self.data.extend_from_slice(value);
                self.has_data = true;
            }
            b"event" => {
                self.name = Some(String::from_utf8_lossy(value).trim_end().to_string());
            }
            _ => {}
        }
    }

    fn dispatch(&mut self, on_event: &mut impl FnMut(SseEvent)) {
        if self.has_data {
            let event = SseEvent {
                name: self.name.take(),
                data: bytes_to_string(std::mem::take(&mut self.data)),
            };
            self.has_data = false;
            self.event_bytes = 0;
            on_event(event);
        } else {
            self.name = None;
            self.event_bytes = 0;
        }
    }

    fn reset(&mut self) {
        self.name = None;
        self.data.clear();
        self.has_data = false;
        self.event_bytes = 0;
    }
}

/// Incremental event-level SSE framer built on [`LineFramer`].
#[derive(Debug)]
pub struct EventFramer {
    lines: LineFramer,
    max_event: usize,
    acc: EventAcc,
}

impl EventFramer {
    pub fn new(max_event: usize) -> Self {
        Self {
            lines: LineFramer::new(max_event),
            max_event,
            acc: EventAcc::default(),
        }
    }

    /// Feed a raw byte chunk, invoking `on_event` for each complete SSE event.
    pub fn push(
        &mut self,
        chunk: &[u8],
        mut on_event: impl FnMut(SseEvent),
    ) -> Result<(), OversizedEvent> {
        if self.acc.oversized {
            return Err(OversizedEvent);
        }
        let max_event = self.max_event;
        let acc = &mut self.acc;
        self.lines.push(chunk, |piece| match piece {
            LinePiece::Line(line) => acc.feed_line(line, max_event, &mut on_event),
            LinePiece::Overflow(_) => acc.oversized = true,
            LinePiece::Partial(_) => {}
        });
        if self.acc.oversized {
            self.lines.clear();
            self.acc.reset();
            return Err(OversizedEvent);
        }
        Ok(())
    }

    /// Flush a trailing line and emit any accumulated event at EOF (even when
    /// the upstream omitted the final blank line).
    pub fn finish(&mut self, mut on_event: impl FnMut(SseEvent)) -> Result<(), OversizedEvent> {
        if self.acc.oversized {
            return Err(OversizedEvent);
        }
        let max_event = self.max_event;
        let acc = &mut self.acc;
        self.lines.finish(|piece| match piece {
            LinePiece::Line(line) => acc.feed_line(line, max_event, &mut on_event),
            LinePiece::Partial(partial)
                if !partial.iter().all(|byte| byte.is_ascii_whitespace()) =>
            {
                acc.feed_line(partial, max_event, &mut on_event);
            }
            LinePiece::Partial(_) | LinePiece::Overflow(_) => {}
        });
        if self.acc.oversized {
            self.lines.clear();
            self.acc.reset();
            return Err(OversizedEvent);
        }
        self.acc.dispatch(&mut on_event);
        Ok(())
    }
}

fn bytes_to_string(bytes: Vec<u8>) -> String {
    match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(err) => String::from_utf8_lossy(err.as_bytes()).into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_framer_handles_lf_crlf_and_cr_across_chunk_splits() {
        let input = b"data: a\ndata: b\r\ndata: c\rdata: d\r\r";
        for chunk_size in [1, 2, 3, 5, 11, input.len()] {
            let mut framer = LineFramer::new(1024);
            let mut lines: Vec<Vec<u8>> = Vec::new();
            for chunk in input.chunks(chunk_size) {
                framer.push(chunk, |piece| match piece {
                    LinePiece::Line(line) => lines.push(line.to_vec()),
                    other => panic!("unexpected {other:?}"),
                });
            }
            framer.finish(|piece| match piece {
                LinePiece::Line(line) => lines.push(line.to_vec()),
                other => panic!("unexpected {other:?}"),
            });
            assert_eq!(
                lines,
                vec![
                    b"data: a\n".to_vec(),
                    b"data: b\r\n".to_vec(),
                    b"data: c\r".to_vec(),
                    b"data: d\r".to_vec(),
                    b"\r".to_vec(),
                ],
                "chunk_size={chunk_size}"
            );
        }
    }

    #[test]
    fn line_framer_overflow_passes_through_until_eol_then_resumes() {
        let mut framer = LineFramer::new(8);
        let mut lines = Vec::new();
        let mut overflow = Vec::new();
        for chunk in [&b"0123456789"[..], &b"abc\r"[..], &b"\nok\n"[..]] {
            framer.push(chunk, |piece| match piece {
                LinePiece::Line(line) => lines.push(line.to_vec()),
                LinePiece::Overflow(bytes) => overflow.extend_from_slice(bytes),
                LinePiece::Partial(_) => {}
            });
        }
        assert_eq!(overflow, b"0123456789abc\r\n");
        assert_eq!(lines, vec![b"ok\n".to_vec()]);
    }

    #[test]
    fn parse_field_strips_at_most_one_leading_space() {
        assert_eq!(
            parse_field(b"data:hello"),
            Some((&b"data"[..], &b"hello"[..]))
        );
        assert_eq!(
            parse_field(b"data: hello"),
            Some((&b"data"[..], &b"hello"[..]))
        );
        assert_eq!(
            parse_field(b"data:  two"),
            Some((&b"data"[..], &b" two"[..]))
        );
        assert_eq!(parse_field(b": comment"), None);
        assert_eq!(parse_field(b""), None);
    }

    #[test]
    fn event_framer_assembles_multiline_cr_crlf_lf_and_utf8_splits() {
        let raw = "event: custom\rdata:  lead\rdata: line2\r\r: ping\n\ndata: €\n\n";
        for chunk_size in [1, 2, 4, raw.len()] {
            let mut framer = EventFramer::new(4096);
            let mut events = Vec::new();
            for chunk in raw.as_bytes().chunks(chunk_size) {
                framer.push(chunk, |ev| events.push(ev)).unwrap();
            }
            framer.finish(|ev| events.push(ev)).unwrap();
            assert_eq!(
                events,
                vec![
                    SseEvent {
                        name: Some("custom".into()),
                        data: " lead\nline2".into(),
                    },
                    SseEvent {
                        name: None,
                        data: "€".into(),
                    },
                ],
                "chunk_size={chunk_size}"
            );
        }
    }

    #[test]
    fn event_framer_rejects_oversized_single_line_and_multiline_events() {
        let mut framer = EventFramer::new(16);
        assert_eq!(framer.push(&[b'x'; 17], |_| {}), Err(OversizedEvent));

        let mut framer = EventFramer::new(16);
        assert_eq!(
            framer.push(b"data: 12345678\ndata: 12345678\n\n", |_| {}),
            Err(OversizedEvent)
        );
    }
}
