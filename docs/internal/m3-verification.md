# M3 verification record (2026-11-19)

Environment: Ubuntu cloud host, 2 vCPU / 2 GB, musl static binary `umsp 0.1.0`,
upstream OpenDesign `https://amr-link.open-design.ai/v1`.
M3 scope: `POST /v1/messages` (OD provider) **non-streaming + streaming**,
`POST /v1/messages/count_tokens` 501, `?beta=true` preserved, Anthropic-native error shape.

## 1. Build and tests

```console
$ ./scripts/build-musl.sh
running 77 tests ... test result: ok. 77 passed; 0 failed
running 47 tests ... test result: ok. 47 passed; 0 failed
binary:  6448528 bytes
archive: dist/umsp-0.1.0-x86_64-unknown-linux-musl.tar.gz
013400e3e3b2df508a37b3568b66c5cbb22eb892400868a41ee9bb38f4cf7a97  umsp-0.1.0-x86_64-unknown-linux-musl.tar.gz
```

M2 baseline was 66 unit + 32 integration = 98; M3 is **77 unit + 47 integration = 124** (all M1/M2 cases retained and green).

New integration cases (local fake Messages upstream, through the real axum Router):

`messages_non_stream_relays_body_verbatim_and_records_usage`,
`messages_non_stream_without_usage_records_usage_missing`,
`messages_query_and_headers_are_relayed_with_default_version`,
`messages_client_anthropic_version_is_not_overwritten`,
`messages_stream_relays_event_sequence_verbatim`,
`messages_stream_truncated_without_message_stop_emits_error_event`,
`messages_stream_keepalive_is_a_ping_event`,
`messages_stream_upstream_error_event_is_passed_through`,
`messages_stream_upstream_connection_error_emits_error_event`,
`messages_stream_client_disconnect_is_recorded`,
`messages_disconnect_before_headers_still_logs_499`,
`messages_upstream_401_is_relayed_verbatim_with_retry_after_seconds`,
`messages_count_tokens_is_501_not_supported`,
`messages_gateway_errors_use_anthropic_shape`,
`messages_upstream_400_body_is_passed_through`.

New unit tests: `MessagesPatcher` 4 (byte-for-byte + usage merge, no `message_stop` treated as incomplete,
upstream error termination, ping/error event shapes), `MessagesUsageFields` 4 (nested/flat cache creation,
missing as None, start+delta merge), `append_query` 1, `retry-after` date→seconds 2, +11 total.

## 2. Real OD: non-streaming

Request (**deliberately without `anthropic-version`**, to verify the backfill; query carries `?beta=true`):

```console
$ curl -sS -D h.txt "http://127.0.0.1:3460/v1/messages?beta=true" \
    -H "Authorization: Bearer ***" -H 'content-type: application/json' \
    -H 'anthropic-beta: interleaved-thinking-2025-05-14' \
    -H 'x-stainless-lang: js' -H 'x-claude-code-session-id: m3-verify' \
    -d '{"model":"od/claude-haiku-5.5","max_tokens":64,
         "messages":[{"role":"user","content":"Reply with exactly: M3 gateway ok"}]}'
HTTP/1.1 200 OK
content-type: application/json
x-request-id: <upstream-request-id>
x-umsp-request-id: 06GZ71TQB6T6BSH5HNCEMGAEAM
x-umsp-provider: od
x-umsp-upstream-model: claude-haiku-5.5
```

body: `type=message`, `content=[{"type":"text","text":"M3 gateway ok"}]`, `stop_reason=end_turn`,
OD's original `usage` (nested `cache_creation.ephemeral_5m/1h_input_tokens`) verbatim.

Tool call (`deepseek-v4.1-flash` + `tools`): 200, `content` types `text,tool_use`,
`stop_reason=tool_use`, `tool_use` is `Bash {"command":"date"}`, usage `input=300/output=48`.

When the client supplies a version it is **forwarded verbatim** (not overwritten):

```console
$ curl ... -H 'anthropic-version: 1999-01-01' ...
HTTP/1.1 400
{"type":"error","error":{"type":"invalid_request_error","message":"unsupported anthropic-version"}}
```

This also proves by contradiction that backfilling `2023-06-01` when the header is missing is a valid path: the same request without that header returns 200 from OD.

## 3. Real OD: streaming

```console
$ curl -sS -N -D h.txt "http://127.0.0.1:3460/v1/messages?beta=true" \
    -H "Authorization: Bearer ***" -H 'anthropic-version: 2023-06-01' \
    -H 'content-type: application/json' \
    -d '{"model":"od/deepseek-v4.1-flash","max_tokens":512,"stream":true,
         "tools":[{"name":"Bash","input_schema":{"type":"object"}}],
         "messages":[{"role":"user","content":"Use the Bash tool to run date."}]}'
HTTP/1.1 200 OK
content-type: text/event-stream
cache-control: no-cache
x-accel-buffering: no
x-umsp-provider: od
x-umsp-request-id: 06GZ71WFG1885ECRMZA3SZ8VW4
x-umsp-upstream-model: deepseek-v4.1-flash
(no content-length)
```

- `event:` 15 lines / `data:` 15 lines: `message_start` ×1, `content_block_start` ×1,
  `content_block_delta` ×10, `content_block_stop` ×1, `message_delta` ×1, `message_stop` ×1.
- **Exactly 1 `message_stop` and it is the last line**; no `[DONE]`; byte-for-byte pass-through (asserted by the fake case).
- Ledger: `prompt=44 / completion=38 / cached=256`, `ttft_ms=1297`, `bytes_out=2420`.

Long stream (24.4s, 1201 `event:` lines): **1 `event: ping` kept verbatim** (not stripped); that OD run
disconnected right after `content_block_stop` without sending `message_delta`/`message_stop`, so the gateway emitted

```
event: error
data: {"type":"error","error":{"type":"upstream_error","message":"upstream stream terminated before completion"}}
```

**No fabricated `message_stop`**; ledger `error_code=upstream_stream_error`, `ttft_ms=1196`,
`bytes_out=182267`. In that run the upstream `message_start.usage.input_tokens=0`, the gateway does not estimate, and the ledger records 0/0.

## 4. count_tokens / error shapes / model discovery

```console
$ curl -sS -D h.txt http://127.0.0.1:3460/v1/messages/count_tokens \
    -H "x-api-key: ***" -H 'content-type: application/json' \
    -d '{"model":"od/claude-haiku-5.5","messages":[{"role":"user","content":"hello"}]}'
HTTP/1.1 501 Not Implemented
x-umsp-code: not_supported_error
x-umsp-provider: od
x-umsp-upstream-model: claude-haiku-5.5
{"type":"error","error":{"type":"not_supported_error","message":"count_tokens is not supported by provider od"}}
```

Not forwarded to OD (the fake case asserts 0 upstream calls; this 501 is not written to the ledger, consistent with the M2 501 sub-endpoint convention).

| Scenario | Result |
|---|---|
| `POST /v1/messages` without a key | 401 `{"type":"error","error":{"type":"authentication_error",...}}` |
| Unknown model | 404 `not_found_error` (Anthropic shape) |
| 50 MiB + 1 request body | 413 `invalid_request_error` (Anthropic shape) |
| `GET /v1/models?limit=1000` | 200, **1.3 ms**, includes `od/deepseek-v4.1-flash`, `od/claude-haiku-5.5` |
| `HEAD /api/hello` | 204 |
| `GET /healthz` | 200 |

`od/claude-haiku-5.5` is a model actually returned by OD `/v1/models` (`owned_by=vela`, 1M context),
and is registered in the config and `CONFIG_TEMPLATE`/`config.example.toml` (not a hardcoded fake model);
Claude Code discovery only keeps `claude`/`anthropic` names, so this id is directly discoverable.

## 5. Regression (same port 3460, same key)

| Case | Result |
|---|---|
| Chat non-streaming | 200 |
| Chat streaming | 200, 1 `data: [DONE]` |
| Responses non-streaming | 200 |
| Responses streaming | 200, 24 `event:` lines |
| 401 / 404 / 413 | 401 / 404 / 413 (Chat shape unchanged; Messages uses the Anthropic shape) |

## 6. Ledger

In `~/.local/share/umsp/logs/2026-11-19.jsonl`, `protocol=messages` has **13 lines**,
and `umsp usage summary --since 2h --protocol messages --json` returns `requests=13`, matching line by line:

| status | stream | prompt | completion | cached | ttft_ms | duration_ms | error_code |
|---|---|---|---|---|---|---|---|
| 200 | false | 24 | 10 | 0 | – | 1,432 | – |
| 200 | false | 300 | 48 | 0 | – | 1,515 | – |
| 200 | true | 44 | 38 | **256** | **1,297** | 1,390 | – |
| 200 | true | 0 | 0 | 0 | 1,196 | 24,422 | `upstream_stream_error` |
| 200 | false | 25 | 11 | 0 | – | 1,460 | – |
| 200 | true | 16 | 7 | 0 | 829 | 941 | – |
| 200 | false | 25 | 11 | 0 | – | 1,276 | – |
| 200 | true | 16 | 7 | 0 | 1,138 | 1,250 | – |
| 200 | true | 38 | 54 | 256 | 1,217 | 1,444 | – |
| 400 | false | 0 | 0 | 0 | – | 310 | `upstream_400` (version forwarded verbatim) |
| 400 | false | 0 | 0 | 0 | – | 298 | `upstream_400` (SDK default version) |
| 404 | false | 0 | 0 | 0 | – | 0 | `model_not_found` |
| 413 | false | 0 | 0 | 0 | – | 0 | `request_too_large` |

`by_status={"200":9,"400":2,"404":1,"413":1}`. Grepping the ledger/runtime logs for the client key in plaintext:
**0** hits; neither `x-api-key` nor `Authorization` appears.

## 7. Anthropic Python SDK (2.10.0, temporary venv)

```
non-stream: message end_turn 'SDK non-stream ok'
stream events: 11 unique: ['content_block_delta','content_block_start','content_block_stop',
                           'message_delta','message_start','message_stop','text']
stream final: end_turn ['thinking','text'] '1\n2\n3'
tool stream events: 33 unique: [... 'input_json' ...]
tool blocks: [('redacted_thinking', None, None), ('tool_use', 'Bash', {'command': 'date'})]
tool stop: end_turn
SDK OK
```

The SDK's `messages.stream()` can fully parse `message_start`→`message_stop`, thinking blocks and
tool input accumulated from `input_json_delta`, showing that the event sequence/order/terminator conform to the Anthropic protocol.

**Note (upstream compatibility)**: SDK 2.10.0 defaults to `anthropic-version: 2026-10-21`, while OD only accepts
`2023-06-01` → after pass-through OD returns 400 `unsupported anthropic-version`. The gateway forwards the client value verbatim per spec,
so tests pin the version with `default_headers={"anthropic-version":"2023-06-01"}`.
Final Claude Code acceptance runs on the user's machine (README provides the config); if it sends a newer version, it likewise needs to be pinned to `2023-06-01`.

## 8. Resources and wrap-up

| Metric | Measured |
|---|---|
| release binary (musl, strip + LTO) | **6,448,528 B ≈ 6.15 MiB** (M2: 6,391,184 B) |
| Idle RSS | **5,932 kB ≈ 5.79 MiB** (after ~115 requests, VmHWM 5,932 kB, 3 threads); **5,900 kB** after a smoke test of the final binary (4 threads) |
| Ports | 3460 / 3461 both released after verification |
| Workspace | committed, `git status` clean |

## 9. Implementation trade-offs (for review)

- Reuse `prepare_request` (auth / 50 MiB / JSON / model / allowlist / `allowed_protocols`)
  and `proxy_non_stream` / `proxy_stream`; add `StreamProtocol::Messages` and
  `stream::MessagesPatcher` (read-only copy, forwarded byte-for-byte).
- `prepare_request` gains a `query` field; only Messages appends it to the upstream URL (Chat/Responses do not).
- **Gateway-generated errors** for `/v1/messages*` now use the Anthropic shape (the auth middleware decides by path, the rest is
  routed by `protocol_gateway_error`); Chat/Responses shapes and behavior are unchanged.
- Streaming termination semantics: Chat/Responses append `[DONE]`; Messages appends no terminator,
  and if the upstream closes early → synthesize `event: error` (do not fabricate `message_stop`). The upstream's own `error`
  event is passed through verbatim and treated as termination.
- `retry-after` HTTP date → integer seconds applies only to Messages (Chat/Responses keep the original value).
- The `count_tokens` 501 is not written to the ledger (no upstream call), consistent with the M2 `/v1/responses/*` 501 convention.
- Config: the `default` client's `allowed_protocols` in `~/.config/umsp/config.toml` is expanded to
  `["chat","responses","messages"]`, and the real model `od/claude-haiku-5.5` is added
  (template synced), satisfying "one key for all three protocols + Claude Code discovery".
