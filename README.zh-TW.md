# UMSP — Universal Model Service Provider

[![CI](https://github.com/yilong-musk/umsp/actions/workflows/ci.yml/badge.svg)](https://github.com/yilong-musk/umsp/actions/workflows/ci.yml)
[![Rust](https://img.shields.io/badge/Rust-1.85+-000000?logo=rust&logoColor=white)](https://www.rust-lang.org)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

[English](README.md) | [简体中文](README.zh-CN.md) | 繁體中文 | [日本語](README.ja.md) | [한국어](README.ko.md) | [Español](README.es.md) | [Français](README.fr.md) | [Deutsch](README.de.md) | [Português](README.pt.md) | [Русский](README.ru.md)

**單一二進位、只監聽回環位址**的 AI 閘道。一個埠同時提供 OpenAI Chat Completions、OpenAI Responses 與 Anthropic Messages，再把每個請求轉給講同一協議的上游；上游不會時就做協議翻譯。沒有 UI、資料庫、Redis 或 Docker。

- 一個 `umsp` 二進位；設定在 `~/.config/umsp/config.toml`，金鑰在 `~/.config/umsp/env`（0600），JSONL 請求帳本在 `~/.local/share/umsp/`。
- 邏輯模型：呼叫方只看到一個模型 id；每個模型有一組有序的 `backends`。
- 按容量選路：先選有新鮮用量的（U），再選沒有資料的（X），最後選只有餘額的（B）；成功服務過的 provider 會釘住五小時。
- 按 provider 熔斷、DeepSeek 餘額健康檢查、Grok 週額度輪詢。千問 Token Plan 兩者都沒有，所以留在 X。
- 對用戶端隱藏 provider；上游錯誤映射成中性的閘道錯誤。

## 快速開始

### 安裝

發行包（伺服器上不需要 Rust 工具鏈）：

```bash
tar -xzf umsp-<version>-x86_64-unknown-linux-musl.tar.gz
install -Dm755 umsp-<version>-x86_64-unknown-linux-musl/umsp ~/.local/bin/umsp
sha256sum -c umsp-<version>-x86_64-unknown-linux-musl.tar.gz.sha256
file ~/.local/bin/umsp                   # 靜態連結
```

推送帶註解的 tag `vX.Y.Z`（版本號必須與 `Cargo.toml` 的 `version` 一致）會把這個封存檔發成 GitHub Release，並用 minisign 簽名（[`umsp.pub`](umsp.pub)）。安裝由你自己做：倉庫也不持有任何主機存取權。見[安裝發行包](docs/operations.md#installing-a-release)。

### 從原始碼建置

```bash
sudo apt-get install -y musl-tools
rustup target add x86_64-unknown-linux-musl
git clone https://github.com/yilong-musk/umsp.git && cd umsp
./scripts/build-musl.sh                  # 產物在 dist/
```

### 執行

```bash
umsp init                                # config.toml、env（0600）、狀態目錄
$EDITOR ~/.config/umsp/env               # 填上游金鑰，例如 DEEPSEEK_API_KEY=...
umsp client add --name default           # 用戶端金鑰只印一次（umsp_...）
umsp check --json                        # 必須全綠
umsp serve                               # 印出 {"event":"ready",...}
```

另開一個終端呼叫：

```bash
export UMSP_API_KEY=umsp_xxx
curl -s http://127.0.0.1:3460/v1/models -H "x-api-key: ***" | jq

curl -s http://127.0.0.1:3460/v1/chat/completions \
  -H "x-api-key: ***" -H 'Content-Type: application/json' \
  -d '{"model":"deepseek-v4.1-flash","messages":[{"role":"user","content":"Reply with only: OK"}]}' | jq
```

`Authorization: Bearer ***` 與 `x-api-key` 等價。

### 裝置綁定（一把金鑰，一台裝置）

用戶端金鑰用首次使用即信任（TOFU）做裝置綁定：新金鑰尚未綁定，**第一個鑑權成功的請求**把它綁到該請求的來源 IP。之後這把金鑰的請求必須來自同一個 IP，否則拒絕並回 `403 device_mismatch`（錯誤本文不洩漏已綁的 IP）。綁定存在 `state_dir/client-bindings.json`，重啟不丟。

- 經反向代理時，真實用戶端 IP 是受信任的回環代理寫入的 `X-Forwarded-For` 最右一條；用戶端自己塞的條目會被忽略。本機（回環）呼叫方綁到 `127.0.0.1`。
- `umsp client list` 顯示每把金鑰的綁定 IP。`umsp client unbind <name>` 清除綁定（下一個請求會重新綁定——裝置 IP 變了時用它）；`rotate` 與 `remove` 會自動清除。
- 同一個 NAT 後面的裝置共用一個公網 IP，無法區分；要歸屬與按裝置撤銷，就一台裝置發一把金鑰。

### 用戶端限額

每把金鑰可以帶可選限額；不設就是不限。在 `umsp client add` 時設定，之後用 `umsp client update <name>` 修改：

- `--expires-in 30d` — 到期後請求被拒，回 `401 key_expired`（`update` 上的 `--no-expiry` 會拿掉它）。
- `--max-concurrency 1` — 同時進行的模型呼叫最多 N 個（1-9），超出回 `429 concurrency_limit`（`--no-concurrency-limit` 取消限制）。
- `--budget-tokens 5000000` — 終身 token 預算。每次呼叫先預扣輸出上限（請求沒設上限時用模型預設值），放不下就回 `403 budget_exhausted`；實際用量在事後扣除（`update` 上的 `--reset-usage` / `--no-budget`）。

細節見 [docs/configuration.md](docs/configuration.md#client-limits)。

## 設定

最小的 `~/.config/umsp/config.toml`：

```toml
[server]
listen = "127.0.0.1:3460"
state_dir = "~/.local/share/umsp"

[[providers]]
id = "deepseek"
vendor = "deepseek"                       # 內建目錄；端點與 quirks 來自它
base_url = "https://api.deepseek.com"
anthropic_base_url = "https://api.deepseek.com/anthropic"
api_key_env = "DEEPSEEK_API_KEY"          # 只寫名稱；值在 env
protocols = ["chat", "responses", "messages"]
quirks = "deepseek"

[[models]]
id = "deepseek-v4.1-flash"                # 呼叫方只認這個邏輯 id
aliases = ["deepseek-flash"]
context_length = 1048000
max_completion_tokens = 128000
capabilities = ["text", "vision", "tools", "reasoning"]
backends = [
  { provider = "deepseek", upstream = "deepseek-flash" },
]

[[clients]]
name = "hermes"
key_hash = "sha256:..."                   # 由 `umsp client add` 寫入
key_prefix = "umsp_ab12cd34"
allowed_models = ["*"]
allowed_protocols = ["chat", "responses", "messages"]
```

- 上游金鑰絕不寫進 `config.toml`；那裡只放 `api_key_env` 的名稱。
- 優先順序：`--config` > `$UMSP_CONFIG` > `~/.config/umsp/config.toml`；`$UMSP_LISTEN`、`$UMSP_STATE_DIR`、`$UMSP_ENV_FILE` 可覆蓋個別路徑。
- `/v1/models` 每個邏輯模型一條，絕不暴露 provider。閘道每 12 小時讀取各 provider 的模型清單並按 id 去重。上下文、輸出上限、顯示名與能力，只要 provider 有送就採用，包含每一檔上下文。單一的 `context_length` 是唯一的一檔，清單有多檔時則取最大的。文件裡的預設值只補清單留空的欄位，絕不覆蓋上游已經給的值。手寫的 `[[models]]` 再補這之後仍空著的欄位。`umsp catalog refresh` 立刻重讀。`allowed_models = ["*"]`（或空清單）才會把這份完整目錄給呼叫方。
- `provider add --vendor ID` 只收廠商 id 與金鑰；每次添加都拿到自己的 id。

完整 schema、provider/model/client 管理與全部 CLI 範例見 [docs/configuration.md](docs/configuration.md)。

## 常用命令

```bash
umsp provider list|add|remove|test      # 上游實例；add 使用內建廠商目錄
umsp login|logout grok                  # 每個 provider 自己的 Grok 裝置碼工作階段
umsp model list|add|remove              # 邏輯模型與別名
umsp model backend add|remove           # 掛上或卸下後端（--first = 更高優先）
umsp client list|add|remove|rotate|unbind # 用戶端金鑰（只存 SHA-256）+ TOFU 裝置綁定
umsp env list|set|unset                 # 上游金鑰，遮罩顯示
umsp usage summary|by-model|prune       # 查詢請求帳本
umsp doctor [--online]                  # 診斷；--online 會花少量 token
```

每個子命令都支援 `--json`。完整的子命令與旗標表見 [docs/configuration.md](docs/configuration.md)。

## HTTP 端點

| 方法 | 路徑 | 鑑權 | 用途 |
|---|---|---|---|
| GET | `/healthz` | 否 | 可用 curl 看的狀態：熔斷、失敗、最近結果、新鮮的 `usage` |
| GET | `/metrics` | 否 | Prometheus 文字，記憶體內，僅回環 |
| GET | `/v1/models` | 是 | 邏輯模型登錄表（平鋪元資料 + `meta` 信封） |
| POST | `/v1/chat/completions` | 是 | Chat Completions，串流與非串流 |
| POST | `/v1/responses` | 是 | Responses，串流與非串流 |
| POST | `/v1/messages` | 是 | Anthropic Messages，串流與非串流 |
| POST | `/v1/messages/count_tokens` | 是 | 後端支援時才代理 |
| HEAD | `/api/hello` | 否 | Claude Code 連線預熱 |

端點行為、錯誤映射、指標、帳本欄位、systemd 單元、退出碼與排錯見 [docs/operations.md](docs/operations.md)。

## Grok 訂閱

`umsp provider add --vendor grok` 建立一個由裝置碼 OAuth 工作階段支撐的 provider，工作階段在 `{config_dir}/sessions/<id>.json`。`umsp provider add --vendor chatgpt` 走 Sign in with ChatGPT（在 `127.0.0.1` 上做 PKCE，用量打在 `https://api.openai.com/v1/responses`，`store: false` 且必須串流）。兩種授權都不是 API 金鑰，兩份工作階段也絕不共用 refresh token。`umsp login <id>` 讓一個 provider 登入；`umsp logout <id>` 只刪那一個檔案。

> **實驗性 / 帳號風險。** Grok 支援使用公開的 CLI OAuth client 與 `cli-chat-proxy` 訂閱端點。它不是 xAI 的官方 API 整合：可能不經通知就壞掉，也可能違反 xAI 的服務條款，包括停用帳號。只在你願意承擔風險的帳號上使用。

閘道會送出必要的 `x-grok-client-version`（xAI 提高最低版本時用 `UMSP_GROK_CLI_VERSION` 覆蓋）。週訂閱用量從帳務端點讀取，不來自短命的 `x-ratelimit-*` 標頭。細節見 [docs/model-routing.md](docs/model-routing.md)。

## 千問 Token Plan

`umsp provider add --vendor qwen-token-plan` 使用訂閱主控台顯示的千問 Token Plan 主機（`token-plan.maas.qianwenaiapi.com`），不是按量付費的 DashScope，也不是國際的 `qwencloudapi.com`。`sk-` 金鑰或 DashScope 主機不會扣這份方案。Chat 走 `https://token-plan.maas.qianwenaiapi.com/compatible-mode/v1`，Responses 走 `https://token-plan.maas.qianwenaiapi.com/compatible-mode/v1/responses`，Messages 走 `https://token-plan.maas.qianwenaiapi.com/apps/anthropic`。金鑰變數是 `QWEN_TOKEN_PLAN_API_KEY`。這份方案的文件面向互動式用戶端。較舊的區域主機 `token-plan.cn-beijing.maas.aliyuncs.com` 目前仍會回應，但那不是主控台的 URL。

## OpenCode Go

`umsp provider add --vendor opencode-go` 使用 OpenCode Go 訂閱主機 `https://opencode.ai/inference/go`，不是按量付費的主機，也不是免費模型（那些在 OpenCode 外面會回 403）。金鑰是 Console API key，存成 `OPENCODE_GO_API_KEY`。Chat 與 Responses 用 OpenAI 那條基底；Messages 用 `https://opencode.ai/inference/go/anthropic`。每個請求送 `User-Agent: umsp/<version>` 與 `x-opencode-session`。用戶端自己帶了這個標頭就沿用；否則閘道用模型與第一則訊息算出雜湊，讓一段對話留在同一個工作階段。Go 不公布餘額或剩餘額度快照，所以這個 provider 按「未測量」排序。

## Command Code

`umsp provider add --vendor command-code` 使用 Provider API `https://api.commandcode.ai/provider/v1`。每月 $1 的 Go 方案不含這個 API。GOAT、Pro、Max、Team 與 Provider 方案有。金鑰來自 Studio，存成 `COMMAND_CODE_API_KEY`。Chat、Responses 與 Messages 共用這個基底。模型清單的每一列寫著 `supported_endpoints`；Claude 模型只有 Messages，所以 Chat 用戶端會被翻譯到 `/messages`，而不是直接打 Chat Completions。

## 文件

| 文件 | 內容 |
|---|---|
| [docs/configuration.md](docs/configuration.md) | 設定 schema、CLI 參考、provider/model/client 流程 |
| [docs/operations.md](docs/operations.md) | 端點、指標、帳本、退出碼、systemd、排錯、測量 |
| [docs/protocols.md](docs/protocols.md) | 各上游 quirks、容錯切換、透明性、SSE、Responses、Messages |
| [docs/model-routing.md](docs/model-routing.md) | 邏輯模型、後端、容量等級、釘選、Grok 額度 |
| [docs/protocol-matrix.md](docs/protocol-matrix.md) | 跨協議翻譯矩陣與有損欄位 |
| [docs/quirks.md](docs/quirks.md) | 上游 quirk 目錄 |
| [CONTRIBUTING.md](CONTRIBUTING.md) / [SECURITY.md](SECURITY.md) | 開發規範 / 漏洞回報 |

## 開發

```bash
make check          # fmt + clippy -D warnings + 測試 + 僅英文語言檢查
./scripts/build-musl.sh
```

倉庫正文只允許英文（`AGENTS.md`）。英文版是唯一權威來源，本頁是它的繁體中文翻譯。`docs/internal/` 裡的里程碑快照是歷史記錄，不代表目前行為。

## License

[MIT](LICENSE) © 2026 YiLongMusk
