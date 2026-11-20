# M4a verification record (2026-11-19)

Environment: Ubuntu cloud host, 2 vCPU / 2 GB, musl static binary `umsp 0.1.0`.
Upstreams: OpenDesign `https://amr-link.open-design.ai/v1`, DeepSeek official
`https://api.deepseek.com` (Anthropic-compatible entry point `https://api.deepseek.com/anthropic`).
M4a scope: DeepSeek provider's **Chat / Responses / Messages non-streaming + streaming**,
`POST /v1/messages/count_tokens` proxy, DS Chat request quirks, usage normalization.
Does not include fallback / circuit breaking / balance health / budget (M4b).

## 1. Build and tests

```console
$ ./scripts/build-musl.sh
running 84 tests ... test result: ok. 84 passed; 0 failed
running 59 tests ... test result: ok. 59 passed; 0 failed
binary:  6456720 bytes
archive: dist/umsp-0.1.0-x86_64-unknown-linux-musl.tar.gz
6e32433b58185b76a5323bb482185e7a3de5bde2d8ae46d42beeef42fbc416e4  umsp-0.1.0-x86_64-unknown-linux-musl.tar.gz
```

M3 baseline was 77 unit + 47 integration = 124; M4a is **84 unit + 59 integration = 143** (all existing cases retained and green).

New integration cases (the same fake upstream serves both OD `/v1/...` and DS `/...`, `/anthropic/v1/...` paths):

- Chat: `ds_chat_backfills_reasoning_content_and_drops_reasoning_details`,
  `ds_chat_user_id_is_stable_and_an_explicit_one_wins`,
  `ds_chat_without_tools_does_not_invent_reasoning_content`,
  `ds_chat_stream_is_forwarded_and_recorded`.
- Responses: `ds_responses_non_stream_relays_body_and_records_usage`,
  `ds_responses_stream_does_not_duplicate_event_lines`,
  `ds_responses_previous_response_id_is_still_400`.
- Messages: `ds_messages_non_stream_relays_body_and_records_usage`,
  `ds_messages_stream_relays_events_and_reads_flat_usage`,
  `ds_messages_query_string_is_preserved`.
- count_tokens: `ds_count_tokens_is_proxied_to_the_anthropic_endpoint`,
  `od_count_tokens_never_reaches_the_upstream`.

New unit tests: DS Chat quirks 5 (fill `reasoning_content` text/placeholder, drop
`reasoning_details` while keeping unknown fields, no fill when there is no `tools`, do not overwrite existing values, `user_id`
stable with explicit value taking precedence), `MessagesUsageFields` flat cache creation merge 1,
`Usage::from_count_tokens` 1, +7 total.

## 2. Config and visibility

The live `~/.config/umsp/config.toml` gains the DS provider and model (no `fallbacks`):

```toml
[[providers]]
id = "deepseek"
base_url = "https://api.deepseek.com"
anthropic_base_url = "https://api.deepseek.com/anthropic"
api_key_env = "DEEPSEEK_API_KEY"
protocols = ["chat", "responses", "messages"]
quirks = "deepseek"

[[models]]
id = "deepseek/deepseek-flash"
provider = "deepseek"
upstream = "deepseek-flash"
aliases = ["deepseek-flash"]
context_length = 1048576
max_completion_tokens = 393216
capabilities = ["text", "vision", "tools", "reasoning"]
```

`umsp check --json`: `ok=true, providers=2, models=3, clients=1, errors=[], warnings=[]`.
`GET /v1/models` (the default client's `allowed_models` has been expanded to `["od/*","deepseek/*"]`):

```console
['od/deepseek-v4.1-flash', 'od/claude-haiku-5.5', 'deepseek/deepseek-flash']
```

## 3. Real DS: Chat

Non-streaming (`max_tokens=24`):

```console
$ curl ... /v1/chat/completions -d '{"model":"deepseek/deepseek-flash","max_tokens":24,
    "messages":[{"role":"user","content":"Reply with exactly: M4a chat ok"}]}'
HTTP/1.1 200 OK
x-ds-trace-id: e5d65c2f57049bc211ca7d9b40ec51f0
x-umsp-provider: deepseek
x-umsp-upstream-model: deepseek-flash
model: deepseek-flash | content: 'M4a chat ok'
reasoning_content: 'The user asks to reply with exactly "M4a chat ok". Simple.'
usage: {'prompt_tokens': 39, 'completion_tokens': 22, 'total_tokens': 61,
        'prompt_tokens_details': {'cached_tokens': 0},
        'completion_tokens_details': {'reasoning_tokens': 16}}
```

Streaming (`stream:true` + `stream_options.include_usage`, 26 `data:` lines, final line `[DONE]`):

```console
text: 'M4a stream ok'
usage: {'prompt_tokens': 39, 'completion_tokens': 24, 'total_tokens': 63,
        'completion_tokens_details': {'reasoning_tokens': 18}}
```

Cross-provider replay + tools (the assistant history has only `reasoning` and `reasoning_details`,
no `reasoning_content`): **200**, showing the gateway's backfill takes effect (DS returns 400 directly when the field is missing):

```console
HTTP/1.1 200 OK
content: 'The command `date` output: ...'
finish_reason: length   # max_tokens=32 exhausted by thinking
usage: {'prompt_tokens': 370, 'completion_tokens': 32}
```

Ledger quirks: `dropped_reasoning_details`, `filled_reasoning_content`, `injected_user_id`.

## 4. Real DS: Responses

Non-streaming (`max_output_tokens=32`): 200, `object=response`, `status=completed`,
`output` has two items `reasoning` (`reasoning_text`) + `message` (`output_text`),
usage `input=39 / output=20 / reasoning=14`.

Streaming: 200, **38 `event:` lines, 11 event types**, of which
`response.created/in_progress/output_item.added/content_part.added/
reasoning_text.delta(19)/reasoning_text.done/content_part.done/
output_item.done/output_text.delta(6)/output_text.done/response.completed`
each appears once (the `.delta` types appear multiple times), and there are **no duplicate inserted `event:` lines**;
`response.completed.response.usage`: `input=40, output=26, reasoning=19`;
final line `data: [DONE]`.

## 5. Real DS: Messages

Non-streaming (**deliberately without `anthropic-version`**, query `?beta=true`): 200; content is
`thinking` (with signature) + `text`; `stop_reason=end_turn`;
usage `input=39, output=20, cache_creation_input_tokens=0, cache_read_input_tokens=0`.

Streaming: 200, 32 `event:` lines (`message_start` 1, `content_block_start` 2,
`ping` 1, `content_block_delta` 24, `content_block_stop` 2, `message_delta` 1,
`message_stop` 1); `message_start.message.usage` provides input/cache,
`message_delta.usage` provides output; text `'M4a messages stream ok'`.

thinking + tool_use (`thinking.budget_tokens=256`): 200, content =
`thinking` (signature) + `tool_use Bash {"command":"date"}`, `stop_reason=tool_use`,
usage `input=300 / output=52`.

`count_tokens` (a required M4a item):

```console
$ curl ... /v1/messages/count_tokens \
    -d '{"model":"deepseek/deepseek-flash","messages":[{"role":"user","content":"hello world"}]}'
HTTP/1.1 200 OK
x-umsp-provider: deepseek
x-umsp-upstream-model: deepseek-flash
{"input_tokens":32}
```

One ledger line: `protocol=messages, provider=deepseek, status=200, prompt_tokens=32,
quirks=['count_tokens']`. The OD route is still 501 and does **not** call upstream (the integration case
`od_count_tokens_never_reaches_the_upstream` asserts zero upstream requests).

## 6. Ledger (real DS lines, excerpt)

```text
protocol  stream status req model                tokens      cached ttft quirks
chat      False  200    deepseek/deepseek-flash  39/22       0      -    [injected_user_id]
chat      True   200    deepseek-flash           39/24       0      133  [injected_user_id]
chat      False  200    deepseek/deepseek-flash  370/32      0      -    [dropped_reasoning_details, filled_reasoning_content, injected_user_id]
responses False  200    deepseek/deepseek-flash  39/20       0      -    -
responses True   200    deepseek-flash           40/26       0      274  -
messages  False  200    deepseek/deepseek-flash  39/20       0      -    -
messages  True   200    deepseek-flash           40/24       0      129  -
messages  False  200    deepseek/deepseek-flash  300/52      0      -    -
messages  False  200    deepseek/deepseek-flash  32/0        0      -    [count_tokens]
```

`protocol` matches the entry point; the `ttft_ms` of all three streaming lines is non-null (129–274 ms).

## 7. Error paths

```console
# Unknown model (DS namespace)
HTTP/1.1 404 Not Found
x-umsp-code: model_not_found
{"error":{"message":"unknown model 'deepseek/nope'","type":"not_found_error","code":"model_not_found","param":null}}

# 50 MiB limit (Chat and Messages each use their own shape)
HTTP/1.1 413 Payload Too Large        x-umsp-code: request_too_large
{"error":{"message":"request body 53477376 bytes exceeds the 52428800 byte limit",...}}
{"type":"error","error":{"type":"invalid_request_error","message":"request body 53477376 bytes exceeds ..."}}

# Bad DS key (second instance on 3461, env points at a fake key)
chat:      HTTP/1.1 401  {"error":{"message":"Authentication Fails, Your api key: **** is invalid (request_id: ...)","type":"authentication_error",...}}
messages:  HTTP/1.1 401  {"type":"error","error":{"type":"authentication_error","message":"Authentication Fails, ..."}}
responses: HTTP/1.1 401  {"error":{"message":"Authentication Fails, ...","type":"authentication_error",...}}
```

At that time, upstream 401s on all three protocols had their **status code and body passed through verbatim**
(DS itself masks the key as `****`), and the gateway did not wrap, retry, or switch provider. This was changed
later: upstream failures are now mapped to neutral errors, see `docs/model-routing.md`.

## 8. Logs and security

- `umsp serve` stderr is **0 lines** after a Chat stream + Messages stream + 404;
  stdout contains only `{"event":"ready",...}`.
- Scanning the live ledger, the bad-key instance ledger, and process output: the DS key (35-character plaintext) appears **0** times.
- Client keys are still stored only as `sha256` + prefix; upstream keys are read only from `~/.config/umsp/env` (0600).

## 9. Resources and wrap-up

- Idle RSS **5,928 kB ≈ 5.8 MiB** (musl release, the `umsp` process itself).
- Binary 6,456,720 B; archive sha256 in section 1.
- All test processes stopped, `3460` / `3461` both released; during this we cleaned up a `target/debug/umsp serve`
  left over from the previous review round (holding 3461, pointing at the `umsp-cache/scratch/umsp-review-m3` config).
- Workspace clean (`git status --short` produces no output).

## 10. Known trade-offs

- DS Responses `previous_response_id` uniformly returns 400 like OD (DS would silently ignore it,
  scrambling context).
- The streaming response header timeout keeps the global 300s (DS docs recommend 600s): long-thinking tasks need to raise
  `stream_header_timeout_s` themselves; M4b can make this a provider-level setting.
- No fallback / circuit breaking / balance health / budget: DS 402 (balance exhausted) is passed through verbatim, and the client handles it.
