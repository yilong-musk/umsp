//! End-to-end tests over the real router with a fake upstream.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};
use tower::ServiceExt;

use umsp::config::{Backend, Client, Config, Model, PROTOCOLS, Provider, ServerConfig};
use umsp::envfile::EnvFile;
use umsp::keys;
use umsp::route::UsageWindow;
use umsp::server::{AppState, build_router};
use umsp::usage::{LogStore, RequestRecord};

const TEST_KEY: &str = "umsp_testkey_0123456789";

/// Deterministic per-test directory under the scratch TMPDIR. A fresh path was
/// leaking one directory per test per run (576 after a day of iteration); a
/// stable name means the next run reuses it after wiping the old state.
fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("umsp-it-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn env_file(dir: &Path) -> EnvFile {
    let path = dir.join("env");
    std::fs::write(
        &path,
        // Both upstreams' keys live in one env file; each provider names its own.
        "TEST_OD_KEY=sk-test\nTEST_DS_KEY=sk-ds-test\n",
    )
    .unwrap();
    EnvFile::load(&path)
}

fn config(dir: &Path, upstream_url: String, max_body: usize) -> Config {
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
        }],
    }
}

// ---------------------------------------------------------------------------
// Fake upstream
// ---------------------------------------------------------------------------

/// What the fake upstream saw on the wire (last request body + headers).
#[derive(Clone, Default)]
struct Recorder {
    seen: Arc<Mutex<Vec<Value>>>,
    headers: Arc<Mutex<Vec<axum::http::HeaderMap>>>,
    uris: Arc<Mutex<Vec<String>>>,
}

impl Recorder {
    fn last(&self) -> Value {
        self.seen
            .lock()
            .unwrap()
            .last()
            .cloned()
            .unwrap_or(Value::Null)
    }

    fn last_headers(&self) -> axum::http::HeaderMap {
        self.headers
            .lock()
            .unwrap()
            .last()
            .cloned()
            .unwrap_or_default()
    }

    fn last_uri(&self) -> String {
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
const SSE_NORMAL: &str = concat!(
    ": upstream comment\n\n",
    "data: {\"id\":\"\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"}}]}\n\n",
    "data: {\"id\":\"\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{}}]}\n\n",
    "data: {\"id\":\"\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning\":\"think\"}}]}\n\n",
    "data: {\"id\":\"\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"f\",\"arguments\":\"{}\"}}]}}]}\n\n",
    "data: {\"id\":\"\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
    "data: {\"id\":\"\",\"object\":\"chat.completion.chunk\",\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":22,\"total_tokens\":33,\"prompt_tokens_details\":{\"cached_tokens\":3},\"completion_tokens_details\":{\"reasoning_tokens\":5}}}\n\n",
    "data: [DONE]\n\n",
);

const SSE_NO_DONE: &str = concat!(
    "data: {\"id\":\"\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\n",
    "data: {\"id\":\"\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
);

const SSE_NO_USAGE: &str = concat!(
    "data: {\"id\":\"\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\n",
    "data: {\"id\":\"\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    "data: [DONE]\n\n",
);

/// Split `text` into pieces at byte fractions, to prove the gateway
/// reassembles nothing: events and even `[DONE]` straddle chunk boundaries.
fn split_pieces(text: &str, fracs: &[f64]) -> Vec<(u64, String)> {
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

async fn sse_response(parts: Vec<(u64, String)>) -> axum::response::Response {
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

async fn sse_error_response() -> axum::response::Response {
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
async fn sse_stall() -> axum::response::Response {
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

async fn sse_endless() -> axum::response::Response {
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

async fn fake_chat(
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
const RESPONSES_SSE_DATA_ONLY: &str = concat!(
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
const RESPONSES_SSE_WITH_EVENTS: &str = concat!(
    "event: response.created\n",
    "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n",
    "event: response.output_text.delta\n",
    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n",
    "event: response.completed\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":11,\"output_tokens\":22}}}\n\n",
    "data: [DONE]\n\n",
);

async fn sse_responses_error_response() -> axum::response::Response {
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

async fn sse_responses_endless() -> axum::response::Response {
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

async fn fake_responses(
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
async fn start_upstream() -> (String, Recorder) {
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
fn upstream_root(v1_url: &str) -> String {
    v1_url.trim_end_matches("/v1").to_string()
}

async fn gateway(dir: &Path, max_body: usize) -> (Router, String, Recorder) {
    gateway_with(dir, max_body, true).await
}

async fn gateway_with(
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
fn add_deepseek(cfg: &mut Config, v1_url: &str) {
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
async fn ds_gateway(dir: &Path) -> (Router, Recorder) {
    let (upstream, rec) = start_upstream().await;
    let mut cfg = config(dir, upstream.clone(), 52_428_800);
    add_deepseek(&mut cfg, &upstream);
    let state = AppState::new(cfg, env_file(dir)).unwrap();
    (build_router(state), rec)
}

fn post_to(uri: &str, body: Value, mode: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("authorization", format!("Bearer {TEST_KEY}"))
        .header("content-type", "application/json")
        .header("x-test-mode", mode)
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

fn chat_request(mode: &str) -> Request<Body> {
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

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), 10 * 1024 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

async fn body_bytes(response: axum::response::Response) -> Vec<u8> {
    axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap()
        .to_vec()
}

fn post_body(body: Value, mode: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("authorization", format!("Bearer {TEST_KEY}"))
        .header("content-type", "application/json")
        .header("x-test-mode", mode)
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

fn stream_request(mode: &str) -> Request<Body> {
    post_body(
        json!({
            "model": "upstream-model",
            "messages": [{"role": "user", "content": "hello"}],
            "stream": true
        }),
        mode,
    )
}

fn responses_body(stream: bool) -> Value {
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

fn responses_post(body: Value, mode: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/responses")
        .header("authorization", format!("Bearer {TEST_KEY}"))
        .header("content-type", "application/json")
        .header("x-test-mode", mode)
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

async fn wait_until(
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

async fn wait_records(dir: &Path, want: usize) -> Vec<RequestRecord> {
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
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn chat_proxy_applies_od_quirks_and_counts_usage() {
    let dir = temp_dir("chat");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;

    let response = app.clone().oneshot(chat_request("normal")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let value = body_json(response).await;

    // Request rewriting: only `model` plus the max_tokens quirk.
    assert_eq!(value["_received"]["model"], "upstream-model");
    assert_eq!(value["_received"]["max_tokens"], 20);
    assert!(value["_received"].get("max_completion_tokens").is_none());
    assert_eq!(value["_received"]["custom_unknown_field"]["keep"], true);
    assert_eq!(
        value["_received"]["reasoning_details"][0]["type"],
        "reasoning.encrypted"
    );

    // Response quirks: finish_reason inferred, tool_call index stripped,
    // reasoning_details preserved.
    assert_eq!(value["choices"][0]["finish_reason"], "length");
    assert!(
        value["choices"][0]["message"]["tool_calls"][0]
            .get("index")
            .is_none()
    );
    assert_eq!(
        value["choices"][0]["message"]["reasoning_details"][0]["summary"],
        "because"
    );

    // Ledger recorded exactly one request with usage.
    let store = umsp::usage::LogStore::new(&dir);
    let records = store.read_since(chrono::Utc::now() - chrono::TimeDelta::minutes(5));
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].status, 200);
    assert_eq!(records[0].prompt_tokens, 10);
    assert_eq!(records[0].completion_tokens, 20);
    assert_eq!(records[0].cached_tokens, 4);
    assert_eq!(records[0].reasoning_tokens, 6);
    assert_eq!(records[0].requested_model, "upstream-model");
    assert_eq!(records[0].upstream_model, "upstream-model");
    assert!(!records[0].quirks.is_empty());
}

#[tokio::test]
async fn empty_choices_are_synthesized() {
    let dir = temp_dir("empty");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let response = app.oneshot(chat_request("empty")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let value = body_json(response).await;
    assert_eq!(value["choices"][0]["message"]["role"], "assistant");
    assert_eq!(value["choices"][0]["finish_reason"], "length");
}

#[tokio::test]
async fn missing_key_is_401_with_umsp_code() {
    let dir = temp_dir("auth");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"model":"upstream-model","messages":[]}"#))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        response.headers().get("x-umsp-code").unwrap(),
        "unauthorized"
    );
    let value = body_json(response).await;
    assert_eq!(value["error"]["code"], "unauthorized");
    assert_eq!(value["error"]["type"], "authentication_error");
}

#[tokio::test]
async fn x_api_key_header_is_accepted() {
    let dir = temp_dir("xapikey");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let request = Request::builder()
        .method("GET")
        .uri("/v1/models")
        .header("x-api-key", TEST_KEY)
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn unknown_model_is_404() {
    let dir = temp_dir("model");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let body = json!({"model": "od/nope", "messages": []});
    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("authorization", format!("Bearer {TEST_KEY}"))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        response.headers().get("x-umsp-code").unwrap(),
        "model_not_found"
    );
}

#[tokio::test]
async fn oversized_body_is_413() {
    let dir = temp_dir("413");
    let (app, _, _) = gateway(&dir, 64).await;
    let big = format!(
        r#"{{"model":"upstream-model","messages":[],"pad":"{}"}}"#,
        "x".repeat(4096)
    );
    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("authorization", format!("Bearer {TEST_KEY}"))
        .header("content-type", "application/json")
        .body(Body::from(big))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        response.headers().get("x-umsp-code").unwrap(),
        "request_too_large"
    );
}

#[tokio::test]
async fn streaming_forwards_bytes_verbatim_and_records_usage() {
    let dir = temp_dir("sse");
    let (app, _, rec) = gateway(&dir, 50 * 1024 * 1024).await;

    let response = app.oneshot(stream_request("sse")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "text/event-stream"
    );
    assert!(response.headers().get("content-length").is_none());
    assert_eq!(response.headers().get("cache-control").unwrap(), "no-cache");
    assert_eq!(response.headers().get("x-accel-buffering").unwrap(), "no");
    assert!(response.headers().get("x-umsp-request-id").is_some());
    assert!(
        response.headers().get("x-umsp-provider").is_none(),
        "provider header must not leak to clients"
    );
    assert!(
        response.headers().get("x-umsp-upstream-model").is_none(),
        "upstream model header must not leak"
    );
    // No upstream header is relayed: vendor trace ids would reveal the backend.
    assert!(response.headers().get("x-request-id").is_none());
    assert!(response.headers().get("x-upstream-echo").is_none());

    // Byte-for-byte: comments, empty deltas, empty id, tool deltas, [DONE].
    let bytes = body_bytes(response).await;
    assert_eq!(
        bytes,
        SSE_NORMAL.as_bytes(),
        "SSE must be forwarded verbatim"
    );

    // Accounting: include_usage was injected because the client omitted it.
    assert_eq!(rec.last()["stream_options"]["include_usage"], true);
    assert_eq!(rec.last()["model"], "upstream-model");

    // Upstream request headers: streaming accept, no compression, exactly one
    // content-type, and the provider key (never the client key).
    let up_headers = rec.last_headers();
    assert_eq!(up_headers.get("accept").unwrap(), "text/event-stream");
    assert_eq!(up_headers.get("accept-encoding").unwrap(), "identity");
    assert_eq!(up_headers.get("authorization").unwrap(), "Bearer sk-test");
    assert_eq!(up_headers.get_all("content-type").iter().count(), 1);

    let records = wait_records(&dir, 1).await;
    assert_eq!(records.len(), 1);
    let r = &records[0];
    assert!(r.stream);
    assert_eq!(r.status, 200);
    assert_eq!(r.upstream_status, Some(200));
    assert_eq!(r.prompt_tokens, 11);
    assert_eq!(r.completion_tokens, 22);
    assert_eq!(r.cached_tokens, 3);
    assert_eq!(r.reasoning_tokens, 5);
    assert!(r.ttft_ms.is_some(), "ttft_ms must be recorded");
    assert_eq!(r.bytes_out, SSE_NORMAL.len() as u64);
    assert!(!r.client_disconnected);
    assert!(r.error_code.is_none());
    assert!(r.quirks.iter().any(|q| q == "injected_include_usage"));
}

#[tokio::test]
async fn streaming_appends_done_when_upstream_omits_it() {
    let dir = temp_dir("sse-nodone");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let bytes = body_bytes(app.oneshot(stream_request("sse-nodone")).await.unwrap()).await;
    let expected = format!("{SSE_NO_DONE}data: [DONE]\n\n");
    assert_eq!(String::from_utf8(bytes).unwrap(), expected);

    let r = wait_until(&dir, |_| true, std::time::Duration::from_secs(5))
        .await
        .expect("ledger row");
    assert!(r.stream);
    assert_eq!(r.status, 200);
    // No usage chunk from upstream -> recorded as missing, never estimated.
    assert_eq!(r.completion_tokens, 0);
    assert!(r.quirks.iter().any(|q| q == "usage_missing"));
}

#[tokio::test]
async fn streaming_respects_explicit_include_usage_false() {
    let dir = temp_dir("sse-nousage");
    let (app, _, rec) = gateway(&dir, 50 * 1024 * 1024).await;
    let body = json!({
        "model": "upstream-model",
        "messages": [],
        "stream": true,
        "stream_options": {"include_usage": false}
    });
    let bytes = body_bytes(app.oneshot(post_body(body, "sse-nousage")).await.unwrap()).await;
    assert_eq!(String::from_utf8(bytes).unwrap(), SSE_NO_USAGE);
    assert_eq!(rec.last()["stream_options"]["include_usage"], false);

    let r = wait_until(&dir, |_| true, std::time::Duration::from_secs(5))
        .await
        .expect("ledger row");
    assert!(r.quirks.iter().any(|q| q == "usage_missing"));
    assert!(!r.quirks.iter().any(|q| q == "injected_include_usage"));
}

#[tokio::test]
async fn streaming_keepalive_fills_upstream_silence() {
    let dir = temp_dir("sse-ka");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let bytes = body_bytes(app.oneshot(stream_request("sse-slow")).await.unwrap()).await;
    let text = String::from_utf8(bytes).unwrap();
    let keepalives = text.matches(": keep-alive\n\n").count();
    assert!(keepalives >= 1, "expected keep-alive comments: {text:?}");
    assert_eq!(
        text.replace(": keep-alive\n\n", ""),
        format!("{SSE_NO_DONE}data: [DONE]\n\n")
    );
}

#[tokio::test]
async fn streaming_idle_timeout_fires_despite_keepalives() {
    let dir = temp_dir("sse-idle");
    let (upstream, _) = start_upstream().await;
    let mut cfg = config(&dir, upstream, 50 * 1024 * 1024);
    // Keepalive fires several times inside one idle window; it must not
    // extend the window.
    cfg.server.stream_idle_timeout_s = 2;
    cfg.server.stream_keepalive_s = 1;
    let app = build_router(AppState::new(cfg, env_file(&dir)).unwrap());

    let started = Instant::now();
    let response = app.oneshot(stream_request("sse-stall")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = tokio::time::timeout(Duration::from_secs(10), body_bytes(response))
        .await
        .expect("stream must end at the idle timeout, not hang");
    let elapsed = started.elapsed();
    let text = String::from_utf8(bytes).unwrap();
    assert!(text.contains(": keep-alive\n\n"), "{text:?}");
    // The wire code stays `upstream_stream_error` (docs/protocols.md); the
    // message and the ledger row carry the timeout.
    assert!(text.contains("upstream stream idle timeout"), "{text:?}");
    assert!(text.ends_with("data: [DONE]\n\n"));
    assert!(elapsed >= Duration::from_secs(2), "{elapsed:?}");

    let r = wait_until(&dir, |_| true, Duration::from_secs(5))
        .await
        .expect("ledger row");
    assert_eq!(r.error_code.as_deref(), Some("upstream_stream_timeout"));
}

#[tokio::test]
async fn streaming_upstream_error_after_headers_emits_error_then_done() {
    let dir = temp_dir("sse-err");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let response = app.oneshot(stream_request("sse-error")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let text = String::from_utf8(body_bytes(response).await).unwrap();
    assert!(
        text.starts_with(
            "data: {\"id\":\"\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"part\"}}]}\n\n"
        ),
        "partial upstream event must be relayed: {text:?}"
    );
    assert!(text.contains("\"type\":\"upstream_error\""));
    assert!(text.contains("\"code\":\"upstream_stream_error\""));
    assert!(text.ends_with("data: [DONE]\n\n"));
    // No forged success: no finish_reason, no usage numbers.
    assert!(!text.contains("finish_reason"));

    let r = wait_until(&dir, |_| true, std::time::Duration::from_secs(5))
        .await
        .expect("ledger row");
    assert_eq!(r.status, 200);
    assert_eq!(r.error_code.as_deref(), Some("upstream_stream_error"));
    assert_eq!(r.completion_tokens, 0);
    assert!(r.ttft_ms.is_some());
}

#[tokio::test]
async fn streaming_upstream_error_before_headers_is_relayed_verbatim() {
    let dir = temp_dir("sse-401");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let response = app.oneshot(stream_request("error401")).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    // Not an SSE response: gateway errors are plain JSON.
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "application/json"
    );
    assert_eq!(
        response.headers().get("x-umsp-code").unwrap(),
        "upstream_auth_failed"
    );
    assert!(
        response.headers().get("x-request-id").is_none(),
        "upstream headers must not leak"
    );
    let value = body_json(response).await;
    assert_eq!(
        value["error"]["message"],
        "the gateway could not authenticate with the upstream service"
    );

    let r = wait_until(&dir, |_| true, std::time::Duration::from_secs(5))
        .await
        .expect("ledger row");
    assert!(r.stream);
    assert_eq!(r.status, 502);
    assert_eq!(r.upstream_status, Some(401));
    assert_eq!(r.error_code.as_deref(), Some("upstream_401"));
}

#[tokio::test]
async fn streaming_disconnect_before_headers_still_logs() {
    let dir = temp_dir("sse-disc-early");
    let (upstream, _rec) = start_upstream().await;
    let cfg = config(&dir, upstream, 50 * 1024 * 1024);
    let state = AppState::new(cfg, env_file(&dir)).unwrap();
    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    use tokio::io::AsyncWriteExt;
    let body = serde_json::to_string(&json!({
        "model": "upstream-model",
        "messages": [],
        "stream": true
    }))
    .unwrap();
    let request = format!(
        "POST /v1/chat/completions HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {TEST_KEY}\r\nContent-Type: application/json\r\nx-test-mode: sse-delay\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let mut sock = tokio::net::TcpStream::connect(addr).await.unwrap();
    sock.write_all(request.as_bytes()).await.unwrap();
    drop(sock); // gone before upstream headers arrive

    let record = wait_until(
        &dir,
        |r| r.client_disconnected,
        std::time::Duration::from_secs(10),
    )
    .await
    .expect("a 499 row must be written instead of losing the request");
    assert_eq!(record.status, 499);
    assert_eq!(record.error_code.as_deref(), Some("client_disconnected"));
    assert!(record.stream);
}

#[tokio::test]
async fn non_boolean_stream_field_is_handled_as_false() {
    let dir = temp_dir("stream-nonbool");
    let (app, _, rec) = gateway(&dir, 50 * 1024 * 1024).await;
    let body = json!({
        "model": "upstream-model",
        "messages": [],
        "stream": "yes"
    });
    let response = app.oneshot(post_body(body, "normal")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "application/json"
    );
    // Normalized on the wire: strict upstreams reject a non-boolean `stream`.
    assert_eq!(rec.last()["stream"], false);
    assert!(rec.last().get("stream_options").is_none());
    let value = body_json(response).await;
    assert_eq!(value["choices"][0]["message"]["content"], "hi");
    let records = wait_records(&dir, 1).await;
    assert!(records[0].quirks.iter().any(|q| q == "stream_non_boolean"));
    assert!(!records[0].stream);
}

#[tokio::test]
async fn streaming_client_disconnect_is_recorded_and_server_survives() {
    let dir = temp_dir("sse-disc");
    let (upstream, _rec) = start_upstream().await;
    let cfg = config(&dir, upstream, 50 * 1024 * 1024);
    let state = AppState::new(cfg, env_file(&dir)).unwrap();
    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let body = json!({
        "model": "upstream-model",
        "messages": [],
        "stream": true
    });
    let body = serde_json::to_string(&body).unwrap();
    let request = format!(
        "POST /v1/chat/completions HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {TEST_KEY}\r\nContent-Type: application/json\r\nx-test-mode: sse-endless\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let mut sock = tokio::net::TcpStream::connect(addr).await.unwrap();
    sock.write_all(request.as_bytes()).await.unwrap();
    let mut buf = [0u8; 1024];
    let n = tokio::time::timeout(std::time::Duration::from_secs(5), sock.read(&mut buf))
        .await
        .expect("first bytes")
        .expect("read");
    assert!(n > 0);
    drop(sock);

    let record = wait_until(
        &dir,
        |r| r.client_disconnected,
        std::time::Duration::from_secs(10),
    )
    .await
    .expect("ledger row with client_disconnected=true");
    assert_eq!(record.status, 200);
    assert!(record.stream);

    // Port still serves after the disconnect.
    let mut sock = tokio::net::TcpStream::connect(addr).await.unwrap();
    sock.write_all(
        format!("GET /healthz HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n").as_bytes(),
    )
    .await
    .unwrap();
    let mut raw = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        sock.read_to_end(&mut raw),
    )
    .await
    .expect("healthz response")
    .expect("read");
    let text = String::from_utf8_lossy(&raw);
    assert!(
        text.starts_with("HTTP/1.1 200"),
        "healthz still up: {text:?}"
    );
}

#[tokio::test]
async fn models_list_is_flat_and_filtered_by_alias_resolution() {
    let dir = temp_dir("models");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let request = Request::builder()
        .method("GET")
        .uri("/v1/models")
        .header("authorization", format!("Bearer {TEST_KEY}"))
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let value = body_json(response).await;
    assert_eq!(value["object"], "list");
    let model = &value["data"][0];
    assert_eq!(model["id"], "upstream-model");
    assert_eq!(model["context_length"], 1048000);
    assert_eq!(model["max_completion_tokens"], 128000);
    assert_eq!(model["capabilities"]["tools"], true);
    assert_eq!(model["capabilities"]["vision"], false);
    assert!(model.get("metadata").is_none());

    // Gateway metadata envelope (cc-switch style consumers): same card data
    // under the names those parsers read, no provider data, flat fields intact.
    let meta = &model["meta"];
    assert_eq!(meta["context_length"], 1048000);
    assert_eq!(meta["max_output_tokens"], 128000);
    assert_eq!(meta["display_name"], "Test Model");
    assert_eq!(meta["tool_call"], true);
    assert_eq!(meta["reasoning"], true);
    assert_eq!(meta["capabilities"]["vision"], false);
    assert!(meta["capabilities"].get("vision_source").is_none());
    assert_eq!(meta["input_modalities"], json!(["text"]));
    assert_eq!(meta["output_modalities"], json!(["text"]));
    for key in ["provider", "upstream", "upstream_model", "backends"] {
        assert!(meta.get(key).is_none(), "meta must not expose {key}");
    }

    // Alias also resolves through the chat endpoint.
    let body = json!({"model": "short", "messages": []});
    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("authorization", format!("Bearer {TEST_KEY}"))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn health_and_hello() {
    let dir = temp_dir("health");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;

    let request = Request::builder()
        .method("GET")
        .uri("/healthz")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let value = body_json(response).await;
    assert_eq!(value["status"], "ok");
    assert_eq!(value["version"], env!("CARGO_PKG_VERSION"));
    assert!(value["providers"]["od"].is_object());

    let request = Request::builder()
        .method("HEAD")
        .uri("/api/hello")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn upstream_error_body_is_passed_through() {
    let dir = temp_dir("upstream-err");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let response = app.oneshot(chat_request("error")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK); // fake returns 200 with an error body
    let value = body_json(response).await;
    assert_eq!(value["error"]["message"], "upstream says no");
    assert!(value.get("choices").is_none());
}

#[tokio::test]
async fn upstream_401_becomes_a_neutral_gateway_error() {
    let dir = temp_dir("upstream-401");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let response = app.oneshot(chat_request("error401")).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(
        response.headers().get("x-umsp-code").unwrap(),
        "upstream_auth_failed"
    );
    assert!(response.headers().get("x-umsp-request-id").is_some());
    let value = body_json(response).await;
    assert_eq!(
        value["error"]["message"],
        "the gateway could not authenticate with the upstream service"
    );
    assert_eq!(value["error"]["code"], "upstream_auth_failed");
}

#[tokio::test]
async fn upstream_426_becomes_a_neutral_gateway_error() {
    let dir = temp_dir("upstream-426");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let response = app.oneshot(chat_request("error426")).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(
        response.headers().get("x-umsp-code").unwrap(),
        "upstream_client_outdated"
    );
    let value = body_json(response).await;
    assert_eq!(value["error"]["code"], "upstream_client_outdated");
    assert!(
        !value.to_string().to_lowercase().contains("grok"),
        "vendor client details must not leak: {value}"
    );
}

#[tokio::test]
async fn upstream_400_body_is_scrubbed_before_reaching_the_client() {
    let dir = temp_dir("scrub-400");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let response = app.oneshot(chat_request("error400-leak")).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "application/json"
    );
    let text = String::from_utf8(body_bytes(response).await).unwrap();
    assert!(
        text.contains("on provider umsp"),
        "provider id must be replaced: {text}"
    );
    assert!(!text.contains(" od"), "provider id leaked: {text}");
    assert!(
        text.contains("upstream-model"),
        "the message itself must survive: {text}"
    );

    let r = wait_until(&dir, |_| true, std::time::Duration::from_secs(5))
        .await
        .expect("ledger row");
    assert_eq!(r.status, 400);
    assert_eq!(r.upstream_status, Some(400));
    assert!(
        r.upstream_body.unwrap_or_default().contains("provider od"),
        "the raw body stays in the ledger"
    );
}

// ---------------------------------------------------------------------------
// Responses (M2)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn responses_non_stream_relays_body_verbatim_and_records_usage() {
    let dir = temp_dir("resp-nonstream");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let response = app
        .oneshot(responses_post(responses_body(false), "normal"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response.headers().get("x-umsp-provider").is_none(),
        "provider header must not leak to clients"
    );
    assert!(
        response.headers().get("x-umsp-upstream-model").is_none(),
        "upstream model header must not leak"
    );
    assert!(response.headers().get("x-umsp-request-id").is_some());

    let value = body_json(response).await;
    assert_eq!(value["id"], "resp_test");
    assert_eq!(value["usage"]["input_tokens"], 11);
    // Only `model` is rewritten; every other field reaches the upstream as sent.
    let received = &value["_received"];
    assert_eq!(received["model"], "upstream-model");
    assert_eq!(received["instructions"], "be brief");
    assert_eq!(received["max_output_tokens"], 64);
    assert_eq!(received["reasoning"]["effort"], "low");
    assert_eq!(received["store"], false);
    assert_eq!(received["metadata"]["k"], "v");
    assert_eq!(received["include"][0], "reasoning.encrypted_content");
    assert_eq!(received["truncation"], "auto");
    assert_eq!(received["tool_choice"], "auto");
    assert_eq!(received["parallel_tool_calls"], true);
    assert_eq!(received["custom_unknown_field"]["keep"], true);
    // No defaults are injected.
    assert!(received.get("temperature").is_none());
    assert!(received.get("top_p").is_none());

    let records = wait_records(&dir, 1).await;
    assert_eq!(records.len(), 1);
    let r = &records[0];
    assert_eq!(r.protocol, "responses");
    assert_eq!(r.status, 200);
    assert!(!r.stream);
    assert_eq!(r.prompt_tokens, 11);
    assert_eq!(r.completion_tokens, 22);
    assert_eq!(r.cached_tokens, 3);
    assert_eq!(r.reasoning_tokens, 5);
    assert!(!r.quirks.iter().any(|q| q == "usage_missing"));
}

#[tokio::test]
async fn responses_non_stream_without_usage_records_usage_missing() {
    let dir = temp_dir("resp-nousage");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let response = app
        .oneshot(responses_post(responses_body(false), "nousage"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let value = body_json(response).await;
    assert_eq!(value["status"], "completed");

    let r = wait_until(&dir, |_| true, std::time::Duration::from_secs(5))
        .await
        .expect("ledger row");
    assert_eq!(r.protocol, "responses");
    assert_eq!(r.prompt_tokens, 0);
    assert!(r.quirks.iter().any(|q| q == "usage_missing"));
}

#[tokio::test]
async fn responses_non_stream_failed_body_is_passed_through() {
    let dir = temp_dir("resp-failed");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let response = app
        .oneshot(responses_post(responses_body(false), "failed"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let value = body_json(response).await;
    assert_eq!(value["status"], "failed");
    assert_eq!(value["error"]["code"], "server_error");
    assert!(value.get("choices").is_none());
}

#[tokio::test]
async fn responses_stream_adds_missing_event_lines() {
    let dir = temp_dir("resp-sse");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let response = app
        .oneshot(responses_post(responses_body(true), "sse"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "text/event-stream"
    );
    assert!(response.headers().get("content-length").is_none());
    assert!(
        response.headers().get("x-umsp-provider").is_none(),
        "provider header must not leak to clients"
    );

    let text = String::from_utf8(body_bytes(response).await).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        if line.starts_with("data: {") {
            let prev = if i == 0 { "" } else { lines[i - 1] };
            assert!(
                prev.starts_with("event: "),
                "data line without a preceding event line: {line}"
            );
        }
    }
    assert_eq!(text.matches("event: ").count(), 8);
    assert_eq!(text.matches("data: [DONE]").count(), 1);
    assert!(text.ends_with("data: [DONE]\n\n"));
    // Removing the inserted event lines gives back the upstream bytes exactly.
    let stripped: String = lines
        .iter()
        .filter(|line| !line.starts_with("event: "))
        .map(|line| format!("{line}\n"))
        .collect();
    assert_eq!(stripped, RESPONSES_SSE_DATA_ONLY);

    let r = wait_until(&dir, |_| true, std::time::Duration::from_secs(5))
        .await
        .expect("ledger row");
    assert_eq!(r.protocol, "responses");
    assert!(r.stream);
    assert_eq!(r.status, 200);
    assert_eq!(r.prompt_tokens, 11);
    assert_eq!(r.completion_tokens, 22);
    assert_eq!(r.cached_tokens, 3);
    assert_eq!(r.reasoning_tokens, 5);
    assert!(r.ttft_ms.is_some(), "ttft_ms must be recorded");
    assert_eq!(r.bytes_out, text.len() as u64);
    assert!(!r.client_disconnected);
    assert!(r.error_code.is_none());
}

#[tokio::test]
async fn responses_stream_keeps_existing_event_lines() {
    let dir = temp_dir("resp-sse-events");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let text = String::from_utf8(
        body_bytes(
            app.oneshot(responses_post(responses_body(true), "sse-events"))
                .await
                .unwrap(),
        )
        .await,
    )
    .unwrap();
    assert_eq!(text, RESPONSES_SSE_WITH_EVENTS);
}

#[tokio::test]
async fn responses_event_lines_can_be_disabled() {
    let dir = temp_dir("resp-sse-off");
    let (app, _, _) = gateway_with(&dir, 50 * 1024 * 1024, false).await;
    let text = String::from_utf8(
        body_bytes(
            app.oneshot(responses_post(responses_body(true), "sse"))
                .await
                .unwrap(),
        )
        .await,
    )
    .unwrap();
    assert_eq!(text, RESPONSES_SSE_DATA_ONLY);
    assert!(!text.contains("event: "));
}

#[tokio::test]
async fn responses_stream_keepalive_fills_upstream_silence() {
    let dir = temp_dir("resp-sse-ka");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let text = String::from_utf8(
        body_bytes(
            app.oneshot(responses_post(responses_body(true), "sse-slow"))
                .await
                .unwrap(),
        )
        .await,
    )
    .unwrap();
    assert!(
        text.matches(": keep-alive\n\n").count() >= 1,
        "expected keep-alive comments: {text:?}"
    );
    assert_eq!(
        text.replace(": keep-alive\n\n", ""),
        RESPONSES_SSE_WITH_EVENTS
    );
}

#[tokio::test]
async fn responses_stream_upstream_error_after_headers_emits_error_event() {
    let dir = temp_dir("resp-sse-err");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let response = app
        .oneshot(responses_post(responses_body(true), "sse-error"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let text = String::from_utf8(body_bytes(response).await).unwrap();
    assert!(
        text.starts_with("event: response.created\ndata: {\"type\":\"response.created\""),
        "partial upstream event must be relayed: {text:?}"
    );
    assert!(text.contains("event: error\ndata: {\"type\":\"error\""));
    assert!(text.contains("\"code\":\"upstream_stream_error\""));
    assert!(text.ends_with("data: [DONE]\n\n"));
    // No forged success.
    assert!(!text.contains("response.completed"));

    let r = wait_until(&dir, |_| true, std::time::Duration::from_secs(5))
        .await
        .expect("ledger row");
    assert_eq!(r.protocol, "responses");
    assert_eq!(r.status, 200);
    assert_eq!(r.error_code.as_deref(), Some("upstream_stream_error"));
    assert!(r.ttft_ms.is_some());
}

#[tokio::test]
async fn responses_stream_upstream_error_before_headers_is_relayed_verbatim() {
    let dir = temp_dir("resp-401");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let response = app
        .oneshot(responses_post(responses_body(true), "error401"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "application/json"
    );
    assert_eq!(
        response.headers().get("x-umsp-code").unwrap(),
        "upstream_auth_failed"
    );
    let value = body_json(response).await;
    assert_eq!(
        value["error"]["message"],
        "the gateway could not authenticate with the upstream service"
    );

    let r = wait_until(&dir, |_| true, std::time::Duration::from_secs(5))
        .await
        .expect("ledger row");
    assert_eq!(r.protocol, "responses");
    assert!(r.stream);
    assert_eq!(r.status, 502);
    assert_eq!(r.upstream_status, Some(401));
    assert_eq!(r.error_code.as_deref(), Some("upstream_401"));
}

#[tokio::test]
async fn responses_previous_response_id_is_400_unsupported_parameter() {
    let dir = temp_dir("resp-prev");
    let (app, _, rec) = gateway(&dir, 50 * 1024 * 1024).await;
    let body = json!({
        "model": "upstream-model",
        "input": "hello",
        "previous_response_id": "resp_does_not_exist"
    });
    let response = app.oneshot(responses_post(body, "normal")).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        response.headers().get("x-umsp-code").unwrap(),
        "unsupported_parameter"
    );
    let value = body_json(response).await;
    assert_eq!(value["error"]["code"], "unsupported_parameter");
    assert_eq!(value["error"]["type"], "invalid_request_error");
    assert_eq!(
        value["error"]["message"],
        "previous_response_id is not supported by this gateway; send the full input history"
    );
    // Rejected before any upstream call.
    assert!(rec.last().is_null());

    let r = wait_until(&dir, |_| true, std::time::Duration::from_secs(5))
        .await
        .expect("ledger row");
    assert_eq!(r.protocol, "responses");
    assert_eq!(r.status, 400);
    assert_eq!(r.error_code.as_deref(), Some("unsupported_parameter"));
}

#[tokio::test]
async fn responses_unsupported_endpoints_are_501_and_chat_survives() {
    let dir = temp_dir("resp-501");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;

    for (method, uri) in [
        ("POST", "/v1/responses/input_tokens"),
        ("GET", "/v1/responses/resp_123"),
        ("POST", "/v1/responses/resp_123/cancel"),
    ] {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("Bearer {TEST_KEY}"))
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::NOT_IMPLEMENTED,
            "{method} {uri} must be 501"
        );
        assert_eq!(
            response.headers().get("x-umsp-code").unwrap(),
            "unsupported_endpoint"
        );
        let value = body_json(response).await;
        assert_eq!(value["error"]["code"], "unsupported_endpoint");
        assert_eq!(value["error"]["type"], "invalid_request_error");
    }

    // The wildcard must not swallow the real Responses endpoint or Chat.
    let response = app
        .clone()
        .oneshot(responses_post(responses_body(false), "normal"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let response = app.oneshot(chat_request("normal")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn responses_oversized_body_is_413() {
    let dir = temp_dir("resp-413");
    let (app, _, _) = gateway(&dir, 64).await;
    let body = json!({"model": "upstream-model", "input": "x".repeat(4096)});
    let response = app.oneshot(responses_post(body, "normal")).await.unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        response.headers().get("x-umsp-code").unwrap(),
        "request_too_large"
    );
}

#[tokio::test]
async fn responses_streaming_client_disconnect_is_recorded() {
    let dir = temp_dir("resp-disc");
    let (upstream, _rec) = start_upstream().await;
    let cfg = config(&dir, upstream, 50 * 1024 * 1024);
    let state = AppState::new(cfg, env_file(&dir)).unwrap();
    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let body = serde_json::to_string(&json!({
        "model": "upstream-model",
        "input": "hi",
        "stream": true
    }))
    .unwrap();
    let request = format!(
        "POST /v1/responses HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {TEST_KEY}\r\nContent-Type: application/json\r\nx-test-mode: sse-endless\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let mut sock = tokio::net::TcpStream::connect(addr).await.unwrap();
    sock.write_all(request.as_bytes()).await.unwrap();
    let mut buf = [0u8; 1024];
    let n = tokio::time::timeout(std::time::Duration::from_secs(5), sock.read(&mut buf))
        .await
        .expect("first bytes")
        .expect("read");
    assert!(n > 0);
    drop(sock);

    let record = wait_until(
        &dir,
        |r| r.client_disconnected,
        std::time::Duration::from_secs(10),
    )
    .await
    .expect("ledger row with client_disconnected=true");
    assert_eq!(record.protocol, "responses");
    assert_eq!(record.status, 200);
    assert!(record.stream);
}

// ---------------------------------------------------------------------------
// M3: Anthropic Messages
// ---------------------------------------------------------------------------

/// A full Anthropic stream: `message_start` with usage, `ping`, thinking text +
/// signature deltas, a `tool_use` block with `input_json_delta`, `message_delta`
/// with the final output count and `message_stop`.
const MESSAGES_SSE: &str = concat!(
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
const MESSAGES_SSE_NO_STOP: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"upstream-model\",\"content\":[],\"usage\":{\"input_tokens\":21,\"output_tokens\":1}}}\n\n",
    "event: ping\n",
    "data: {\"type\":\"ping\"}\n\n",
);

/// The upstream itself reports an error mid-stream; its body must be relayed as-is.
const MESSAGES_SSE_UPSTREAM_ERROR: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"usage\":{\"input_tokens\":9,\"output_tokens\":1}}}\n\n",
    "event: error\n",
    "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n",
);

/// DeepSeek's Anthropic-compatible stream: `cache_creation_input_tokens` is
/// flat (spec 2.0 / 2.3) instead of OpenDesign's nested `cache_creation`.
const DS_MESSAGES_SSE: &str = concat!(
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

async fn sse_messages_error_response() -> axum::response::Response {
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

async fn sse_messages_endless() -> axum::response::Response {
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

fn messages_non_stream_body(with_usage: bool) -> Value {
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

async fn fake_messages(
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

async fn fake_count_tokens(
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

fn messages_body(stream: bool) -> Value {
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

fn messages_post(body: Value, mode: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/messages?beta=true")
        .header("authorization", format!("Bearer {TEST_KEY}"))
        .header("content-type", "application/json")
        .header("x-test-mode", mode)
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

#[tokio::test]
async fn messages_non_stream_relays_body_verbatim_and_records_usage() {
    let dir = temp_dir("msg-nonstream");
    let (app, _, rec) = gateway(&dir, 50 * 1024 * 1024).await;

    let response = app
        .oneshot(messages_post(messages_body(false), "normal"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "application/json"
    );
    assert!(
        response.headers().get("x-umsp-provider").is_none(),
        "provider header must not leak to clients"
    );
    assert!(
        response.headers().get("x-umsp-upstream-model").is_none(),
        "upstream model header must not leak"
    );
    assert!(response.headers().get("x-umsp-request-id").is_some());
    let value = body_json(response).await;

    // Anthropic content blocks survive untouched, including thinking and tool_use.
    let types: Vec<&str> = value["content"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        types,
        ["thinking", "redacted_thinking", "text", "tool_use"],
        "content blocks must be relayed in order"
    );
    assert_eq!(value["stop_reason"], "tool_use");
    assert_eq!(
        value["usage"]["cache_creation"]["ephemeral_5m_input_tokens"],
        3
    );

    // Only `model` changed upstream; everything else is byte-identical in shape.
    let received = &rec.last();
    assert_eq!(received["model"], "upstream-model");
    assert_eq!(received["max_tokens"], 64);
    assert_eq!(received["system"][0]["text"], "be brief");
    assert_eq!(received["system"][0]["cache_control"]["type"], "ephemeral");
    assert_eq!(
        received["messages"][0]["content"][0]["cache_control"]["type"],
        "ephemeral"
    );
    assert_eq!(received["tools"][0]["input_schema"]["type"], "object");
    assert_eq!(received["tool_choice"]["type"], "auto");
    assert_eq!(received["thinking"]["budget_tokens"], 1024);
    assert_eq!(received["metadata"]["user_id"], "u1");
    assert_eq!(received["stop_sequences"][0], "STOP");
    assert_eq!(received["temperature"], 0.3);
    assert_eq!(received["top_p"], 0.9);
    assert_eq!(received["top_k"], 5);
    assert_eq!(received["output_config"]["effort"], "high");
    assert_eq!(received["context_management"]["edits"], json!([]));
    assert_eq!(received["custom_beta_field"]["keep"], true);
    assert_eq!(received["stream"], false);
    assert!(
        received.get("max_output_tokens").is_none(),
        "the gateway must not add defaults"
    );

    let records = wait_records(&dir, 1).await;
    assert_eq!(records.len(), 1);
    let r = &records[0];
    assert_eq!(r.protocol, "messages");
    assert!(!r.stream);
    assert_eq!(r.status, 200);
    assert_eq!(r.upstream_status, Some(200));
    assert_eq!(r.prompt_tokens, 21);
    assert_eq!(r.completion_tokens, 9);
    assert_eq!(r.cached_tokens, 4);
    assert!(r.ttft_ms.is_none());
    assert!(r.bytes_out > 0);
    assert!(r.error_code.is_none());
    assert!(!r.quirks.iter().any(|q| q == "usage_missing"));
}

#[tokio::test]
async fn messages_non_stream_without_usage_records_usage_missing() {
    let dir = temp_dir("msg-nousage");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let response = app
        .oneshot(messages_post(messages_body(false), "nousage"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let value = body_json(response).await;
    assert_eq!(value["type"], "message");
    assert!(value.get("usage").is_none());

    let records = wait_records(&dir, 1).await;
    assert_eq!(records[0].protocol, "messages");
    assert_eq!(records[0].prompt_tokens, 0);
    assert!(records[0].quirks.iter().any(|q| q == "usage_missing"));
}

#[tokio::test]
async fn messages_query_and_headers_are_relayed_with_default_version() {
    let dir = temp_dir("msg-headers");
    let (app, _, rec) = gateway(&dir, 50 * 1024 * 1024).await;

    let request = Request::builder()
        .method("POST")
        .uri("/v1/messages?beta=true")
        .header("authorization", format!("Bearer {TEST_KEY}"))
        .header("x-api-key", TEST_KEY)
        .header("content-type", "application/json")
        .header(
            "anthropic-beta",
            "fine-grained-tool-streaming-2025-05-14,interleaved-thinking-2025-05-14",
        )
        .header("x-stainless-lang", "js")
        .header("x-claude-code-session-id", "sess-42")
        .body(Body::from(
            serde_json::to_vec(&messages_body(false)).unwrap(),
        ))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    assert_eq!(
        rec.last_uri(),
        "/v1/messages?beta=true",
        "query string must survive to the upstream"
    );
    let up = rec.last_headers();
    // Missing on the client side -> the gateway injects the OD-required version.
    assert_eq!(up.get("anthropic-version").unwrap(), "2023-06-01");
    assert_eq!(
        up.get("anthropic-beta").unwrap(),
        "fine-grained-tool-streaming-2025-05-14,interleaved-thinking-2025-05-14",
        "beta values are relayed verbatim, never whitelisted"
    );
    assert_eq!(up.get("x-stainless-lang").unwrap(), "js");
    assert_eq!(up.get("x-claude-code-session-id").unwrap(), "sess-42");
    // Client credentials never leak; the provider key is injected instead.
    assert_eq!(up.get("authorization").unwrap(), "Bearer sk-test");
    assert!(up.get("x-api-key").is_none());
    assert_eq!(up.get_all("content-type").iter().count(), 1);
    assert_eq!(up.get("accept-encoding").unwrap(), "identity");
}

#[tokio::test]
async fn messages_client_anthropic_version_is_not_overwritten() {
    let dir = temp_dir("msg-version");
    let (app, _, rec) = gateway(&dir, 50 * 1024 * 1024).await;
    let request = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("authorization", format!("Bearer {TEST_KEY}"))
        .header("content-type", "application/json")
        .header("anthropic-version", "2023-01-01")
        .body(Body::from(
            serde_json::to_vec(&messages_body(false)).unwrap(),
        ))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        rec.last_headers().get("anthropic-version").unwrap(),
        "2023-01-01"
    );
    assert_eq!(rec.last_uri(), "/v1/messages");
}

#[tokio::test]
async fn messages_stream_relays_event_sequence_verbatim() {
    let dir = temp_dir("msg-sse");
    let (app, _, rec) = gateway(&dir, 50 * 1024 * 1024).await;

    let response = app
        .oneshot(messages_post(messages_body(true), "sse"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "text/event-stream"
    );
    assert!(response.headers().get("content-length").is_none());
    let bytes = body_bytes(response).await;
    assert_eq!(
        String::from_utf8(bytes).unwrap(),
        MESSAGES_SSE,
        "the event sequence must be relayed verbatim, ping included"
    );
    assert_eq!(rec.last()["stream"], true);

    let records = wait_records(&dir, 1).await;
    let r = &records[0];
    assert_eq!(r.protocol, "messages");
    assert!(r.stream);
    assert_eq!(r.status, 200);
    assert_eq!(r.prompt_tokens, 21);
    assert_eq!(r.completion_tokens, 17);
    assert_eq!(r.cached_tokens, 4);
    assert!(r.ttft_ms.is_some(), "ttft_ms from the first data event");
    assert_eq!(r.bytes_out, MESSAGES_SSE.len() as u64);
    assert!(!r.client_disconnected);
    assert!(r.error_code.is_none());
}

#[tokio::test]
async fn messages_stream_truncated_without_message_stop_emits_error_event() {
    let dir = temp_dir("msg-nostop");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let bytes = body_bytes(
        app.oneshot(messages_post(messages_body(true), "sse-nostop"))
            .await
            .unwrap(),
    )
    .await;
    let text = String::from_utf8(bytes).unwrap();
    assert!(text.starts_with(MESSAGES_SSE_NO_STOP));
    assert!(text.contains("event: error\ndata: "));
    assert!(
        !text.contains("message_stop"),
        "the gateway must never fabricate message_stop"
    );
    assert!(!text.contains("[DONE]"), "Messages has no [DONE] sentinel");

    let r = wait_until(&dir, |_| true, std::time::Duration::from_secs(5))
        .await
        .expect("ledger row");
    assert_eq!(r.error_code.as_deref(), Some("upstream_stream_error"));
}

#[tokio::test]
async fn messages_stream_keepalive_is_a_ping_event() {
    let dir = temp_dir("msg-keepalive");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let bytes = body_bytes(
        app.oneshot(messages_post(messages_body(true), "sse-slow"))
            .await
            .unwrap(),
    )
    .await;
    let text = String::from_utf8(bytes).unwrap();
    let pings = text
        .matches("event: ping\ndata: {\"type\":\"ping\"}\n\n")
        .count();
    assert!(pings >= 2, "upstream ping + gateway keepalive: {text:?}");
    let strip = |s: &str| s.replace("event: ping\ndata: {\"type\":\"ping\"}\n\n", "");
    assert_eq!(
        strip(&text),
        strip(MESSAGES_SSE),
        "keepalive must not disturb the relayed sequence"
    );
}

#[tokio::test]
async fn messages_stream_upstream_error_event_is_passed_through() {
    let dir = temp_dir("msg-upstream-error");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let bytes = body_bytes(
        app.oneshot(messages_post(messages_body(true), "sse-error-event"))
            .await
            .unwrap(),
    )
    .await;
    let text = String::from_utf8(bytes).unwrap();
    assert_eq!(text, MESSAGES_SSE_UPSTREAM_ERROR);
    // No extra error event and no forged terminator.
    assert_eq!(text.matches("event: error").count(), 1);
    assert!(!text.contains("message_stop"));
    assert!(!text.contains("[DONE]"));

    let r = wait_until(&dir, |_| true, std::time::Duration::from_secs(5))
        .await
        .expect("ledger row");
    assert!(r.stream);
    assert_eq!(r.status, 200);
}

#[tokio::test]
async fn messages_stream_upstream_connection_error_emits_error_event() {
    let dir = temp_dir("msg-sse-error");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let response = app
        .oneshot(messages_post(messages_body(true), "sse-error"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let text = String::from_utf8(body_bytes(response).await).unwrap();
    assert!(text.starts_with("event: message_start\n"));
    assert!(text.contains("event: error\ndata: "));
    assert!(text.contains("\"type\":\"upstream_error\""));
    assert!(!text.contains("message_stop"));
    assert!(!text.contains("[DONE]"));

    let r = wait_until(&dir, |_| true, std::time::Duration::from_secs(5))
        .await
        .expect("ledger row");
    assert_eq!(r.error_code.as_deref(), Some("upstream_stream_error"));
}

#[tokio::test]
async fn messages_stream_client_disconnect_is_recorded() {
    let dir = temp_dir("msg-disc");
    let (upstream, _rec) = start_upstream().await;
    let cfg = config(&dir, upstream, 50 * 1024 * 1024);
    let state = AppState::new(cfg, env_file(&dir)).unwrap();
    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let body = serde_json::to_string(&messages_body(true)).unwrap();
    let request = format!(
        "POST /v1/messages?beta=true HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {TEST_KEY}\r\ncontent-type: application/json\r\nx-test-mode: sse-endless\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let mut sock = tokio::net::TcpStream::connect(addr).await.unwrap();
    sock.write_all(request.as_bytes()).await.unwrap();
    let mut buf = [0u8; 1024];
    let n = tokio::time::timeout(std::time::Duration::from_secs(5), sock.read(&mut buf))
        .await
        .expect("first bytes")
        .expect("read");
    assert!(n > 0);
    drop(sock);

    let record = wait_until(
        &dir,
        |r| r.client_disconnected,
        std::time::Duration::from_secs(10),
    )
    .await
    .expect("ledger row with client_disconnected=true");
    assert_eq!(record.protocol, "messages");
    assert_eq!(record.status, 200);
    assert!(record.stream);
}

#[tokio::test]
async fn messages_disconnect_before_headers_still_logs_499() {
    let dir = temp_dir("msg-disc-early");
    let (upstream, _rec) = start_upstream().await;
    let cfg = config(&dir, upstream, 50 * 1024 * 1024);
    let state = AppState::new(cfg, env_file(&dir)).unwrap();
    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    use tokio::io::AsyncWriteExt;
    let body = serde_json::to_string(&messages_body(true)).unwrap();
    let request = format!(
        "POST /v1/messages HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {TEST_KEY}\r\ncontent-type: application/json\r\nx-test-mode: sse-delay\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let mut sock = tokio::net::TcpStream::connect(addr).await.unwrap();
    sock.write_all(request.as_bytes()).await.unwrap();
    drop(sock); // gone before upstream headers arrive

    let record = wait_until(
        &dir,
        |r| r.client_disconnected,
        std::time::Duration::from_secs(10),
    )
    .await
    .expect("a 499 row must be written instead of losing the request");
    assert_eq!(record.protocol, "messages");
    assert_eq!(record.status, 499);
    assert_eq!(record.error_code.as_deref(), Some("client_disconnected"));
    assert!(record.stream);
}

#[tokio::test]
async fn messages_upstream_401_becomes_a_neutral_gateway_error() {
    let dir = temp_dir("msg-401");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let response = app
        .oneshot(messages_post(messages_body(true), "error401"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(
        response.headers().get("x-umsp-code").unwrap(),
        "upstream_auth_failed"
    );
    assert!(
        response.headers().get("x-request-id").is_none(),
        "upstream headers must not leak"
    );
    assert!(response.headers().get("x-should-retry").is_none());
    assert!(response.headers().get("retry-after").is_none());
    let value = body_json(response).await;
    assert_eq!(value["type"], "error");
    assert_eq!(value["error"]["type"], "api_error");
    assert_eq!(
        value["error"]["message"],
        "the gateway could not authenticate with the upstream service"
    );

    let r = wait_until(&dir, |_| true, std::time::Duration::from_secs(5))
        .await
        .expect("ledger row");
    assert_eq!(r.status, 502);
    assert_eq!(r.upstream_status, Some(401));
    assert_eq!(r.error_code.as_deref(), Some("upstream_401"));
}

#[tokio::test]
async fn messages_count_tokens_is_501_not_supported() {
    let dir = temp_dir("msg-count");
    let (app, _, rec) = gateway(&dir, 50 * 1024 * 1024).await;
    let body = json!({
        "model": "upstream-model",
        "messages": [{"role": "user", "content": "hello"}]
    });
    let request = Request::builder()
        .method("POST")
        .uri("/v1/messages/count_tokens")
        .header("authorization", format!("Bearer {TEST_KEY}"))
        .header("x-api-key", TEST_KEY)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    assert_eq!(
        response.headers().get("x-umsp-code").unwrap(),
        "not_supported_error"
    );
    let value = body_json(response).await;
    assert_eq!(
        value,
        json!({
            "type": "error",
            "error": {
                "type": "not_supported_error",
                "message": "count_tokens is not supported by any backend of model 'upstream-model'"
            }
        })
    );
    assert!(
        rec.seen.lock().unwrap().is_empty(),
        "count_tokens must not be forwarded to the OD 404"
    );
}

#[tokio::test]
async fn messages_gateway_errors_use_anthropic_shape() {
    let dir = temp_dir("msg-errors");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;

    // No key -> 401 in Anthropic shape.
    let request = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let value = body_json(response).await;
    assert_eq!(value["type"], "error");
    assert_eq!(value["error"]["type"], "authentication_error");

    // Unknown model -> 404 not_found_error, still Anthropic shape.
    let body = json!({"model": "od/nope", "messages": []});
    let request = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("authorization", format!("Bearer {TEST_KEY}"))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let value = body_json(response).await;
    assert_eq!(value["type"], "error");
    assert_eq!(value["error"]["type"], "not_found_error");
}

#[tokio::test]
async fn messages_upstream_400_body_is_passed_through() {
    let dir = temp_dir("msg-400");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let response = app
        .oneshot(messages_post(messages_body(false), "error400"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let value = body_json(response).await;
    assert_eq!(value["type"], "error");
    assert_eq!(value["error"]["type"], "invalid_request_error");
    assert_eq!(value["error"]["message"], "max_tokens: field required");
}

// ---------------------------------------------------------------------------
// M4a: DeepSeek provider
// ---------------------------------------------------------------------------

/// A DS Chat request that replays an OpenDesign-style history: the assistant
/// turn carries `reasoning` + `reasoning_details` but no `reasoning_content`.
fn ds_chat_body() -> Value {
    json!({
        "model": "deepseek-flash",
        "messages": [
            {"role": "user", "content": "run f"},
            {"role": "assistant", "content": null, "reasoning": "I should call f",
             "reasoning_details": [{"type": "reasoning.encrypted", "data": "abc"}],
             "tool_calls": [{"id": "call_1", "type": "function",
                             "function": {"name": "f", "arguments": "{}"}}]},
            {"role": "tool", "tool_call_id": "call_1", "content": "ok"},
            {"role": "assistant", "content": "done"}
        ],
        "tools": [{"type": "function", "function": {"name": "f",
                    "parameters": {"type": "object", "properties": {}}}}],
        "thinking": {"type": "enabled"},
        "reasoning_effort": "max",
        "custom_unknown_field": {"keep": true}
    })
}

#[tokio::test]
async fn ds_chat_backfills_reasoning_content_and_drops_reasoning_details() {
    let dir = temp_dir("ds-chat");
    let (app, rec) = ds_gateway(&dir).await;
    let response = app
        .clone()
        .oneshot(post_to("/v1/chat/completions", ds_chat_body(), "normal"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let seen = rec.last();
    // The alias resolved to the DS model and only the upstream id went out.
    assert_eq!(seen["model"], "deepseek-flash");
    // `reasoning` text backfills the required field; the bare assistant turn
    // gets a single space instead of a 400.
    assert_eq!(seen["messages"][1]["reasoning_content"], "I should call f");
    assert_eq!(seen["messages"][3]["reasoning_content"], " ");
    // OD's encrypted reasoning is OpenDesign-only.
    assert!(seen["messages"][1].get("reasoning_details").is_none());
    // Unknown fields, thinking and reasoning_effort pass through untouched.
    assert_eq!(seen["custom_unknown_field"]["keep"], true);
    assert_eq!(seen["thinking"]["type"], "enabled");
    assert_eq!(seen["reasoning_effort"], "max");
    // A stable per-client user_id is injected for KVCache isolation.
    let user_id = seen["user_id"]
        .as_str()
        .expect("user_id must be injected for DS");
    assert!(user_id.starts_with("umsp-"), "got {user_id}");

    let record = wait_until(
        &dir,
        |r| r.protocol == "chat",
        std::time::Duration::from_secs(10),
    )
    .await
    .expect("chat ledger row");
    assert_eq!(record.provider, "deepseek");
    assert_eq!(record.upstream_model, "deepseek-flash");
    assert_eq!(record.status, 200);
    assert!(
        record
            .quirks
            .contains(&"filled_reasoning_content".to_string())
    );
    assert!(
        record
            .quirks
            .contains(&"filled_reasoning_placeholder".to_string())
    );
    assert!(
        record
            .quirks
            .contains(&"dropped_reasoning_details".to_string())
    );
    assert!(record.quirks.contains(&"injected_user_id".to_string()));
}

#[tokio::test]
async fn ds_chat_user_id_is_stable_and_an_explicit_one_wins() {
    let dir = temp_dir("ds-user-id");
    let (app, rec) = ds_gateway(&dir).await;
    for _ in 0..2 {
        let response = app
            .clone()
            .oneshot(post_to("/v1/chat/completions", ds_chat_body(), "normal"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
    let seen = rec.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0]["user_id"], seen[1]["user_id"], "must be stable");
    let injected = seen[0]["user_id"].clone();

    let mut explicit = ds_chat_body();
    explicit["user_id"] = json!("client-picked");
    let response = app
        .oneshot(post_to("/v1/chat/completions", explicit, "normal"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(rec.last()["user_id"], "client-picked");
    assert_ne!(rec.last()["user_id"], injected);
}

#[tokio::test]
async fn ds_chat_without_tools_does_not_invent_reasoning_content() {
    let dir = temp_dir("ds-no-tools");
    let (app, rec) = ds_gateway(&dir).await;
    let mut body = ds_chat_body();
    body.as_object_mut().unwrap().remove("tools");
    let response = app
        .oneshot(post_to("/v1/chat/completions", body, "normal"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let seen = rec.last();
    assert!(seen["messages"][1].get("reasoning_content").is_none());
    assert!(seen["messages"][3].get("reasoning_content").is_none());
    assert!(seen["messages"][1].get("reasoning_details").is_none());
}

#[tokio::test]
async fn ds_chat_stream_is_forwarded_and_recorded() {
    let dir = temp_dir("ds-chat-stream");
    let (app, rec) = ds_gateway(&dir).await;
    let mut body = ds_chat_body();
    body["stream"] = json!(true);
    let response = app
        .oneshot(post_to("/v1/chat/completions", body, "sse"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response.headers().get("x-umsp-provider").is_none(),
        "provider header must not leak to clients"
    );
    let bytes = body_bytes(response).await;
    let text = String::from_utf8(bytes).unwrap();
    assert!(text.contains("data: [DONE]"));
    // The upstream got the DS quirks on the streaming path too.
    assert_eq!(
        rec.last()["messages"][1]["reasoning_content"],
        "I should call f"
    );
    assert_eq!(rec.last()["stream"], true);

    let record = wait_until(
        &dir,
        |r| r.protocol == "chat" && r.stream,
        std::time::Duration::from_secs(10),
    )
    .await
    .expect("streaming chat ledger row");
    assert_eq!(record.provider, "deepseek");
    assert!(record.ttft_ms.is_some(), "ttft_ms must be recorded");
    assert_eq!(record.prompt_tokens, 11);
    assert_eq!(record.completion_tokens, 22);
}

#[tokio::test]
async fn ds_responses_non_stream_relays_body_and_records_usage() {
    let dir = temp_dir("ds-resp");
    let (app, rec) = ds_gateway(&dir).await;
    let mut body = responses_body(false);
    body["model"] = json!("deepseek-flash");
    let response = app
        .oneshot(post_to("/v1/responses", body, "normal"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response.headers().get("x-umsp-provider").is_none(),
        "provider header must not leak to clients"
    );
    assert!(
        response.headers().get("x-umsp-upstream-model").is_none(),
        "upstream model header must not leak"
    );
    let value = body_json(response).await;
    assert_eq!(value["object"], "response");
    assert_eq!(value["output"][0]["type"], "message");
    // Only `model` is rewritten; everything else is relayed as-is.
    assert_eq!(rec.last()["model"], "deepseek-flash");
    assert_eq!(rec.last()["instructions"], "be brief");
    assert_eq!(rec.last()["custom_unknown_field"]["keep"], true);

    let record = wait_until(
        &dir,
        |r| r.protocol == "responses",
        std::time::Duration::from_secs(10),
    )
    .await
    .expect("responses ledger row");
    assert_eq!(record.provider, "deepseek");
    assert_eq!(record.status, 200);
    assert_eq!(record.prompt_tokens, 11);
    assert_eq!(record.completion_tokens, 22);
    assert_eq!(record.cached_tokens, 3);
    assert_eq!(record.reasoning_tokens, 5);
}

#[tokio::test]
async fn ds_responses_stream_does_not_duplicate_event_lines() {
    let dir = temp_dir("ds-resp-stream");
    let (app, rec) = ds_gateway(&dir).await;
    let mut body = responses_body(true);
    body["model"] = json!("deepseek-flash");
    let response = app
        .oneshot(post_to("/v1/responses", body, "sse-events"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "text/event-stream"
    );
    assert!(
        response.headers().get("x-umsp-provider").is_none(),
        "provider header must not leak to clients"
    );
    let text = String::from_utf8(body_bytes(response).await).unwrap();
    // DeepSeek already sends `event:` lines; the patcher must not add a second one.
    assert_eq!(text.matches("event: response.created").count(), 1);
    assert_eq!(text.matches("event: response.output_text.delta").count(), 1);
    assert_eq!(text.matches("event: response.completed").count(), 1);
    assert_eq!(text.matches("event: ").count(), 3, "text was:\n{text}");
    assert!(text.ends_with("data: [DONE]\n\n"));
    assert_eq!(rec.last()["stream"], true);

    let record = wait_until(
        &dir,
        |r| r.protocol == "responses" && r.stream,
        std::time::Duration::from_secs(10),
    )
    .await
    .expect("streaming responses ledger row");
    assert_eq!(record.provider, "deepseek");
    // usage comes from `response.completed.response.usage`.
    assert_eq!(record.prompt_tokens, 11);
    assert_eq!(record.completion_tokens, 22);
    assert!(record.ttft_ms.is_some());
}

#[tokio::test]
async fn ds_responses_previous_response_id_is_still_400() {
    let dir = temp_dir("ds-resp-prev");
    let (app, _rec) = ds_gateway(&dir).await;
    let mut body = responses_body(false);
    body["model"] = json!("deepseek-flash");
    body["previous_response_id"] = json!("resp_does_not_exist");
    let response = app
        .oneshot(post_to("/v1/responses", body, "normal"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let value = body_json(response).await;
    assert_eq!(value["error"]["code"], "unsupported_parameter");
}

#[tokio::test]
async fn ds_messages_non_stream_relays_body_and_records_usage() {
    let dir = temp_dir("ds-msg");
    let (app, rec) = ds_gateway(&dir).await;
    let mut body = messages_body(false);
    body["model"] = json!("deepseek-flash");
    let response = app
        .oneshot(post_to("/v1/messages", body, "flat-cache"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response.headers().get("x-umsp-provider").is_none(),
        "provider header must not leak to clients"
    );
    assert!(
        response.headers().get("x-umsp-upstream-model").is_none(),
        "upstream model header must not leak"
    );
    let value = body_json(response).await;
    assert_eq!(value["type"], "message");
    // The DS Anthropic path, not the OpenAI one.
    assert_eq!(rec.last_uri(), "/anthropic/v1/messages");
    // Only `model` is rewritten and `anthropic-version` is filled in.
    assert_eq!(rec.last()["model"], "deepseek-flash");
    assert_eq!(rec.last()["thinking"]["type"], "enabled");
    assert_eq!(rec.last()["custom_beta_field"]["keep"], true);
    assert_eq!(
        rec.last_headers()
            .get("anthropic-version")
            .unwrap()
            .to_str()
            .unwrap(),
        "2023-06-01"
    );

    let record = wait_until(
        &dir,
        |r| r.protocol == "messages" && !r.stream,
        std::time::Duration::from_secs(10),
    )
    .await
    .expect("messages ledger row");
    assert_eq!(record.provider, "deepseek");
    assert_eq!(record.upstream_model, "deepseek-flash");
    // Flat `cache_creation_input_tokens` still leaves the shared fields intact.
    assert_eq!(record.prompt_tokens, 21);
    assert_eq!(record.completion_tokens, 9);
    assert_eq!(record.cached_tokens, 2);
}

#[tokio::test]
async fn ds_messages_stream_relays_events_and_reads_flat_usage() {
    let dir = temp_dir("ds-msg-stream");
    let (app, _rec) = ds_gateway(&dir).await;
    let mut body = messages_body(true);
    body["model"] = json!("deepseek-flash");
    let response = app
        .oneshot(post_to("/v1/messages?beta=true", body, "sse-flat"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "text/event-stream"
    );
    let text = String::from_utf8(body_bytes(response).await).unwrap();
    assert!(text.contains("event: message_start"));
    assert!(text.contains("event: message_stop"));
    assert_eq!(text.matches("event: ").count(), 7, "text was:\n{text}");

    let record = wait_until(
        &dir,
        |r| r.protocol == "messages" && r.stream,
        std::time::Duration::from_secs(10),
    )
    .await
    .expect("streaming messages ledger row");
    assert_eq!(record.provider, "deepseek");
    // message_start input/cache + message_delta output merge.
    assert_eq!(record.prompt_tokens, 21);
    assert_eq!(record.completion_tokens, 17);
    assert_eq!(record.cached_tokens, 4);
    assert!(record.ttft_ms.is_some());
}

#[tokio::test]
async fn ds_messages_query_string_is_preserved() {
    let dir = temp_dir("ds-msg-query");
    let (app, rec) = ds_gateway(&dir).await;
    let mut body = messages_body(false);
    body["model"] = json!("deepseek-flash");
    let response = app
        .oneshot(post_to("/v1/messages?beta=true", body, "normal"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(rec.last_uri(), "/anthropic/v1/messages?beta=true");
}

#[tokio::test]
async fn ds_count_tokens_is_proxied_to_the_anthropic_endpoint() {
    let dir = temp_dir("ds-count");
    let (app, rec) = ds_gateway(&dir).await;
    let body = json!({
        "model": "deepseek-flash",
        "messages": [{"role": "user", "content": "hello"}],
        "tools": [{"name": "Bash", "input_schema": {"type": "object"}}]
    });
    let response = app
        .oneshot(post_to("/v1/messages/count_tokens", body, "normal"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response.headers().get("x-umsp-provider").is_none(),
        "provider header must not leak to clients"
    );
    let value = body_json(response).await;
    assert_eq!(value["input_tokens"], 31);
    assert_eq!(rec.last_uri(), "/anthropic/v1/messages/count_tokens");
    // The body is relayed with only `model` rewritten; version defaulted.
    assert_eq!(rec.last()["model"], "deepseek-flash");
    assert_eq!(
        rec.last_headers()
            .get("anthropic-version")
            .unwrap()
            .to_str()
            .unwrap(),
        "2023-06-01"
    );

    let record = wait_until(
        &dir,
        |r| r.quirks.iter().any(|q| q == "count_tokens"),
        std::time::Duration::from_secs(10),
    )
    .await
    .expect("count_tokens ledger row");
    assert_eq!(record.protocol, "messages");
    assert_eq!(record.provider, "deepseek");
    assert_eq!(record.status, 200);
    assert_eq!(record.prompt_tokens, 31);
}

#[tokio::test]
async fn od_count_tokens_never_reaches_the_upstream() {
    let dir = temp_dir("od-count");
    let (app, _upstream, rec) = gateway(&dir, 50 * 1024 * 1024).await;
    let mut body = messages_body(false);
    body["model"] = json!("upstream-model");
    let response = app
        .oneshot(post_to("/v1/messages/count_tokens", body, "normal"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    let value = body_json(response).await;
    assert_eq!(value["error"]["type"], "not_supported_error");
    assert!(
        rec.uris.lock().unwrap().is_empty(),
        "OD count_tokens must not be forwarded"
    );
}

// ---------------------------------------------------------------------------
// M4b: fallback + circuit breaker
// ---------------------------------------------------------------------------

/// Per-model scripted upstream: `plan[model]` is a queue of HTTP statuses
/// (`200` = normal success) consumed in order; `hits` records every upstream
/// model called, in order.
#[derive(Clone, Default)]
struct ScriptedFake {
    plan: Arc<Mutex<HashMap<String, Vec<u16>>>>,
    hits: Arc<Mutex<Vec<String>>>,
    stream_dies: Arc<AtomicBool>,
    delay_ms: Arc<AtomicU64>,
}

impl ScriptedFake {
    fn queue(&self, model: &str, statuses: &[u16]) {
        self.plan
            .lock()
            .unwrap()
            .entry(model.to_string())
            .or_default()
            .extend_from_slice(statuses);
    }

    fn hits(&self) -> Vec<String> {
        self.hits.lock().unwrap().clone()
    }

    fn hits_of(&self, model: &str) -> usize {
        self.hits().iter().filter(|hit| *hit == model).count()
    }

    fn set_delay_ms(&self, ms: u64) {
        self.delay_ms.store(ms, Ordering::SeqCst);
    }

    fn die_in_stream(&self) {
        self.stream_dies.store(true, Ordering::SeqCst);
    }

    fn heal_stream(&self) {
        self.stream_dies.store(false, Ordering::SeqCst);
    }
}

async fn scripted_handler(
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

async fn start_scripted() -> (String, ScriptedFake) {
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
fn m4b_config(dir: &Path, od_base: String, ds_root: String, with_fallback: bool) -> Config {
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

async fn m4b_gateway_with(
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

async fn m4b_gateway(dir: &Path, with_fallback: bool) -> (Router, ScriptedFake) {
    m4b_gateway_with(dir, None, with_fallback).await
}

fn chat_model(model: &str, stream: bool) -> Request<Body> {
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

fn messages_model(model: &str) -> Request<Body> {
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

fn healthz_request() -> Request<Body> {
    Request::builder()
        .uri("/healthz")
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn fallback_triggers_on_retryable_statuses() {
    for status in [500u16, 429, 402, 503] {
        let dir = temp_dir(&format!("fb-{status}"));
        let (app, fake) = m4b_gateway(&dir, true).await;
        fake.queue("upstream-model", &[status]);

        let response = app
            .oneshot(chat_model("upstream-model", false))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "upstream {status} must fall back to the same protocol"
        );
        assert!(
            !response.headers().contains_key("x-umsp-provider"),
            "provider header must not leak to clients"
        );
        assert!(
            !response.headers().contains_key("x-umsp-upstream-model"),
            "upstream model header must not leak"
        );
        let value = body_json(response).await;
        assert_eq!(value["_served_by"], "fallback-model");
        assert_eq!(
            fake.hits(),
            vec!["upstream-model".to_string(), "fallback-model".to_string()]
        );

        // The ledger records the provider that actually answered, one hop.
        let records = wait_records(&dir, 1).await;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].status, 200);
        assert_eq!(records[0].provider, "deepseek");
        assert_eq!(records[0].upstream_model, "fallback-model");
        assert_eq!(records[0].fallbacks, 1);
        assert!(
            records[0]
                .quirks
                .iter()
                .any(|quirk| quirk == "fallback_from:od"),
            "missing fallback_from quirk: {:?}",
            records[0].quirks
        );
    }
}

#[tokio::test]
async fn fallback_triggers_on_connection_error() {
    let dir = temp_dir("fb-conn");
    // Nothing listens on port 1: the primary attempt fails before headers.
    let (app, fake) = m4b_gateway_with(&dir, Some("http://127.0.0.1:1/v1".into()), true).await;

    let response = app
        .oneshot(chat_model("upstream-model", false))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        !response.headers().contains_key("x-umsp-provider"),
        "provider header must not leak to clients"
    );
    assert_eq!(fake.hits(), vec!["fallback-model".to_string()]);
}

#[tokio::test]
async fn client_errors_do_not_fallback() {
    for status in [400u16, 404, 422] {
        let dir = temp_dir(&format!("fb-no-{status}"));
        let (app, fake) = m4b_gateway(&dir, true).await;
        fake.queue("upstream-model", &[status]);

        let response = app
            .oneshot(chat_model("upstream-model", false))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::from_u16(status).unwrap());
        let value = body_json(response).await;
        assert_eq!(
            value["error"]["message"],
            format!("upstream-model says {status}")
        );
        assert_eq!(
            fake.hits(),
            vec!["upstream-model".to_string()],
            "upstream {status} must not trigger a fallback"
        );
    }
}

#[tokio::test]
async fn failed_fallback_returns_the_last_upstream_error() {
    let dir = temp_dir("fb-both-fail");
    let (app, fake) = m4b_gateway(&dir, true).await;
    fake.queue("upstream-model", &[500]);
    fake.queue("fallback-model", &[502]);

    let response = app
        .oneshot(chat_model("upstream-model", false))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert!(
        !response.headers().contains_key("x-umsp-provider"),
        "provider header must not leak to clients"
    );
    let value = body_json(response).await;
    assert_eq!(
        value["error"]["message"],
        "the upstream service failed to handle the request"
    );
    assert_eq!(value["error"]["code"], "upstream_error");

    let records = wait_records(&dir, 1).await;
    assert_eq!(records[0].status, 502);
    assert_eq!(records[0].upstream_status, Some(502));
    assert_eq!(records[0].provider, "deepseek");
    assert_eq!(records[0].error_code.as_deref(), Some("upstream_502"));
    assert!(
        records[0]
            .upstream_body
            .as_deref()
            .unwrap_or("")
            .contains("fallback-model says 502"),
        "the raw upstream body stays available in the ledger"
    );
    assert_eq!(records[0].fallbacks, 1);
}

#[tokio::test]
async fn stream_failure_after_200_does_not_fallback() {
    let dir = temp_dir("fb-stream");
    let (app, fake) = m4b_gateway(&dir, true).await;
    fake.die_in_stream();

    let response = app
        .oneshot(chat_model("upstream-model", true))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let _ = body_bytes(response).await;
    assert_eq!(
        fake.hits(),
        vec!["upstream-model".to_string()],
        "a stream that already sent 200 must not switch provider"
    );
}

#[tokio::test]
async fn five_failures_open_the_circuit_and_reject_with_503() {
    let dir = temp_dir("circuit-open");
    let (app, fake) = m4b_gateway(&dir, false).await;
    fake.queue("upstream-model", &[500; 5]);

    for _ in 0..5 {
        let response = app
            .clone()
            .oneshot(chat_model("upstream-model", false))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    }

    let health = body_json(app.clone().oneshot(healthz_request()).await.unwrap()).await;
    assert_eq!(health["status"], "degraded");
    assert_eq!(health["providers"]["od"]["circuit"], "open");
    assert_eq!(health["providers"]["od"]["failures"], 5);
    assert!(health["providers"]["od"]["opened_at"].is_string());
    assert_eq!(health["providers"]["od"]["reason"], "http_500");

    let response = app
        .oneshot(chat_model("upstream-model", false))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.headers()["x-umsp-code"], "temporarily_unavailable");
    assert!(
        !response.headers().contains_key("x-umsp-provider"),
        "provider header must not leak to clients"
    );
    let value = body_json(response).await;
    assert_eq!(value["error"]["code"], "temporarily_unavailable");
    assert_eq!(
        fake.hits_of("upstream-model"),
        5,
        "an open circuit must not touch the upstream"
    );
}

#[tokio::test]
async fn not_found_never_opens_the_circuit() {
    let dir = temp_dir("circuit-404");
    let (app, fake) = m4b_gateway(&dir, false).await;
    fake.queue("upstream-model", &[404; 5]);

    for _ in 0..5 {
        let response = app
            .clone()
            .oneshot(chat_model("upstream-model", false))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
    let health = body_json(app.oneshot(healthz_request()).await.unwrap()).await;
    assert_eq!(health["providers"]["od"]["circuit"], "closed");
    assert_eq!(health["providers"]["od"]["failures"], 0);
}

#[tokio::test]
async fn a_spent_usage_window_skips_the_only_provider() {
    let dir = temp_dir("usage-spent");
    let (upstream, rec) = start_upstream().await;
    let cfg = config(&dir, upstream, 52_428_800);
    let state = AppState::new(cfg, env_file(&dir)).unwrap();
    let now = Instant::now();
    state.note_provider_usage(
        "od",
        UsageWindow {
            duration: Duration::from_secs(7 * 86400),
            remaining_fraction: 0.0,
            reset_at: Some(now + Duration::from_secs(3600)),
            observed_at: now,
        },
    );
    let app = build_router(state);

    let response = app
        .clone()
        .oneshot(chat_model("upstream-model", false))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let response = app
        .oneshot(chat_model("upstream-model", true))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        rec.seen.lock().unwrap().is_empty(),
        "a spent provider must not be called"
    );
}

#[tokio::test]
async fn half_open_probe_client_error_does_not_wedge_the_circuit() {
    let dir = temp_dir("circuit-probe-404");
    let (root, fake) = start_scripted().await;
    let cfg = m4b_config(&dir, format!("{root}/v1"), root.clone(), false);
    let state = AppState::new(cfg, env_file(&dir)).unwrap();
    let app = build_router(state.clone());

    // Open, then wait out the (shortened) cooldown so the next request probes.
    state
        .circuits()
        .force_open("od", "http_500", Duration::from_millis(40));
    tokio::time::sleep(Duration::from_millis(60)).await;
    fake.queue("upstream-model", &[404, 200]);

    // The probe reaches the upstream and gets a client error: relayed verbatim,
    // never counted as a circuit failure.
    let response = app
        .clone()
        .oneshot(chat_model("upstream-model", false))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(fake.hits_of("upstream-model"), 1);

    // The provider answered, so the probe slot was released and the circuit
    // closed instead of staying half_open with a leaked probe forever.
    let health = body_json(app.clone().oneshot(healthz_request()).await.unwrap()).await;
    assert_eq!(health["providers"]["od"]["circuit"], "closed");
    assert_eq!(health["providers"]["od"]["failures"], 0);

    // The next request is served, not a permanent 503.
    let response = app
        .oneshot(chat_model("upstream-model", false))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(fake.hits_of("upstream-model"), 2);
}

#[tokio::test]
async fn cancelled_half_open_probe_releases_the_slot() {
    let dir = temp_dir("circuit-probe-cancel");
    let (root, fake) = start_scripted().await;
    let cfg = m4b_config(&dir, format!("{root}/v1"), root.clone(), false);
    let state = AppState::new(cfg, env_file(&dir)).unwrap();
    let app = build_router(state.clone());

    state
        .circuits()
        .force_open("od", "http_500", Duration::from_millis(40));
    tokio::time::sleep(Duration::from_millis(60)).await;

    // The half-open probe hangs upstream; the client disconnects (the request
    // future is dropped) before any result is recorded.
    fake.set_delay_ms(300);
    let cancelled = tokio::time::timeout(
        Duration::from_millis(50),
        app.clone().oneshot(chat_model("upstream-model", false)),
    )
    .await;
    assert!(cancelled.is_err(), "the probe request must be cancelled");

    // The probe slot was released: the next request is a fresh probe and is
    // served instead of a permanent 503.
    fake.set_delay_ms(0);
    let response = app
        .clone()
        .oneshot(chat_model("upstream-model", false))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let health = body_json(app.oneshot(healthz_request()).await.unwrap()).await;
    assert_eq!(health["providers"]["od"]["circuit"], "closed");
    assert_eq!(health["providers"]["od"]["failures"], 0);
}

#[tokio::test]
async fn a_successful_provider_sticks_until_it_fails() {
    let dir = temp_dir("route-pin");
    let (app, fake) = m4b_gateway(&dir, true).await;
    fake.queue("upstream-model", &[500, 200]);
    fake.queue("fallback-model", &[200, 500]);

    // Primary fails, fallback answers, and the model pins the fallback.
    let response = app
        .clone()
        .oneshot(chat_model("upstream-model", false))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        !response.headers().contains_key("x-umsp-provider"),
        "provider header must not leak to clients"
    );

    // The pin fails, so the request selects again and the primary answers.
    let response = app
        .clone()
        .oneshot(chat_model("upstream-model", false))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // The new pin is the primary. The fallback is not called.
    let response = app
        .oneshot(chat_model("upstream-model", false))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        fake.hits(),
        vec![
            "upstream-model".to_string(),
            "fallback-model".to_string(),
            "fallback-model".to_string(),
            "upstream-model".to_string(),
            "upstream-model".to_string(),
        ]
    );
    let records = wait_records(&dir, 3).await;
    assert_eq!(records[0].provider, "deepseek");
    assert_eq!(records[0].fallbacks, 1);
    assert!(
        records[0]
            .quirks
            .iter()
            .any(|quirk| quirk == "fallback_from:od")
    );
    assert_eq!(records[1].provider, "od");
    assert_eq!(records[1].fallbacks, 1);
    assert!(
        records[1]
            .quirks
            .iter()
            .any(|quirk| quirk == "fallback_from:deepseek")
    );
    assert_eq!(records[2].provider, "od");
    assert_eq!(records[2].fallbacks, 0);
}

#[tokio::test]
async fn open_circuit_still_uses_a_closed_fallback() {
    let dir = temp_dir("circuit-fb");
    let (root, fake) = start_scripted().await;
    let cfg = m4b_config(&dir, format!("{root}/v1"), root, true);
    let state = AppState::new(cfg, env_file(&dir)).unwrap();
    assert!(
        state
            .circuits()
            .force_open("od", "test", Duration::from_secs(60))
    );
    let app = build_router(state);
    fake.queue("fallback-model", &[200, 200]);

    let response = app
        .clone()
        .oneshot(chat_model("upstream-model", false))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        fake.hits_of("upstream-model"),
        0,
        "an open circuit must not be called"
    );

    // The fallback is now pinned, so the next request skips the open provider again.
    let response = app
        .oneshot(chat_model("upstream-model", false))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(fake.hits_of("upstream-model"), 0);
    assert_eq!(fake.hits_of("fallback-model"), 2);
}

#[tokio::test]
async fn a_stream_that_dies_after_200_unpins_the_provider() {
    let dir = temp_dir("route-stream-unpin");
    let (app, fake) = m4b_gateway(&dir, true).await;

    // First request: primary fails, fallback succeeds, so the model pins deepseek.
    fake.queue("upstream-model", &[500]);
    fake.queue("fallback-model", &[200]);
    let response = app
        .clone()
        .oneshot(chat_model("upstream-model", false))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // The pinned stream starts with 200 and then dies: the pump must unpin it.
    fake.die_in_stream();
    let response = app
        .clone()
        .oneshot(chat_model("upstream-model", true))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let _ = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    fake.heal_stream();

    // With the pin gone, selection falls back to configuration order (od first).
    fake.queue("upstream-model", &[200]);
    let response = app
        .oneshot(chat_model("upstream-model", false))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        fake.hits(),
        vec![
            "upstream-model".to_string(),
            "fallback-model".to_string(),
            "fallback-model".to_string(),
            "upstream-model".to_string(),
        ]
    );
}

#[tokio::test]
async fn a_retryable_failure_tries_the_same_providers_next_upstream() {
    let dir = temp_dir("route-second-upstream");
    let (root, fake) = start_scripted().await;
    let mut cfg = m4b_config(&dir, format!("{root}/v1"), root, false);
    cfg.models[0].backends.push(Backend {
        provider: "od".into(),
        upstream: "second-model".into(),
    });
    let state = AppState::new(cfg, env_file(&dir)).unwrap();
    let app = build_router(state);
    fake.queue("upstream-model", &[503]);
    fake.queue("second-model", &[200]);

    let response = app
        .oneshot(chat_model("upstream-model", false))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        fake.hits(),
        vec!["upstream-model".to_string(), "second-model".to_string()]
    );
}

#[tokio::test]
async fn deepseek_402_opens_the_circuit_immediately() {
    let dir = temp_dir("ds-402");
    let (app, fake) = m4b_gateway(&dir, false).await;
    fake.queue("fallback-model", &[402]);

    let response = app
        .clone()
        .oneshot(chat_model("deepseek-flash", false))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let value = body_json(response).await;
    assert_eq!(value["error"]["code"], "upstream_quota_exhausted");

    let health = body_json(app.clone().oneshot(healthz_request()).await.unwrap()).await;
    assert_eq!(health["providers"]["deepseek"]["circuit"], "open");
    assert_eq!(
        health["providers"]["deepseek"]["reason"],
        "insufficient_balance"
    );
    assert!(health["providers"]["deepseek"]["opened_at"].is_string());

    let response = app
        .oneshot(chat_model("deepseek-flash", false))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        fake.hits_of("fallback-model"),
        1,
        "a balance-open circuit must reject without an upstream call"
    );
}

#[tokio::test]
async fn od_401_opens_the_circuit_and_stays_opaque() {
    let dir = temp_dir("od-401");
    let (app, fake) = m4b_gateway(&dir, false).await;
    fake.queue("upstream-model", &[401]);

    let response = app
        .clone()
        .oneshot(chat_model("upstream-model", false))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let value = body_json(response).await;
    assert_eq!(value["error"]["code"], "upstream_auth_failed");

    let health = body_json(app.clone().oneshot(healthz_request()).await.unwrap()).await;
    assert_eq!(health["providers"]["od"]["circuit"], "open");
    assert_eq!(health["providers"]["od"]["reason"], "auth_failed");

    let response = app
        .oneshot(chat_model("upstream-model", false))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(fake.hits_of("upstream-model"), 1);
}

#[tokio::test]
async fn circuit_open_uses_the_anthropic_error_shape_for_messages() {
    let dir = temp_dir("circuit-messages");
    let (app, fake) = m4b_gateway(&dir, false).await;
    fake.queue("upstream-model", &[401]);

    let response = app
        .clone()
        .oneshot(messages_model("upstream-model"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);

    let response = app.oneshot(messages_model("upstream-model")).await.unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.headers()["x-umsp-code"], "temporarily_unavailable");
    let value = body_json(response).await;
    assert_eq!(value["type"], "error");
    assert_eq!(value["error"]["type"], "api_error");
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("temporarily unavailable")
    );
}

#[tokio::test]
async fn healthz_circuit_fields_never_leak_keys() {
    let dir = temp_dir("healthz-keys");
    let (app, _fake) = m4b_gateway(&dir, false).await;

    let body = body_bytes(app.oneshot(healthz_request()).await.unwrap()).await;
    let text = String::from_utf8(body).unwrap();
    assert!(!text.contains("sk-test"), "healthz leaked an upstream key");
    assert!(
        !text.contains("sk-ds-test"),
        "healthz leaked an upstream key"
    );
    let value: Value = serde_json::from_str(&text).unwrap();
    for provider in ["od", "deepseek"] {
        assert!(value["providers"][provider]["circuit"].is_string());
        assert!(value["providers"][provider].get("failures").is_some());
        assert!(value["providers"][provider].get("opened_at").is_some());
        assert!(value["providers"][provider].get("reason").is_some());
    }
}

#[tokio::test]
async fn success_resets_the_failure_window() {
    let dir = temp_dir("circuit-reset");
    let (app, fake) = m4b_gateway(&dir, false).await;
    fake.queue("upstream-model", &[500, 500, 500, 500, 200]);

    for _ in 0..4 {
        let response = app
            .clone()
            .oneshot(chat_model("upstream-model", false))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    }
    let response = app
        .clone()
        .oneshot(chat_model("upstream-model", false))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let health = body_json(app.oneshot(healthz_request()).await.unwrap()).await;
    assert_eq!(health["providers"]["od"]["circuit"], "closed");
    assert_eq!(health["providers"]["od"]["failures"], 0);
}

// ---------------------------------------------------------------------------
// M5: Prometheus /metrics
// ---------------------------------------------------------------------------

fn metrics_request() -> Request<Body> {
    Request::builder()
        .uri("/metrics")
        .body(Body::empty())
        .unwrap()
}

async fn metrics_text(app: Router) -> String {
    let response = app.oneshot(metrics_request()).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["content-type"],
        "text/plain; version=0.0.4; charset=utf-8"
    );
    String::from_utf8(body_bytes(response).await).unwrap()
}

/// Sum of every series of a counter, e.g. all `umsp_requests_total{...}` lines.
fn counter_sum(text: &str, name: &str) -> u64 {
    let prefix = format!("{name}{{");
    text.lines()
        .filter(|line| line.starts_with(&prefix))
        .filter_map(|line| line.rsplit(' ').next()?.parse::<u64>().ok())
        .sum()
}

#[tokio::test]
async fn metrics_need_no_key_and_match_the_ledger() {
    let dir = temp_dir("metrics-basic");
    let (app, _, rec) = gateway(&dir, 50 * 1024 * 1024).await;

    // No Authorization header: admin endpoints sit outside client auth.
    let before = metrics_text(app.clone()).await;
    assert!(before.contains("# TYPE umsp_requests_total counter"));
    assert!(before.contains("# TYPE umsp_request_duration_seconds histogram"));
    assert!(before.contains("umsp_circuit_open{provider=\"od\"} 0"));
    assert_eq!(counter_sum(&before, "umsp_requests_total"), 0);
    assert!(
        rec.seen.lock().unwrap().is_empty(),
        "/metrics must never call an upstream"
    );

    let response = app.clone().oneshot(chat_request("normal")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let records = wait_records(&dir, 1).await;

    let text = metrics_text(app).await;
    assert!(text.contains(
        "umsp_requests_total{provider=\"od\",model=\"upstream-model\",protocol=\"chat\",status=\"200\",stream=\"false\"} 1"
    ));
    assert!(text.contains(
        "umsp_request_duration_seconds_count{provider=\"od\",model=\"upstream-model\",protocol=\"chat\"} 1"
    ));
    assert!(text.contains(
        "umsp_tokens_total{provider=\"od\",model=\"upstream-model\",direction=\"input\"} 10"
    ));
    assert!(text.contains("direction=\"output\"} 20"));
    assert!(text.contains("direction=\"cached\"} 4"));
    assert!(text.contains("direction=\"reasoning\"} 6"));
    // No client name, client key or request id in the exposition.
    assert!(!text.contains(TEST_KEY));
    assert!(!text.contains("tester"));
    assert!(!text.contains("umsp_testkey"));
    // One finished request = one series increment = one ledger row.
    assert_eq!(
        counter_sum(&text, "umsp_requests_total"),
        records.len() as u64
    );
    assert_eq!(records.len(), 1);
}

#[tokio::test]
async fn metrics_count_a_fallback_once_under_the_serving_provider() {
    let dir = temp_dir("metrics-fallback");
    let (app, fake) = m4b_gateway(&dir, true).await;
    fake.queue("upstream-model", &[502]);

    let response = app
        .clone()
        .oneshot(chat_model("upstream-model", false))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let records = wait_records(&dir, 1).await;

    let text = metrics_text(app).await;
    // One finished request, accounted under the provider that answered.
    assert!(text.contains(
        "umsp_requests_total{provider=\"deepseek\",model=\"upstream-model\",protocol=\"chat\",status=\"200\",stream=\"false\"} 1"
    ));
    assert!(
        !text.contains("umsp_requests_total{provider=\"od\""),
        "the absorbed primary attempt must not count as a second request"
    );
    assert_eq!(
        counter_sum(&text, "umsp_requests_total"),
        records.len() as u64
    );
    // The absorbed 502 still shows up as an upstream error of the primary.
    assert!(text.contains("umsp_upstream_errors_total{provider=\"od\",code=\"upstream_502\"} 1"));
}

#[tokio::test]
async fn metrics_record_stream_ttft_and_return_inflight_to_zero() {
    let dir = temp_dir("metrics-stream");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;

    let response = app.clone().oneshot(stream_request("sse")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    // Draining the body runs the pump to completion, which writes the ledger.
    let body = body_bytes(response).await;
    assert!(body.ends_with(b"[DONE]\n\n"));
    let records = wait_records(&dir, 1).await;
    assert!(records[0].ttft_ms.is_some(), "stream must record a TTFT");

    let text = metrics_text(app).await;
    assert!(text.contains("stream=\"true\""));
    assert!(text.contains(
        "umsp_ttft_seconds_count{provider=\"od\",model=\"upstream-model\",protocol=\"chat\"} 1"
    ));
    assert!(text.contains("umsp_request_duration_seconds_count{provider=\"od\""));
    // The in-flight gauge drops back to zero and its series disappears.
    assert!(
        !text.contains("umsp_inflight_requests{"),
        "inflight gauge leaked: {text}"
    );
}

#[tokio::test]
async fn oauth_accounts_send_independent_session_headers() {
    let dir = temp_dir("oauth-headers");
    let (upstream, rec) = start_upstream().await;
    let mut cfg = config(&dir, upstream, 50 * 1024 * 1024);
    let base = cfg.providers[0].base_url.clone();
    for id in ["grok", "grok-2"] {
        cfg.providers.push(Provider {
            id: id.into(),
            vendor: Some("grok".into()),
            base_url: base.clone(),
            anthropic_base_url: None,
            api_key_env: String::new(),
            auth: "oauth".into(),
            protocols: vec!["responses".into()],
            quirks: None,
        });
    }
    cfg.models.push(Model {
        id: "grok-a".into(),
        backends: vec![Backend {
            provider: "grok".into(),
            upstream: "grok-4.7".into(),
        }],
        aliases: vec![],
        context_length: 256_000,
        context_lengths: Vec::new(),
        max_completion_tokens: 0,
        capabilities: vec!["text".into()],
        display_name: None,
        created: 1_789_026_056,
        description: None,
    });
    cfg.models.push(Model {
        id: "grok-b".into(),
        backends: vec![Backend {
            provider: "grok-2".into(),
            upstream: "grok-4.7".into(),
        }],
        aliases: vec![],
        context_length: 256_000,
        context_lengths: Vec::new(),
        max_completion_tokens: 0,
        capabilities: vec!["text".into()],
        display_name: None,
        created: 1_789_026_056,
        description: None,
    });
    let sessions = dir.join("sessions");
    umsp::sessions::write_session(
        &sessions,
        "grok",
        "token-account-a",
        "refresh-a",
        "2099-01-01T00:00:00Z",
    )
    .unwrap();
    umsp::sessions::write_session(
        &sessions,
        "grok-2",
        "token-account-b",
        "refresh-b",
        "2099-01-01T00:00:00Z",
    )
    .unwrap();
    let state = AppState::with_sessions(cfg, env_file(&dir), sessions).unwrap();
    let app = build_router(state);

    let response = app.clone().oneshot(chat_request("normal")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response = app
        .clone()
        .oneshot(responses_named("grok-a"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response = app.oneshot(responses_named("grok-b")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let seen = rec.headers.lock().unwrap().clone();
    assert_eq!(seen.len(), 3);
    let auth = |i: usize| {
        seen[i]
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    };
    let hdr = |i: usize, name: &str| {
        seen[i]
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    };
    assert_eq!(auth(0), "Bearer sk-test");
    assert!(hdr(0, "x-xai-token-auth").is_empty());
    assert_eq!(auth(1), "Bearer token-account-a");
    assert_eq!(hdr(1, "x-xai-token-auth"), "xai-grok-cli");
    assert_eq!(hdr(1, "x-grok-model-override"), "grok-4.7");
    assert_eq!(
        hdr(1, "x-grok-client-version"),
        umsp::sessions::GROK_CLI_VERSION
    );
    assert_eq!(auth(2), "Bearer token-account-b");
    assert_eq!(hdr(2, "x-xai-token-auth"), "xai-grok-cli");
    assert_eq!(hdr(2, "x-grok-model-override"), "grok-4.7");
    assert_eq!(
        hdr(2, "x-grok-client-version"),
        umsp::sessions::GROK_CLI_VERSION
    );
    assert!(
        !std::fs::read_to_string(dir.join("sessions/grok.json"))
            .unwrap()
            .contains("token-account-b")
    );
}

#[tokio::test]
async fn registered_model_accepts_every_protocol_against_responses_only() {
    let dir = temp_dir("translate-responses-only");
    let (upstream, rec) = start_upstream().await;
    let mut cfg = config(&dir, upstream, 50 * 1024 * 1024);
    cfg.providers[0].protocols = vec!["responses".into()];
    cfg.providers[0].quirks = None;
    let state = AppState::new(cfg, env_file(&dir)).unwrap();
    let app = build_router(state);

    let chat = app.clone().oneshot(chat_request("normal")).await.unwrap();
    assert_eq!(chat.status(), StatusCode::OK);
    let chat_body = body_json(chat).await;
    assert_eq!(chat_body["object"], "chat.completion");
    assert_eq!(chat_body["model"], "upstream-model");
    assert_eq!(chat_body["choices"][0]["message"]["content"], "hi");
    assert_eq!(chat_body["usage"]["prompt_tokens"], 11);
    let seen = rec.last();
    assert!(
        seen.get("input").is_some(),
        "upstream saw a responses body: {seen}"
    );
    assert!(seen.get("messages").is_none(), "{seen}");
    assert_eq!(seen["model"], "upstream-model");

    let messages = app
        .clone()
        .oneshot(post_to(
            "/v1/messages",
            json!({
                "model": "upstream-model",
                "max_tokens": 32,
                "system": "be brief",
                "messages": [{"role": "user", "content": "hello"}],
                "stream": false
            }),
            "normal",
        ))
        .await
        .unwrap();
    assert_eq!(messages.status(), StatusCode::OK);
    let messages_body = body_json(messages).await;
    assert_eq!(messages_body["type"], "message");
    assert_eq!(messages_body["content"][0]["text"], "hi");
    assert_eq!(messages_body["stop_reason"], "end_turn");
    let seen = rec.last();
    assert_eq!(seen["instructions"], "be brief");
    assert_eq!(seen["max_output_tokens"], 32);

    let stream = app.oneshot(stream_request("sse")).await.unwrap();
    assert_eq!(stream.status(), StatusCode::OK);
    let bytes = body_bytes(stream).await;
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("\"content\":\"hi\""), "{text}");
    assert!(text.contains("data: [DONE]"), "{text}");

    let records = wait_records(&dir, 3).await;
    assert!(
        records.iter().all(|record| {
            record
                .quirks
                .iter()
                .any(|quirk| quirk.starts_with("translated:"))
        }),
        "{:?}",
        records
            .iter()
            .map(|record| &record.quirks)
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn higher_priority_responses_provider_is_tried_before_a_native_chat_backend() {
    let dir = temp_dir("translate-priority");
    let (root, fake) = start_scripted().await;
    let mut cfg = config(&dir, format!("{root}/v1"), 50 * 1024 * 1024);
    cfg.providers[0].base_url = format!("{root}/v1");
    cfg.providers.push(Provider {
        id: "responses-only".into(),
        vendor: Some("open-design".into()),
        base_url: format!("{root}/v1"),
        anthropic_base_url: None,
        api_key_env: "TEST_OD_KEY".into(),
        auth: "api_key".into(),
        protocols: vec!["responses".into()],
        quirks: None,
    });
    cfg.models[0].backends = vec![
        Backend {
            provider: "responses-only".into(),
            upstream: "only-model".into(),
        },
        Backend {
            provider: "od".into(),
            upstream: "upstream-model".into(),
        },
    ];
    fake.queue("only-model", &[502]);
    let state = AppState::new(cfg, env_file(&dir)).unwrap();
    let app = build_router(state);
    let response = app.oneshot(chat_request("normal")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["choices"][0]["message"]["content"], "ok");
    assert_eq!(
        fake.hits(),
        vec!["only-model".to_string(), "upstream-model".to_string()]
    );
    let records = wait_records(&dir, 1).await;
    assert_eq!(records[0].provider, "od");
    assert!(
        records[0]
            .quirks
            .iter()
            .any(|quirk| quirk == "fallback_from:responses-only"),
        "{:?}",
        records[0].quirks
    );
}

fn responses_named(model: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/responses")
        .header("authorization", format!("Bearer {TEST_KEY}"))
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "model": model,
                "input": "ping",
                "stream": false
            }))
            .unwrap(),
        ))
        .unwrap()
}

// ---------------------------------------------------------------------------
// Grok weekly billing producer
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct BillingCall {
    path: String,
    authorization: String,
    user_id: Option<String>,
}

#[derive(Clone, Default)]
struct BillingFake {
    seen: Arc<Mutex<Vec<BillingCall>>>,
}

async fn billing_user(
    State(fake): State<BillingFake>,
    headers: axum::http::HeaderMap,
) -> Json<Value> {
    fake.seen.lock().unwrap().push(BillingCall {
        path: "/v1/user".into(),
        authorization: header(&headers, "authorization").unwrap_or_default(),
        user_id: header(&headers, "x-userid"),
    });
    Json(json!({"xUserId": "2011310503512195073"}))
}

async fn billing_credits(
    State(fake): State<BillingFake>,
    headers: axum::http::HeaderMap,
) -> Json<Value> {
    fake.seen.lock().unwrap().push(BillingCall {
        path: "/v1/billing".into(),
        authorization: header(&headers, "authorization").unwrap_or_default(),
        user_id: header(&headers, "x-userid"),
    });
    Json(json!({
        "config": {
            "currentPeriod": {
                "type": "USAGE_PERIOD_TYPE_WEEKLY",
                "start": "2026-11-19T08:33:29.430+00:00",
                "end": "2099-11-26T08:33:29.430+00:00"
            },
            "creditUsagePercent": 44.0,
            "productUsage": [{"product": "GrokBuild", "usagePercent": 44.0}],
            "isUnifiedBillingUser": true
        }
    }))
}

fn header(headers: &axum::http::HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

#[tokio::test]
async fn grok_billing_reads_the_weekly_pool_and_user_id() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let fake = BillingFake::default();
    let app = Router::new()
        .route("/v1/user", axum::routing::get(billing_user))
        .route("/v1/billing", axum::routing::get(billing_credits))
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let dir = temp_dir("grok-billing");
    let sessions_dir = dir.join("sessions");
    std::fs::create_dir_all(&sessions_dir).unwrap();
    umsp::sessions::write_session(
        &sessions_dir,
        "grok",
        "session-token",
        "refresh-token",
        "2099-01-01T00:00:00Z",
    )
    .unwrap();
    let store = umsp::sessions::SessionStore::new(sessions_dir, reqwest::Client::new());
    let provider = Provider {
        id: "grok".into(),
        vendor: Some("grok".into()),
        base_url: format!("http://{addr}/v1"),
        anthropic_base_url: None,
        api_key_env: String::new(),
        auth: "oauth".into(),
        protocols: vec!["responses".into()],
        quirks: None,
    };

    let (usage, user_id) =
        umsp::grok_billing::fetch_usage(&reqwest::Client::new(), &store, &provider, "1.0.46", None)
            .await
            .unwrap();
    assert!((usage.remaining_fraction - 0.56).abs() < 1e-9);
    assert_eq!(usage.used_percent, 44.0);
    assert_eq!(user_id, "2011310503512195073");

    let seen = fake.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].path, "/v1/user");
    assert_eq!(seen[0].user_id, None);
    assert_eq!(seen[1].path, "/v1/billing");
    assert_eq!(seen[1].user_id.as_deref(), Some("2011310503512195073"));
    assert_eq!(seen[1].authorization, "Bearer session-token");
}

// ---------------------------------------------------------------------------
// Device binding (TOFU on source IP)
// ---------------------------------------------------------------------------

/// A chat request whose effective source IP is forced via XFF. The in-process
/// router has no socket peer, so the auth layer treats it as loopback and
/// trusts the proxy-supplied header — exactly the public reverse-proxy path.
fn chat_request_from_ip(mode: &str, xff: &str) -> Request<Body> {
    let mut req = chat_request(mode);
    req.headers_mut()
        .insert("x-forwarded-for", xff.parse().unwrap());
    req
}

fn bound_ip_of(dir: &Path, client: &str) -> Option<String> {
    umsp::bindings::BindingStore::load(dir).bound_ip(client)
}

#[tokio::test]
async fn key_binds_on_first_use_and_rejects_other_ips() {
    let dir = temp_dir("bind-first");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;

    // First authenticated request binds the key to the loopback peer.
    let response = app.clone().oneshot(chat_request("normal")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(bound_ip_of(&dir, "tester").as_deref(), Some("127.0.0.1"));

    // Same key from a different device IP: 403, and the error must not leak
    // the bound IP (the caller may hold a leaked key).
    let response = app
        .clone()
        .oneshot(chat_request_from_ip("normal", "9.9.9.9"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        response.headers().get("x-umsp-code").unwrap(),
        "device_mismatch"
    );
    let value = body_json(response).await;
    assert_eq!(value["error"]["code"], "device_mismatch");
    let text = value.to_string();
    assert!(!text.contains("127.0.0.1"), "bound IP leaked: {text}");

    // The rejected request must not move the binding; the bound device still works.
    assert_eq!(bound_ip_of(&dir, "tester").as_deref(), Some("127.0.0.1"));
    let response = app.oneshot(chat_request("normal")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Ledger rows carry the effective client IP.
    let store = LogStore::new(&dir);
    let rows = store.read_since(chrono::Utc::now() - chrono::TimeDelta::minutes(5));
    assert!(rows.len() >= 2);
    for row in &rows {
        assert_eq!(row.client_ip.as_deref(), Some("127.0.0.1"));
    }
}

#[tokio::test]
async fn device_mismatch_uses_messages_error_shape() {
    let dir = temp_dir("bind-msg");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let response = app.clone().oneshot(chat_request("normal")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let mut req = post_to(
        "/v1/messages",
        json!({
            "model": "upstream-model",
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}]
        }),
        "normal",
    );
    req.headers_mut()
        .insert("x-forwarded-for", "9.9.9.9".parse().unwrap());
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let value = body_json(response).await;
    assert_eq!(value["type"], "error");
    assert_eq!(value["error"]["type"], "permission_error");
    assert!(!value.to_string().contains("127.0.0.1"));
}

#[tokio::test]
async fn xff_rightmost_entry_wins_over_spoofed_left() {
    let dir = temp_dir("bind-xff");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;

    // Left entries are client-controlled; the proxy wrote the rightmost one.
    let response = app
        .clone()
        .oneshot(chat_request_from_ip("normal", "1.2.3.4, 9.9.9.9"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(bound_ip_of(&dir, "tester").as_deref(), Some("9.9.9.9"));

    let response = app
        .clone()
        .oneshot(chat_request_from_ip("normal", "9.9.9.9"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Presenting only the spoofed left entry must not pass.
    let response = app
        .oneshot(chat_request_from_ip("normal", "1.2.3.4"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn unbind_then_reload_reaches_live_state() {
    let dir = temp_dir("bind-unbind");
    let (upstream, _rec) = start_upstream().await;
    let cfg = config(&dir, upstream, 50 * 1024 * 1024);
    let state = AppState::new(cfg, env_file(&dir)).unwrap();
    let app = build_router(state.clone());

    let response = app.clone().oneshot(chat_request("normal")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response = app
        .clone()
        .oneshot(chat_request_from_ip("normal", "9.9.9.9"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    // CLI unbind writes the file; SIGHUP reloads it into the live state.
    assert!(
        umsp::bindings::BindingStore::load(&dir)
            .unbind("tester")
            .unwrap()
    );
    state.reload_bindings(&dir);

    // The next request re-binds (TOFU) to the new device IP.
    let response = app
        .clone()
        .oneshot(chat_request_from_ip("normal", "9.9.9.9"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(bound_ip_of(&dir, "tester").as_deref(), Some("9.9.9.9"));
    let response = app
        .oneshot(chat_request_from_ip("normal", "8.8.8.8"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn binding_persists_across_restart() {
    let dir = temp_dir("bind-restart");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let response = app.clone().oneshot(chat_request("normal")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    drop(app);

    // A fresh state over the same state_dir must enforce the stored binding.
    let (app2, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let response = app2
        .clone()
        .oneshot(chat_request_from_ip("normal", "9.9.9.9"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = app2.oneshot(chat_request("normal")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn count_tokens_enforces_binding() {
    let dir = temp_dir("bind-count");
    let (app, _rec) = ds_gateway(&dir).await;
    let response = app.clone().oneshot(chat_request("normal")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let mut req = post_to(
        "/v1/messages/count_tokens",
        json!({
            "model": "deepseek-flash",
            "messages": [{"role": "user", "content": "hi"}]
        }),
        "normal",
    );
    req.headers_mut()
        .insert("x-forwarded-for", "9.9.9.9".parse().unwrap());
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn models_get_binds_and_enforces() {
    let dir = temp_dir("bind-models");
    let (app, _, _) = gateway(&dir, 50 * 1024 * 1024).await;
    let get = |xff: Option<&str>| {
        let mut builder = Request::builder()
            .method("GET")
            .uri("/v1/models")
            .header("x-api-key", TEST_KEY);
        if let Some(ip) = xff {
            builder = builder.header("x-forwarded-for", ip);
        }
        builder.body(Body::empty()).unwrap()
    };
    let response = app.clone().oneshot(get(None)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(bound_ip_of(&dir, "tester").as_deref(), Some("127.0.0.1"));
    let response = app.oneshot(get(Some("9.9.9.9"))).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}
