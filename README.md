# UMSP — Universal Model Service Provider

[![CI](https://github.com/yilong-musk/umsp/actions/workflows/ci.yml/badge.svg)](https://github.com/yilong-musk/umsp/actions/workflows/ci.yml)
[![Rust](https://img.shields.io/badge/Rust-1.85+-000000?logo=rust&logoColor=white)](https://www.rust-lang.org)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

English | [Chinese (Simplified)](README.zh-CN.md)

A **single-binary, loopback-only** AI gateway: it serves three protocols — OpenAI Chat Completions /
OpenAI Responses / Anthropic Messages — on one port and forwards each as-is to an upstream that
natively supports the same protocol. No UI, no database service, no Redis, no Docker, and no
budget/rate-limiting logic.

> "Service Provider" in the name is from the client's point of view: to callers, UMSP is a service
> provider that can serve any model; from the upstream's perspective, it is still a gateway / router.

- Binary name: `umsp`
- Config: `~/.config/umsp/config.toml`
- Keys: `~/.config/umsp/env` (0600)
- State: `~/.local/share/umsp/` (JSONL request ledger)
- Listen: `127.0.0.1:3460`

> **Current version status: M5**. Implemented: config/auth, `/v1/models` (one entry per logical
> model, provider not exposed), `/healthz` (including circuit-breaker state), `/metrics`,
> non-streaming and streaming SSE pass-through for the three protocols OpenAI Chat Completions /
> OpenAI Responses / Anthropic Messages across **multiple upstreams** (OpenDesign / DeepSeek),
> `/v1/messages/count_tokens` (proxied for DS / 501 for OD), **failover across the backend list**,
> **provider circuit breaking**, **capacity-aware backend selection**, **DS balance health**, request ledger and `umsp usage`.
> Budget/RPM limits are out of scope for this milestone.
> See `docs/model-routing.md` for the model and routing design; see `docs/quirks.md` and
> `docs/protocol-matrix.md` for protocol details. `docs/internal/m0-…m5-verification.md` are historical
> acceptance snapshots for each milestone; they still contain the `od/...` model ids used at the
> time, which have since been replaced by the current "logical model + backends".

---

## Installation

### Release package (recommended; no Rust toolchain needed on the server)

```bash
tar -xzf umsp-<version>-x86_64-linux-musl.tar.gz
install -Dm755 umsp-<version>-x86_64-linux-musl/umsp ~/.local/bin/umsp
install -Dm644 umsp-<version>-x86_64-linux-musl/umsp.service ~/.config/systemd/user/umsp.service
install -Dm644 umsp-<version>-x86_64-linux-musl/config.example.toml ~/.config/umsp/config.example.toml
```

Verify:

```bash
sha256sum -c umsp-<version>-x86_64-linux-musl.tar.gz.sha256
file ~/.local/bin/umsp   # → statically linked
```

### Build from source (static musl)

```bash
git clone https://github.com/yilong-musk/umsp.git
cd umsp
sudo apt-get install -y musl-tools          # provides musl-gcc
rustup target add x86_64-unknown-linux-musl
./scripts/build-musl.sh                     # output in dist/
```

`scripts/build-musl.sh` runs `cargo fmt --check`, `clippy -D warnings`, and `cargo test` in order,
then builds the musl release, packages it, and generates the sha256.

---

## Quick start

```bash
umsp init                                   # write config.toml / env / state directory
$EDITOR ~/.config/umsp/env                  # fill in OPEN_DESIGN_API_KEY=... and DEEPSEEK_API_KEY=...
umsp client add --name default              # prints the plaintext key once, in the form umsp_xxx
umsp check --json                           # start only after everything is green
umsp serve                                  # prints one line {"event":"ready",...} to stdout
```

In another terminal:

```bash
export UMSP_API_KEY=umsp_xxx                 # the key printed above
curl -s http://127.0.0.1:3460/v1/models -H "x-api-key: $UMSP_API_KEY" | jq
curl -s http://127.0.0.1:3460/v1/chat/completions \
  -H "x-api-key: $UMSP_API_KEY" -H 'Content-Type: application/json' \
  -d '{"model":"deepseek-v4.1-flash","messages":[{"role":"user","content":"Reply with only: OK"}]}' | jq
```

You can also put the key in `Authorization: Bearer $UMSP_API_KEY`; both forms are equivalent.

Hermes integration (Chat Completions):

```bash
hermes chat -q "Reply with only: OK" \
  --provider custom --base-url http://127.0.0.1:3460/v1 \
  --api-key "$UMSP_API_KEY" --model deepseek-v4.1-flash
```

---

## Subcommands

Every subcommand supports `--json` (one line of valid JSON on stdout); `--config PATH` overrides the
config path globally.

| Command | Description |
|---|---|
| `umsp serve [--listen ADDR]` | Start the data plane; prints the ready line to stdout and flushes it; logs go to stderr |
| `umsp init [--state-dir PATH] [--force]` | Create config.toml, env (0600), and the state directory |
| `umsp config path\|show [--redact]\|validate` | Inspect/validate configuration |
| `umsp check [--json]` | Validate config + keys + state directory; exit code 10 on failure |
| `umsp doctor [--online] [--json]` | More detailed diagnostics; `--online` actually probes upstreams |
| `umsp vendor list` | List built-in vendors (id, endpoint, protocols, quirks, auth, number of configured instances) |
| `umsp provider list\|add\|remove\|test` | Manage upstream providers; `add --vendor ID` needs only the vendor id + key (or, for `grok`, no key), with endpoint/protocol from the built-in catalog, and assigns a unique id each time; `list` merges circuit-breaker/recent status from `/healthz` |
| `umsp login <provider>` | Device-code sign-in for one Grok subscription provider. Each id has its own session file |
| `umsp logout <provider>` | Delete that provider's session file |
| `umsp env list\|set\|unset` | Manage upstream keys in the env file (masked display only; `set` hides input on a TTY) |
| `umsp model list [--aliases]\|add\|remove` | Manage the local model registry (logical id + aliases) |
| `umsp model backend add\|remove` | Attach/detach a backend for an existing model (`--first` inserts at the front of the tie-break order) |
| `umsp client list\|add\|remove\|rotate` | Manage client keys (only SHA-256 stored) |
| `umsp key list\|add\|revoke` | Compatibility alias for `client` |
| `umsp usage summary\|by-client\|by-model\|by-protocol\|prune` | Aggregate the JSONL ledger |
| `umsp probe --provider ID [--model ID]` | Send one minimal request (chat) to a provider |
| `umsp call --model ID --prompt TEXT` | Call the upstream once directly (chat, non-streaming) |
| `umsp logs tail [--lines N] [--follow]` | View the request ledger |
| `umsp logs export --since 24h --out PATH` | Export the ledger |
| `umsp version [--json]` | Version + build target |
| `umsp completion bash\|zsh\|fish` | Generate a completion script |

Examples:

```bash
# Built-in vendor catalog: endpoints, protocols, and quirks are built in; you can only choose from the catalog
umsp vendor list

# Add a provider: supply only the vendor id and key (the key is read from stdin, not argv / history)
read -rsp 'DeepSeek key: ' K && echo
printf '%s\n' "$K" | umsp provider add --vendor deepseek

# The same vendor can be added repeatedly: each add gets its own id (deepseek, deepseek-2…),
# endpoints come from the catalog, and instances neither overwrite nor merge with each other
umsp provider add --vendor deepseek --no-key

# Grok subscription accounts are the same shape: one provider id, one session.
# `grok` and `grok-2` do not share a refresh token. Sign in each id on its own.
umsp provider add --vendor grok
umsp login grok
umsp provider add --vendor grok
umsp login grok-2
umsp model add --id grok-4.7 --backend grok:grok-4.7 --backend grok-2:grok-4.7

# Experimental / account risk: Grok support uses a public CLI OAuth client and the
# `cli-chat-proxy` subscription endpoint. It is not an official xAI API integration: it can
# break without notice and may violate xAI's terms of service, including account suspension.
# Use it only with an account you are willing to risk. Session tokens stay in
# sessions/<id>.json (0600) and are never written to logs or config.
# The gateway sends the required x-grok-client-version; if xAI raises the
# minimum, set UMSP_GROK_CLI_VERSION in the env file and restart.

# Attach another backend to an existing model (insert at the front of the tie-break order)
umsp model backend add claude-haiku-5.5 \
  --backend deepseek-2:deepseek-flash --first

# Keys in the env file are shown masked only
umsp env list

# One logical model, two upstream backends. Config order breaks ties; usage gates selection.
umsp model add --id deepseek-v4.1-flash \
  --backend open-design:deepseek-v4.1-flash \
  --backend deepseek:deepseek-flash \
  --alias deepseek-flash --context-length 1048000 --max-completion-tokens 128000 \
  --capability text,vision,tools,reasoning --display-name "DeepSeek V4.1 Flash"

umsp client add --name hermes --models 'deepseek-*,claude-*' --protocols chat --json
umsp usage summary --since 24h --json
umsp doctor --json
```

When the CLI modifies configuration it uses `toml_edit` to preserve comments and formatting, writes a
temporary file, then atomically replaces it with `rename`; if the process pointed to by
`state_dir/umsp.pid` is still alive, it automatically sends `SIGHUP` to trigger a hot reload (disable
with `--no-reload`).

---

## HTTP endpoints

| Method | Path | Auth | Description |
|---|---|---|---|
| GET | `/healthz` | No | Gateway and provider status, including `circuit`/`failures`/`opened_at`/`reason` and a fresh `usage` snapshot (`remaining_fraction`, `windows`, `reset_in_s`); `?probe=1` performs a real probe (cached 30s; DS checks balance first so no tokens are spent) |
| GET | `/metrics` | No | Prometheus text (`text/plain; version=0.0.4`), aggregated in memory, does not hit upstreams; loopback only (non-loopback sources get 403 `loopback_only`) |
| HEAD | `/api/hello` | No | 204, Claude Code connection warm-up |
| GET | `/v1/models` | Yes | Local registry, with flattened `context_length` / `max_completion_tokens` / `capabilities` / `display_name` |
| POST | `/v1/chat/completions` | Yes | Non-streaming forwarding + streaming SSE pass-through (`stream: true`, OD / DS) |
| POST | `/v1/responses` | Yes | OpenAI Responses non-streaming + streaming (M2 OD, M4a DS) |
| POST | `/v1/messages` | Yes | Anthropic Messages non-streaming + streaming (M3 OD, M4a DS; preserves `?beta=true`) |
| POST | `/v1/messages/count_tokens` | Yes | DS proxies to `{ds_anthropic}/v1/messages/count_tokens` (200); OD returns 501 `not_supported_error` (not forwarded) |
| any | `/v1/responses/*` | Yes | `501 unsupported_endpoint`: `input_tokens`, `GET /{id}`, cancel, and input_items are not proxied |

Authentication accepts the Bearer scheme in the `Authorization` header, or the `x-api-key` header
(both values are the same client key); client keys are stored only as `sha256:<hex>` + prefix,
comparison is constant-time, and plaintext never appears in logs.

### Prometheus `/metrics` (M5)

```bash
curl -s http://127.0.0.1:3460/metrics
```

Prometheus text format (`text/plain; version=0.0.4; charset=utf-8`), all from process memory and
reset on restart; rendering only reads memory and sends no upstream requests, with responses < 1 ms
(measured).

| Metric | Type | Labels | Semantics |
|---|---|---|---|
| `umsp_requests_total` | counter | `provider,model,protocol,status,stream` | Incremented once per **completed client request**; `model` is the logical model id and `provider` is the one that actually served it (after failover, the one that took over); maps one-to-one to ledger lines |
| `umsp_request_duration_seconds` | histogram | `provider,model,protocol` | End-to-end latency, buckets 0.05/0.1/0.25/0.5/1/2/5/10/30/60/300 |
| `umsp_ttft_seconds` | histogram | `provider,model,protocol` | Time to first byte for streaming; non-streaming has no TTFT and produces no series |
| `umsp_tokens_total` | counter | `provider,model,direction` | `direction=input\|output\|cached\|reasoning`; records only tokens the upstream **actually reported** (no series for 0) |
| `umsp_upstream_errors_total` | counter | `provider,code` | Recorded on **every failed upstream attempt** (`upstream_502`, `upstream_timeout`…), including primary-provider failures absorbed by retry/failover |
| `umsp_circuit_open` | gauge | `provider` | 1 = circuit open or half-open, 0 = closed; every provider in the config appears |
| `umsp_inflight_requests` | gauge | `provider` | Number of requests currently waiting on a provider; drops as soon as the request/stream finishes (cancellation also returns it) |

Accounting rules (aligned with `umsp usage`):

- A client request counts only once in `/metrics`: `requests_total` + one `duration` (+ `ttft` for
  streaming); the sum of `requests_total` series == number of ledger lines.
- When failover succeeds, the skipped backend's failure is not recorded as another request, only in
  `upstream_errors_total`; therefore `upstream_errors_total` can exceed `requests_total`.
- Low-cardinality labels: no `request_id`, client name, or key; `model` is the logical model id
  (without provider prefix); upstream model names appear only in the ledger and runtime logs.
- `/metrics`, like `/healthz`, does not require a client key and only accepts loopback sources; when
  the listen address is configured as non-loopback, these two endpoints also return
  `403 loopback_only`.

### Errors generated by the gateway

Protocol-native shape + `X-UMSP-Code` response header:

```json
{"error":{"message":"unknown model 'nope'","type":"not_found_error","code":"model_not_found","param":null}}
```

Upstream failures are **not passed through verbatim**: 401/402/403/429/5xx are uniformly mapped to
gateway errors (see "Fully transparent to clients" below); only 4xx caused by the client itself keep
their status code and (redacted) body.

### OpenDesign quirks (implemented in M0)

| Symptom | Handling |
|---|---|
| The choice has no `finish_reason` when truncated | `completion_tokens >= max_tokens` → `length`, otherwise `stop` |
| Thinking consumes the whole budget → `choices: []` | Synthesize `{index:0,message:{role:"assistant",content:""},finish_reason:"length"}` |
| Both `max_tokens` + `max_completion_tokens` given | Remove the latter, keep only `max_tokens` |
| Non-streaming `message.tool_calls[].index` | Strip it |
| `message.reasoning_details` | Keep as-is |
| Unknown fields | Neither request nor response is rewritten (only `model` and the quirk fields above) |

### DeepSeek official provider (M4a)

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

### Backend failover and circuit breaking

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
  `reasoning_effort=none`. `400/404/422` do not count as failures.
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

### Fully transparent to clients (provider invisible)

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

### Streaming (SSE, M1)

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

### OpenAI Responses (M2)

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
  `data: {"type":"error",...}`, then `data: [DONE]`, without fabricating `response.completed`;
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

### Anthropic Messages (M3)

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

---

## Configuration schema

`~/.config/umsp/config.toml`:

```toml
[server]
listen = "127.0.0.1:3460"          # loopback only; doctor warns on non-loopback
max_body_bytes = 52428800          # 50 MiB; 413 if exceeded
request_timeout_s = 600            # total duration for non-streaming
stream_idle_timeout_s = 300        # streaming: max gap between two upstream data chunks
stream_header_timeout_s = 300      # streaming: max wait for upstream response headers (OD)
stream_keepalive_s = 15            # streaming: silence period before sending ": keep-alive"
state_dir = "~/.local/share/umsp"   # ledger and pid file
responses_emit_event_lines = true  # whether Responses streaming adds `event: <type>` lines (default true)
log_retention_days = 30            # default retention for umsp usage prune

[[providers]]
id = "open-design"
vendor = "open-design"             # built-in vendor: endpoints and quirks come from the catalog
base_url = "https://amr-link.open-design.ai/v1"
anthropic_base_url = ""            # optional; Messages / count_tokens use it (M3 / M4a)
api_key_env = "OPEN_DESIGN_API_KEY" # variable name only; the value lives in the env file
protocols = ["chat", "responses", "messages"]
quirks = "opendesign"              # opendesign | deepseek | none

[[providers]]
id = "deepseek"
vendor = "deepseek"
base_url = "https://api.deepseek.com"
anthropic_base_url = "https://api.deepseek.com/anthropic"
api_key_env = "DEEPSEEK_API_KEY"
protocols = ["chat", "responses", "messages"]
quirks = "deepseek"                # also determines the count_tokens proxy

# The vendor catalog lives in code (src/vendors.rs): open-design, deepseek, grok.
# Adding a vendor requires changing that one place and rebuilding; an unknown vendor in the config fails validation.
# `auth = "oauth"` (grok) stores one session per provider id under sessions/<id>.json.
# `auth = "api_key"` (the default) still reads api_key_env from the env file.

[[models]]
id = "deepseek-v4.1-flash"          # logical model: callers only know this id
aliases = ["deepseek-flash"]        # compatibility with older clients
context_length = 1048000            # flattened context_length (Hermes reads this)
max_completion_tokens = 128000
capabilities = ["text", "vision", "tools", "reasoning"]
display_name = "DeepSeek V4.1 Flash"
backends = [                        # tie-break order; usage and balance gate health selection
  { provider = "open-design", upstream = "deepseek-v4.1-flash" },
  { provider = "deepseek",    upstream = "deepseek-flash" },
]
created = 1789026056

[[clients]]
name = "hermes"
key_hash = "sha256:<64 hex>"       # written by `umsp client add`
key_prefix = "umsp_ab12cd34"
allowed_models = ["deepseek-*", "claude-*"]   # glob, empty = *
allowed_protocols = ["chat", "responses"]  # chat / responses / messages
```

Conventions:

- Upstream keys live only in `~/.config/umsp/env`; the config contains only `api_key_env`; env file
  permissions must be 0600.
- Environment variables take precedence over the env file (both systemd `EnvironmentFile` and shell
  overrides apply).
- Config precedence: `--config` > `$UMSP_CONFIG` > `~/.config/umsp/config.toml`; `$UMSP_LISTEN`
  overrides the listen address, `$UMSP_STATE_DIR` overrides the state directory, `$UMSP_ENV_FILE`
  overrides the env file path.
- `/v1/models` shows exactly one entry per logical model, with no provider / upstream fields; view
  aliases with `umsp model list --aliases`.

---

## Request ledger and usage

One line of JSON per request, written to `state_dir/logs/YYYY-MM-DD.jsonl`:

```
ts, request_id, client, protocol, requested_model, provider, upstream_model, stream,
status, upstream_status, prompt_tokens, completion_tokens, cached_tokens,
reasoning_tokens, ttft_ms, duration_ms, bytes_in, bytes_out, error_code,
upstream_request_id, client_disconnected, fallbacks, quirks
```

- No prompt/body is recorded by default.
- Each line is a single `write_all` (single-line atomic); under 50-way concurrent load no lines are
  lost or interleaved (M5 regression test `concurrent_appends_never_merge_or_drop_lines`).

Accounting semantics of `fallbacks` and the provider fields:

| Field | Meaning |
|---|---|
| `provider` / `upstream_model` | The backend that **actually served** this request: after failover, the one failed over to |
| `fallbacks` | Number of failovers: `0` = the first backend succeeded on the first try, `1..n-1` = how many times it skipped and switched |
| `quirks` | On failover, appends `fallback_from:<primary provider id>`; also keeps markers such as `injected_include_usage`, `usage_missing` |
| `status` | HTTP status returned to the client (a successful failover is 200, not the primary backend's 5xx) |

`/metrics` is consistent with it: `umsp_requests_total` counts once by the final provider; the
primary provider's failure only goes into `umsp_upstream_errors_total`.

Query and cleanup:

```bash
umsp usage summary --since 24h --json                    # totals/buckets/tokens
umsp usage summary --since 7d --provider deepseek --json # only the provider that actually served
umsp usage summary --since 7d --client planner --protocol chat --json
umsp usage by-client|by-model|by-protocol --since 30d --json
umsp usage prune --days 30 --json    # keep the last 30 days; today's file is never deleted
umsp usage prune --before 30d        # equivalent form (mutually exclusive with --days)
```

- In `summary`, `requests` / `by_status` / `by_protocol` / `by_model` / `by_client` / `by_provider`
  all aggregate the same set of records, and the sum of buckets always equals `requests` (unit test
  `aggregate_buckets_and_tokens_match_the_record_count`).
- `prune` deletes whole files by date; even `--days 0` does not delete today's ledger; the deletion
  count and paths appear in `--json`'s `removed_count` / `removed`.
- Retention is controlled by `log_retention_days` (serve never deletes automatically; prune is
  invoked by a human or a scheduled job).

---

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Success |
| 2 | Usage error (including clap parse failures and duplicate client additions) |
| 3 | Network/timeout |
| 4 | Upstream banned/rate-limited/insufficient balance |
| 5 | Target does not exist (model/provider/client) |
| 6 | Circuit open (may be returned since M4) |
| 10 | Other (including `umsp check`/`doctor` validation failures) |

In `--json` mode, failures also print one line to stdout:

```json
{"ok":false,"error":{"code":"CONFIG_INVALID","message":"..."}}
```

---

## systemd (user unit)

```bash
install -Dm644 deploy/umsp.service ~/.config/systemd/user/umsp.service
systemctl --user daemon-reload
systemctl --user enable --now umsp
systemctl --user status umsp
journalctl --user -u umsp -f
```

`EnvironmentFile=%h/.config/umsp/env` injects upstream keys via systemd; `ProtectHome=read-only` +
`ReadWritePaths=%h/.local/share/umsp` ensure only the ledger directory is writable.

Hardening already baked into the unit (`deploy/umsp.service`, passes `systemd-analyze verify`):

| Directive | Value | Effect |
|---|---|---|
| `MemoryMax` | `256M` | If exceeded, the process is OOM-killed by the cgroup and restarted via `Restart=on-failure` (measured peak 14 MB, ample headroom) |
| `TasksMax` | `256` | Thread/task limit |
| `LimitNOFILE` | `8192` | Concurrent connection fd limit |
| `NoNewPrivileges` / `ProtectSystem=strict` / `ProtectHome=read-only` | — | Read-only system and home directories, with only the ledger directory writable |
| `PrivateTmp` | `true` | Private /tmp |

Prerequisites: `~/.local/bin/umsp` exists, `umsp init` has generated
`~/.config/umsp/{config.toml,env}`, and `umsp doctor` is all green. `umsp doctor` probes `/metrics`
when serve is running (skipped = not running, not counted as failure).

---

## Troubleshooting

| Symptom | Diagnosis |
|---|---|
| Exits immediately on startup | `umsp check --json` to see `errors`; `umsp doctor` for specific items |
| All requests return 401 | No client configured or wrong key: `umsp client list` (prefix only), and `umsp client rotate NAME` if needed |
| 404 `model_not_found` | `umsp model list` to confirm the canonical id; use `--aliases` to look up aliases |
| 403 `model_not_allowed` | The client's `allowed_models` glob does not match |
| Streaming hangs/is very slow | Check `ttft_ms` in the ledger; `upstream_stream_timeout` means the upstream sent no data for 300s, so adjust `stream_idle_timeout_s` |
| `upstream_stream_error` mid-stream | The upstream disconnected; the gateway already sent a protocol-native error event and `[DONE]`, and the client can retry |
| 502 `upstream_unreachable` / 504 `upstream_timeout` | `umsp probe --provider open-design`; check the network and `base_url` |
| 500 `upstream_key_missing` | The corresponding `api_key_env` is missing from `~/.config/umsp/env` (the log has the provider name) |
| 413 | Request body exceeds `server.max_body_bytes` |
| Upstream 401/5xx becomes 502 | By design: upstream failures are uniformly redacted externally; check `upstream_status` / `upstream_body` in the ledger for the raw text |
| Config changes do not take effect | Run `umsp config validate`, then `kill -HUP $(cat ~/.local/share/umsp/umsp.pid)` |
| Find a request in the logs | `umsp logs tail --lines 50` (stderr carries runtime log JSON) |

`umsp doctor --online` sends a real `max_tokens=1` request to each provider (spends a very small
number of tokens).

---

## Measurements (M0, 2026-11-19, 2 vCPU / 2 GB cloud host)

| Metric | Measured |
|---|---|
| Release binary (musl, strip + LTO) | **6,309,264 B ≈ 6.02 MiB** |
| Idle RSS | **4,228 kB ≈ 4.13 MiB** (5,796 kB after 37 requests, VmHWM 6,000 kB) |
| `GET /v1/models` latency | Average over 20 runs **0.53 ms** (min 0.41 / max 1.87 ms) |
| `GET /healthz` latency | Average over 20 runs **0.42 ms** |
| Non-streaming Chat round trip (OD, single) | **1.50–1.62 s** (almost entirely upstream time) |
| Cold start (process start → ready line) | **9 ms** (`/proc` timing; about 31 ms including shell startup) |
| Thread count | 3 |

See `docs/internal/m0-verification.md` for build/test commands and full evidence.

Known trade-off: the request body is read entirely into memory (limit 50 MiB), so handling a request
near the limit peaks at about 57 MiB RSS; M1's streaming forwarding processes chunk by chunk.

---

## Load and resources (M5, 2026-11-19)

`bash scripts/load-test.sh` (self-hosted fake upstream + isolated config/state directories,
automatically cleaned up afterwards) measured on a 2 vCPU / 2 GB cloud host:

| Scenario | Result |
|---|---|
| 50 concurrent non-streaming × 30s | **100,944 requests, 0 failures**, 3,362 rps, p95 22.3 ms, peak RSS **14.0 MB** |
| 20 concurrent streaming × 60s | **16,848 requests, 0 failures**, TTFT p50 **4.1 ms** / p95 6.1 ms, RSS 13.7 MB |
| Fixed 10,000 requests | Completed in 3.36 s (2,975 rps), RSS **13.7 MB → 13.7 MB** (no growth) |
| Cold start → `/healthz` 200 | **16–136 ms** (including polling granularity) |
| Idle RSS / binary | **7.3 MB** / **6,612,368 B ≈ 6.31 MiB** (musl release, stripped; glibc release 6,491,400 B) |
| `/metrics` and ledger consistency | Sum of `umsp_requests_total` series **127,792** == JSONL lines **127,792**; `ttft_seconds_count` 16,848 == streaming request count |

The load test also found and fixed a real defect: under concurrency, single-line ledger writes with
`writeln!` were split into two `write` calls, losing 2,790/129,279 lines (2.2%) at 50-way concurrency.
Now a single line is one `write_all` plus an in-process mutex. See
`docs/internal/m5-verification.md` for the full commands and output.

---

## Known limitations

- `/metrics` aggregates only in process memory and resets on restart; there is no remote storage and
  no multi-replica aggregation (single-instance gateway, within this milestone's scope).
- `umsp usage` scans JSONL synchronously; at the million-lines-per-day level it gets slow (measured
  about 0.4 s for 128k lines). Log retention relies on manually/scheduled
  `umsp usage prune --days N`.
- `cargo deny` has one documented exception: `RUSTSEC-2026-0543` (chrono marked unmaintained, no CVE,
  no security upgrade); replacing it with jiff/time touches timestamps in the ledger, CLI, and stream
  parsing, so it is a separate project.
- Public ingress (Caddy/TLS), multi-replica/Redis, budget/RPM, and a UI are all out of scope for this
  milestone.

---

## License

[MIT](LICENSE) © 2026 YiLongMusk
