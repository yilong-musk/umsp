# UMSP — Universal Model Service Provider

[![CI](https://github.com/yilong-musk/umsp/actions/workflows/ci.yml/badge.svg)](https://github.com/yilong-musk/umsp/actions/workflows/ci.yml)
[![Rust](https://img.shields.io/badge/Rust-1.85+-000000?logo=rust&logoColor=white)](https://www.rust-lang.org)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

[Deutsch](README.de.md) | [English](README.md) | Español | [Français](README.fr.md) | [日本語](README.ja.md) | [한국어](README.ko.md) | [Português](README.pt.md) | [Русский](README.ru.md) | [简体中文](README.zh-CN.md) | [繁體中文](README.zh-TW.md)

Una pasarela de IA de **un solo binario, solo en loopback**. Expone OpenAI Chat Completions, OpenAI Responses y Anthropic Messages en un puerto, y reenvía cada petición a un origen que hable el mismo protocolo, o lo traduce cuando el backend no puede. Sin UI, base de datos, Redis ni Docker.

- Un binario `umsp`; la configuración en `~/.config/umsp/config.toml`, las claves en `~/.config/umsp/env` (0600) y el libro de peticiones JSONL en `~/.local/share/umsp/`.
- Modelos lógicos: quien llama ve un id de modelo; cada modelo tiene una lista ordenada de `backends`.
- Selección según capacidad: primero el uso fresco (U), luego sin datos (X), luego solo saldo (B); un provider que tuvo éxito queda fijado cinco horas.
- Cortacircuitos por provider, salud del saldo de DeepSeek y sondeo de la cuota semanal de Grok. Qwen Token Plan no tiene ninguna de las dos, así que se queda en X.
- Las peticiones del cliente no revelan el provider; los errores de origen se asignan a errores neutros de la pasarela.

## Inicio rápido

### Instalación

Paquete de release (sin cadena Rust en el servidor):

```bash
tar -xzf umsp-<version>-x86_64-unknown-linux-musl.tar.gz
install -Dm755 umsp-<version>-x86_64-unknown-linux-musl/umsp ~/.local/bin/umsp
sha256sum -c umsp-<version>-x86_64-unknown-linux-musl.tar.gz.sha256
file ~/.local/bin/umsp                   # enlazado estáticamente
```

Enviar un tag anotado `vX.Y.Z` (el número debe coincidir con `version` en `Cargo.toml`) publica este archivo como GitHub Release, firmado con minisign ([`umsp.pub`](umsp.pub)). Instalarlo es cosa suya: el repositorio tampoco tiene acceso al host. Véase [Instalar un release](docs/operations.md#installing-a-release).

### Compilar desde el código

```bash
sudo apt-get install -y musl-tools
rustup target add x86_64-unknown-linux-musl
git clone https://github.com/yilong-musk/umsp.git && cd umsp
./scripts/build-musl.sh                  # la salida queda en dist/
```

### Ejecutar

```bash
umsp init                                # config.toml, env (0600), directorio de estado
$EDITOR ~/.config/umsp/env               # claves de origen, p. ej. DEEPSEEK_API_KEY=...
umsp client add --name default           # la clave de cliente se imprime una vez (umsp_...)
umsp check --json                        # tiene que salir en verde
umsp serve                               # imprime {"event":"ready",...}
```

Llamarlo desde otro terminal:

```bash
export UMSP_API_KEY=umsp_xxx
curl -s http://127.0.0.1:3460/v1/models -H "x-api-key: ***" | jq

curl -s http://127.0.0.1:3460/v1/chat/completions \
  -H "x-api-key: ***" -H 'Content-Type: application/json' \
  -d '{"model":"deepseek-v4.1-flash","messages":[{"role":"user","content":"Reply with only: OK"}]}' | jq
```

`Authorization: Bearer ***` equivale a `x-api-key`.

### Vinculación de dispositivo (una clave, un dispositivo)

Las claves de cliente usan vinculación de dispositivo por confianza en el primer uso (TOFU): una clave nueva no está vinculada, y la **primera petición autenticada** la ata a la IP de origen de esa petición. Toda petición posterior con esa clave debe venir de la misma IP o se rechaza con `403 device_mismatch` (el cuerpo del error nunca revela la IP vinculada). Las vinculaciones persisten en `state_dir/client-bindings.json` y sobreviven a los reinicios.

- Detrás de un proxy inverso, la IP real del cliente es la entrada más a la derecha de `X-Forwarded-For` escrita por el proxy de loopback de confianza; las entradas que pone el cliente se ignoran. Quien llama en local (loopback) se vincula a `127.0.0.1`.
- `umsp client list` muestra la IP vinculada de cada clave. `umsp client unbind <name>` borra una vinculación (la siguiente petición vuelve a vincular: úselo cuando cambie la IP de un dispositivo); `rotate` y `remove` la borran solos.
- Los dispositivos detrás del mismo NAT comparten una IP pública y no se distinguen; emita una clave por dispositivo para la atribución y la revocación por dispositivo.

### Límites del cliente

Cada clave puede llevar límites opcionales; no ponerlos significa sin límite. Se fijan en `umsp client add` y se cambian con `umsp client update <name>`:

- `--expires-in 30d` — pasado ese plazo la clave se rechaza con `401 key_expired` (`--no-expiry` en `update` lo quita).
- `--max-concurrency 1` — como mucho N (1-9) llamadas a modelo en vuelo; el resto recibe `429 concurrency_limit` (`--no-concurrency-limit` lo quita).
- `--budget-tokens 5000000` — presupuesto de tokens de por vida. Cada llamada reserva primero su tope de salida (el valor por defecto del modelo si la petición no pone ninguno) y se rechaza con `403 budget_exhausted` si no cabe; el uso real se carga después (`--reset-usage` / `--no-budget` en `update`).

Detalles: [docs/configuration.md](docs/configuration.md#client-limits).

## Configuración

`~/.config/umsp/config.toml` mínimo:

```toml
[server]
listen = "127.0.0.1:3460"
state_dir = "~/.local/share/umsp"

[[providers]]
id = "deepseek"
vendor = "deepseek"                       # catálogo integrado; de ahí salen endpoints y quirks
base_url = "https://api.deepseek.com"
anthropic_base_url = "https://api.deepseek.com/anthropic"
api_key_env = "DEEPSEEK_API_KEY"          # solo el nombre; el valor vive en env
protocols = ["chat", "responses", "messages"]
quirks = "deepseek"

[[models]]
id = "deepseek-v4.1-flash"                # quien llama solo conoce este id lógico
aliases = ["deepseek-flash"]
context_length = 1048000
max_completion_tokens = 128000
capabilities = ["text", "vision", "tools", "reasoning"]
backends = [
  { provider = "deepseek", upstream = "deepseek-flash" },
]

[[clients]]
name = "hermes"
key_hash = "sha256:..."                   # lo escribe `umsp client add`
key_prefix = "umsp_ab12cd34"
allowed_models = ["*"]
allowed_protocols = ["chat", "responses", "messages"]
```

- Las claves de origen nunca van en `config.toml`; allí solo están los nombres de `api_key_env`.
- Precedencia: `--config` > `$UMSP_CONFIG` > `~/.config/umsp/config.toml`; `$UMSP_LISTEN`, `$UMSP_STATE_DIR` y `$UMSP_ENV_FILE` sustituyen rutas sueltas.
- `/v1/models` contiene una entrada por modelo lógico y nunca expone providers. Cada 12 horas la pasarela lee la lista de modelos de cada provider y deduplica por id. El contexto, el límite de salida, el nombre visible y las capacidades salen de esa lista cuando el provider los envía, incluidos todos los escalones de contexto. Un solo `context_length` es el único escalón, o el mayor cuando la lista tiene varios. Un valor por defecto documentado rellena un campo que la lista dejó vacío y nunca sobrescribe un valor de origen. Una entrada `[[models]]` escrita a mano rellena lo que siga vacío. `umsp catalog refresh` lee de inmediato. `allowed_models = ["*"]` (o una lista vacía) muestra ese catálogo completo.
- `provider add --vendor ID` solo toma el id del proveedor y la clave; cada alta recibe su propio id.

Esquema completo, gestión de provider/model/client y todos los ejemplos de CLI: [docs/configuration.md](docs/configuration.md).

## Comandos habituales

```bash
umsp provider list|add|remove|test      # instancias de origen; add usa el catálogo integrado
umsp login|logout grok                  # sesión de código de dispositivo de Grok, por provider
umsp model list|add|remove              # modelos lógicos y alias
umsp model backend add|remove           # enganchar o soltar un backend (--first = más prioridad)
umsp client list|add|remove|rotate|unbind # claves de cliente (se guarda SHA-256) + vinculación TOFU
umsp env list|set|unset                 # claves de origen, enmascaradas
umsp usage summary|by-model|prune       # consulta el libro de peticiones
umsp doctor [--online]                  # diagnóstico; --online gasta unos pocos tokens
```

Todo subcomando admite `--json`. La tabla completa de subcomandos e indicadores está en [docs/configuration.md](docs/configuration.md).

## Endpoints HTTP

| Método | Ruta | Auth | Propósito |
|---|---|---|---|
| GET | `/healthz` | No | Estado consultable con curl: circuito, fallos, último resultado, `usage` fresco |
| GET | `/metrics` | No | Texto Prometheus, en memoria, solo loopback |
| GET | `/v1/models` | Sí | Registro de modelos lógicos (metadatos planos + sobre `meta`) |
| POST | `/v1/chat/completions` | Sí | Chat Completions, con y sin streaming |
| POST | `/v1/responses` | Sí | Responses, con y sin streaming |
| POST | `/v1/messages` | Sí | Anthropic Messages, con y sin streaming |
| POST | `/v1/messages/count_tokens` | Sí | Se hace de proxy donde el backend lo admite |
| HEAD | `/api/hello` | No | Calentamiento de la conexión de Claude Code |

Comportamiento de los endpoints, asignación de errores, métricas, campos del libro, unidad systemd, códigos de salida y resolución de problemas: [docs/operations.md](docs/operations.md).

## Suscripciones de Grok

`umsp provider add --vendor grok` crea un provider respaldado por una sesión OAuth de código de dispositivo en `{config_dir}/sessions/<id>.json`. `umsp provider add --vendor chatgpt` usa Sign in with ChatGPT (PKCE en `127.0.0.1`, uso del plan en `https://api.openai.com/v1/responses` con `store: false` y stream obligatorio). Ninguna de las dos concesiones es una clave de API, y las dos sesiones nunca comparten un refresh token. `umsp login <id>` inicia sesión en un provider; `umsp logout <id>` borra solo ese archivo.

> **Experimental / riesgo para la cuenta.** El soporte de Grok usa un cliente OAuth público de CLI y el endpoint de suscripción `cli-chat-proxy`. No es una integración oficial de la API de xAI: puede romperse sin aviso y puede infringir las condiciones de xAI, incluida la suspensión de la cuenta. Úselo solo con una cuenta que esté dispuesto a arriesgar.

La pasarela envía el `x-grok-client-version` exigido (se sustituye con `UMSP_GROK_CLI_VERSION` cuando xAI sube el mínimo). El uso semanal de la suscripción se lee del endpoint de facturación, no de las cabeceras efímeras `x-ratelimit-*`. Detalles: [docs/model-routing.md](docs/model-routing.md).

## Qwen Token Plan

`umsp provider add --vendor qwen-token-plan` usa el host de Qwen Token Plan que muestra la consola de la suscripción (`token-plan.maas.qianwenaiapi.com`), no el DashScope de pago por uso ni el host internacional `qwencloudapi.com`. Una clave `sk-` o el host de DashScope no descuentan este plan. Chat va a `https://token-plan.maas.qianwenaiapi.com/compatible-mode/v1`, Responses a `https://token-plan.maas.qianwenaiapi.com/compatible-mode/v1/responses` y Messages a `https://token-plan.maas.qianwenaiapi.com/apps/anthropic`. La variable de la clave es `QWEN_TOKEN_PLAN_API_KEY`. El plan está documentado para clientes interactivos. El host regional antiguo `token-plan.cn-beijing.maas.aliyuncs.com` todavía responde, pero no es la URL de la consola.

## OpenCode Go

`umsp provider add --vendor opencode-go` usa el host de la suscripción OpenCode Go `https://opencode.ai/inference/go`, no el host de pago por uso ni los modelos gratuitos (esos devuelven 403 fuera de OpenCode). La clave es una clave de API de la consola, guardada como `OPENCODE_GO_API_KEY`. Chat y Responses usan la base de OpenAI; Messages usa `https://opencode.ai/inference/go/anthropic`. Cada petición envía `User-Agent: umsp/<version>` y `x-opencode-session`. Si el cliente trae una cabecera con ese nombre, se conserva; si no, la pasarela hace un hash del modelo y del primer mensaje para que una conversación se quede en una sesión. Go no publica un saldo ni una instantánea de cuota restante, así que el provider se ordena como no medido.

## Command Code

`umsp provider add --vendor command-code` usa la Provider API en `https://api.commandcode.ai/provider/v1`. El plan Go de 1 $ al mes no incluye esta API. GOAT, Pro, Max, Team y el plan Provider sí. La clave sale de Studio y se guarda como `COMMAND_CODE_API_KEY`. Chat, Responses y Messages comparten esa base. Cada fila de la lista de modelos nombra `supported_endpoints`; un modelo Claude es solo Messages, así que un cliente Chat se traduce a `/messages` en lugar de enviarse a Chat Completions.

## Documentación

| Documento | Contenido |
|---|---|
| [docs/configuration.md](docs/configuration.md) | Esquema de configuración, referencia de CLI, flujos de provider/model/client |
| [docs/operations.md](docs/operations.md) | Endpoints, métricas, libro, códigos de salida, systemd, resolución de problemas, medidas |
| [docs/protocols.md](docs/protocols.md) | Peculiaridades por origen, conmutación, transparencia, SSE, Responses, Messages |
| [docs/model-routing.md](docs/model-routing.md) | Modelos lógicos, backends, clases de capacidad, fijación, cuota de Grok |
| [docs/protocol-matrix.md](docs/protocol-matrix.md) | Matriz de traducción entre protocolos y campos con pérdida |
| [docs/quirks.md](docs/quirks.md) | Catálogo de peculiaridades de origen |
| [CONTRIBUTING.md](CONTRIBUTING.md) / [SECURITY.md](SECURITY.md) | Reglas de desarrollo / aviso de vulnerabilidades |

## Desarrollo

```bash
make check          # fmt + clippy -D warnings + pruebas + comprobación de solo inglés
./scripts/build-musl.sh
```

El repositorio es solo en inglés (`AGENTS.md`). El inglés es la fuente de verdad; esta página es su traducción al español. Las instantáneas de hitos en `docs/internal/` son historia y no describen el comportamiento actual.

## License

[MIT](LICENSE) © 2026 YiLongMusk
