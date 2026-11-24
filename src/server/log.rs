use serde_json::json;

use crate::util;

pub(super) fn plog_route(event: &str, model: &str, provider: &str, reason: &str) {
    eprintln!(
        "{}",
        json!({
            "ts": util::now_rfc3339(),
            "level": if event == "route_unpin" { "warn" } else { "info" },
            "event": event,
            "model": model,
            "provider": provider,
            "reason": reason,
        })
    );
}

/// Operational log event for the circuit state machine (no keys, no bodies).
pub(super) fn plog_health(event: &str, provider: &str, model: &str, reason: &str) {
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
pub(super) fn plog_fallback(
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

/// Operational log for device-binding events. Server-side only: the bound IP
/// never appears in client-facing responses.
pub(super) fn plog_binding(
    event: &str,
    client: &str,
    presented_ip: &str,
    bound_ip: Option<&str>,
    request_id: &str,
) {
    let line = json!({
        "ts": util::now_rfc3339(),
        "level": if event == "client_bound" { "info" } else { "warn" },
        "event": event,
        "request_id": request_id,
        "client": client,
        "presented_ip": presented_ip,
        "bound_ip": bound_ip,
    });
    eprintln!("{line}");
}

// ---------------------------------------------------------------------------
// Server lifecycle
// ---------------------------------------------------------------------------
