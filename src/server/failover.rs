use std::collections::{HashMap, HashSet};
use std::future::Future;

use axum::http::{HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;

use crate::adapter::{self, PaymentFailurePolicy};
use crate::circuit::Admission;
use crate::config::{Config, Model, Provider};
use crate::route::PinDrop;
use crate::upstream;
use crate::usage::RequestRecord;
use crate::util;

use super::client::{ChatOutcome, ChatStream, ProxyError};
use super::error::Protocol;
use super::handlers::DEFAULT_ANTHROPIC_VERSION;
use super::log::{plog_fallback, plog_health, plog_route};
use super::state::AppState;

// ---------------------------------------------------------------------------
// Fallback planning and circuit accounting (spec 3.3 / 3.4)
// ---------------------------------------------------------------------------

/// One candidate upstream for a request: a logical model served through one
/// provider/upstream pair.
#[derive(Debug, Clone)]
pub(super) struct AttemptTarget {
    pub(super) model: Model,
    pub(super) provider: Provider,
    pub(super) upstream: String,
    /// Protocol actually sent upstream. Equals the client protocol on a
    /// passthrough, and a protocol the provider lists when translating.
    pub(super) upstream_protocol: String,
}

/// Outcome of one attempt, before anything reaches the client.
pub(super) enum AttemptResult {
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
    pub(super) fn retryable(&self) -> bool {
        match self {
            AttemptResult::Response(outcome) => upstream::status_is_retryable(outcome.status),
            AttemptResult::Stream(stream) => upstream::status_is_retryable(stream.status),
            AttemptResult::Error(_)
            | AttemptResult::CircuitOpen { .. }
            | AttemptResult::Untranslatable { .. } => true,
        }
    }

    pub(super) fn reason(&self) -> String {
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
    pub(super) fn upstream_fault(&self) -> Option<String> {
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

/// Backends that can serve `model`, in configuration order.
///
/// A backend is eligible when its provider exists and speaks at least one of
/// the three protocols. The attempt uses the client protocol when the provider
/// lists it (passthrough) and otherwise the first protocol in that provider's
/// list (translation). `count_tokens` stays strict: the provider must speak
/// Messages and implement the endpoint. Selection reorders this list.
pub(super) fn attempt_candidates(
    cfg: &Config,
    model: &Model,
    protocol: &str,
    count_tokens: bool,
    listings: &HashMap<String, Vec<crate::catalog::ListedModel>>,
) -> Vec<AttemptTarget> {
    model
        .backends
        .iter()
        .filter_map(|backend| {
            let provider = cfg.providers.iter().find(|p| p.id == backend.provider)?;
            let protocols = crate::catalog::protocols_for(
                listings,
                &provider.id,
                &backend.upstream,
                &provider.protocols,
            );
            let upstream_protocol = if count_tokens {
                if !protocols.iter().any(|item| item == protocol)
                    || !provider.supports_count_tokens()
                {
                    return None;
                }
                protocol
            } else {
                crate::translate::select_upstream_protocol(&protocols, protocol)?
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
/// `model` plus provider adapter patches. A protocol mismatch is translated
/// first, then runs through the same adapter patch hook.
pub(super) fn prepare_attempt(
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
    notes.extend(adapter::for_provider(&target.provider).patch_request(
        &mut body,
        &target.upstream_protocol,
        client_name,
    ));
    Ok((body, notes))
}

/// Passthrough keeps the client's headers. A translated hop drops Anthropic
/// framing when the upstream protocol is not Messages, and fills
/// `anthropic-version` when the upstream protocol is Messages.
pub(super) fn headers_for_attempt(
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
/// the target provider's adapter patches are applied. Returns the applied quirk
/// tags for the ledger.
pub(super) fn attempt_payload(
    base: &Value,
    target: &AttemptTarget,
    protocol: &str,
    client_name: &str,
) -> (Value, Vec<String>) {
    let mut payload = base.clone();
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("model".to_string(), Value::String(target.upstream.clone()));
    }
    // Quirks are provider behaviour, so they run per attempt: two backends of
    // the same model may need different patches.
    let applied =
        adapter::for_provider(&target.provider).patch_request(&mut payload, protocol, client_name);
    (payload, applied)
}

/// Feed one upstream status into the provider's circuit (spec 3.4).
///
/// `probe_gen` is `Some(generation)` when this attempt held the single
/// half-open probe slot. A non-retryable client error on a probe proves the
/// provider is reachable, so it closes the circuit instead of leaving it stuck
/// in `half_open`; the probe guard would release the slot either way.
pub(super) fn note_circuit_result(
    state: &AppState,
    target: &AttemptTarget,
    status: u16,
    probe_gen: Option<u64>,
) {
    let circuits = state.circuits();
    if status < 400 {
        let closed = match probe_gen {
            Some(generation) => circuits.record_probe_success(&target.provider.id, generation),
            None => circuits.record_success(&target.provider.id),
        };
        if closed.is_some() {
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
            let (reason, cooldown) = match adapter::for_provider(&target.provider)
                .payment_failure_policy()
            {
                PaymentFailurePolicy::InsufficientBalance => {
                    ("insufficient_balance", config.ds_balance_cooldown)
                }
                PaymentFailurePolicy::PaymentRequired => ("payment_required", config.auth_cooldown),
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
            if let Some(generation) = probe_gen {
                if circuits
                    .record_probe_success(&target.provider.id, generation)
                    .is_some()
                {
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
}

/// Connection error / timeout: a retryable failure that counts toward the
/// circuit (spec 3.4).
pub(super) fn note_circuit_error(state: &AppState, target: &AttemptTarget, err: &ProxyError) {
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

/// Walks one selection list. A failed upstream is skipped; the provider's
/// other backends stay in the list. The list is built once.
pub(super) struct AttemptCursor {
    pub(super) excluded_providers: HashSet<String>,
    pub(super) skipped: HashSet<(String, String)>,
    pub(super) planned: Vec<AttemptTarget>,
    pub(super) index: usize,
    pub(super) loaded: bool,
    /// A non-empty candidate list produced an empty plan: every backend is
    /// known to be spent, so retrying one would only burn a request.
    pub(super) exhausted: bool,
    /// Completions read and write the model pin. `count_tokens` does not.
    pub(super) use_pin: bool,
}

impl AttemptCursor {
    pub(super) fn next(
        &mut self,
        state: &AppState,
        model: &str,
        candidates: &[AttemptTarget],
    ) -> Option<AttemptTarget> {
        if !self.loaded {
            self.planned = state.route_targets(
                model,
                candidates,
                &self.excluded_providers,
                &self.skipped,
                self.use_pin,
            );
            self.exhausted = self.planned.is_empty() && !candidates.is_empty();
            self.loaded = true;
        }
        while self.index < self.planned.len() {
            let target = self.planned[self.index].clone();
            self.index += 1;
            if self.excluded_providers.contains(&target.provider.id) {
                continue;
            }
            let key = (target.provider.id.clone(), target.upstream.clone());
            if self.skipped.contains(&key) {
                continue;
            }
            return Some(target);
        }
        None
    }

    pub(super) fn skip_backend(&mut self, provider: &str, upstream: &str) {
        self.skipped
            .insert((provider.to_string(), upstream.to_string()));
    }

    pub(super) fn skip_provider(&mut self, provider: &str) {
        self.excluded_providers.insert(provider.to_string());
    }

    pub(super) fn has_more(&self) -> bool {
        self.planned.iter().skip(self.index).any(|target| {
            !self.excluded_providers.contains(&target.provider.id)
                && !self
                    .skipped
                    .contains(&(target.provider.id.clone(), target.upstream.clone()))
        })
    }
}

pub(super) fn attempt_succeeded(result: &AttemptResult) -> bool {
    match result {
        AttemptResult::Response(outcome) => (200..300).contains(&outcome.status),
        AttemptResult::Stream(stream) => (200..300).contains(&stream.status),
        _ => false,
    }
}

pub(super) fn backend_remains(
    candidates: &[AttemptTarget],
    provider: &str,
    skipped: &HashSet<(String, String)>,
) -> bool {
    candidates.iter().any(|candidate| {
        candidate.provider.id == provider
            && !skipped.contains(&(candidate.provider.id.clone(), candidate.upstream.clone()))
    })
}

/// Move past a failed attempt. A local translation miss does not clear the
/// pin. An unreachable backend clears it only after that provider has no
/// backend left. Balance exhaustion and an open circuit clear it immediately.
pub(super) fn advance_route(
    state: &AppState,
    model: &str,
    candidates: &[AttemptTarget],
    cursor: &mut AttemptCursor,
    target: &AttemptTarget,
    result: &AttemptResult,
) -> bool {
    if attempt_succeeded(result) {
        if cursor.use_pin {
            note_route_success(state, model, &target.provider.id);
        }
        return false;
    }
    if !result.retryable() {
        return false;
    }
    let provider_wide = match result {
        AttemptResult::CircuitOpen { .. } => true,
        AttemptResult::Response(outcome) => outcome.status == 402,
        AttemptResult::Stream(stream) => stream.status == 402,
        _ => false,
    };
    let clears_pin = !matches!(result, AttemptResult::Untranslatable { .. });
    if provider_wide {
        if cursor.use_pin {
            note_route_failure(state, model, &target.provider.id);
        }
        cursor.skip_provider(&target.provider.id);
        return true;
    }
    cursor.skip_backend(&target.provider.id, &target.upstream);
    if cursor.use_pin
        && clears_pin
        && !backend_remains(candidates, &target.provider.id, &cursor.skipped)
    {
        note_route_failure(state, model, &target.provider.id);
    }
    true
}

pub(super) fn note_route_success(state: &AppState, model: &str, provider: &str) {
    if state.remember_route(model, provider) {
        plog_route("route_pin", model, provider, "selected");
    }
}

pub(super) fn note_route_failure(state: &AppState, model: &str, provider: &str) {
    if state.forget_route(model, provider) {
        plog_route(
            "route_unpin",
            model,
            provider,
            PinDrop::Unreachable.as_str(),
        );
    }
}

/// Inputs to the shared backend attempt loop.
pub(super) struct AttemptLoop<'a> {
    pub(super) state: &'a AppState,
    pub(super) model_id: &'a str,
    pub(super) protocol: Protocol,
    pub(super) request_id: &'a str,
    pub(super) client_name: &'a str,
    pub(super) payload: &'a Value,
    pub(super) client_headers: &'a HeaderMap,
    pub(super) request_quirks: &'a [String],
    pub(super) use_pin: bool,
    pub(super) translate_buffered_response: bool,
}

/// Final state produced by [`run_attempts`].
pub(super) struct AttemptsOutcome {
    pub(super) last: Option<(
        AttemptResult,
        AttemptTarget,
        Option<crate::metrics::Inflight>,
        Vec<String>,
    )>,
    pub(super) fallbacks_used: u32,
    pub(super) first_provider: Option<String>,
    pub(super) translation_error: Option<String>,
    pub(super) exhausted: bool,
}

/// Shared candidate attempt driver for both buffered and streaming proxy paths.
///
/// Walks `candidates` via `AttemptCursor`, applies request translation/quirks,
/// enforces circuit admission and probe/in-flight guards, records upstream
/// fault metrics, optionally translates buffered responses, and advances the
/// route on retryable outcomes.
pub(super) async fn run_attempts<F, Fut>(
    ctx: AttemptLoop<'_>,
    candidates: &[AttemptTarget],
    mut on_attempt: impl FnMut(&AttemptTarget, u32),
    mut send: F,
) -> AttemptsOutcome
where
    F: FnMut(AttemptTarget, Value, HeaderMap, Option<u64>) -> Fut,
    Fut: Future<Output = AttemptResult>,
{
    let protocol_name = ctx.protocol.as_str();
    let mut cursor = AttemptCursor {
        excluded_providers: HashSet::new(),
        skipped: HashSet::new(),
        planned: Vec::new(),
        index: 0,
        loaded: false,
        exhausted: false,
        use_pin: ctx.use_pin,
    };
    let mut fallbacks_used = 0u32;
    let mut previous_reason: Option<String> = None;
    let mut previous: Option<AttemptTarget> = None;
    let mut first_provider: Option<String> = None;
    let mut translation_error: Option<String> = None;
    let mut last: Option<(
        AttemptResult,
        AttemptTarget,
        Option<crate::metrics::Inflight>,
        Vec<String>,
    )> = None;

    while let Some(target) = cursor.next(ctx.state, ctx.model_id, candidates) {
        if let Some(prev) = &previous {
            fallbacks_used += 1;
            plog_fallback(
                ctx.request_id,
                ctx.client_name,
                &prev.provider.id,
                &prev.upstream,
                &target.provider.id,
                &target.upstream,
                previous_reason.as_deref().unwrap_or("retryable"),
            );
        }
        on_attempt(&target, fallbacks_used);
        first_provider.get_or_insert_with(|| target.provider.id.clone());

        let (mut result, inflight, attempt_quirks) =
            match prepare_attempt(ctx.payload, &target, protocol_name, ctx.client_name) {
                Err(message) => (AttemptResult::Untranslatable { message }, None, Vec::new()),
                Ok((attempt_body, mut attempt_quirks)) => {
                    attempt_quirks.splice(0..0, ctx.request_quirks.iter().cloned());
                    let attempt_headers = headers_for_attempt(
                        ctx.client_headers,
                        protocol_name,
                        &target.upstream_protocol,
                    );
                    let admission = ctx.state.circuits().admit(&target.provider.id);
                    // A granted half-open probe owns the single probe slot until
                    // this attempt records a result; a cancelled request
                    // releases it on drop so the circuit stays half_open.
                    let _probe = match &admission {
                        Admission::Probe(guard) => Some(guard),
                        _ => None,
                    };
                    // In-flight gauge: travels with a live stream into the pump,
                    // or drops when replaced / when the buffered call returns.
                    let inflight = ctx.state.metrics().inflight(&target.provider.id);
                    let result = match &admission {
                        Admission::Reject => AttemptResult::CircuitOpen {
                            reason: ctx.state.circuits().view(&target.provider.id).reason,
                        },
                        Admission::Allow | Admission::Probe(_) => {
                            send(
                                target.clone(),
                                attempt_body,
                                attempt_headers,
                                _probe.map(|guard| guard.generation()),
                            )
                            .await
                        }
                    };
                    (result, Some(inflight), attempt_quirks)
                }
            };

        if let Some(code) = result.upstream_fault() {
            ctx.state
                .metrics()
                .note_upstream_error(&target.provider.id, &code);
        }
        if ctx.translate_buffered_response {
            if let AttemptResult::Response(outcome) = &mut result {
                if (200..300).contains(&outcome.status) && target.upstream_protocol != protocol_name
                {
                    match crate::translate::translate_response(
                        &outcome.body,
                        &target.upstream_protocol,
                        protocol_name,
                        &target.model.id,
                    ) {
                        Ok(body) => outcome.body = body,
                        Err(message) => {
                            cursor.skip_backend(&target.provider.id, &target.upstream);
                            previous_reason = Some("translation_failed".to_string());
                            if cursor.has_more() {
                                previous = Some(target);
                                continue;
                            }
                            translation_error = Some(message);
                            last = Some((result, target, inflight, attempt_quirks));
                            break;
                        }
                    }
                }
            }
        }
        previous_reason = Some(result.reason());
        let proceed = advance_route(
            ctx.state,
            ctx.model_id,
            candidates,
            &mut cursor,
            &target,
            &result,
        );
        last = Some((result, target.clone(), inflight, attempt_quirks));
        if proceed {
            previous = Some(target);
            continue;
        }
        break;
    }

    AttemptsOutcome {
        last,
        fallbacks_used,
        first_provider,
        translation_error,
        exhausted: cursor.exhausted,
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn request_record(
    request_id: &str,
    client: &str,
    client_ip: &str,
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
        client_ip: Some(client_ip.to_string()),
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
