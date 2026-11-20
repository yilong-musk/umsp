# M2 Verification Record (2026-11-19)

Environment: Ubuntu cloud host, 2 vCPU / 2 GB, musl static binary `umsp 0.1.0`,
upstream OpenDesign `https://amr-link.open-design.ai/v1`, model `od/deepseek-v4.1-flash`.
M2 scope: `POST /v1/responses` (OD provider) **non-streaming + streaming**, backfilling the
`event:` lines OD omits, rejecting `previous_response_id`, and 501 for unsupported Responses sub-endpoints.

## 1. Build and Tests

```console
$ cargo fmt --all -- --check
$ cargo clippy --all-targets -- -D warnings
$ cargo test
running 66 tests ... test result: ok. 66 passed; 0 failed
running 32 tests ... test result: ok. 32 passed; 0 failed
```

M1 baseline: 57 unit + 19 integration = 76; M2: 66 unit + 32 integration = 98 (all M1 cases retained and green).

New integration cases (local fake Responses upstream, through a real axum Router):

`responses_non_stream_relays_body_verbatim_and_records_usage`,
`responses_non_stream_without_usage_records_usage_missing`,
`responses_non_stream_failed_body_is_passed_through`,
`responses_stream_adds_missing_event_lines`, `responses_stream_keeps_existing_event_lines`,
`responses_event_lines_can_be_disabled`, `responses_stream_keepalive_fills_upstream_silence`,
`responses_stream_upstream_error_after_headers_emits_error_event`,
`responses_stream_upstream_error_before_headers_is_relayed_verbatim`,
`responses_previous_response_id_is_400_unsupported_parameter`,
`responses_unsupported_endpoints_are_501_and_chat_survives`,
`responses_oversized_body_is_413`, `responses_streaming_client_disconnect_is_recorded`.

New unit tests: 7 for `ResponsesPatcher` (insert event lines / no duplication when already present / byte-identical
when the switch is off / split across chunks / untouched when no type or comments / error event shape / Chat observer still byte-for-byte),
2 for `Usage::from_responses`, +9 total.

Build:

```console
$ ./scripts/build-musl.sh
binary:  6391184 bytes
archive: dist/umsp-0.1.0-x86_64-unknown-linux-musl.tar.gz
e3e4850541ffc58112f4c1dd51fa33fae15e88a5a9e25f2a57c53e9e787c7d98  umsp-0.1.0-x86_64-unknown-linux-musl.tar.gz
```

## 2. Real OD: Non-Streaming Responses

```console
$ curl -sS -D h.txt http://127.0.0.1:3460/v1/responses \
    -H "Authorization: Bearer ***" -H 'Content-Type: application/json' \
    -d '{"model":"od/deepseek-v4.1-flash","input":"Reply only: M2 non-streaming OK","max_output_tokens":64}'
HTTP/1.1 200 OK
content-type: application/json
x-request-id: <upstream-request-id>
x-umsp-request-id: 06GZ6WPWGFGNRQNPTGH2B27BMM
x-umsp-provider: od
x-umsp-upstream-model: deepseek-v4.1-flash
```

body: `object=response`, `status=completed`, `output` contains `reasoning` + `message`,
top-level `usage = {"input_tokens":39,"output_tokens":37,"total_tokens":76,
"input_tokens_details":{"cached_tokens":0},"output_tokens_details":{"reasoning_tokens":30}}`.
The body is relayed byte-for-byte (the fake upstream case asserts that only `model` is rewritten and unknown fields are untouched).

## 3. Real OD: Streaming Responses

```console
$ curl -sS -N -D h.txt http://127.0.0.1:3460/v1/responses \
    -H "Authorization: Bearer ***" -H 'Content-Type: application/json' \
    -d '{"model":"od/deepseek-v4.1-flash","input":"Reply only: M2 streaming OK","stream":true,"max_output_tokens":64}'
HTTP/1.1 200 OK
content-type: text/event-stream
cache-control: no-cache
x-accel-buffering: no
x-umsp-provider: od
x-umsp-request-id: 06GZ6WQ26ZP8KBN9K1117Y3DWW
x-umsp-upstream-model: deepseek-v4.1-flash
(no content-length)
```

- **29** `event:` lines, **30** `data:` lines, **1** `data: [DONE]`.
- The event sequence matches the spec: `response.created`, `response.in_progress`,
  `response.output_item.added`, `response.reasoning_summary_part.added`,
  `response.reasoning_summary_text.delta/done`, `response.output_item.done`,
  `response.content_part.added`, `response.output_text.delta/done`,
  `response.content_part.done`, `response.completed`.
- `response.completed.response.usage` =
  `{"input_tokens":43,"output_tokens":43,"total_tokens":81,...}` (original OD fields, not rewritten).
- The stream ends with `data: [DONE]\n\n` exactly once.

## 4. The `responses_emit_event_lines` Switch

Second instance (temporary config, `127.0.0.1:3461`, `responses_emit_event_lines=false`):

```console
status=200 event_lines=0 data_lines=18 done=1
data: {"type":"response.created",...}     # first line is data; no event line
content-type: text/event-stream
```

Both the default (true) and disabled (false) modes match the spec: when disabled the output is fully verbatim and no bytes are inserted.

## 5. Unsupported Parameters / Endpoints

```console
$ curl ... -d '{"model":"od/deepseek-v4.1-flash","input":"hi","previous_response_id":"resp_fake"}'
400  x-umsp-code: unsupported_parameter
{"error":{"message":"previous_response_id is not supported by this gateway; send the full input history",
          "type":"invalid_request_error","code":"unsupported_parameter","param":null}}

POST /v1/responses/input_tokens     -> 501 x-umsp-code: unsupported_endpoint
GET  /v1/responses/resp_1           -> 501 x-umsp-code: unsupported_endpoint
POST /v1/responses/resp_1/cancel    -> 501 x-umsp-code: unsupported_endpoint
```

Any present non-`null` value yields 400; an explicit `"previous_response_id": null` is treated as unset (so
serializers that always include the key are not caught out). This is documented.

## 6. Chat Regression (Same Port 3460, Same Key)

| Case | Result |
|---|---|
| Non-streaming Chat | 200, `choices[0].message.content = "chat ok"`, usage normal |
| Streaming Chat | 200, 22 `data:`, 1 `[DONE]` |
| No key | 401 |
| Unknown model | 404 `model_not_found` |
| 50 MiB + 1 request body | 413 `request_too_large` |
| `GET /v1/models` | 200 |
| `GET /healthz` | 200 |
| `HEAD /api/hello` | 204 |

501s occur only on `/v1/responses/*` subpaths; `POST /v1/responses` and Chat both work normally.

## 7. Ledger

3 rows with `protocol=responses` in `~/.local/share/umsp/logs/2026-11-19.jsonl`:

| stream | status | prompt | completion | cached | reasoning | ttft_ms | duration_ms | error_code |
|---|---|---|---|---|---|---|---|---|
| false | 200 | 39 | 37 | 0 | 30 | – | 1410 | – |
| true | 200 | 38 | 43 | 0 | 37 | **1130** | 1393 | – |
| false | 400 | 0 | 0 | 0 | 0 | – | 0 | `unsupported_parameter` |

`umsp usage summary --since 1h --protocol responses --json` returns `requests=3`, matching the row count;
`by_protocol={"responses":3}`, `by_model={"od/deepseek-v4.1-flash":3}`.
Grepping the ledger for the client key plaintext: 0 hits; the runtime log (stderr JSON) also contains no key.

Note: the 400 row is rejected before routing, so `provider` is empty (consistent with the 413/invalid_json rows in M1).

## 8. OpenAI Python SDK

The environment ships `openai 2.54.0` (no temporary venv needed):

```console
non-stream id: resp_03AaHzdAohwKhiegAAAACzBqPNAgHY_f77s66YLgCMatdXXAgDjYs
non-stream status: completed
non-stream output_text: 'SDK non-streaming OK'
non-stream usage: ResponseUsage(input_tokens=40, output_tokens=45, ...)
stream event count: 29   stream unique types: 13
stream completed usage: ResponseUsage(input_tokens=39, output_tokens=40, ...)
stream output_text: 'SDK streaming OK'
SDK OK
```

`client.responses.create(..., stream=True)` dispatches all events by `data.type` (the backfilled
`event:` lines do not interfere with parsing); non-streaming returns a complete response object.

Also: with `max_output_tokens=48`, OD returns `response.incomplete` because reasoning consumes the whole
budget; the gateway relays it verbatim (it does not fabricate `response.completed`), and the SDK parses it fine.

## 9. Resources and Wrap-Up

| Metric | Measured |
|---|---|
| Release binary (musl, strip + LTO) | **6,391,184 B ≈ 6.10 MiB** (M1: 6,366,608 B) |
| Idle RSS (after ~12 requests) | **6,012 kB ≈ 5.87 MiB** (VmHWM 6,016 kB, 4 threads) |
| Second instance RSS | 5,896 kB (3 threads) |
| Ports | 3460 / 3461 both released after verification |

## 10. Implementation Trade-offs (for Review)

- Shared `prepare_request` (auth / 50 MiB / JSON / model / allowlist /
  `allowed_protocols`) and shared `proxy_stream` + `stream::Observer`; Chat and Responses
  use the same code path; Chat's byte-for-byte forwarding, keepalive, disconnect, and usage/TTFT ledger behavior are unchanged.
- The non-streaming Responses body is left completely untouched (only usage is parsed); Chat still
  re-serializes via the M0 quirk path (unchanged).
- Error events synthesized by the gateway are always `event: error` + `data {...}` (even with
  `responses_emit_event_lines=false`): this is a gateway-generated protocol-native error, not upstream bytes.
- 501s for `POST /v1/responses/*` are not written to the ledger (they are not proxy requests and make no upstream call);
  each `POST /v1/responses` request produces exactly one row (including 400 rejections).
- The `default` client's `allowed_protocols` in `config.toml` was expanded from `["chat"]` to
  `["chat", "responses"]` so the same key works for both Chat and Responses.
