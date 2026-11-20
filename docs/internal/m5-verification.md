# M5 verification record: `/metrics` + usage wrap-up + systemd/deployment + load

Date: 2026-11-19 · Baseline: M4b (102 unit + 74 integration) · Environment: 2 vCPU / 2 GB cloud host, Ubuntu, Rust 1.100

## 1. Deliverables

| Slice | Content |
|---|---|
| metrics | `src/metrics.rs`: in-memory aggregation + Prometheus text; `AppState::finish_request` is the single exit point (one ledger line + one metrics update); `/metrics`, `/healthz` loopback-only, no key required |
| usage | `usage summary/by-* --provider`; `usage prune --days N` (mutually exclusive with `--before`, never deletes the current day) |
| deploy/CI | doctor gains `logs` writability and `metrics` probing (not running = skip); `deny.toml` + CI `deny` job (release depends on test+deny); `publish = false`; release script drops python3 |
| load | found and fixed concurrent ledger line loss; `examples/fake_upstream.rs`, `examples/loadgen.rs`, `scripts/load-test.sh` |
| docs | final README + this section |

## 2. Quality gates

```console
$ cargo fmt --all -- --check            # no output
$ cargo clippy --all-targets -- -D warnings
    Finished `dev` profile [unoptimized + debuginfo] target(s)
$ cargo test
    test result: ok. 114 passed; 0 failed        # unit (M4b: 102, +12)
    test result: ok.  77 passed; 0 failed        # integration (M4b: 74, +3)
$ cargo deny check                       # cargo-deny 0.21.0
advisories ok, bans ok, licenses ok, sources ok
$ systemd-analyze verify deploy/umsp.service   # exit 0
```

`cargo deny` exception (recorded in `deny.toml`): `RUSTSEC-2026-0543` marks chrono as unmaintained (no CVE, no security upgrade). Replacing it with jiff/time would touch ledger/CLI/stream-parsing timestamps, so it is left as a separate change.

## 3. `/metrics`

Output after real requests (excerpt, one smoke run each for OD + DS):

```console
$ curl -s http://127.0.0.1:3460/metrics | grep -E '^umsp_(requests_total|ttft_seconds_count|tokens_total|upstream_errors_total)'
umsp_requests_total{provider="deepseek",model="deepseek-flash",protocol="chat",status="200",stream="false"} 1
umsp_requests_total{provider="deepseek",model="deepseek-flash",protocol="messages",status="200",stream="false"} 1
umsp_requests_total{provider="od",model="deepseek-v4.1-flash",protocol="chat",status="200",stream="false"} 1
umsp_requests_total{provider="od",model="deepseek-v4.1-flash",protocol="chat",status="200",stream="true"} 1
umsp_ttft_seconds_count{provider="od",model="deepseek-v4.1-flash",protocol="chat"} 1
umsp_tokens_total{provider="od",model="deepseek-v4.1-flash",direction="input"} 70
umsp_tokens_total{provider="od",model="deepseek-v4.1-flash",direction="output"} 51
umsp_tokens_total{provider="od",model="deepseek-v4.1-flash",direction="reasoning"} 49
```

- 4 requests → 4 `requests_total` series (matching the 4 ledger lines); only the streaming one lands in `ttft_seconds`.
- `Content-Type: text/plain; version=0.0.4; charset=utf-8`; 200 even without Authorization; the fake upstream was not accessed before/after `curl` (integration test `metrics_need_no_key_and_match_the_ledger`).
- Fallback semantics: `metrics_count_a_fallback_once_under_the_serving_provider` asserts that when a primary 502 is absorbed by the fallback, `requests_total` has only the `provider="deepseek"` series, and the primary's 502 only goes into `umsp_upstream_errors_total{provider="od",code="upstream_502"}`.
- Non-loopback sources get 403 `loopback_only` (`admin_loopback_guard`, `into_make_service_with_connect_info`).

## 4. Load/resources (`bash scripts/load-test.sh`)

The script spins up its own fake upstream (`127.0.0.1:18998`, 2 ms latency, 12 chunks × 4 ms) and an isolated gateway (`127.0.0.1:18997`, separate config/env/state), and cleans up automatically when done.

| Scenario | Requests | Failures | rps | p95 latency | TTFT p50/p95 | Peak RSS |
|---|---|---|---|---|---|---|
| 50 concurrent non-streaming × 30s | 100,944 | 0 | 3,362 | 22.3 ms | — | 14.0 MB |
| 20 concurrent streaming × 60s | 16,848 | 0 | 280 | 74.2 ms (whole stream) | 4.1 / 6.1 ms | 13.7 MB |
| 20 concurrent fixed 10,000 | 10,000 | 0 | 2,975 | 9.9 ms | — | 13.7 MB → 13.7 MB |

- Cold start (exec → `/healthz` 200): 16 ms / 136 ms (two runs, including 10 ms polling granularity).
- Idle RSS 7.3 MB; release binary 6,491,400 B ≈ 6.19 MiB (target < 25 MB); musl archive in section 6.
- Consistency: sum of `umsp_requests_total` series **127,792** == JSONL line count **127,792**; `umsp_ttft_seconds_count` **16,848** == streaming request count; `requests` in `umsp usage summary --since 24h --json` is also 127,792, and each of `by_status/by_protocol/by_client/by_model/by_provider` sums to it.
- Target comparison: the 50-concurrent streaming target is < 120 MB; measured streaming 13.7 MB, non-streaming 14.0 MB.

## 5. Defect found and fixed by load testing

After 50-concurrent non-streaming for 30 s, `/metrics` showed 129,279 ≠ `umsp usage` 123,699 (difference 5,580 = 2,790 × 2). The ledger contained exactly 2,790 blank lines and 2,790 lines where two records were concatenated (JSON parse failures were skipped).

Root cause: `LogStore::append` used `writeln!(file, "{line}")`, and `File`'s `write_fmt` splits the body and `\n` into two `write` calls; with the same O_APPEND handle, two requests interleaved → body A + body B + `\n` + `\n`, dropping one line and adding a blank one.

Fix: build the whole line (including `\n`) in one buffer and write it with a single `write_all`, plus an outer `Arc<Mutex<()>>` (shared by clone). Regression test `concurrent_appends_never_merge_or_drop_lines` (50 threads × 40 lines, must be exactly 2,000 lines with no blanks). Re-run after the fix: 127,792 == 127,792.

## 6. Build / release archive / systemd

```console
$ bash scripts/build-musl.sh        # fmt + clippy + test + musl release + packaging
binary:  6612368 bytes
archive: dist/umsp-0.1.0-x86_64-unknown-linux-musl.tar.gz
f1697768b32e6891e303ce61f4b3efce853fb20810f845b501f64ae117cb6059  umsp-0.1.0-x86_64-unknown-linux-musl.tar.gz
$ tar tzf dist/umsp-0.1.0-x86_64-unknown-linux-musl.tar.gz
umsp-0.1.0-x86_64-unknown-linux-musl/
umsp-0.1.0-x86_64-unknown-linux-musl/umsp
umsp-0.1.0-x86_64-unknown-linux-musl/umsp.service
umsp-0.1.0-x86_64-unknown-linux-musl/README.md
umsp-0.1.0-x86_64-unknown-linux-musl/config.example.toml
$ (cd dist && sha256sum -c umsp-0.1.0-x86_64-unknown-linux-musl.tar.gz.sha256)
umsp-0.1.0-x86_64-unknown-linux-musl.tar.gz: OK
$ file dist/umsp-0.1.0-x86_64-unknown-linux-musl/umsp
ELF 64-bit LSB pie executable, x86-64, static-pie linked, stripped
```

`[profile.release]` already has `strip = true`, `lto = "thin"`, `codegen-units = 1`, `panic = "abort"`; the version number is now read from `Cargo.toml` with `awk`, so the repository no longer depends on python3.

`deploy/umsp.service` (not enabled): `MemoryMax=256M`, `TasksMax=256`, `NoNewPrivileges=true`, `ProtectSystem=strict`, `ProtectHome=read-only`, `PrivateTmp=true`, `ReadWritePaths=%h/.local/share/umsp`, `LimitNOFILE=8192`, `EnvironmentFile=%h/.config/umsp/env`; `systemd-analyze verify` exits 0. Upgrade steps: `install` → `systemctl --user daemon-reload && systemctl --user enable --now umsp`.

## 7. Real upstream smoke (M4 does not regress)

```console
OD chat non-streaming   http=200 1.31s  {"content":"OK", ...}
OD chat streaming       http=200 2.73s  ... data: [DONE]
DS chat non-streaming   http=200 0.64s  {"content":"OK", ...}
DS count_tokens         http=200 {"input_tokens":31}
```

The corresponding ledger has 4 lines (`provider` od/od/deepseek/deepseek, all status 200), `umsp usage summary --since 1h --provider od` = 4, `--provider deepseek` = 6 (including 2 lines left by the M4 review). 3460 was released after the smoke test.

## 8. `umsp doctor`

With serve running (the address from `UMSP_LISTEN` or the config):

```console
ok    metrics   /metrics 200 on 127.0.0.1:18997 (2450 bytes read)
ok    logs      /…/state/logs is writable
ok    state_dir /…/state
ok    listen    127.0.0.1:3460 (loopback)
ok    env_file  /…/env mode 600
ok    provider_key:fake  LOAD_KEY is set
summary {"ok":10,"warn":0,"fail":0}
```

With serve not running: `metrics` is `skip` (not counted as a failure).

## 9. Remaining items and boundaries

- Ledger retention is still manual: `umsp usage prune --days 30` (auto-deletion inside serve is not recommended).
- `/metrics` is not persisted and does not aggregate across replicas; it resets on restart.
- M5 adds no protocols/providers and does not change routing semantics; all M4b fallback/circuit/ProbeGuard tests remain (77 integration items including 22 M4b items).
