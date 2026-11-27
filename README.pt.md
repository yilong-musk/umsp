# UMSP — Universal Model Service Provider

[![CI](https://github.com/yilong-musk/umsp/actions/workflows/ci.yml/badge.svg)](https://github.com/yilong-musk/umsp/actions/workflows/ci.yml)
[![Rust](https://img.shields.io/badge/Rust-1.85+-000000?logo=rust&logoColor=white)](https://www.rust-lang.org)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

[English](README.md) | [简体中文](README.zh-CN.md) | [繁體中文](README.zh-TW.md) | [日本語](README.ja.md) | [한국어](README.ko.md) | [Español](README.es.md) | [Français](README.fr.md) | [Deutsch](README.de.md) | Português | [Русский](README.ru.md)

Uma porta de IA de **binário único, só em loopback**. Expõe OpenAI Chat Completions, OpenAI Responses e Anthropic Messages numa porta e reencaminha cada pedido para uma origem que fale o mesmo protocolo — ou traduz quando o backend não consegue. Sem interface, base de dados, Redis ou Docker.

- Um binário `umsp`; configuração em `~/.config/umsp/config.toml`, chaves em `~/.config/umsp/env` (0600), livro de pedidos JSONL em `~/.local/share/umsp/`.
- Modelos lógicos: quem chama vê um id de modelo; cada modelo tem uma lista ordenada de `backends`.
- Escolha segundo a capacidade: primeiro o uso fresco (U), depois sem dados (X), depois só saldo (B); um provider que teve sucesso fica fixo cinco horas.
- Disjuntor por provider, saúde do saldo DeepSeek, sondagem da quota semanal do Grok. O Qwen Token Plan não tem nenhuma das duas, por isso fica em X.
- Os pedidos do cliente não revelam o provider; os erros de origem mapeiam-se para erros neutros da porta.

## Início rápido

### Instalação

Pacote de release (sem cadeia Rust no servidor):

```bash
tar -xzf umsp-<version>-x86_64-unknown-linux-musl.tar.gz
install -Dm755 umsp-<version>-x86_64-unknown-linux-musl/umsp ~/.local/bin/umsp
sha256sum -c umsp-<version>-x86_64-unknown-linux-musl.tar.gz.sha256
file ~/.local/bin/umsp                   # ligado estaticamente
```

Enviar um tag anotado `vX.Y.Z` (o número tem de igualar `version` em `Cargo.toml`) publica este arquivo como GitHub Release, assinado com minisign ([`umsp.pub`](umsp.pub)). Instalá-lo é consigo: o repositório também não tem acesso ao anfitrião. Ver [Instalar um release](docs/operations.md#installing-a-release).

### Compilar a partir do código

```bash
sudo apt-get install -y musl-tools
rustup target add x86_64-unknown-linux-musl
git clone https://github.com/yilong-musk/umsp.git && cd umsp
./scripts/build-musl.sh                  # saída em dist/
```

### Executar

```bash
umsp init                                # config.toml, env (0600), diretório de estado
$EDITOR ~/.config/umsp/env               # chaves de origem, p. ex. DEEPSEEK_API_KEY=...
umsp client add --name default           # a chave de cliente imprime-se uma vez (umsp_...)
umsp check --json                        # tem de ficar verde
umsp serve                               # imprime {"event":"ready",...}
```

Chamar a partir de outro terminal:

```bash
export UMSP_API_KEY=umsp_xxx
curl -s http://127.0.0.1:3460/v1/models -H "x-api-key: ***" | jq

curl -s http://127.0.0.1:3460/v1/chat/completions \
  -H "x-api-key: ***" -H 'Content-Type: application/json' \
  -d '{"model":"deepseek-v4.1-flash","messages":[{"role":"user","content":"Reply with only: OK"}]}' | jq
```

`Authorization: Bearer ***` equivale a `x-api-key`.

### Ligação de dispositivo (uma chave, um dispositivo)

As chaves de cliente usam ligação de dispositivo por confiança no primeiro uso (TOFU): uma chave nova não está ligada, e o **primeiro pedido autenticado** liga-a ao IP de origem desse pedido. Todo o pedido posterior com essa chave tem de vir do mesmo IP, senão é recusado com `403 device_mismatch` (o corpo do erro nunca revela o IP ligado). As ligações persistem em `state_dir/client-bindings.json` e sobrevivem a reinícios.

- Atrás de um proxy inverso, o IP real do cliente é a entrada mais à direita de `X-Forwarded-For` escrita pelo proxy de loopback de confiança; as entradas fornecidas pelo cliente são ignoradas. Quem chama em local (loopback) liga-se a `127.0.0.1`.
- `umsp client list` mostra o IP ligado de cada chave. `umsp client unbind <name>` apaga uma ligação (o pedido seguinte volta a ligar — use quando o IP de um dispositivo mudar); `rotate` e `remove` apagam-na sozinhos.
- Dispositivos atrás do mesmo NAT partilham um IP público e não se distinguem; emita uma chave por dispositivo para atribuição e revogação por dispositivo.

### Limites do cliente

Cada chave pode levar limites opcionais; não os definir significa sem limite. Definem-se com `umsp client add` e mudam-se com `umsp client update <name>`:

- `--expires-in 30d` — depois disso a chave é recusada com `401 key_expired` (`--no-expiry` em `update` tira-o).
- `--max-concurrency 1` — no máximo N (1-9) chamadas de modelo em curso; o resto recebe `429 concurrency_limit` (`--no-concurrency-limit` tira-o).
- `--budget-tokens 5000000` — orçamento de tokens para a vida. Cada chamada reserva primeiro o teto de saída (o valor por omissão do modelo se o pedido não definir nenhum) e é recusada com `403 budget_exhausted` se não couber; o uso real é debitado depois (`--reset-usage` / `--no-budget` em `update`).

Pormenores: [docs/configuration.md](docs/configuration.md#client-limits).

## Configuração

`~/.config/umsp/config.toml` mínimo:

```toml
[server]
listen = "127.0.0.1:3460"
state_dir = "~/.local/share/umsp"

[[providers]]
id = "deepseek"
vendor = "deepseek"                       # catálogo integrado; endpoints e quirks vêm daqui
base_url = "https://api.deepseek.com"
anthropic_base_url = "https://api.deepseek.com/anthropic"
api_key_env = "DEEPSEEK_API_KEY"          # só o nome; o valor vive em env
protocols = ["chat", "responses", "messages"]
quirks = "deepseek"

[[models]]
id = "deepseek-v4.1-flash"                # quem chama só conhece este id lógico
aliases = ["deepseek-flash"]
context_length = 1048000
max_completion_tokens = 128000
capabilities = ["text", "vision", "tools", "reasoning"]
backends = [
  { provider = "deepseek", upstream = "deepseek-flash" },
]

[[clients]]
name = "hermes"
key_hash = "sha256:..."                   # escrito por `umsp client add`
key_prefix = "umsp_ab12cd34"
allowed_models = ["*"]
allowed_protocols = ["chat", "responses", "messages"]
```

- As chaves de origem nunca vão para `config.toml`; lá só estão os nomes `api_key_env`.
- Precedência: `--config` > `$UMSP_CONFIG` > `~/.config/umsp/config.toml`; `$UMSP_LISTEN`, `$UMSP_STATE_DIR`, `$UMSP_ENV_FILE` substituem caminhos isolados.
- `/v1/models` contém uma entrada por modelo lógico e nunca expõe providers. De 12 em 12 horas a porta lê a lista de modelos de cada provider e deduplica por id. Contexto, limite de saída, nome visível e capacidades vêm dessa lista quando o provider os envia, incluindo cada patamar de contexto. Um único `context_length` é o único patamar, ou o maior quando a lista tem vários. Um valor por omissão documentado preenche um campo que a lista deixou vazio e nunca sobrescreve um valor de origem. Uma entrada `[[models]]` escrita à mão preenche o que ainda estiver vazio. `umsp catalog refresh` lê de imediato. `allowed_models = ["*"]` (ou uma lista vazia) mostra esse catálogo completo.
- `provider add --vendor ID` só recebe o id do fornecedor e a chave; cada adição recebe o seu próprio id.

Esquema completo, gestão de provider/model/client e todos os exemplos de CLI: [docs/configuration.md](docs/configuration.md).

## Comandos habituais

```bash
umsp provider list|add|remove|test      # instâncias de origem; add usa o catálogo integrado
umsp login|logout grok                  # sessão de código de dispositivo Grok, por provider
umsp model list|add|remove              # modelos lógicos e aliases
umsp model backend add|remove           # ligar ou desligar um backend (--first = mais prioridade)
umsp client list|add|remove|rotate|unbind # chaves de cliente (guarda-se SHA-256) + ligação TOFU
umsp env list|set|unset                 # chaves de origem, mascaradas
umsp usage summary|by-model|prune       # consulta o livro de pedidos
umsp doctor [--online]                  # diagnóstico; --online gasta alguns tokens
```

Cada subcomando aceita `--json`. A tabela completa de subcomandos e flags está em [docs/configuration.md](docs/configuration.md).

## Endpoints HTTP

| Método | Caminho | Auth | Função |
|---|---|---|---|
| GET | `/healthz` | Não | Estado legível com curl: circuito, falhas, último resultado, `usage` fresco |
| GET | `/metrics` | Não | Texto Prometheus, em memória, só loopback |
| GET | `/v1/models` | Sim | Registo de modelos lógicos (metadados planos + envelope `meta`) |
| POST | `/v1/chat/completions` | Sim | Chat Completions, em fluxo e fora de fluxo |
| POST | `/v1/responses` | Sim | Responses, em fluxo e fora de fluxo |
| POST | `/v1/messages` | Sim | Anthropic Messages, em fluxo e fora de fluxo |
| POST | `/v1/messages/count_tokens` | Sim | Faz de proxy onde o backend o suporta |
| HEAD | `/api/hello` | Não | Aquecimento da ligação do Claude Code |

Comportamento dos endpoints, mapeamento de erros, métricas, campos do livro, unidade systemd, códigos de saída e resolução de problemas: [docs/operations.md](docs/operations.md).

## Subscrições Grok

`umsp provider add --vendor grok` cria um provider apoiado numa sessão OAuth de código de dispositivo em `{config_dir}/sessions/<id>.json`. `umsp provider add --vendor chatgpt` usa Sign in with ChatGPT (PKCE em `127.0.0.1`, uso do plano em `https://api.openai.com/v1/responses` com `store: false` e fluxo obrigatório). Nenhuma das duas concessões é uma chave de API, e as duas sessões nunca partilham um refresh token. `umsp login <id>` inicia sessão num provider; `umsp logout <id>` apaga só esse ficheiro.

> **Experimental / risco para a conta.** O suporte Grok usa um cliente OAuth de CLI público e o endpoint de subscrição `cli-chat-proxy`. Não é uma integração oficial da API da xAI: pode partir sem aviso e pode violar os termos da xAI, incluindo a suspensão da conta. Use-o só com uma conta que esteja disposto a arriscar.

A porta envia o `x-grok-client-version` exigido (substitua com `UMSP_GROK_CLI_VERSION` quando a xAI subir o mínimo). O uso semanal da subscrição lê-se no endpoint de faturação, não nos cabeçalhos efémeros `x-ratelimit-*`. Pormenores: [docs/model-routing.md](docs/model-routing.md).

## Qwen Token Plan

`umsp provider add --vendor qwen-token-plan` usa o host do Qwen Token Plan mostrado na consola da subscrição (`token-plan.maas.qianwenaiapi.com`), não o DashScope de pagamento por uso nem o host internacional `qwencloudapi.com`. Uma chave `sk-` ou o host DashScope não descontam este plano. O Chat vai para `https://token-plan.maas.qianwenaiapi.com/compatible-mode/v1`, o Responses para `https://token-plan.maas.qianwenaiapi.com/compatible-mode/v1/responses` e o Messages para `https://token-plan.maas.qianwenaiapi.com/apps/anthropic`. A variável da chave é `QWEN_TOKEN_PLAN_API_KEY`. O plano está documentado para clientes interativos. O host regional antigo `token-plan.cn-beijing.maas.aliyuncs.com` ainda responde, mas não é o URL da consola.

## OpenCode Go

`umsp provider add --vendor opencode-go` usa o host de subscrição OpenCode Go `https://opencode.ai/inference/go`, não o host de pagamento por uso nem os modelos gratuitos (esses devolvem 403 fora do OpenCode). A chave é uma chave de API da consola, guardada como `OPENCODE_GO_API_KEY`. Chat e Responses usam a base OpenAI; Messages usa `https://opencode.ai/inference/go/anthropic`. Cada pedido envia `User-Agent: umsp/<version>` e `x-opencode-session`. Um cabeçalho do cliente com esse nome é mantido; caso contrário a porta faz o hash do modelo e da primeira mensagem para uma conversa ficar numa sessão. O Go não publica saldo nem um instantâneo de quota restante, por isso o provider ordena-se como não medido.

## Command Code

`umsp provider add --vendor command-code` usa a Provider API em `https://api.commandcode.ai/provider/v1`. O plano Go de 1 $ por mês não inclui esta API. GOAT, Pro, Max, Team e o plano Provider incluem. A chave vem do Studio e guarda-se como `COMMAND_CODE_API_KEY`. Chat, Responses e Messages partilham essa base. Cada linha da lista de modelos nomeia `supported_endpoints`; um modelo Claude é só Messages, por isso um cliente Chat é traduzido para `/messages` em vez de ser enviado para Chat Completions.

## Documentação

| Documento | Conteúdo |
|---|---|
| [docs/configuration.md](docs/configuration.md) | Esquema de configuração, referência de CLI, fluxos de provider/model/client |
| [docs/operations.md](docs/operations.md) | Endpoints, métricas, livro, códigos de saída, systemd, resolução de problemas, medições |
| [docs/protocols.md](docs/protocols.md) | Quirks por origem, comutação, transparência, SSE, Responses, Messages |
| [docs/model-routing.md](docs/model-routing.md) | Modelos lógicos, backends, classes de capacidade, fixação, quota do Grok |
| [docs/protocol-matrix.md](docs/protocol-matrix.md) | Matriz de tradução entre protocolos e campos com perda |
| [docs/quirks.md](docs/quirks.md) | Catálogo de quirks de origem |
| [CONTRIBUTING.md](CONTRIBUTING.md) / [SECURITY.md](SECURITY.md) | Regras de desenvolvimento / comunicação de vulnerabilidades |

## Desenvolvimento

```bash
make check          # fmt + clippy -D warnings + testes + verificação só de inglês
./scripts/build-musl.sh
```

O repositório é só em inglês (`AGENTS.md`). O inglês é a fonte de verdade; esta página é a tradução em português. Os instantâneos de marcos em `docs/internal/` são história e não descrevem o comportamento atual.

## License

[MIT](LICENSE) © 2026 YiLongMusk
