use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::http::StatusCode;
use axum::response::Response;

use crate::stream;
use crate::usage::{RequestRecord, Usage};

use super::client::Upstream;
use super::error::{
    Protocol, circuit_open_response, gateway_error, mapped_upstream_status, no_backend_response,
    protocol_gateway_error, truncate_body, upstream_failure_response,
};
use super::failover::{
    AttemptLoop, AttemptResult, AttemptsOutcome, attempt_candidates, note_circuit_error,
    note_circuit_result, request_record, run_attempts,
};
use super::log::{plog, plog_health, plog_route};
use super::prepare::Prepared;
use super::state::AppState;

/// Ledger guard for the streaming path: if the handler future is cancelled
/// because the client vanished before the stream opened, the row is still
/// written (status 499, `client_disconnected`) instead of disappearing.
pub(super) struct StreamGuard {
    state: AppState,
    started: Instant,
    record: Option<RequestRecord>,
}

impl StreamGuard {
    pub(super) fn new(state: AppState, record: RequestRecord, started: Instant) -> Self {
        Self {
            state,
            started,
            record: Some(record),
        }
    }

    pub(super) fn record_mut(&mut self) -> &mut RequestRecord {
        self.record.as_mut().expect("stream record present")
    }

    pub(super) fn take(&mut self) -> RequestRecord {
        self.record.take().expect("stream record present")
    }

    /// Write the row and disarm the guard.
    pub(super) fn commit(&mut self) {
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
pub(super) enum StreamProtocol {
    Chat,
    Responses { emit_event_lines: bool },
    Messages,
}

impl StreamProtocol {
    pub(super) fn protocol(self) -> Protocol {
        match self {
            StreamProtocol::Chat => Protocol::Chat,
            StreamProtocol::Responses { .. } => Protocol::Responses,
            StreamProtocol::Messages => Protocol::Messages,
        }
    }

    pub(super) fn name(self) -> &'static str {
        self.protocol().as_str()
    }

    pub(super) fn observer(self) -> stream::Observer {
        match self {
            StreamProtocol::Chat => stream::Observer::chat(),
            StreamProtocol::Responses { emit_event_lines } => {
                stream::Observer::responses(emit_event_lines)
            }
            StreamProtocol::Messages => stream::Observer::messages(),
        }
    }
}

/// Streaming proxy for Chat Completions / Responses / Messages: open the
/// upstream stream, then hand the live body to `pump_stream` and an
/// unaggregated byte stream to the client.
pub(super) async fn proxy_stream(
    state: AppState,
    prepared: Prepared,
    request_quirks: Vec<String>,
    protocol: StreamProtocol,
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
        query,
        ..
    } = prepared;
    let up = Upstream::with_clients(
        cfg.clone(),
        state.env(),
        state.http(),
        state.stream_http().clone(),
        state.sessions(),
    );
    let header_timeout = Duration::from_secs(cfg.server.stream_header_timeout_s.max(1));
    let protocol_kind = protocol.protocol();
    let protocol_name = protocol.name();

    let candidates = attempt_candidates(
        &cfg,
        &model,
        protocol_name,
        false,
        &state.catalog_snapshot(),
    );
    if candidates.is_empty() {
        return no_backend_response(protocol_name, &model.id, &request_id);
    }

    let first = &candidates[0];
    let mut guard = StreamGuard::new(
        state.clone(),
        request_record(
            &request_id,
            &client.name,
            &client_ip,
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

    // A retry is only possible before any byte reaches the client, so
    // everything up to the `200` below is safe.
    let AttemptsOutcome {
        last,
        fallbacks_used,
        first_provider,
        exhausted,
        ..
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
            use_pin: true,
            translate_buffered_response: false,
        },
        &candidates,
        |_, _| {},
        |target, attempt_body, attempt_headers, probe_gen| {
            let state = &state;
            let up = &up;
            let query = &query;
            async move {
                let attempt_started = Instant::now();
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
                            attempt_started.elapsed().as_millis() as u64,
                            (stream.status >= 400).then(|| format!("http_{}", stream.status)),
                        );
                        // A 2xx stream is judged when it ends (in
                        // `pump_stream`): recording success at the headers
                        // would reset the failure window, so a provider that
                        // always dies mid-stream could never trip the circuit.
                        // A half-open probe still closes on the headers.
                        if stream.status >= 400 || probe_gen.is_some() {
                            note_circuit_result(state, &target, stream.status, probe_gen);
                        }
                        AttemptResult::Stream(stream)
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

    let Some((result, target, inflight, attempt_quirks)) = last else {
        // Not a client disconnect: disarm before returning a gateway error.
        let _ = guard.take();
        if exhausted {
            return circuit_open_response(protocol_name, &request_id);
        }
        return no_backend_response(protocol_name, &model.id, &request_id);
    };
    {
        let record = guard.record_mut();
        record.requested_model = target.model.id.clone();
        record.provider = target.provider.id.clone();
        record.upstream_model = target.upstream.clone();
        record.fallbacks = fallbacks_used;
        record.quirks.extend(attempt_quirks);
        if fallbacks_used > 0 {
            if let Some(from) = &first_provider {
                record.quirks.push(format!("fallback_from:{from}"));
            }
        }
    }

    let stream = match result {
        AttemptResult::Stream(stream) => stream,
        AttemptResult::Error(err) => {
            let (status, code, message) = (err.status(), err.code(), err.client_message());
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
            return protocol_gateway_error(protocol_name, status, code, message, &request_id);
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
                return protocol_gateway_error(
                    protocol_name,
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
pub(super) enum PumpObserver {
    Native(stream::Observer),
    Bridge(Box<crate::translate::StreamBridge>),
}

impl PumpObserver {
    pub(super) fn transform(&mut self, chunk: &[u8], out: &mut Vec<u8>) -> bool {
        match self {
            Self::Native(observer) => observer.transform(chunk, out),
            Self::Bridge(bridge) => bridge.push(chunk, out),
        }
    }

    pub(super) fn finish(&mut self, out: &mut Vec<u8>) {
        match self {
            Self::Native(observer) => observer.finish(out),
            Self::Bridge(bridge) => bridge.finish(out),
        }
    }

    pub(super) fn done(&self) -> bool {
        match self {
            Self::Native(observer) => observer.done(),
            Self::Bridge(bridge) => bridge.done(),
        }
    }

    pub(super) fn usage(&self) -> Option<Usage> {
        match self {
            Self::Native(observer) => observer.usage(),
            Self::Bridge(bridge) => bridge.usage(),
        }
    }

    pub(super) fn keepalive(&self) -> &'static [u8] {
        match self {
            Self::Native(observer) => observer.keepalive(),
            Self::Bridge(bridge) => bridge.keepalive(),
        }
    }

    pub(super) fn error_event(&mut self, message: &str) -> Vec<u8> {
        match self {
            Self::Native(observer) => observer.error_event(message),
            Self::Bridge(bridge) => bridge.error_event(message),
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn pump_stream(
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
    // The idle deadline only moves when upstream bytes arrive. Keepalive
    // frames are gateway-generated and must not extend it, otherwise a silent
    // upstream would be kept open forever.
    let mut idle_deadline = tokio::time::Instant::now() + idle;

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
            item = upstream.next() => {
                match item {
                    None => break,
                    Some(Err(_)) => {
                        upstream_error = Some("upstream_stream_error");
                        break;
                    }
                    Some(Ok(chunk)) => {
                        idle_deadline = tokio::time::Instant::now() + idle;
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
            _ = tokio::time::sleep_until(idle_deadline) => {
                upstream_error = Some("upstream_stream_timeout");
                break;
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

    // A stream that failed after its 200 is evidence against the provider:
    // count it toward the circuit and drop the five-hour pin so the next
    // request can select again. A clean end is the stream's success. A client
    // disconnect says nothing about the provider and records neither.
    if !disconnected {
        match record.error_code.clone() {
            Some(code) => {
                if state.circuits().record_failure(&provider_id, &code) {
                    plog_health("circuit_open", &provider_id, &record.upstream_model, &code);
                }
                if state.forget_route(&record.requested_model, &provider_id) {
                    plog_route("route_unpin", &record.requested_model, &provider_id, &code);
                }
            }
            None => {
                if state.circuits().record_success(&provider_id).is_some() {
                    plog_health(
                        "circuit_close",
                        &provider_id,
                        &record.upstream_model,
                        "success",
                    );
                }
            }
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
