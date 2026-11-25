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

use super::admission::BudgetSlot;
use super::error::{GatewayError, Protocol, gateway_error, messages_error};
use super::log::{plog, plog_binding};
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
            // Expiry is checked before the device binding, so an expired key
            // can never claim (or probe) a binding.
            if client.is_expired(chrono::Utc::now()) {
                let request_id = util::new_request_id();
                plog(
                    "warn",
                    "client_key_expired",
                    &request_id,
                    Some(&client.name),
                    None,
                    Some("key_expired"),
                );
                return auth_error(
                    req.uri().path(),
                    StatusCode::UNAUTHORIZED,
                    "authentication_error",
                    "key_expired",
                    "this key has expired; ask the gateway admin to extend or replace it",
                    &request_id,
                );
            }
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

/// The output-token cap a request asks for, which is what a token budget
/// reserves. Chat honours either field depending on the upstream, so the
/// larger one counts. No cap: nothing is reserved.
fn requested_output_cap(protocol: Protocol, payload: &Value) -> u64 {
    let field = |key: &str| payload.get(key).and_then(Value::as_u64);
    match protocol {
        Protocol::Chat => field("max_completion_tokens").max(field("max_tokens")),
        Protocol::Responses => field("max_output_tokens"),
        Protocol::Messages => field("max_tokens"),
    }
    .unwrap_or(0)
}

/// A client-facing rejection from the auth layer, in the caller's protocol
/// shape: Anthropic's envelope on `/v1/messages*`, the gateway's elsewhere.
pub(super) fn auth_error(
    path: &str,
    status: StatusCode,
    messages_type: &str,
    code: &str,
    message: &str,
    request_id: &str,
) -> Response {
    if path.starts_with("/v1/messages") {
        messages_error(status, messages_type, code, message, request_id)
    } else {
        gateway_error(status, code, message, request_id)
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
    pub(super) model: Model,
    pub(super) client_headers: HeaderMap,
    /// Raw client query string (`?beta=true` on Messages); relayed upstream.
    pub(super) query: Option<String>,
}

/// Auth, body limit, JSON parse, model resolution, allowlist and protocol check.
///
/// Shared by `/v1/chat/completions`, `/v1/responses`, and `/v1/messages` so the
/// entry points cannot drift apart. On failure the error carries the rendered
/// status/code/message and a ledger row has already been written.
pub(super) async fn prepare_request(
    state: &AppState,
    req: Request,
    protocol: Protocol,
) -> std::result::Result<Prepared, GatewayError> {
    let protocol_name = protocol.as_str();
    let request_id = util::new_request_id();
    let started = Instant::now();
    let client = match req.extensions().get::<AuthedClient>() {
        Some(c) => c.0.clone(),
        None => {
            return Err(GatewayError {
                protocol,
                status: StatusCode::UNAUTHORIZED,
                code: "unauthorized",
                message: "missing or invalid API key".to_string(),
                request_id,
            });
        }
    };
    // Resolved by the auth layer; the fallback only fires for in-process
    // callers that bypass the middleware (tests).
    let req_ip = req
        .extensions()
        .get::<ClientIp>()
        .map(|c| c.0.clone())
        .unwrap_or_else(|| "127.0.0.1".to_string());

    let budget_slot = req.extensions().get::<BudgetSlot>().cloned();

    let cfg = state.cfg();
    let (parts, body) = req.into_parts();
    let query = parts.uri.query().map(|q| q.to_string());
    let max_body = cfg.server.max_body_bytes;

    let reject = |status: StatusCode,
                  code: &'static str,
                  requested_model: &str,
                  bytes_in: u64,
                  message: String|
     -> GatewayError {
        log_gateway_error(
            state,
            &cfg,
            &request_id,
            &client.name,
            protocol_name,
            requested_model,
            "",
            status,
            code,
            bytes_in,
            started.elapsed().as_millis() as u64,
            false,
        );
        GatewayError {
            protocol,
            status,
            code,
            message,
            request_id: request_id.clone(),
        }
    };

    if let Some(len) = parts
        .headers
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok())
    {
        if len > max_body {
            return Err(reject(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request_too_large",
                "",
                len as u64,
                format!("request body {len} bytes exceeds the {max_body} byte limit"),
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
            return Err(reject(status, code, "", 0, err.to_string()));
        }
    };
    let bytes_in = bytes.len() as u64;

    let payload: Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(err) => {
            return Err(reject(
                StatusCode::BAD_REQUEST,
                "invalid_json",
                "",
                bytes_in,
                format!("request body is not valid JSON: {err}"),
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
    if protocol == Protocol::Responses
        && payload
            .get("previous_response_id")
            .is_some_and(|value| !value.is_null())
    {
        return Err(reject(
            StatusCode::BAD_REQUEST,
            "unsupported_parameter",
            &requested_model,
            bytes_in,
            "previous_response_id is not supported by this gateway; send the full input history"
                .to_string(),
        ));
    }

    if requested_model.is_empty() {
        return Err(reject(
            StatusCode::BAD_REQUEST,
            "missing_model",
            "",
            bytes_in,
            "request body must contain a 'model' field".to_string(),
        ));
    }

    let models = state.models();
    let Some(model) = registry::resolve_models(&models, &requested_model).cloned() else {
        return Err(reject(
            StatusCode::NOT_FOUND,
            "model_not_found",
            &requested_model,
            bytes_in,
            format!("unknown model '{requested_model}'"),
        ));
    };

    if !util::any_glob_match(&client.allowed_models, &model.id) {
        return Err(reject(
            StatusCode::FORBIDDEN,
            "model_not_allowed",
            &requested_model,
            bytes_in,
            format!(
                "client '{}' is not allowed to use model '{}'",
                client.name, model.id
            ),
        ));
    }

    if !client.allowed_protocols.iter().any(|p| p == protocol_name) {
        return Err(reject(
            StatusCode::FORBIDDEN,
            "protocol_not_allowed",
            &requested_model,
            bytes_in,
            format!(
                "client '{}' is not allowed to use the {protocol_name} protocol",
                client.name
            ),
        ));
    }

    // Token budget: reserve the requested output cap. Only model-call routes
    // carry a slot (count_tokens is free and never reserves).
    if let (Some(budget), Some(slot)) = (client.budget_tokens, budget_slot) {
        let amount = requested_output_cap(protocol, &payload);
        match state.budgets().try_reserve(&client.name, budget, amount) {
            Ok(reservation) => slot.fill(reservation),
            Err(denied) => {
                return Err(reject(
                    StatusCode::FORBIDDEN,
                    "budget_exhausted",
                    &requested_model,
                    bytes_in,
                    denied.message(),
                ));
            }
        }
    }

    Ok(Prepared {
        cfg,
        client,
        client_ip: req_ip,
        request_id,
        started,
        bytes_in,
        payload,
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
