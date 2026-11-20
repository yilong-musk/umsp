# M0 Verification Record (2026-11-19)

Environment: Ubuntu cloud host, 2 vCPU / 2 GB, musl static binary `umsp 0.1.0`,
upstream OpenDesign `https://amr-link.open-design.ai/v1`, model `od/deepseek-v4.1-flash`.

## 1. Build and Tests

```console
$ cargo fmt --all -- --check
$ cargo clippy --all-targets -- -D warnings
$ cargo test
running 45 tests ... test result: ok. 45 passed; 0 failed
running 11 tests ... test result: ok. 11 passed; 0 failed
```

`cargo test` covers: the five QUIRKS rules, key generation/hashing/constant-time comparison,
config validation, glob, usage extraction and aggregation, ledger read/write, and 11 integration
cases that go through a real axum Router
(fake upstream: normal response, `choices: []`, upstream 401 relayed verbatim, missing key, unknown model, 413, 501 streaming rejection,
`/v1/models` flat fields, healthz + HEAD /api/hello).

Release:

```console
$ ./scripts/build-musl.sh
binary:  6309264 bytes
archive: dist/umsp-0.1.0-x86_64-unknown-linux-musl.tar.gz
$ file ~/.local/bin/umsp
ELF 64-bit LSB pie executable, x86-64, statically linked, stripped
```

## 2. Configuration and Self-Check

```console
$ umsp check --json
{"ok":true,...,"providers":1,"models":1,"clients":1,"errors":[],"warnings":[]}
$ umsp doctor --json
{"ok":true,"version":"0.1.0","summary":{"ok":9,"warn":0,"fail":0}}
```

## 3. Startup

```console
$ umsp serve
{"event":"ready","addr":"127.0.0.1:3460","version":"0.1.0"}
```

The ready line goes to stdout and is flushed; stderr contains only JSON-line runtime logs.
Cold start (process start → ready line written, measured from `/proc/<pid>/stat` and the ready file mtime): **9 ms**.
SIGTERM exits gracefully and removes `state_dir/umsp.pid`.

## 4. Live HTTP Checks (curl)

| Request | Result |
|---|---|
| `GET /v1/models` + Bearer | 200, first byte 3.77 ms; 20-run average **0.48 ms**, max 0.65 ms |
| `GET /healthz` | 200, 20-run average **0.42 ms** |
| `GET /healthz?probe=1` | 200, first run 1556 ms (real upstream probe), 0.45 ms when cached within 30s |
| `HEAD /api/hello` | **204** |
| `POST /v1/chat/completions` (non-streaming) | 200, **1.274 s** (single run) / 3-run average 1.435 s |
| `umsp call --model od/deepseek-v4.1-flash` | 200, `content":"normal"`, 1649 ms |
| `umsp probe --provider od` | 200, 1357 ms, usage 5 in / 14 out |

`/v1/models` response (excerpt):

```json
{"id":"od/deepseek-v4.1-flash","object":"model","owned_by":"od",
 "context_length":1048000,"max_completion_tokens":128000,
 "capabilities":{"text":true,"vision":true,"tools":true,"reasoning":true},
 "display_name":"DeepSeek V4.1 Flash (OpenDesign)"}
```

That is, OD's original `metadata.context_limit` has been flattened into a top-level `context_length`
at the config layer; the response has no `metadata` field.

The non-streaming response passes through `reasoning_details` (summary + encrypted); `finish_reason` is normal:

```json
{"choices":[{"index":0,"finish_reason":"stop","message":{"role":"assistant","content":"normal",
 "reasoning_details":[{"index":0,"type":"reasoning.summary","summary":"..."},
                      {"index":1,"type":"reasoning.encrypted","data":"..."}]}}],
 "usage":{"prompt_tokens":34,"completion_tokens":23,
          "completion_tokens_details":{"reasoning_tokens":21},"total_tokens":57}}
```

## 5. Hermes E2E (Chat Completions)

```console
$ hermes chat -q "Reply only: normal" --provider custom --base-url http://127.0.0.1:3460/v1 \
      --api-key "$UMSP_API_KEY" --model od/deepseek-v4.1-flash
☤ Hermes
normal
HERMES_EXIT=0   # 5 seconds
```

The ledger has two records (Hermes first requests streaming, the gateway returns 501 per M0 scope,
and Hermes automatically falls back to non-streaming):

```json
{"status":501,"stream":true,"error_code":"streaming_not_implemented","bytes_in":60690}
{"status":200,"stream":false,"duration_ms":2472,"prompt_tokens":15383,"completion_tokens":...}
```

## 6. Error Paths

| Case | Result |
|---|---|
| Bad key | **401** `{"error":{"code":"unauthorized","type":"authentication_error"}}` + `X-UMSP-Code: unauthorized` |
| No key | 401 |
| Unknown model `od/nope` | **404** `model_not_found` |
| 52,428,801 B body (> 50 MiB) | **413** `request_too_large`, 0.6 ms (rejected early based on Content-Length) |
| Exactly 50 MiB and not JSON | 400 `invalid_json` (parse failed after reading up to the limit) |
| `stream: true` | **501** `streaming_not_implemented` (out of M0 scope; provided in M1) |
| Upstream 401 (integration-test fake upstream) | 401 + original body relayed verbatim, no `X-UMSP-Code` wrapper |
| Invalid config + SIGHUP | keeps serving with the old config (200), logs `config_reload_failed` to stderr |
| Restored config + SIGHUP | `config_reloaded` |

## 7. Logs and Usage

```console
$ wc -l ~/.local/share/umsp/logs/*.jsonl
7
$ umsp usage summary --since 24h --json | jq '{requests,errors,by_status}'
{"requests":7,"errors":4,"by_status":{"200":3,"400":1,"404":1,"413":1,"501":1}}
```

`requests` matches the JSONL line count (7 = 7).
A full-text scan of the JSONL and serve stderr for `authorization|bearer|x-api-key|sk-|umsp_` found 0 hits:
the ledger contains only field names and stats, with no key plaintext.

## 8. Resources

| Metric | Measured | Target |
|---|---|---|
| Release binary (musl, strip + LTO thin) | **6,309,264 B ≈ 6.02 MiB** | < 25 MB ✅ |
| Idle RSS (after startup) | **4,168 kB ≈ 4.07 MiB** | < 50 MB ✅ |
| RSS after 43 requests / peak | **5,712 kB** / 5,712 kB | ✅ |
| 20 MiB request body peak (one-off) | 57,744 kB (temporary buffer while reading the body, released after it drains) | See note below |
| Threads | 3 | — |
| Cold start | 9 ms | — |

Note: a body at the 50 MiB limit is read entirely into memory (`axum::body::to_bytes`), so handling
a request near the limit peaks RSS at ~57 MiB; this is a known M0 trade-off (M1 streaming forwards
chunk by chunk and no longer buffers the whole body).
Idle and regular-request resident memory stays below 5.7 MiB.

## 9. Rework Round (After Review Round 1)

Review (default) conclusion: only item 1 was valid (missed alias collision detection); the README `***` and `umsp probe --protocol`
items were false positives caused by tool-output redaction (verified by reading files directly via base64 and re-running the commands). In addition, two non-blocking items were handled in parallel.

### 9.1 Alias vs. model id Collision (Two-Pass Validation)

`src/config.rs::validate` now collects all model ids in a first pass (including duplicate detection) and validates aliases only in a second pass,
so a collision is reported even when the alias is declared before the conflicting model. Measured with both declaration orders (`target/release/umsp`):

```console
$ umsp check --config umsp_alias_a_to_b.toml --json   # alias first, model id after
{"ok":false,...,"errors":[{"code":"alias_collides_with_model",
  "message":"alias 'shared' of model 'od/a' collides with a model id"}],...}
exit=10

$ umsp check --config umsp_alias_b_to_a.toml --json   # model id first
{"ok":false,...,"errors":[{"code":"alias_collides_with_model",...}],...}
exit=10
```

New unit tests `alias_colliding_with_later_model_id_is_error` and
`alias_colliding_with_earlier_model_id_is_error` (plus `alias_self_is_only_a_warning`, which ensures
that "a model using its own id as an alias" is still only a warning).

### 9.2 Non-Blocking Items (Handled Together)

| Item | Handling | Measured |
|---|---|---|
| `umsp probe --protocol` non-chat | rejected early at the CLI boundary; no request sent, no misleading fields emitted | `umsp probe --provider od --protocol responses --json` → `{"ok":false,"error":{"code":"USAGE","message":"protocol 'responses' is not implemented in M0 (chat only)"}}`, exit 2 |
| `config.toml` permissions | uniform `0600` after `umsp init` and CLI atomic writes (the file contains the key hash + prefix) | new `umsp init` output is `600`; the live file has been `chmod 600` |
| Upstream `content-type` duplication | `forward_request_headers` skips `content-type` (same handling as `accept`) | captured by a local echo upstream: `content_type_count=1`, `accept_count=1`, `authorization_count=1` |
| `type` for 501 | `501 → invalid_request_error` (the caller can just set `stream:false`; this is not a server fault) | 501 response `"type":"invalid_request_error"`, `X-UMSP-Code: streaming_not_implemented` |
| `ok` in `usage --json` was a number | `ok` is now a boolean command status, and the successful-request count was renamed `ok_requests` | `{"ok":true,...,"requests":37,"ok_requests":19,"errors":18}`; `by-model` / `prune` likewise gained `"ok":true` |

### 9.3 Re-Test After Rework (Real Output)

```console
$ cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings   # passes
$ cargo test
running 51 tests ... test result: ok. 51 passed; 0 failed
running 11 tests ... test result: ok. 11 passed; 0 failed

$ ./scripts/build-musl.sh
binary:  6309264 bytes
archive: dist/umsp-0.1.0-x86_64-unknown-linux-musl.tar.gz
sha256(archive)=e04ba77cc32889da5ff84217b36267322de4b1691eef010f0424dd697fea5d10
sha256(binary)= d01163dfa772e457609bf467b75d5297725485afee290c1fff2f4372d01a5d8d
```

| Request | Measured After Rework |
|---|---|
| `GET /v1/models` (x-api-key / Bearer equivalent) | 200, 0.92–1.54 ms; flat `context_length=1048000`, `max_completion_tokens=128000`, `capabilities` object |
| `GET /healthz` | 200, 0.43 ms |
| `GET /healthz?probe=1` | 200, 1462–3700 ms (real upstream probe, 30s cache) |
| `HEAD /api/hello` | 204, 0 bytes |
| `POST /v1/chat/completions` (non-streaming OD) | 200, 1.50–1.62 s, `content="normal"`, `finish_reason=stop`, `reasoning_details` preserved (`reasoning_tokens=18/21`) |
| `umsp call --model od/deepseek-v4.1-flash` | 200, 1.387 s, `content="ok"` |
| `umsp probe --provider od` (chat) | 200, 1427–1581 ms |
| Bad key / no key | 401 `authentication_error` |
| Unknown model | 404 `model_not_found` |
| 52,430,076 B body | 413 `request_too_large`, 0.71 ms (early rejection via Content-Length) |
| Upstream parameter error | 400, body relayed verbatim, no `X-UMSP-Code` |
| Hermes E2E (original acceptance command) | replied "normal", exit 0, 8 s (501 streaming first → automatic non-streaming fallback) |

Resources and ledger (same instance after rework):

```console
$ ps -o rss= -p <pid>          # idle 4228 kB; 5796 kB after 37 requests (VmHWM 6000 kB)
$ wc -l ~/.local/share/umsp/logs/2026-11-19.jsonl
37
$ umsp usage summary --since 24h --json | jq '{requests,ok,ok_requests,errors,by_status}'
{"requests":37,"ok":true,"ok_requests":19,"errors":18,
 "by_status":{"200":19,"400":4,"404":4,"413":3,"501":7}}
```

`requests` (37) == JSONL line count (37); scanning the ledger and serve stderr for
`authorization|bearer|x-api-key|sk-|umsp_` plus the client key / OD key plaintext produced **0 hits**.
