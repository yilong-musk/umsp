//! End-to-end tests over the real router with a fake upstream.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};

use umsp::config::{Backend, Client, Config, Model, PROTOCOLS, Provider, ServerConfig};
use umsp::envfile::EnvFile;
use umsp::keys;
use umsp::server::{AppState, build_router};
use umsp::usage::{LogStore, RequestRecord};

pub(crate) const TEST_KEY: &str = "umsp_testkey_0123456789";

/// Deterministic per-test directory under the scratch TMPDIR. A fresh path was
/// leaking one directory per test per run (576 after a day of iteration); a
/// stable name means the next run reuses it after wiping the old state.
pub(crate) fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("umsp-it-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

pub(crate) fn env_file(dir: &Path) -> EnvFile {
    let path = dir.join("env");
    std::fs::write(
        &path,
        // Both upstreams' keys live in one env file; each provider names its own.
        "TEST_OD_KEY=sk-test\nTEST_DS_KEY=sk-ds-test\n",
    )
    .unwrap();
    EnvFile::load(&path)
}

pub(crate) fn config(dir: &Path, upstream_url: String, max_body: usize) -> Config {
    Config {
        server: ServerConfig {
            listen: "127.0.0.1:0".into(),
            max_body_bytes: max_body,
            request_timeout_s: 30,
            stream_idle_timeout_s: 5,
            stream_header_timeout_s: 10,
            stream_keepalive_s: 1,
            state_dir: dir.display().to_string(),
            responses_emit_event_lines: true,
            log_retention_days: 30,
        },
        providers: vec![Provider {
            id: "od".into(),
            vendor: Some("open-design".into()),
            base_url: upstream_url,
            anthropic_base_url: None,
            api_key_env: "TEST_OD_KEY".into(),
            auth: "api_key".into(),
            protocols: PROTOCOLS.iter().map(|s| s.to_string()).collect(),
            quirks: Some("opendesign".into()),
        }],
        models: vec![Model {
            id: "upstream-model".into(),
            backends: vec![Backend {
                provider: "od".into(),
                upstream: "upstream-model".into(),
            }],
            aliases: vec!["short".into()],
            context_length: 1048000,
            context_lengths: Vec::new(),
            max_completion_tokens: 128000,
            capabilities: vec!["text".into(), "tools".into(), "reasoning".into()],
            display_name: Some("Test Model".into()),
            created: 1_789_026_056,
            description: None,
        }],
        clients: vec![Client {
            name: "tester".into(),
            key_hash: keys::hash_key(TEST_KEY),
            key_prefix: keys::key_prefix(TEST_KEY),
            allowed_models: vec!["*".into()],
            allowed_protocols: PROTOCOLS.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }],
    }
}

// ---------------------------------------------------------------------------
// Fake upstream
// ---------------------------------------------------------------------------

/// What the fake upstream saw on the wire (last request body + headers).
#[derive(Clone, Default)]
pub(crate) struct Recorder {
    pub(crate) seen: Arc<Mutex<Vec<Value>>>,
    pub(crate) headers: Arc<Mutex<Vec<axum::http::HeaderMap>>>,
    pub(crate) uris: Arc<Mutex<Vec<String>>>,
}

impl Recorder {
    pub(crate) fn last(&self) -> Value {
        self.seen
            .lock()
            .unwrap()
            .last()
            .cloned()
            .unwrap_or(Value::Null)
    }

    pub(crate) fn last_headers(&self) -> axum::http::HeaderMap {
        self.headers
            .lock()
            .unwrap()
            .last()
            .cloned()
            .unwrap_or_default()
    }

    pub(crate) fn last_uri(&self) -> String {
        self.uris
            .lock()
            .unwrap()
            .last()
            .cloned()
            .unwrap_or_default()
    }
}

/// A realistic OD stream: comment, empty delta, reasoning, tool delta, empty
/// `id`, final usage chunk, `[DONE]`.
pub(crate) const SSE_NORMAL: &str = concat!(
    ": upstream comment\n\n",
    "data: {\"id\":\"\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"}}]}\n\n",
    "data: {\"id\":\"\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{}}]}\n\n",
    "data: {\"id\":\"\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning\":\"think\"}}]}\n\n",
    "data: {\"id\":\"\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"f\",\"arguments\":\"{}\"}}]}}]}\n\n",
    "data: {\"id\":\"\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
    "data: {\"id\":\"\",\"object\":\"chat.completion.chunk\",\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":22,\"total_tokens\":33,\"prompt_tokens_details\":{\"cached_tokens\":3},\"completion_tokens_details\":{\"reasoning_tokens\":5}}}\n\n",
    "data: [DONE]\n\n",
);

pub(crate) const SSE_NO_DONE: &str = concat!(
    "data: {\"id\":\"\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\n",
    "data: {\"id\":\"\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
);

pub(crate) const SSE_NO_USAGE: &str = concat!(
    "data: {\"id\":\"\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\n",
    "data: {\"id\":\"\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    "data: [DONE]\n\n",
);

/// Split `text` into pieces at byte fractions, to prove the gateway
/// reassembles nothing: events and even `[DONE]` straddle chunk boundaries.
pub(crate) fn split_pieces(text: &str, fracs: &[f64]) -> Vec<(u64, String)> {
    let mut out = Vec::new();
    let mut start = 0usize;
    for frac in fracs {
        let mut end = (text.len() as f64 * frac) as usize;
        while end > start && !text.is_char_boundary(end) {
            end -= 1;
        }
        out.push((0, text[start..end].to_string()));
        start = end;
    }
    out.push((0, text[start..].to_string()));
    out
}

pub(crate) async fn sse_response(parts: Vec<(u64, String)>) -> axum::response::Response {
    let stream = futures_util::stream::unfold(parts.into_iter(), |mut iter| async move {
        let (delay_ms, text) = iter.next()?;
        if delay_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        }
        Some((Ok::<_, std::io::Error>(Bytes::from(text)), iter))
    });
    axum::response::Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("x-request-id", "upstream-req-1")
        .body(Body::from_stream(stream))
        .unwrap()
}

pub(crate) async fn sse_error_response() -> axum::response::Response {
    // Headers and one event first, then the connection dies mid-stream.
    let stream = futures_util::stream::unfold(0u8, |i| async move {
        match i {
            0 => Some((
                Ok::<_, std::io::Error>(Bytes::from(
                    "data: {\"id\":\"\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"part\"}}]}\n\n",
                )),
                1,
            )),
            1 => {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                Some((Err(std::io::Error::other("upstream exploded")), 2))
            }
            _ => None,
        }
    });
    axum::response::Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// One event, then the upstream keeps the connection open but never sends
/// another byte.
pub(crate) async fn sse_stall() -> axum::response::Response {
    let stream = futures_util::stream::unfold(0u8, |i| async move {
        match i {
            0 => Some((
                Ok::<_, std::io::Error>(Bytes::from(
                    "data: {\"id\":\"\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"part\"}}]}\n\n",
                )),
                1,
            )),
            _ => {
                futures_util::future::pending::<()>().await;
                None
            }
        }
    });
    axum::response::Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(stream))
        .unwrap()
}

pub(crate) async fn sse_endless() -> axum::response::Response {
    let stream = futures_util::stream::unfold(0usize, |i| async move {
        if i >= 500 {
            return None;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let chunk = format!(
            "data: {{\"id\":\"\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{i}\"}}}}]}}\n\n"
        );
        Some((Ok::<_, std::io::Error>(Bytes::from(chunk)), i + 1))
    });
    axum::response::Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(stream))
        .unwrap()
}

pub(crate) async fn fake_chat(
    State(rec): State<Recorder>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    rec.seen.lock().unwrap().push(body.clone());
    rec.headers.lock().unwrap().push(headers.clone());
    let mode = headers
        .get("x-test-mode")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("normal")
        .to_string();
    match mode.as_str() {
        "sse" => return sse_response(split_pieces(SSE_NORMAL, &[0.17, 0.42, 0.66, 0.9])).await,
        "sse-nodone" => return sse_response(split_pieces(SSE_NO_DONE, &[0.55])).await,
        "sse-nousage" => return sse_response(split_pieces(SSE_NO_USAGE, &[0.6])).await,
        "sse-error" => return sse_error_response().await,
        "sse-slow" => {
            let mut pieces = split_pieces(SSE_NO_DONE, &[0.4]);
            pieces[1].0 = 1300; // upstream goes quiet for 1.3s
            return sse_response(pieces).await;
        }
        "sse-endless" => return sse_endless().await,
        "sse-stall" => return sse_stall().await,
        "slow" => {
            // Buffered response that takes a while, to exercise a client that
            // gives up first.
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        }
        "sse-delay" => {
            // Headers only after a pause, to exercise "client left first".
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            return sse_response(split_pieces(SSE_NO_USAGE, &[0.5])).await;
        }
        "error401" => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": {"message": "bad upstream key", "type": "authentication_error", "code": "invalid_api_key"}})),
            )
                .into_response();
        }
        "error426" => {
            return (
                StatusCode::UPGRADE_REQUIRED,
                Json(json!({"error": {"message": "Your Grok CLI version (none) is outdated"}})),
            )
                .into_response();
        }
        _ => {}
    }
    let choices = match mode.as_str() {
        "empty" => json!([]),
        "error" => {
            return Json(
                json!({"error": {"message": "upstream says no", "type": "invalid_request_error"}}),
            )
            .into_response();
        }
        // Deliberately vendor-flavoured 400: the client must not see the ids.
        "error400-leak" => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": {
                        "message": "model upstream-model is not available on provider od",
                        "type": "invalid_request_error",
                        "code": "model_not_found"
                    }
                })),
            )
                .into_response();
        }
        _ => json!([{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "hi",
                "reasoning_details": [{"type": "reasoning.summary", "summary": "because"}],
                "tool_calls": [{
                    "index": 0,
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "f", "arguments": "{}"}
                }]
            }
        }]),
    };
    Json(json!({
        "id": "chatcmpl-test",
        "object": "chat.completion",
        "choices": choices,
        "usage": {
            "prompt_tokens": 10,
            "completion_tokens": 20,
            "total_tokens": 30,
            "prompt_tokens_details": {"cached_tokens": 4},
            "completion_tokens_details": {"reasoning_tokens": 6}
        },
        "_received": body
    }))
    .into_response()
}

/// Responses upstream that only sends `data:` lines (what OpenDesign does).
pub(crate) const RESPONSES_SSE_DATA_ONLY: &str = concat!(
    "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"status\":\"in_progress\"}}\n\n",
    "data: {\"type\":\"response.in_progress\",\"response\":{\"id\":\"resp_1\"}}\n\n",
    "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_1\",\"role\":\"assistant\",\"content\":[]}}\n\n",
    "data: {\"type\":\"response.content_part.added\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0,\"part\":{\"type\":\"output_text\",\"text\":\"\"}}\n\n",
    "data: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0,\"delta\":\"hi\"}\n\n",
    "data: {\"type\":\"response.output_text.done\",\"item_id\":\"msg_1\",\"output_index\":0,\"content_index\":0,\"text\":\"hi\"}\n\n",
    "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_1\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"hi\"}]}}\n\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"usage\":{\"input_tokens\":11,\"output_tokens\":22,\"total_tokens\":33,\"input_tokens_details\":{\"cached_tokens\":3},\"output_tokens_details\":{\"reasoning_tokens\":5}}}}\n\n",
    "data: [DONE]\n\n",
);

/// Same stream, but already carrying `event:` lines (a compliant upstream).
pub(crate) const RESPONSES_SSE_WITH_EVENTS: &str = concat!(
    "event: response.created\n",
    "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n",
    "event: response.output_text.delta\n",
    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n",
    "event: response.completed\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":11,\"output_tokens\":22}}}\n\n",
    "data: [DONE]\n\n",
);

pub(crate) async fn sse_responses_error_response() -> axum::response::Response {
    // One good event, then the upstream connection dies.
    let stream = futures_util::stream::unfold(0u8, |i| async move {
        match i {
            0 => Some((
                Ok::<_, std::io::Error>(Bytes::from(
                    "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n",
                )),
                1,
            )),
            1 => {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                Some((Err(std::io::Error::other("upstream exploded")), 2))
            }
            _ => None,
        }
    });
    axum::response::Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(stream))
        .unwrap()
}

pub(crate) async fn sse_responses_endless() -> axum::response::Response {
    let stream = futures_util::stream::unfold(0usize, |i| async move {
        if i >= 500 {
            return None;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let chunk =
            format!("data: {{\"type\":\"response.output_text.delta\",\"delta\":\"{i}\"}}\n\n");
        Some((Ok::<_, std::io::Error>(Bytes::from(chunk)), i + 1))
    });
    axum::response::Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(stream))
        .unwrap()
}

pub(crate) async fn fake_responses(
    State(rec): State<Recorder>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    rec.seen.lock().unwrap().push(body.clone());
    rec.headers.lock().unwrap().push(headers.clone());
    let mode = headers
        .get("x-test-mode")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("normal")
        .to_string();
    match mode.as_str() {
        "sse" => {
            return sse_response(split_pieces(
                RESPONSES_SSE_DATA_ONLY,
                &[0.13, 0.37, 0.61, 0.88],
            ))
            .await;
        }
        "sse-events" => {
            return sse_response(split_pieces(RESPONSES_SSE_WITH_EVENTS, &[0.55])).await;
        }
        "sse-error" => return sse_responses_error_response().await,
        "sse-endless" => return sse_responses_endless().await,
        "sse-slow" => {
            let mut pieces = split_pieces(RESPONSES_SSE_WITH_EVENTS, &[0.35]);
            pieces[1].0 = 1300; // upstream goes quiet for 1.3s
            return sse_response(pieces).await;
        }
        "sse-delay" => {
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            return sse_response(split_pieces(RESPONSES_SSE_WITH_EVENTS, &[0.5])).await;
        }
        "error401" => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": {"message": "bad upstream key", "type": "authentication_error", "code": "invalid_api_key"}})),
            )
                .into_response();
        }
        "error426" => {
            return (
                StatusCode::UPGRADE_REQUIRED,
                Json(json!({"error": {"message": "Your Grok CLI version (none) is outdated"}})),
            )
                .into_response();
        }
        "failed" => {
            return Json(json!({
                "id": "resp_failed",
                "object": "response",
                "status": "failed",
                "error": {"code": "server_error", "message": "model overloaded"},
                "_received": body
            }))
            .into_response();
        }
        "nousage" => {
            return Json(json!({
                "id": "resp_test",
                "object": "response",
                "status": "completed",
                "output": [],
                "_received": body
            }))
            .into_response();
        }
        _ => {}
    }
    Json(json!({
        "id": "resp_test",
        "object": "response",
        "status": "completed",
        "output": [{"type": "message", "id": "msg_1", "role": "assistant",
                    "content": [{"type": "output_text", "text": "hi"}]}],
        "usage": {
            "input_tokens": 11,
            "output_tokens": 22,
            "total_tokens": 33,
            "input_tokens_details": {"cached_tokens": 3},
            "output_tokens_details": {"reasoning_tokens": 5}
        },
        "_received": body
    }))
    .into_response()
}

/// The fake upstream serves both provider URL layouts:
/// OpenDesign under `/v1/...` and DeepSeek under `/chat/completions`,
/// `/responses`, `/anthropic/v1/messages` (spec 2.0).
pub(crate) async fn start_upstream() -> (String, Recorder) {
    let rec = Recorder::default();
    let app = Router::new()
        .route("/v1/chat/completions", post(fake_chat))
        .route("/v1/responses", post(fake_responses))
        .route("/v1/messages", post(fake_messages))
        .route("/chat/completions", post(fake_chat))
        .route("/responses", post(fake_responses))
        .route("/anthropic/v1/messages", post(fake_messages))
        .route(
            "/anthropic/v1/messages/count_tokens",
            post(fake_count_tokens),
        )
        .with_state(rec.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}/v1"), rec)
}

/// Root URL (`http://host:port`) of the fake upstream, without the `/v1` suffix
/// DeepSeek does not use.
pub(crate) fn upstream_root(v1_url: &str) -> String {
    v1_url.trim_end_matches("/v1").to_string()
}

pub(crate) async fn gateway(dir: &Path, max_body: usize) -> (Router, String, Recorder) {
    gateway_with(dir, max_body, true).await
}

pub(crate) async fn gateway_with(
    dir: &Path,
    max_body: usize,
    emit_event_lines: bool,
) -> (Router, String, Recorder) {
    let (upstream, rec) = start_upstream().await;
    let mut cfg = config(dir, upstream.clone(), max_body);
    cfg.server.responses_emit_event_lines = emit_event_lines;
    let state = AppState::new(cfg, env_file(dir)).unwrap();
    (build_router(state), upstream, rec)
}

/// Register the DeepSeek provider and its model next to OpenDesign (M4a).
pub(crate) fn add_deepseek(cfg: &mut Config, v1_url: &str) {
    let root = upstream_root(v1_url);
    cfg.providers.push(Provider {
        id: "deepseek".into(),
        vendor: Some("deepseek".into()),
        base_url: root.clone(),
        anthropic_base_url: Some(format!("{root}/anthropic")),
        api_key_env: "TEST_DS_KEY".into(),
        auth: "api_key".into(),
        protocols: PROTOCOLS.iter().map(|s| s.to_string()).collect(),
        quirks: Some("deepseek".into()),
    });
    cfg.models.push(Model {
        id: "deepseek-flash".into(),
        backends: vec![Backend {
            provider: "deepseek".into(),
            upstream: "deepseek-flash".into(),
        }],
        aliases: vec!["deepseek-flash".into()],
        context_length: 1_048_576,
        context_lengths: Vec::new(),
        max_completion_tokens: 393_216,
        capabilities: vec!["text".into(), "tools".into(), "reasoning".into()],
        display_name: Some("DeepSeek Flash (DeepSeek)".into()),
        created: 1_789_026_056,
        description: None,
    });
    cfg.clients[0].allowed_models = vec!["*".into()];
}

/// Gateway with OpenDesign **and** DeepSeek routes pointing at one fake upstream.
pub(crate) async fn ds_gateway(dir: &Path) -> (Router, Recorder) {
    let (upstream, rec) = start_upstream().await;
    let mut cfg = config(dir, upstream.clone(), 52_428_800);
    add_deepseek(&mut cfg, &upstream);
    let state = AppState::new(cfg, env_file(dir)).unwrap();
    (build_router(state), rec)
}

pub(crate) fn post_to(uri: &str, body: Value, mode: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("authorization", format!("Bearer {TEST_KEY}"))
        .header("content-type", "application/json")
        .header("x-test-mode", mode)
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

pub(crate) fn chat_request(mode: &str) -> Request<Body> {
    let body = json!({
        "model": "upstream-model",
        "messages": [{"role": "user", "content": "hello"}],
        "max_tokens": 20,
        "max_completion_tokens": 99,
        "reasoning_details": [{"type": "reasoning.encrypted", "data": "abc"}],
        "custom_unknown_field": {"keep": true}
    });
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("authorization", format!("Bearer {TEST_KEY}"))
        .header("content-type", "application/json")
        .header("x-test-mode", mode)
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

pub(crate) async fn body_json(response: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), 10 * 1024 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

pub(crate) async fn body_bytes(response: axum::response::Response) -> Vec<u8> {
    axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap()
        .to_vec()
}

pub(crate) fn post_body(body: Value, mode: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("authorization", format!("Bearer {TEST_KEY}"))
        .header("content-type", "application/json")
        .header("x-test-mode", mode)
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

pub(crate) fn stream_request(mode: &str) -> Request<Body> {
    post_body(
        json!({
            "model": "upstream-model",
            "messages": [{"role": "user", "content": "hello"}],
            "stream": true
        }),
        mode,
    )
}

pub(crate) fn responses_body(stream: bool) -> Value {
    json!({
        "model": "upstream-model",
        "input": [
            {"role": "user", "content": [{"type": "input_text", "text": "hello"}]}
        ],
        "instructions": "be brief",
        "tools": [{"type": "function", "name": "f", "parameters": {"type": "object"}}],
        "tool_choice": "auto",
        "parallel_tool_calls": true,
        "max_output_tokens": 64,
        "reasoning": {"effort": "low"},
        "store": false,
        "metadata": {"k": "v"},
        "include": ["reasoning.encrypted_content"],
        "truncation": "auto",
        "custom_unknown_field": {"keep": true},
        "stream": stream
    })
}

pub(crate) fn responses_post(body: Value, mode: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/responses")
        .header("authorization", format!("Bearer {TEST_KEY}"))
        .header("content-type", "application/json")
        .header("x-test-mode", mode)
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

pub(crate) async fn wait_until(
    dir: &Path,
    pred: impl Fn(&RequestRecord) -> bool,
    timeout: std::time::Duration,
) -> Option<RequestRecord> {
    let store = LogStore::new(dir);
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let records = store.read_since(chrono::Utc::now() - chrono::TimeDelta::minutes(5));
        if let Some(found) = records.iter().find(|r| pred(r)) {
            return Some(found.clone());
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

pub(crate) async fn wait_records(dir: &Path, want: usize) -> Vec<RequestRecord> {
    let store = LogStore::new(dir);
    for _ in 0..200 {
        let records = store.read_since(chrono::Utc::now() - chrono::TimeDelta::minutes(5));
        if records.len() >= want {
            return records;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    store.read_since(chrono::Utc::now() - chrono::TimeDelta::minutes(5))
}

// ---------------------------------------------------------------------------
// M3: Anthropic Messages
// ---------------------------------------------------------------------------

/// A full Anthropic stream: `message_start` with usage, `ping`, thinking text +
/// signature deltas, a `tool_use` block with `input_json_delta`, `message_delta`
/// with the final output count and `message_stop`.
pub(crate) const MESSAGES_SSE: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"upstream-model\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":21,\"output_tokens\":1,\"cache_read_input_tokens\":4,\"cache_creation\":{\"ephemeral_5m_input_tokens\":3}}}}\n\n",
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
    "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
    "event: content_block_stop\n",
    "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
    "event: content_block_start\n",
    "data: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"Bash\",\"input\":{}}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"command\\\":\\\"date\\\"}\"}}\n\n",
    "event: content_block_stop\n",
    "data: {\"type\":\"content_block_stop\",\"index\":2}\n\n",
    "event: message_delta\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":17}}\n\n",
    "event: message_stop\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

/// Only the opening events: no `message_stop`, i.e. a truncated stream.
pub(crate) const MESSAGES_SSE_NO_STOP: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"upstream-model\",\"content\":[],\"usage\":{\"input_tokens\":21,\"output_tokens\":1}}}\n\n",
    "event: ping\n",
    "data: {\"type\":\"ping\"}\n\n",
);

/// The upstream itself reports an error mid-stream; its body must be relayed as-is.
pub(crate) const MESSAGES_SSE_UPSTREAM_ERROR: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"usage\":{\"input_tokens\":9,\"output_tokens\":1}}}\n\n",
    "event: error\n",
    "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n",
);

/// DeepSeek's Anthropic-compatible stream: `cache_creation_input_tokens` is
/// flat (spec 2.0 / 2.3) instead of OpenDesign's nested `cache_creation`.
pub(crate) const DS_MESSAGES_SSE: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_ds\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"deepseek-flash\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":21,\"output_tokens\":1,\"cache_read_input_tokens\":4,\"cache_creation_input_tokens\":7}}}\n\n",
    "event: ping\n",
    "data: {\"type\":\"ping\"}\n\n",
    "event: content_block_start\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
    "event: content_block_stop\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "event: message_delta\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":17}}\n\n",
    "event: message_stop\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

pub(crate) async fn sse_messages_error_response() -> axum::response::Response {
    // One good event, then the upstream connection dies.
    let stream = futures_util::stream::unfold(0u8, |i| async move {
        match i {
            0 => Some((
                Ok::<_, std::io::Error>(Bytes::from(
                    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"usage\":{\"input_tokens\":5,\"output_tokens\":1}}}\n\n",
                )),
                1,
            )),
            1 => {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                Some((Err(std::io::Error::other("upstream exploded")), 2))
            }
            _ => None,
        }
    });
    axum::response::Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(stream))
        .unwrap()
}

pub(crate) async fn sse_messages_endless() -> axum::response::Response {
    let stream = futures_util::stream::unfold(0usize, |i| async move {
        if i >= 500 {
            return None;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let chunk = if i == 0 {
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"content\":[],\"usage\":{\"input_tokens\":5,\"output_tokens\":1}}}\n\n".to_string()
        } else {
            format!(
                "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{i}\"}}}}\n\n"
            )
        };
        Some((Ok::<_, std::io::Error>(Bytes::from(chunk)), i + 1))
    });
    axum::response::Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(stream))
        .unwrap()
}

pub(crate) fn messages_non_stream_body(with_usage: bool) -> Value {
    let mut body = json!({
        "id": "msg_test",
        "type": "message",
        "role": "assistant",
        "model": "upstream-model",
        "content": [
            {"type": "thinking", "thinking": "ponder", "signature": "sig"},
            {"type": "redacted_thinking", "data": "opaque"},
            {"type": "text", "text": "hi"},
            {"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"command": "date"}}
        ],
        "stop_reason": "tool_use",
        "stop_sequence": null
    });
    if with_usage {
        body["usage"] = json!({
            "input_tokens": 21,
            "output_tokens": 9,
            "cache_read_input_tokens": 4,
            "cache_creation": {"ephemeral_5m_input_tokens": 3, "ephemeral_1h_input_tokens": 2}
        });
    }
    body
}

pub(crate) async fn fake_messages(
    State(rec): State<Recorder>,
    uri: axum::extract::OriginalUri,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    rec.seen.lock().unwrap().push(body.clone());
    rec.headers.lock().unwrap().push(headers.clone());
    rec.uris.lock().unwrap().push(uri.0.to_string());
    let mode = headers
        .get("x-test-mode")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("normal")
        .to_string();
    match mode.as_str() {
        "sse" => {
            return sse_response(split_pieces(MESSAGES_SSE, &[0.11, 0.33, 0.57, 0.79])).await;
        }
        "sse-nostop" => {
            return sse_response(split_pieces(MESSAGES_SSE_NO_STOP, &[0.5])).await;
        }
        "sse-error" => return sse_messages_error_response().await,
        "sse-error-event" => {
            return sse_response(split_pieces(MESSAGES_SSE_UPSTREAM_ERROR, &[0.6])).await;
        }
        "sse-slow" => {
            let mut pieces = split_pieces(MESSAGES_SSE, &[0.08]);
            pieces[1].0 = 1300; // upstream goes quiet for 1.3s
            return sse_response(pieces).await;
        }
        "sse-delay" => {
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            return sse_response(split_pieces(MESSAGES_SSE_NO_STOP, &[0.5])).await;
        }
        "sse-endless" => return sse_messages_endless().await,
        "error401" => {
            let mut resp = Json(json!({
                "type": "error",
                "error": {"type": "authentication_error", "message": "invalid x-api-key"}
            }))
            .into_response();
            *resp.status_mut() = StatusCode::UNAUTHORIZED;
            resp.headers_mut().insert(
                "retry-after",
                "Wed, 21 Oct 2099 07:28:00 GMT".parse().unwrap(),
            );
            resp.headers_mut()
                .insert("x-request-id", "upstream-msg-401".parse().unwrap());
            resp.headers_mut()
                .insert("x-should-retry", "true".parse().unwrap());
            return resp;
        }
        "error400" => {
            let mut resp = Json(json!({
                "type": "error",
                "error": {"type": "invalid_request_error", "message": "max_tokens: field required"}
            }))
            .into_response();
            *resp.status_mut() = StatusCode::BAD_REQUEST;
            return resp;
        }
        "nousage" => {
            let mut body = messages_non_stream_body(false);
            body["_received"] = body.clone();
            return Json(body).into_response();
        }
        // DeepSeek usage shape: flat `cache_creation_input_tokens`.
        "flat-cache" => {
            let mut body = messages_non_stream_body(true);
            body["usage"] = json!({
                "input_tokens": 21,
                "output_tokens": 9,
                "cache_creation_input_tokens": 7,
                "cache_read_input_tokens": 2
            });
            return Json(body).into_response();
        }
        "sse-flat" => {
            return sse_response(split_pieces(DS_MESSAGES_SSE, &[0.2, 0.55, 0.85])).await;
        }
        _ => {}
    }
    let mut body = messages_non_stream_body(true);
    body["_received"] = body.clone();
    body["_received"]["_received"] = Value::Null;
    Json(body).into_response()
}

pub(crate) async fn fake_count_tokens(
    State(rec): State<Recorder>,
    uri: axum::extract::OriginalUri,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    rec.seen.lock().unwrap().push(body);
    rec.headers.lock().unwrap().push(headers);
    rec.uris.lock().unwrap().push(uri.0.to_string());
    Json(json!({"input_tokens": 31})).into_response()
}

pub(crate) fn messages_body(stream: bool) -> Value {
    json!({
        "model": "upstream-model",
        "max_tokens": 64,
        "messages": [{
            "role": "user",
            "content": [{"type": "text", "text": "hello", "cache_control": {"type": "ephemeral"}}]
        }],
        "system": [{"type": "text", "text": "be brief", "cache_control": {"type": "ephemeral"}}],
        "tools": [{
            "name": "Bash",
            "description": "run a command",
            "input_schema": {"type": "object", "properties": {"command": {"type": "string"}}}
        }],
        "tool_choice": {"type": "auto"},
        "thinking": {"type": "enabled", "budget_tokens": 1024},
        "metadata": {"user_id": "u1"},
        "stop_sequences": ["STOP"],
        "temperature": 0.3,
        "top_p": 0.9,
        "top_k": 5,
        "output_config": {"effort": "high"},
        "context_management": {"edits": []},
        "custom_beta_field": {"keep": true},
        "stream": stream
    })
}

pub(crate) fn messages_post(body: Value, mode: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/messages?beta=true")
        .header("authorization", format!("Bearer {TEST_KEY}"))
        .header("content-type", "application/json")
        .header("x-test-mode", mode)
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

// ---------------------------------------------------------------------------
// M4b: fallback + circuit breaker
// ---------------------------------------------------------------------------

/// Per-model scripted upstream: `plan[model]` is a queue of HTTP statuses
/// (`200` = normal success) consumed in order; `hits` records every upstream
/// model called, in order.
#[derive(Clone, Default)]
pub(crate) struct ScriptedFake {
    plan: Arc<Mutex<HashMap<String, Vec<u16>>>>,
    hits: Arc<Mutex<Vec<String>>>,
    stream_dies: Arc<AtomicBool>,
    delay_ms: Arc<AtomicU64>,
}

impl ScriptedFake {
    pub(crate) fn queue(&self, model: &str, statuses: &[u16]) {
        self.plan
            .lock()
            .unwrap()
            .entry(model.to_string())
            .or_default()
            .extend_from_slice(statuses);
    }

    pub(crate) fn hits(&self) -> Vec<String> {
        self.hits.lock().unwrap().clone()
    }

    pub(crate) fn hits_of(&self, model: &str) -> usize {
        self.hits().iter().filter(|hit| *hit == model).count()
    }

    pub(crate) fn set_delay_ms(&self, ms: u64) {
        self.delay_ms.store(ms, Ordering::SeqCst);
    }

    pub(crate) fn die_in_stream(&self) {
        self.stream_dies.store(true, Ordering::SeqCst);
    }

    pub(crate) fn heal_stream(&self) {
        self.stream_dies.store(false, Ordering::SeqCst);
    }
}

pub(crate) async fn scripted_handler(
    State(fake): State<ScriptedFake>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    fake.hits.lock().unwrap().push(model.clone());
    let delay_ms = fake.delay_ms.load(Ordering::SeqCst);
    if delay_ms > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
    }
    let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    if stream && fake.stream_dies.load(Ordering::SeqCst) {
        // 200 + one event, then the connection dies: no fallback is possible.
        return sse_error_response().await;
    }
    let planned = {
        let mut plan = fake.plan.lock().unwrap();
        match plan.get_mut(&model) {
            Some(queue) if !queue.is_empty() => queue.remove(0),
            _ => 200,
        }
    };
    if planned == 200 {
        if stream {
            return sse_response(split_pieces(SSE_NO_USAGE, &[0.5])).await;
        }
        return Json(json!({
            "id": "scripted",
            "object": "chat.completion",
            "choices": [{"index": 0, "index_note": "scripted", "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5},
            "_served_by": model,
        }))
        .into_response();
    }
    (
        StatusCode::from_u16(planned).unwrap(),
        Json(json!({
            "error": {
                "message": format!("{model} says {planned}"),
                "type": "upstream_error",
                "code": format!("error_{planned}")
            }
        })),
    )
        .into_response()
}

pub(crate) async fn start_scripted() -> (String, ScriptedFake) {
    let fake = ScriptedFake::default();
    let app = Router::new()
        .route("/v1/chat/completions", post(scripted_handler))
        .route("/v1/responses", post(scripted_handler))
        .route("/v1/messages", post(scripted_handler))
        .route("/chat/completions", post(scripted_handler))
        .route("/responses", post(scripted_handler))
        .route("/anthropic/v1/messages", post(scripted_handler))
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), fake)
}

/// Two providers, two models: `od/upstream-model` (upstream `upstream-model`)
/// optionally falls back to `deepseek/deepseek-flash` (upstream
/// `fallback-model`), both same-protocol.
pub(crate) fn m4b_config(
    dir: &Path,
    od_base: String,
    ds_root: String,
    with_fallback: bool,
) -> Config {
    let mut cfg = config(dir, od_base, 52_428_800);
    cfg.providers.push(Provider {
        id: "deepseek".into(),
        vendor: Some("deepseek".into()),
        base_url: ds_root.clone(),
        anthropic_base_url: Some(format!("{ds_root}/anthropic")),
        api_key_env: "TEST_DS_KEY".into(),
        auth: "api_key".into(),
        protocols: PROTOCOLS.iter().map(|s| s.to_string()).collect(),
        quirks: Some("deepseek".into()),
    });
    if with_fallback {
        cfg.models[0].backends.push(Backend {
            provider: "deepseek".into(),
            upstream: "fallback-model".into(),
        });
    }
    cfg.models.push(Model {
        id: "deepseek-flash".into(),
        backends: vec![Backend {
            provider: "deepseek".into(),
            upstream: "fallback-model".into(),
        }],
        aliases: vec!["deepseek-flash".into()],
        context_length: 1_048_576,
        context_lengths: Vec::new(),
        max_completion_tokens: 393_216,
        capabilities: vec!["text".into()],
        display_name: Some("DeepSeek Flash (DeepSeek)".into()),
        created: 1_789_026_056,
        description: None,
    });
    cfg.clients[0].allowed_models = vec!["*".into()];
    cfg
}

pub(crate) async fn m4b_gateway_with(
    dir: &Path,
    od_base: Option<String>,
    with_fallback: bool,
) -> (Router, ScriptedFake) {
    let (root, fake) = start_scripted().await;
    let od = od_base.unwrap_or_else(|| format!("{root}/v1"));
    let cfg = m4b_config(dir, od, root, with_fallback);
    let state = AppState::new(cfg, env_file(dir)).unwrap();
    (build_router(state), fake)
}

pub(crate) async fn m4b_gateway(dir: &Path, with_fallback: bool) -> (Router, ScriptedFake) {
    m4b_gateway_with(dir, None, with_fallback).await
}

pub(crate) fn chat_model(model: &str, stream: bool) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("authorization", format!("Bearer {TEST_KEY}"))
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "model": model,
                "messages": [{"role": "user", "content": "hello"}],
                "stream": stream
            }))
            .unwrap(),
        ))
        .unwrap()
}

pub(crate) fn messages_model(model: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("authorization", format!("Bearer {TEST_KEY}"))
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "model": model,
                "max_tokens": 16,
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .unwrap(),
        ))
        .unwrap()
}

pub(crate) fn healthz_request() -> Request<Body> {
    Request::builder()
        .uri("/healthz")
        .body(Body::empty())
        .unwrap()
}
