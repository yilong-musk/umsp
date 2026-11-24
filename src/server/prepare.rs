use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use serde_json::Value;

use crate::config::{Client, Config, Model};
use crate::keys;
use crate::registry;
use crate::usage::RequestRecord;
use crate::util;

use super::error::{gateway_error, messages_error, protocol_gateway_error};
use super::log::plog_binding;
use super::state::AppState;

#[derive(Clone)]
pub(super) struct AuthedClient(pub(super) Client);

/// Effective source IP resolved at auth time; carried into the ledger row.
#[derive(Clone)]
pub(super) struct ClientIp(pub(super) String);

pub(super) async fn auth_middleware(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Response {
    let presented = keys::extract_presented_key(req.headers());
    let cfg = state.cfg();
    let matched = presented.and_then(|key| keys::authenticate(&cfg.clients, &key).cloned());
    match matched {
        Some(client) => {
            // TOFU device binding: the first authenticated request binds the
            // key to its effective source IP; later requests must match.
            let peer = req
                .extensions()
                .get::<ConnectInfo<SocketAddr>>()
                .map(|c| c.0.ip());
            let xff = req
                .headers()
                .get("x-forwarded-for")
                .and_then(|v| v.to_str().ok());
            let ip = crate::bindings::effective_client_ip(peer, xff).to_string();
            match state.bindings().check_or_bind(&client.name, &ip) {
                crate::bindings::BindOutcome::Allowed { first_use } => {
                    if first_use {
                        plog_binding("client_bound", &client.name, &ip, Some(&ip), "-");
                    }
                    req.extensions_mut().insert(AuthedClient(client));
                    req.extensions_mut().insert(ClientIp(ip));
                    next.run(req).await
                }
                crate::bindings::BindOutcome::Mismatch { bound_ip } => {
                    let request_id = util::new_request_id();
                    plog_binding(
                        "device_mismatch",
                        &client.name,
                        &ip,
                        Some(&bound_ip),
                        &request_id,
                    );
                    // The caller may hold a leaked key: never echo the bound
                    // IP (or any device detail) back in the error body.
                    let message = "this key is bound to another device; the gateway admin can clear the binding with `umsp client unbind`";
                    if req.uri().path().starts_with("/v1/messages") {
                        messages_error(
                            StatusCode::FORBIDDEN,
                            "permission_error",
                            "device_mismatch",
                            message,
                            &request_id,
                        )
                    } else {
                        gateway_error(
                            StatusCode::FORBIDDEN,
                            "device_mismatch",
                            message,
                            &request_id,
                        )
                    }
                }
                crate::bindings::BindOutcome::PersistFailed => {
                    let request_id = util::new_request_id();
                    plog_binding(
                        "binding_persist_failed",
                        &client.name,
                        &ip,
                        None,
                        &request_id,
                    );
                    gateway_error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "binding_persist_failed",
                        "device binding could not be persisted; request rejected",
                        &request_id,
                    )
                }
            }
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

/// Everything a protocol handler needs after auth, body parsing and routing.
pub(super) struct Prepared {
    pub(super) cfg: Arc<Config>,
    pub(super) client: Client,
    /// Effective source IP resolved by the auth layer (device binding).
    pub(super) client_ip: String,
    pub(super) request_id: String,
    pub(super) started: Instant,
    pub(super) bytes_in: u64,
    pub(super) payload: Value,
    pub(super) requested_model: String,
    pub(super) model: Model,
    pub(super) client_headers: HeaderMap,
    /// Raw client query string (`?beta=true` on Messages); relayed upstream.
    pub(super) query: Option<String>,
}

/// Auth, body limit, JSON parse, model resolution, allowlist and protocol check.
///
/// Shared by `/v1/chat/completions` and `/v1/responses` so the two entry points
/// cannot drift apart. On failure the gateway error response is returned and a
/// ledger row has already been written.
// The error path is a fully built `Response` (headers + body); boxing it would
// only add an allocation on the rare failure path.
#[allow(clippy::result_large_err)]
pub(super) async fn prepare_request(
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
    // Resolved by the auth layer; the fallback only fires for in-process
    // callers that bypass the middleware (tests).
    let req_ip = req
        .extensions()
        .get::<ClientIp>()
        .map(|c| c.0.clone())
        .unwrap_or_else(|| "127.0.0.1".to_string());

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

    let models = state.models();
    let Some(model) = registry::resolve_models(&models, &requested_model).cloned() else {
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
        client_ip: req_ip,
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

#[allow(clippy::too_many_arguments)]
pub(super) fn log_gateway_error(
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
        client_ip: None,
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
