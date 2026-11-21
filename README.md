# UMSP — Universal Model Service Provider

[![CI](https://github.com/yilong-musk/umsp/actions/workflows/ci.yml/badge.svg)](https://github.com/yilong-musk/umsp/actions/workflows/ci.yml)
[![Rust](https://img.shields.io/badge/Rust-1.85+-000000?logo=rust&logoColor=white)](https://www.rust-lang.org)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

English | [Chinese (Simplified)](README.zh-CN.md)

A **single-binary, loopback-only** AI gateway. It exposes OpenAI Chat Completions, OpenAI Responses,
and Anthropic Messages on one port, then forwards each request to an upstream that speaks the same
protocol — or translates it when the backend cannot. No UI, database, Redis, or Docker.

- One `umsp` binary; config in `~/.config/umsp/config.toml`, keys in `~/.config/umsp/env` (0600),
  JSONL request ledger in `~/.local/share/umsp/`.
- Logical models: callers see one model id; each model has an ordered `backends` list.
- Capacity-aware selection: fresh usage first (U), then no data (X), then balance-only (B);
  a successful provider stays pinned for five hours.
- Per-provider circuit breaking, DeepSeek balance health, Grok weekly-quota polling.
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
publishes this archive and installs it on the configured host. See
[GitHub release deploy](docs/operations.md#github-release-deploy).

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

`Authorization: Bearer $UMSP_API_KEY` is equivalent to `x-api-key`.

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
- `provider add --vendor ID` takes only the vendor id and key; every add gets its own id.

Full schema, provider/model/client management, and all CLI examples:
[docs/configuration.md](docs/configuration.md).

## Common commands

```bash
umsp provider list|add|remove|test      # upstream instances; add uses the built-in vendor catalog
umsp login|logout grok                  # per-provider Grok device-code session
umsp model list|add|remove              # logical models and aliases
umsp model backend add|remove           # attach/detach a backend (--first = higher priority)
umsp client list|add|remove|rotate      # client keys (only SHA-256 stored)
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
| GET | `/v1/models` | Yes | Logical model registry |
| POST | `/v1/chat/completions` | Yes | Chat Completions, streaming and non-streaming |
| POST | `/v1/responses` | Yes | Responses, streaming and non-streaming |
| POST | `/v1/messages` | Yes | Anthropic Messages, streaming and non-streaming |
| POST | `/v1/messages/count_tokens` | Yes | Proxied where the backend supports it |
| HEAD | `/api/hello` | No | Claude Code connection warm-up |

Detailed endpoint behavior, error mapping, metrics, ledger fields, systemd unit, exit codes, and
troubleshooting: [docs/operations.md](docs/operations.md).

## Grok subscriptions

`umsp provider add --vendor grok` creates a provider backed by a device-code OAuth session at
`~/.config/umsp/sessions/<id>.json` (0600), one file per provider id. `umsp login <id>` performs the
sign-in.

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

The repository is English-only (`AGENTS.md`); `README.zh-CN.md` is the only translated document.
Historical milestone snapshots live in `docs/internal/` and are not current behavior.

## License

[MIT](LICENSE) © 2026 YiLongMusk
