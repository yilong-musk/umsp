# UMSP — Universal Model Service Provider

中文（简体） | [English](README.md)

一个**单二进制、只监听回环**的 AI 网关：把 OpenAI Chat Completions / OpenAI Responses /
Anthropic Messages 三种协议收在同一个端口上，按协议原样转发给原生支持该协议的上游。
没有 UI、没有数据库服务、没有 Redis、没有 Docker，也没有预算/限流逻辑。

> 名字里的「Service Provider」是站在客户端视角说的：对调用方而言，UMSP 就是一个能提供
> 任意模型的服务提供方；对上游来说，它仍是网关 / 路由。

- 二进制名：`umsp`
- 配置：`~/.config/umsp/config.toml`
- 密钥：`~/.config/umsp/env`（0600）
- 状态：`~/.local/share/umsp/`（JSONL 请求账本）
- 监听：`127.0.0.1:3460`

> **当前版本状态：M5**。已实现：配置/鉴权、`/v1/models`（一个逻辑模型一条，不暴露 provider）、
> `/healthz`（含熔断状态）、`/metrics`、OpenAI Chat Completions / OpenAI Responses /
> Anthropic Messages 三种协议经**多家上游**（OpenDesign / DeepSeek）的非流式与流式 SSE 透传、
> `/v1/messages/count_tokens`（DS 代理 / OD 501）、**按后端列表切换**、**provider 熔断**、
> **DS 余额健康**、请求账本与 `umsp usage`。
> 预算/RPM 限额不在本期范围。
> 模型与路由设计见 `docs/model-routing.md`；协议细节见 `docs/quirks.md` 与
> `docs/protocol-matrix.md`。`docs/internal/m0-…m5-verification.md` 是各里程碑的历史验收快照，
> 其中仍保留当时的 `od/...` 模型 id，已被现在的「逻辑模型 + backends」取代。

---

## 安装

### 发布包（推荐，服务器不需要 Rust 工具链）

```bash
tar -xzf umsp-<version>-x86_64-linux-musl.tar.gz
install -Dm755 umsp-<version>-x86_64-linux-musl/umsp ~/.local/bin/umsp
install -Dm644 umsp-<version>-x86_64-linux-musl/umsp.service ~/.config/systemd/user/umsp.service
install -Dm644 umsp-<version>-x86_64-linux-musl/config.example.toml ~/.config/umsp/config.example.toml
```

校验：

```bash
sha256sum -c umsp-<version>-x86_64-linux-musl.tar.gz.sha256
file ~/.local/bin/umsp   # → statically linked
```

### 从源码构建（musl 静态）

```bash
sudo apt-get install -y musl-tools          # 提供 musl-gcc
rustup target add x86_64-unknown-linux-musl
./scripts/build-musl.sh                     # 产物在 dist/
```

`scripts/build-musl.sh` 会依次跑 `cargo fmt --check`、`clippy -D warnings`、
`cargo test`，然后构建 musl release 并打包 + 生成 sha256。

---

## 快速开始

```bash
umsp init                                   # 写 config.toml / env / state 目录
$EDITOR ~/.config/umsp/env                  # 填 OPEN_DESIGN_API_KEY=... 与 DEEPSEEK_API_KEY=...
umsp client add --name default              # 打印一次明文 key，形如 umsp_xxx
umsp check --json                           # 全绿后再启动
umsp serve                                  # stdout 打一行 {"event":"ready",...}
```

另一个终端：

```bash
export UMSP_API_KEY=umsp_xxx                 # 刚才打印的那把
curl -s http://127.0.0.1:3460/v1/models -H "x-api-key: $UMSP_API_KEY" | jq
curl -s http://127.0.0.1:3460/v1/chat/completions \
  -H "x-api-key: $UMSP_API_KEY" -H 'Content-Type: application/json' \
  -d '{"model":"deepseek-v4.1-flash","messages":[{"role":"user","content":"只回复：正常"}]}' | jq
```

也可以把 key 放在 `Authorization: Bearer $UMSP_API_KEY` 里，两种方式等价。

Hermes 接入（Chat Completions）：

```bash
hermes chat -q "只回复：正常" \
  --provider custom --base-url http://127.0.0.1:3460/v1 \
  --api-key "$UMSP_API_KEY" --model deepseek-v4.1-flash
```

---

## 子命令

每个子命令都支持 `--json`（stdout 一行合法 JSON）；`--config PATH` 可全局覆盖配置路径。

| 命令 | 说明 |
|---|---|
| `umsp serve [--listen ADDR]` | 启动数据面；stdout 打 ready 行并 flush，日志走 stderr |
| `umsp init [--state-dir PATH] [--force]` | 创建 config.toml、env（0600）、state 目录 |
| `umsp config path\|show [--redact]\|validate` | 查看/校验配置 |
| `umsp check [--json]` | 配置 + 密钥 + 状态目录校验，失败退出码 10 |
| `umsp doctor [--online] [--json]` | 更细的诊断项；`--online` 会真探测上游 |
| `umsp vendor list` | 列出内置厂商（id、端点、协议、quirks、auth、已配置实例数） |
| `umsp provider list\|add\|remove\|test` | 管理上游 provider；`add --vendor ID` 只需厂商 id + key（`grok` 不用 key），端点/协议来自内置目录，每次分配唯一 id；`list` 合并 `/healthz` 的熔断/最近状态 |
| `umsp login <provider>` | 给一个 Grok 订阅 provider 做 device-code 登录。每个 id 有自己的 session 文件 |
| `umsp logout <provider>` | 删除该 provider 的 session 文件 |
| `umsp env list\|set\|unset` | 管理 env 文件里的上游密钥（只显示掩码；`set` 在 TTY 上隐藏输入） |
| `umsp model list [--aliases]\|add\|remove` | 管理本地模型注册表（逻辑 id + aliases） |
| `umsp model backend add\|remove` | 给已有模型挂/摘后端（`--first` 插到最前，即最高优先级） |
| `umsp client list\|add\|remove\|rotate` | 管理客户端 key（只存 SHA-256） |
| `umsp key list\|add\|revoke` | `client` 的兼容别名 |
| `umsp usage summary\|by-client\|by-model\|by-protocol\|prune` | 聚合 JSONL 账本 |
| `umsp probe --provider ID [--model ID]` | 对某 provider 发一条最小请求（chat） |
| `umsp call --model ID --prompt TEXT` | 直接调用上游一次（chat，非流式） |
| `umsp logs tail [--lines N] [--follow]` | 查看请求账本 |
| `umsp logs export --since 24h --out PATH` | 导出账本 |
| `umsp version [--json]` | 版本 + 构建 target |
| `umsp completion bash\|zsh\|fish` | 生成补全脚本 |

示例：

```bash
# 内置厂商目录：端点、协议、quirks 都内置，只能从目录里选
umsp vendor list

# 加 provider：只给厂商 id 和 key（key 从 stdin 读，不进 argv / history）
read -rsp 'DeepSeek key: ' K && echo
printf '%s\n' "$K" | umsp provider add --vendor deepseek

# 同一厂商可以重复添加：每次分配独立 id（deepseek、deepseek-2…），
# 端点来自目录，互不覆盖、互不合并
umsp provider add --vendor deepseek --no-key

# Grok 订阅也是这个形状：一个 provider id 一份 session。
# grok 和 grok-2 不共用 refresh token，要分别登录。
umsp provider add --vendor grok
umsp login grok
umsp provider add --vendor grok
umsp login grok-2
umsp model add --id grok-4.7 --backend grok:grok-4.7 --backend grok-2:grok-4.7

# 实验性 / 账号风险：Grok 走公开 CLI OAuth client 与 `cli-chat-proxy` 订阅端点，不是 xAI 官方 API。
# 接口可能随时变化，也可能违反 xAI 服务条款（包括封号）。只在你愿意承担风险的账号上使用。
# session token 只存放在 sessions/<id>.json（0600），不会写入日志或配置。

# 给已有模型补一个后端（插到最前 = 最高优先级）
umsp model backend add claude-haiku-5.5 \
  --backend deepseek-2:deepseek-flash --first

# env 文件里的密钥只看掩码
umsp env list

# 一个逻辑模型，两个上游后端；顺序即优先级（前面失败才轮到后面）
umsp model add --id deepseek-v4.1-flash \
  --backend open-design:deepseek-v4.1-flash \
  --backend deepseek:deepseek-flash \
  --alias deepseek-flash --context-length 1048000 --max-completion-tokens 128000 \
  --capability text,vision,tools,reasoning --display-name "DeepSeek V4.1 Flash"

umsp client add --name hermes --models 'deepseek-*,claude-*' --protocols chat --json
umsp usage summary --since 24h --json
umsp doctor --json
```

CLI 修改配置时用 `toml_edit` 保留注释与格式，写临时文件后 `rename` 原子替换；
若 `state_dir/umsp.pid` 指向的进程还活着，会自动发 `SIGHUP` 触发热加载（`--no-reload` 关闭）。

---

## HTTP 端点

| 方法 | 路径 | 鉴权 | 说明 |
|---|---|---|---|
| GET | `/healthz` | 否 | 网关与 provider 状态，含 `circuit`/`failures`/`opened_at`/`reason`；`?probe=1` 真探测（缓存 30s，DS 先查余额不花 token） |
| GET | `/metrics` | 否 | Prometheus 文本（`text/plain; version=0.0.4`），内存聚合，不触发上游；仅回环（非回环来源 403 `loopback_only`） |
| HEAD | `/api/hello` | 否 | 204，Claude Code 连接预热 |
| GET | `/v1/models` | 是 | 本地注册表，扁平 `context_length` / `max_completion_tokens` / `capabilities` / `display_name` |
| POST | `/v1/chat/completions` | 是 | 非流式转发 + 流式 SSE 透传（`stream: true`，OD / DS） |
| POST | `/v1/responses` | 是 | OpenAI Responses 非流式 + 流式（M2 OD、M4a DS） |
| POST | `/v1/messages` | 是 | Anthropic Messages 非流式 + 流式（M3 OD、M4a DS；保留 `?beta=true`） |
| POST | `/v1/messages/count_tokens` | 是 | DS 代理到 `{ds_anthropic}/v1/messages/count_tokens`（200）；OD 501 `not_supported_error`（不转发） |
| any | `/v1/responses/*` | 是 | `501 unsupported_endpoint`：`input_tokens`、`GET /{id}`、cancel、input_items 不代理 |

鉴权接受 `Authorization` 头的 Bearer 方案，或 `x-api-key` 头（值都是同一个客户端 key）；客户端 key 只存 `sha256:<hex>` + 前缀，比较用常量时间实现，日志不出现明文。

### Prometheus `/metrics`（M5）

```bash
curl -s http://127.0.0.1:3460/metrics
```

Prometheus 文本格式（`text/plain; version=0.0.4; charset=utf-8`），全部来自进程内存，重启清零；渲染只读内存、不发上游请求，响应 < 1 ms（实测）。

| 指标 | 类型 | 标签 | 语义 |
|---|---|---|---|
| `umsp_requests_total` | counter | `provider,model,protocol,status,stream` | 每**完成一个客户端请求**记 1 次；`model` 是逻辑模型 id，`provider` 是实际服务的那家（切换后就是切换后的），与账本行一一对应 |
| `umsp_request_duration_seconds` | histogram | `provider,model,protocol` | 端到端耗时，buckets 0.05/0.1/0.25/0.5/1/2/5/10/30/60/300 |
| `umsp_ttft_seconds` | histogram | `provider,model,protocol` | 流式首字节；非流式没有 TTFT，不产生序列 |
| `umsp_tokens_total` | counter | `provider,model,direction` | `direction=input\|output\|cached\|reasoning`，只记上游**真的报了**的 token（0 不建序列） |
| `umsp_upstream_errors_total` | counter | `provider,code` | **每次失败的上游尝试**都记一次（`upstream_502`、`upstream_timeout`…），包括被重试/fallback 吸收的主 provider 失败 |
| `umsp_circuit_open` | gauge | `provider` | 1 = 熔断打开或半开，0 = 关闭；配置里的 provider 都会出现 |
| `umsp_inflight_requests` | gauge | `provider` | 正在等某个 provider 的请求数；请求/流结束即回落（取消也会归还） |

记账规则（与 `umsp usage` 对齐）：

- 一个客户端请求在 `/metrics` 里只算一次：`requests_total` + 一次 `duration`（+ 流式的 `ttft`）；`requests_total` 的序列之和 == 账本行数。
- 后端切换命中时不会把被跳过后端的失败再记一条请求，只把它记进 `upstream_errors_total`；因此 `upstream_errors_total` 可以大于 `requests_total`。
- 标签低基数：没有 `request_id`、客户端名或 key；`model` 是逻辑模型 id（不含 provider 前缀），上游模型名只出现在账本与运行日志里。
- `/metrics` 和 `/healthz` 一样不需要客户端 key，且只接受回环来源；监听地址配成非回环时这两个端点也会回 `403 loopback_only`。

### 网关自己产生的错误

协议原生形状 + `X-UMSP-Code` 响应头：

```json
{"error":{"message":"unknown model 'nope'","type":"not_found_error","code":"model_not_found","param":null}}
```

上游失败**不会原样透传**：401/402/403/429/5xx 统一映射为网关错误（见下文「对客户端完全透明」），
只有客户端自身引起的 4xx 保留状态码与（脱敏后的）body。

### OpenDesign quirks（M0 已实现）

| 现象 | 处理 |
|---|---|
| 截断时 choice 缺 `finish_reason` | `completion_tokens >= max_tokens` → `length`，否则 `stop` |
| 思考吃光预算 → `choices: []` | 合成 `{index:0,message:{role:"assistant",content:""},finish_reason:"length"}` |
| 同时给 `max_tokens` + `max_completion_tokens` | 删除后者，只保留 `max_tokens` |
| 非流式 `message.tool_calls[].index` | 剥掉 |
| `message.reasoning_details` | 原样保留 |
| 未知字段 | 请求与响应都不改写（只改 `model` 与上述 quirk 字段） |

### DeepSeek 官方 provider（M4a）

`provider = "deepseek"`、`quirks = "deepseek"`：三个入口共用同一个模型 ID
（`deepseek-v4.1-flash`，别名 `deepseek-flash`）。DeepSeek 三种协议都支持，这些路由保持同协议透传。

| 入口 | DS 上游路径 |
|---|---|
| `/v1/chat/completions` | `https://api.deepseek.com/chat/completions` |
| `/v1/responses` | `https://api.deepseek.com/responses` |
| `/v1/messages` | `https://api.deepseek.com/anthropic/v1/messages` |
| `/v1/messages/count_tokens` | `https://api.deepseek.com/anthropic/v1/messages/count_tokens` |

- **Chat 请求 quirk**（跨 provider 回放必需）：
  - 请求带 `tools` 时，每条历史 assistant 消息必须有非空 `reasoning_content`；
    缺失时先用 `reasoning` 文本补，仍无则补一个空格（否则 DS 直接 400）。
    账本 `quirks` 记 `filled_reasoning_content` / `filled_reasoning_placeholder`。
  - 删除 OD 专有的 `reasoning_details`（含 encrypted），只保留纯文本
    （`dropped_reasoning_details`）；未知字段不动。
  - `user_id` 用于 KVCache 隔离：客户端显式给了就尊重；否则注入
    `umsp-<sha256(client name) 前 16 hex>`（跨重启稳定，`injected_user_id`）。
  - `thinking` / `reasoning_effort` 原样透传，**不补默认值**（DS 思考模式默认开启）。
- **Responses**：DS 的流本身带 `event:` 行，`responses_emit_event_lines` 不会重复插入；
  `reasoning_text.*` / `function_call` 事件原样透传；usage 取自
  `response.completed.response.usage`；`previous_response_id` 仍是 400。
- **Messages**：走 `anthropic_base_url`（`/anthropic/v1/messages`），`?beta=true` 等
  query 原样；`anthropic-version` 缺省补 `2023-06-01`；`thinking` / `tool_use` /
  `signature_delta` 原样透传；`count_tokens` 真代理（实测 200 `{"input_tokens":N}`，
  即使余额为负也可用）。
- **usage 归一化**：Messages 的 cache creation 两种形状都认（OD 嵌套
  `cache_creation.ephemeral_*`、DS 扁平 `cache_creation_input_tokens`）。

### 后端切换与熔断

- **backends**：`[[models]] backends = [{ provider, upstream }, ...]`，顺序即优先级。每个请求按顺序
  尝试，只切到**同协议**且能服务该协议的后端；每个后端最多试一次，因此不可能循环。切换只发生在
  **尚未向客户端发出任何响应字节**之前（SSE 一旦返回 200 就不再切）。
- **可重试**：连接错误、超时、429、5xx、402；**不重试**：400/401/403/404/422（401 是上游 key
  配置错误，换后端只会把几家都打爆）。所有后端都失败时，客户端拿到中性网关错误（`upstream_error`，502）；
  上游原始状态与 body 留在账本（`upstream_status` / `upstream_body`）。`count_tokens` 只会选实现了它的后端。
- **熔断**：每 provider 一个内存状态机。30 秒内 5 次**可重试**失败 → `open` 30 秒 → `half_open`
  放 1 个探针；成功回 `closed`，探针失败回 `open`。DS `402` → `open` 10 分钟，后台每 5 分钟查
  `GET /user/balance`，额度恢复自动 `closed`；OD `401/402` → `open` 5 分钟，后台用
  `max_tokens=1` + `reasoning_effort=none` 的推理探针恢复。`400/404/422` 不计失败。
- **半开探针槽位**：探针槽由 RAII guard 持有——请求被取消（客户端断开）或探针拿到非重试 4xx 时
  **只释放槽位、不计失败**，下一个请求可继续探针；探针拿到 4xx 视为 provider 可达，直接回
  `closed`。15 分钟兜底回收泄漏的槽位。并发请求在探针期间仍一律拒绝。
- 某后端熔断打开时请求会跳过它，继续尝试后面 closed 的后端；全部不可用时返回协议原生 `503` +
  `X-UMSP-Code: temporarily_unavailable`，**不碰上游**。
- `/healthz` 每个 provider 带 `circuit`（`closed|open|half_open`）、`failures`、`opened_at`、
  `reason`；`?probe=1` 对 DS 先查余额（`is_available=false` 直接 degraded，不花 token），其余
  provider 用最小推理探针（缓存 30s）。
- 账本：`model` 是**逻辑模型 id**，`provider` / `upstream_model` 记录**实际应答**的后端；
  `fallbacks` 是切换次数（0 = 首选后端一次成功），`quirks` 里留 `fallback_from:<首选 provider>`。
  日志事件：`fallback`、`circuit_open`、`circuit_close`（含 provider/model/reason，不含 key）。

限制（有意不做）：

- **没有预算 / RPM / 429 预算拒绝**，不做多副本 / Redis。只有选中的 provider 不会说客户端协议时才做跨协议翻译。
- Chat / Responses 的流式响应头超时沿用全局 `stream_header_timeout_s`（默认 300s）；
  DS 文档建议 600s（10 分钟未开始推理才断连），长思考任务可自行调大。

### 对客户端完全透明（provider 不可见）

调用方只看到 UMSP 这一个 provider：切换、熔断与上游身份都留在网关内部。

- 响应头只保留 `x-umsp-request-id`（以及表示层 `content-type` / SSE 头）；上游的 `server`、
  `x-request-id`、`x-ds-trace-id`、`cf-ray`、`x-should-retry`、`anthropic-ratelimit-*`、cookie
  一律不转发。
- 上游 2xx 的 body 与流式字节**原样**返回——只去掉身份信息，不动内容。
- 上游失败的映射（客户端既看不到哪家上游，也看不到上游原文）：

| 上游情况 | 客户端状态 | `X-UMSP-Code` |
|---|---|---|
| 400/404/409/413/415/422（请求本身有错） | 同状态码 | 无（保留上游形状，body 已脱敏） |
| 401/403（网关到上游的凭据问题） | 502 | `upstream_auth_failed` |
| 402（额度耗尽） | 502 | `upstream_quota_exhausted` |
| 429 | 429 | `rate_limited` |
| 5xx | 502 | `upstream_error` |
| 连接失败 / 超时 | 502 / 504 | `upstream_unreachable` / `upstream_timeout` |
| 全部后端熔断 | 503 | `temporarily_unavailable` |

- 上游原始状态与（截断到 2 KB 的）body 落账本：`upstream_status`、`upstream_body`；用响应头里的
  `x-umsp-request-id` 到 `~/.local/share/umsp/logs/` 对账。
- 4xx 透传的 body 会把 provider id 与上游模型名替换成逻辑模型 id（`umsp` / `<model-id>`），
  其余文字保留，方便调用方修正请求。
- 残留：`/healthz`、`/metrics` 是运维端点（同一 loopback 端口、未按 client key 鉴权），能看到
  provider 名；流式拿到 200 之后，上游事件体里自带的错误文本会按协议原样透过。

### 流式（SSE，M1）

`"stream": true` 时网关不再缓冲，也不重新序列化：

- **逐字节透传**上游 body：SSE 注释、空 `delta:{}`、`delta.reasoning`、`id:""`、
  tools delta、`data: [DONE]` 一律原样到达客户端；上游没发 `[DONE]` 时补一次。
- 响应头：`Content-Type: text/event-stream`、`Cache-Control: no-cache`、
  `X-Accel-Buffering: no`、无 `Content-Length`，另加 `x-umsp-request-id`；
  **不转发任何上游头**。
- 客户端没给 `stream_options.include_usage` 时网关注入 `{"include_usage": true}` 以便记账
  （账本 `quirks` 记 `injected_include_usage`）；显式 `false` 时尊重客户端，
  账本记 `usage_missing`，不估算 token。
- 上游静默 15s（`stream_keepalive_s`）发一次 `: keep-alive` 注释。
- 上游响应头 300s（`stream_header_timeout_s`）、流空闲 300s（`stream_idle_timeout_s`）
  超时；流式客户端**不设**总超时，长流不会被 600s 截断。
- 已发出 200 后上游断流/报错：发一条协议原生
  `data: {"error":{"message":…,"type":"upstream_error","code":"upstream_stream_error"}}`，
  再 `data: [DONE]`；不伪造 `finish_reason` 或 usage，账本 `error_code=upstream_stream_error`。
- 上游在响应头之前失败：走统一失败映射（见「对客户端完全透明」），**不转成 SSE**。
- 客户端断开：立刻停止转发并取消上游请求；账本 `client_disconnected=true`
  （若连上游响应头都没等到就断开，记一行 `status=499` 的 `client_disconnected` 行，
  不丢请求）。

限制（M1 / Chat）：

- 非布尔 `stream`（`"yes"`、`1`、`null`）按 `false` 处理，转发前归一化为 `false`
  （账本 `quirks` 记 `stream_non_boolean`）：严格上游会直接以 400 拒绝非布尔值。
- 不支持请求体压缩（`Content-Encoding: gzip` 不处理）；
- 流式响应不做 gzip。

### OpenAI Responses（M2）

`POST /v1/responses` 接 OpenDesign 与 DeepSeek（M2 / M4a）。整个响应体默认逐字节透传。

- **请求**：只改写 `model`（→ `model.upstream`）。`input`（字符串 / 消息数组 /
  `function_call_output`）、`instructions`、`tools`、`tool_choice`、
  `parallel_tool_calls`、`max_output_tokens`、`reasoning`、`text`、`store`、
  `metadata`、`include`、`truncation`、`stream` 与未知字段全部原样透传；不补任何
  默认值；请求体上限仍是 50 MiB（超出 413）。
- `previous_response_id` 一律 `400 unsupported_parameter`（OD 对未知 id 报错，
  DS 会静默忽略导致上下文错乱）：客户端必须每次发送完整 input 历史。
  显式 `null` 视作未设置（不拒绝），避免总是序列化该 key 的客户端被误伤。
- **非流式**：2xx 的状态码与 body 原样返回；上游失败走统一映射（见「对客户端完全透明」）；
  usage 从顶层 `usage.input_tokens` / `output_tokens` /
  `input_tokens_details.cached_tokens` / `output_tokens_details.reasoning_tokens`
  读取后只写账本，不改写 body；没有 usage 记 `usage_missing`，不估算。
- **流式**：upstream 的 `data:` 行逐字节透传。OD 只发 `data:`、不发 `event:`，
  网关默认按 data JSON 的 `type` 补 `event: <type>` 行（同一事件块已有则不重复；
  `[DONE]`、注释、空行、顺序都不动）。`responses_emit_event_lines = false`
  时完全原样。
- 15s `: keep-alive`；客户端断开立刻取消上游并记 `client_disconnected`（响应头
  之前断开也写 499 行）；已发 200 后上游断流 → 发协议原生
  `event: error` + `data: {"type":"error",...}`，再 `data: [DONE]`，不伪造
  `response.completed`；上游响应头前非 2xx 时走统一失败映射，不转 SSE。
- 不代理 `POST /v1/responses/input_tokens`、`GET /v1/responses/{id}`、cancel、
  `input_items`（统一 `501 unsupported_endpoint`）；不实现 WebSocket Responses。

Codex 接入（`~/.codex/config.toml`）：

```toml
model_provider = "umsp"
model = "deepseek-v4.1-flash"

[model_providers.umsp]
name = "AI Gateway"
base_url = "http://127.0.0.1:3460/v1"
wire_api = "responses"
env_key = "UMSP_API_KEY"
supports_websockets = false
```

`base_url` 必须带 `/v1`（Codex 会自己追加 `/responses`）；`supports_websockets = false`
避免 Codex 先重试 5 次 WebSocket 再回落 HTTPS。

### Anthropic Messages（M3）

`POST /v1/messages` 接 OpenDesign 与 DeepSeek（M3 / M4a）。用于 Claude Code 等
Anthropic 客户端。provider 会说 Messages 时请求原样转发；否则这一跳从它会说的协议翻译过来。

- **入口**：`POST /v1/messages`，query string 原样带给上游（Claude Code 会发
  `?beta=true`）。`POST /v1/messages/count_tokens`：路由到 DS 时**真代理**到
  `{anthropic_base_url}/v1/messages/count_tokens`（200 `{"input_tokens":N}`）；
  路由到 OD（上游 404）时网关返回 Anthropic 原生
  `501 {"type":"error","error":{"type":"not_supported_error","message":"count_tokens is not supported by any backend of model 'claude-haiku-5.5'"}}`，
  且不转发上游。Claude Code 会回落到字符估算，不会断。
- **请求**：只改写 `model`（→ `model.upstream`）。`max_tokens`、`messages`、`system`
  （字符串或数组）、`tools`、`tool_choice`、`thinking`、`metadata`、`stop_sequences`、
  `stream`、`temperature`、`top_p`、`top_k`、`output_config`、`context_management`、
  `cache_control` 及未知 beta 字段全部原样，不合并/重排/转字符串，不补默认值。
- **请求头**：`anthropic-version`、`anthropic-beta`、其他 `anthropic-*`、
  `x-claude-code-*`、`x-stainless-*` 原样转发（**不按 beta 值白名单**）；客户端没给
  `anthropic-version` 时补 `2023-06-01`（OD 强制）。客户端 `Authorization` / `x-api-key`
  不转发，上游统一注入 provider key 的 `Authorization: Bearer <provider-key>``。
- **非流式**：2xx 的状态码与 body 原样返回（`thinking` / `redacted_thinking` /
  `text` / `tool_use` 原样）；usage 读 `input_tokens` / `output_tokens` /
  `cache_read_input_tokens`，cache creation 兼容 OD 嵌套
  （`cache_creation.ephemeral_5m_input_tokens` / `1h`）与 DS 扁平
  （`cache_creation_input_tokens`），只写账本不改 body；无 usage 记 `usage_missing`。
- **流式**：事件序列**逐字节透传**（`message_start`、`content_block_*`、
  `thinking_delta` / `signature_delta` / `input_json_delta` / `text_delta`、
  `message_delta`、`message_stop`、`ping`、`error`），**不会**追加 `[DONE]`。
  上游静默 15s 时网关发 `event: ping` + `data: {"type":"ping"}`（不是注释）。
  已发 200 后上游断流或未发 `message_stop` 就关闭 → 发 `event: error` +
  `data: {"type":"error","error":{...}}`，**绝不伪造 `message_stop`**。
- **错误**：网关自己产生的错误用 Anthropic 形状
  `{"type":"error","error":{"type":...,"message":...}}`（401/403/404/413/501 等）；
  上游失败统一映射（见「对客户端完全透明」）；`retry-after` 若是 HTTP date
  会转成整数秒，`x-should-retry` / `anthropic-ratelimit-unified-*` 原样。
- **模型发现**：`HEAD /api/hello` 204，`GET /v1/models` 本地注册表（毫秒级）。
  Claude Code 只保留 `claude` / `anthropic` 的模型 id，所以在 `config.toml` 里登记
  OD 的真实模型（例如 `claude-haiku-5.5`）后，开启
  `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1` 即可被发现。

Claude Code 接入（在用户电脑上执行；网关只监听回环，需 SSH 端口转发或本机运行）：

```bash
# 本机运行网关时
export ANTHROPIC_BASE_URL=http://127.0.0.1:3460
export ANTHROPIC_AUTH_TOKEN=umsp_xxx        # 或 ANTHROPIC_API_KEY
export ANTHROPIC_MODEL=claude-haiku-5.5
claude -p "用 Bash 执行 date 并告诉我结果"
```

远程机器上运行网关时，先 `ssh -N -L 3460:127.0.0.1:3460 <server>`，再设
`ANTHROPIC_BASE_URL=http://127.0.0.1:3460`。

注意：OD 只接受 `anthropic-version: 2023-06-01`；网关按规格原样转发客户端给的值
（缺省才补）。新版 Anthropic SDK（实测 2.10.0 默认 `2026-10-21`）会被 OD 以
`400 unsupported anthropic-version` 拒绝，此时把客户端版本固定为 `2023-06-01`
（SDK 可用 `default_headers={"anthropic-version": "2023-06-01"}`）。

---

## 配置 schema

`~/.config/umsp/config.toml`：

```toml
[server]
listen = "127.0.0.1:3460"          # 只允许回环；非回环 doctor 会 warn
max_body_bytes = 52428800          # 50 MiB，超出 413
request_timeout_s = 600            # 非流式总时长
stream_idle_timeout_s = 300        # 流式：上游两次数据之间的最长间隔
stream_header_timeout_s = 300      # 流式：等待上游响应头的上限（OD）
stream_keepalive_s = 15            # 流式：向上游静默多久发一次 ": keep-alive"
state_dir = "~/.local/share/umsp"   # 账本与 pid 文件
responses_emit_event_lines = true  # Responses 流式是否补 `event: <type>` 行（默认 true）
log_retention_days = 30            # umsp usage prune 的默认保留期

[[providers]]
id = "open-design"
vendor = "open-design"             # 内置厂商：端点与 quirks 由目录决定
base_url = "https://amr-link.open-design.ai/v1"
anthropic_base_url = ""            # 可选；Messages / count_tokens 走它（M3 / M4a）
api_key_env = "OPEN_DESIGN_API_KEY" # 只写变量名，值放 env 文件
protocols = ["chat", "responses", "messages"]
quirks = "opendesign"              # opendesign | deepseek | none

[[providers]]
id = "deepseek"
vendor = "deepseek"
base_url = "https://api.deepseek.com"
anthropic_base_url = "https://api.deepseek.com/anthropic"
api_key_env = "DEEPSEEK_API_KEY"
protocols = ["chat", "responses", "messages"]
quirks = "deepseek"                # count_tokens 代理也由它决定

# 厂商目录在代码里（src/vendors.rs）：open-design、deepseek、grok。
# 新增厂商需要改这一处并重新构建；配置里出现未知 vendor 会直接校验失败。
# `auth = "oauth"`（grok）按 provider id 把 session 放在 sessions/<id>.json。
# `auth = "api_key"`（默认）仍然从 env 文件读 api_key_env。

[[models]]
id = "deepseek-v4.1-flash"          # 逻辑模型：调用方只认这个 id
aliases = ["deepseek-flash"]        # 兼容旧客户端
context_length = 1048000            # 拍平后的 context_length（Hermes 读这个）
max_completion_tokens = 128000
capabilities = ["text", "vision", "tools", "reasoning"]
display_name = "DeepSeek V4.1 Flash"
backends = [                        # 顺序即优先级；一个失败才轮到下一个
  { provider = "open-design", upstream = "deepseek-v4.1-flash" },
  { provider = "deepseek",    upstream = "deepseek-flash" },
]
created = 1789026056

[[clients]]
name = "hermes"
key_hash = "sha256:<64 hex>"       # 由 `umsp client add` 写入
key_prefix = "umsp_ab12cd34"
allowed_models = ["deepseek-*", "claude-*"]   # glob，空 = *
allowed_protocols = ["chat", "responses"]  # chat / responses / messages
```

约定：

- 上游密钥只放 `~/.config/umsp/env`，配置里只写 `api_key_env`；env 文件权限必须是 0600。
- 环境变量优先于 env 文件（systemd `EnvironmentFile` 与 shell 覆盖都生效）。
- 配置优先级：`--config` > `$UMSP_CONFIG` > `~/.config/umsp/config.toml`；
  `$UMSP_LISTEN` 覆盖监听地址、`$UMSP_STATE_DIR` 覆盖状态目录、`$UMSP_ENV_FILE` 覆盖 env 文件路径。
- `/v1/models` 每个逻辑模型只出现一条，不含 provider / 上游字段；别名用 `umsp model list --aliases` 查看。

---

## 请求账本与用量

每个请求一行 JSON，写在 `state_dir/logs/YYYY-MM-DD.jsonl`：

```
ts, request_id, client, protocol, requested_model, provider, upstream_model, stream,
status, upstream_status, prompt_tokens, completion_tokens, cached_tokens,
reasoning_tokens, ttft_ms, duration_ms, bytes_in, bytes_out, error_code,
upstream_request_id, client_disconnected, fallbacks, quirks
```

- 默认不记 prompt/正文。
- 每行一次 `write_all`（单行原子），50 并发压测下不会丢行/串行（M5 回归测试 `concurrent_appends_never_merge_or_drop_lines`）。

`fallbacks` 与 provider 字段的记账语义：

| 字段 | 含义 |
|---|---|
| `provider` / `upstream_model` | **实际服务**这次请求的后端：切换命中时就是切换后的那一家 |
| `fallbacks` | 切换次数：`0` = 首选后端一次成功，`1..n-1` = 跳过并切换了几次 |
| `quirks` | 切换时追加 `fallback_from:<首选 provider id>`，另外保留 `injected_include_usage`、`usage_missing` 等标记 |
| `status` | 返回给客户端的 HTTP 状态（切换成功就是 200，而不是首选后端的 5xx） |

`/metrics` 与它一致：`umsp_requests_total` 按最终 provider 记 1 次；主 provider 的失败只进 `umsp_upstream_errors_total`。

查询与清理：

```bash
umsp usage summary --since 24h --json                    # 总量/分桶/tokens
umsp usage summary --since 7d --provider deepseek --json # 只看实际服务的 provider
umsp usage summary --since 7d --client planner --protocol chat --json
umsp usage by-client|by-model|by-protocol --since 30d --json
umsp usage prune --days 30 --json    # 保留最近 30 天；今天的文件永不删除
umsp usage prune --before 30d        # 等价写法（与 --days 互斥）
```

- `summary` 的 `requests` / `by_status` / `by_protocol` / `by_model` / `by_client` / `by_provider` 都从同一批记录聚合，分桶之和恒等于 `requests`（单元测试 `aggregate_buckets_and_tokens_match_the_record_count`）。
- `prune` 只按日期整文件删除，`--days 0` 也不会删今天的账本；删除数量与路径在 `--json` 的 `removed_count` / `removed` 里。
- 保留期见 `log_retention_days`（serve 不会自动删；prune 由人/定时任务调用）。

---

## 退出码

| 码 | 含义 |
|---|---|
| 0 | 成功 |
| 2 | 用法错误（含 clap 解析失败、重复添加客户端） |
| 3 | 网络/超时 |
| 4 | 上游封禁/限流/余额不足 |
| 5 | 目标不存在（模型/provider/client） |
| 6 | 熔断中（M4 起可能返回） |
| 10 | 其他（含 `umsp check`/`doctor` 校验失败） |

`--json` 模式下失败也会在 stdout 打一行：

```json
{"ok":false,"error":{"code":"CONFIG_INVALID","message":"..."}}
```

---

## systemd（user unit）

```bash
install -Dm644 deploy/umsp.service ~/.config/systemd/user/umsp.service
systemctl --user daemon-reload
systemctl --user enable --now umsp
systemctl --user status umsp
journalctl --user -u umsp -f
```

`EnvironmentFile=%h/.config/umsp/env` 由 systemd 注入上游密钥；`ProtectHome=read-only` +
`ReadWritePaths=%h/.local/share/umsp` 保证只有账本目录可写。

unit 里已经写死的硬化项（`deploy/umsp.service`，`systemd-analyze verify` 通过）：

| 指令 | 值 | 作用 |
|---|---|---|
| `MemoryMax` | `256M` | 超过即被 cgroup OOM 杀掉并 `Restart=on-failure` 重启（实测峰值 14 MB，余量充足） |
| `TasksMax` | `256` | 线程/任务数上限 |
| `LimitNOFILE` | `8192` | 并发连接 fd 上限 |
| `NoNewPrivileges` / `ProtectSystem=strict` / `ProtectHome=read-only` | — | 只读系统与家目录，只放开账本目录 |
| `PrivateTmp` | `true` | 私有 /tmp |

前置条件：`~/.local/bin/umsp` 存在、`umsp init` 已生成 `~/.config/umsp/{config.toml,env}`、`umsp doctor` 全绿。
`umsp doctor` 会在 serve 运行时探测 `/metrics`（跳过=未运行，不算失败）。

---

## 排错

| 症状 | 排查 |
|---|---|
| 启动即退出 | `umsp check --json` 看 `errors`；`umsp doctor` 看具体项 |
| 所有请求 401 | 没配 client 或 key 不对：`umsp client list`（只看前缀），必要时 `umsp client rotate NAME` |
| 404 `model_not_found` | `umsp model list` 确认 canonical id；别名用 `--aliases` 查 |
| 403 `model_not_allowed` | 该 client 的 `allowed_models` glob 不匹配 |
| 流式卡住/极慢 | 看账本 `ttft_ms`；`upstream_stream_timeout` 表示上游 300s 没给数据，可调 `stream_idle_timeout_s` |
| 流中收到 `upstream_stream_error` | 上游断流；网关已按协议原生形状补发错误事件与 `[DONE]`，客户端可重试 |
| 502 `upstream_unreachable` / 504 `upstream_timeout` | `umsp probe --provider open-design`；检查网络与 `base_url` |
| 500 `upstream_key_missing` | `~/.config/umsp/env` 里没有对应 `api_key_env`（日志里有 provider 名） |
| 413 | 请求体超过 `server.max_body_bytes` |
| 上游 401/5xx 变成 502 | 设计如此：上游失败对外统一脱敏；看账本 `upstream_status` / `upstream_body` 找原文 |
| 改了配置不生效 | `umsp config validate` 后 `kill -HUP $(cat ~/.local/share/umsp/umsp.pid)` |
| 日志里找请求 | `umsp logs tail --lines 50`（stderr 是运行日志 JSON） |

`umsp doctor --online` 会对每个 provider 发一条 `max_tokens=1` 的真实请求（花极少量 token）。

---

## 实测（M0，2026-11-19，2 vCPU / 2 GB 云主机）

| 指标 | 实测 |
|---|---|
| release 二进制（musl, strip + LTO） | **6,309,264 B ≈ 6.02 MiB** |
| 空载 RSS | **4,228 kB ≈ 4.13 MiB**（37 次请求后 5,796 kB，VmHWM 6,000 kB） |
| `GET /v1/models` 耗时 | 20 次平均 **0.53 ms**（min 0.41 / max 1.87 ms） |
| `GET /healthz` 耗时 | 20 次平均 **0.42 ms** |
| 非流式 Chat 往返（OD，单次） | **1.50–1.62 s**（几乎全部是上游耗时） |
| 冷启动（进程启动 → ready 行） | **9 ms**（`/proc` 计时；含 shell 启动约 31 ms） |
| 线程数 | 3 |

构建/测试命令与完整证据见 `docs/internal/m0-verification.md`。

已知取舍：请求体会整体读入内存（上限 50 MiB），因此处理接近上限的请求时 RSS 峰值约 57 MiB；
M1 的流式转发改为逐块处理。

---

## 负载与资源（M5，2026-11-19）

`bash scripts/load-test.sh`（自建 fake upstream + 独立配置/状态目录，跑完自动清理）在2 vCPU / 2 GB 云主机 上实测：

| 场景 | 结果 |
|---|---|
| 50 并发非流式 × 30s | **100,944 请求，0 失败**，3,362 rps，p95 22.3 ms，RSS 峰值 **14.0 MB** |
| 20 并发流式 × 60s | **16,848 请求，0 失败**，TTFT p50 **4.1 ms** / p95 6.1 ms，RSS 13.7 MB |
| 固定 10,000 请求 | 3.36 s 完成（2,975 rps），RSS **13.7 MB → 13.7 MB**（无增长） |
| 冷启动 → `/healthz` 200 | **16–136 ms**（含轮询粒度） |
| 空载 RSS / 二进制 | **7.3 MB** / **6,612,368 B ≈ 6.31 MiB**（musl release，stripped；glibc release 6,491,400 B） |
| `/metrics` 与账本一致性 | `umsp_requests_total` 序列和 **127,792** == JSONL 行数 **127,792**；`ttft_seconds_count` 16,848 == 流式请求数 |

压测同时发现并修复了一个真实缺陷：并发下单行账本用 `writeln!` 会拆成两次 `write`，50 并发时丢了 2,790/129,279 行（2.2%）。现在单行一次 `write_all` + 进程内互斥。完整命令与输出见 `docs/internal/m5-verification.md`。

---

## 已知限制

- `/metrics` 只在进程内存里聚合，重启清零；没有远端存储，也不做多副本聚合（单实例网关，符合本期范围）。
- `umsp usage` 是同步扫 JSONL；单日百万行级别会慢（实测 12.8 万行约 0.4 s）。日志保留靠 `umsp usage prune --days N` 手动/定时触发。
- `cargo deny` 有一个已记录的例外：`RUSTSEC-2026-0543`（chrono 标记 unmaintained，无 CVE、无安全升级）；替换成 jiff/time 牵涉账本、CLI 与流解析的时间戳，单独立项做。
- 公共入口（Caddy/TLS）、多副本/Redis、预算/RPM、UI 都不在本期范围内。

---

## 许可

[MIT](LICENSE) © 2026 YiLongMusk
