# UMSP — Universal Model Service Provider

[![CI](https://github.com/yilong-musk/umsp/actions/workflows/ci.yml/badge.svg)](https://github.com/yilong-musk/umsp/actions/workflows/ci.yml)
[![Rust](https://img.shields.io/badge/Rust-1.85+-000000?logo=rust&logoColor=white)](https://www.rust-lang.org)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

[Deutsch](README.de.md) | [English](README.md) | [Español](README.es.md) | [Français](README.fr.md) | 日本語 | [한국어](README.ko.md) | [Português](README.pt.md) | [Русский](README.ru.md) | [简体中文](README.zh-CN.md) | [繁體中文](README.zh-TW.md)

**単一バイナリで、ループバックだけ**の AI ゲートウェイです。一つのポートで OpenAI Chat Completions、OpenAI Responses、Anthropic Messages を出し、各リクエストを同じプロトコルを話す上流へ転送します。上流が話せないときは翻訳します。UI、データベース、Redis、Docker はありません。

- `umsp` バイナリが一つ。設定は `~/.config/umsp/config.toml`、鍵は `~/.config/umsp/env`（0600）、JSONL のリクエスト台帳は `~/.local/share/umsp/`。
- 論理モデル: 呼び出し側が見るモデル id は一つ。各モデルには順序付きの `backends` があります。
- 容量を見て選ぶ: 新鮮な使用量（U）が先、データなし（X）が次、残高だけ（B）が最後。成功した provider は 5 時間ピン留めされます。
- provider ごとのサーキットブレーカ、DeepSeek の残高ヘルス、Grok の週次枠のポーリング。Qwen Token Plan にはどちらもないので X のままです。
- クライアントのリクエストから provider は見えません。上流のエラーは中立なゲートウェイエラーに写像されます。

## クイックスタート

### インストール

リリースパッケージ（サーバに Rust ツールチェーンは不要）:

```bash
tar -xzf umsp-<version>-x86_64-unknown-linux-musl.tar.gz
install -Dm755 umsp-<version>-x86_64-unknown-linux-musl/umsp ~/.local/bin/umsp
sha256sum -c umsp-<version>-x86_64-unknown-linux-musl.tar.gz.sha256
file ~/.local/bin/umsp                   # 静的リンク
```

注釈付きタグ `vX.Y.Z` を push すると（番号は `Cargo.toml` の `version` と一致させる）、このアーカイブが minisign 署名付きの GitHub Release になります（[`umsp.pub`](umsp.pub)）。インストールは自分で行います。リポジトリはホストへのアクセスも持ちません。[リリースのインストール](docs/operations.md#installing-a-release)を参照してください。

### ソースからビルド

```bash
sudo apt-get install -y musl-tools
rustup target add x86_64-unknown-linux-musl
git clone https://github.com/yilong-musk/umsp.git && cd umsp
./scripts/build-musl.sh                  # 出力は dist/
```

### 実行

```bash
umsp init                                # config.toml、env（0600）、状態ディレクトリ
$EDITOR ~/.config/umsp/env               # 上流の鍵を入れる。例: DEEPSEEK_API_KEY=...
umsp client add --name default           # クライアント鍵は一度だけ表示（umsp_...）
umsp check --json                        # 緑であること
umsp serve                               # {"event":"ready",...} を表示
```

別の端末から呼ぶ:

```bash
export UMSP_API_KEY=umsp_xxx
curl -s http://127.0.0.1:3460/v1/models -H "x-api-key: ***" | jq

curl -s http://127.0.0.1:3460/v1/chat/completions \
  -H "x-api-key: ***" -H 'Content-Type: application/json' \
  -d '{"model":"deepseek-v4.1-flash","messages":[{"role":"user","content":"Reply with only: OK"}]}' | jq
```

`Authorization: Bearer ***` は `x-api-key` と同等です。

### デバイス束縛（鍵一つ、デバイス一つ）

クライアント鍵は初回使用時信頼（TOFU）でデバイスに束縛されます。新しい鍵は未束縛で、**最初に認証されたリクエスト**がそのリクエストの送信元 IP に鍵を束縛します。以後その鍵のリクエストは同じ IP からでなければ拒否され、`403 device_mismatch` になります（エラー本文は束縛済み IP を明かしません）。束縛は `state_dir/client-bindings.json` に残り、再起動しても消えない。

- リバースプロキシの背後では、本物のクライアント IP は信頼されたループバックプロキシが書いた `X-Forwarded-For` の右端です。クライアントが入れた項目は無視されます。ローカル（ループバック）の呼び出し元は `127.0.0.1` に束縛されます。
- `umsp client list` は各鍵の束縛 IP を表示します。`umsp client unbind <name>` は束縛を消します（次のリクエストで束縛し直す。デバイスの IP が変わったときに使う）。`rotate` と `remove` は自動で消します。
- 同じ NAT の背後のデバイスは一つの公開 IP を共有し、区別できません。帰属とデバイス単位の失効には、デバイスごとに鍵を発行してください。

### クライアント制限

各鍵には任意の制限を付けられます。未設定は無制限です。`umsp client add` で設定し、`umsp client update <name>` で変えます。

- `--expires-in 30d` — その後のリクエストは `401 key_expired` で拒否されます（`update` の `--no-expiry` で外す）。
- `--max-concurrency 1` — 同時に走れるモデル呼び出しは最大 N（1-9）。超えると `429 concurrency_limit`（`--no-concurrency-limit` で外す）。
- `--budget-tokens 5000000` — 生涯の token 予算。各呼び出しは先に出力上限（リクエストが指定しなければモデルの既定）を予約し、収まらなければ `403 budget_exhausted`。実際の使用量は後から課金します（`update` の `--reset-usage` / `--no-budget`）。

詳細は [docs/configuration.md](docs/configuration.md#client-limits)。

## 設定

最小の `~/.config/umsp/config.toml`:

```toml
[server]
listen = "127.0.0.1:3460"
state_dir = "~/.local/share/umsp"

[[providers]]
id = "deepseek"
vendor = "deepseek"                       # 内蔵カタログ。エンドポイントと quirks はここから
base_url = "https://api.deepseek.com"
anthropic_base_url = "https://api.deepseek.com/anthropic"
api_key_env = "DEEPSEEK_API_KEY"          # 名前だけ。値は env にある
protocols = ["chat", "responses", "messages"]
quirks = "deepseek"

[[models]]
id = "deepseek-v4.1-flash"                # 呼び出し側が知るのはこの論理 id だけ
aliases = ["deepseek-flash"]
context_length = 1048000
max_completion_tokens = 128000
capabilities = ["text", "vision", "tools", "reasoning"]
backends = [
  { provider = "deepseek", upstream = "deepseek-flash" },
]

[[clients]]
name = "hermes"
key_hash = "sha256:..."                   # `umsp client add` が書く
key_prefix = "umsp_ab12cd34"
allowed_models = ["*"]
allowed_protocols = ["chat", "responses", "messages"]
```

- 上流の鍵は決して `config.toml` に書きません。そこにあるのは `api_key_env` の名前だけです。
- 優先順位: `--config` > `$UMSP_CONFIG` > `~/.config/umsp/config.toml`。`$UMSP_LISTEN`、`$UMSP_STATE_DIR`、`$UMSP_ENV_FILE` は個別のパスを上書きします。
- `/v1/models` は論理モデルごとに一つで、provider は出しません。ゲートウェイは 12 時間ごとに各 provider のモデル一覧を読み、id で重複を除きます。コンテキスト、出力上限、表示名、能力は、provider が送っていればそれを使い、コンテキストの各段も含みます。単一の `context_length` は唯一の段か、一覧に複数あるときは最大の段です。文書化された既定は一覧が空にしたフィールドだけを埋め、上流の値は上書きしません。手書きの `[[models]]` は、その後も空のものを埋めます。`umsp catalog refresh` は直ちに読み直します。`allowed_models = ["*"]`（または空の一覧）がその完全なカタログを見せます。
- `provider add --vendor ID` が受け取るのはベンダー id と鍵だけです。追加のたびに独自の id が付きます。

完全なスキーマ、provider/model/client の管理、すべての CLI 例は [docs/configuration.md](docs/configuration.md) にあります。

## よく使うコマンド

```bash
umsp provider list|add|remove|test      # 上流インスタンス。add は内蔵ベンダーカタログを使う
umsp login|logout grok                  # provider ごとの Grok デバイスコードセッション
umsp model list|add|remove              # 論理モデルと別名
umsp model backend add|remove           # バックエンドの取り付けと取り外し（--first = より高い優先度）
umsp client list|add|remove|rotate|unbind # クライアント鍵（SHA-256 を保存）+ TOFU のデバイス束縛
umsp env list|set|unset                 # 上流の鍵。マスク表示
umsp usage summary|by-model|prune       # リクエスト台帳を照会
umsp doctor [--online]                  # 診断。--online は数 token を使う
```

すべてのサブコマンドが `--json` をサポートします。サブコマンドとフラグの全表は [docs/configuration.md](docs/configuration.md) を参照してください。

## HTTP エンドポイント

| メソッド | パス | 認証 | 用途 |
|---|---|---|---|
| GET | `/healthz` | なし | curl できる状態: ブレーカ、失敗、直近の結果、新鮮な `usage` |
| GET | `/metrics` | なし | Prometheus テキスト、メモリ内、ループバックのみ |
| GET | `/v1/models` | あり | 論理モデルの登録（フラットなメタデータ + `meta` 封筒） |
| POST | `/v1/chat/completions` | あり | Chat Completions。ストリーミングと非ストリーミング |
| POST | `/v1/responses` | あり | Responses。ストリーミングと非ストリーミング |
| POST | `/v1/messages` | あり | Anthropic Messages。ストリーミングと非ストリーミング |
| POST | `/v1/messages/count_tokens` | あり | バックエンドが対応しているところだけプロキシ |
| HEAD | `/api/hello` | なし | Claude Code の接続ウォームアップ |

エンドポイントの挙動、エラー写像、メトリクス、台帳フィールド、systemd ユニット、終了コード、トラブルシュートは [docs/operations.md](docs/operations.md) にあります。

## Grok のサブスクリプション

`umsp provider add --vendor grok` は、デバイスコード OAuth セッションを `{config_dir}/sessions/<id>.json` に持つ provider を作ります。`umsp provider add --vendor chatgpt` は Sign in with ChatGPT を使います（`127.0.0.1` 上の PKCE、プラン使用量は `https://api.openai.com/v1/responses`、`store: false` でストリーム必須）。どちらも API 鍵ではなく、二つのセッションが refresh token を共有することはありません。`umsp login <id>` は一つの provider にサインインし、`umsp logout <id>` はそのファイルだけを消します。

> **実験的 / アカウントリスク。** Grok 対応は公開の CLI OAuth client と `cli-chat-proxy` のサブスクリプションエンドポイントを使います。公式の xAI API 統合ではありません。予告なく壊れ、xAI の利用規約に反する可能性があり、アカウント停止を含みます。リスクを受け入れられるアカウントでのみ使ってください。

ゲートウェイは必要な `x-grok-client-version` を送ります（xAI が最低版を上げたら `UMSP_GROK_CLI_VERSION` で上書き）。週次のサブスクリプション使用量は課金エンドポイントから読み、短命の `x-ratelimit-*` ヘッダからは読みません。詳細は [docs/model-routing.md](docs/model-routing.md)。

## Qwen Token Plan

`umsp provider add --vendor qwen-token-plan` は、サブスクリプションのコンソールに出る Qwen Token Plan のホスト（`token-plan.maas.qianwenaiapi.com`）を使います。従量の DashScope でも、国際向けの `qwencloudapi.com` でもありません。`sk-` の鍵や DashScope のホストではこのプランは減りません。Chat は `https://token-plan.maas.qianwenaiapi.com/compatible-mode/v1`、Responses は `https://token-plan.maas.qianwenaiapi.com/compatible-mode/v1/responses`、Messages は `https://token-plan.maas.qianwenaiapi.com/apps/anthropic` です。鍵の変数は `QWEN_TOKEN_PLAN_API_KEY` です。このプランは対話クライアント向けと文書化されています。古い地域ホスト `token-plan.cn-beijing.maas.aliyuncs.com` はまだ応答しますが、コンソールの URL ではありません。

## OpenCode Go

`umsp provider add --vendor opencode-go` は OpenCode Go のサブスクリプションホスト `https://opencode.ai/inference/go` を使います。従量ホストでも無料モデルでもありません（無料モデルは OpenCode の外では 403 になります）。鍵は Console API key で、`OPENCODE_GO_API_KEY` として保存します。Chat と Responses は OpenAI の基底を使い、Messages は `https://opencode.ai/inference/go/anthropic` を使います。各リクエストは `User-Agent: umsp/<version>` と `x-opencode-session` を送ります。クライアントがその名前のヘッダを持っていればそれを残し、なければゲートウェイがモデルと最初のメッセージをハッシュして、一つの会話を一つのセッションに留めます。Go は残高や残枠のスナップショットを出さないので、この provider は未計測として並びます。

## Command Code

`umsp provider add --vendor command-code` は Provider API `https://api.commandcode.ai/provider/v1` を使います。月額 $1 の Go プランにはこの API は含まれません。GOAT、Pro、Max、Team、Provider プランには含まれます。鍵は Studio から来て、`COMMAND_CODE_API_KEY` として保存されます。Chat、Responses、Messages はその基底を共有します。モデル一覧の各行は `supported_endpoints` を持ちます。Claude モデルは Messages のみなので、Chat クライアントは Chat Completions へ送られず、`/messages` へ翻訳されます。

## ドキュメント

| ドキュメント | 内容 |
|---|---|
| [docs/configuration.md](docs/configuration.md) | 設定スキーマ、CLI リファレンス、provider/model/client の手順 |
| [docs/operations.md](docs/operations.md) | エンドポイント、メトリクス、台帳、終了コード、systemd、トラブルシュート、測定 |
| [docs/protocols.md](docs/protocols.md) | 上流ごとの quirks、フェイルオーバー、透過性、SSE、Responses、Messages |
| [docs/model-routing.md](docs/model-routing.md) | 論理モデル、バックエンド、容量クラス、ピン留め、Grok の枠 |
| [docs/protocol-matrix.md](docs/protocol-matrix.md) | プロトコル間の翻訳行列と失われるフィールド |
| [docs/quirks.md](docs/quirks.md) | 上流 quirk のカタログ |
| [CONTRIBUTING.md](CONTRIBUTING.md) / [SECURITY.md](SECURITY.md) | 開発ルール / 脆弱性の報告 |

## 開発

```bash
make check          # fmt + clippy -D warnings + テスト + 英語のみの言語チェック
./scripts/build-musl.sh
```

リポジトリの本文は英語のみです（`AGENTS.md`）。英語版が唯一の正本で、このページはその日本語訳です。`docs/internal/` のマイルストーンスナップショットは履歴であり、現在の挙動ではありません。

## License

[MIT](LICENSE) © 2026 YiLongMusk
