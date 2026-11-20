# M4b verification record: fallback + provider circuit breaking + DeepSeek balance health

Date: 2026-11-19 · Baseline: M4a · Spec: internal design doc §3.3 / §3.4 / §2.0 / §6.1 / §8.3

## 1. Implementation scope

### 1.1 fallback (spec 3.3)

- `Model.fallbacks` is actually executed: each request takes the **first** available target in order, **same protocol**, cross-provider,
  **at most 1 hop**; `count_tokens` only falls back to providers that implement that endpoint (currently only DS).
- **Only takes effect before response headers are sent to the client**: once SSE has returned 200 there is no more switching (failures mid-stream emit the protocol-native
  error event per 2.0).
- Retryable: connection errors, timeouts (header/total duration), 429, 5xx, 402; not retried: 400/401/403/404/422
  (401 is an upstream key configuration error; switching providers would only take down both).
- If the fallback target also fails → return the **last upstream error verbatim** (status code + body + key headers such as `retry-after`),
  unwrapped; the status code and `X-UMSP-Code` appear only for errors the gateway itself produces.
- Fallback requests re-apply the target provider's Chat request quirks (e.g. DS's `reasoning_content`
  backfill) and rewrite `model`; other client fields are untouched.
- Config (`~/.config/umsp/config.toml` synced with `config.example.toml`):

  ```toml
  # od/deepseek-v4.1-flash
  fallbacks = ["deepseek/deepseek-flash"]
  # deepseek/deepseek-flash
  fallbacks = ["od/deepseek-v4.1-flash"]
  ```

  A bidirectional cycle is allowed, but a request takes at most 1 hop, so it never loops.
- Validation (`umsp check` / `umsp config validate`): the target must exist (`fallback_missing`), must not self-reference
  (`fallback_self`), and the target provider must support all protocols of the source provider (`fallback_protocol`).

### 1.2 Circuit breaking (spec 3.4)

- One in-memory state machine per provider (`src/circuit.rs`, Mutex + tokio `Instant`):
  `closed` → 5 retryable failures within 30s → `open` (30s) → `half_open` (1 probe; concurrent requests are all rejected
  while the probe is in flight) → success `closed` / failure back to `open`.
- `count_tokens` shares the same provider circuit as normal requests.
- Only 429 / 5xx / connection errors / timeouts are counted; 400/403/404/422 are not counted and do not reset the count.
- Special cases:
  - DS `402` → immediately `open` for 10 minutes, reason `insufficient_balance`; in the background every 5 minutes
    `GET https://api.deepseek.com/user/balance`, and `is_available=true` automatically `closed`.
  - OD `401`/`402` → immediately `open` for 5 minutes, reason `auth_failed` / `payment_required`; in the background a minimal reasoning probe
    with `max_tokens=1` + `reasoning_effort=none` restores it (not `/v1/models`).
  - 401 from other providers likewise opens the auth circuit; 402 for 5 minutes.
- While the circuit is open: **no upstream contact**, directly return protocol-native `503` + `X-UMSP-Code: provider_circuit_open`
  (OpenAI shape for Chat/Responses, Anthropic shape for Messages), with
  `x-umsp-provider` / `x-umsp-upstream-model`. If a fallback is configured and its target is `closed`, that hop is still allowed
  (without exceeding 1).
- The background health task is started by `serve` and `abort`ed when `serve` exits, so it never blocks process exit; single binary, no external
  scheduler. Circuit state is in memory only and clears on restart (allowed by spec v1).

### 1.3 Status / logs (spec 4.2 / 6.3)

- Each ledger line gains `fallbacks` (`0`/`1`); `provider` / `upstream_model` record the one that **actually answered**,
  and `quirks` gains `fallback_from:<primary provider>`. Thus `umsp usage`'s `by_provider`/`by_model`
  are counted by the actual serving party, with no CLI changes.
- New ops log (stderr JSONL) events:
  - `fallback`: request_id/client/provider/model + from_provider/from_model/reason;
  - `circuit_open`: provider/model/reason (`http_5xx`, `http_402:insufficient_balance`,
    `auth_failed`, `upstream_timeout`…);
  - `circuit_close`: provider/model/reason (`success`, `probe_ok`, `balance_restored`);
  - `circuit_open_reject`: requests rejected by an open circuit.
  None contain keys or bodies.

### 1.4 `/healthz` (spec 6.1)

Each provider now carries `circuit` (`closed|open|half_open`), `failures` (retryable failures in the current window),
`opened_at` (RFC3339), `reason`; when open, the overall `status` becomes `degraded` (HTTP is still 200).

`?probe=1`: DS first does `GET /user/balance` (`is_available=false` goes straight to degraded, spending no tokens, and also lifts
`balance` to a provider-level field); other providers use the minimal reasoning probe (30s cache).

## 2. Automated tests

```
$ cargo fmt --check
$ cargo clippy --all-targets -- -D warnings
$ cargo test
```

Results (2026-11-19, local machine):

- Unit tests **102 passed** (M4a 84 + M4b 13 + review 5: cancellation releases / old guard does not clear a new probe /
  leak backstop reclamation / background probe holds the slot / background re-take of a cancelled slot).
- Integration tests **74 passed** (M4a 59 + M4b 13 + review 2, all through the real router + local fake upstream).
- fmt / clippy clean (`-D warnings`).

New integration cases (`tests/integration.rs` "M4b" section):

| Case | Assertion |
|---|---|
| `fallback_triggers_on_retryable_statuses` | 500/429/402/503 → 200, `x-umsp-provider=deepseek`, ledger `fallbacks=1` + `fallback_from:od` |
| `fallback_triggers_on_connection_error` | od points at `127.0.0.1:1` (connection refused) → fallback serves 200 |
| `client_errors_do_not_fallback` | 400/404/422 → returned verbatim, only primary is hit |
| `failed_fallback_returns_the_last_upstream_error` | primary 500 + fallback 502 → 502 verbatim `fallback-model says 502` |
| `stream_failure_after_200_does_not_fallback` | stream breaks after SSE 200 → no provider switch |
| `five_failures_open_the_circuit_and_reject_with_503` | 5×500 → `/healthz` open/failures=5/opened_at; next request 503 `provider_circuit_open` with no upstream call |
| `not_found_never_opens_the_circuit` | 5×404 → circuit closed, failures 0 |
| `open_circuit_still_uses_a_closed_fallback` | after the primary circuit opens, requests are still served by the closed fallback and the primary is no longer hit |
| `deepseek_402_opens_the_circuit_immediately` | DS 402 passed through → open/`insufficient_balance`, next request 503 with no upstream call |
| `od_401_passes_through_and_opens_the_circuit` | 401 verbatim pass-through → open/`auth_failed`, next request 503 |
| `circuit_open_uses_the_anthropic_error_shape_for_messages` | `/v1/messages` circuit open → `{"type":"error","error":{"type":"api_error"}}` + `X-UMSP-Code` |
| `healthz_circuit_fields_never_leak_keys` | circuit fields present, body contains no `sk-test`/`sk-ds-test` |
| `success_resets_the_failure_window` | 4×500 + 1×200 → closed, failures 0 |
| `half_open_probe_client_error_does_not_wedge_the_circuit` | open + after cooldown the probe gets 404 → 404 passed through verbatim, circuit `closed` (not counted as failure), next request 200 (not a permanent 503) |
| `cancelled_half_open_probe_releases_the_slot` | client gives up while the probe hangs (future dropped) → slot released, next request 200, circuit `closed`, failures 0 |

## 3. Real upstream verification

### 3.1 DS balance

```
$ curl -H "Authorization: Bearer ***" https://api.deepseek.com/user/balance
HTTP 200
{"is_available":true,"balance_infos":[{"currency":"CNY","total_balance":"5.78",
 "granted_balance":"0.00","topped_up_balance":"5.78"}]}
```

`GET /healthz?probe=1` (release binary, 3460):

```json
{"od": {"circuit": "closed", "probe": {"status": "ok", "latency_ms": 1604}},
 "deepseek": {"circuit": "closed",
   "probe": {"status": "ok", "latency_ms": 464,
             "balance": {"is_available": true, "total": "5.78", "currency": "CNY"}},
   "balance": {"is_available": true, "total": "5.78", "currency": "CNY"}}}
```

### 3.2 Real requests (M1–M4a regression)

All six entry points return HTTP 200 (`max_tokens`/`max_output_tokens` = 16):

| Request | Status | usage |
|---|---|---|
| chat `od/deepseek-v4.1-flash` | 200 | prompt 33 / completion 16 (reasoning 16) |
| chat `deepseek/deepseek-flash` | 200 | prompt 33 / completion 8 (reasoning 6) |
| responses OD | 200 | 33 / 7 (reasoning 5) |
| responses DS | 200 | 33 / 12 (reasoning 10) |
| messages OD | 200 | 33 / 10, `cache_creation.ephemeral_*` shape |
| messages DS | 200 | 33 / 13, flat `cache_creation_input_tokens` |

`umsp usage summary` / `by-model` aggregate the new ledger lines correctly (`by_provider` shows `deepseek` for the fallback lines).

## 4. Failure-injection test (release binary + local fake upstream, isolated instance on 3461)

Config: `od` → `http://127.0.0.1:19999/v1`, `deepseek` → `http://127.0.0.1:19999`;
`od/primary-model` falls back to `deepseek/fallback-model`; `od/solo-model` has no fallback.
The fake always returns 500 for `primary-model`/`solo-model` and always 200 for `fallback-model`.

```
--- 1) fallback
resp: HTTP/1.1 200 OK x-umsp-provider: deepseek x-umsp-upstream-model: fallback-model
upstream hits: ['primary-model', 'fallback-model']

--- 2) od/solo-model five consecutive 500s (count merges with the step-1 primary 500 = 5)
  #1 500   #2 500   #3 500   #4 500   #5 503 provider_circuit_open

--- 3) healthz
{"status": "degraded", "od": {"circuit": "open", "failures": 5,
 "opened_at": "2026-11-19T20:19:51.111+08:00", "reason": "http_500"}}

--- 4) request again after the circuit opens
resp: HTTP/1.1 503 Service Unavailable x-umsp-code: provider_circuit_open
upstream calls before=6 after=6        # no upstream hit

--- 5) circuit open + fallback closed
resp: HTTP/1.1 200 OK x-umsp-provider: deepseek x-umsp-upstream-model: fallback-model

--- 6) logs
{"event":"fallback","provider":"deepseek","model":"fallback-model",
 "from_provider":"od","from_model":"primary-model","reason":"http_500"}
{"event":"circuit_open","provider":"od","model":"solo-model","reason":"http_500"}
{"event":"circuit_open_reject","provider":"od","code":"provider_circuit_open"}
{"event":"fallback",...,"reason":"circuit_open:http_500"}
```

Ledger (8 lines from the same instance):

```
od/primary-model -> provider=deepseek upstream=fallback-model status=200 fallbacks=1 [fallback_from:od]
od/solo-model    -> provider=od       upstream=solo-model     status=500 fallbacks=0
... ×4
od/solo-model    -> provider=od       upstream=solo-model     status=503 error_code=provider_circuit_open
... ×2 (circuit rejections, no upstream calls)
od/primary-model -> provider=deepseek upstream=fallback-model status=200 fallbacks=1 [fallback_from:od]
```

## 5. Resources and wrap-up

- release binary: `6,396,360` bytes (strip + lto=thin, after the review fixes).
- Idle RSS: `8,840 kB` (M4b main body, with serve running on 3460); the final binary after the review fixes measured `3,648 kB` (3463, after probing + two real requests).
- 3460 / 3461 / 3462 / 19997 / 19999 were all released after verification ended; workspace clean (see commit log).

## 6. Known boundaries

- **No same-provider retry**: one failure goes straight to fallback (at most 1 hop); there is no "retry once, then fallback".
- `duration_ms` still counts only the **final** upstream call's duration (M4a semantics), not the total time including fallback.
- Circuit state is in memory and clears on restart; not shared across replicas (not a v1 goal).
- `/healthz?probe=1` only reports; it does not change circuit state. Recovery is done by the background task or the probe of the next real request.
- If the DS balance query fails, the circuit stays open and retries after 5 minutes; a single timeout does not let traffic through.

## 7. Review fix: half-open probe slot leak (2026-11-19, round 2)

### 7.1 Defect

After the `open` cooldown ends, `admit()` switches the state to `half_open` and sets `probe_in_flight=true`, but the flag is only
reset when "the upstream result is recorded normally". Two real outcomes do not record a result:

1. **Request cancelled**: the client disconnects before the probe request returns / the task is aborted, and the future is simply dropped;
2. **Probe gets a non-retryable 4xx**: the `_ => {}` branch in `not_found_never_opens_the_circuit` calls neither
   `record_success` nor `record_failure` (harmless in the closed state, but a hard hang in the probe state).

Both leave `probe_in_flight` permanently `true`; every later request takes the `half_open` branch and gets
`Reject`, the provider returns 503 forever until the process restarts; `take_probe()` only recognizes `state == Open`, so the background
health task cannot rescue it either.

### 7.2 Fix

- `Admission::Probe` now carries a `ProbeGuard`: whoever gets the probe **must** hold the guard, and on Drop, if
  `probe_generation` is still its own generation, the slot is released — **release only, no failure counted** — the state stays
  `half_open` and the next request can continue probing. `take_probe()` likewise returns a guard, and the background task's probe
  is protected the same way.
- Every grant / cleanup of the probe slot increments `probe_generation`; `record_success` / `record_failure` /
  `force_open` / `close` all clean up the slot and increment the generation, so **a late old guard cannot accidentally clear a new probe's slot**.
- A probe getting a non-retryable 4xx (400/403/404/422…) means the provider is reachable: call `close()` directly (log
  `circuit_close reason=probe_reachable:http_<code>`), not treated as a failure.
- `PROBE_STALE_AFTER` (15 minutes, larger than the 600s non-streaming timeout) as a backstop: even if a guard leaks
  (`mem::forget` and the like), the slot is reclaimed rather than wedged forever.
- `take_probe()` (background health task) now also accepts "`half_open` with the slot idle/expired": after a probe is
  cancelled, even with no client traffic, the background task can probe and recover on its next tick.
- Concurrent requests during a probe are still always `Reject`ed (single-probe semantics unchanged).

### 7.3 Tests

- Unit (`src/circuit.rs`): `cancelled_probe_releases_the_slot` (the reproduction path from the review),
  `stale_guard_cannot_release_a_newer_probe`, `leaked_probe_slot_is_reclaimed_after_the_backstop`,
  `background_probe_owns_the_slot_until_dropped`.
- Integration (`tests/integration.rs`): `half_open_probe_client_error_does_not_wedge_the_circuit`,
  `cancelled_half_open_probe_releases_the_slot` (the fake upstream supports delays; the request future is dropped
  after 50ms).
- Reverse verification (reverting the fixes one at a time and running the tests): making `ProbeGuard::drop` a no-op →
  `cancelled_probe_releases_the_slot` and `cancelled_half_open_probe_releases_the_slot` fail;
  removing the close for the 4xx probe → `half_open_probe_client_error_does_not_wedge_the_circuit`
  fails (`half_open != closed`). That is, each of the two new cases guards half of the fix.

### 7.4 Release binary failure injection (3462 + fake 19997, same config)

```
== 1) 5x500 on od/solo-model ==
  #1..#5 500
  healthz: circuit=open failures=5 reason=http_500 opened_at=2026-11-19T20:48:06.506+08:00
== 2) wait 31s; the single probe returns 404 ==
  probe response: 404    body: {"error": {"message": "solo-model says 404", ...}}
  healthz: circuit=closed failures=0 reason=None opened_at=None
== 3) next request must be served, not 503 ==
  response: 200          healthz: circuit=closed failures=0
== 4) fallback regression: primary 500 -> fallback 200 ==
  HTTP/1.1 200 OK  x-umsp-provider: deepseek  x-umsp-upstream-model: fallback-model
== 5) cancellation: open -> wait -> client aborts a hung probe ==
  healthz after warmup: circuit=closed failures=0
  5x500                  -> circuit=open failures=5 reason=http_500
  waiting 31s ...
  aborted probe http_code=000 (curl exit 28 = client gave up)
  healthz after cancel: circuit=half_open failures=5 reason=http_500   # cancellation is not counted as failure
  next request: 200      healthz: circuit=closed failures=0
```

serve logs (no keys):

```
{"event":"circuit_open","provider":"od","model":"solo-model","reason":"http_500"}
{"event":"circuit_close","provider":"od","model":"solo-model","reason":"probe_reachable:http_404"}
{"event":"circuit_open","provider":"od","model":"solo-model","reason":"http_500"}
{"event":"circuit_close","provider":"od","model":"solo-model","reason":"success"}
```

`/healthz?probe=1` (same instance): both `od` / `deepseek` show `circuit=closed`, `probe=ok`.
The fake upstream printed `BrokenPipeError` when the probe was cancelled, confirming the connection was indeed abandoned before the result returned.
