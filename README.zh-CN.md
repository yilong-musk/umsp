# UMSP — Universal Model Service Provider

[![CI](https://github.com/yilong-musk/umsp/actions/workflows/ci.yml/badge.svg)](https://github.com/yilong-musk/umsp/actions/workflows/ci.yml)
[![Rust](https://img.shields.io/badge/Rust-1.85+-000000?logo=rust&logoColor=white)](https://www.rust-lang.org)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

[English](README.md) | 简体中文

**单二进制、只监听回环地址**的 AI 网关：一个端口同时提供 OpenAI Chat Completions、OpenAI
Responses 和 Anthropic Messages；上游支持同一协议就原样转发，不支持就做协议翻译。没有 UI、
数据库、Redis、Docker。

- 一个 `umsp` 二进制；配置 `~/.config/umsp/config.toml`，密钥 `~/.config/umsp/env`（0600），
  JSONL 请求账本 `~/.local/share/umsp/`
- 逻辑模型：调用方只认一个模型 id；每个模型有一组有序 `backends`
- 容量感知选路：有 fresh 用量（U）优先，其次无数据（X），最后只有余额（B）；成功服务后 pin 5 小时
- 按 provider 熔断、DeepSeek 余额健康检查、Grok 周额度轮询。千问 Token Plan 两者都没有，归入 X。
- 对客户端隐藏 provider；上游错误统一映射为中性网关错误

## 快速开始

### 安装

发行包（服务器不需要 Rust 工具链）：

```bash
tar -xzf umsp-<version>-x86_64-unknown-linux-musl.tar.gz
install -Dm755 umsp-<version>-x86_64-unknown-linux-musl/umsp ~/.local/bin/umsp
sha256sum -c umsp-<version>-x86_64-unknown-linux-musl.tar.gz.sha256
file ~/.local/bin/umsp                   # 静态链接
```

推送带注释的 tag `vX.Y.Z`（版本号必须与 `Cargo.toml` 的 `version` 一致）会发布上面的归档，
并安装到已配置的主机。见 [GitHub release deploy](docs/operations.md#github-release-deploy)。

### 从源码构建

```bash
sudo apt-get install -y musl-tools
rustup target add x86_64-unknown-linux-musl
git clone https://github.com/yilong-musk/umsp.git && cd umsp
./scripts/build-musl.sh                  # 产物在 dist/
```

### 运行

```bash
umsp init                                # 生成 config.toml、env（0600）和状态目录
$EDITOR ~/.config/umsp/env               # 填上游 key，例如 DEEPSEEK_API_KEY=...
umsp client add --name default           # 只打印一次客户端 key（umsp_...）
umsp check --json                        # 全绿再启动
umsp serve                               # 打印 {"event":"ready",...}
```

另开终端调用：

```bash
export UMSP_API_KEY=umsp_xxx
curl -s http://127.0.0.1:3460/v1/models -H "x-api-key: $UMSP_API_KEY" | jq

curl -s http://127.0.0.1:3460/v1/chat/completions \
  -H "x-api-key: $UMSP_API_KEY" -H 'Content-Type: application/json' \
  -d '{"model":"deepseek-v4.1-flash","messages":[{"role":"user","content":"Reply with only: OK"}]}' | jq
```

`Authorization: Bearer $UMSP_API_KEY` 与 `x-api-key` 等价。

## 配置

最小 `~/.config/umsp/config.toml`：

```toml
[server]
listen = "127.0.0.1:3460"
state_dir = "~/.local/share/umsp"

[[providers]]
id = "deepseek"
vendor = "deepseek"                       # 内置目录；端点和 quirks 由目录决定
base_url = "https://api.deepseek.com"
anthropic_base_url = "https://api.deepseek.com/anthropic"
api_key_env = "DEEPSEEK_API_KEY"          # 只写变量名，值在 env 文件
protocols = ["chat", "responses", "messages"]
quirks = "deepseek"

[[models]]
id = "deepseek-v4.1-flash"                # 调用方只认这个逻辑 id
aliases = ["deepseek-flash"]
context_length = 1048000
max_completion_tokens = 128000
capabilities = ["text", "vision", "tools", "reasoning"]
backends = [
  { provider = "deepseek", upstream = "deepseek-flash" },
]

[[clients]]
name = "hermes"
key_hash = "sha256:..."                   # 由 `umsp client add` 写入
key_prefix = "umsp_ab12cd34"
allowed_models = ["*"]
allowed_protocols = ["chat", "responses", "messages"]
```

- 上游密钥只放 `env`，`config.toml` 只写 `api_key_env` 变量名
- 优先级：`--config` > `$UMSP_CONFIG` > `~/.config/umsp/config.toml`；
  `$UMSP_LISTEN`、`$UMSP_STATE_DIR`、`$UMSP_ENV_FILE` 可覆盖单项
- `/v1/models` 一个逻辑模型一条，不暴露 provider。网关每 12 小时读取各 provider 的模型列表并按 id 去重。列表里带的上下文、输出上限、显示名和能力直接采用。上游给了多档上下文时，`context_lengths` 保留每一档，`context_length` 取最大的那档。上游没给的字段先用文档里的缺省值补，缺省值不会盖掉上游已经给的值；手写 `[[models]]` 只补这之后仍空着的字段。`umsp catalog refresh` 立刻重读。`allowed_models = ["*"]`（或留空）才会把这份完整列表给调用方。
- `provider add --vendor ID` 只收厂商 id 与 key；每次添加分配独立 id

完整 schema、provider/model/client 管理与全部 CLI 示例见
[docs/configuration.md](docs/configuration.md)（英文）。

## 常用命令

```bash
umsp provider list|add|remove|test      # 上游实例；add 使用内置厂商目录
umsp login|logout grok                  # 每个 provider 独立的 Grok 设备码登录
umsp model list|add|remove              # 逻辑模型与别名
umsp model backend add|remove           # 增删后端（--first 提到最前）
umsp client list|add|remove|rotate      # 客户端 key（只存 SHA-256）
umsp env list|set|unset                 # 上游密钥，掩码显示
umsp usage summary|by-model|prune       # 查询请求账本
umsp doctor [--online]                  # 诊断；--online 会花少量 token
```

所有子命令都支持 `--json`。完整子命令表见 [docs/configuration.md](docs/configuration.md)。

## HTTP 端点

| 方法 | 路径 | 鉴权 | 用途 |
|---|---|---|---|
| GET | `/healthz` | 否 | 熔断、失败、最近状态与 fresh `usage` 快照 |
| GET | `/metrics` | 否 | Prometheus 文本，内存聚合，仅回环 |
| GET | `/v1/models` | 是 | 逻辑模型注册表 |
| POST | `/v1/chat/completions` | 是 | Chat Completions，流式/非流式 |
| POST | `/v1/responses` | 是 | Responses，流式/非流式 |
| POST | `/v1/messages` | 是 | Anthropic Messages，流式/非流式 |
| POST | `/v1/messages/count_tokens` | 是 | 后端支持时真代理 |
| HEAD | `/api/hello` | 否 | Claude Code 连接预热 |

端点细节、错误映射、指标、账本字段、systemd、退出码与排障见
[docs/operations.md](docs/operations.md)（英文）。

## Grok 订阅

`umsp provider add --vendor grok` 创建一个由设备码 OAuth 会话支撑的 provider，session 存在
`~/.config/umsp/sessions/<id>.json`（0600），每个 provider id 一份；`umsp login <id>` 登录。
`umsp provider add --vendor chatgpt` 走 Sign in with ChatGPT：浏览器回到 `127.0.0.1` 的 `/auth/callback`，用量打在 `https://api.openai.com/v1/responses`，每次请求 `store: false` 且必须流式。这台机器没有浏览器时，把回调端口用 SSH 转出去再打开登录链接。

> **实验性 / 账号风险。** Grok 走公开 CLI OAuth client 与 `cli-chat-proxy` 订阅端点，不是 xAI
> 官方 API 集成：接口可能随时变化，也可能违反 xAI 服务条款（包括封号）。只在你愿意承担风险的
> 账号上使用。

网关会发送必需的 `x-grok-client-version`；xAI 提高最低版本时，用 `UMSP_GROK_CLI_VERSION`
覆盖。周额度从 billing 接口读取，不来自短周期的 `x-ratelimit-*` 响应头。细节见
[docs/model-routing.md](docs/model-routing.md)（英文）。

## 千问 Token Plan

`umsp provider add --vendor qwen-token-plan` 使用订阅后台显示的千问 Token Plan
（`token-plan.maas.qianwenaiapi.com`），不是按量付费的 DashScope，也不是海外的
`qwencloudapi.com`。`sk-` 开头的通用 Key 或 DashScope 域名不会扣这份套餐。
Chat 走 `https://token-plan.maas.qianwenaiapi.com/compatible-mode/v1`，
Responses 走 `https://token-plan.maas.qianwenaiapi.com/compatible-mode/v1/responses`，
Messages 走 `https://token-plan.maas.qianwenaiapi.com/apps/anthropic`。
密钥变量是 `QWEN_TOKEN_PLAN_API_KEY`。该套餐限定在交互式客户端。
旧的华北 2 域名 `token-plan.cn-beijing.maas.aliyuncs.com` 目前仍可应答，但不是后台给出的地址。

## 文档

| 文档 | 内容 |
|---|---|
| [docs/configuration.md](docs/configuration.md) | 配置 schema、CLI 参考、provider/model/client 流程 |
| [docs/operations.md](docs/operations.md) | 端点、指标、账本、退出码、systemd、排障、测量 |
| [docs/protocols.md](docs/protocols.md) | 各上游 quirks、切换、透明性、SSE、Responses、Messages |
| [docs/model-routing.md](docs/model-routing.md) | 逻辑模型、后端、容量分类、pin、Grok 额度 |
| [docs/protocol-matrix.md](docs/protocol-matrix.md) | 跨协议翻译矩阵与有损字段 |
| [docs/quirks.md](docs/quirks.md) | 上游 quirk 目录 |
| [CONTRIBUTING.md](CONTRIBUTING.md) / [SECURITY.md](SECURITY.md) | 开发规范 / 漏洞报告 |

## 开发

```bash
make check          # fmt + clippy -D warnings + 测试 + 英文检查
./scripts/build-musl.sh
```

仓库正文只允许英文（`AGENTS.md`），`README.zh-CN.md` 是唯一中文文档；`docs/internal/` 里的
里程碑验收快照是历史记录，不代表当前行为。

## License

[MIT](LICENSE) © 2026 YiLongMusk
