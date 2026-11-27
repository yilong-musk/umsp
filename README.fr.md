# UMSP — Universal Model Service Provider

[![CI](https://github.com/yilong-musk/umsp/actions/workflows/ci.yml/badge.svg)](https://github.com/yilong-musk/umsp/actions/workflows/ci.yml)
[![Rust](https://img.shields.io/badge/Rust-1.85+-000000?logo=rust&logoColor=white)](https://www.rust-lang.org)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

[Deutsch](README.de.md) | [English](README.md) | [Español](README.es.md) | Français | [日本語](README.ja.md) | [한국어](README.ko.md) | [Português](README.pt.md) | [Русский](README.ru.md) | [简体中文](README.zh-CN.md) | [繁體中文](README.zh-TW.md)

Une passerelle d'IA **à binaire unique, en loopback seulement**. Elle expose OpenAI Chat Completions, OpenAI Responses et Anthropic Messages sur un port, puis transmet chaque requête à un amont qui parle le même protocole — ou le traduit quand le backend ne le peut pas. Pas d'UI, de base de données, de Redis ni de Docker.

- Un binaire `umsp` ; la configuration dans `~/.config/umsp/config.toml`, les clés dans `~/.config/umsp/env` (0600), le registre JSONL des requêtes dans `~/.local/share/umsp/`.
- Modèles logiques : l'appelant voit un seul id de modèle ; chaque modèle a une liste ordonnée de `backends`.
- Choix selon la capacité : d'abord l'usage frais (U), puis l'absence de données (X), puis le seul solde (B) ; un provider qui a réussi reste épinglé cinq heures.
- Disjoncteur par provider, santé du solde DeepSeek, sondage du quota hebdomadaire Grok. Qwen Token Plan n'a ni l'un ni l'autre, donc il reste en X.
- Les requêtes client ne révèlent pas le provider ; les erreurs amont sont ramenées à des erreurs neutres de passerelle.

## Démarrage rapide

### Installation

Paquet de release (pas de chaîne Rust sur le serveur) :

```bash
tar -xzf umsp-<version>-x86_64-unknown-linux-musl.tar.gz
install -Dm755 umsp-<version>-x86_64-unknown-linux-musl/umsp ~/.local/bin/umsp
sha256sum -c umsp-<version>-x86_64-unknown-linux-musl.tar.gz.sha256
file ~/.local/bin/umsp                   # lié statiquement
```

Pousser un tag annoté `vX.Y.Z` (le numéro doit égaler `version` dans `Cargo.toml`) publie cette archive comme GitHub Release, signée avec minisign ([`umsp.pub`](umsp.pub)). L'installation vous revient : le dépôt n'a pas non plus d'accès à l'hôte. Voir [Installer une release](docs/operations.md#installing-a-release).

### Compiler depuis les sources

```bash
sudo apt-get install -y musl-tools
rustup target add x86_64-unknown-linux-musl
git clone https://github.com/yilong-musk/umsp.git && cd umsp
./scripts/build-musl.sh                  # sortie dans dist/
```

### Lancer

```bash
umsp init                                # config.toml, env (0600), répertoire d'état
$EDITOR ~/.config/umsp/env               # clés amont, p. ex. DEEPSEEK_API_KEY=...
umsp client add --name default           # la clé client s'affiche une fois (umsp_...)
umsp check --json                        # doit être vert
umsp serve                               # affiche {"event":"ready",...}
```

L'appeler depuis un autre terminal :

```bash
export UMSP_API_KEY=umsp_xxx
curl -s http://127.0.0.1:3460/v1/models -H "x-api-key: ***" | jq

curl -s http://127.0.0.1:3460/v1/chat/completions \
  -H "x-api-key: ***" -H 'Content-Type: application/json' \
  -d '{"model":"deepseek-v4.1-flash","messages":[{"role":"user","content":"Reply with only: OK"}]}' | jq
```

`Authorization: Bearer ***` équivaut à `x-api-key`.

### Liaison d'appareil (une clé, un appareil)

Les clés client utilisent une liaison d'appareil par confiance au premier usage (TOFU) : une clé neuve n'est pas liée, et la **première requête authentifiée** la lie à l'IP source de cette requête. Toute requête ultérieure avec cette clé doit venir de la même IP, sinon elle est refusée avec `403 device_mismatch` (le corps d'erreur ne révèle jamais l'IP liée). Les liaisons persistent dans `state_dir/client-bindings.json` et survivent aux redémarrages.

- Derrière un proxy inverse, la vraie IP client est l'entrée la plus à droite de `X-Forwarded-For` écrite par le proxy loopback de confiance ; les entrées fournies par le client sont ignorées. Les appelants locaux (loopback) se lient à `127.0.0.1`.
- `umsp client list` montre l'IP liée de chaque clé. `umsp client unbind <name>` efface une liaison (la requête suivante relie — à utiliser quand l'IP d'un appareil change) ; `rotate` et `remove` l'effacent tout seuls.
- Les appareils derrière le même NAT partagent une IP publique et sont indistinguables ; émettez une clé par appareil pour l'attribution et la révocation par appareil.

### Limites client

Chaque clé peut porter des limites facultatives ; ne pas les fixer signifie illimité. On les pose avec `umsp client add` et on les change avec `umsp client update <name>` :

- `--expires-in 30d` — passé ce délai la clé est refusée avec `401 key_expired` (`--no-expiry` sur `update` l'enlève).
- `--max-concurrency 1` — au plus N (1-9) appels de modèle en cours ; au-delà, `429 concurrency_limit` (`--no-concurrency-limit` l'enlève).
- `--budget-tokens 5000000` — budget de tokens à vie. Chaque appel réserve d'abord son plafond de sortie (la valeur par défaut du modèle si la requête n'en fixe pas) et est refusé avec `403 budget_exhausted` s'il ne tient pas ; l'usage réel est imputé ensuite (`--reset-usage` / `--no-budget` sur `update`).

Détails : [docs/configuration.md](docs/configuration.md#client-limits).

## Configuration

`~/.config/umsp/config.toml` minimal :

```toml
[server]
listen = "127.0.0.1:3460"
state_dir = "~/.local/share/umsp"

[[providers]]
id = "deepseek"
vendor = "deepseek"                       # catalogue intégré ; endpoints et quirks en viennent
base_url = "https://api.deepseek.com"
anthropic_base_url = "https://api.deepseek.com/anthropic"
api_key_env = "DEEPSEEK_API_KEY"          # le nom seulement ; la valeur vit dans env
protocols = ["chat", "responses", "messages"]
quirks = "deepseek"

[[models]]
id = "deepseek-v4.1-flash"                # l'appelant ne connaît que cet id logique
aliases = ["deepseek-flash"]
context_length = 1048000
max_completion_tokens = 128000
capabilities = ["text", "vision", "tools", "reasoning"]
backends = [
  { provider = "deepseek", upstream = "deepseek-flash" },
]

[[clients]]
name = "hermes"
key_hash = "sha256:..."                   # écrit par `umsp client add`
key_prefix = "umsp_ab12cd34"
allowed_models = ["*"]
allowed_protocols = ["chat", "responses", "messages"]
```

- Les clés amont ne vont jamais dans `config.toml` ; seuls les noms `api_key_env` y figurent.
- Préséance : `--config` > `$UMSP_CONFIG` > `~/.config/umsp/config.toml` ; `$UMSP_LISTEN`, `$UMSP_STATE_DIR`, `$UMSP_ENV_FILE` remplacent des chemins isolés.
- `/v1/models` contient une entrée par modèle logique et n'expose jamais les providers. Toutes les 12 heures la passerelle lit la liste de modèles de chaque provider et déduplique par id. Contexte, limite de sortie, nom affiché et capacités viennent de cette liste quand le provider les envoie, y compris chaque palier de contexte. Un seul `context_length` est le seul palier, ou le plus grand quand la liste en a plusieurs. Une valeur par défaut documentée remplit un champ que la liste a laissé vide et n'écrase jamais une valeur amont. Une entrée `[[models]]` écrite à la main remplit ensuite ce qui est encore vide. `umsp catalog refresh` relit tout de suite. `allowed_models = ["*"]` (ou une liste vide) montre ce catalogue complet.
- `provider add --vendor ID` ne prend que l'id du fournisseur et la clé ; chaque ajout reçoit son propre id.

Schéma complet, gestion provider/model/client et tous les exemples CLI : [docs/configuration.md](docs/configuration.md).

## Commandes courantes

```bash
umsp provider list|add|remove|test      # instances amont ; add utilise le catalogue intégré
umsp login|logout grok                  # session de code d'appareil Grok, par provider
umsp model list|add|remove              # modèles logiques et alias
umsp model backend add|remove           # attacher ou détacher un backend (--first = plus prioritaire)
umsp client list|add|remove|rotate|unbind # clés client (SHA-256 stocké) + liaison TOFU
umsp env list|set|unset                 # clés amont, masquées
umsp usage summary|by-model|prune       # interroge le registre des requêtes
umsp doctor [--online]                  # diagnostic ; --online dépense quelques tokens
```

Chaque sous-commande accepte `--json`. Le tableau complet des sous-commandes et des indicateurs est dans [docs/configuration.md](docs/configuration.md).

## Endpoints HTTP

| Méthode | Chemin | Auth | Rôle |
|---|---|---|---|
| GET | `/healthz` | Non | État lisible par curl : circuit, échecs, dernier résultat, `usage` frais |
| GET | `/metrics` | Non | Texte Prometheus, en mémoire, loopback seulement |
| GET | `/v1/models` | Oui | Registre des modèles logiques (métadonnées plates + enveloppe `meta`) |
| POST | `/v1/chat/completions` | Oui | Chat Completions, en flux et hors flux |
| POST | `/v1/responses` | Oui | Responses, en flux et hors flux |
| POST | `/v1/messages` | Oui | Anthropic Messages, en flux et hors flux |
| POST | `/v1/messages/count_tokens` | Oui | Relais là où le backend le prend en charge |
| HEAD | `/api/hello` | Non | Préchauffage de connexion Claude Code |

Comportement des endpoints, correspondance des erreurs, métriques, champs du registre, unité systemd, codes de sortie et dépannage : [docs/operations.md](docs/operations.md).

## Abonnements Grok

`umsp provider add --vendor grok` crée un provider adossé à une session OAuth par code d'appareil dans `{config_dir}/sessions/<id>.json`. `umsp provider add --vendor chatgpt` utilise Sign in with ChatGPT (PKCE sur `127.0.0.1`, usage du plan sur `https://api.openai.com/v1/responses` avec `store: false` et un flux obligatoire). Aucun des deux octrois n'est une clé d'API, et les deux sessions ne partagent jamais de refresh token. `umsp login <id>` connecte un provider ; `umsp logout <id>` ne supprime que ce fichier.

> **Expérimental / risque pour le compte.** La prise en charge de Grok utilise un client OAuth CLI public et l'endpoint d'abonnement `cli-chat-proxy`. Ce n'est pas une intégration officielle de l'API xAI : elle peut casser sans préavis et peut enfreindre les conditions d'xAI, y compris la suspension du compte. Ne l'utilisez qu'avec un compte que vous acceptez de risquer.

La passerelle envoie le `x-grok-client-version` requis (à remplacer par `UMSP_GROK_CLI_VERSION` quand xAI relève le minimum). L'usage hebdomadaire de l'abonnement se lit sur l'endpoint de facturation, pas sur les en-têtes éphémères `x-ratelimit-*`. Détails : [docs/model-routing.md](docs/model-routing.md).

## Qwen Token Plan

`umsp provider add --vendor qwen-token-plan` utilise l'hôte Qwen Token Plan affiché dans la console d'abonnement (`token-plan.maas.qianwenaiapi.com`), pas le DashScope au paiement à l'usage ni l'hôte international `qwencloudapi.com`. Une clé `sk-` ou l'hôte DashScope ne débitera pas ce plan. Chat va vers `https://token-plan.maas.qianwenaiapi.com/compatible-mode/v1`, Responses vers `https://token-plan.maas.qianwenaiapi.com/compatible-mode/v1/responses` et Messages vers `https://token-plan.maas.qianwenaiapi.com/apps/anthropic`. La variable de clé est `QWEN_TOKEN_PLAN_API_KEY`. Le plan est documenté pour des clients interactifs. L'ancien hôte régional `token-plan.cn-beijing.maas.aliyuncs.com` répond encore, mais ce n'est pas l'URL de la console.

## OpenCode Go

`umsp provider add --vendor opencode-go` utilise l'hôte d'abonnement OpenCode Go `https://opencode.ai/inference/go`, pas l'hôte au paiement à l'usage ni les modèles gratuits (ceux-ci renvoient 403 hors d'OpenCode). La clé est une clé d'API de la console, stockée comme `OPENCODE_GO_API_KEY`. Chat et Responses utilisent la base OpenAI ; Messages utilise `https://opencode.ai/inference/go/anthropic`. Chaque requête envoie `User-Agent: umsp/<version>` et `x-opencode-session`. Un en-tête client de ce nom est conservé ; sinon la passerelle hache le modèle et le premier message pour qu'une conversation reste sur une session. Go ne publie ni solde ni instantané de quota restant, donc le provider se classe comme non mesuré.

## Command Code

`umsp provider add --vendor command-code` utilise la Provider API à `https://api.commandcode.ai/provider/v1`. Le plan Go à 1 $ par mois n'inclut pas cette API. GOAT, Pro, Max, Team et le plan Provider oui. La clé vient de Studio et est stockée comme `COMMAND_CODE_API_KEY`. Chat, Responses et Messages partagent cette base. Chaque ligne de la liste de modèles nomme `supported_endpoints` ; un modèle Claude n'est que Messages, donc un client Chat est traduit vers `/messages` au lieu d'être envoyé à Chat Completions.

## Documentation

| Document | Contenu |
|---|---|
| [docs/configuration.md](docs/configuration.md) | Schéma de configuration, référence CLI, flux provider/model/client |
| [docs/operations.md](docs/operations.md) | Endpoints, métriques, registre, codes de sortie, systemd, dépannage, mesures |
| [docs/protocols.md](docs/protocols.md) | Quirks par amont, bascule, transparence, SSE, Responses, Messages |
| [docs/model-routing.md](docs/model-routing.md) | Modèles logiques, backends, classes de capacité, épinglage, quota Grok |
| [docs/protocol-matrix.md](docs/protocol-matrix.md) | Matrice de traduction entre protocoles et champs avec perte |
| [docs/quirks.md](docs/quirks.md) | Catalogue des quirks amont |
| [CONTRIBUTING.md](CONTRIBUTING.md) / [SECURITY.md](SECURITY.md) | Règles de développement / signalement de vulnérabilités |

## Développement

```bash
make check          # fmt + clippy -D warnings + tests + contrôle anglais seul
./scripts/build-musl.sh
```

Le dépôt est en anglais seulement (`AGENTS.md`). L'anglais est la source de vérité ; cette page en est la traduction française. Les instantanés d'étapes dans `docs/internal/` sont de l'historique et ne décrivent pas le comportement actuel.

## License

[MIT](LICENSE) © 2026 YiLongMusk
