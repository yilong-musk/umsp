# UMSP — Universal Model Service Provider

[![CI](https://github.com/yilong-musk/umsp/actions/workflows/ci.yml/badge.svg)](https://github.com/yilong-musk/umsp/actions/workflows/ci.yml)
[![Rust](https://img.shields.io/badge/Rust-1.85+-000000?logo=rust&logoColor=white)](https://www.rust-lang.org)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

[German](README.de.md) | English | [Spanish](README.es.md) | [French](README.fr.md) | [Japanese](README.ja.md) | [Korean](README.ko.md) | [Portuguese](README.pt.md) | [Russian](README.ru.md) | [Chinese (Simplified)](README.zh-CN.md) | [Chinese (Traditional)](README.zh-TW.md)

A **single-binary, loopback-only** AI gateway. It exposes OpenAI Chat Completions, OpenAI Responses,
and Anthropic Messages on one port, then forwards each request to an upstream that speaks the same
protocol — or translates it when the backend cannot. No UI, database, Redis, or Docker.

- One `umsp` binary; config in `~/.config/umsp/config.toml`, keys in `~/.config/umsp/env` (0600),
  JSONL request ledger in `~/.local/share/umsp/`.
- Logical models: callers see one model id; each model has an ordered `backends` list.
- Capacity-aware selection: fresh usage first (U), then no data (X), then balance-only (B);
  a successful provider stays pinned for five hours.
- Per-provider circuit breaking, DeepSeek balance health, Grok weekly-quota polling.
  Qwen Token Plan has neither, so it stays in X.
- Client requests are provider-invisible; upstream errors are mapped to neutral gateway errors.

## Quick start

### Install

Release package (no Rust toolchain on the server):

```bash
tar -xzf umsp-<version>-x86_64-unknown-linux-musl.tar.gz
install -Dm755 umsp-<version>-x86_64-unknown-linux-musl/umsp ~/.local/bin/umsp
sha256sum -c umsp-<version>-x86_64-unknown-linux-musl.tar.gz.sha256
file ~/.local/bin/umsp                   # statically linked
```

Pushing an annotated tag `vX.Y.Z` (the number must match `version` in `Cargo.toml`)
publishes this archive as a GitHub Release, signed with minisign
([`umsp.pub`](umsp.pub)). Installing it is up to you: the repository also
gets no host access. See
[Installing a release](docs/operations.md#installing-a-release).

### Build from source

```bash
sudo apt-get install -y musl-tools
rustup target add x86_64-unknown-linux-musl
git clone https://github.com/yilong-musk/umsp.git && cd umsp
./scripts/build-musl.sh                  # output in dist/
```

### Run

```bash
umsp init                                # config.toml, env (0600), state directory
$EDITOR ~/.config/umsp/env               # add upstream keys, e.g. DEEPSEEK_API_KEY=...
umsp client add --name default           # prints the client key once (umsp_...)
umsp check --json                        # must be green
umsp serve                               # prints {"event":"ready",...}
```

Call it from another terminal:

```bash
export UMSP_API_KEY=umsp_xxx
curl -s http://127.0.0.1:3460/v1/models -H "x-api-key: $UMSP_API_KEY" | jq

curl -s http://127.0.0.1:3460/v1/chat/completions \
  -H "x-api-key: $UMSP_API_KEY" -H 'Content-Type: application/json' \
  -d '{"model":"deepseek-v4.1-flash","messages":[{"role":"user","content":"Reply with only: OK"}]}' | jq
```

`Authorization: Bearer ***` is equivalent to `x-api-key`.

### Device binding (one key, one device)

Client keys use trust-on-first-use (TOFU) device binding: a new key is unbound,
and the **first authenticated request** binds it to that request's source IP.
Every later request with that key must come from the same IP or it is rejected
with `403 device_mismatch` (the error body never reveals the bound IP).
Bindings persist in `state_dir/client-bindings.json` and survive restarts.

- Behind a reverse proxy the real client IP is the rightmost `X-Forwarded-For`
  entry written by the trusted loopback proxy; client-supplied entries are
  ignored. Local (loopback) callers bind to `127.0.0.1`.
- `umsp client list` shows each key's bound IP. `umsp client unbind <name>`
  clears a binding (the next request re-binds — use it when a device's IP
  changes); `rotate` and `remove` clear it automatically.
- Devices behind the same NAT share one public IP and are indistinguishable;
  issue one key per device for attribution and per-device revocation.

### Client limits

Each key can carry optional limits; unset means unlimited. Set them on
`umsp client add` and change them with `umsp client update <name>`:

- `--expires-in 30d` — the key is rejected with `401 key_expired` after that
  (`--no-expiry` on `update` removes it).
- `--max-concurrency 1` — at most N (1-9) model calls in flight; more get
  `429 concurrency_limit` (`--no-concurrency-limit` removes it).
- `--budget-tokens 5000000` — lifetime token budget. Each call reserves its
  output cap (the model's default when the request sets none) first and is refused with `403 budget_exhausted` when it does not
  fit; real usage is charged afterwards (`--reset-usage` / `--no-budget` on
  `update`).

Details: [docs/configuration.md](docs/configuration.md#client-limits).

## Configuration

Minimal `~/.config/umsp/config.toml`:

```toml
[server]
listen = "127.0.0.1:3460"
state_dir = "~/.local/share/umsp"

[[providers]]
id = "deepseek"
vendor = "deepseek"                       # built-in catalog; endpoints and quirks come from it
base_url = "https://api.deepseek.com"
anthropic_base_url = "https://api.deepseek.com/anthropic"
api_key_env = "DEEPSEEK_API_KEY"          # name only; the value lives in env
protocols = ["chat", "responses", "messages"]
quirks = "deepseek"

[[models]]
id = "deepseek-v4.1-flash"                # callers only know this logical id
aliases = ["deepseek-flash"]
context_length = 1048000
max_completion_tokens = 128000
capabilities = ["text", "vision", "tools", "reasoning"]
backends = [
  { provider = "deepseek", upstream = "deepseek-flash" },
]

[[clients]]
name = "hermes"
key_hash = "sha256:..."                   # written by `umsp client add`
key_prefix = "umsp_ab12cd34"
allowed_models = ["*"]
allowed_protocols = ["chat", "responses", "messages"]
```

- Upstream keys never go in `config.toml`; only `api_key_env` names do.
- Precedence: `--config` > `$UMSP_CONFIG` > `~/.config/umsp/config.toml`;
  `$UMSP_LISTEN`, `$UMSP_STATE_DIR`, `$UMSP_ENV_FILE` override individual paths.
- `/v1/models` contains one entry per logical model and never exposes providers.
  Every 12 hours the gateway reads each provider's model list and deduplicates
  by id. Context, output limit, display name, and capabilities come from that
  list when the provider sends them, including every context tier. A single
  `context_length` is the only tier, or the largest when the list has several.
  A documented default fills a field the
  list left empty and never overwrites an upstream value. A handwritten
  `[[models]]` entry then fills whatever is still empty. `umsp catalog refresh` reads
  immediately. `allowed_models = ["*"]` (or an empty list) shows that full catalog.
- `provider add --vendor ID` takes only the vendor id and key; every add gets its own id.

Full schema, provider/model/client management, and all CLI examples:
[docs/configuration.md](docs/configuration.md).

## Common commands

```bash
umsp provider list|add|remove|test      # upstream instances; add uses the built-in vendor catalog
umsp login|logout grok                  # per-provider Grok device-code session
umsp model list|add|remove              # logical models and aliases
umsp model backend add|remove           # attach/detach a backend (--first = higher priority)
umsp client list|add|remove|rotate|unbind # client keys (SHA-256 stored) + TOFU device binding
umsp env list|set|unset                 # upstream keys, masked
umsp usage summary|by-model|prune       # query the request ledger
umsp doctor [--online]                  # diagnostics; --online spends a few tokens
```

Every subcommand supports `--json`. See [docs/configuration.md](docs/configuration.md) for the full
table of subcommands and flags.

## HTTP endpoints

| Method | Path | Auth | Purpose |
|---|---|---|---|
| GET | `/healthz` | No | Curl-able status: circuit, failures, last result, fresh `usage` |
| GET | `/metrics` | No | Prometheus text, in-memory, loopback only |
| GET | `/v1/models` | Yes | Logical model registry (flat metadata + `meta` envelope) |
| POST | `/v1/chat/completions` | Yes | Chat Completions, streaming and non-streaming |
| POST | `/v1/responses` | Yes | Responses, streaming and non-streaming |
| POST | `/v1/messages` | Yes | Anthropic Messages, streaming and non-streaming |
| POST | `/v1/messages/count_tokens` | Yes | Proxied where the backend supports it |
| HEAD | `/api/hello` | No | Claude Code connection warm-up |

Detailed endpoint behavior, error mapping, metrics, ledger fields, systemd unit, exit codes, and
troubleshooting: [docs/operations.md](docs/operations.md).

## Grok subscriptions

`umsp provider add --vendor grok` creates a provider backed by a device-code OAuth session at
`{config_dir}/sessions/<id>.json`. `umsp provider add --vendor chatgpt` uses Sign in with
ChatGPT (PKCE on `127.0.0.1`, plan usage on `https://api.openai.com/v1/responses` with
`store: false` and a required stream). Neither grant is an API key, and the two sessions
never share a refresh token. `umsp login <id>` signs one provider in; `umsp logout <id>`
deletes only that file.

> **Experimental / account risk.** Grok support uses a public CLI OAuth client and the
> `cli-chat-proxy` subscription endpoint. It is not an official xAI API integration: it can break
> without notice and may violate xAI's terms of service, including account suspension. Use it only
> with an account you are willing to risk.

The gateway sends the required `x-grok-client-version` (override with `UMSP_GROK_CLI_VERSION` when
xAI raises the minimum). Weekly subscription usage is read from the billing endpoint, not from the
short-lived `x-ratelimit-*` headers. Details: [docs/model-routing.md](docs/model-routing.md).

## Qwen Token Plan

`umsp provider add --vendor qwen-token-plan` uses the Qwen Token Plan host shown in the
subscription console (`token-plan.maas.qianwenaiapi.com`), not pay-as-you-go DashScope and
not the international `qwencloudapi.com` host. A `sk-` key or the DashScope host will not
draw down this plan. Chat goes to
`https://token-plan.maas.qianwenaiapi.com/compatible-mode/v1`, Responses to
`https://token-plan.maas.qianwenaiapi.com/compatible-mode/v1/responses`, and Messages to
`https://token-plan.maas.qianwenaiapi.com/apps/anthropic`. The key variable is
`QWEN_TOKEN_PLAN_API_KEY`. The plan is documented for
interactive clients. The older regional host
`token-plan.cn-beijing.maas.aliyuncs.com` still answers, but it is not the console URL.

## OpenCode Go

`umsp provider add --vendor opencode-go` uses the OpenCode Go subscription host
`https://opencode.ai/inference/go`, not the pay-as-you-go host and not the free models
(those return 403 outside OpenCode). The key is a Console API key, stored as
`OPENCODE_GO_API_KEY`. Chat and Responses use the OpenAI base; Messages uses
`https://opencode.ai/inference/go/anthropic`. Each request sends `User-Agent: umsp/<version>`
and `x-opencode-session`. A client header with that name is kept; otherwise the gateway
hashes the model and the first message so one conversation stays on one session.
Go does not publish a balance or remaining-quota snapshot, so the provider sorts as
unmeasured.

## Command Code

`umsp provider add --vendor command-code` uses the Provider API at
`https://api.commandcode.ai/provider/v1`. The $1 Go plan does not include this API.
GOAT, Pro, Max, Team, and the Provider plan do. The key comes from Studio and is stored
as `COMMAND_CODE_API_KEY`. Chat, Responses, and Messages share that base. Each model
list row names `supported_endpoints`; a Claude model is Messages only, so a Chat client
is translated onto `/messages` instead of being sent to Chat Completions.

## Documentation

| Document | Contents |
|---|---|
| [docs/configuration.md](docs/configuration.md) | Config schema, CLI reference, provider/model/client workflows |
| [docs/operations.md](docs/operations.md) | Endpoints, metrics, ledger, exit codes, systemd, troubleshooting, measurements |
| [docs/protocols.md](docs/protocols.md) | Per-upstream quirks, failover, transparency, SSE, Responses, Messages |
| [docs/model-routing.md](docs/model-routing.md) | Logical models, backends, capacity classes, pinning, Grok quota |
| [docs/protocol-matrix.md](docs/protocol-matrix.md) | Cross-protocol translation matrix and lossy fields |
| [docs/quirks.md](docs/quirks.md) | Upstream quirk catalog |
| [CONTRIBUTING.md](CONTRIBUTING.md) / [SECURITY.md](SECURITY.md) | Development rules / vulnerability reporting |

## Development

```bash
make check          # fmt + clippy -D warnings + tests + English-only langcheck
./scripts/build-musl.sh
```

The repository is English-only (`AGENTS.md`). Translations of this page: `README.de.md`, `README.es.md`, `README.fr.md`, `README.ja.md`, `README.ko.md`, `README.pt.md`, `README.ru.md`, `README.zh-CN.md`, `README.zh-TW.md`.
Historical milestone snapshots live in `docs/internal/` and are not current behavior.

## License

[MIT](LICENSE) © 2026 YiLongMusk
