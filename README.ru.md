# UMSP — Universal Model Service Provider

[![CI](https://github.com/yilong-musk/umsp/actions/workflows/ci.yml/badge.svg)](https://github.com/yilong-musk/umsp/actions/workflows/ci.yml)
[![Rust](https://img.shields.io/badge/Rust-1.85+-000000?logo=rust&logoColor=white)](https://www.rust-lang.org)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

[English](README.md) | [简体中文](README.zh-CN.md) | [繁體中文](README.zh-TW.md) | [日本語](README.ja.md) | [한국어](README.ko.md) | [Español](README.es.md) | [Français](README.fr.md) | [Deutsch](README.de.md) | [Português](README.pt.md) | Русский

Шлюз ИИ: **один бинарник, только loopback**. На одном порту отдаёт OpenAI Chat Completions, OpenAI Responses и Anthropic Messages и пересылает каждый запрос upstream, который говорит тем же протоколом, — или переводит, если бэкенд не умеет. Без интерфейса, базы, Redis и Docker.

- Один бинарник `umsp`; конфиг в `~/.config/umsp/config.toml`, ключи в `~/.config/umsp/env` (0600), журнал запросов JSONL в `~/.local/share/umsp/`.
- Логические модели: вызывающий видит один id модели; у каждой модели упорядоченный список `backends`.
- Выбор по ёмкости: сначала свежее использование (U), затем нет данных (X), затем только баланс (B); успешный provider остаётся закреплённым пять часов.
- Размыкатель на каждый provider, здоровье баланса DeepSeek, опрос недельной квоты Grok. У Qwen Token Plan нет ни того ни другого, поэтому он остаётся в X.
- Запросы клиента не показывают provider; ошибки upstream отображаются в нейтральные ошибки шлюза.

## Быстрый старт

### Установка

Пакет релиза (цепочка Rust на сервере не нужна):

```bash
tar -xzf umsp-<version>-x86_64-unknown-linux-musl.tar.gz
install -Dm755 umsp-<version>-x86_64-unknown-linux-musl/umsp ~/.local/bin/umsp
sha256sum -c umsp-<version>-x86_64-unknown-linux-musl.tar.gz.sha256
file ~/.local/bin/umsp                   # статическая линковка
```

Аннотированный тег `vX.Y.Z` (номер должен совпасть с `version` в `Cargo.toml`) публикует этот архив как GitHub Release, подписанный minisign ([`umsp.pub`](umsp.pub)). Ставить его вам: у репозитория нет и доступа к хосту. См. [Установка релиза](docs/operations.md#installing-a-release).

### Сборка из исходников

```bash
sudo apt-get install -y musl-tools
rustup target add x86_64-unknown-linux-musl
git clone https://github.com/yilong-musk/umsp.git && cd umsp
./scripts/build-musl.sh                  # результат в dist/
```

### Запуск

```bash
umsp init                                # config.toml, env (0600), каталог состояния
$EDITOR ~/.config/umsp/env               # ключи upstream, например DEEPSEEK_API_KEY=...
umsp client add --name default           # ключ клиента печатается один раз (umsp_...)
umsp check --json                        # должно быть зелёным
umsp serve                               # печатает {"event":"ready",...}
```

Вызов из другого терминала:

```bash
export UMSP_API_KEY=umsp_xxx
curl -s http://127.0.0.1:3460/v1/models -H "x-api-key: ***" | jq

curl -s http://127.0.0.1:3460/v1/chat/completions \
  -H "x-api-key: ***" -H 'Content-Type: application/json' \
  -d '{"model":"deepseek-v4.1-flash","messages":[{"role":"user","content":"Reply with only: OK"}]}' | jq
```

`Authorization: Bearer ***` равнозначно `x-api-key`.

### Привязка устройства (один ключ, одно устройство)

Ключи клиента привязывают устройство при первом использовании (TOFU): новый ключ не привязан, и **первый аутентифицированный запрос** привязывает его к исходному IP этого запроса. Каждый следующий запрос с этим ключом должен прийти с того же IP, иначе он отвергается с `403 device_mismatch` (тело ошибки никогда не раскрывает привязанный IP). Привязки лежат в `state_dir/client-bindings.json` и переживают перезапуск.

- За обратным прокси настоящий IP клиента — самая правая запись `X-Forwarded-For`, которую записал доверенный loopback-прокси; записи, подставленные клиентом, игнорируются. Локальные (loopback) вызывающие привязываются к `127.0.0.1`.
- `umsp client list` показывает привязанный IP каждого ключа. `umsp client unbind <name>` снимает привязку (следующий запрос привяжет заново — когда у устройства сменился IP); `rotate` и `remove` снимают её сами.
- Устройства за одним NAT делят один публичный IP и неразличимы; выдавайте по ключу на устройство для учёта и отзыва по устройству.

### Лимиты клиента

У каждого ключа могут быть необязательные лимиты; не задано — значит без лимита. Ставятся в `umsp client add` и меняются через `umsp client update <name>`:

- `--expires-in 30d` — после этого срока ключ отвергается с `401 key_expired` (`--no-expiry` у `update` снимает срок).
- `--max-concurrency 1` — одновременно не больше N (1-9) вызовов модели; лишние получают `429 concurrency_limit` (`--no-concurrency-limit` снимает ограничение).
- `--budget-tokens 5000000` — пожизненный бюджет токенов. Каждый вызов сначала резервирует потолок вывода (значение модели по умолчанию, если запрос его не задал) и отвергается с `403 budget_exhausted`, если не помещается; фактический расход списывается потом (`--reset-usage` / `--no-budget` у `update`).

Подробности: [docs/configuration.md](docs/configuration.md#client-limits).

## Конфигурация

Минимальный `~/.config/umsp/config.toml`:

```toml
[server]
listen = "127.0.0.1:3460"
state_dir = "~/.local/share/umsp"

[[providers]]
id = "deepseek"
vendor = "deepseek"                       # встроенный каталог; конечные точки и quirks оттуда
base_url = "https://api.deepseek.com"
anthropic_base_url = "https://api.deepseek.com/anthropic"
api_key_env = "DEEPSEEK_API_KEY"          # только имя; значение живёт в env
protocols = ["chat", "responses", "messages"]
quirks = "deepseek"

[[models]]
id = "deepseek-v4.1-flash"                # вызывающий знает только этот логический id
aliases = ["deepseek-flash"]
context_length = 1048000
max_completion_tokens = 128000
capabilities = ["text", "vision", "tools", "reasoning"]
backends = [
  { provider = "deepseek", upstream = "deepseek-flash" },
]

[[clients]]
name = "hermes"
key_hash = "sha256:..."                   # пишет `umsp client add`
key_prefix = "umsp_ab12cd34"
allowed_models = ["*"]
allowed_protocols = ["chat", "responses", "messages"]
```

- Ключи upstream никогда не попадают в `config.toml`; там только имена `api_key_env`.
- Приоритет: `--config` > `$UMSP_CONFIG` > `~/.config/umsp/config.toml`; `$UMSP_LISTEN`, `$UMSP_STATE_DIR`, `$UMSP_ENV_FILE` перекрывают отдельные пути.
- `/v1/models` содержит одну запись на логическую модель и никогда не показывает provider. Каждые 12 часов шлюз читает список моделей каждого provider и убирает дубли по id. Контекст, предел вывода, отображаемое имя и возможности берутся из этого списка, когда provider их прислал, включая каждый ярус контекста. Одно `context_length` — единственный ярус или наибольший, если в списке их несколько. Документированное значение по умолчанию заполняет поле, которое список оставил пустым, и никогда не затирает значение upstream. Рукописная запись `[[models]]` заполняет то, что после этого всё ещё пусто. `umsp catalog refresh` читает сразу. `allowed_models = ["*"]` (или пустой список) показывает этот полный каталог.
- `provider add --vendor ID` принимает только id поставщика и ключ; каждое добавление получает свой id.

Полная схема, управление provider/model/client и все примеры CLI: [docs/configuration.md](docs/configuration.md).

## Частые команды

```bash
umsp provider list|add|remove|test      # экземпляры upstream; add берёт встроенный каталог
umsp login|logout grok                  # сессия кода устройства Grok, на каждый provider
umsp model list|add|remove              # логические модели и псевдонимы
umsp model backend add|remove           # прицепить или снять бэкенд (--first = выше приоритет)
umsp client list|add|remove|rotate|unbind # ключи клиента (хранится SHA-256) + привязка TOFU
umsp env list|set|unset                 # ключи upstream, маскируются
umsp usage summary|by-model|prune       # запрос журнала
umsp doctor [--online]                  # диагностика; --online тратит несколько токенов
```

Каждая подкоманда поддерживает `--json`. Полная таблица подкоманд и флагов — в [docs/configuration.md](docs/configuration.md).

## Конечные точки HTTP

| Метод | Путь | Auth | Назначение |
|---|---|---|---|
| GET | `/healthz` | Нет | Состояние, которое можно спросить curl: размыкатель, сбои, последний результат, свежий `usage` |
| GET | `/metrics` | Нет | Текст Prometheus, в памяти, только loopback |
| GET | `/v1/models` | Да | Реестр логических моделей (плоские метаданные + конверт `meta`) |
| POST | `/v1/chat/completions` | Да | Chat Completions, поток и без потока |
| POST | `/v1/responses` | Да | Responses, поток и без потока |
| POST | `/v1/messages` | Да | Anthropic Messages, поток и без потока |
| POST | `/v1/messages/count_tokens` | Да | Проксируется там, где бэкенд это умеет |
| HEAD | `/api/hello` | Нет | Прогрев соединения Claude Code |

Поведение конечных точек, отображение ошибок, метрики, поля журнала, юнит systemd, коды выхода и диагностика: [docs/operations.md](docs/operations.md).

## Подписки Grok

`umsp provider add --vendor grok` создаёт provider на сессии OAuth с кодом устройства в `{config_dir}/sessions/<id>.json`. `umsp provider add --vendor chatgpt` использует Sign in with ChatGPT (PKCE на `127.0.0.1`, использование плана на `https://api.openai.com/v1/responses` с `store: false` и обязательным потоком). Ни одно из двух разрешений не является ключом API, и две сессии никогда не делят refresh token. `umsp login <id>` входит в один provider; `umsp logout <id>` удаляет только этот файл.

> **Экспериментально / риск для аккаунта.** Поддержка Grok использует публичный OAuth-клиент CLI и конечную точку подписки `cli-chat-proxy`. Это не официальная интеграция API xAI: она может сломаться без предупреждения и может нарушать условия xAI, включая блокировку аккаунта. Используйте только с аккаунтом, которым готовы рискнуть.

Шлюз отправляет требуемый `x-grok-client-version` (перекрывается `UMSP_GROK_CLI_VERSION`, когда xAI поднимает минимум). Недельное использование подписки читается с конечной точки биллинга, а не из короткоживущих заголовков `x-ratelimit-*`. Подробности: [docs/model-routing.md](docs/model-routing.md).

## Qwen Token Plan

`umsp provider add --vendor qwen-token-plan` использует хост Qwen Token Plan из консоли подписки (`token-plan.maas.qianwenaiapi.com`), а не оплату по факту DashScope и не международный хост `qwencloudapi.com`. Ключ `sk-` или хост DashScope этот план не списывают. Chat идёт на `https://token-plan.maas.qianwenaiapi.com/compatible-mode/v1`, Responses на `https://token-plan.maas.qianwenaiapi.com/compatible-mode/v1/responses`, Messages на `https://token-plan.maas.qianwenaiapi.com/apps/anthropic`. Переменная ключа — `QWEN_TOKEN_PLAN_API_KEY`. План описан для интерактивных клиентов. Старый региональный хост `token-plan.cn-beijing.maas.aliyuncs.com` всё ещё отвечает, но это не URL консоли.

## OpenCode Go

`umsp provider add --vendor opencode-go` использует хост подписки OpenCode Go `https://opencode.ai/inference/go`, а не хост оплаты по факту и не бесплатные модели (те вне OpenCode возвращают 403). Ключ — это ключ API консоли, он хранится как `OPENCODE_GO_API_KEY`. Chat и Responses используют базу OpenAI; Messages использует `https://opencode.ai/inference/go/anthropic`. Каждый запрос шлёт `User-Agent: umsp/<version>` и `x-opencode-session`. Заголовок клиента с этим именем сохраняется; иначе шлюз хеширует модель и первое сообщение, чтобы один разговор оставался на одной сессии. Go не публикует баланс и снимок остатка квоты, поэтому provider сортируется как неизмеренный.

## Command Code

`umsp provider add --vendor command-code` использует Provider API по адресу `https://api.commandcode.ai/provider/v1`. План Go за 1 $ в месяц этот API не включает. GOAT, Pro, Max, Team и план Provider включают. Ключ берётся в Studio и хранится как `COMMAND_CODE_API_KEY`. Chat, Responses и Messages делят эту базу. Каждая строка списка моделей называет `supported_endpoints`; модель Claude — только Messages, поэтому клиент Chat переводится на `/messages`, а не отправляется в Chat Completions.

## Документация

| Документ | Содержание |
|---|---|
| [docs/configuration.md](docs/configuration.md) | Схема конфига, справочник CLI, сценарии provider/model/client |
| [docs/operations.md](docs/operations.md) | Конечные точки, метрики, журнал, коды выхода, systemd, диагностика, измерения |
| [docs/protocols.md](docs/protocols.md) | Quirks каждого upstream, переключение, прозрачность, SSE, Responses, Messages |
| [docs/model-routing.md](docs/model-routing.md) | Логические модели, бэкенды, классы ёмкости, закрепление, квота Grok |
| [docs/protocol-matrix.md](docs/protocol-matrix.md) | Матрица перевода между протоколами и поля с потерей |
| [docs/quirks.md](docs/quirks.md) | Каталог quirks upstream |
| [CONTRIBUTING.md](CONTRIBUTING.md) / [SECURITY.md](SECURITY.md) | Правила разработки / сообщение об уязвимостях |

## Разработка

```bash
make check          # fmt + clippy -D warnings + тесты + проверка «только английский»
./scripts/build-musl.sh
```

Текст репозитория только на английском (`AGENTS.md`). Английский — единственный источник истины; эта страница — его русский перевод. Снимки вех в `docs/internal/` — история, а не текущее поведение.

## License

[MIT](LICENSE) © 2026 YiLongMusk
