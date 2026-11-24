# Protocol behavior and upstream quirks

UMSP forwards a request natively when the selected backend speaks the client protocol; otherwise it
translates. This document collects the per-upstream behavior. See
[protocol-matrix.md](protocol-matrix.md) for the field-by-field translation table and
[quirks.md](quirks.md) for the quirk catalog.

## OpenDesign quirks (implemented in M0)

| Symptom | Handling |
|---|---|
| The choice has no `finish_reason` when truncated | `completion_tokens >= max_tokens` → `length`, otherwise `stop` |
| Thinking consumes the whole budget → `choices: []` | Synthesize `{index:0,message:{role:"assistant",content:""},finish_reason:"length"}` |
| Both `max_tokens` + `max_completion_tokens` given | Remove the latter, keep only `max_tokens` |
| Non-streaming `message.tool_calls[].index` | Strip it |
| `message.reasoning_details` | Keep as-is |
| Unknown fields | Neither request nor response is rewritten (only `model` and the quirk fields above) |

## DeepSeek official provider (M4a)

`provider = "deepseek"`, `quirks = "deepseek"`: all three entry points share the same model ID
(`deepseek-v4.1-flash`, alias `deepseek-flash`). DeepSeek lists all three protocols, so
these routes stay same-protocol passthrough.

| Entry point | DS upstream path |
|---|---|
| `/v1/chat/completions` | `https://api.deepseek.com/chat/completions` |
| `/v1/responses` | `https://api.deepseek.com/responses` |
| `/v1/messages` | `https://api.deepseek.com/anthropic/v1/messages` |
| `/v1/messages/count_tokens` | `https://api.deepseek.com/anthropic/v1/messages/count_tokens` |

- **Chat request quirks** (required for cross-provider replay):
  - When the request has `tools`, every historical assistant message must have a non-empty
    `reasoning_content`; if missing, fill it first with the `reasoning` text, and if still missing,
    fill a single space (otherwise DS returns 400 directly). The ledger records
    `filled_reasoning_content` / `filled_reasoning_placeholder` under `quirks`.
  - Remove OD-specific `reasoning_details` (including encrypted), keeping only plain text
    (`dropped_reasoning_details`); unknown fields are untouched.
  - `user_id` is used for KVCache isolation: respect it if the client explicitly provides it;
    otherwise inject `umsp-<first 16 hex of sha256(client name)>` (stable across restarts,
    `injected_user_id`).
  - `thinking` / `reasoning_effort` pass through as-is, **no defaults added** (DS thinking mode is
    on by default).
- **Responses**: DS streams already carry `event:` lines, so `responses_emit_event_lines` does not
  insert duplicates; `reasoning_text.*` / `function_call` events pass through verbatim; usage is
  taken from `response.completed.response.usage`; `previous_response_id` still returns 400.
- **Messages**: goes through `anthropic_base_url` (`/anthropic/v1/messages`), with query such as
  `?beta=true` preserved as-is; `anthropic-version` defaults to `2023-06-01` if missing; `thinking` /
  `tool_use` / `signature_delta` pass through verbatim; `count_tokens` is genuinely proxied (measured
  200 `{"input_tokens":N}`, usable even with a negative balance).
- **usage normalization**: both cache creation shapes are recognized for Messages (OD nested
  `cache_creation.ephemeral_*`, DS flat `cache_creation_input_tokens`).

## Backend failover and circuit breaking

- **backends**: `[[models]] backends = [{ provider, upstream }, ...]`. A request considers only
  those backends. A provider is skipped when its circuit is open or a fresh usage window is
  exhausted. Providers with fresh usage (U) are tried before providers with neither usage nor a
  usable balance (X), which are tried before balance-only providers (B). U is ranked by the
  tightest remaining fraction; only fractions are compared, never request/token/currency
  absolutes. The provider that serves a model is remembered for five hours and is reused until
  that record expires or the provider cannot take a request. Failover happens only **before any
  response bytes have been sent to the client** (once SSE has returned 200, it no longer switches),
  and each backend is tried at most once.
- **Retryable**: connection errors, timeouts, 429, 5xx, 402; **not retried**: 400/401/403/404/422
  (401 is an upstream key misconfiguration; switching backends would only burn through every vendor).
  When all backends fail, the client gets a neutral gateway error (`upstream_error`, 502); the raw
  upstream status and body stay in the ledger (`upstream_status` / `upstream_body`). `count_tokens`
  only selects backends that implement it.
- **Circuit breaker**: one in-memory state machine per provider. 5 **retryable** failures within 30
  seconds → `open` for 30 seconds → `half_open` admits 1 probe; success returns to `closed`, probe
  failure returns to `open`. DS `402` → `open` for 10 minutes, with a background check of
  `GET /user/balance` every 5 minutes, automatically returning to `closed` when quota recovers; OD
  `401/402` → `open` for 5 minutes, recovering via a background reasoning probe with `max_tokens=1` +
  `reasoning_effort=none`. `400/404/422` do not count as failures. A stream that already returned
  200 is judged when it ends: an upstream break or idle timeout after the headers counts as a
  retryable failure, a clean end counts as the success (a client disconnect counts as neither).
- **Half-open probe slot**: the probe slot is held by an RAII guard — if the request is cancelled
  (client disconnects) or the probe gets a non-retryable 4xx, it **only releases the slot and does
  not count a failure**, so the next request can continue probing; a 4xx on the probe means the
  provider is reachable, so it goes straight back to `closed`. A 15-minute safety net reclaims leaked
  slots. Concurrent requests are still rejected while the probe is in flight.
- When a backend's circuit is open, requests skip it and continue trying later closed backends; when
  all are unavailable, returns a protocol-native `503` + `X-UMSP-Code: temporarily_unavailable`,
  **without touching upstreams**.
- `/healthz` includes `circuit` (`closed|open|half_open`), `failures`, `opened_at`, `reason` for each
  provider; `?probe=1` checks the DS balance first (`is_available=false` goes straight to degraded,
  no tokens spent), while other providers use a minimal reasoning probe (cached 30s).
- Ledger: `model` is the **logical model id**; `provider` / `upstream_model` record the backend that
  **actually responded**; `fallbacks` is the number of failovers (0 = the first backend succeeded on
  the first try), and `quirks` keeps `fallback_from:<primary provider>`. Log events: `fallback`,
  `circuit_open`, `circuit_close` (with provider/model/reason, no keys).

Limitations (intentionally not done):

- **No budget / RPM / 429 budget rejection**, no multi-replica / Redis. Cross-protocol
  translation runs only when the chosen provider does not speak the client's protocol.
- For Chat / Responses streaming, the response-header timeout uses the global
  `stream_header_timeout_s` (default 300s); DS documentation recommends 600s (disconnect only if
  reasoning has not started for 10 minutes), so long-thinking tasks can raise it.

## Fully transparent to clients (provider invisible)

Callers see only UMSP as a single provider: failover, circuit breaking, and upstream identity stay
inside the gateway.

- Response headers retain only `x-umsp-request-id` (plus presentation-layer `content-type` / SSE
  headers); upstream `server`, `x-request-id`, `x-ds-trace-id`, `cf-ray`, `x-should-retry`,
  `anthropic-ratelimit-*`, and cookies are never forwarded.
- Upstream 2xx bodies and streaming bytes are returned **verbatim** — only identity information is
  removed, content is untouched.
- Upstream failure mapping (the client sees neither which upstream nor the upstream's raw text):

| Upstream situation | Client status | `X-UMSP-Code` |
|---|---|---|
| 400/404/409/413/415/422 (the request itself is wrong) | Same status code | None (upstream shape kept, body redacted) |
| 401/403 (gateway-to-upstream credential problem) | 502 | `upstream_auth_failed` |
| 402 (quota exhausted) | 502 | `upstream_quota_exhausted` |
| 429 | 429 | `rate_limited` |
| 5xx | 502 | `upstream_error` |
| Connection failure / timeout | 502 / 504 | `upstream_unreachable` / `upstream_timeout` |
| All backends circuit-open | 503 | `temporarily_unavailable` |

- The upstream's raw status and body (truncated to 2 KB) go to the ledger: `upstream_status`,
  `upstream_body`; use `x-umsp-request-id` from the response header to reconcile against
  `~/.local/share/umsp/logs/`.
- For 4xx bodies passed through, the provider id and upstream model name are replaced with the
  logical model id (`umsp` / `<model-id>`), leaving the rest of the text intact so callers can fix
  their request.
- Caveats: `/healthz` and `/metrics` are operational endpoints (same loopback port, not
  authenticated by client key) and can reveal provider names; after a streaming response has
  returned 200, error text embedded in upstream event bodies passes through as-is per protocol.

## Streaming (SSE, M1)

When `"stream": true`, the gateway no longer buffers or re-serializes:

- **Byte-for-byte pass-through** of the upstream body: SSE comments, empty `delta:{}`,
  `delta.reasoning`, `id:""`, tool deltas, and `data: [DONE]` all reach the client as-is; if the
  upstream did not send `[DONE]`, one is appended.
- Response headers: `Content-Type: text/event-stream`, `Cache-Control: no-cache`,
  `X-Accel-Buffering: no`, no `Content-Length`, plus `x-umsp-request-id`; **no upstream headers are
  forwarded**.
- If the client did not provide `stream_options.include_usage`, the gateway injects
  `{"include_usage": true}` for accounting (ledger `quirks` records `injected_include_usage`); an
  explicit `false` is respected, the ledger records `usage_missing`, and tokens are not estimated.
- After 15s of upstream silence (`stream_keepalive_s`), sends a `: keep-alive` comment.
- Upstream response header timeout 300s (`stream_header_timeout_s`), stream idle timeout 300s
  (`stream_idle_timeout_s`); streaming clients have **no** overall timeout, so long streams are not
  cut off at 600s.
- If the upstream disconnects/errors after 200 has been sent: send a protocol-native
  `data: {"error":{"message":…,"type":"upstream_error","code":"upstream_stream_error"}}`, then
  `data: [DONE]`; do not fabricate `finish_reason` or usage, and the ledger records
  `error_code=upstream_stream_error`.
- If the upstream fails before response headers: use the unified failure mapping (see "Fully
  transparent to clients"), **not converted to SSE**.
- Client disconnect: stop forwarding immediately and cancel the upstream request; ledger
  `client_disconnected=true` (if it disconnects before even receiving upstream response headers,
  record a `status=499` `client_disconnected` line so the request is not lost).

Limitations (M1 / Chat):

- Non-boolean `stream` (`"yes"`, `1`, `null`) is treated as `false` and normalized to `false` before
  forwarding (ledger `quirks` records `stream_non_boolean`): strict upstreams reject non-boolean
  values with 400.
- Request body compression is not supported (`Content-Encoding: gzip` is not handled);
- Streaming responses are not gzip-compressed.

## OpenAI Responses (M2)

`POST /v1/responses` serves OpenDesign and DeepSeek (M2 / M4a). The entire response body passes
through byte-for-byte by default.

- **Request**: only `model` is rewritten (→ `model.upstream`). `input` (string / message array /
  `function_call_output`), `instructions`, `tools`, `tool_choice`, `parallel_tool_calls`,
  `max_output_tokens`, `reasoning`, `text`, `store`, `metadata`, `include`, `truncation`, `stream`,
  and unknown fields all pass through unchanged; no defaults are added; the request body limit is
  still 50 MiB (413 beyond that).
- `previous_response_id` always returns `400 unsupported_parameter` (OD errors on unknown ids, DS
  silently ignores them causing context corruption): clients must send the full input history each
  time. An explicit `null` is treated as unset (not rejected), to avoid breaking clients that always
  serialize the key.
- **Non-streaming**: 2xx status code and body returned as-is; upstream failures use the unified
  mapping (see "Fully transparent to clients"); usage is read from top-level `usage.input_tokens` /
  `output_tokens` / `input_tokens_details.cached_tokens` /
  `output_tokens_details.reasoning_tokens` and written only to the ledger, without rewriting the
  body; missing usage records `usage_missing`, no estimation.
- **Streaming**: upstream `data:` lines pass through byte-for-byte. OD sends only `data:`, not
  `event:`; the gateway by default adds an `event: <type>` line based on the `type` in the data JSON
  (not duplicated if the same event block already has one; `[DONE]`, comments, blank lines, and
  order are untouched). With `responses_emit_event_lines = false` it is completely as-is.
- 15s `: keep-alive`; client disconnect cancels the upstream immediately and records
  `client_disconnected` (also writes a 499 line if disconnecting before response headers); upstream
  disconnect after 200 → send protocol-native `event: error` +
  `data: {"type":"error","code":"upstream_stream_error","message":…,"param":null,"sequence_number":…}`
  (the flat shape the Responses protocol defines; `sequence_number` continues the upstream numbering
  when it has one), then `data: [DONE]`, without fabricating `response.completed`;
  non-2xx before upstream response headers uses the unified failure mapping, not converted to SSE.
- Does not proxy `POST /v1/responses/input_tokens`, `GET /v1/responses/{id}`, cancel, `input_items`
  (all `501 unsupported_endpoint`); does not implement WebSocket Responses.

Codex integration (`~/.codex/config.toml`):

```toml
model_provider = "umsp"
model = "deepseek-v4.1-flash"

[model_providers.umsp]
name = "AI Gateway"
base_url = "http://127.0.0.1:3460/v1"
wire_api = "responses"
env_key = "UMSP_API_KEY"
supports_websockets = false
```

`base_url` must include `/v1` (Codex appends `/responses` itself); `supports_websockets = false`
avoids Codex retrying WebSocket 5 times before falling back to HTTPS.

## Anthropic Messages (M3)

`POST /v1/messages` serves OpenDesign and DeepSeek (M3 / M4a). Intended for Anthropic clients such
as Claude Code. When the provider speaks Messages, the body is forwarded as-is; otherwise the
gateway translates that attempt from a protocol the provider does speak.

- **Entry points**: `POST /v1/messages`, query string passed to the upstream as-is (Claude Code sends
  `?beta=true`). `POST /v1/messages/count_tokens`: when routed to DS, **genuinely proxies** to
  `{anthropic_base_url}/v1/messages/count_tokens` (200 `{"input_tokens":N}`); when routed to OD
  (upstream 404), the gateway returns Anthropic-native
  `501 {"type":"error","error":{"type":"not_supported_error","message":"count_tokens is not supported by any backend of model 'claude-haiku-5.5'"}}`,
  without forwarding upstream. Claude Code falls back to character estimation and does not break.
- **Request**: only `model` is rewritten (→ `model.upstream`). `max_tokens`, `messages`, `system`
  (string or array), `tools`, `tool_choice`, `thinking`, `metadata`, `stop_sequences`, `stream`,
  `temperature`, `top_p`, `top_k`, `output_config`, `context_management`, `cache_control`, and
  unknown beta fields all pass through unchanged, without merging/reordering/stringifying, and no
  defaults are added.
- **Request headers**: `anthropic-version`, `anthropic-beta`, other `anthropic-*`,
  `x-claude-code-*`, `x-stainless-*` are forwarded as-is (**no beta-value allowlist**); when the
  client does not provide `anthropic-version`, `2023-06-01` is added (required by OD). Client
  `Authorization` / `x-api-key` are not forwarded; instead, the gateway injects the provider key as
  `Authorization: Bearer <provider-key>``.
- **Non-streaming**: 2xx status and body returned as-is (`thinking` / `redacted_thinking` / `text` /
  `tool_use` unchanged); usage reads `input_tokens` / `output_tokens` / `cache_read_input_tokens`;
  cache creation supports both OD nested (`cache_creation.ephemeral_5m_input_tokens` / `1h`) and DS
  flat (`cache_creation_input_tokens`), written to the ledger only, body unchanged; no usage records
  `usage_missing`.
- **Streaming**: event sequence passes through **byte-for-byte** (`message_start`, `content_block_*`,
  `thinking_delta` / `signature_delta` / `input_json_delta` / `text_delta`, `message_delta`,
  `message_stop`, `ping`, `error`), and **does not** append `[DONE]`. After 15s of upstream silence
  the gateway sends `event: ping` + `data: {"type":"ping"}` (not a comment). If the upstream
  disconnects after 200 or closes without sending `message_stop` → send `event: error` +
  `data: {"type":"error","error":{...}}`, **never fabricate `message_stop`**.
- **Errors**: errors generated by the gateway use the Anthropic shape
  `{"type":"error","error":{"type":...,"message":...}}` (401/403/404/413/501, etc.); upstream
  failures use the unified mapping (see "Fully transparent to clients"); if `retry-after` is an HTTP
  date it is converted to integer seconds; `x-should-retry` / `anthropic-ratelimit-unified-*` pass
  through as-is.
- **Model discovery**: `HEAD /api/hello` 204, `GET /v1/models` local registry (millisecond latency).
  Claude Code only keeps model ids containing `claude` / `anthropic`, so after registering OD's real
  model (e.g. `claude-haiku-5.5`) in `config.toml`, enabling
  `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1` makes it discoverable.

Claude Code integration (run on the user's machine; the gateway listens on loopback only, so use SSH
port forwarding or run it locally):

```bash
# When running the gateway locally
export ANTHROPIC_BASE_URL=http://127.0.0.1:3460
export ANTHROPIC_AUTH_TOKEN=umsp_xxx        # or ANTHROPIC_API_KEY
export ANTHROPIC_MODEL=claude-haiku-5.5
claude -p "Run date with Bash and tell me the result"
```

When running the gateway on a remote machine, first `ssh -N -L 3460:127.0.0.1:3460 <server>`, then set
`ANTHROPIC_BASE_URL=http://127.0.0.1:3460`.

Note: OD only accepts `anthropic-version: 2023-06-01`; the gateway forwards the client's value as-is
per spec (only filling in the default when absent). Newer Anthropic SDKs (2.10.0 defaults to
`2026-10-21`, measured) are rejected by OD with `400 unsupported anthropic-version`; in that case pin
the client version to `2023-06-01` (SDKs can use
`default_headers={"anthropic-version": "2023-06-01"}`).
