# Protocol matrix

All three entry points share the same model registry and the same set of client keys. A registered model can be called on any of the three. When the provider lists the client's protocol, that attempt is forwarded unchanged. When it does not, the attempt is translated into the first protocol in the provider's own list and the response is translated back. Backend order stays the priority. `count_tokens` is not translated.

| Dimension | Chat Completions | Responses | Anthropic Messages |
|---|---|---|---|
| Gateway entry | `POST /v1/chat/completions` | `POST /v1/responses` | `POST /v1/messages` (query preserved, e.g. `?beta=true`) |
| OD upstream path | `{od}/chat/completions` | `{od}/responses` | `{od}/messages` |
| DS upstream path | `{ds}/chat/completions` | `{ds}/responses` | `{ds_anthropic}/v1/messages` |
| Stream format | `data:` + `[DONE]` | `data:` (adds `event: <type>` by default) + `[DONE]` | `event:`/`data:`, ends with `message_stop` |
| Tool calls | `tool_calls` delta | `function_call` items + arguments delta | `tool_use` blocks + `input_json_delta` |
| reasoning | `reasoning` / `reasoning_content` | `reasoning` summary events | `thinking` / `redacted_thinking` blocks |
| usage | `prompt/completion/total` | `input/output/total` | `input/output/cache_*` |
| Error shape | OpenAI `error` | OpenAI `error` | `{"type":"error","error":{...}}` |
| Upstream errors | passed through as-is | passed through as-is | passed through as-is |
| M2 status | non-streaming ✅ / streaming ✅ (byte-for-byte SSE passthrough) | non-streaming ✅ / streaming ✅ (event lines added, can be disabled) | — |
| M3 status | same as above (unchanged) | same as above (unchanged) | non-streaming ✅ / streaming ✅ (OD) |
| M4a status | OD + **DS ✅** (DS request quirks: `reasoning_content` filled in, `reasoning_details` dropped, `user_id` injected) | OD + **DS ✅** (DS provides its own `event:` lines, no duplicates inserted; usage taken from `response.completed.response.usage`) | OD + **DS ✅** (`{ds_anthropic}/v1/messages`; `count_tokens` DS 200 / OD 501) |

## Translation fidelity

A translated request keeps the semantic core (text, images, tool calls, usage) and records every
lossy decision in the request's ledger `quirks` list.

- Sampling: `temperature` and `top_p` are clamped to `0..1` for Messages targets
  (`clamped_temperature`, `clamped_top_p`).
- Output budget: a Messages target without `max_tokens` uses the logical model's
  `max_completion_tokens` (`defaulted_max_tokens`); the 8192 fallback only applies when the model
  limit is unknown.
- Tools: only client function tools are translated. Chat built-ins, Anthropic server tools and
  Responses custom tools are dropped (`dropped_server_tool:<name>`); a named `tool_choice` that
  points at a dropped tool is dropped too.
- Structured output: Chat `response_format` and Responses `text.format` map to each other for
  `json_object` and `json_schema`; Messages has no equivalent and records
  `dropped_response_format`.
- Content: assistant images are dropped (`dropped_assistant_image`); a Messages `tool_result` with
  `is_error` becomes an `error:` prefix and omitted images are announced in the text.
- Finish reasons: `content_filter` / `refusal` survive translation; Responses returns
  `status: incomplete` with `incomplete_details.reason`. A Responses body with `status: failed` or
  `cancelled` becomes an upstream error instead of a truncated success.
- Concurrency: `n` and `parallel_tool_calls` pass through to Chat targets; Responses accepts
  `parallel_tool_calls` but records `dropped_n`; Messages records `dropped_n` and
  `dropped_parallel_tool_calls`.


## DeepSeek (M4a) highlights

- Paths: Chat `{ds}/chat/completions`, Responses `{ds}/responses`,
  Messages `{ds_anthropic}/v1/messages`, count_tokens
  `{ds_anthropic}/v1/messages/count_tokens` (`{ds}=https://api.deepseek.com`,
  `{ds_anthropic}=https://api.deepseek.com/anthropic`).
- Chat: when the request carries `tools`, assistant history messages must have a non-empty
  `reasoning_content` (if missing, fill it from the `reasoning` text, or with a space if that
  is also absent); `reasoning_details` (an OD extension) is dropped;
  `user_id` prefers an explicit client value, otherwise injects `umsp-<sha256(client name)[:16]>`;
  `thinking` / `reasoning_effort` get no default values.
- Responses: DS streams carry their own `event:` lines → the gateway does not insert them
  (no duplication when the same event block already has one), `reasoning_text.*` /
  `function_call` events pass through as-is; `previous_response_id` still returns 400.
- Messages: `anthropic-version` defaults to `2023-06-01` when missing, `?beta=true` passes
  through as-is; `count_tokens` is truly proxied (it works even with a negative balance),
  while OD still returns 501 and is not forwarded.
- usage: cache creation in Messages supports both DS's flat `cache_creation_input_tokens`
  and OD's nested `cache_creation.ephemeral_*`.
- Translation is per attempt and only on a protocol mismatch; see `docs/internal/m4b-verification.md` for fallback and
  circuit breaking: switch in backend list order (each backend at most once), and return
  503 `temporarily_unavailable` when the circuit is open.

## Messages (M3) highlights

- Only `model` is rewritten; `system` arrays/strings, `cache_control`, `tools`/`tool_choice`,
  `thinking`, `output_config`, `context_management`, and unknown beta fields all pass through
  as-is, with no default values added.
- Queries such as `?beta=true` are passed to the upstream as-is; `anthropic-version` is filled
  with `2023-06-01` when missing; `anthropic-beta` / `x-claude-code-*` / `x-stainless-*` are
  forwarded as-is (no value-based allowlist).
- Streaming events are passed through byte-for-byte; **no `[DONE]` is appended**, `message_stop`
  is supplied by the upstream and the gateway never fabricates it; an upstream stream
  break/early close → `event: error` + `data`; 15s of silence → `event: ping` + `data`.
- `POST /v1/messages/count_tokens` → `501 not_supported_error` (OD upstream returns 404, not forwarded).

## Responses (M2) highlights

- Only `model` is rewritten; `input` / `instructions` / `tools` / `reasoning` / `store` etc.
  all pass through as-is, with no default values added.
- `previous_response_id` → `400 unsupported_parameter` (both upstreams are unreliable for
  stateful context).
- Streaming: OD sends only `data:`; when `responses_emit_event_lines = true` (the default),
  `event: <type>` is added from the `type` in the data JSON, existing event lines are not
  duplicated, and `[DONE]` gets no event line; setting it to `false` leaves everything as-is.
- `POST /v1/responses/input_tokens`, `GET /v1/responses/{id}`, cancel, input_items
  → `501 unsupported_endpoint`; WebSocket Responses is not implemented (Codex is configured
  with `supports_websockets = false`).

## Request header policy

Denylist (not forwarded): `Authorization`, `x-api-key`, `Host`, `Content-Length`, `Connection`,
`Keep-Alive`, `Proxy-*`, `Transfer-Encoding`, `TE`, `Trailer`, `Upgrade`, `Cookie`.
All other headers (`anthropic-*`, `OpenAI-Beta`, `x-stainless-*`, `x-claude-code-*`, `Idempotency-Key`)
are forwarded as-is; the gateway forces `Accept-Encoding: identity` and overrides `Authorization`
with the upstream key.

## Response header policy

Only the presentation and retry-related `content-type` (when passing through a 4xx) and
`retry-after` are kept; the upstream's `server`, `x-request-id`, `x-should-retry`,
`anthropic-ratelimit-unified-*`, `openai-processing-ms`, `x-ds-trace-id`, and cookies are never
forwarded. The gateway only appends `x-umsp-request-id` (plus `x-umsp-code` for errors).
Streaming responses additionally set `Content-Type: text/event-stream`, `Cache-Control: no-cache`,
`X-Accel-Buffering: no`, and never have `Content-Length` (chunked).

## Usage normalization

Internally unified as:

```rust
struct Usage {
    input_tokens: u64,
    output_tokens: u64,
    cached_tokens: u64,
    reasoning_tokens: u64,
    cache_creation_tokens: u64,
    total_tokens: u64,
}
```

- Chat: `usage.prompt_tokens` / `completion_tokens` / `prompt_tokens_details.cached_tokens` /
  `completion_tokens_details.reasoning_tokens`.
- Responses: `usage.input_tokens` / `output_tokens` / `input_tokens_details.cached_tokens` /
  `output_tokens_details.reasoning_tokens` (M2).
- Messages: `usage.input_tokens` / `output_tokens` / `cache_read_input_tokens`;
  cache creation supports both nested (OD `cache_creation.ephemeral_*`) and flat (DS `cache_creation_input_tokens`) forms (M3).
