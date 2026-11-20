# M1 Verification Record (2026-11-19)

Environment: Ubuntu cloud host, 2 vCPU / 2 GB, musl static binary `umsp 0.1.0`,
upstream OpenDesign `https://amr-link.open-design.ai/v1`, model `od/deepseek-v4.1-flash`.
M1 scope: `stream: true` for Chat Completions (byte-for-byte SSE pass-through + OD streaming quirks + usage/ledger).

## 1. Build and Tests

```console
$ cargo fmt --all -- --check
$ cargo clippy --all-targets -- -D warnings
$ cargo test
running 57 tests ... test result: ok. 57 passed; 0 failed
running 19 tests ... test result: ok. 19 passed; 0 failed
```

Integration tests (fake SSE upstream, through a real axum Router) cover:

`streaming_forwards_bytes_verbatim_and_records_usage` (verbatim chunks / empty delta / `id:""` / usage),
`streaming_appends_done_when_upstream_omits_it`, `streaming_respects_explicit_include_usage_false`,
`streaming_keepalive_fills_upstream_silence`, `streaming_upstream_error_after_headers_emits_error_then_done`,
`streaming_upstream_error_before_headers_is_relayed_verbatim`,
`streaming_disconnect_before_headers_still_logs` (the 499 row is not lost),
`streaming_client_disconnect_is_recorded_and_server_survives`,
`non_boolean_stream_field_is_handled_as_false`, plus the 10 M0 regression cases.

New unit test `multibyte_character_split_across_chunks`: when an SSE event is split across
chunks in the middle of a multibyte UTF-8 character, the side-channel scanner no longer drops
the event (forwarding was already byte-for-byte and is unaffected).

Build:

```console
$ ./scripts/build-musl.sh
binary:  6366608 bytes
archive: dist/umsp-0.1.0-x86_64-unknown-linux-musl.tar.gz
$ file ~/.local/bin/umsp
ELF 64-bit LSB pie executable, x86-64, static-pie linked, stripped
```

## 2. Startup and Basic Endpoints

```console
{"event":"ready","addr":"127.0.0.1:3460","version":"0.1.0"}   # stdout, flushed immediately
GET  /healthz        200
GET  /v1/models      200
HEAD /api/hello      204
```

## 3. Real OD Streaming (curl -N, 3 runs, final binary)

Request body: `{"model":"od/deepseek-v4.1-flash","messages":[{"role":"user","content":"introduce yourself in one sentence"}],"stream":true}`

| Run | HTTP | First byte | Total | Downstream bytes |
|---|---|---|---|---|
| 1 | 200 | 1.295 s | 1.896 s | 9,000 |
| 2 | 200 | 1.256 s | 1.934 s | 9,912 |
| 3 | 200 | 1.260 s | 1.951 s | 9,916 |

Response headers: `content-type: text/event-stream`, `cache-control: no-cache`,
`x-accel-buffering: no`, `transfer-encoding: chunked`, **no `content-length`**;
`x-request-id` (upstream) and `x-umsp-request-id` / `x-umsp-provider` / `x-umsp-upstream-model` are all present.

The body has **53** `data:` events total and exactly 1 `data: [DONE]` at the end; the first event is OD's
`{"id":"resp_…","choices":[{"index":0,"delta":{}}],…}` empty delta; mid-stream `id:""` events and
`reasoning` deltas all arrive verbatim; the last event carries
`usage:{prompt_tokens:34,completion_tokens:…,completion_tokens_details:{reasoning_tokens:…}}`.

Corresponding ledger rows (3):

```json
{"status":200,"upstream_status":200,"stream":true,"ttft_ms":1294,"duration_ms":1895,
 "bytes_out":9000,"prompt_tokens":34,"completion_tokens":83,
 "quirks":["injected_include_usage"],"client_disconnected":false}
```

`ttft_ms` is taken from the first upstream `data:` JSON event (an empty delta counts); it is non-zero.

## 4. Hermes E2E (Streaming)

Plain reply:

```console
$ UMSP_API_KEY=$(cat ~/.config/umsp/client-default.key)
$ hermes chat -q "Reply only: normal" --provider custom --base-url http://127.0.0.1:3460/v1 \
      --api-key "$UMSP_API_KEY" --model od/deepseek-v4.1-flash
☤ Hermes
normal
```

The ledger gains exactly 1 row: `{"status":200,"stream":true,"ttft_ms":2112,"completion_tokens":2}`; there is **no 501 fallback**.

Tool streaming (`"run date with terminal, then tell me the result"`): 2 `tool_calls`, answer
`Thu Nov 19 06:29:31 PM CST 2026`; the ledger gains 2 rows, both `status=200, stream=true`
(`ttft_ms` 1606 / 2465). All old `501 streaming_not_implemented` rows predate
17:44 (M0 period); none appeared after the M1 binary started.

## 5. Disconnect

After `timeout 2.5 curl -N … "count from 1 to 300"` is cut off mid-stream (curl rc=124):

- `/healthz` still returns 200 and the process has not panicked (0 `panic` hits in stderr);
- ledger: `{"status":200,"stream":true,"client_disconnected":true,"bytes_out":15351}`;
- stderr: `{"event":"client_disconnected","request_id":"…"}`.

The upstream request is cancelled when the response stream is dropped (the pump drops `reqwest::Response` as soon as it observes `tx.closed()`).

## 6. Regression (Non-Streaming and Error Paths)

| Case | Result |
|---|---|
| Non-streaming Chat | 200, `content="normal"`, usage 34/2 |
| Bad key | 401 `unauthorized` |
| Unknown model `od/nope` | 404 `model_not_found` |
| 51 MiB body | 413 `request_too_large` (early rejection via Content-Length) |
| `stream: "yes"` | 200 non-streaming JSON (the gateway normalizes it to `false`; ledger `quirks:["stream_non_boolean"]`); before the fix OD returned 400 directly |
| `stream_options.include_usage:false` | 200 SSE; not injected, and when OD still returns usage it is recorded as usual |
| Non-2xx before upstream response headers | integration test: status code/body/key headers relayed verbatim, not converted to SSE |
| Upstream 401 body | integration test: relayed verbatim, not wrapped |

## 7. Logs and Ledger

```console
$ python3 count_usage.py     # filter the last 1h by ts, then compare with summary
total_rows=75 in_window=50 summary_requests=50 match=True
$ umsp usage summary --since 1h --json
{"requests":50,"ok_requests":35,"errors":15,"by_status":{"200":35,"400":3,"404":4,"413":4,"499":1,"501":3}}
```

The 501s in `by_status` are historical M0 rows (ts ≤ 17:44); the 499 is the disconnect test row.

Leak scan: 0 hits for the client key plaintext, 0 hits for the upstream key, and
0 hits for `authorization: bearer` in the ledger and serve stderr.

## 8. Resources

| Metric | Measured | Target |
|---|---|---|
| Release binary (musl, strip + LTO thin) | **6,366,608 B ≈ 6.07 MiB** | < 25 MB ✅ |
| Idle RSS (after 3 streaming runs + Hermes E2E + 1 disconnect) | **5,808 kB ≈ 5.67 MiB** | < 50 MB ✅ |
| Threads | 4 | — |
| Single streaming run (~9–10 KB output) | first byte ~1.26 s, total 1.90–1.95 s (upstream dominates) | — |

## 9. Known Limitations (M1)

- The request body is still read in full (50 MiB limit); there is no request-side streaming forwarding; `Content-Encoding: gzip` is not handled.
- Streaming responses are not gzipped; `reasoning` fields are not rewritten/mirrored.
- Stream idle 300s (`stream_idle_timeout_s`), upstream response headers 300s (`stream_header_timeout_s`),
  and keepalive 15s (`stream_keepalive_s`) are configurable; on timeout the stream ends with a protocol-native error + `[DONE]`.
