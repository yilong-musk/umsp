# UMSP — Universal Model Service Provider

[![CI](https://github.com/yilong-musk/umsp/actions/workflows/ci.yml/badge.svg)](https://github.com/yilong-musk/umsp/actions/workflows/ci.yml)
[![Rust](https://img.shields.io/badge/Rust-1.85+-000000?logo=rust&logoColor=white)](https://www.rust-lang.org)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Deutsch | [English](README.md) | [Español](README.es.md) | [Français](README.fr.md) | [日本語](README.ja.md) | [한국어](README.ko.md) | [Português](README.pt.md) | [Русский](README.ru.md) | [简体中文](README.zh-CN.md) | [繁體中文](README.zh-TW.md)

Ein KI-Gateway als **ein Binary, nur Loopback**. Es bietet OpenAI Chat Completions, OpenAI Responses und Anthropic Messages auf einem Port und leitet jede Anfrage an einen Upstream weiter, der dasselbe Protokoll spricht — oder übersetzt sie, wenn das Backend das nicht kann. Keine Oberfläche, keine Datenbank, kein Redis, kein Docker.

- Ein Binary `umsp`; Konfiguration in `~/.config/umsp/config.toml`, Schlüssel in `~/.config/umsp/env` (0600), JSONL-Anfragebuch in `~/.local/share/umsp/`.
- Logische Modelle: Aufrufer sehen eine Modell-Id; jedes Modell hat eine geordnete Liste `backends`.
- Auswahl nach Kapazität: zuerst frische Nutzung (U), dann keine Daten (X), dann nur Guthaben (B); ein erfolgreicher Provider bleibt fünf Stunden festgenagelt.
- Sicherung je Provider, DeepSeek-Guthaben, Abruf des wöchentlichen Grok-Kontingents. Qwen Token Plan hat keines von beiden und bleibt deshalb in X.
- Client-Anfragen verraten den Provider nicht; Upstream-Fehler werden auf neutrale Gateway-Fehler abgebildet.

## Schnellstart

### Installation

Release-Paket (keine Rust-Toolchain auf dem Server):

```bash
tar -xzf umsp-<version>-x86_64-unknown-linux-musl.tar.gz
install -Dm755 umsp-<version>-x86_64-unknown-linux-musl/umsp ~/.local/bin/umsp
sha256sum -c umsp-<version>-x86_64-unknown-linux-musl.tar.gz.sha256
file ~/.local/bin/umsp                   # statisch gelinkt
```

Ein annotierter Tag `vX.Y.Z` (die Zahl muss `version` in `Cargo.toml` entsprechen) veröffentlicht dieses Archiv als GitHub Release, signiert mit minisign ([`umsp.pub`](umsp.pub)). Installieren tun Sie selbst: das Repository hat auch keinen Host-Zugriff. Siehe [Ein Release installieren](docs/operations.md#installing-a-release).

### Aus dem Quelltext bauen

```bash
sudo apt-get install -y musl-tools
rustup target add x86_64-unknown-linux-musl
git clone https://github.com/yilong-musk/umsp.git && cd umsp
./scripts/build-musl.sh                  # Ausgabe in dist/
```

### Starten

```bash
umsp init                                # config.toml, env (0600), Zustandsverzeichnis
$EDITOR ~/.config/umsp/env               # Upstream-Schlüssel, z. B. DEEPSEEK_API_KEY=...
umsp client add --name default           # der Client-Schlüssel wird einmal gedruckt (umsp_...)
umsp check --json                        # muss grün sein
umsp serve                               # druckt {"event":"ready",...}
```

Aus einem anderen Terminal aufrufen:

```bash
export UMSP_API_KEY=umsp_xxx
curl -s http://127.0.0.1:3460/v1/models -H "x-api-key: ***" | jq

curl -s http://127.0.0.1:3460/v1/chat/completions \
  -H "x-api-key: ***" -H 'Content-Type: application/json' \
  -d '{"model":"deepseek-v4.1-flash","messages":[{"role":"user","content":"Reply with only: OK"}]}' | jq
```

`Authorization: Bearer ***` ist gleichwertig mit `x-api-key`.

### Gerätebindung (ein Schlüssel, ein Gerät)

Client-Schlüssel binden das Gerät beim ersten Gebrauch (TOFU): ein neuer Schlüssel ist ungebunden, und die **erste authentisierte Anfrage** bindet ihn an die Quell-IP dieser Anfrage. Jede spätere Anfrage mit diesem Schlüssel muss von derselben IP kommen, sonst wird sie mit `403 device_mismatch` abgelehnt (der Fehlertext verrät die gebundene IP nie). Bindungen liegen in `state_dir/client-bindings.json` und überleben Neustarts.

- Hinter einem Reverse-Proxy ist die echte Client-IP der rechteste `X-Forwarded-For`-Eintrag, den der vertraute Loopback-Proxy geschrieben hat; vom Client gelieferte Einträge werden ignoriert. Lokale (Loopback-)Aufrufer binden an `127.0.0.1`.
- `umsp client list` zeigt die gebundene IP jedes Schlüssels. `umsp client unbind <name>` löscht eine Bindung (die nächste Anfrage bindet neu — benutzen, wenn sich die IP eines Geräts ändert); `rotate` und `remove` löschen sie von selbst.
- Geräte hinter demselben NAT teilen eine öffentliche IP und sind nicht unterscheidbar; geben Sie je Gerät einen Schlüssel aus, für Zuordnung und Widerruf je Gerät.

### Client-Grenzen

Jeder Schlüssel kann optionale Grenzen tragen; ungesetzt heißt unbegrenzt. Setzen mit `umsp client add`, ändern mit `umsp client update <name>`:

- `--expires-in 30d` — danach wird der Schlüssel mit `401 key_expired` abgelehnt (`--no-expiry` bei `update` nimmt sie weg).
- `--max-concurrency 1` — höchstens N (1-9) Modellaufrufe gleichzeitig; mehr bekommen `429 concurrency_limit` (`--no-concurrency-limit` nimmt sie weg).
- `--budget-tokens 5000000` — Token-Budget auf Lebenszeit. Jeder Aufruf reserviert zuerst seine Ausgabeobergrenze (den Modellstandard, wenn die Anfrage keine setzt) und wird mit `403 budget_exhausted` abgelehnt, wenn sie nicht passt; der echte Verbrauch wird danach gebucht (`--reset-usage` / `--no-budget` bei `update`).

Einzelheiten: [docs/configuration.md](docs/configuration.md#client-limits).

## Konfiguration

Minimales `~/.config/umsp/config.toml`:

```toml
[server]
listen = "127.0.0.1:3460"
state_dir = "~/.local/share/umsp"

[[providers]]
id = "deepseek"
vendor = "deepseek"                       # eingebauter Katalog; Endpunkte und Quirks kommen daher
base_url = "https://api.deepseek.com"
anthropic_base_url = "https://api.deepseek.com/anthropic"
api_key_env = "DEEPSEEK_API_KEY"          # nur der Name; der Wert liegt in env
protocols = ["chat", "responses", "messages"]
quirks = "deepseek"

[[models]]
id = "deepseek-v4.1-flash"                # Aufrufer kennen nur diese logische Id
aliases = ["deepseek-flash"]
context_length = 1048000
max_completion_tokens = 128000
capabilities = ["text", "vision", "tools", "reasoning"]
backends = [
  { provider = "deepseek", upstream = "deepseek-flash" },
]

[[clients]]
name = "hermes"
key_hash = "sha256:..."                   # geschrieben von `umsp client add`
key_prefix = "umsp_ab12cd34"
allowed_models = ["*"]
allowed_protocols = ["chat", "responses", "messages"]
```

- Upstream-Schlüssel stehen nie in `config.toml`; dort stehen nur `api_key_env`-Namen.
- Vorrang: `--config` > `$UMSP_CONFIG` > `~/.config/umsp/config.toml`; `$UMSP_LISTEN`, `$UMSP_STATE_DIR`, `$UMSP_ENV_FILE` überschreiben einzelne Pfade.
- `/v1/models` enthält einen Eintrag je logischem Modell und legt nie Provider offen. Alle 12 Stunden liest das Gateway die Modellliste jedes Providers und entfernt Duplikate nach Id. Kontext, Ausgabegrenze, Anzeigename und Fähigkeiten kommen aus dieser Liste, wenn der Provider sie schickt, einschließlich jeder Kontextstufe. Ein einzelnes `context_length` ist die einzige Stufe oder die größte, wenn die Liste mehrere hat. Ein dokumentierter Standard füllt ein Feld, das die Liste leer ließ, und überschreibt nie einen Upstream-Wert. Ein handgeschriebener `[[models]]`-Eintrag füllt, was danach noch leer ist. `umsp catalog refresh` liest sofort. `allowed_models = ["*"]` (oder eine leere Liste) zeigt diesen vollen Katalog.
- `provider add --vendor ID` nimmt nur die Anbieter-Id und den Schlüssel; jedes Hinzufügen bekommt eine eigene Id.

Volles Schema, Verwaltung von Provider/Modell/Client und alle CLI-Beispiele: [docs/configuration.md](docs/configuration.md).

## Häufige Befehle

```bash
umsp provider list|add|remove|test      # Upstream-Instanzen; add nutzt den eingebauten Katalog
umsp login|logout grok                  # Grok-Gerätecode-Sitzung je Provider
umsp model list|add|remove              # logische Modelle und Aliase
umsp model backend add|remove           # ein Backend an- oder abhängen (--first = höhere Priorität)
umsp client list|add|remove|rotate|unbind # Client-Schlüssel (SHA-256 gespeichert) + TOFU-Gerätebindung
umsp env list|set|unset                 # Upstream-Schlüssel, maskiert
umsp usage summary|by-model|prune       # das Anfragebuch abfragen
umsp doctor [--online]                  # Diagnose; --online verbraucht ein paar Tokens
```

Jeder Unterbefehl unterstützt `--json`. Die volle Tabelle der Unterbefehle und Flags steht in [docs/configuration.md](docs/configuration.md).

## HTTP-Endpunkte

| Methode | Pfad | Auth | Zweck |
|---|---|---|---|
| GET | `/healthz` | Nein | Mit curl lesbarer Zustand: Sicherung, Fehler, letztes Ergebnis, frisches `usage` |
| GET | `/metrics` | Nein | Prometheus-Text, im Speicher, nur Loopback |
| GET | `/v1/models` | Ja | Register der logischen Modelle (flache Metadaten + `meta`-Umschlag) |
| POST | `/v1/chat/completions` | Ja | Chat Completions, strömend und nicht strömend |
| POST | `/v1/responses` | Ja | Responses, strömend und nicht strömend |
| POST | `/v1/messages` | Ja | Anthropic Messages, strömend und nicht strömend |
| POST | `/v1/messages/count_tokens` | Ja | Weitergereicht, wo das Backend es kann |
| HEAD | `/api/hello` | Nein | Verbindungsaufwärmen für Claude Code |

Verhalten der Endpunkte, Fehlerabbildung, Metriken, Buchfelder, systemd-Unit, Exit-Codes und Fehlersuche: [docs/operations.md](docs/operations.md).

## Grok-Abos

`umsp provider add --vendor grok` legt einen Provider an, der auf einer Gerätecode-OAuth-Sitzung in `{config_dir}/sessions/<id>.json` steht. `umsp provider add --vendor chatgpt` nutzt Sign in with ChatGPT (PKCE auf `127.0.0.1`, Plannutzung auf `https://api.openai.com/v1/responses` mit `store: false` und Pflicht-Stream). Keine der beiden Erteilungen ist ein API-Schlüssel, und die beiden Sitzungen teilen nie ein Refresh-Token. `umsp login <id>` meldet einen Provider an; `umsp logout <id>` löscht nur diese Datei.

> **Experimentell / Kontorisiko.** Die Grok-Unterstützung nutzt einen öffentlichen CLI-OAuth-Client und den Abo-Endpunkt `cli-chat-proxy`. Das ist keine offizielle xAI-API-Anbindung: sie kann ohne Ankündigung brechen und gegen die Nutzungsbedingungen von xAI verstoßen, einschließlich Kontosperre. Nur mit einem Konto verwenden, dessen Risiko Sie tragen.

Das Gateway sendet das nötige `x-grok-client-version` (mit `UMSP_GROK_CLI_VERSION` überschreiben, wenn xAI das Minimum anhebt). Die wöchentliche Abonutzung kommt vom Abrechnungsendpunkt, nicht von den kurzlebigen `x-ratelimit-*`-Headern. Einzelheiten: [docs/model-routing.md](docs/model-routing.md).

## Qwen Token Plan

`umsp provider add --vendor qwen-token-plan` nutzt den Qwen-Token-Plan-Host aus der Abo-Konsole (`token-plan.maas.qianwenaiapi.com`), nicht das Pay-as-you-go-DashScope und nicht den internationalen Host `qwencloudapi.com`. Ein `sk-`-Schlüssel oder der DashScope-Host bucht diesen Plan nicht ab. Chat geht an `https://token-plan.maas.qianwenaiapi.com/compatible-mode/v1`, Responses an `https://token-plan.maas.qianwenaiapi.com/compatible-mode/v1/responses` und Messages an `https://token-plan.maas.qianwenaiapi.com/apps/anthropic`. Die Schlüsselvariable ist `QWEN_TOKEN_PLAN_API_KEY`. Der Plan ist für interaktive Clients dokumentiert. Der ältere regionale Host `token-plan.cn-beijing.maas.aliyuncs.com` antwortet noch, ist aber nicht die Konsolen-URL.

## OpenCode Go

`umsp provider add --vendor opencode-go` nutzt den OpenCode-Go-Abo-Host `https://opencode.ai/inference/go`, nicht den Pay-as-you-go-Host und nicht die freien Modelle (die geben außerhalb von OpenCode 403). Der Schlüssel ist ein Console-API-Schlüssel, gespeichert als `OPENCODE_GO_API_KEY`. Chat und Responses nutzen die OpenAI-Basis; Messages nutzt `https://opencode.ai/inference/go/anthropic`. Jede Anfrage sendet `User-Agent: umsp/<version>` und `x-opencode-session`. Ein Client-Header dieses Namens bleibt; sonst hasht das Gateway Modell und erste Nachricht, damit ein Gespräch auf einer Sitzung bleibt. Go veröffentlicht weder Guthaben noch eine Restkontingent-Momentaufnahme, deshalb sortiert der Provider als ungemessen.

## Command Code

`umsp provider add --vendor command-code` nutzt die Provider API unter `https://api.commandcode.ai/provider/v1`. Der Go-Plan für 1 $ im Monat enthält diese API nicht. GOAT, Pro, Max, Team und der Provider-Plan schon. Der Schlüssel kommt aus Studio und wird als `COMMAND_CODE_API_KEY` gespeichert. Chat, Responses und Messages teilen diese Basis. Jede Zeile der Modellliste nennt `supported_endpoints`; ein Claude-Modell ist nur Messages, deshalb wird ein Chat-Client auf `/messages` übersetzt, statt an Chat Completions geschickt zu werden.

## Dokumentation

| Dokument | Inhalt |
|---|---|
| [docs/configuration.md](docs/configuration.md) | Konfigurationsschema, CLI-Referenz, Abläufe für Provider/Modell/Client |
| [docs/operations.md](docs/operations.md) | Endpunkte, Metriken, Buch, Exit-Codes, systemd, Fehlersuche, Messungen |
| [docs/protocols.md](docs/protocols.md) | Quirks je Upstream, Failover, Transparenz, SSE, Responses, Messages |
| [docs/model-routing.md](docs/model-routing.md) | Logische Modelle, Backends, Kapazitätsklassen, Pinning, Grok-Kontingent |
| [docs/protocol-matrix.md](docs/protocol-matrix.md) | Übersetzungsmatrix zwischen Protokollen und verlustbehaftete Felder |
| [docs/quirks.md](docs/quirks.md) | Katalog der Upstream-Quirks |
| [CONTRIBUTING.md](CONTRIBUTING.md) / [SECURITY.md](SECURITY.md) | Entwicklungsregeln / Meldung von Schwachstellen |

## Entwicklung

```bash
make check          # fmt + clippy -D warnings + Tests + Sprachprüfung nur Englisch
./scripts/build-musl.sh
```

Das Repository ist nur auf Englisch (`AGENTS.md`). Englisch ist die maßgebliche Fassung; diese Seite ist die deutsche Übersetzung. Meilenstein-Schnappschüsse in `docs/internal/` sind Historie und nicht das aktuelle Verhalten.

## License

[MIT](LICENSE) © 2026 YiLongMusk
