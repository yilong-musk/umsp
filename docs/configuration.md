# Configuration and CLI reference

This is the full reference for `config.toml` and the command-line interface. For a minimal example, see the [README](../README.md#configuration).

## Subcommands

Every subcommand supports `--json` (one line of valid JSON on stdout); `--config PATH` overrides the
config path globally.

| Command | Description |
|---|---|
| `umsp serve [--listen ADDR]` | Start the data plane; prints the ready line to stdout and flushes it; logs go to stderr |
| `umsp init [--state-dir PATH] [--force]` | Create config.toml, env (0600), and the state directory |
| `umsp config path\|show [--redact]\|validate` | Inspect/validate configuration |
| `umsp check [--json]` | Validate config + keys + state directory; exit code 10 on failure |
| `umsp doctor [--online] [--json]` | More detailed diagnostics; `--online` actually probes upstreams |
| `umsp vendor list` | List built-in vendors (id, endpoint, protocols, quirks, auth, number of configured instances) |
| `umsp provider list\|add\|remove\|test` | Manage upstream providers; `add --vendor ID` needs only the vendor id + key (or, for `grok`, no key), with endpoint/protocol from the built-in catalog, and assigns a unique id each time; `list` merges circuit-breaker/recent status from `/healthz` |
| `umsp login <provider>` | Device-code sign-in for one Grok subscription provider. Each id has its own session file |
| `umsp logout <provider>` | Delete that provider's session file |
| `umsp env list\|set\|unset` | Manage upstream keys in the env file (masked display only; `set` hides input on a TTY) |
| `umsp model list [--aliases]\|add\|remove` | Manage the local model registry (logical id + aliases) |
| `umsp model backend add\|remove` | Attach/detach a backend for an existing model (`--first` inserts at the front of the tie-break order) |
| `umsp client list\|add\|remove\|rotate` | Manage client keys (only SHA-256 stored) |
| `umsp key list\|add\|revoke` | Compatibility alias for `client` |
| `umsp usage summary\|by-client\|by-model\|by-protocol\|prune` | Aggregate the JSONL ledger |
| `umsp probe --provider ID [--model ID]` | Send one minimal request (chat) to a provider |
| `umsp call --model ID --prompt TEXT` | Call the upstream once directly (chat, non-streaming) |
| `umsp logs tail [--lines N] [--follow]` | View the request ledger |
| `umsp logs export --since 24h --out PATH` | Export the ledger |
| `umsp version [--json]` | Version + build target |
| `umsp completion bash\|zsh\|fish` | Generate a completion script |

Examples:

```bash
# Built-in vendor catalog: endpoints, protocols, and quirks are built in; you can only choose from the catalog
umsp vendor list

# Add a provider: supply only the vendor id and key (the key is read from stdin, not argv / history)
read -rsp 'DeepSeek key: ' K && echo
printf '%s\n' "$K" | umsp provider add --vendor deepseek

# The same vendor can be added repeatedly: each add gets its own id (deepseek, deepseek-2…),
# endpoints come from the catalog, and instances neither overwrite nor merge with each other
umsp provider add --vendor deepseek --no-key

# Grok subscription accounts are the same shape: one provider id, one session.
# `grok` and `grok-2` do not share a refresh token. Sign in each id on its own.
umsp provider add --vendor grok
umsp login grok
umsp provider add --vendor grok
umsp login grok-2
umsp model add --id grok-4.7 --backend grok:grok-4.7 --backend grok-2:grok-4.7

# Experimental / account risk: Grok support uses a public CLI OAuth client and the
# `cli-chat-proxy` subscription endpoint. It is not an official xAI API integration: it can
# break without notice and may violate xAI's terms of service, including account suspension.
# Use it only with an account you are willing to risk. Session tokens stay in
# sessions/<id>.json (0600) and are never written to logs or config.
# The gateway sends the required x-grok-client-version; if xAI raises the
# minimum, set UMSP_GROK_CLI_VERSION in the env file and restart.

# Attach another backend to an existing model (insert at the front of the tie-break order)
umsp model backend add claude-haiku-5.5 \
  --backend deepseek-2:deepseek-flash --first

# Keys in the env file are shown masked only
umsp env list

# One logical model, two upstream backends. Config order breaks ties; usage gates selection.
umsp model add --id deepseek-v4.1-flash \
  --backend open-design:deepseek-v4.1-flash \
  --backend deepseek:deepseek-flash \
  --alias deepseek-flash --context-length 1048000 --max-completion-tokens 128000 \
  --capability text,vision,tools,reasoning --display-name "DeepSeek V4.1 Flash"

umsp client add --name hermes --models 'deepseek-*,claude-*' --protocols chat --json
umsp usage summary --since 24h --json
umsp doctor --json
```

When the CLI modifies configuration it uses `toml_edit` to preserve comments and formatting, writes a
temporary file, then atomically replaces it with `rename`; if the process pointed to by
`state_dir/umsp.pid` is still alive, it automatically sends `SIGHUP` to trigger a hot reload (disable
with `--no-reload`).

---

## Configuration schema

`~/.config/umsp/config.toml`:

```toml
[server]
listen = "127.0.0.1:3460"          # loopback only; doctor warns on non-loopback
max_body_bytes = 52428800          # 50 MiB; 413 if exceeded
request_timeout_s = 600            # total duration for non-streaming
stream_idle_timeout_s = 300        # streaming: max gap between two upstream data chunks
stream_header_timeout_s = 300      # streaming: max wait for upstream response headers (OD)
stream_keepalive_s = 15            # streaming: silence period before sending ": keep-alive"
state_dir = "~/.local/share/umsp"   # ledger and pid file
responses_emit_event_lines = true  # whether Responses streaming adds `event: <type>` lines (default true)
log_retention_days = 30            # default retention for umsp usage prune

[[providers]]
id = "open-design"
vendor = "open-design"             # built-in vendor: endpoints and quirks come from the catalog
base_url = "https://amr-link.open-design.ai/v1"
anthropic_base_url = ""            # optional; Messages / count_tokens use it (M3 / M4a)
api_key_env = "OPEN_DESIGN_API_KEY" # variable name only; the value lives in the env file
protocols = ["chat", "responses", "messages"]
quirks = "opendesign"              # opendesign | deepseek | none

[[providers]]
id = "deepseek"
vendor = "deepseek"
base_url = "https://api.deepseek.com"
anthropic_base_url = "https://api.deepseek.com/anthropic"
api_key_env = "DEEPSEEK_API_KEY"
protocols = ["chat", "responses", "messages"]
quirks = "deepseek"                # also determines the count_tokens proxy

# The vendor catalog lives in code (src/vendors.rs): open-design, deepseek, grok, qwen-token-plan.
# `qwen-token-plan` is the Qwen Token Plan console host, not pay-as-you-go DashScope
# and not the international qwencloudapi.com host.
# Chat: https://token-plan.maas.qianwenaiapi.com/compatible-mode/v1
# Messages: https://token-plan.maas.qianwenaiapi.com/apps/anthropic
# The key variable is QWEN_TOKEN_PLAN_API_KEY. Responses is not offered.
# token-plan.cn-beijing.maas.aliyuncs.com still answers, but it is not the console URL.
# Adding a vendor requires changing that one place and rebuilding; an unknown vendor in the config fails validation.
# `auth = "oauth"` (grok) stores one session per provider id under sessions/<id>.json.
# `auth = "api_key"` (the default) still reads api_key_env from the env file.

[[models]]
id = "deepseek-v4.1-flash"          # logical model: callers only know this id
aliases = ["deepseek-flash"]        # compatibility with older clients
context_length = 1048000            # flattened context_length (Hermes reads this)
max_completion_tokens = 128000
capabilities = ["text", "vision", "tools", "reasoning"]
display_name = "DeepSeek V4.1 Flash"
backends = [                        # tie-break order; usage and balance gate health selection
  { provider = "open-design", upstream = "deepseek-v4.1-flash" },
  { provider = "deepseek",    upstream = "deepseek-flash" },
]
created = 1789026056

[[clients]]
name = "hermes"
key_hash = "sha256:<64 hex>"       # written by `umsp client add`
key_prefix = "umsp_ab12cd34"
allowed_models = ["deepseek-*", "claude-*"]   # glob, empty = *
allowed_protocols = ["chat", "responses"]  # chat / responses / messages
```

Conventions:

- Upstream keys live only in `~/.config/umsp/env`; the config contains only `api_key_env`; env file
  permissions must be 0600.
- Environment variables take precedence over the env file (both systemd `EnvironmentFile` and shell
  overrides apply).
- Config precedence: `--config` > `$UMSP_CONFIG` > `~/.config/umsp/config.toml`; `$UMSP_LISTEN`
  overrides the listen address, `$UMSP_STATE_DIR` overrides the state directory, `$UMSP_ENV_FILE`
  overrides the env file path.
- `/v1/models` shows exactly one entry per logical model, with no provider / upstream fields; view
  aliases with `umsp model list --aliases`.
