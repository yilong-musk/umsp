# Model registry and routing

Since 2026-11-20, models and providers are decoupled: **a model is a logical entity, and a provider is merely one of its backends**.

## Principles

- Callers only know model ids (for example `deepseek-v4.1-flash`); `/v1/models` contains exactly one entry per model.
- The gateway re-reads each provider's `GET {base_url}/models` every 12 hours and deduplicates by id. `umsp catalog refresh` reads immediately and resets that clock. The same id on several providers is one model, with one backend per provider in configuration order. Context length, max completion, display name, description, and capabilities (`text`, `vision`, `reasoning`) are taken from that list when the provider sends them (`metadata.context_limit` / `output_budget`, `context_window` / `max_output_tokens`, `name`, input modalities, reasoning flags). A later provider fills a field only when the earlier one left it empty. A list that is only ids, such as Qwen Token Plan, does not carry those fields. A documented default in `model_defaults` then fills a published context, output limit, or capability, and only when that field is still empty. Configured `[[models]]` entries fill whatever remains (and still supply aliases and any backend whose upstream name differs from the advertised id); they do not overwrite a value the list or the default already carried. A configured model that no provider has listed stays, so a provider without a model list does not disappear. Until the first successful read, `/v1/models` is the configured set. A failed read keeps that provider's previous list.
- A client's `allowed_models` still filters that list. An empty list, or `*`, shows the full catalog.
- provider / upstream never appear in model ids, `/v1/models`, or `allowed_models`.
- A model may have multiple backends. A provider is skipped when its circuit is open or a fresh usage window is exhausted. Providers with fresh usage (`U`) are tried first, then providers with neither usage nor a usable balance (`X`), then balance-only providers (`B`). The provider that serves the model sticks for five hours, or until it cannot take a request. The full ordering rules are in [Capacity-aware selection](#capacity-aware-selection).

## Capacity-aware selection

Selection reads in-memory capacity snapshots; it never calls an upstream on the request path.

- **U** — at least one fresh usage window. Ranked by the tightest remaining fraction, ascending. The second-tightest fraction breaks a tie, then configuration order. Only fractions are compared: request counts, token counts and currency amounts from different vendors are never compared as absolutes.
- **X** — no fresh usage and no fresh, usable balance.
- **B** — no fresh usage, but a fresh balance that is available and non-zero. Balance is a health gate, not a cross-provider ranking signal.

A fresh window was observed within five minutes and, when it has a reset time, still has not reset. Any fresh window at `0` makes the provider ineligible; if every backend is spent, the request gets `503 temporarily_unavailable` instead of burning a call on an exhausted account. A provider with a fresh usage window is not gated by a balance observation: usage wins.

Grok's four `x-ratelimit-*` response headers are short-lived throttles (they refill within minutes), **not** the weekly subscription pool. The health loop polls `GET {base_url}/billing?format=credits` every four minutes using the provider's session token and the numeric `x-userid` from `GET {base_url}/v1/user`. It converts `creditUsagePercent` to `remaining_fraction = 1 - used / 100` and `currentPeriod.end` to the reset time. The pool is unified across Grok products (`isUnifiedBillingUser`). If billing cannot be read, the provider has no fresh usage and falls to X or B by its balance data.

Qwen Token Plan stays in **X**. Its subscription quota is not on the model host: the `sk-sp-` key cannot read plan usage or a balance, and this gateway does not keep a console sign-in. Per-request token counts in a completion are ledger fields, not a usage window. With no snapshot, the provider is healthy while its circuit is closed and sorts after every provider that has fresh usage, ahead of balance-only providers.

## Configuration

```toml
[[providers]]
id = "open-design"                  # full name, no abbreviations like od
vendor = "open-design"              # built-in catalog entry (umsp vendor list)
base_url = "https://amr-link.open-design.ai/v1"
api_key_env = "OPEN_DESIGN_API_KEY"
protocols = ["chat", "responses", "messages"]
quirks = "opendesign"

[[providers]]
id = "deepseek"
vendor = "deepseek"
base_url = "https://api.deepseek.com"
anthropic_base_url = "https://api.deepseek.com/anthropic"
api_key_env = "DEEPSEEK_API_KEY"
protocols = ["chat", "responses", "messages"]
quirks = "deepseek"

[[models]]
id = "deepseek-v4.1-flash"
aliases = ["deepseek-flash"]
context_length = 1048000
max_completion_tokens = 128000
capabilities = ["text", "vision", "tools", "reasoning"]
display_name = "DeepSeek V4.1 Flash"
backends = [
  { provider = "open-design", upstream = "deepseek-v4.1-flash" },
  { provider = "deepseek",    upstream = "deepseek-flash" },
]
```

## Maintaining via CLI (no manual TOML edits)

```bash
umsp vendor list                                   # built-in vendors: id, endpoint, protocols, quirks
umsp provider add --vendor deepseek                # only the vendor id and key (key read from stdin)
umsp provider add --vendor deepseek --id work      # optional id hint, still uniquified

umsp model backend add deepseek-v4.1-flash --backend deepseek-personal:deepseek-flash --first
umsp model backend remove deepseek-v4.1-flash --provider open-design
umsp env list          # masked + source (file / process)
umsp env set OTHER_KEY --stdin
umsp env unset OTHER_KEY
```

## Grok subscription accounts

A Grok provider does not use `api_key_env`. `auth = "oauth"` means one subscription session, stored at `{config_dir}/sessions/<provider-id>.json` (mode 0600). Adding the vendor again allocates `grok-2`, `grok-3`, and each id has its own file, its own circuit, and its own place in a model's `backends` list. Refreshing one account cannot rotate another account's token.

```bash
umsp provider add --vendor grok
umsp login grok
umsp provider add --vendor grok
umsp login grok-2
umsp model add --id grok-4.7 --backend grok:grok-4.7 --backend grok-2:grok-4.7
```

The Grok data plane speaks Responses only and adds the CLI proxy headers (`X-XAI-Token-Auth: xai-grok-cli`, `x-grok-client-version`, `x-grok-model-override`) on the way up. `x-grok-client-version` defaults to 1.0.46; override it with `UMSP_GROK_CLI_VERSION` in the env file when xAI raises the minimum. API-key providers are unchanged.

## ChatGPT subscription accounts

A ChatGPT provider also uses `auth = "oauth"`, but the grant is Sign in with ChatGPT (PKCE, loopback `127.0.0.1`, callback `/auth/callback`). It does not share a refresh token with a Grok session. The data plane is Responses on `https://api.openai.com/v1`: every request sets `store` to false and forces a stream, then the gateway returns the completed response as one JSON object. On a host without a browser, forward the printed callback port and open the URL elsewhere.

```bash
umsp provider add --vendor chatgpt
umsp login chatgpt
```

- **Vendors are built in**: only ids listed by `umsp vendor list` can be added; endpoint, protocols, and quirks are determined by `src/vendors.rs`, and `provider add` does not accept a custom base_url; an unknown vendor fails outright (exit code 2). Adding a vendor = adding one record in `src/vendors.rs` (and keeping the references valid in the `config.rs` validation).
- **A provider is an independent instance**: every `provider add --vendor X` allocates a unique id (`X`, `X-2`, `X-3`…). The same vendor / same key can be added multiple times; each entry has its own id, circuit state, and metric labels, and none overwrites another. Without `--api-key-env` it defaults to `<ID>_API_KEY`; `--id` is only a readability hint.
- `provider list` also probes the local `/healthz` once and shows each provider's `circuit` / `failures` / `last_status`; when the gateway is not running it notes `gateway: not running`, the circuit column shows `-`, and the command itself does not fail.
- `model backend add` appends to the end of the tie-break order by default; `--first` inserts at the front.
- `model backend remove --provider` removes all backends of that provider; **removing the last backend is refused**.
- `env set` hides input on a terminal; piped input (`printf … | umsp env set K`) is suitable for scripts.
- After changing the env file you must restart `umsp serve` (env is read only at startup; SIGHUP reloads config only).

## Resolution and routing

1. The request's `model` is first matched exactly against `id`, then against aliases; neither → 404 `model_not_found`.
2. Candidate backends = entries in `backends` whose provider exists and speaks at least one protocol. Providers whose circuit is open or whose fresh usage window is exhausted are skipped. The rest are grouped U > X > B; U groups are ranked by remaining fraction, then configuration order. A live pin for the model is tried first for five hours. `count_tokens` still requires a Messages backend that can proxy it, and it does not read or write that pin. The attempt uses the client protocol when the provider lists it, and otherwise translates into the first protocol in that provider's list.
3. Try that order; only retryable failures (connection errors, timeouts, 429, 5xx, 402) move on to the next backend, while 400/401/403/404/422 return immediately. A retryable failure of one upstream still tries a later backend of the same provider. 402 and an open circuit skip the rest of that provider.
4. Each backend is tried at most once; the list is linear, so no loop exists.
5. Switching happens only before any byte has been sent to the client; once SSE has returned 200 there is no further switching.
6. All backends unavailable (all circuit-broken or all usage-exhausted) → 503 `temporarily_unavailable`; a model with no usable backend → 400 `no_backend_for_protocol`. A registered model is not rejected only because a provider omits the client's protocol.
7. On each attempt the request body's `model` is rewritten to that backend's `upstream`; provider-specific quirks are applied per backend as well.

## Validation (`umsp check` / `umsp validate`)

- Errors: model id empty/duplicate, no `backends`, backend provider does not exist, `upstream` empty.
- Warnings: the same model listing the same provider twice, duplicate `(provider, upstream)`, missing `context_length` / `max_completion_tokens`, an alias written as its own id.
- Error level: an alias colliding with any model id, an alias shared by two models.
- provider ids must be full-name slugs (lowercase letters/digits/`.`/`-`); a 2-character id (such as `od`) triggers the `provider_id_short` warning — once there are many providers in the same space, abbreviations are bound to collide.

## Observability

- `/v1/models`: one model = one object, with fields `id / object / owned_by="umsp" / created / context_length / context_lengths / max_completion_tokens / capabilities / display_name / description`; no provider, upstream, or backends. `context_lengths` lists every tier the upstream published, ascending. `context_length` is that only tier, or the largest when the upstream publishes more than one (Grok's `context_window` plus `context_windows`).
- `/metrics`: the `model` label is the logical model id, `provider` is the backend that actually serves.
- Ledger: `requested_model` stores the logical model id, `provider` / `upstream_model` store the actual backend, `fallbacks` is the number of switches, and `quirks` carries `fallback_from:<preferred provider>`.
- Response headers: the client only ever sees `x-umsp-request-id`; provider, upstream model name, and upstream headers never leave the gateway.
- Admin view: `umsp model list --aliases` shows each model's `backends` and aliases (local only, no HTTP).

## Differences from the old design

| Old | New |
|---|---|
| model id `provider/upstream` | logical id, no provider prefix |
| `[[models]] provider` + `upstream` | `backends = [{ provider, upstream }, …]` |
| `fallbacks = ["<model-id>"]`, at most 1 hop | backend list, selected then tried once each |
| `/v1/models` with `provider` / `upstream_model` | no provider-related fields exposed |
| `allowed_models = ["od/*"]` | `allowed_models = ["deepseek-*", "claude-*"]` |
| provider id may use the abbreviation `od` | must be a full-name slug (`open-design`) |
