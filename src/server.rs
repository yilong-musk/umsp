//! HTTP data plane: auth, `/v1/models`, `/healthz`, Chat Completions proxy.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, Query, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, head, post};
use chrono::Utc;
use serde_json::{Value, json};

use crate::circuit::{Admission, CircuitBreaker, CircuitState};
use crate::config::{Client, Config, Model, Provider};
use crate::envfile::EnvFile;
use crate::error::{Result, UmspError};
use crate::health::{self, Balance};
use crate::keys;
use crate::metrics::Metrics;
use crate::quirks;
use crate::registry;
use crate::sessions::{self, SessionError, SessionStore};
use crate::stream;
use crate::upstream::{self, forward_request_headers, relay_response_headers};
use crate::usage::{LogStore, RequestRecord, Usage};
use crate::util;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct ProviderHealth {
    pub last_success: Option<String>,
    pub last_error: Option<String>,
    pub last_status: Option<u16>,
    pub last_latency_ms: Option<u64>,
}

#[derive(Debug, Clone)]
struct ProbeCacheEntry {
    at: Instant,
    value: Value,
}

pub struct AppState {
    inner: Arc<Inner>,
}

struct Inner {
    cfg: RwLock<Arc<Config>>,
    env: EnvFile,
    http: reqwest::Client,
    http_stream: reqwest::Client,
    started: Instant,
    health: Mutex<HashMap<String, ProviderHealth>>,
    probe_cache: Mutex<HashMap<String, ProbeCacheEntry>>,
    circuits: CircuitBreaker,
    logs: LogStore,
    metrics: Metrics,
    sessions: SessionStore,
}

impl Clone for AppState {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl AppState {
    pub fn new(cfg: Config, env: EnvFile) -> Result<Self> {
        let dir = crate::config::sessions_dir(&crate::config::default_config_path());
        Self::with_sessions(cfg, env, dir)
    }

    /// `sessions_dir` is `{config_dir}/sessions`. Each oauth provider reads only
    /// its own `<id>.json` from that directory.
    pub fn with_sessions(
        cfg: Config,
        env: EnvFile,
        sessions_dir: std::path::PathBuf,
    ) -> Result<Self> {
        let http = upstream::build_http_client(&cfg)?;
        let sessions = SessionStore::new(sessions_dir, http.clone());
        Self::with_store(cfg, env, http, sessions)
    }

    pub fn with_store(
        cfg: Config,
        env: EnvFile,
        http: reqwest::Client,
        sessions: SessionStore,
    ) -> Result<Self> {
        let http_stream = upstream::build_stream_client()?;
        let logs = LogStore::new(&cfg.server.state_path());
        Ok(Self {
            inner: Arc::new(Inner {
                cfg: RwLock::new(Arc::new(cfg)),
                env,
                http,
                http_stream,
                started: Instant::now(),
                health: Mutex::new(HashMap::new()),
                probe_cache: Mutex::new(HashMap::new()),
                circuits: CircuitBreaker::default(),
                logs,
                metrics: Metrics::new(),
                sessions,
            }),
        })
    }

    pub fn circuits(&self) -> &CircuitBreaker {
        &self.inner.circuits
    }

    pub fn cfg(&self) -> Arc<Config> {
        Arc::clone(&self.inner.cfg.read().expect("config lock poisoned"))
    }

    pub fn env(&self) -> &EnvFile {
        &self.inner.env
    }

    pub fn http(&self) -> &reqwest::Client {
        &self.inner.http
    }

    /// Timeout-free client used for SSE upstream requests.
    pub fn stream_http(&self) -> &reqwest::Client {
        &self.inner.http_stream
    }

    pub fn logs(&self) -> &LogStore {
        &self.inner.logs
    }

    pub fn metrics(&self) -> &Metrics {
        &self.inner.metrics
    }

    pub fn sessions(&self) -> SessionStore {
        self.inner.sessions.clone()
    }

    /// Upstream client bound to this process's HTTP pools and session store.
    pub fn upstream(&self) -> Upstream {
        Upstream::with_clients(
            self.cfg(),
            self.env().clone(),
            self.http().clone(),
            self.stream_http().clone(),
            self.sessions(),
        )
    }

    /// Single exit point for a finished request: one ledger row *and* one
    /// metrics observation, so `/metrics` and `umsp usage` cannot diverge.
    pub fn finish_request(&self, record: &RequestRecord) {
        self.inner.metrics.observe(record);
        let _ = self.inner.logs.append(record);
    }

    pub fn uptime_s(&self) -> u64 {
        self.inner.started.elapsed().as_secs()
    }

    /// Swap in a new config (SIGHUP reload). Returns the previous one.
    pub fn replace_config(&self, cfg: Config) -> Arc<Config> {
        let mut guard = self.inner.cfg.write().expect("config lock poisoned");
        std::mem::replace(&mut *guard, Arc::new(cfg))
    }

    pub fn note_health(&self, provider: &str, status: u16, latency_ms: u64, error: Option<String>) {
        let mut health = self.inner.health.lock().expect("health lock poisoned");
        let entry = health.entry(provider.to_string()).or_default();
        entry.last_status = Some(status);
        entry.last_latency_ms = Some(latency_ms);
        if status < 400 {
            entry.last_success = Some(util::now_rfc3339());
            entry.last_error = None;
        } else {
            entry.last_error = error.or_else(|| Some(format!("http_{status}")));
        }
    }

    fn provider_health(&self, provider: &str) -> ProviderHealth {
        self.inner
            .health
            .lock()
            .expect("health lock poisoned")
            .get(provider)
            .cloned()
            .unwrap_or_default()
    }

    fn probe_cached(&self, provider: &str, ttl: Duration) -> Option<Value> {
        let cache = self.inner.probe_cache.lock().expect("probe lock poisoned");
        cache
            .get(provider)
            .filter(|entry| entry.at.elapsed() < ttl)
            .map(|entry| entry.value.clone())
    }

    fn probe_store(&self, provider: &str, value: Value) {
        let mut cache = self.inner.probe_cache.lock().expect("probe lock poisoned");
        cache.insert(
            provider.to_string(),
            ProbeCacheEntry {
                at: Instant::now(),
                value,
            },
        );
    }
}

// ---------------------------------------------------------------------------
// Proxy
// ---------------------------------------------------------------------------

/// Result of one upstream Chat Completions call.
#[derive(Debug)]
pub struct ChatOutcome {
    pub status: u16,
    pub headers: Vec<(HeaderName, HeaderValue)>,
    pub body: Vec<u8>,
    pub usage: Option<Usage>,
    pub quirks: Vec<String>,
    pub upstream_request_id: Option<String>,
    pub duration_ms: u64,
}

/// Result of an upstream streaming call, before any byte reaches the client.
#[derive(Debug)]
pub struct ChatStream {
    pub status: u16,
    pub headers: Vec<(HeaderName, HeaderValue)>,
    pub upstream_request_id: Option<String>,
    /// Live upstream body; the caller pumps it to the client.
    pub response: reqwest::Response,
}

#[derive(Debug)]
pub enum ProxyError {
    MissingKey {
        provider: String,
        env: String,
    },
    /// Subscription session missing or refresh rejected. Same client outcome as
    /// an upstream 401: the circuit opens as `auth_failed`.
    Auth(String),
    Timeout(String),
    Network(String),
}

impl ProxyError {
    pub fn code(&self) -> &'static str {
        match self {
            ProxyError::MissingKey { .. } => "upstream_key_missing",
            ProxyError::Auth(_) => "upstream_auth_failed",
            ProxyError::Timeout(_) => "upstream_timeout",
            ProxyError::Network(_) => "upstream_unreachable",
        }
    }
}

/// Minimal upstream client shared by `serve` and `umsp call`.
pub struct Upstream {
    pub cfg: Arc<Config>,
    pub env: EnvFile,
    pub http: reqwest::Client,
    /// Timeout-free client for SSE (see `upstream::build_stream_client`).
    pub http_stream: reqwest::Client,
    sessions: SessionStore,
}

impl Upstream {
    pub fn new(cfg: Arc<Config>, env: EnvFile, sessions_dir: &std::path::Path) -> Result<Self> {
        let http = upstream::build_http_client(&cfg)?;
        let http_stream = upstream::build_stream_client()?;
        let sessions = SessionStore::new(sessions_dir.to_path_buf(), http.clone());
        Ok(Self {
            cfg,
            env,
            http,
            http_stream,
            sessions,
        })
    }

    /// Reuse the long-lived clients owned by `AppState` instead of building
    /// (and warming) new connection pools per request.
    pub fn with_clients(
        cfg: Arc<Config>,
        env: EnvFile,
        http: reqwest::Client,
        http_stream: reqwest::Client,
        sessions: SessionStore,
    ) -> Self {
        Self {
            cfg,
            env,
            http,
            http_stream,
            sessions,
        }
    }

    fn api_key(&self, provider: &Provider) -> std::result::Result<String, ProxyError> {
        self.env
            .get(&provider.api_key_env)
            .ok_or_else(|| ProxyError::MissingKey {
                provider: provider.id.clone(),
                env: provider.api_key_env.clone(),
            })
    }

    async fn bearer(&self, provider: &Provider) -> std::result::Result<String, ProxyError> {
        if !provider.uses_oauth() {
            return self.api_key(provider);
        }
        self.sessions
            .access_token(&provider.id)
            .await
            .map_err(|err| match err {
                SessionError::Missing { provider, .. } => ProxyError::MissingKey {
                    provider,
                    env: "oauth session".into(),
                },
                SessionError::Refresh { .. } => ProxyError::Auth(err.to_string()),
            })
    }

    #[allow(clippy::too_many_arguments)]
    async fn send(
        &self,
        http: &reqwest::Client,
        url: &str,
        provider: &Provider,
        body: &Value,
        payload: &[u8],
        client_headers: &HeaderMap,
        accept: &'static str,
    ) -> std::result::Result<reqwest::Response, ProxyError> {
        let token = self.bearer(provider).await?;
        let response = self
            .send_with_token(
                http,
                url,
                provider,
                body,
                payload,
                client_headers,
                accept,
                &token,
            )
            .await?;
        if response.status().as_u16() == 401 && provider.uses_oauth() {
            match self.sessions.refresh_rejected(&provider.id, &token).await {
                Ok(fresh) => {
                    let _ = response.bytes().await;
                    return self
                        .send_with_token(
                            http,
                            url,
                            provider,
                            body,
                            payload,
                            client_headers,
                            accept,
                            &fresh,
                        )
                        .await;
                }
                Err(_) => return Ok(response),
            }
        }
        Ok(response)
    }

    #[allow(clippy::too_many_arguments)]
    async fn send_with_token(
        &self,
        http: &reqwest::Client,
        url: &str,
        provider: &Provider,
        body: &Value,
        payload: &[u8],
        client_headers: &HeaderMap,
        accept: &'static str,
        token: &str,
    ) -> std::result::Result<reqwest::Response, ProxyError> {
        let mut req = http
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ACCEPT, accept)
            .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"));
        if provider.uses_oauth() {
            req = req.header("X-XAI-Token-Auth", sessions::CLI_TOKEN_AUTH);
            if let Some(model) = body.get("model").and_then(Value::as_str) {
                let value = reqwest::header::HeaderValue::from_str(model).map_err(|_| {
                    ProxyError::Network(format!(
                        "upstream model id for provider '{}' is not a valid header value",
                        provider.id
                    ))
                })?;
                req = req.header("x-grok-model-override", value);
            }
        }
        for (name, value) in forward_request_headers(client_headers) {
            if provider.uses_oauth() && sessions::is_cli_session_header(name.as_str()) {
                continue;
            }
            req = req.header(name, value);
        }
        req.body(payload.to_vec()).send().await.map_err(|e| {
            if e.is_timeout() {
                ProxyError::Timeout(format!("upstream request to {url} timed out: {e}"))
            } else {
                ProxyError::Network(format!("upstream request to {url} failed: {e}"))
            }
        })
    }

    /// Forward a non-streaming Chat Completions request. The body must already
    /// have `model` set to the upstream model id.
    pub async fn chat(
        &self,
        provider: &Provider,
        upstream_model: &str,
        body: &Value,
        client_headers: &HeaderMap,
    ) -> std::result::Result<ChatOutcome, ProxyError> {
        let _ = upstream_model;
        self.forward(provider, "chat", None, None, body, client_headers)
            .await
    }

    /// Forward a non-streaming OpenAI Responses request. The body must already
    /// have `model` set to the upstream model id; everything else is relayed.
    pub async fn responses(
        &self,
        provider: &Provider,
        body: &Value,
        client_headers: &HeaderMap,
    ) -> std::result::Result<ChatOutcome, ProxyError> {
        self.forward(provider, "responses", None, None, body, client_headers)
            .await
    }

    /// Forward a non-streaming Anthropic Messages request (spec 2.3). The body
    /// must already have `model` set to the upstream model id; the client's
    /// raw query string (`?beta=true`) is relayed unchanged.
    pub async fn messages(
        &self,
        provider: &Provider,
        query: Option<&str>,
        body: &Value,
        client_headers: &HeaderMap,
    ) -> std::result::Result<ChatOutcome, ProxyError> {
        self.forward(provider, "messages", None, query, body, client_headers)
            .await
    }

    /// Forward Anthropic `POST /v1/messages/count_tokens` (spec 2.3). Only
    /// providers that implement it get here — DeepSeek's Anthropic-compatible
    /// endpoint; OpenDesign 404s, so the gateway answers 501 before calling.
    pub async fn count_tokens(
        &self,
        provider: &Provider,
        body: &Value,
        client_headers: &HeaderMap,
    ) -> std::result::Result<ChatOutcome, ProxyError> {
        self.forward(
            provider,
            "messages",
            Some("count_tokens"),
            None,
            body,
            client_headers,
        )
        .await
    }

    /// Shared non-streaming path for `chat`, `responses`, `messages` and
    /// Messages `count_tokens` (`endpoint` = the trailing path segment).
    ///
    /// Chat responses are re-serialized after the OpenDesign quirks are applied
    /// (M0 behaviour); Responses/Messages bodies are relayed byte-for-byte and
    /// only *read* for usage (spec 2.2 / 2.3).
    async fn forward(
        &self,
        provider: &Provider,
        protocol: &str,
        endpoint: Option<&str>,
        query: Option<&str>,
        body: &Value,
        client_headers: &HeaderMap,
    ) -> std::result::Result<ChatOutcome, ProxyError> {
        let url = upstream::protocol_url(provider, protocol)
            .map(|url| match endpoint {
                Some(suffix) => format!("{url}/{suffix}"),
                None => url,
            })
            .map(|url| upstream::append_query(url, query))
            .ok_or_else(|| {
                ProxyError::Network(format!(
                    "provider '{}' has no {protocol} endpoint",
                    provider.id
                ))
            })?;
        let started = Instant::now();
        let payload = serde_json::to_vec(body)
            .map_err(|e| ProxyError::Network(format!("cannot serialize request: {e}")))?;
        let resp = self
            .send(
                &self.http.clone(),
                &url,
                provider,
                body,
                &payload,
                client_headers,
                "application/json",
            )
            .await?;

        let status = resp.status().as_u16();
        let mut headers = relay_response_headers(resp.headers());
        if protocol == "messages" {
            upstream::normalize_retry_after(&mut headers);
        }
        let upstream_request_id = resp
            .headers()
            .get("x-request-id")
            .or_else(|| resp.headers().get("x-ds-trace-id"))
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| ProxyError::Network(format!("cannot read upstream response body: {e}")))?;

        let mut quirks_applied = Vec::new();
        let mut usage = None;
        let body_out: Vec<u8> = if status < 400 {
            match serde_json::from_slice::<Value>(&bytes) {
                Ok(mut value) if value.is_object() => {
                    usage = if endpoint.is_some() {
                        Usage::from_count_tokens(&value)
                    } else if protocol == "responses" {
                        Usage::from_responses(&value)
                    } else if protocol == "messages" {
                        Usage::from_messages(&value)
                    } else {
                        Usage::from_chat(&value)
                    };
                    if protocol == "chat" {
                        if provider.quirks.as_deref() == Some("opendesign") {
                            quirks_applied = quirks::apply_chat_response_quirks(
                                &mut value,
                                effective_budget(body),
                            );
                        }
                        serde_json::to_vec(&value).unwrap_or_else(|_| bytes.to_vec())
                    } else {
                        // Only `model` may change on the way up; the response is
                        // passed through untouched (spec 2.0 / 2.2).
                        bytes.to_vec()
                    }
                }
                _ => bytes.to_vec(),
            }
        } else {
            // Upstream errors pass through byte-for-byte.
            bytes.to_vec()
        };

        Ok(ChatOutcome {
            status,
            headers,
            body: body_out,
            usage,
            quirks: quirks_applied,
            upstream_request_id,
            duration_ms: started.elapsed().as_millis() as u64,
        })
    }

    /// Open a streaming Chat Completions request. Returns as soon as the
    /// upstream response *headers* arrive (bounded by `header_timeout`); the
    /// body is left untouched for the caller to pump byte-for-byte.
    pub async fn chat_stream(
        &self,
        provider: &Provider,
        body: &Value,
        client_headers: &HeaderMap,
        header_timeout: std::time::Duration,
    ) -> std::result::Result<ChatStream, ProxyError> {
        self.forward_stream(provider, "chat", None, body, client_headers, header_timeout)
            .await
    }

    /// Open a streaming OpenAI Responses request (`Accept: text/event-stream`).
    pub async fn responses_stream(
        &self,
        provider: &Provider,
        body: &Value,
        client_headers: &HeaderMap,
        header_timeout: std::time::Duration,
    ) -> std::result::Result<ChatStream, ProxyError> {
        self.forward_stream(
            provider,
            "responses",
            None,
            body,
            client_headers,
            header_timeout,
        )
        .await
    }

    /// Open a streaming Anthropic Messages request (spec 2.3).
    pub async fn messages_stream(
        &self,
        provider: &Provider,
        query: Option<&str>,
        body: &Value,
        client_headers: &HeaderMap,
        header_timeout: std::time::Duration,
    ) -> std::result::Result<ChatStream, ProxyError> {
        self.forward_stream(
            provider,
            "messages",
            query,
            body,
            client_headers,
            header_timeout,
        )
        .await
    }

    async fn forward_stream(
        &self,
        provider: &Provider,
        protocol: &str,
        query: Option<&str>,
        body: &Value,
        client_headers: &HeaderMap,
        header_timeout: std::time::Duration,
    ) -> std::result::Result<ChatStream, ProxyError> {
        let url = upstream::protocol_url(provider, protocol)
            .map(|url| upstream::append_query(url, query))
            .ok_or_else(|| {
                ProxyError::Network(format!(
                    "provider '{}' has no {protocol} endpoint",
                    provider.id
                ))
            })?;
        let payload = serde_json::to_vec(body)
            .map_err(|e| ProxyError::Network(format!("cannot serialize request: {e}")))?;
        let http_stream = self.http_stream.clone();
        let resp = match tokio::time::timeout(
            header_timeout,
            self.send(
                &http_stream,
                &url,
                provider,
                body,
                &payload,
                client_headers,
                "text/event-stream",
            ),
        )
        .await
        {
            Err(_) => {
                return Err(ProxyError::Timeout(format!(
                    "upstream {url} sent no response headers within {}s",
                    header_timeout.as_secs()
                )));
            }
            Ok(Err(err)) => return Err(err),
            Ok(Ok(resp)) => resp,
        };

        let status = resp.status().as_u16();
        let mut headers = relay_response_headers(resp.headers());
        if protocol == "messages" {
            upstream::normalize_retry_after(&mut headers);
        }
        let upstream_request_id = resp
            .headers()
            .get("x-request-id")
            .or_else(|| resp.headers().get("x-ds-trace-id"))
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        Ok(ChatStream {
            status,
            headers,
            upstream_request_id,
            response: resp,
        })
    }
}

fn effective_budget(body: &Value) -> Option<u64> {
    quirks::effective_max_tokens(body)
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn build_router(state: AppState) -> Router {
    let api = Router::new()
        .route("/v1/models", get(models_handler))
        .route("/v1/chat/completions", post(chat_handler))
        .route("/v1/responses", post(responses_handler))
        .route("/v1/messages", post(messages_handler))
        .route(
            "/v1/messages/count_tokens",
            post(messages_count_tokens_handler),
        )
        // Retrieval, cancel, input_items and input_tokens are not proxied; a
        // protocol-native 501 beats a bare 404 (spec 2.2, unsupported path).
        .route("/v1/responses/{*rest}", any(responses_unsupported_handler))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));

    Router::new()
        .route("/api/hello", head(hello_handler))
        .merge(
            Router::new()
                .route("/healthz", get(healthz_handler))
                .route("/metrics", get(metrics_handler))
                .route_layer(middleware::from_fn(admin_loopback_guard)),
        )
        .merge(api)
        .layer(axum::extract::DefaultBodyLimit::disable())
        .with_state(state)
}

#[derive(Clone)]
struct AuthedClient(Client);

async fn auth_middleware(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    let presented = keys::extract_presented_key(req.headers());
    let cfg = state.cfg();
    let matched = presented.and_then(|key| keys::authenticate(&cfg.clients, &key).cloned());
    match matched {
        Some(client) => {
            req.extensions_mut().insert(AuthedClient(client));
            next.run(req).await
        }
        None => {
            let request_id = util::new_request_id();
            let message =
                "missing or invalid API key (send Authorization: Bearer <key> or x-api-key)";
            let is_messages = req.uri().path().starts_with("/v1/messages");
            let mut resp = if is_messages {
                messages_error(
                    StatusCode::UNAUTHORIZED,
                    "authentication_error",
                    "unauthorized",
                    message,
                    &request_id,
                )
            } else {
                gateway_error(
                    StatusCode::UNAUTHORIZED,
                    "unauthorized",
                    message,
                    &request_id,
                )
            };
            resp.headers_mut().insert(
                "www-authenticate",
                HeaderValue::from_static("Bearer realm=\"umsp\""),
            );
            resp
        }
    }
}

fn error_type_for(status: StatusCode) -> &'static str {
    match status.as_u16() {
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        413 => "invalid_request_error",
        429 => "rate_limit_error",
        // A protocol/feature the gateway does not speak yet is the caller's
        // mistake to correct (e.g. retry with "stream": false), not a server fault.
        501 => "invalid_request_error",
        500..=599 => "server_error",
        _ => "invalid_request_error",
    }
}

/// Gateway-originated error in OpenAI's native shape, plus `X-UMSP-Code`.
pub fn gateway_error(status: StatusCode, code: &str, message: &str, request_id: &str) -> Response {
    let payload = json!({
        "error": {
            "message": message,
            "type": error_type_for(status),
            "code": code,
            "param": null
        }
    });
    let body = serde_json::to_vec(&payload).unwrap_or_default();
    let mut resp = Response::builder()
        .status(status)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .header("x-umsp-request-id", request_id)
        .header("x-umsp-code", code)
        .body(Body::from(body))
        .expect("valid response");
    if status == StatusCode::METHOD_NOT_ALLOWED {
        resp.headers_mut()
            .insert(axum::http::header::ALLOW, HeaderValue::from_static("POST"));
    }
    resp
}

/// Anthropic Messages error shape (spec 2.0): `{"type":"error","error":{"type","message"}}`.
///
/// `error_type` is an Anthropic error type (`not_supported_error`,
/// `authentication_error`, ...). `code` becomes `X-UMSP-Code` for scripts.
pub fn messages_error(
    status: StatusCode,
    error_type: &str,
    code: &str,
    message: &str,
    request_id: &str,
) -> Response {
    let payload = json!({
        "type": "error",
        "error": {"type": error_type, "message": message}
    });
    let body = serde_json::to_vec(&payload).unwrap_or_default();
    Response::builder()
        .status(status)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .header("x-umsp-request-id", request_id)
        .header("x-umsp-code", code)
        .body(Body::from(body))
        .expect("valid response")
}

fn messages_error_type(status: StatusCode) -> &'static str {
    match status.as_u16() {
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        429 => "rate_limit_error",
        501 => "not_supported_error",
        500..=599 => "api_error",
        _ => "invalid_request_error",
    }
}

/// A gateway-originated error in the protocol's native shape: OpenAI for
/// Chat/Responses, Anthropic for Messages (spec 2.0, errors).
pub fn protocol_gateway_error(
    protocol: &str,
    status: StatusCode,
    code: &str,
    message: &str,
    request_id: &str,
) -> Response {
    if protocol == "messages" {
        messages_error(
            status,
            messages_error_type(status),
            code,
            message,
            request_id,
        )
    } else {
        gateway_error(status, code, message, request_id)
    }
}

async fn hello_handler() -> StatusCode {
    StatusCode::NO_CONTENT
}

async fn models_handler(State(state): State<AppState>, req: Request) -> Response {
    let request_id = util::new_request_id();
    let cfg = state.cfg();
    let client = req
        .extensions()
        .get::<AuthedClient>()
        .map(|c| c.0.name.clone());
    let payload = registry::models_payload(&cfg);
    let visible = visible_models(&cfg, client.as_deref(), &payload);
    let body = serde_json::to_vec(&visible).unwrap_or_default();
    Response::builder()
        .status(StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .header("x-umsp-request-id", request_id)
        .body(Body::from(body))
        .expect("valid response")
}

fn visible_models(cfg: &Config, client: Option<&str>, payload: &Value) -> Value {
    let Some(client_name) = client else {
        return payload.clone();
    };
    let Some(client) = cfg.client(client_name) else {
        return payload.clone();
    };
    if client.allowed_models.is_empty() {
        return payload.clone();
    }
    let data: Vec<Value> = payload["data"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter(|row| {
                    row.get("id")
                        .and_then(Value::as_str)
                        .is_some_and(|id| util::any_glob_match(&client.allowed_models, id))
                })
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    json!({ "object": "list", "data": data })
}

/// Admin endpoints (`/healthz`, `/metrics`) answer loopback peers only
/// (spec 6.2 / 8.5). A request with no `ConnectInfo` never crossed a socket —
/// the in-process router used by tests — so it is allowed.
async fn admin_loopback_guard(req: Request, next: Next) -> Response {
    match req.extensions().get::<ConnectInfo<SocketAddr>>() {
        Some(ConnectInfo(addr)) if !addr.ip().is_loopback() => loopback_only_response(),
        _ => next.run(req).await,
    }
}

fn loopback_only_response() -> Response {
    Response::builder()
        .status(StatusCode::FORBIDDEN)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            "{\"error\":{\"message\":\"admin endpoints are loopback-only\",\
             \"type\":\"permission_error\",\"code\":\"loopback_only\"}}",
        ))
        .expect("valid response")
}

async fn healthz_handler(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let probe = params
        .get("probe")
        .map(|v| v == "1" || v == "true")
        .unwrap_or(false);
    let cfg = state.cfg();
    let mut providers = serde_json::Map::new();
    let mut overall_ok = true;

    for provider in &cfg.providers {
        let circuit = state.circuits().view(&provider.id);
        let mut entry = json!({
            "status": "ok",
            "circuit": circuit.state,
            "failures": circuit.failures,
            "opened_at": circuit.opened_at,
            "reason": circuit.reason,
            "models": cfg.models.iter().filter(|m| m.backends.iter().any(|b| b.provider == provider.id)).count(),
        });
        if circuit.state != CircuitState::Closed {
            entry["status"] = json!("degraded");
            overall_ok = false;
        }
        let health = state.provider_health(&provider.id);
        if let Some(status) = health.last_status {
            entry["last_status"] = json!(status);
            entry["last_success"] = json!(health.last_success);
            entry["last_error"] = json!(health.last_error);
            entry["last_latency_ms"] = json!(health.last_latency_ms);
            if status >= 400 {
                entry["status"] = json!("degraded");
                overall_ok = false;
            }
        }
        if probe {
            let probe_value = probe_provider(&state, &cfg, provider).await;
            if let Some(balance) = probe_value.get("balance") {
                entry["balance"] = balance.clone();
            }
            if probe_value["status"] != "ok" {
                overall_ok = false;
            }
            entry["probe"] = probe_value;
        }
        providers.insert(provider.id.clone(), entry);
    }

    let payload = json!({
        "status": if overall_ok { "ok" } else { "degraded" },
        "version": VERSION,
        "uptime_s": state.uptime_s(),
        "providers": providers,
    });
    let body = serde_json::to_vec(&payload).unwrap_or_default();
    let status = if overall_ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    // Health endpoints conventionally stay 200 for "degraded"; the body carries state.
    let _ = status;
    Response::builder()
        .status(StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .expect("valid response")
}

/// `GET /metrics` — Prometheus text exposition (spec 6.2). Loopback only, no
/// client key required, never triggers an upstream request.
async fn metrics_handler(State(state): State<AppState>) -> Response {
    let cfg = state.cfg();
    let circuits: Vec<(String, bool)> = cfg
        .providers
        .iter()
        .map(|provider| {
            let open = state.circuits().view(&provider.id).state != CircuitState::Closed;
            (provider.id.clone(), open)
        })
        .collect();
    let body = state.metrics().render(&circuits);
    Response::builder()
        .status(StatusCode::OK)
        .header(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )
        .body(Body::from(body))
        .expect("valid response")
}

async fn probe_provider(state: &AppState, cfg: &Config, provider: &Provider) -> Value {
    if let Some(cached) = state.probe_cached(&provider.id, Duration::from_secs(30)) {
        return cached;
    }
    // DeepSeek: check the balance first — an empty account is a definitive
    // answer and must not burn a token (spec 6.1).
    let mut balance_info: Option<Balance> = None;
    if provider.quirks.as_deref() == Some("deepseek") {
        match deepseek_balance(state, provider).await {
            Ok(balance) => {
                if !balance.is_available {
                    let value = json!({
                        "status": "degraded",
                        "error": "insufficient_balance",
                        "balance": balance.to_json(),
                    });
                    state.probe_store(&provider.id, value.clone());
                    return value;
                }
                balance_info = Some(balance);
            }
            Err(err) => {
                let value = json!({"status": "error", "error": err});
                state.probe_store(&provider.id, value.clone());
                return value;
            }
        }
    }
    let Some(upstream) = cfg.models.iter().find_map(|m| {
        m.backends
            .iter()
            .find(|b| b.provider == provider.id)
            .map(|b| b.upstream.clone())
    }) else {
        let value = json!({"status": "unknown", "error": "no model configured for provider"});
        state.probe_store(&provider.id, value.clone());
        return value;
    };
    let up = state.upstream();
    let started = Instant::now();
    let outcome = if provider.supports("chat") {
        let body = json!({
            "model": upstream,
            "messages": [{"role": "user", "content": "ping"}],
            "max_tokens": 1,
            "reasoning_effort": "none",
            "stream": false
        });
        up.chat(provider, &upstream, &body, &HeaderMap::new()).await
    } else if provider.supports("responses") {
        let body = json!({
            "model": upstream,
            "input": "ping",
            "max_output_tokens": 16,
            "stream": false
        });
        up.responses(provider, &body, &HeaderMap::new()).await
    } else {
        let value = json!({"status": "unknown", "error": "provider has no probeable protocol"});
        state.probe_store(&provider.id, value.clone());
        return value;
    };
    let mut value = match outcome {
        Ok(outcome) => {
            state.note_health(&provider.id, outcome.status, outcome.duration_ms, None);
            if outcome.status < 400 {
                json!({"status": "ok", "latency_ms": outcome.duration_ms})
            } else {
                json!({"status": "degraded", "http_status": outcome.status, "latency_ms": outcome.duration_ms})
            }
        }
        Err(err) => {
            let latency = started.elapsed().as_millis() as u64;
            state.note_health(&provider.id, 0, latency, Some(err.code().to_string()));
            json!({"status": "error", "error": err.code(), "latency_ms": latency})
        }
    };
    if let Some(balance) = balance_info {
        value["balance"] = balance.to_json();
    }
    state.probe_store(&provider.id, value.clone());
    value
}

/// Everything a protocol handler needs after auth, body parsing and routing.
struct Prepared {
    cfg: Arc<Config>,
    client: Client,
    request_id: String,
    started: Instant,
    bytes_in: u64,
    payload: Value,
    requested_model: String,
    model: Model,
    client_headers: HeaderMap,
    /// Raw client query string (`?beta=true` on Messages); relayed upstream.
    query: Option<String>,
}

/// Auth, body limit, JSON parse, model resolution, allowlist and protocol check.
///
/// Shared by `/v1/chat/completions` and `/v1/responses` so the two entry points
/// cannot drift apart. On failure the gateway error response is returned and a
/// ledger row has already been written.
// The error path is a fully built `Response` (headers + body); boxing it would
// only add an allocation on the rare failure path.
#[allow(clippy::result_large_err)]
async fn prepare_request(
    state: &AppState,
    req: Request,
    protocol: &str,
) -> std::result::Result<Prepared, Response> {
    let request_id = util::new_request_id();
    let started = Instant::now();
    let client = match req.extensions().get::<AuthedClient>() {
        Some(c) => c.0.clone(),
        None => {
            return Err(protocol_gateway_error(
                protocol,
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "missing or invalid API key",
                &request_id,
            ));
        }
    };

    let cfg = state.cfg();
    let (parts, body) = req.into_parts();
    let query = parts.uri.query().map(|q| q.to_string());
    let max_body = cfg.server.max_body_bytes;

    if let Some(len) = parts
        .headers
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok())
    {
        if len > max_body {
            log_gateway_error(
                state,
                &cfg,
                &request_id,
                &client.name,
                protocol,
                "",
                "",
                StatusCode::PAYLOAD_TOO_LARGE,
                "request_too_large",
                len as u64,
                started.elapsed().as_millis() as u64,
                false,
            );
            return Err(protocol_gateway_error(
                protocol,
                StatusCode::PAYLOAD_TOO_LARGE,
                "request_too_large",
                &format!("request body {len} bytes exceeds the {max_body} byte limit"),
                &request_id,
            ));
        }
    }

    let bytes = match axum::body::to_bytes(body, max_body).await {
        Ok(bytes) => bytes,
        Err(err) => {
            let status = if err
                .to_string()
                .to_ascii_lowercase()
                .contains("length limit")
            {
                StatusCode::PAYLOAD_TOO_LARGE
            } else {
                StatusCode::BAD_REQUEST
            };
            let code = if status == StatusCode::PAYLOAD_TOO_LARGE {
                "request_too_large"
            } else {
                "invalid_body"
            };
            log_gateway_error(
                state,
                &cfg,
                &request_id,
                &client.name,
                protocol,
                "",
                "",
                status,
                code,
                0,
                started.elapsed().as_millis() as u64,
                false,
            );
            return Err(protocol_gateway_error(
                protocol,
                status,
                code,
                &err.to_string(),
                &request_id,
            ));
        }
    };
    let bytes_in = bytes.len() as u64;

    let payload: Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(err) => {
            log_gateway_error(
                state,
                &cfg,
                &request_id,
                &client.name,
                protocol,
                "",
                "",
                StatusCode::BAD_REQUEST,
                "invalid_json",
                bytes_in,
                started.elapsed().as_millis() as u64,
                false,
            );
            return Err(protocol_gateway_error(
                protocol,
                StatusCode::BAD_REQUEST,
                "invalid_json",
                &format!("request body is not valid JSON: {err}"),
                &request_id,
            ));
        }
    };

    let requested_model = payload
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    // A hard gateway rule, checked before routing so a bad status cannot leak
    // through for an unknown model (spec 2.2): both upstreams are unreliable
    // for `previous_response_id` (OD 400s on a fake id, DS silently ignores it
    // and loses context), so the client must send the full input history.
    if protocol == "responses"
        && payload
            .get("previous_response_id")
            .is_some_and(|value| !value.is_null())
    {
        log_gateway_error(
            state,
            &cfg,
            &request_id,
            &client.name,
            protocol,
            &requested_model,
            "",
            StatusCode::BAD_REQUEST,
            "unsupported_parameter",
            bytes_in,
            started.elapsed().as_millis() as u64,
            false,
        );
        return Err(protocol_gateway_error(
            protocol,
            StatusCode::BAD_REQUEST,
            "unsupported_parameter",
            "previous_response_id is not supported by this gateway; send the full input history",
            &request_id,
        ));
    }

    if requested_model.is_empty() {
        log_gateway_error(
            state,
            &cfg,
            &request_id,
            &client.name,
            protocol,
            "",
            "",
            StatusCode::BAD_REQUEST,
            "missing_model",
            bytes_in,
            started.elapsed().as_millis() as u64,
            false,
        );
        return Err(protocol_gateway_error(
            protocol,
            StatusCode::BAD_REQUEST,
            "missing_model",
            "request body must contain a 'model' field",
            &request_id,
        ));
    }

    let Some(model) = registry::resolve(&cfg, &requested_model).cloned() else {
        log_gateway_error(
            state,
            &cfg,
            &request_id,
            &client.name,
            protocol,
            &requested_model,
            "",
            StatusCode::NOT_FOUND,
            "model_not_found",
            bytes_in,
            started.elapsed().as_millis() as u64,
            false,
        );
        return Err(protocol_gateway_error(
            protocol,
            StatusCode::NOT_FOUND,
            "model_not_found",
            &format!("unknown model '{requested_model}'"),
            &request_id,
        ));
    };

    if !util::any_glob_match(&client.allowed_models, &model.id) {
        log_gateway_error(
            state,
            &cfg,
            &request_id,
            &client.name,
            protocol,
            &requested_model,
            "",
            StatusCode::FORBIDDEN,
            "model_not_allowed",
            bytes_in,
            started.elapsed().as_millis() as u64,
            false,
        );
        return Err(protocol_gateway_error(
            protocol,
            StatusCode::FORBIDDEN,
            "model_not_allowed",
            &format!(
                "client '{}' is not allowed to use model '{}'",
                client.name, model.id
            ),
            &request_id,
        ));
    }

    if !client.allowed_protocols.iter().any(|p| p == protocol) {
        log_gateway_error(
            state,
            &cfg,
            &request_id,
            &client.name,
            protocol,
            &requested_model,
            "",
            StatusCode::FORBIDDEN,
            "protocol_not_allowed",
            bytes_in,
            started.elapsed().as_millis() as u64,
            false,
        );
        return Err(protocol_gateway_error(
            protocol,
            StatusCode::FORBIDDEN,
            "protocol_not_allowed",
            &format!(
                "client '{}' is not allowed to use the {protocol} protocol",
                client.name
            ),
            &request_id,
        ));
    }

    Ok(Prepared {
        cfg,
        client,
        request_id,
        started,
        bytes_in,
        payload,
        requested_model,
        model,
        client_headers: parts.headers,
        query,
    })
}

async fn chat_handler(State(state): State<AppState>, req: Request) -> Response {
    let Prepared {
        cfg,
        client,
        request_id,
        started,
        bytes_in,
        mut payload,
        requested_model,
        model,
        client_headers,
        ..
    } = match prepare_request(&state, req, "chat").await {
        Ok(prepared) => prepared,
        Err(response) => return response,
    };

    // `stream` is only a stream when the client sent a real boolean; anything
    // else (`"yes"`, 1, null) is handled as `false` — including on the wire,
    // because strict upstreams (OD) reject a non-boolean `stream` with 400.
    let stream_field = payload.get("stream").cloned();
    let stream_requested = stream_field
        .as_ref()
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let non_boolean_stream = stream_field.as_ref().is_some_and(|v| !v.is_boolean());

    // Only protocol-level adjustments happen here; `model` and the provider's
    // request quirks are applied per attempt, because two backends of the same
    // model may need different patches.
    let mut request_quirks = Vec::new();
    if non_boolean_stream {
        request_quirks.push("stream_non_boolean".to_string());
    }
    if let Some(obj) = payload.as_object_mut() {
        if non_boolean_stream {
            obj.insert("stream".to_string(), Value::Bool(false));
        }
    }
    if stream_requested && inject_stream_include_usage(&mut payload) {
        request_quirks.push("injected_include_usage".to_string());
    }

    if stream_requested {
        return proxy_stream(
            state,
            cfg,
            client,
            request_id,
            started,
            bytes_in,
            requested_model,
            model,
            payload,
            client_headers,
            request_quirks,
            None,
            StreamProtocol::Chat,
        )
        .await;
    }

    proxy_non_stream(
        state,
        cfg,
        client,
        request_id,
        started,
        bytes_in,
        requested_model,
        model,
        payload,
        client_headers,
        request_quirks,
        NonStreamCall::Chat,
    )
    .await
}

/// Which non-streaming upstream call a handler is making.
///
/// `MessagesCountTokens` rides the Messages protocol but has its own endpoint
/// (`/v1/messages/count_tokens`) and its own usage shape (spec 2.3).
#[derive(Debug, Clone)]
enum NonStreamCall {
    Chat,
    Responses,
    Messages { query: Option<String> },
    MessagesCountTokens,
}

impl NonStreamCall {
    /// Ledger `protocol` value: count_tokens is a Messages-protocol call.
    fn protocol(&self) -> &'static str {
        match self {
            NonStreamCall::Chat => "chat",
            NonStreamCall::Responses => "responses",
            NonStreamCall::Messages { .. } | NonStreamCall::MessagesCountTokens => "messages",
        }
    }

    fn query(&self) -> Option<&str> {
        match self {
            NonStreamCall::Messages { query } => query.as_deref(),
            _ => None,
        }
    }

    /// Ledger quirk label for calls that are not one of the three entry points.
    fn quirk(&self) -> Option<&'static str> {
        match self {
            NonStreamCall::MessagesCountTokens => Some("count_tokens"),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Fallback planning and circuit accounting (spec 3.3 / 3.4)
// ---------------------------------------------------------------------------

/// One candidate upstream for a request: a logical model served through one
/// provider/upstream pair.
#[derive(Debug, Clone)]
struct AttemptTarget {
    model: Model,
    provider: Provider,
    upstream: String,
    /// Protocol actually sent upstream. Equals the client protocol on a
    /// passthrough, and a protocol the provider lists when translating.
    upstream_protocol: String,
}

/// Outcome of one attempt, before anything reaches the client.
enum AttemptResult {
    Response(ChatOutcome),
    Stream(ChatStream),
    Error(ProxyError),
    CircuitOpen {
        reason: Option<String>,
    },
    /// The client body could not be translated into this provider's protocol.
    /// Retryable via a later backend, and not a circuit failure.
    Untranslatable {
        message: String,
    },
}

impl AttemptResult {
    /// Whether switching to a fallback may help (spec 3.3: connection error,
    /// timeout, 429, 5xx, 402). A circuit-open primary is also retryable *via a
    /// fallback* — the fallback itself is never attempted twice.
    fn retryable(&self) -> bool {
        match self {
            AttemptResult::Response(outcome) => upstream::status_is_retryable(outcome.status),
            AttemptResult::Stream(stream) => upstream::status_is_retryable(stream.status),
            AttemptResult::Error(_)
            | AttemptResult::CircuitOpen { .. }
            | AttemptResult::Untranslatable { .. } => true,
        }
    }

    fn reason(&self) -> String {
        match self {
            AttemptResult::Response(outcome) => format!("http_{}", outcome.status),
            AttemptResult::Stream(stream) => format!("http_{}", stream.status),
            AttemptResult::Error(err) => err.code().to_string(),
            AttemptResult::CircuitOpen { reason } => format!(
                "circuit_open:{}",
                reason.as_deref().unwrap_or("provider_circuit_open")
            ),
            AttemptResult::Untranslatable { message } => message.clone(),
        }
    }

    /// Ledger-style code for an upstream fault on this attempt, if any.
    ///
    /// `umsp_upstream_errors_total` counts every failed attempt — including the
    /// ones a retry or a fallback absorbed — while `umsp_requests_total` counts
    /// one finished request under its final provider/model.
    fn upstream_fault(&self) -> Option<String> {
        match self {
            AttemptResult::Response(outcome) if outcome.status >= 400 => {
                Some(format!("upstream_{}", outcome.status))
            }
            AttemptResult::Stream(stream) if stream.status >= 400 => {
                Some(format!("upstream_{}", stream.status))
            }
            AttemptResult::Error(ProxyError::Timeout(_)) => Some("upstream_timeout".to_string()),
            AttemptResult::Error(ProxyError::Network(_)) => {
                Some("upstream_unreachable".to_string())
            }
            _ => None,
        }
    }
}

/// Ordered backends that can serve `model`.
///
/// A backend is eligible when its provider exists and speaks at least one of
/// the three protocols. The attempt uses the client protocol when the provider
/// lists it (passthrough) and otherwise the first protocol in that provider's
/// list (translation). `count_tokens` stays strict: the provider must speak
/// Messages and implement the endpoint. Order is config order.
fn attempt_candidates(
    cfg: &Config,
    model: &Model,
    protocol: &str,
    count_tokens: bool,
) -> Vec<AttemptTarget> {
    model
        .backends
        .iter()
        .filter_map(|backend| {
            let provider = cfg.providers.iter().find(|p| p.id == backend.provider)?;
            let upstream_protocol = if count_tokens {
                if !provider.supports(protocol) || !provider.supports_count_tokens() {
                    return None;
                }
                protocol
            } else {
                crate::translate::select_upstream_protocol(&provider.protocols, protocol)?
            };
            Some(AttemptTarget {
                model: model.clone(),
                provider: provider.clone(),
                upstream: backend.upstream.clone(),
                upstream_protocol: upstream_protocol.to_string(),
            })
        })
        .collect()
}

/// Body for one attempt. Passthrough keeps the client JSON and only rewrites
/// `model` plus provider quirks. A protocol mismatch is translated first.
fn prepare_attempt(
    base: &Value,
    target: &AttemptTarget,
    client_protocol: &str,
    client_name: &str,
) -> std::result::Result<(Value, Vec<String>), String> {
    if target.upstream_protocol == client_protocol {
        return Ok(attempt_payload(base, target, client_protocol, client_name));
    }
    let translated = crate::translate::translate_request_with_budget(
        base,
        client_protocol,
        &target.upstream_protocol,
        Some(target.model.max_completion_tokens),
    )
    .map_err(|err| {
        format!(
            "cannot translate {client_protocol} request to {}: {err}",
            target.upstream_protocol
        )
    })?;
    let mut notes = vec![format!(
        "translated:{client_protocol}->{}",
        target.upstream_protocol
    )];
    notes.extend(translated.notes);
    let mut body = translated.body;
    if let Some(obj) = body.as_object_mut() {
        obj.insert("model".to_string(), Value::String(target.upstream.clone()));
    }
    if target.upstream_protocol == "chat" {
        match target.provider.quirks.as_deref() {
            Some("opendesign") => notes.extend(quirks::apply_request_quirks(&mut body)),
            Some("deepseek") => {
                notes.extend(quirks::apply_deepseek_request_quirks(
                    &mut body,
                    client_name,
                ));
            }
            _ => {}
        }
    }
    Ok((body, notes))
}

/// Passthrough keeps the client's headers. A translated hop drops Anthropic
/// framing when the upstream protocol is not Messages, and fills
/// `anthropic-version` when the upstream protocol is Messages.
fn headers_for_attempt(
    headers: &HeaderMap,
    client_protocol: &str,
    upstream_protocol: &str,
) -> HeaderMap {
    if client_protocol == upstream_protocol {
        return headers.clone();
    }
    let mut out = headers.clone();
    if upstream_protocol == "messages" {
        if !out.contains_key("anthropic-version") {
            out.insert(
                "anthropic-version",
                HeaderValue::from_static(DEFAULT_ANTHROPIC_VERSION),
            );
        }
        return out;
    }
    let strip: Vec<HeaderName> = out
        .keys()
        .filter(|name| {
            let lower = name.as_str();
            lower.starts_with("anthropic-") || lower.starts_with("x-claude-")
        })
        .cloned()
        .collect();
    for name in strip {
        out.remove(name);
    }
    out
}

/// Body for one attempt: `model` is rewritten to the backend's upstream id and
/// the target provider's request quirks are applied. Returns the applied quirk
/// tags for the ledger.
fn attempt_payload(
    base: &Value,
    target: &AttemptTarget,
    protocol: &str,
    client_name: &str,
) -> (Value, Vec<String>) {
    let mut payload = base.clone();
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("model".to_string(), Value::String(target.upstream.clone()));
    }
    let mut applied = Vec::new();
    // Quirks are provider behaviour, so they run per attempt: two backends of
    // the same model may need different patches.
    if protocol == "chat" {
        match target.provider.quirks.as_deref() {
            Some("opendesign") => applied.extend(quirks::apply_request_quirks(&mut payload)),
            Some("deepseek") => applied.extend(quirks::apply_deepseek_request_quirks(
                &mut payload,
                client_name,
            )),
            _ => {}
        }
    }
    (payload, applied)
}

/// Feed one upstream status into the provider's circuit (spec 3.4).
///
/// `probe` is `true` when this attempt held the single half-open probe slot. A
/// non-retryable client error on a probe proves the provider is reachable, so
/// it closes the circuit instead of leaving it stuck in `half_open`; the probe
/// guard would release the slot either way.
fn note_circuit_result(state: &AppState, target: &AttemptTarget, status: u16, probe: bool) {
    let circuits = state.circuits();
    if status < 400 {
        if circuits.record_success(&target.provider.id).is_some() {
            plog_health(
                "circuit_close",
                &target.provider.id,
                &target.upstream,
                "success",
            );
        }
        return;
    }
    let config = circuits.config();
    match status {
        401 => {
            if circuits.force_open(&target.provider.id, "auth_failed", config.auth_cooldown) {
                plog_health(
                    "circuit_open",
                    &target.provider.id,
                    &target.upstream,
                    "http_401",
                );
            }
        }
        402 => {
            let deepseek = target.provider.quirks.as_deref() == Some("deepseek");
            let (reason, cooldown) = if deepseek {
                ("insufficient_balance", config.ds_balance_cooldown)
            } else {
                ("payment_required", config.auth_cooldown)
            };
            if circuits.force_open(&target.provider.id, reason, cooldown) {
                plog_health(
                    "circuit_open",
                    &target.provider.id,
                    &target.upstream,
                    &format!("http_402:{reason}"),
                );
            }
        }
        status if status == 429 || (500..=599).contains(&status) => {
            let reason = format!("http_{status}");
            if circuits.record_failure(&target.provider.id, &reason) {
                plog_health(
                    "circuit_open",
                    &target.provider.id,
                    &target.upstream,
                    &reason,
                );
            }
        }
        _ => {
            // 400/403/404/422 and friends: never a circuit failure. A probe that
            // got an answer at all closes the circuit (the provider is reachable
            // and the key works).
            if probe && circuits.close(&target.provider.id) {
                plog_health(
                    "circuit_close",
                    &target.provider.id,
                    &target.upstream,
                    &format!("probe_reachable:http_{status}"),
                );
            }
        }
    }
}

/// Connection error / timeout: a retryable failure that counts toward the
/// circuit (spec 3.4).
fn note_circuit_error(state: &AppState, target: &AttemptTarget, err: &ProxyError) {
    if matches!(err, ProxyError::Auth(_)) {
        let cooldown = state.circuits().config().auth_cooldown;
        if state
            .circuits()
            .force_open(&target.provider.id, "auth_failed", cooldown)
        {
            plog_health(
                "circuit_open",
                &target.provider.id,
                &target.upstream,
                "auth_failed",
            );
        }
        return;
    }
    if state
        .circuits()
        .record_failure(&target.provider.id, err.code())
    {
        plog_health(
            "circuit_open",
            &target.provider.id,
            &target.upstream,
            err.code(),
        );
    }
}

/// Operational log event for the circuit state machine (no keys, no bodies).
fn plog_health(event: &str, provider: &str, model: &str, reason: &str) {
    eprintln!(
        "{}",
        json!({
            "ts": util::now_rfc3339(),
            "level": if event == "circuit_open" { "warn" } else { "info" },
            "event": event,
            "provider": provider,
            "model": model,
            "reason": reason,
        })
    );
}

/// Operational log event for a fallback hop.
fn plog_fallback(
    request_id: &str,
    client: &str,
    from_provider: &str,
    from_model: &str,
    to_provider: &str,
    to_model: &str,
    reason: &str,
) {
    eprintln!(
        "{}",
        json!({
            "ts": util::now_rfc3339(),
            "level": "warn",
            "event": "fallback",
            "request_id": request_id,
            "client": client,
            "provider": to_provider,
            "model": to_model,
            "from_provider": from_provider,
            "from_model": from_model,
            "reason": reason,
        })
    );
}

/// 400 when the logical model exists but no backend can serve this protocol.
fn no_backend_response(protocol: &str, model_id: &str, request_id: &str) -> Response {
    protocol_gateway_error(
        protocol,
        StatusCode::BAD_REQUEST,
        "no_backend_for_protocol",
        &format!("model '{model_id}' has no usable backend"),
        request_id,
    )
}

/// Protocol-native 503 for a circuit-open provider that has no usable fallback
/// (spec 3.4).
fn circuit_open_response(protocol: &str, request_id: &str) -> Response {
    protocol_gateway_error(
        protocol,
        StatusCode::SERVICE_UNAVAILABLE,
        "temporarily_unavailable",
        "the requested model is temporarily unavailable; retry after the cooldown",
        request_id,
    )
}

// ---------------------------------------------------------------------------
// Client-facing surface: provider identity never leaves the gateway
// ---------------------------------------------------------------------------

/// Upstream headers a client may see.
///
/// Only representation metadata and the neutral `retry-after` survive; vendor
/// `server`/trace/cookie headers would reveal which upstream served the call.
fn client_response_headers(
    headers: &[(HeaderName, HeaderValue)],
) -> Vec<(HeaderName, HeaderValue)> {
    headers
        .iter()
        .filter(|(name, _)| matches!(name.as_str(), "content-type" | "retry-after"))
        .cloned()
        .collect()
}

fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_')
}

/// Replace `needle` only where it stands alone, so `deepseek` inside
/// `deepseek-v4.1-flash` is left alone.
fn replace_token(text: &str, needle: &str, replacement: &str) -> String {
    if needle.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find(needle) {
        let before_ok = rest[..pos]
            .chars()
            .next_back()
            .map(|c| !is_token_char(c))
            .unwrap_or(true);
        let after = &rest[pos + needle.len()..];
        let after_ok = after
            .chars()
            .next()
            .map(|c| !is_token_char(c))
            .unwrap_or(true);
        out.push_str(&rest[..pos]);
        if before_ok && after_ok {
            out.push_str(replacement);
        } else {
            out.push_str(needle);
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

/// Rewrite upstream identifiers out of a body the client will see.
fn scrub_upstream_identifiers(body: &[u8], model_id: &str, target: &AttemptTarget) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(body) else {
        return body.to_vec();
    };
    let text = replace_token(text, &target.upstream, model_id);
    replace_token(&text, &target.provider.id, "umsp").into_bytes()
}

/// Keep a bounded copy of an upstream body in the ledger for diagnosis.
fn truncate_body(body: &[u8]) -> String {
    const LIMIT: usize = 2048;
    let text = String::from_utf8_lossy(body);
    let mut out: String = text.chars().take(LIMIT).collect();
    if text.chars().count() > LIMIT {
        out.push('…');
    }
    out
}

/// Turn a final upstream failure into a client-facing response.
///
/// Client-caused 4xx keep their status and their (scrubbed) body so callers can
/// still fix their requests. Everything else becomes a UMSP envelope with a
/// neutral message: the upstream status and body stay in the ledger, keyed by
/// `x-umsp-request-id`.
/// Status the client sees for an upstream failure. Client-caused 4xx keep their
/// status; authentication, quota and server failures become gateway errors so a
/// caller never mistakes an upstream problem for its own.
fn mapped_upstream_status(status: u16) -> u16 {
    match status {
        401..=403 | 500..=599 => 502,
        other => other,
    }
}

fn upstream_failure_response(
    protocol: &str,
    status: u16,
    body: &[u8],
    model_id: &str,
    target: &AttemptTarget,
    request_id: &str,
    rewrite_error: bool,
) -> Response {
    if (400..500).contains(&status) && !matches!(status, 401..=403 | 429) {
        if rewrite_error {
            let message = crate::translate::error_message(body)
                .map(|message| {
                    String::from_utf8_lossy(&scrub_upstream_identifiers(
                        message.as_bytes(),
                        model_id,
                        target,
                    ))
                    .into_owned()
                })
                .unwrap_or_else(|| "the upstream service rejected the request".to_string());
            return protocol_gateway_error(
                protocol,
                StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST),
                "upstream_rejected",
                &message,
                request_id,
            );
        }
        if body.is_empty() {
            return protocol_gateway_error(
                protocol,
                StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST),
                "upstream_rejected",
                "the upstream service rejected the request",
                request_id,
            );
        }
        let builder = Response::builder()
            .status(StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST))
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .header("x-umsp-request-id", request_id);
        return builder
            .body(Body::from(scrub_upstream_identifiers(
                body, model_id, target,
            )))
            .unwrap_or_else(|_| {
                gateway_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "response_build_failed",
                    "failed to build response",
                    request_id,
                )
            });
    }

    let client_status =
        StatusCode::from_u16(mapped_upstream_status(status)).unwrap_or(StatusCode::BAD_GATEWAY);
    let (status, code, message) = match status {
        429 => (
            client_status,
            "rate_limited",
            "the upstream service is rate limiting requests; retry later",
        ),
        401 | 403 => (
            client_status,
            "upstream_auth_failed",
            "the gateway could not authenticate with the upstream service",
        ),
        402 => (
            client_status,
            "upstream_quota_exhausted",
            "the upstream service reported an exhausted quota",
        ),
        _ => (
            client_status,
            "upstream_error",
            "the upstream service failed to handle the request",
        ),
    };
    protocol_gateway_error(protocol, status, code, message, request_id)
}

/// Shared non-streaming proxy path: one upstream call, one ledger row.
///
/// A provider that speaks the client protocol relays the body. A mismatch
/// translates the successful body back into the client's protocol.
#[allow(clippy::too_many_arguments)]
async fn proxy_non_stream(
    state: AppState,
    cfg: Arc<Config>,
    client: Client,
    request_id: String,
    started: Instant,
    bytes_in: u64,
    _requested_model: String,
    model: Model,
    payload: Value,
    client_headers: HeaderMap,
    request_quirks: Vec<String>,
    call: NonStreamCall,
) -> Response {
    let up = Upstream::with_clients(
        cfg.clone(),
        state.env().clone(),
        state.http().clone(),
        state.stream_http().clone(),
        state.sessions(),
    );
    let protocol = call.protocol();
    let count_tokens = matches!(call, NonStreamCall::MessagesCountTokens);

    let candidates = attempt_candidates(&cfg, &model, protocol, count_tokens);
    if candidates.is_empty() {
        return no_backend_response(protocol, &model.id, &request_id);
    }

    let mut fallbacks_used = 0u32;
    let mut previous_reason: Option<String> = None;
    let mut last: Option<(AttemptResult, AttemptTarget, Vec<String>)> = None;

    for (index, target) in candidates.iter().enumerate() {
        if index > 0 {
            fallbacks_used += 1;
            let previous = &candidates[index - 1];
            plog_fallback(
                &request_id,
                &client.name,
                &previous.provider.id,
                &previous.upstream,
                &target.provider.id,
                &target.upstream,
                previous_reason.as_deref().unwrap_or("retryable"),
            );
        }
        let (attempt_body, attempt_quirks) =
            match prepare_attempt(&payload, target, protocol, &client.name) {
                Ok((body, provider_quirks)) => {
                    let mut attempt_quirks = request_quirks.clone();
                    attempt_quirks.extend(provider_quirks);
                    (body, attempt_quirks)
                }
                Err(message) => {
                    previous_reason = Some(message.clone());
                    last = Some((
                        AttemptResult::Untranslatable { message },
                        target.clone(),
                        Vec::new(),
                    ));
                    if index + 1 < candidates.len() {
                        continue;
                    }
                    break;
                }
            };
        let attempt_headers =
            headers_for_attempt(&client_headers, protocol, &target.upstream_protocol);
        let admission = state.circuits().admit(&target.provider.id);
        // A granted half-open probe owns the single probe slot for the attempt.
        // Dropping the guard before a result is recorded (client cancel, aborted
        // task) only releases the slot; the circuit stays half_open.
        let _probe = match &admission {
            Admission::Probe(guard) => Some(guard),
            _ => None,
        };
        let _inflight = state.metrics().inflight(&target.provider.id);
        let result = match &admission {
            Admission::Reject => AttemptResult::CircuitOpen {
                reason: state.circuits().view(&target.provider.id).reason,
            },
            Admission::Allow | Admission::Probe(_) => {
                let attempt_started = Instant::now();
                let outcome = if count_tokens {
                    up.count_tokens(&target.provider, &attempt_body, &attempt_headers)
                        .await
                } else {
                    match target.upstream_protocol.as_str() {
                        "responses" => {
                            up.responses(&target.provider, &attempt_body, &attempt_headers)
                                .await
                        }
                        "messages" => {
                            let query = (protocol == "messages").then(|| call.query()).flatten();
                            up.messages(&target.provider, query, &attempt_body, &attempt_headers)
                                .await
                        }
                        _ => {
                            up.chat(
                                &target.provider,
                                &target.upstream,
                                &attempt_body,
                                &attempt_headers,
                            )
                            .await
                        }
                    }
                };
                match outcome {
                    Ok(outcome) => {
                        state.note_health(
                            &target.provider.id,
                            outcome.status,
                            outcome.duration_ms,
                            (outcome.status >= 400).then(|| format!("http_{}", outcome.status)),
                        );
                        note_circuit_result(&state, target, outcome.status, _probe.is_some());
                        AttemptResult::Response(outcome)
                    }
                    Err(err) => {
                        state.note_health(
                            &target.provider.id,
                            0,
                            attempt_started.elapsed().as_millis() as u64,
                            Some(err.code().to_string()),
                        );
                        note_circuit_error(&state, target, &err);
                        AttemptResult::Error(err)
                    }
                }
            }
        };
        let retryable = result.retryable();
        let has_next = index + 1 < candidates.len();
        if let Some(code) = result.upstream_fault() {
            state
                .metrics()
                .note_upstream_error(&target.provider.id, &code);
        }
        previous_reason = Some(result.reason());
        last = Some((result, target.clone(), attempt_quirks));
        if !(retryable && has_next) {
            break;
        }
    }

    let (result, target, attempt_quirks) = last.expect("at least one attempt candidate");
    let duration_ms = started.elapsed().as_millis() as u64;
    let mut record = request_record(
        &request_id,
        &client.name,
        protocol,
        &target.model.id,
        &target.provider.id,
        &target.upstream,
        false,
        bytes_in,
        duration_ms,
        attempt_quirks,
    );
    record.fallbacks = fallbacks_used;
    if fallbacks_used > 0 {
        record
            .quirks
            .push(format!("fallback_from:{}", candidates[0].provider.id));
    }
    if let Some(quirk) = call.quirk() {
        record.quirks.push(quirk.to_string());
    }

    match result {
        AttemptResult::Response(mut outcome) => {
            if outcome.status < 400 && target.upstream_protocol != protocol {
                match crate::translate::translate_response(
                    &outcome.body,
                    &target.upstream_protocol,
                    protocol,
                    &target.model.id,
                ) {
                    Ok(body) => outcome.body = body,
                    Err(err) => {
                        record.status = StatusCode::BAD_GATEWAY.as_u16();
                        record.upstream_status = Some(outcome.status);
                        record.error_code = Some("translation_failed".to_string());
                        record.duration_ms = outcome.duration_ms;
                        state.finish_request(&record);
                        return protocol_gateway_error(
                            protocol,
                            StatusCode::BAD_GATEWAY,
                            "translation_failed",
                            &err,
                            &request_id,
                        );
                    }
                }
            }
            record.status = mapped_upstream_status(outcome.status);
            record.upstream_status = Some(outcome.status);
            record.bytes_out = outcome.body.len() as u64;
            record.upstream_request_id = outcome.upstream_request_id.clone();
            record.duration_ms = outcome.duration_ms;
            record.quirks.extend(outcome.quirks.iter().cloned());
            if let Some(usage) = outcome.usage {
                record.prompt_tokens = usage.input_tokens;
                record.completion_tokens = usage.output_tokens;
                record.cached_tokens = usage.cached_tokens;
                record.reasoning_tokens = usage.reasoning_tokens;
            } else if outcome.status < 400 && (protocol == "responses" || protocol == "messages") {
                // Responses / Messages only: never estimate tokens, record that
                // we looked and the upstream did not report usage.
                record.quirks.push("usage_missing".to_string());
            }
            if outcome.status >= 400 {
                record.error_code = Some(format!("upstream_{}", outcome.status));
                record.upstream_body = Some(truncate_body(&outcome.body));
            }
            state.finish_request(&record);

            if outcome.status >= 400 {
                return upstream_failure_response(
                    protocol,
                    outcome.status,
                    &outcome.body,
                    &target.model.id,
                    &target,
                    &request_id,
                    target.upstream_protocol != protocol,
                );
            }

            let mut builder = Response::builder().status(outcome.status);
            for (name, value) in client_response_headers(&outcome.headers) {
                builder = builder.header(name, value);
            }
            builder
                .header("x-umsp-request-id", &request_id)
                .body(Body::from(outcome.body))
                .unwrap_or_else(|_| {
                    gateway_error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "response_build_failed",
                        "failed to build response",
                        &request_id,
                    )
                })
        }
        AttemptResult::Error(err) => {
            let (status, code, message) = match &err {
                ProxyError::MissingKey { .. } => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "upstream_key_missing",
                    "the gateway is missing credentials for an upstream service".to_string(),
                ),
                ProxyError::Auth(_) => (
                    StatusCode::BAD_GATEWAY,
                    "upstream_auth_failed",
                    "the gateway could not authenticate with the upstream service".to_string(),
                ),
                ProxyError::Timeout(_) => (
                    StatusCode::GATEWAY_TIMEOUT,
                    "upstream_timeout",
                    "the upstream service did not respond in time".to_string(),
                ),
                ProxyError::Network(_) => (
                    StatusCode::BAD_GATEWAY,
                    "upstream_unreachable",
                    "the upstream service could not be reached".to_string(),
                ),
            };
            record.status = status.as_u16();
            record.error_code = Some(code.to_string());
            state.finish_request(&record);
            plog(
                "warn",
                "upstream_error",
                &request_id,
                Some(&client.name),
                Some(&target.provider.id),
                Some(code),
            );
            gateway_error(status, code, &message, &request_id)
        }
        AttemptResult::CircuitOpen { reason } => {
            record.status = StatusCode::SERVICE_UNAVAILABLE.as_u16();
            record.error_code = Some("provider_circuit_open".to_string());
            state.finish_request(&record);
            plog(
                "warn",
                "circuit_open_reject",
                &request_id,
                Some(&client.name),
                Some(&target.provider.id),
                reason.as_deref().or(Some("provider_circuit_open")),
            );
            circuit_open_response(protocol, &request_id)
        }
        AttemptResult::Untranslatable { message } => {
            record.status = StatusCode::BAD_REQUEST.as_u16();
            record.error_code = Some("translation_failed".to_string());
            state.finish_request(&record);
            protocol_gateway_error(
                protocol,
                StatusCode::BAD_REQUEST,
                "translation_failed",
                &message,
                &request_id,
            )
        }
        AttemptResult::Stream(_) => unreachable!("non-streaming path cannot produce a stream"),
    }
}

/// `POST /v1/responses` — OpenAI Responses (OD provider, non-streaming and
/// streaming). Only `model` is rewritten; every other field is relayed as-is
/// (spec 2.2).
async fn responses_handler(State(state): State<AppState>, req: Request) -> Response {
    let Prepared {
        cfg,
        client,
        request_id,
        started,
        bytes_in,
        payload,
        requested_model,
        model,
        client_headers,
        ..
    } = match prepare_request(&state, req, "responses").await {
        Ok(prepared) => prepared,
        Err(response) => return response,
    };

    // `previous_response_id` was already rejected in `prepare_request` (before
    // routing, so it is 400 even for an unknown model). `model` is rewritten
    // per attempt.
    let emit_event_lines = cfg.server.responses_emit_event_lines;
    let stream_requested = payload
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    if stream_requested {
        return proxy_stream(
            state,
            cfg,
            client,
            request_id,
            started,
            bytes_in,
            requested_model,
            model,
            payload,
            client_headers,
            Vec::new(),
            None,
            StreamProtocol::Responses { emit_event_lines },
        )
        .await;
    }

    proxy_non_stream(
        state,
        cfg,
        client,
        request_id,
        started,
        bytes_in,
        requested_model,
        model,
        payload,
        client_headers,
        Vec::new(),
        NonStreamCall::Responses,
    )
    .await
}

/// Default Anthropic API version injected when the client omits it; OD's
/// `/v1/messages` rejects requests without one (spec 2.0 / 2.3).
const DEFAULT_ANTHROPIC_VERSION: &str = "2023-06-01";

fn with_default_anthropic_version(mut headers: HeaderMap) -> HeaderMap {
    if !headers.contains_key("anthropic-version") {
        headers.insert(
            "anthropic-version",
            HeaderValue::from_static(DEFAULT_ANTHROPIC_VERSION),
        );
    }
    headers
}

/// `POST /v1/messages` — Anthropic Messages (OD provider, non-streaming and
/// streaming). Only `model` is rewritten; the query string, every header
/// outside the deny list and every body field are relayed as-is (spec 2.3).
async fn messages_handler(State(state): State<AppState>, req: Request) -> Response {
    let Prepared {
        cfg,
        client,
        request_id,
        started,
        bytes_in,
        payload,
        requested_model,
        model,
        client_headers,
        query,
    } = match prepare_request(&state, req, "messages").await {
        Ok(prepared) => prepared,
        Err(response) => return response,
    };

    let client_headers = with_default_anthropic_version(client_headers);
    let stream_requested = payload
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    if stream_requested {
        return proxy_stream(
            state,
            cfg,
            client,
            request_id,
            started,
            bytes_in,
            requested_model,
            model,
            payload,
            client_headers,
            Vec::new(),
            query,
            StreamProtocol::Messages,
        )
        .await;
    }

    proxy_non_stream(
        state,
        cfg,
        client,
        request_id,
        started,
        bytes_in,
        requested_model,
        model,
        payload,
        client_headers,
        Vec::new(),
        NonStreamCall::Messages { query },
    )
    .await
}

/// `POST /v1/messages/count_tokens` (spec 2.3).
///
/// DeepSeek's Anthropic-compatible endpoint implements it, so the request is
/// proxied to `{anthropic_base_url}/v1/messages/count_tokens` and its answer
/// (`{"input_tokens": N}`) relayed. OpenDesign 404s on it, so the gateway
/// answers a protocol-native 501 instead of leaking that 404 — no upstream call
/// and no ledger row in that case (the M2 `/v1/responses/*` 501 convention).
async fn messages_count_tokens_handler(State(state): State<AppState>, req: Request) -> Response {
    let Prepared {
        cfg,
        client,
        request_id,
        started,
        bytes_in,
        payload,
        requested_model,
        model,
        client_headers,
        ..
    } = match prepare_request(&state, req, "messages").await {
        Ok(prepared) => prepared,
        Err(response) => return response,
    };

    if attempt_candidates(&cfg, &model, "messages", true).is_empty() {
        let message = format!(
            "count_tokens is not supported by any backend of model '{}'",
            model.id
        );
        return messages_error(
            StatusCode::NOT_IMPLEMENTED,
            "not_supported_error",
            "not_supported_error",
            &message,
            &request_id,
        );
    }

    let client_headers = with_default_anthropic_version(client_headers);
    proxy_non_stream(
        state,
        cfg,
        client,
        request_id,
        started,
        bytes_in,
        requested_model,
        model,
        payload,
        client_headers,
        Vec::new(),
        NonStreamCall::MessagesCountTokens,
    )
    .await
}

/// Anything under `/v1/responses/` that the gateway does not proxy: token
/// counting, retrieval (`GET /v1/responses/{id}`), cancel and `input_items`.
/// A 501 with `unsupported_endpoint` is the honest answer: the gateway does not
/// hold Responses state, so callers must send the full input history.
async fn responses_unsupported_handler(State(_state): State<AppState>, req: Request) -> Response {
    let request_id = util::new_request_id();
    let path = req.uri().path().to_string();
    let message = if path.ends_with("/input_tokens") {
        format!(
            "{path} is not supported by this gateway; count tokens with the upstream SDK or a local tokenizer"
        )
    } else {
        format!(
            "{path} is not proxied by this gateway; Responses state (retrieval/cancel/input_items) is not supported — send the full input history"
        )
    };
    gateway_error(
        StatusCode::NOT_IMPLEMENTED,
        "unsupported_endpoint",
        &message,
        &request_id,
    )
}

/// Ask the upstream for a trailing usage chunk when the client did not say.
///
/// Returns `true` when the request was modified. An explicit
/// `include_usage: false` is respected (the ledger then records
/// `usage_missing` rather than changing what the client asked for).
fn inject_stream_include_usage(payload: &mut Value) -> bool {
    let Some(obj) = payload.as_object_mut() else {
        return false;
    };
    match obj.get_mut("stream_options") {
        Some(Value::Object(opts)) => {
            if opts.contains_key("include_usage") {
                return false;
            }
            opts.insert("include_usage".to_string(), Value::Bool(true));
            true
        }
        // A non-object `stream_options` is the client's business; leave it.
        Some(_) => false,
        None => {
            obj.insert("stream_options".to_string(), json!({"include_usage": true}));
            true
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn request_record(
    request_id: &str,
    client: &str,
    protocol: &str,
    requested_model: &str,
    provider: &str,
    upstream_model: &str,
    stream: bool,
    bytes_in: u64,
    duration_ms: u64,
    quirks: Vec<String>,
) -> RequestRecord {
    RequestRecord {
        ts: util::now_rfc3339(),
        request_id: request_id.to_string(),
        client: client.to_string(),
        protocol: protocol.to_string(),
        requested_model: requested_model.to_string(),
        provider: provider.to_string(),
        upstream_model: upstream_model.to_string(),
        stream,
        status: 0,
        upstream_status: None,
        upstream_body: None,
        prompt_tokens: 0,
        completion_tokens: 0,
        cached_tokens: 0,
        reasoning_tokens: 0,
        ttft_ms: None,
        duration_ms,
        bytes_in,
        bytes_out: 0,
        error_code: None,
        upstream_request_id: None,
        client_disconnected: false,
        quirks,
        fallbacks: 0,
    }
}

/// Ledger guard for the streaming path: if the handler future is cancelled
/// because the client vanished before the stream opened, the row is still
/// written (status 499, `client_disconnected`) instead of disappearing.
struct StreamGuard {
    state: AppState,
    started: Instant,
    record: Option<RequestRecord>,
}

impl StreamGuard {
    fn new(state: AppState, record: RequestRecord, started: Instant) -> Self {
        Self {
            state,
            started,
            record: Some(record),
        }
    }

    fn record_mut(&mut self) -> &mut RequestRecord {
        self.record.as_mut().expect("stream record present")
    }

    fn take(&mut self) -> RequestRecord {
        self.record.take().expect("stream record present")
    }

    /// Write the row and disarm the guard.
    fn commit(&mut self) {
        if let Some(record) = self.record.take() {
            self.state.finish_request(&record);
        }
    }
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        if let Some(mut record) = self.record.take() {
            record.client_disconnected = true;
            record.status = 499;
            record.error_code = Some("client_disconnected".to_string());
            record.duration_ms = self.started.elapsed().as_millis() as u64;
            self.state.finish_request(&record);
        }
    }
}

/// Which protocol a streaming proxy is carrying, and how its bytes are shaped.
#[derive(Clone, Copy, Debug)]
enum StreamProtocol {
    Chat,
    Responses { emit_event_lines: bool },
    Messages,
}

impl StreamProtocol {
    fn name(self) -> &'static str {
        match self {
            StreamProtocol::Chat => "chat",
            StreamProtocol::Responses { .. } => "responses",
            StreamProtocol::Messages => "messages",
        }
    }

    fn observer(self) -> stream::Observer {
        match self {
            StreamProtocol::Chat => stream::Observer::chat(),
            StreamProtocol::Responses { emit_event_lines } => {
                stream::Observer::responses(emit_event_lines)
            }
            StreamProtocol::Messages => stream::Observer::messages(),
        }
    }
}

/// Streaming proxy for Chat Completions / Responses: open the upstream stream,
/// then hand the live body to `pump_stream` and an unaggregated byte stream to
/// the client.
#[allow(clippy::too_many_arguments)]
async fn proxy_stream(
    state: AppState,
    cfg: Arc<Config>,
    client: Client,
    request_id: String,
    started: Instant,
    bytes_in: u64,
    _requested_model: String,
    model: Model,
    payload: Value,
    client_headers: HeaderMap,
    request_quirks: Vec<String>,
    query: Option<String>,
    protocol: StreamProtocol,
) -> Response {
    let up = Upstream::with_clients(
        cfg.clone(),
        state.env().clone(),
        state.http().clone(),
        state.stream_http().clone(),
        state.sessions(),
    );
    let header_timeout = Duration::from_secs(cfg.server.stream_header_timeout_s.max(1));
    let protocol_name = protocol.name();

    let candidates = attempt_candidates(&cfg, &model, protocol_name, false);
    if candidates.is_empty() {
        return no_backend_response(protocol_name, &model.id, &request_id);
    }

    let first = &candidates[0];
    let mut guard = StreamGuard::new(
        state.clone(),
        request_record(
            &request_id,
            &client.name,
            protocol_name,
            &first.model.id,
            &first.provider.id,
            &first.upstream,
            true,
            bytes_in,
            started.elapsed().as_millis() as u64,
            Vec::new(),
        ),
        started,
    );

    // Backends are tried in config order; a retry is only possible before any
    // byte reaches the client, so everything up to the `200` below is safe.

    let mut fallbacks_used = 0u32;
    let mut previous_reason: Option<String> = None;
    let mut last: Option<(
        AttemptResult,
        AttemptTarget,
        Option<crate::metrics::Inflight>,
        Vec<String>,
    )> = None;

    for (index, target) in candidates.iter().enumerate() {
        if index > 0 {
            fallbacks_used += 1;
            let previous = &candidates[index - 1];
            plog_fallback(
                &request_id,
                &client.name,
                &previous.provider.id,
                &previous.upstream,
                &target.provider.id,
                &target.upstream,
                previous_reason.as_deref().unwrap_or("retryable"),
            );
        }
        let (attempt_body, attempt_quirks) =
            match prepare_attempt(&payload, target, protocol_name, &client.name) {
                Ok((body, provider_quirks)) => {
                    let mut attempt_quirks = request_quirks.clone();
                    attempt_quirks.extend(provider_quirks);
                    (body, attempt_quirks)
                }
                Err(message) => {
                    previous_reason = Some(message.clone());
                    last = Some((
                        AttemptResult::Untranslatable { message },
                        target.clone(),
                        None,
                        Vec::new(),
                    ));
                    if index + 1 < candidates.len() {
                        continue;
                    }
                    break;
                }
            };
        let attempt_headers =
            headers_for_attempt(&client_headers, protocol_name, &target.upstream_protocol);
        let admission = state.circuits().admit(&target.provider.id);
        // A granted half-open probe owns the single probe slot until this
        // attempt records a result; a cancelled request releases it on drop.
        let _probe = match &admission {
            Admission::Probe(guard) => Some(guard),
            _ => None,
        };
        // In-flight gauge: the guard travels with a live stream into the pump,
        // and is dropped as soon as an attempt ends.
        let inflight = state.metrics().inflight(&target.provider.id);
        let result = match &admission {
            Admission::Reject => AttemptResult::CircuitOpen {
                reason: state.circuits().view(&target.provider.id).reason,
            },
            Admission::Allow | Admission::Probe(_) => {
                let upstream_query = if target.upstream_protocol == "messages" {
                    query.as_deref()
                } else {
                    None
                };
                let opened = match target.upstream_protocol.as_str() {
                    "responses" => {
                        up.responses_stream(
                            &target.provider,
                            &attempt_body,
                            &attempt_headers,
                            header_timeout,
                        )
                        .await
                    }
                    "messages" => {
                        up.messages_stream(
                            &target.provider,
                            upstream_query,
                            &attempt_body,
                            &attempt_headers,
                            header_timeout,
                        )
                        .await
                    }
                    _ => {
                        up.chat_stream(
                            &target.provider,
                            &attempt_body,
                            &attempt_headers,
                            header_timeout,
                        )
                        .await
                    }
                };
                match opened {
                    Ok(stream) => {
                        state.note_health(
                            &target.provider.id,
                            stream.status,
                            started.elapsed().as_millis() as u64,
                            (stream.status >= 400).then(|| format!("http_{}", stream.status)),
                        );
                        note_circuit_result(&state, target, stream.status, _probe.is_some());
                        AttemptResult::Stream(stream)
                    }
                    Err(err) => {
                        state.note_health(
                            &target.provider.id,
                            0,
                            started.elapsed().as_millis() as u64,
                            Some(err.code().to_string()),
                        );
                        note_circuit_error(&state, target, &err);
                        AttemptResult::Error(err)
                    }
                }
            }
        };
        let retryable = result.retryable();
        let has_next = index + 1 < candidates.len();
        if let Some(code) = result.upstream_fault() {
            state
                .metrics()
                .note_upstream_error(&target.provider.id, &code);
        }
        previous_reason = Some(result.reason());
        last = Some((result, target.clone(), Some(inflight), attempt_quirks));
        if !(retryable && has_next) {
            break;
        }
    }

    let (result, target, inflight, attempt_quirks) = last.expect("at least one attempt candidate");
    {
        let record = guard.record_mut();
        record.requested_model = target.model.id.clone();
        record.provider = target.provider.id.clone();
        record.upstream_model = target.upstream.clone();
        record.fallbacks = fallbacks_used;
        record.quirks.extend(attempt_quirks);
        if fallbacks_used > 0 {
            record
                .quirks
                .push(format!("fallback_from:{}", candidates[0].provider.id));
        }
    }

    let stream = match result {
        AttemptResult::Stream(stream) => stream,
        AttemptResult::Error(err) => {
            let (status, code, message) = match &err {
                ProxyError::MissingKey { .. } => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "upstream_key_missing",
                    "the gateway is missing credentials for an upstream service".to_string(),
                ),
                ProxyError::Auth(_) => (
                    StatusCode::BAD_GATEWAY,
                    "upstream_auth_failed",
                    "the gateway could not authenticate with the upstream service".to_string(),
                ),
                ProxyError::Timeout(_) => (
                    StatusCode::GATEWAY_TIMEOUT,
                    "upstream_timeout",
                    "the upstream service did not respond in time".to_string(),
                ),
                ProxyError::Network(_) => (
                    StatusCode::BAD_GATEWAY,
                    "upstream_unreachable",
                    "the upstream service could not be reached".to_string(),
                ),
            };
            {
                let record = guard.record_mut();
                record.status = status.as_u16();
                record.error_code = Some(code.to_string());
                record.duration_ms = started.elapsed().as_millis() as u64;
            }
            guard.commit();
            plog(
                "warn",
                "upstream_error",
                &request_id,
                Some(&client.name),
                Some(&target.provider.id),
                Some(code),
            );
            return gateway_error(status, code, &message, &request_id);
        }
        AttemptResult::CircuitOpen { reason } => {
            {
                let record = guard.record_mut();
                record.status = StatusCode::SERVICE_UNAVAILABLE.as_u16();
                record.error_code = Some("provider_circuit_open".to_string());
                record.duration_ms = started.elapsed().as_millis() as u64;
            }
            guard.commit();
            plog(
                "warn",
                "circuit_open_reject",
                &request_id,
                Some(&client.name),
                Some(&target.provider.id),
                reason.as_deref().or(Some("provider_circuit_open")),
            );
            return circuit_open_response(protocol_name, &request_id);
        }
        AttemptResult::Untranslatable { message } => {
            {
                let record = guard.record_mut();
                record.status = StatusCode::BAD_REQUEST.as_u16();
                record.error_code = Some("translation_failed".to_string());
                record.duration_ms = started.elapsed().as_millis() as u64;
            }
            guard.commit();
            return protocol_gateway_error(
                protocol_name,
                StatusCode::BAD_REQUEST,
                "translation_failed",
                &message,
                &request_id,
            );
        }
        AttemptResult::Response(_) => {
            unreachable!("streaming path cannot produce a buffered response")
        }
    };

    let status = stream.status;

    // Upstream failed before the stream started: relay status, body and key
    // headers verbatim, never as SSE (spec 2.0 / 8.3).
    if !(200..300).contains(&status) {
        let upstream_request_id = stream.upstream_request_id;
        let body = match stream.response.bytes().await {
            Ok(bytes) => bytes,
            Err(err) => {
                {
                    let record = guard.record_mut();
                    record.status = StatusCode::BAD_GATEWAY.as_u16();
                    record.upstream_status = Some(status);
                    record.error_code = Some("upstream_unreachable".to_string());
                    record.duration_ms = started.elapsed().as_millis() as u64;
                }
                guard.commit();
                return gateway_error(
                    StatusCode::BAD_GATEWAY,
                    "upstream_unreachable",
                    &format!("cannot read upstream response body: {err}"),
                    &request_id,
                );
            }
        };
        {
            let record = guard.record_mut();
            record.status = mapped_upstream_status(status);
            record.upstream_status = Some(status);
            record.bytes_out = body.len() as u64;
            record.upstream_request_id = upstream_request_id;
            record.error_code = Some(format!("upstream_{status}"));
            record.duration_ms = started.elapsed().as_millis() as u64;
            if !body.is_empty() {
                record.upstream_body = Some(truncate_body(&body));
            }
        }
        let duration_ms = guard.record_mut().duration_ms;
        state.note_health(
            &target.provider.id,
            status,
            duration_ms,
            Some(format!("http_{status}")),
        );
        guard.commit();

        return upstream_failure_response(
            protocol_name,
            status,
            &body,
            &target.model.id,
            &target,
            &request_id,
            target.upstream_protocol != protocol_name,
        );
    }

    {
        let record = guard.record_mut();
        record.status = status;
        record.upstream_status = Some(status);
        record.upstream_request_id = stream.upstream_request_id;
    }
    state.note_health(
        &target.provider.id,
        status,
        started.elapsed().as_millis() as u64,
        None,
    );

    let (tx, rx) = tokio::sync::mpsc::channel::<std::result::Result<Bytes, std::io::Error>>(32);
    let body_stream = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    });

    let bridge = if target.upstream_protocol == protocol_name {
        None
    } else {
        Some(
            crate::translate::StreamBridge::new(
                &target.upstream_protocol,
                protocol_name,
                &target.model.id,
                matches!(
                    protocol,
                    StreamProtocol::Responses {
                        emit_event_lines: true
                    }
                ),
            )
            .expect("client and upstream protocols are chat, responses, or messages"),
        )
    };
    let inflight = inflight.expect("a live stream holds the in-flight gauge");
    // From here on the pump owns the record and writes it when the stream ends.
    tokio::spawn(pump_stream(
        state.clone(),
        target.provider.id.clone(),
        stream.response,
        tx,
        guard.take(),
        started,
        Duration::from_secs(cfg.server.stream_idle_timeout_s.max(1)),
        Duration::from_secs(cfg.server.stream_keepalive_s.max(1)),
        protocol,
        bridge,
        inflight,
    ));

    // Framing is ours and no upstream header reaches the client: vendor trace
    // ids and cookies would tell a caller which provider served the request.
    let builder =
        Response::builder().status(StatusCode::from_u16(status).unwrap_or(StatusCode::OK));
    builder
        .header(axum::http::header::CONTENT_TYPE, "text/event-stream")
        .header(axum::http::header::CACHE_CONTROL, "no-cache")
        .header("x-accel-buffering", "no")
        .header("x-umsp-request-id", &request_id)
        .body(Body::from_stream(body_stream))
        .unwrap_or_else(|_| {
            gateway_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "response_build_failed",
                "failed to build response",
                &request_id,
            )
        })
}

/// Forward upstream SSE bytes to the client while observing a copy for the
/// ledger.
///
/// Chat bytes are forwarded verbatim; Responses bytes gain `event:` lines where
/// OpenDesign omitted them (spec 2.2, toggled by `responses_emit_event_lines`).
///
/// - keep-alive comment every `keepalive` of upstream silence;
/// - idle gaps longer than `idle` end the stream with a protocol-native error;
/// - a client disconnect drops the receiver, which this task notices through
///   `tx.closed()`/send failure, then dropping the upstream response cancels
///   the upstream request;
/// - `[DONE]` is passed through untouched and never synthesized twice.
enum PumpObserver {
    Native(stream::Observer),
    Bridge(Box<crate::translate::StreamBridge>),
}

impl PumpObserver {
    fn transform(&mut self, chunk: &[u8], out: &mut Vec<u8>) -> bool {
        match self {
            Self::Native(observer) => observer.transform(chunk, out),
            Self::Bridge(bridge) => bridge.push(chunk, out),
        }
    }

    fn finish(&mut self, out: &mut Vec<u8>) {
        match self {
            Self::Native(observer) => observer.finish(out),
            Self::Bridge(bridge) => bridge.finish(out),
        }
    }

    fn done(&self) -> bool {
        match self {
            Self::Native(observer) => observer.done(),
            Self::Bridge(bridge) => bridge.done(),
        }
    }

    fn usage(&self) -> Option<Usage> {
        match self {
            Self::Native(observer) => observer.usage(),
            Self::Bridge(bridge) => bridge.usage(),
        }
    }

    fn keepalive(&self) -> &'static [u8] {
        match self {
            Self::Native(observer) => observer.keepalive(),
            Self::Bridge(bridge) => bridge.keepalive(),
        }
    }

    fn error_event(&mut self, message: &str) -> Vec<u8> {
        match self {
            Self::Native(observer) => observer.error_event(message),
            Self::Bridge(bridge) => bridge.error_event(message),
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn pump_stream(
    state: AppState,
    provider_id: String,
    resp: reqwest::Response,
    tx: tokio::sync::mpsc::Sender<std::result::Result<Bytes, std::io::Error>>,
    mut record: RequestRecord,
    started: Instant,
    idle: Duration,
    keepalive: Duration,
    protocol: StreamProtocol,
    bridge: Option<crate::translate::StreamBridge>,
    // Held for the whole stream so `umsp_inflight_requests` counts it.
    _inflight: crate::metrics::Inflight,
) {
    use futures_util::StreamExt;

    let mut upstream = resp.bytes_stream();
    let mut observer = match bridge {
        Some(bridge) => PumpObserver::Bridge(Box::new(bridge)),
        None => PumpObserver::Native(protocol.observer()),
    };
    let keepalive_frame = observer.keepalive();
    let mut bytes_out: u64 = 0;
    let mut upstream_error: Option<&'static str> = None;
    let mut disconnected = false;
    let mut ka_deadline = tokio::time::Instant::now() + keepalive;

    loop {
        if observer.done() {
            break;
        }
        tokio::select! {
            biased;
            _ = tx.closed() => {
                disconnected = true;
                break;
            }
            _ = tokio::time::sleep_until(ka_deadline) => {
                if tx.send(Ok(Bytes::from_static(keepalive_frame))).await.is_err() {
                    disconnected = true;
                    break;
                }
                bytes_out += keepalive_frame.len() as u64;
                ka_deadline = tokio::time::Instant::now() + keepalive;
            }
            item = tokio::time::timeout(idle, upstream.next()) => {
                match item {
                    Err(_) => {
                        upstream_error = Some("upstream_stream_timeout");
                        break;
                    }
                    Ok(None) => break,
                    Ok(Some(Err(_))) => {
                        upstream_error = Some("upstream_stream_error");
                        break;
                    }
                    Ok(Some(Ok(chunk))) => {
                        let mut out = Vec::with_capacity(chunk.len() + 32);
                        if observer.transform(&chunk, &mut out) && record.ttft_ms.is_none() {
                            record.ttft_ms = Some(started.elapsed().as_millis() as u64);
                        }
                        let finished = observer.done();
                        let len = out.len() as u64;
                        if !out.is_empty() && tx.send(Ok(Bytes::from(out))).await.is_err() {
                            disconnected = true;
                            break;
                        }
                        bytes_out += len;
                        ka_deadline = tokio::time::Instant::now() + keepalive;
                        if finished {
                            break;
                        }
                    }
                }
            }
        }
    }

    // A partial line buffered for event-line patching is still real upstream
    // data: flush it verbatim before any synthesized terminator.
    if !disconnected {
        let mut tail = Vec::new();
        observer.finish(&mut tail);
        if !tail.is_empty() {
            let len = tail.len() as u64;
            match tokio::time::timeout(Duration::from_secs(5), tx.send(Ok(Bytes::from(tail)))).await
            {
                Ok(Ok(())) => bytes_out += len,
                _ => disconnected = true,
            }
        }
    }

    if !disconnected {
        // Chat/Responses streams terminate with `data: [DONE]`. Anthropic
        // Messages never gets a synthesized `message_stop` (spec 2.3): an
        // upstream that broke — or closed before `message_stop` — ends with a
        // protocol-native `error` event instead.
        let is_messages = matches!(protocol, StreamProtocol::Messages);
        let synthesized_error = match upstream_error {
            Some(code) => Some(code),
            None if is_messages && !observer.done() => Some("upstream_stream_error"),
            None => None,
        };
        match synthesized_error {
            Some(code) => {
                state.metrics().note_upstream_error(&record.provider, code);
                let message = if code == "upstream_stream_timeout" {
                    "upstream stream idle timeout"
                } else {
                    "upstream stream terminated before completion"
                };
                record.error_code = Some(code.to_string());
                let mut parts = vec![Bytes::from(observer.error_event(message))];
                if !is_messages {
                    parts.push(Bytes::from_static(stream::DONE_EVENT));
                }
                for part in parts {
                    let len = part.len() as u64;
                    match tokio::time::timeout(Duration::from_secs(5), tx.send(Ok(part))).await {
                        Ok(Ok(())) => bytes_out += len,
                        _ => {
                            disconnected = true;
                            break;
                        }
                    }
                }
            }
            None if !observer.done() => {
                let done = Bytes::from_static(stream::DONE_EVENT);
                let len = done.len() as u64;
                if let Ok(Ok(())) =
                    tokio::time::timeout(Duration::from_secs(5), tx.send(Ok(done))).await
                {
                    bytes_out += len;
                }
            }
            None => {}
        }
    }

    record.client_disconnected = disconnected;
    record.bytes_out = bytes_out;
    record.duration_ms = started.elapsed().as_millis() as u64;
    match observer.usage() {
        Some(usage) => {
            record.prompt_tokens = usage.input_tokens;
            record.completion_tokens = usage.output_tokens;
            record.cached_tokens = usage.cached_tokens;
            record.reasoning_tokens = usage.reasoning_tokens;
        }
        None if record.error_code.is_none() && !disconnected => {
            record.quirks.push("usage_missing".to_string());
        }
        None => {}
    }
    state.finish_request(&record);

    if let Some(code) = record.error_code.clone() {
        plog(
            "warn",
            "upstream_stream_error",
            &record.request_id,
            Some(&record.client),
            Some(&provider_id),
            Some(&code),
        );
    }
    if disconnected {
        plog(
            "info",
            "client_disconnected",
            &record.request_id,
            Some(&record.client),
            Some(&provider_id),
            None,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn log_gateway_error(
    state: &AppState,
    _cfg: &Config,
    request_id: &str,
    client: &str,
    protocol: &str,
    requested_model: &str,
    provider: &str,
    status: StatusCode,
    code: &str,
    bytes_in: u64,
    duration_ms: u64,
    stream: bool,
) {
    // Pre-routing failures never reach an upstream, so there is no upstream
    // model to record.
    let upstream_model = String::new();
    let record = RequestRecord {
        ts: util::now_rfc3339(),
        request_id: request_id.to_string(),
        client: client.to_string(),
        protocol: protocol.to_string(),
        requested_model: requested_model.to_string(),
        provider: provider.to_string(),
        upstream_model,
        status: status.as_u16(),
        upstream_status: None,
        upstream_body: None,
        prompt_tokens: 0,
        completion_tokens: 0,
        cached_tokens: 0,
        reasoning_tokens: 0,
        ttft_ms: None,
        duration_ms,
        bytes_in,
        bytes_out: 0,
        stream,
        error_code: Some(code.to_string()),
        upstream_request_id: None,
        client_disconnected: false,
        quirks: Vec::new(),
        fallbacks: 0,
    };
    state.finish_request(&record);
}

// ---------------------------------------------------------------------------
// Operational log (stderr, JSON lines)
// ---------------------------------------------------------------------------

pub fn plog(
    level: &str,
    event: &str,
    request_id: &str,
    client: Option<&str>,
    provider: Option<&str>,
    code: Option<&str>,
) {
    let line = json!({
        "ts": util::now_rfc3339(),
        "level": level,
        "event": event,
        "request_id": request_id,
        "client": client,
        "provider": provider,
        "code": code,
    });
    eprintln!("{line}");
}

// ---------------------------------------------------------------------------
// Server lifecycle
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Background health (spec 3.4 / 6.1)
// ---------------------------------------------------------------------------

/// How often an open DeepSeek circuit re-checks `/user/balance`.
const BALANCE_INTERVAL: Duration = Duration::from_secs(300);
/// How often the health task looks at the circuits at all.
const HEALTH_TICK: Duration = Duration::from_secs(60);

async fn deepseek_balance(
    state: &AppState,
    provider: &Provider,
) -> std::result::Result<Balance, String> {
    let key = state
        .env()
        .get(&provider.api_key_env)
        .ok_or_else(|| format!("missing {}", provider.api_key_env))?;
    health::fetch_balance(state.http(), provider, &key).await
}

/// Recovery loop for open circuits. DeepSeek is re-checked through the free
/// balance endpoint every five minutes; everything else gets one minimal
/// inference probe (`max_tokens: 1`, `reasoning_effort: none`).
///
/// Spawned by `serve` and aborted when serving stops, so it can never keep the
/// process alive.
async fn health_loop(state: AppState) {
    let mut ticker = tokio::time::interval(HEALTH_TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_balance: HashMap<String, Instant> = HashMap::new();
    loop {
        ticker.tick().await;
        let cfg = state.cfg();
        let circuits = state.circuits();
        for provider in &cfg.providers {
            let view = circuits.view(&provider.id);
            if view.state == CircuitState::Closed {
                continue;
            }
            let Some((model, upstream)) = cfg.models.iter().find_map(|m| {
                m.backends
                    .iter()
                    .find(|b| b.provider == provider.id)
                    .map(|b| (m.clone(), b.upstream.clone()))
            }) else {
                continue;
            };

            if provider.quirks.as_deref() == Some("deepseek") {
                let due = last_balance
                    .get(&provider.id)
                    .map(|at| at.elapsed() >= BALANCE_INTERVAL)
                    .unwrap_or(true);
                if due {
                    last_balance.insert(provider.id.clone(), Instant::now());
                    match deepseek_balance(&state, provider).await {
                        Ok(balance) if balance.is_available => {
                            let balance_reason = matches!(
                                view.reason.as_deref(),
                                Some("insufficient_balance") | Some("balance_check_failed") | None
                            );
                            if balance_reason && circuits.close(&provider.id) {
                                plog_health(
                                    "circuit_close",
                                    &provider.id,
                                    &upstream,
                                    "balance_restored",
                                );
                            }
                            if balance_reason {
                                continue;
                            }
                        }
                        Ok(_) => {
                            // Still no credit: keep the long open window.
                            circuits.force_open(
                                &provider.id,
                                "insufficient_balance",
                                circuits.config().ds_balance_cooldown,
                            );
                            continue;
                        }
                        Err(err) => {
                            plog_health(
                                "circuit_open",
                                &provider.id,
                                &upstream,
                                &format!("balance_check_failed:{err}"),
                            );
                            if view.state == CircuitState::HalfOpen {
                                circuits.force_open(
                                    &provider.id,
                                    "balance_check_failed",
                                    BALANCE_INTERVAL,
                                );
                            }
                            continue;
                        }
                    }
                }
            }

            let Some(_probe_guard) = circuits.take_probe(&provider.id) else {
                continue;
            };
            let target = AttemptTarget {
                model: model.clone(),
                provider: provider.clone(),
                upstream: upstream.clone(),
                upstream_protocol: "chat".into(),
            };
            let up = Upstream::with_clients(
                cfg.clone(),
                state.env().clone(),
                state.http().clone(),
                state.stream_http().clone(),
                state.sessions(),
            );
            let outcome = if target.provider.supports("chat") {
                let body = json!({
                    "model": target.upstream,
                    "messages": [{"role": "user", "content": "ping"}],
                    "max_tokens": 1,
                    "reasoning_effort": "none",
                    "stream": false
                });
                up.chat(&target.provider, &target.upstream, &body, &HeaderMap::new())
                    .await
            } else if target.provider.supports("responses") {
                let body = json!({
                    "model": target.upstream,
                    "input": "ping",
                    "max_output_tokens": 16,
                    "stream": false
                });
                up.responses(&target.provider, &body, &HeaderMap::new())
                    .await
            } else {
                continue;
            };
            match outcome {
                Ok(outcome) => {
                    state.note_health(
                        &target.provider.id,
                        outcome.status,
                        outcome.duration_ms,
                        (outcome.status >= 400).then(|| format!("http_{}", outcome.status)),
                    );
                    if outcome.status < 400 || !upstream::status_is_retryable(outcome.status) {
                        // A client error still proves the provider is reachable
                        // and the key works: close, don't count it.
                        if circuits.close(&target.provider.id) {
                            plog_health(
                                "circuit_close",
                                &target.provider.id,
                                &target.upstream,
                                "probe_ok",
                            );
                        }
                    } else {
                        note_circuit_result(&state, &target, outcome.status, true);
                    }
                }
                Err(err) => {
                    state.note_health(&target.provider.id, 0, 0, Some(err.code().to_string()));
                    note_circuit_error(&state, &target, &err);
                }
            }
        }
    }
}

pub async fn serve(config_path: &std::path::Path, listen_override: Option<String>) -> Result<()> {
    let cfg = Config::load_checked(config_path)?;
    let listen = listen_override
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            std::env::var("UMSP_LISTEN")
                .ok()
                .filter(|v| !v.trim().is_empty())
        })
        .unwrap_or_else(|| cfg.server.listen.clone());
    let env_path = Config::resolve_env_path();
    let env = EnvFile::load(&env_path);
    let state = AppState::with_sessions(cfg, env, crate::config::sessions_dir(config_path))?;
    let app = build_router(state.clone());

    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .map_err(|e| UmspError::other("BIND", format!("cannot bind {listen}: {e}")))?;
    let local_addr = listener
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| listen.clone());

    let pid_file = state.cfg().server.pid_file();
    if let Some(parent) = pid_file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&pid_file, format!("{}\n", std::process::id()));

    println!(
        "{}",
        json!({"event": "ready", "addr": local_addr, "version": VERSION})
    );
    use std::io::Write;
    let _ = std::io::stdout().flush();

    // SIGHUP: reload config if valid, keep the old one otherwise.
    let reload_state = state.clone();
    let config_path_owned = config_path.to_path_buf();
    tokio::spawn(async move {
        let mut hup = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()) {
            Ok(sig) => sig,
            Err(_) => return,
        };
        while hup.recv().await.is_some() {
            match Config::load_checked(&config_path_owned) {
                Ok(cfg) => {
                    let issues = cfg.validate();
                    let errors = issues.iter().filter(|i| i.level == "error").count();
                    if errors == 0 {
                        reload_state.replace_config(cfg);
                        plog("info", "config_reloaded", "-", None, None, None);
                    } else {
                        plog(
                            "error",
                            "config_reload_rejected",
                            "-",
                            None,
                            None,
                            Some("CONFIG_INVALID"),
                        );
                    }
                }
                Err(err) => {
                    plog(
                        "error",
                        "config_reload_failed",
                        "-",
                        None,
                        None,
                        Some(&err.code),
                    );
                }
            }
        }
    });

    // Background circuit recovery (spec 3.4): spawned after the listener is up
    // and aborted as soon as serving stops, so it never delays shutdown.
    let health_task = tokio::spawn(health_loop(state.clone()));

    let shutdown = shutdown_signal();
    let result = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .await;
    health_task.abort();
    let _ = std::fs::remove_file(&pid_file);
    result.map_err(|e| UmspError::other("SERVE", format!("server error: {e}")))
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    let term = async {
        if let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sig.recv().await;
        }
    };
    tokio::select! {
        _ = ctrl_c => {},
        _ = term => {},
    }
}

/// Best-effort SIGHUP to a running `umsp serve` so CLI edits take effect.
pub fn reload_running_server(cfg: &Config) {
    let pid_file = cfg.server.pid_file();
    let Ok(raw) = std::fs::read_to_string(&pid_file) else {
        return;
    };
    let Ok(pid) = raw.trim().parse::<i32>() else {
        return;
    };
    let alive = std::path::Path::new(&format!("/proc/{pid}")).exists();
    if !alive {
        return;
    }
    let Ok(kill) = std::process::Command::new("kill")
        .args(["-HUP", &pid.to_string()])
        .status()
    else {
        return;
    };
    if kill.success() {
        plog("info", "reload_signalled", "-", None, None, None);
    }
}

/// Helper used by tests and CLI JSON output.
pub fn success_json(value: Value) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("ok".to_string(), Value::Bool(true));
    if let Value::Object(map) = value {
        for (k, v) in map {
            obj.insert(k, v);
        }
    } else {
        obj.insert("data".to_string(), value);
    }
    Value::Object(obj)
}

impl IntoResponse for ProxyError {
    fn into_response(self) -> Response {
        let status = match &self {
            ProxyError::MissingKey { .. } => StatusCode::INTERNAL_SERVER_ERROR,
            ProxyError::Auth(_) => StatusCode::BAD_GATEWAY,
            ProxyError::Timeout(_) => StatusCode::GATEWAY_TIMEOUT,
            ProxyError::Network(_) => StatusCode::BAD_GATEWAY,
        };
        let message = match &self {
            ProxyError::MissingKey { provider, env } => {
                format!("upstream key for provider '{provider}' is missing; set {env}")
            }
            ProxyError::Auth(msg) => msg.clone(),
            ProxyError::Timeout(msg) | ProxyError::Network(msg) => msg.clone(),
        };
        gateway_error(status, self.code(), &message, "-")
    }
}

/// Convenience for CLI commands: resolve a model and its preferred backend.
pub fn resolve_target(cfg: &Config, requested: &str) -> Result<(Model, Provider, String)> {
    let model = registry::resolve(cfg, requested)
        .cloned()
        .ok_or_else(|| UmspError::not_found(format!("unknown model '{requested}'")))?;
    let backend = model
        .backends
        .first()
        .cloned()
        .ok_or_else(|| UmspError::not_found(format!("model '{}' has no backends", model.id)))?;
    let provider = cfg
        .provider(&backend.provider)
        .cloned()
        .ok_or_else(|| UmspError::not_found(format!("unknown provider '{}'", backend.provider)))?;
    Ok((model, provider, backend.upstream.clone()))
}

/// Timestamp helper for the CLI.
pub fn now_iso() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_representation_headers_survive() {
        let headers = vec![
            (
                HeaderName::from_static("content-type"),
                HeaderValue::from_static("application/json"),
            ),
            (
                HeaderName::from_static("retry-after"),
                HeaderValue::from_static("3"),
            ),
            (
                HeaderName::from_static("server"),
                HeaderValue::from_static("open-design"),
            ),
            (
                HeaderName::from_static("x-request-id"),
                HeaderValue::from_static("vendor-trace"),
            ),
        ];
        let kept = client_response_headers(&headers);
        assert_eq!(kept.len(), 2);
        assert!(
            kept.iter()
                .all(|(name, _)| name == "content-type" || name == "retry-after")
        );
    }

    #[test]
    fn upstream_failures_map_to_gateway_statuses() {
        assert_eq!(mapped_upstream_status(400), 400);
        assert_eq!(mapped_upstream_status(404), 404);
        assert_eq!(mapped_upstream_status(401), 502);
        assert_eq!(mapped_upstream_status(402), 502);
        assert_eq!(mapped_upstream_status(403), 502);
        assert_eq!(mapped_upstream_status(429), 429);
        assert_eq!(mapped_upstream_status(503), 502);
        assert_eq!(mapped_upstream_status(200), 200);
    }

    #[test]
    fn replace_token_respects_word_boundaries() {
        assert_eq!(
            replace_token("a deepseek b", "deepseek", "umsp"),
            "a umsp b"
        );
        assert_eq!(
            replace_token("deepseek-v4.1-flash", "deepseek", "umsp"),
            "deepseek-v4.1-flash",
            "a provider id inside an upstream model id must stay untouched"
        );
        assert_eq!(replace_token("xdeepseek", "deepseek", "umsp"), "xdeepseek");
        assert_eq!(replace_token("deepseek.", "deepseek", "umsp"), "umsp.");
    }

    #[test]
    fn gateway_error_has_native_shape_and_code_header() {
        let resp = gateway_error(StatusCode::NOT_FOUND, "model_not_found", "nope", "rid1");
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            resp.headers().get("x-umsp-code").unwrap(),
            "model_not_found"
        );
        assert_eq!(resp.headers().get("x-umsp-request-id").unwrap(), "rid1");
    }

    #[test]
    fn error_type_mapping() {
        assert_eq!(
            error_type_for(StatusCode::UNAUTHORIZED),
            "authentication_error"
        );
        assert_eq!(error_type_for(StatusCode::BAD_GATEWAY), "server_error");
        assert_eq!(
            error_type_for(StatusCode::BAD_REQUEST),
            "invalid_request_error"
        );
        // 501 tells the caller to change the request (stream:false), so it is not
        // a server fault.
        assert_eq!(
            error_type_for(StatusCode::NOT_IMPLEMENTED),
            "invalid_request_error"
        );
    }
}
