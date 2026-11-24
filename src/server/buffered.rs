use std::time::Instant;

use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;

use super::client::Upstream;
use super::error::{
    Protocol, circuit_open_response, client_response_headers, gateway_error,
    mapped_upstream_status, no_backend_response, protocol_gateway_error, truncate_body,
    upstream_failure_response,
};
use super::failover::{
    AttemptLoop, AttemptResult, AttemptTarget, AttemptsOutcome, attempt_candidates,
    note_circuit_error, note_circuit_result, request_record, run_attempts,
};
use super::log::plog;
use super::prepare::Prepared;
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
    pub(super) fn protocol(&self) -> Protocol {
        match self {
            NonStreamCall::Chat => Protocol::Chat,
            NonStreamCall::Responses => Protocol::Responses,
            NonStreamCall::Messages { .. } | NonStreamCall::MessagesCountTokens => {
                Protocol::Messages
            }
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
pub(super) async fn proxy_non_stream(
    state: AppState,
    prepared: Prepared,
    request_quirks: Vec<String>,
    call: NonStreamCall,
) -> Response {
    let Prepared {
        cfg,
        client,
        client_ip,
        request_id,
        started,
        bytes_in,
        model,
        payload,
        client_headers,
        ..
    } = prepared;
    let up = Upstream::with_clients(
        cfg.clone(),
        state.env(),
        state.http(),
        state.stream_http().clone(),
        state.sessions(),
    );
    let protocol_kind = call.protocol();
    let protocol = protocol_kind.as_str();
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

    let AttemptsOutcome {
        last,
        fallbacks_used,
        first_provider,
        translation_error,
        exhausted,
    } = run_attempts(
        AttemptLoop {
            state: &state,
            model_id: &model.id,
            protocol: protocol_kind,
            request_id: &request_id,
            client_name: &client.name,
            payload: &payload,
            client_headers: &client_headers,
            request_quirks: &request_quirks,
            use_pin: !count_tokens,
            translate_buffered_response: true,
        },
        &candidates,
        |target, fallbacks| cancel_guard.attempting(target, fallbacks),
        |target, attempt_body, attempt_headers, probe_gen| {
            let state = &state;
            let up = &up;
            let call = &call;
            async move {
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
                            let query = (protocol_kind == Protocol::Messages)
                                .then(|| call.query())
                                .flatten();
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
                        note_circuit_result(state, &target, outcome.status, probe_gen);
                        AttemptResult::Response(outcome)
                    }
                    Err(err) => {
                        state.note_health(
                            &target.provider.id,
                            0,
                            attempt_started.elapsed().as_millis() as u64,
                            Some(err.code().to_string()),
                        );
                        note_circuit_error(state, &target, &err);
                        AttemptResult::Error(err)
                    }
                }
            }
        },
    )
    .await;
    // From here on the request runs to completion synchronously and writes its
    // own row.
    cancel_guard.disarm();
    drop(cancel_guard);

    let Some((result, target, _inflight, attempt_quirks)) = last else {
        if exhausted {
            return circuit_open_response(protocol, &request_id);
        }
        return no_backend_response(protocol, &model.id, &request_id);
    };
    drop(_inflight);
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
