use std::sync::Arc;
use std::time::Instant;

use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use serde_json::Value;

use crate::config::{Config, Provider};
use crate::envfile::EnvFile;
use crate::error::Result;
use crate::quirks;
use crate::sessions::{self, SessionError, SessionStore};
use crate::upstream::{self, forward_request_headers, relay_response_headers};
use crate::usage::Usage;

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

    pub fn status(&self) -> StatusCode {
        match self {
            ProxyError::MissingKey { .. } => StatusCode::INTERNAL_SERVER_ERROR,
            ProxyError::Auth(_) | ProxyError::Network(_) => StatusCode::BAD_GATEWAY,
            ProxyError::Timeout(_) => StatusCode::GATEWAY_TIMEOUT,
        }
    }

    /// Message safe to show an API client: no provider ids, env names or
    /// upstream detail.
    pub fn client_message(&self) -> &'static str {
        match self {
            ProxyError::MissingKey { .. } => {
                "the gateway is missing credentials for an upstream service"
            }
            ProxyError::Auth(_) => "the gateway could not authenticate with the upstream service",
            ProxyError::Timeout(_) => "the upstream service did not respond in time",
            ProxyError::Network(_) => "the upstream service could not be reached",
        }
    }
}

/// Minimal upstream client shared by `serve` and `umsp call`.
pub struct Upstream {
    pub cfg: Arc<Config>,
    pub env: Arc<EnvFile>,
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
            env: Arc::new(env),
            http,
            http_stream,
            sessions,
        })
    }

    /// Reuse the long-lived clients owned by `AppState` instead of building
    /// (and warming) new connection pools per request.
    pub fn with_clients(
        cfg: Arc<Config>,
        env: Arc<EnvFile>,
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

    fn grok_cli_version(&self) -> String {
        self.env
            .get("UMSP_GROK_CLI_VERSION")
            .unwrap_or_else(|| sessions::GROK_CLI_VERSION.to_string())
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
        if provider.vendor.as_deref() == Some("grok") {
            req = req
                .header("X-XAI-Token-Auth", sessions::CLI_TOKEN_AUTH)
                .header("x-grok-client-version", self.grok_cli_version());
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
        if provider.vendor.as_deref() == Some("opencode-go") {
            let session = upstream::opencode_session(client_headers, body);
            let session = reqwest::header::HeaderValue::from_str(&session).map_err(|_| {
                ProxyError::Network(format!(
                    "opencode session for provider '{}' is not a valid header value",
                    provider.id
                ))
            })?;
            req = req
                .header(reqwest::header::USER_AGENT, upstream::OPENCODE_USER_AGENT)
                .header("x-opencode-session", session);
        }
        for (name, value) in forward_request_headers(client_headers) {
            if provider.vendor.as_deref() == Some("grok")
                && sessions::is_cli_session_header(name.as_str())
            {
                continue;
            }
            if provider.vendor.as_deref() == Some("opencode-go")
                && (name.as_str().eq_ignore_ascii_case("user-agent")
                    || name.as_str().eq_ignore_ascii_case("x-opencode-session"))
            {
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
        let bytes = if provider.vendor.as_deref() == Some("chatgpt") {
            if let Some(completed) = crate::chatgpt_oauth::collapse_responses_stream(&bytes) {
                serde_json::to_vec(&completed).unwrap_or_else(|_| bytes.to_vec())
            } else {
                bytes.to_vec()
            }
        } else {
            bytes.to_vec()
        };

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
