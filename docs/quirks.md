# OpenDesign / DeepSeek quirks (measured behavior)

This file records the behavior of OpenDesign (provider id `open-design`, called `od` before 2026-11-20) and official DeepSeek (provider id `deepseek`) measured on 2026-11-19, along with the gateway's handling rules. M1 covers the Chat non-streaming + streaming (byte-for-byte SSE passthrough) part.

## Chat Completions (OD)

| Symptom | Trigger | Gateway handling | Status |
|---|---|---|---|
| choice missing `finish_reason` | output truncated | `completion_tokens >= max_tokens` → fill in `length`, otherwise `stop` | Implemented in M0 |
| `choices: []` | thinking consumed the entire budget (completion_tokens > 0 in usage) | synthesize `{index:0,message:{role:"assistant",content:""},finish_reason:"length"}` | Implemented in M0 |
| `choices: []` | both `max_tokens` and `max_completion_tokens` given | strip `max_completion_tokens` on the request side, keep only `max_tokens` | Implemented in M0 |
| `message.tool_calls[].index` | non-streaming response | strip `index` (non-standard field) | Implemented in M0 |
| `message.reasoning_details` | non-streaming response, contains `reasoning.summary` + `reasoning.encrypted` | preserved as-is, not parsed and not rewritten | Implemented in M0 |
| many empty `delta: {}`, empty `id` | streaming | passed through as-is (no re-serialization) | Implemented in M1 |
| `delta.reasoning` | streaming | passed through as-is (not mirrored, not rewritten) | Implemented in M1 |
| non-boolean `stream` (`"yes"`/`1`/`null`) | request | normalized to `false` before forwarding (OD originally returned 400 `invalid_request_error` outright) | Implemented in M1 |
| upstream stream ends without sending `[DONE]` | streaming | append one `data: [DONE]`; not repeated if already sent | Implemented in M1 |
| client sets `stream_options.include_usage: false` | streaming | not injected; OD may still return usage on its own, ledger it if present, record `usage_missing` if not | Implemented in M1 |

## Chat Completions (official DeepSeek)

| Symptom | Trigger | Gateway handling | Status |
|---|---|---|---|
| assistant history missing `reasoning_content` when `tools` is present → upstream 400 | replay across providers (OD responses only contain `reasoning`/`reasoning_details`) | fill a non-empty `reasoning_content` on every assistant message: use the `reasoning` text first, and if there is still none add a single space | Implemented in M4a |
| `reasoning_details` (including encrypted) is an OD extension | replaying OD history to DS | drop the field, keep only the plain text; unknown fields are left untouched | Implemented in M4a |
| `user_id` used for KVCache isolation | any Chat request | respect an explicit client value; otherwise inject `umsp-<first 16 hex of sha256(client name)>` (stable across restarts) | Implemented in M4a |
| `thinking` / `reasoning_effort` | any Chat request | passed through as-is, no default values added (DS thinking mode is on by default) | Implemented in M4a |
| balance exhausted (402) / auth failure (401) | upstream | 402 trips the circuit immediately for 10 minutes (DS) / 5 minutes (OD) and tries the next backend; 401 trips it for 5 minutes. Both map externally to 502 (`upstream_quota_exhausted` / `upstream_auth_failed`); once the circuit is open, 503 `temporarily_unavailable` | Implemented in M4b |

## General request-side rules

- Only `model` (canonical/alias → upstream id) and quirk fields are changed; all other bytes are preserved (`system` arrays, `cache_control`, tool schemas, and unknown fields are left untouched).
- Do not invent `max_tokens` / `temperature` / `top_p`.
- Request body limit 50 MiB → 413; `Content-Encoding: gzip` is not handled (M0 does not implement decompression).
- Client keys are used only for auth and attribution and are not forwarded to the upstream; upstream keys are injected by the gateway.

## Error handling

- Client-caused 4xx keep their status and (identifier-scrubbed) body; 401/402/403/429/5xx are mapped
  to neutral gateway errors and never relayed verbatim. See `docs/model-routing.md`.
- Gateway's own errors: OpenAI-native `{"error":{...}}` + `X-UMSP-Code`.
- Gateway-to-upstream connection/timeout: 504 `upstream_timeout` / 502 `upstream_unreachable`.

## Other measured facts

- OD `GET /v1/models` takes about 0.31s; `HEAD /api/hello` returns 404 (so the gateway returns 204 itself).
- OD `/v1/messages` returns 400 when `anthropic-version` is missing (handled in M3).
- OD Responses streams contain only `data:` lines and no `event:` lines (handled in M2).
- OD `count_tokens` does not exist (501 since M3, not forwarded); DS's Anthropic-compatible entry implements it (proxied in M4a, returns `{"input_tokens":N}`).
- DeepSeek's `previous_response_id` is silently ignored (M2 returns a uniform 400).
- DeepSeek Responses streams **do** carry `event:` lines (measured 38 lines / 11 event types); the gateway does not insert duplicates; usage lives at `response.completed.response.usage`.
- DeepSeek Messages usage uses the flat `cache_creation_input_tokens` (OD uses nested `cache_creation.ephemeral_*`); both must be recognized.
- With `tools`, DeepSeek Chat returns 400 when an assistant history message lacks `reasoning_content`; after filling it in, the same request measured 200 (`prompt_tokens=370`). `user_id` is accepted by DS (KVCache isolation).
