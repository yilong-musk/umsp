use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{ConnectInfo, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use serde_json::{Value, json};

use crate::circuit::CircuitState;
use crate::config::Config;
use crate::registry;
use crate::util;

use super::VERSION;
use super::background::{probe_provider, refresh_catalog};
use super::buffered::{NonStreamCall, proxy_non_stream};
use super::error::{gateway_error, messages_error};
use super::failover::attempt_candidates;
use super::prepare::{AuthedClient, Prepared, prepare_request};
use super::state::AppState;
use super::stream_proxy::{StreamProtocol, proxy_stream};

pub(super) async fn hello_handler() -> StatusCode {
    StatusCode::NO_CONTENT
}

pub(super) async fn models_handler(State(state): State<AppState>, req: Request) -> Response {
    let request_id = util::new_request_id();
    let cfg = state.cfg();
    let models = state.models();
    let client = req
        .extensions()
        .get::<AuthedClient>()
        .map(|c| c.0.name.clone());
    let payload = registry::models_from(&models);
    let visible = visible_models(&cfg, client.as_deref(), &payload);
    let body = serde_json::to_vec(&visible).unwrap_or_default();
    Response::builder()
        .status(StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .header("x-umsp-request-id", request_id)
        .body(Body::from(body))
        .expect("valid response")
}

pub(super) fn visible_models(cfg: &Config, client: Option<&str>, payload: &Value) -> Value {
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

/// Admin endpoints (`/healthz`, `/metrics`, `/catalog/refresh`) answer loopback peers only
/// (spec 6.2 / 8.5). A request with no `ConnectInfo` never crossed a socket —
/// the in-process router used by tests — so it is allowed.
pub(super) async fn admin_loopback_guard(req: Request, next: Next) -> Response {
    match req.extensions().get::<ConnectInfo<SocketAddr>>() {
        Some(ConnectInfo(addr)) if !addr.ip().is_loopback() => loopback_only_response(),
        _ => next.run(req).await,
    }
}

pub(super) fn loopback_only_response() -> Response {
    Response::builder()
        .status(StatusCode::FORBIDDEN)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            "{\"error\":{\"message\":\"admin endpoints are loopback-only\",\
             \"type\":\"permission_error\",\"code\":\"loopback_only\"}}",
        ))
        .expect("valid response")
}

pub(super) async fn healthz_handler(
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

    let probe_results: HashMap<String, Value> = if probe {
        let futs = cfg.providers.iter().map(|provider| {
            let state = state.clone();
            let cfg = Arc::clone(&cfg);
            let provider = provider.clone();
            async move {
                let value = probe_provider(&state, &cfg, &provider).await;
                (provider.id, value)
            }
        });
        futures_util::future::join_all(futs)
            .await
            .into_iter()
            .collect()
    } else {
        HashMap::new()
    };

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
        if let Some(usage) = state.route_usage(&provider.id) {
            entry["usage"] = json!({
                "remaining_fraction": usage.remaining_fraction,
                "windows": usage.windows,
                "reset_in_s": usage.reset_in_s,
            });
            if usage.remaining_fraction <= 0.0 {
                entry["status"] = json!("degraded");
                overall_ok = false;
            }
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
        if let Some(probe_value) = probe_results.get(&provider.id) {
            if let Some(balance) = probe_value.get("balance") {
                entry["balance"] = balance.clone();
            }
            if probe_value["status"] != "ok" {
                overall_ok = false;
            }
            entry["probe"] = probe_value.clone();
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
    // Health endpoints conventionally stay 200 for "degraded"; the body carries state.
    Response::builder()
        .status(StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .expect("valid response")
}

/// `GET /metrics` — Prometheus text exposition (spec 6.2). Loopback only, no
/// client key required, never triggers an upstream request.
pub(super) async fn metrics_handler(State(state): State<AppState>) -> Response {
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

pub(super) async fn chat_handler(State(state): State<AppState>, req: Request) -> Response {
    let Prepared {
        cfg,
        client,
        client_ip,
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
            client_ip,
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
        client_ip,
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

/// `POST /v1/responses` — OpenAI Responses (OD provider, non-streaming and
/// streaming). Only `model` is rewritten; every other field is relayed as-is
/// (spec 2.2).
pub(super) async fn responses_handler(State(state): State<AppState>, req: Request) -> Response {
    let Prepared {
        cfg,
        client,
        client_ip,
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
            client_ip,
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
        client_ip,
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
pub(super) const DEFAULT_ANTHROPIC_VERSION: &str = "2023-06-01";

pub(super) fn with_default_anthropic_version(mut headers: HeaderMap) -> HeaderMap {
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
pub(super) async fn messages_handler(State(state): State<AppState>, req: Request) -> Response {
    let Prepared {
        cfg,
        client,
        client_ip,
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
            client_ip,
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
        client_ip,
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
pub(super) async fn messages_count_tokens_handler(
    State(state): State<AppState>,
    req: Request,
) -> Response {
    let Prepared {
        cfg,
        client,
        client_ip,
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

    if attempt_candidates(&cfg, &model, "messages", true, &state.catalog_snapshot()).is_empty() {
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
        client_ip,
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
pub(super) async fn responses_unsupported_handler(
    State(_state): State<AppState>,
    req: Request,
) -> Response {
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
pub(super) fn inject_stream_include_usage(payload: &mut Value) -> bool {
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

pub(super) async fn catalog_refresh_handler(State(state): State<AppState>) -> Response {
    let providers = refresh_catalog(&state).await;
    let body = serde_json::to_vec(&json!({"ok": true, "providers": providers})).unwrap_or_default();
    Response::builder()
        .status(StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .expect("valid response")
}
