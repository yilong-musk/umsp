#!/usr/bin/env bash
# Load / resource acceptance (spec 8.4).
#
# Starts a fake upstream and an isolated gateway (own config, env file and
# state dir), then runs the three phases:
#   1. 50 concurrent non-streaming requests for 30s
#   2. 20 concurrent streaming requests for 60s
#   3. 10,000 requests, watching RSS for growth
# and finally compares /metrics counters with the JSONL ledger.
#
# Everything runs on dedicated ports under a temp dir and is torn down on
# exit. JSON lines on stdout; logs stay in the work dir.
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"

GW_ADDR="${GW_ADDR:-127.0.0.1:18997}"
UP_ADDR="${UP_ADDR:-127.0.0.1:18998}"
WORK="${UMSP_LOAD_DIR:-$(mktemp -d -t umsp-load-XXXXXX)}"
mkdir -p "$WORK"
echo "{\"workdir\":\"$WORK\",\"gateway\":\"$GW_ADDR\",\"upstream\":\"$UP_ADDR\"}"

cargo build --release --bins --examples >/dev/null

cleanup() {
  [ -n "${GW_PID:-}" ] && kill "$GW_PID" 2>/dev/null || true
  [ -n "${UP_PID:-}" ] && kill "$UP_PID" 2>/dev/null || true
  wait 2>/dev/null || true
}
trap cleanup EXIT

./target/release/examples/fake_upstream --listen "$UP_ADDR" \
  --delay-ms 2 --chunks 12 --chunk-ms 4 >"$WORK/upstream.log" 2>&1 &
UP_PID=$!

export UMSP_CONFIG="$WORK/config.toml" UMSP_ENV_FILE="$WORK/env"
./target/release/umsp init --state-dir "$WORK/state" --force >/dev/null
printf 'LOAD_KEY=sk-load-fake\n' >"$WORK/env"
chmod 600 "$WORK/env"
# Drop the template's real OpenDesign provider: this box only talks to the fake.
./target/release/umsp model remove od/deepseek-v4.1-flash --json >/dev/null
./target/release/umsp model remove od/claude-haiku-5.5 --json >/dev/null
./target/release/umsp provider remove od --json >/dev/null
./target/release/umsp provider add --id fake --base-url "http://$UP_ADDR/v1" \
  --api-key-env LOAD_KEY --protocols chat,responses --quirks none --json >/dev/null
./target/release/umsp model add --id fake/load-model --provider fake \
  --upstream load-model --capability text --context-length 128000 \
  --max-completion-tokens 4096 --json >/dev/null
KEY="$(./target/release/umsp client add --name load --models 'fake/*' \
  --protocols chat,responses --json | sed -n 's/.*"key":"\([^"]*\)".*/\1/p')"
[ -n "$KEY" ] || {
  echo '{"error":"client key was not created"}' >&2
  exit 1
}

# Cold start: exec -> first successful /healthz response.
./target/release/umsp serve --listen "$GW_ADDR" >"$WORK/serve.log" 2>&1 &
GW_PID=$!
START_NS=$(date +%s%N)
READY_MS=""
for _ in $(seq 1 400); do
  if curl -fsS -m 1 "http://$GW_ADDR/healthz" >/dev/null 2>&1; then
    END_NS=$(date +%s%N)
    READY_MS=$(((END_NS - START_NS) / 1000000))
    break
  fi
  sleep 0.01
done
[ -n "$READY_MS" ] || {
  echo '{"error":"gateway did not become ready"}' >&2
  exit 1
}

rss_kb() { ps -o rss= -p "$GW_PID" | tr -d ' '; }
BIN_BYTES=$(stat -c%s target/release/umsp)
echo "{\"phase\":\"startup\",\"ready_ms\":$READY_MS,\"idle_rss_kb\":$(rss_kb),\"binary_bytes\":$BIN_BYTES}"

# run_phase <name> <peak|before-after> <loadgen args...>
run_phase() {
  local name=$1 sampler=$2
  shift 2
  local out="$WORK/$name.json"
  ./target/release/examples/loadgen --addr "$GW_ADDR" --path /v1/chat/completions \
    --key "$KEY" --model fake/load-model "$@" >"$out" &
  local pid=$!
  local peak=0 before after
  before=$(rss_kb)
  while kill -0 "$pid" 2>/dev/null; do
    local now
    now=$(rss_kb)
    [ "$now" -gt "$peak" ] && peak=$now
    sleep 0.2
  done
  wait "$pid" || true
  after=$(rss_kb)
  echo "{\"phase\":\"$name\",\"rss_before_kb\":$before,\"rss_peak_kb\":$peak,\"rss_after_kb\":$after,\"result\":$(cat "$out")}"
}

run_phase nonstream-50x30s peak --concurrency 50 --duration 30
run_phase stream-20x60s peak --concurrency 20 --duration 60 --stream
run_phase requests-10000 before-after --concurrency 20 --requests 10000

# /metrics vs ledger: one series increment per finished request.
METRICS="$(curl -fsS "http://$GW_ADDR/metrics")"
SERIES_SUM=$(printf '%s\n' "$METRICS" | awk '/^umsp_requests_total\{/{sum+=$NF} END{print sum+0}')
LEDGER_ROWS=$(cat "$WORK"/state/logs/*.jsonl | wc -l)
TTFT_COUNT=$(printf '%s\n' "$METRICS" | awk '/^umsp_ttft_seconds_count\{/{sum+=$NF} END{print sum+0}')
echo "{\"phase\":\"consistency\",\"metrics_requests_total\":$SERIES_SUM,\"ledger_rows\":$LEDGER_ROWS,\"metrics_ttft_count\":$TTFT_COUNT}"

USAGE=$(./target/release/umsp usage summary --since 24h --json)
echo "{\"phase\":\"usage\",\"summary\":$USAGE}"
echo "{\"phase\":\"doctor\",\"checks\":$(./target/release/umsp doctor --json)}"
