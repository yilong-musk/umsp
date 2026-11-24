use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use serde_json::Value;

use crate::circuit::Admission;
use crate::config::{Client, Config, Model};

use super::client::Upstream;
use super::error::{
    circuit_open_response, client_response_headers, gateway_error, mapped_upstream_status,
    no_backend_response, protocol_gateway_error, truncate_body, upstream_failure_response,
};
use super::failover::{
    AttemptCursor, AttemptResult, AttemptTarget, advance_route, attempt_candidates,
    headers_for_attempt, note_circuit_error, note_circuit_result, prepare_attempt, request_record,
};
use super::log::{plog, plog_fallback};
use super::state::AppState;

/// Which non-streaming upstream call a handler is making.
///
/// `MessagesCountTokens` rides the Messages protocol but has its own endpoint
/// (`/v1/messages/count_tokens`) and its own usage shape (spec 2.3).
#[derive(Debug, Clone)]
pub(super) enum NonStreamCall {
    Chat,
    Responses,
    Messages { query: Option<String> },
    MessagesCountTokens,
}

impl NonStreamCall {
    /// Ledger `protocol` value: count_tokens is a Messages-protocol call.
    pub(super) fn protocol(&self) -> &'static str {
        match self {
            NonStreamCall::Chat => "chat",
            NonStreamCall::Responses => "responses",
            NonStreamCall::Messages { .. } | NonStreamCall::MessagesCountTokens => "messages",
        }
    }

    pub(super) fn query(&self) -> Option<&str> {
        match self {
            NonStreamCall::Messages { query } => query.as_deref(),
            _ => None,
        }
    }

    /// Ledger quirk label for calls that are not one of the three entry points.
    pub(super) fn quirk(&self) -> Option<&'static str> {
        match self {
            NonStreamCall::MessagesCountTokens => Some("count_tokens"),
            _ => None,
        }
    }
}

/// Writes a 499 ledger row when a buffered request is dropped (the client went
/// away) while an attempt is in flight. The normal path disarms it and writes
/// its own row, so every request ends with exactly one row and one metric.
pub(super) struct BufferedCancelGuard<'a> {
    state: &'a AppState,
    request_id: &'a str,
    client: &'a str,
    client_ip: &'a str,
    protocol: &'a str,
    bytes_in: u64,
    started: Instant,
    quirks: &'a [String],
    /// `(model id, provider id, upstream model)` of the attempt in flight.
    target: Option<(String, String, String)>,
    fallbacks: u32,
}

impl BufferedCancelGuard<'_> {
    pub(super) fn attempting(&mut self, target: &AttemptTarget, fallbacks: u32) {
        self.target = Some((
            target.model.id.clone(),
            target.provider.id.clone(),
            target.upstream.clone(),
        ));
        self.fallbacks = fallbacks;
    }

    pub(super) fn disarm(&mut self) {
        self.target = None;
    }
}

impl Drop for BufferedCancelGuard<'_> {
    fn drop(&mut self) {
        let Some((model, provider, upstream)) = self.target.take() else {
            return;
        };
        let mut record = request_record(
            self.request_id,
            self.client,
            self.client_ip,
            self.protocol,
            &model,
            &provider,
            &upstream,
            false,
            self.bytes_in,
            self.started.elapsed().as_millis() as u64,
            self.quirks.to_vec(),
        );
        record.status = 499;
        record.client_disconnected = true;
        record.error_code = Some("client_disconnected".to_string());
        record.fallbacks = self.fallbacks;
        self.state.finish_request(&record);
    }
}

/// Shared non-streaming proxy path: one upstream call, one ledger row.
///
/// A provider that speaks the client protocol relays the body. A mismatch
/// translates the successful body back into the client's protocol.
#[allow(clippy::too_many_arguments)]
pub(super) async fn proxy_non_stream(
    state: AppState,
    cfg: Arc<Config>,
    client: Client,
    client_ip: String,
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
        state.env(),
        state.http(),
        state.stream_http().clone(),
        state.sessions(),
    );
    let protocol = call.protocol();
    let count_tokens = matches!(call, NonStreamCall::MessagesCountTokens);

    let candidates = attempt_candidates(
        &cfg,
        &model,
        protocol,
        count_tokens,
        &state.catalog_snapshot(),
    );
    if candidates.is_empty() {
        return no_backend_response(protocol, &model.id, &request_id);
    }

    let mut cursor = AttemptCursor {
        excluded_providers: HashSet::new(),
        skipped: HashSet::new(),
        planned: Vec::new(),
        index: 0,
        loaded: false,
        exhausted: false,
        use_pin: !count_tokens,
    };
    let mut fallbacks_used = 0u32;
    let mut previous_reason: Option<String> = None;
    let mut previous: Option<AttemptTarget> = None;
    let mut first_provider: Option<String> = None;
    let mut translation_error: Option<String> = None;
    let mut last: Option<(AttemptResult, AttemptTarget, Vec<String>)> = None;
    let mut cancel_guard = BufferedCancelGuard {
        state: &state,
        request_id: &request_id,
        client: &client.name,
        client_ip: &client_ip,
        protocol,
        bytes_in,
        started,
        quirks: &request_quirks,
        target: None,
        fallbacks: 0,
    };

    while let Some(target) = cursor.next(&state, &model.id, &candidates) {
        if let Some(prev) = &previous {
            fallbacks_used += 1;
            plog_fallback(
                &request_id,
                &client.name,
                &prev.provider.id,
                &prev.upstream,
                &target.provider.id,
                &target.upstream,
                previous_reason.as_deref().unwrap_or("retryable"),
            );
        }
        cancel_guard.attempting(&target, fallbacks_used);
        first_provider.get_or_insert_with(|| target.provider.id.clone());
        let (mut result, attempt_quirks) =
            match prepare_attempt(&payload, &target, protocol, &client.name) {
                Err(message) => (AttemptResult::Untranslatable { message }, Vec::new()),
                Ok((attempt_body, mut attempt_quirks)) => {
                    attempt_quirks.splice(0..0, request_quirks.iter().cloned());
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
                                        up.responses(
                                            &target.provider,
                                            &attempt_body,
                                            &attempt_headers,
                                        )
                                        .await
                                    }
                                    "messages" => {
                                        let query = (protocol == "messages")
                                            .then(|| call.query())
                                            .flatten();
                                        up.messages(
                                            &target.provider,
                                            query,
                                            &attempt_body,
                                            &attempt_headers,
                                        )
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
                                        (outcome.status >= 400)
                                            .then(|| format!("http_{}", outcome.status)),
                                    );
                                    note_circuit_result(
                                        &state,
                                        &target,
                                        outcome.status,
                                        _probe.is_some(),
                                    );
                                    AttemptResult::Response(outcome)
                                }
                                Err(err) => {
                                    state.note_health(
                                        &target.provider.id,
                                        0,
                                        attempt_started.elapsed().as_millis() as u64,
                                        Some(err.code().to_string()),
                                    );
                                    note_circuit_error(&state, &target, &err);
                                    AttemptResult::Error(err)
                                }
                            }
                        }
                    };
                    (result, attempt_quirks)
                }
            };
        if let Some(code) = result.upstream_fault() {
            state
                .metrics()
                .note_upstream_error(&target.provider.id, &code);
        }
        if let AttemptResult::Response(outcome) = &mut result {
            if (200..300).contains(&outcome.status) && target.upstream_protocol != protocol {
                match crate::translate::translate_response(
                    &outcome.body,
                    &target.upstream_protocol,
                    protocol,
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
                        last = Some((result, target, attempt_quirks));
                        break;
                    }
                }
            }
        }
        previous_reason = Some(result.reason());
        let proceed = advance_route(
            &state,
            &model.id,
            &candidates,
            &mut cursor,
            &target,
            &result,
        );
        last = Some((result, target.clone(), attempt_quirks));
        if proceed {
            previous = Some(target);
            continue;
        }
        break;
    }
    // From here on the request runs to completion synchronously and writes its
    // own row.
    cancel_guard.disarm();
    drop(cancel_guard);

    let Some((result, target, attempt_quirks)) = last else {
        if cursor.exhausted {
            return circuit_open_response(protocol, &request_id);
        }
        return no_backend_response(protocol, &model.id, &request_id);
    };
    let duration_ms = started.elapsed().as_millis() as u64;
    let mut record = request_record(
        &request_id,
        &client.name,
        &client_ip,
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
        if let Some(from) = &first_provider {
            record.quirks.push(format!("fallback_from:{from}"));
        }
    }
    if let Some(quirk) = call.quirk() {
        record.quirks.push(quirk.to_string());
    }

    match result {
        AttemptResult::Response(outcome) => {
            if let Some(message) = translation_error {
                record.status = StatusCode::BAD_GATEWAY.as_u16();
                record.upstream_status = Some(outcome.status);
                record.error_code = Some("translation_failed".to_string());
                record.duration_ms = outcome.duration_ms;
                state.finish_request(&record);
                return protocol_gateway_error(
                    protocol,
                    StatusCode::BAD_GATEWAY,
                    "translation_failed",
                    &message,
                    &request_id,
                );
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
            let (status, code, message) = (err.status(), err.code(), err.client_message());
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
            protocol_gateway_error(protocol, status, code, message, &request_id)
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
