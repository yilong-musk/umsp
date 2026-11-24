use axum::body::Body;
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::json;

use super::client::ProxyError;
use super::failover::AttemptTarget;

pub(super) fn error_type_for(status: StatusCode) -> &'static str {
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

pub(super) fn messages_error_type(status: StatusCode) -> &'static str {
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

/// 400 when the logical model exists but no backend can serve this protocol.
pub(super) fn no_backend_response(protocol: &str, model_id: &str, request_id: &str) -> Response {
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
pub(super) fn circuit_open_response(protocol: &str, request_id: &str) -> Response {
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
pub(super) fn client_response_headers(
    headers: &[(HeaderName, HeaderValue)],
) -> Vec<(HeaderName, HeaderValue)> {
    headers
        .iter()
        .filter(|(name, _)| matches!(name.as_str(), "content-type" | "retry-after"))
        .cloned()
        .collect()
}

pub(super) fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_')
}

/// Replace `needle` only where it stands alone, so `deepseek` inside
/// `deepseek-v4.1-flash` is left alone.
pub(super) fn replace_token(text: &str, needle: &str, replacement: &str) -> String {
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
pub(super) fn scrub_upstream_identifiers(
    body: &[u8],
    model_id: &str,
    target: &AttemptTarget,
) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(body) else {
        return body.to_vec();
    };
    let text = replace_token(text, &target.upstream, model_id);
    replace_token(&text, &target.provider.id, "umsp").into_bytes()
}

/// Keep a bounded copy of an upstream body in the ledger for diagnosis.
pub(super) fn truncate_body(body: &[u8]) -> String {
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
pub(super) fn mapped_upstream_status(status: u16) -> u16 {
    match status {
        401..=403 | 426 | 500..=599 => 502,
        other => other,
    }
}

pub(super) fn upstream_failure_response(
    protocol: &str,
    status: u16,
    body: &[u8],
    model_id: &str,
    target: &AttemptTarget,
    request_id: &str,
    rewrite_error: bool,
) -> Response {
    if status == 426 {
        return protocol_gateway_error(
            protocol,
            StatusCode::BAD_GATEWAY,
            "upstream_client_outdated",
            "the upstream service requires a newer gateway client version",
            request_id,
        );
    }
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

impl IntoResponse for ProxyError {
    fn into_response(self) -> Response {
        let status = self.status();
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
