//! Usage extraction and the per-request JSONL ledger (spec 4.2).

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Local, TimeDelta, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Normalized usage across protocols.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
    pub reasoning_tokens: u64,
    pub cache_creation_tokens: u64,
    pub total_tokens: u64,
}

impl Usage {
    /// Extract from an OpenAI Chat Completions `usage` object.
    pub fn from_chat(json: &Value) -> Option<Self> {
        let usage = json.get("usage")?;
        if usage.is_null() {
            return None;
        }
        Self::from_chat_usage(usage)
    }

    fn from_chat_usage(usage: &Value) -> Option<Self> {
        let input = usage
            .get("prompt_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let output = usage
            .get("completion_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let cached = usage
            .pointer("/prompt_tokens_details/cached_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let reasoning = usage
            .pointer("/completion_tokens_details/reasoning_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let total = usage
            .get("total_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(input + output);
        if input == 0 && output == 0 && total == 0 {
            return None;
        }
        Some(Self {
            input_tokens: input,
            output_tokens: output,
            cached_tokens: cached,
            reasoning_tokens: reasoning,
            cache_creation_tokens: 0,
            total_tokens: total,
        })
    }

    /// Extract from an OpenAI Responses payload (spec 2.0).
    ///
    /// Accepts both shapes because the same fields appear in two places:
    /// - non-streaming: top-level `usage`;
    /// - streaming: `response.completed` → `response.usage`.
    pub fn from_responses(json: &Value) -> Option<Self> {
        let usage = json
            .get("response")
            .and_then(|response| response.get("usage"))
            .or_else(|| json.get("usage"))?;
        if usage.is_null() {
            return None;
        }
        let input = usage
            .get("input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let output = usage
            .get("output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let cached = usage
            .pointer("/input_tokens_details/cached_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let reasoning = usage
            .pointer("/output_tokens_details/reasoning_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let total = usage
            .get("total_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(input + output);
        if input == 0 && output == 0 && total == 0 {
            return None;
        }
        Some(Self {
            input_tokens: input,
            output_tokens: output,
            cached_tokens: cached,
            reasoning_tokens: reasoning,
            cache_creation_tokens: 0,
            total_tokens: total,
        })
    }
    /// Extract from an Anthropic Messages payload: the top-level `usage`
    /// object of a non-streaming message (spec 2.3 / 2.0).
    pub fn from_messages(json: &Value) -> Option<Self> {
        MessagesUsageFields::parse(json.get("usage")?).to_usage()
    }

    /// Anthropic `count_tokens` answers `{"input_tokens": N}` — a real count,
    /// so it is recorded as prompt usage instead of `usage_missing`.
    pub fn from_count_tokens(json: &Value) -> Option<Self> {
        let input = json.get("input_tokens").and_then(Value::as_u64)?;
        Some(Self {
            input_tokens: input,
            total_tokens: input,
            ..Self::default()
        })
    }
}

/// Anthropic Messages `usage` fields, kept optional so a streaming
/// `message_start` (input/cache) and `message_delta` (output) can be merged
/// without a missing field clobbering an observed one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MessagesUsageFields {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
    pub cache_creation_tokens: Option<u64>,
}

impl MessagesUsageFields {
    pub fn parse(usage: &Value) -> Self {
        if !usage.is_object() {
            return Self::default();
        }
        let num = |key: &str| usage.get(key).and_then(Value::as_u64);
        // Cache creation has two shapes: DS flattens it, OD nests it under
        // `cache_creation` with a 5m/1h split (spec 2.0).
        let cache_creation_tokens = num("cache_creation_input_tokens").or_else(|| {
            usage
                .get("cache_creation")
                .filter(|v| v.is_object())
                .and_then(|cc| {
                    let five = cc.get("ephemeral_5m_input_tokens").and_then(Value::as_u64);
                    let hour = cc.get("ephemeral_1h_input_tokens").and_then(Value::as_u64);
                    (five.is_some() || hour.is_some())
                        .then(|| five.unwrap_or(0) + hour.unwrap_or(0))
                })
        });
        Self {
            input_tokens: num("input_tokens"),
            output_tokens: num("output_tokens"),
            cached_tokens: num("cache_read_input_tokens"),
            cache_creation_tokens,
        }
    }

    pub fn merge(&mut self, other: Self) {
        if other.input_tokens.is_some() {
            self.input_tokens = other.input_tokens;
        }
        if other.output_tokens.is_some() {
            self.output_tokens = other.output_tokens;
        }
        if other.cached_tokens.is_some() {
            self.cached_tokens = other.cached_tokens;
        }
        if other.cache_creation_tokens.is_some() {
            self.cache_creation_tokens = other.cache_creation_tokens;
        }
    }

    pub fn to_usage(self) -> Option<Usage> {
        let input = self.input_tokens.unwrap_or(0);
        let output = self.output_tokens.unwrap_or(0);
        let cached = self.cached_tokens.unwrap_or(0);
        let cache_creation = self.cache_creation_tokens.unwrap_or(0);
        if input == 0 && output == 0 && cached == 0 && cache_creation == 0 {
            return None;
        }
        Some(Usage {
            input_tokens: input,
            output_tokens: output,
            cached_tokens: cached,
            reasoning_tokens: 0,
            cache_creation_tokens: cache_creation,
            total_tokens: input + output,
        })
    }
}

/// One line per request in `state_dir/logs/YYYY-MM-DD.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestRecord {
    pub ts: String,
    pub request_id: String,
    pub client: String,
    pub protocol: String,
    pub requested_model: String,
    pub provider: String,
    pub upstream_model: String,
    pub stream: bool,
    pub status: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_status: Option<u16>,
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub cached_tokens: u64,
    #[serde(default)]
    pub reasoning_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttft_ms: Option<u64>,
    #[serde(default)]
    pub duration_ms: u64,
    #[serde(default)]
    pub bytes_in: u64,
    #[serde(default)]
    pub bytes_out: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_request_id: Option<String>,
    /// Upstream error body (truncated) when the client got a UMSP envelope
    /// instead of the raw response. Never client-visible; diagnosis only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_body: Option<String>,
    #[serde(default)]
    pub client_disconnected: bool,
    /// Backend switches performed by this request (0 = first backend served).
    /// The row's `provider`/`upstream_model` are the ones that actually served it.
    #[serde(default)]
    pub fallbacks: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub quirks: Vec<String>,
}

impl RequestRecord {
    pub fn total_tokens(&self) -> u64 {
        self.prompt_tokens + self.completion_tokens
    }
}

#[derive(Debug, Clone)]
pub struct LogStore {
    dir: PathBuf,
    /// Serializes appends across clones of one store. `writeln!` on a `File`
    /// issues several `write` syscalls (body, then `\n`), so concurrent
    /// requests used to merge two records into one line and leave an empty
    /// line behind — load testing at 50 concurrent requests lost ~2% of rows.
    write_lock: Arc<Mutex<()>>,
}

impl LogStore {
    pub fn new(state_dir: &Path) -> Self {
        Self {
            dir: state_dir.join("logs"),
            write_lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn file_for(&self, when: DateTime<Local>) -> PathBuf {
        self.dir.join(format!("{}.jsonl", when.format("%Y-%m-%d")))
    }

    pub fn append(&self, record: &RequestRecord) -> std::io::Result<()> {
        let line = serde_json::to_string(record).map_err(std::io::Error::other)?;
        let mut buf = Vec::with_capacity(line.len() + 1);
        buf.extend_from_slice(line.as_bytes());
        buf.push(b'\n');
        // One write per record, under the shared lock: a whole line or nothing.
        let _guard = self
            .write_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::fs::create_dir_all(&self.dir)?;
        let path = self.file_for(Local::now());
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        file.write_all(&buf)?;
        Ok(())
    }

    /// All JSONL files, oldest first.
    pub fn files(&self) -> Vec<PathBuf> {
        let Ok(read) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut files: Vec<PathBuf> = read
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "jsonl"))
            .collect();
        files.sort();
        files
    }

    /// Files that could contain records since `since` (one day of slack for timezones).
    pub fn files_since(&self, since: DateTime<Utc>) -> Vec<PathBuf> {
        let cutoff = (since.with_timezone(&Local) - TimeDelta::days(1))
            .format("%Y-%m-%d")
            .to_string();
        self.files()
            .into_iter()
            .filter(|p| {
                p.file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .is_some_and(|stem| stem >= cutoff)
            })
            .collect()
    }

    pub fn read_since(&self, since: DateTime<Utc>) -> Vec<RequestRecord> {
        let mut out = Vec::new();
        for path in self.files_since(since) {
            let Ok(raw) = std::fs::read_to_string(&path) else {
                continue;
            };
            for line in raw.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                let Ok(record) = serde_json::from_str::<RequestRecord>(line) else {
                    continue;
                };
                let Ok(ts) = DateTime::parse_from_rfc3339(&record.ts) else {
                    continue;
                };
                if ts.with_timezone(&Utc) >= since {
                    out.push(record);
                }
            }
        }
        out.sort_by(|a, b| a.ts.cmp(&b.ts));
        out
    }

    /// Delete files older than the cutoff date (YYYY-MM-DD stems).
    pub fn prune_before(&self, cutoff: DateTime<Utc>) -> std::io::Result<Vec<PathBuf>> {
        let cutoff_stem = cutoff.with_timezone(&Local).format("%Y-%m-%d").to_string();
        let mut removed = Vec::new();
        for path in self.files() {
            let stem = path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            if stem < cutoff_stem {
                std::fs::remove_file(&path)?;
                removed.push(path);
            }
        }
        Ok(removed)
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Aggregate {
    pub requests: u64,
    pub ok: u64,
    pub errors: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cached_tokens: u64,
    pub reasoning_tokens: u64,
    pub total_tokens: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub duration_ms_total: u64,
    pub by_status: BTreeMap<String, u64>,
    pub by_protocol: BTreeMap<String, u64>,
    pub by_client: BTreeMap<String, u64>,
    pub by_model: BTreeMap<String, u64>,
    pub by_provider: BTreeMap<String, u64>,
}

impl Aggregate {
    pub fn avg_duration_ms(&self) -> f64 {
        if self.requests == 0 {
            0.0
        } else {
            self.duration_ms_total as f64 / self.requests as f64
        }
    }
}

pub fn aggregate(records: &[RequestRecord]) -> Aggregate {
    let mut agg = Aggregate::default();
    for r in records {
        agg.requests += 1;
        if r.status < 400 {
            agg.ok += 1;
        } else {
            agg.errors += 1;
        }
        agg.prompt_tokens += r.prompt_tokens;
        agg.completion_tokens += r.completion_tokens;
        agg.cached_tokens += r.cached_tokens;
        agg.reasoning_tokens += r.reasoning_tokens;
        agg.total_tokens += r.total_tokens();
        agg.bytes_in += r.bytes_in;
        agg.bytes_out += r.bytes_out;
        agg.duration_ms_total += r.duration_ms;
        *agg.by_status.entry(r.status.to_string()).or_default() += 1;
        *agg.by_protocol.entry(r.protocol.clone()).or_default() += 1;
        *agg.by_client.entry(r.client.clone()).or_default() += 1;
        *agg.by_model.entry(r.requested_model.clone()).or_default() += 1;
        *agg.by_provider.entry(r.provider.clone()).or_default() += 1;
    }
    agg
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extracts_chat_usage() {
        let value = json!({
            "usage": {
                "prompt_tokens": 12,
                "completion_tokens": 34,
                "total_tokens": 46,
                "prompt_tokens_details": {"cached_tokens": 5},
                "completion_tokens_details": {"reasoning_tokens": 7}
            }
        });
        let usage = Usage::from_chat(&value).unwrap();
        assert_eq!(usage.input_tokens, 12);
        assert_eq!(usage.output_tokens, 34);
        assert_eq!(usage.cached_tokens, 5);
        assert_eq!(usage.reasoning_tokens, 7);
        assert_eq!(usage.total_tokens, 46);
    }

    #[test]
    fn missing_usage_returns_none() {
        assert!(Usage::from_chat(&json!({"choices": []})).is_none());
        assert!(Usage::from_chat(&json!({"usage": null})).is_none());
        assert!(Usage::from_chat(&json!({"usage": {}})).is_none());
    }

    #[test]
    fn extracts_responses_usage_from_body_and_event() {
        let usage = json!({
            "input_tokens": 12,
            "output_tokens": 34,
            "total_tokens": 46,
            "input_tokens_details": {"cached_tokens": 5},
            "output_tokens_details": {"reasoning_tokens": 7}
        });
        let body = json!({"id": "resp_1", "object": "response", "usage": usage.clone()});
        let from_body = Usage::from_responses(&body).unwrap();
        assert_eq!(from_body.input_tokens, 12);
        assert_eq!(from_body.output_tokens, 34);
        assert_eq!(from_body.cached_tokens, 5);
        assert_eq!(from_body.reasoning_tokens, 7);
        assert_eq!(from_body.total_tokens, 46);

        let event =
            json!({"type": "response.completed", "response": {"id": "resp_1", "usage": usage}});
        assert_eq!(Usage::from_responses(&event), Some(from_body));
    }

    #[test]
    fn responses_usage_missing_is_none_not_zero() {
        assert!(Usage::from_responses(&json!({"id": "resp_1"})).is_none());
        assert!(
            Usage::from_responses(&json!({"type": "response.completed", "response": {}})).is_none()
        );
        assert!(Usage::from_responses(&json!({"usage": null})).is_none());
        assert!(Usage::from_responses(&json!({"usage": {}})).is_none());
    }

    #[test]
    fn total_defaults_to_sum() {
        let usage =
            Usage::from_chat(&json!({"usage": {"prompt_tokens": 3, "completion_tokens": 4}}))
                .unwrap();
        assert_eq!(usage.total_tokens, 7);
    }

    #[test]
    fn extracts_messages_usage_with_nested_cache_creation() {
        let value = json!({
            "id": "msg_1",
            "type": "message",
            "usage": {
                "input_tokens": 12,
                "output_tokens": 34,
                "cache_read_input_tokens": 5,
                "cache_creation": {
                    "ephemeral_5m_input_tokens": 7,
                    "ephemeral_1h_input_tokens": 9
                }
            }
        });
        let usage = Usage::from_messages(&value).unwrap();
        assert_eq!(usage.input_tokens, 12);
        assert_eq!(usage.output_tokens, 34);
        assert_eq!(usage.cached_tokens, 5);
        assert_eq!(usage.cache_creation_tokens, 16);
        assert_eq!(usage.total_tokens, 46);
    }

    #[test]
    fn extracts_messages_usage_with_flat_cache_creation() {
        let usage = Usage::from_messages(&json!({
            "usage": {
                "input_tokens": 3,
                "output_tokens": 4,
                "cache_creation_input_tokens": 11,
                "cache_read_input_tokens": 2
            }
        }))
        .unwrap();
        assert_eq!(usage.cache_creation_tokens, 11);
        assert_eq!(usage.cached_tokens, 2);
    }

    #[test]
    fn messages_usage_missing_is_none_not_zero() {
        assert!(Usage::from_messages(&json!({"id": "msg_1"})).is_none());
        assert!(Usage::from_messages(&json!({"usage": null})).is_none());
        assert!(Usage::from_messages(&json!({"usage": {}})).is_none());
    }

    #[test]
    fn messages_usage_fields_merge_start_and_delta() {
        let start = MessagesUsageFields::parse(&json!({
            "input_tokens": 21,
            "cache_read_input_tokens": 4,
            "cache_creation": {"ephemeral_5m_input_tokens": 3}
        }));
        let delta = MessagesUsageFields::parse(&json!({"output_tokens": 17}));
        let mut merged = start;
        merged.merge(delta);
        let usage = merged.to_usage().unwrap();
        assert_eq!(usage.input_tokens, 21);
        assert_eq!(usage.output_tokens, 17);
        assert_eq!(usage.cached_tokens, 4);
        assert_eq!(usage.cache_creation_tokens, 3);
    }

    #[test]
    fn messages_usage_fields_merge_flat_cache_creation() {
        // DeepSeek's Anthropic-compatible stream uses the flat field.
        let start = MessagesUsageFields::parse(&json!({
            "input_tokens": 21,
            "cache_read_input_tokens": 4,
            "cache_creation_input_tokens": 7
        }));
        let delta = MessagesUsageFields::parse(&json!({"output_tokens": 17}));
        let mut merged = start;
        merged.merge(delta);
        let usage = merged.to_usage().unwrap();
        assert_eq!(usage.input_tokens, 21);
        assert_eq!(usage.output_tokens, 17);
        assert_eq!(usage.cached_tokens, 4);
        assert_eq!(usage.cache_creation_tokens, 7);
    }

    #[test]
    fn extracts_count_tokens_input() {
        let usage = Usage::from_count_tokens(&json!({"input_tokens": 31})).unwrap();
        assert_eq!(usage.input_tokens, 31);
        assert_eq!(usage.output_tokens, 0);
        assert_eq!(usage.total_tokens, 31);
        assert!(Usage::from_count_tokens(&json!({"error": "nope"})).is_none());
        assert!(Usage::from_count_tokens(&json!({})).is_none());
    }

    fn record(status: u16, tokens: (u64, u64)) -> RequestRecord {
        RequestRecord {
            ts: (chrono::Utc::now() - TimeDelta::minutes(5))
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            request_id: "x".into(),
            client: "c".into(),
            protocol: "chat".into(),
            requested_model: "od/m".into(),
            provider: "od".into(),
            upstream_model: "m".into(),
            stream: false,
            status,
            upstream_status: Some(status),
            upstream_body: None,
            prompt_tokens: tokens.0,
            completion_tokens: tokens.1,
            cached_tokens: 0,
            reasoning_tokens: 0,
            ttft_ms: None,
            duration_ms: 100,
            bytes_in: 10,
            bytes_out: 20,
            error_code: None,
            upstream_request_id: None,
            client_disconnected: false,
            fallbacks: 0,
            quirks: vec![],
        }
    }

    #[test]
    fn aggregates_tokens_and_counts() {
        let agg = aggregate(&[record(200, (10, 5)), record(500, (1, 0))]);
        assert_eq!(agg.requests, 2);
        assert_eq!(agg.ok, 1);
        assert_eq!(agg.errors, 1);
        assert_eq!(agg.prompt_tokens, 11);
        assert_eq!(agg.total_tokens, 16);
        assert_eq!(agg.by_status.get("200"), Some(&1));
        assert_eq!(agg.avg_duration_ms(), 100.0);
    }

    #[test]
    fn log_store_roundtrip() {
        let dir = std::env::temp_dir().join(format!("umsp-log-{}", std::process::id()));
        let store = LogStore::new(&dir);
        let rec = record(200, (1, 2));
        store.append(&rec).unwrap();
        // Force the file name to today's local date.
        let since = Utc::now() - TimeDelta::hours(1);
        let back = store.read_since(since);
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].request_id, "x");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn concurrent_appends_never_merge_or_drop_lines() {
        // 50 threads × 40 rows reproduced the original bug: `writeln!` issued
        // separate body and newline writes, so two records could land on one
        // line (unparsable) plus an empty line, losing a row.
        let dir = std::env::temp_dir().join(format!(
            "umsp-log-race-{}-{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let store = LogStore::new(&dir);
        let threads = 50;
        let per_thread = 40;
        let mut handles = Vec::new();
        for _ in 0..threads {
            let store = store.clone();
            handles.push(std::thread::spawn(move || {
                for _ in 0..per_thread {
                    store.append(&record(200, (1, 2))).unwrap();
                }
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
        let since = Utc::now() - TimeDelta::hours(1);
        assert_eq!(store.read_since(since).len(), threads * per_thread);
        let files = store.files();
        assert_eq!(files.len(), 1);
        let raw = std::fs::read_to_string(&files[0]).unwrap();
        assert_eq!(raw.lines().count(), threads * per_thread);
        assert!(
            raw.lines().all(|line| !line.is_empty()),
            "empty line from a split newline"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
