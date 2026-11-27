# UMSP — Universal Model Service Provider

[![CI](https://github.com/yilong-musk/umsp/actions/workflows/ci.yml/badge.svg)](https://github.com/yilong-musk/umsp/actions/workflows/ci.yml)
[![Rust](https://img.shields.io/badge/Rust-1.85+-000000?logo=rust&logoColor=white)](https://www.rust-lang.org)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

[English](README.md) | [简体中文](README.zh-CN.md) | [繁體中文](README.zh-TW.md) | [日本語](README.ja.md) | 한국어 | [Español](README.es.md) | [Français](README.fr.md) | [Deutsch](README.de.md) | [Português](README.pt.md) | [Русский](README.ru.md)

**단일 바이너리, 루프백만** 듣는 AI 게이트웨이입니다. 한 포트에서 OpenAI Chat Completions, OpenAI Responses, Anthropic Messages를 열고, 각 요청을 같은 프로토콜을 말하는 업스트림으로 넘깁니다. 업스트림이 말하지 못하면 번역합니다. UI, 데이터베이스, Redis, Docker는 없습니다.

- `umsp` 바이너리 하나. 설정은 `~/.config/umsp/config.toml`, 키는 `~/.config/umsp/env`(0600), JSONL 요청 원장은 `~/.local/share/umsp/`.
- 논리 모델: 호출자는 모델 id 하나만 봅니다. 각 모델에는 순서가 있는 `backends`가 있습니다.
- 용량을 보고 고릅니다. 신선한 사용량(U)이 먼저, 데이터 없음(X)이 다음, 잔액만(B)이 마지막입니다. 성공한 provider는 다섯 시간 고정됩니다.
- provider별 차단기, DeepSeek 잔액 건강, Grok 주간 할당량 폴링. Qwen Token Plan에는 둘 다 없어 X에 남습니다.
- 클라이언트 요청에서는 provider가 보이지 않습니다. 업스트림 오류는 중립적인 게이트웨이 오류로 옮깁니다.

## 빠른 시작

### 설치

릴리스 패키지(서버에 Rust 도구 사슬은 필요 없습니다):

```bash
tar -xzf umsp-<version>-x86_64-unknown-linux-musl.tar.gz
install -Dm755 umsp-<version>-x86_64-unknown-linux-musl/umsp ~/.local/bin/umsp
sha256sum -c umsp-<version>-x86_64-unknown-linux-musl.tar.gz.sha256
file ~/.local/bin/umsp                   # 정적 링크
```

주석이 달린 태그 `vX.Y.Z`를 push하면(번호는 `Cargo.toml`의 `version`과 같아야 함) 이 아카이브가 minisign으로 서명된 GitHub Release가 됩니다([`umsp.pub`](umsp.pub)). 설치는 직접 합니다. 저장소는 호스트 접근도 갖지 않습니다. [릴리스 설치](docs/operations.md#installing-a-release)를 보세요.

### 소스에서 빌드

```bash
sudo apt-get install -y musl-tools
rustup target add x86_64-unknown-linux-musl
git clone https://github.com/yilong-musk/umsp.git && cd umsp
./scripts/build-musl.sh                  # 출력은 dist/
```

### 실행

```bash
umsp init                                # config.toml, env(0600), 상태 디렉터리
$EDITOR ~/.config/umsp/env               # 업스트림 키. 예: DEEPSEEK_API_KEY=...
umsp client add --name default           # 클라이언트 키는 한 번만 출력 (umsp_...)
umsp check --json                        # 초록이어야 함
umsp serve                               # {"event":"ready",...} 를 출력
```

다른 터미널에서 호출:

```bash
export UMSP_API_KEY=umsp_xxx
curl -s http://127.0.0.1:3460/v1/models -H "x-api-key: ***" | jq

curl -s http://127.0.0.1:3460/v1/chat/completions \
  -H "x-api-key: ***" -H 'Content-Type: application/json' \
  -d '{"model":"deepseek-v4.1-flash","messages":[{"role":"user","content":"Reply with only: OK"}]}' | jq
```

`Authorization: Bearer ***`는 `x-api-key`와 같습니다.

### 장치 바인딩 (키 하나, 장치 하나)

클라이언트 키는 최초 사용 시 신뢰(TOFU)로 장치에 묶입니다. 새 키는 풀려 있고, **처음 인증된 요청**이 그 요청의 출발 IP에 키를 묶습니다. 이후 그 키의 요청은 같은 IP에서 와야 하며, 아니면 `403 device_mismatch`로 거절됩니다(오류 본문은 묶인 IP를 밝히지 않습니다). 바인딩은 `state_dir/client-bindings.json`에 남고 재시작해도 유지됩니다.

- 리버스 프록시 뒤에서는 실제 클라이언트 IP가 신뢰된 루프백 프록시가 쓴 `X-Forwarded-For`의 맨 오른쪽입니다. 클라이언트가 넣은 항목은 무시됩니다. 로컬(루프백) 호출자는 `127.0.0.1`에 묶입니다.
- `umsp client list`는 각 키의 바인딩 IP를 보여 줍니다. `umsp client unbind <name>`은 바인딩을 지웁니다(다음 요청이 다시 묶습니다. 장치 IP가 바뀔 때 씁니다). `rotate`와 `remove`는 자동으로 지웁니다.
- 같은 NAT 뒤의 장치는 공인 IP 하나를 공유해 구분할 수 없습니다. 귀속과 장치별 폐기는 장치마다 키를 발급하세요.

### 클라이언트 한도

각 키는 선택적 한도를 가질 수 있습니다. 설정하지 않으면 무제한입니다. `umsp client add`에서 정하고 `umsp client update <name>`으로 바꿉니다.

- `--expires-in 30d` — 그 뒤의 요청은 `401 key_expired`로 거절됩니다(`update`의 `--no-expiry`가 제거합니다).
- `--max-concurrency 1` — 동시에 진행 중인 모델 호출은 최대 N개(1-9)입니다. 넘으면 `429 concurrency_limit`(`--no-concurrency-limit`이 제거합니다).
- `--budget-tokens 5000000` — 평생 token 예산. 각 호출은 먼저 출력 상한(요청이 정하지 않으면 모델 기본값)을 예약하고, 맞지 않으면 `403 budget_exhausted`입니다. 실제 사용량은 뒤에 청구됩니다(`update`의 `--reset-usage` / `--no-budget`).

자세한 내용: [docs/configuration.md](docs/configuration.md#client-limits).

## 설정

최소 `~/.config/umsp/config.toml`:

```toml
[server]
listen = "127.0.0.1:3460"
state_dir = "~/.local/share/umsp"

[[providers]]
id = "deepseek"
vendor = "deepseek"                       # 내장 카탈로그. 엔드포인트와 quirks는 여기서
base_url = "https://api.deepseek.com"
anthropic_base_url = "https://api.deepseek.com/anthropic"
api_key_env = "DEEPSEEK_API_KEY"          # 이름만. 값은 env에
protocols = ["chat", "responses", "messages"]
quirks = "deepseek"

[[models]]
id = "deepseek-v4.1-flash"                # 호출자가 아는 것은 이 논리 id뿐
aliases = ["deepseek-flash"]
context_length = 1048000
max_completion_tokens = 128000
capabilities = ["text", "vision", "tools", "reasoning"]
backends = [
  { provider = "deepseek", upstream = "deepseek-flash" },
]

[[clients]]
name = "hermes"
key_hash = "sha256:..."                   # `umsp client add`가 씀
key_prefix = "umsp_ab12cd34"
allowed_models = ["*"]
allowed_protocols = ["chat", "responses", "messages"]
```

- 업스트림 키는 `config.toml`에 넣지 않습니다. 거기에는 `api_key_env` 이름만 있습니다.
- 우선순위: `--config` > `$UMSP_CONFIG` > `~/.config/umsp/config.toml`. `$UMSP_LISTEN`, `$UMSP_STATE_DIR`, `$UMSP_ENV_FILE`은 개별 경로를 덮습니다.
- `/v1/models`는 논리 모델당 하나이며 provider를 노출하지 않습니다. 게이트웨이는 12시간마다 각 provider의 모델 목록을 읽고 id로 중복을 뺍니다. 컨텍스트, 출력 한도, 표시 이름, 능력은 provider가 보내면 그것을 쓰고, 모든 컨텍스트 단계를 포함합니다. 하나의 `context_length`는 유일한 단계이거나, 목록에 여러 단계가 있으면 가장 큰 단계입니다. 문서화된 기본값은 목록이 비워 둔 필드만 채우고 업스트림 값을 덮지 않습니다. 손으로 쓴 `[[models]]`는 그 뒤에도 빈 것을 채웁니다. `umsp catalog refresh`는 즉시 다시 읽습니다. `allowed_models = ["*"]`(또는 빈 목록)가 그 전체 카탈로그를 보여 줍니다.
- `provider add --vendor ID`는 벤더 id와 키만 받습니다. 추가할 때마다 자기 id를 받습니다.

전체 스키마, provider/model/client 관리, 모든 CLI 예: [docs/configuration.md](docs/configuration.md).

## 자주 쓰는 명령

```bash
umsp provider list|add|remove|test      # 업스트림 인스턴스. add는 내장 벤더 카탈로그를 씀
umsp login|logout grok                  # provider별 Grok 장치 코드 세션
umsp model list|add|remove              # 논리 모델과 별칭
umsp model backend add|remove           # 백엔드 붙이기/떼기 (--first = 더 높은 우선순위)
umsp client list|add|remove|rotate|unbind # 클라이언트 키(SHA-256 저장) + TOFU 장치 바인딩
umsp env list|set|unset                 # 업스트림 키, 가림
umsp usage summary|by-model|prune       # 요청 원장 조회
umsp doctor [--online]                  # 진단. --online은 token을 조금 씀
```

모든 하위 명령은 `--json`을 지원합니다. 하위 명령과 플래그 전체 표는 [docs/configuration.md](docs/configuration.md)에 있습니다.

## HTTP 엔드포인트

| 메서드 | 경로 | 인증 | 용도 |
|---|---|---|---|
| GET | `/healthz` | 아니오 | curl로 보는 상태: 차단기, 실패, 최근 결과, 신선한 `usage` |
| GET | `/metrics` | 아니오 | Prometheus 텍스트, 메모리, 루프백만 |
| GET | `/v1/models` | 예 | 논리 모델 등록(평탄한 메타데이터 + `meta` 봉투) |
| POST | `/v1/chat/completions` | 예 | Chat Completions, 스트리밍과 비스트리밍 |
| POST | `/v1/responses` | 예 | Responses, 스트리밍과 비스트리밍 |
| POST | `/v1/messages` | 예 | Anthropic Messages, 스트리밍과 비스트리밍 |
| POST | `/v1/messages/count_tokens` | 예 | 백엔드가 지원하는 곳에서만 프록시 |
| HEAD | `/api/hello` | 아니오 | Claude Code 연결 예열 |

엔드포인트 동작, 오류 매핑, 지표, 원장 필드, systemd 유닛, 종료 코드, 문제 해결: [docs/operations.md](docs/operations.md).

## Grok 구독

`umsp provider add --vendor grok`는 `{config_dir}/sessions/<id>.json`의 장치 코드 OAuth 세션으로 뒷받침되는 provider를 만듭니다. `umsp provider add --vendor chatgpt`는 Sign in with ChatGPT를 씁니다(`127.0.0.1`의 PKCE, 플랜 사용량은 `https://api.openai.com/v1/responses`, `store: false`이고 스트림이 필수). 둘 다 API 키가 아니고, 두 세션은 refresh token을 공유하지 않습니다. `umsp login <id>`는 provider 하나에 로그인하고, `umsp logout <id>`는 그 파일만 지웁니다.

> **실험적 / 계정 위험.** Grok 지원은 공개 CLI OAuth client와 `cli-chat-proxy` 구독 엔드포인트를 씁니다. 공식 xAI API 통합이 아닙니다. 예고 없이 깨질 수 있고 xAI 서비스 약관을 어길 수 있으며, 계정 정지를 포함합니다. 위험을 감수할 계정에서만 쓰세요.

게이트웨이는 필요한 `x-grok-client-version`을 보냅니다(xAI가 최소 버전을 올리면 `UMSP_GROK_CLI_VERSION`으로 덮습니다). 주간 구독 사용량은 청구 엔드포인트에서 읽고, 짧은 `x-ratelimit-*` 헤더에서는 읽지 않습니다. 자세한 내용: [docs/model-routing.md](docs/model-routing.md).

## Qwen Token Plan

`umsp provider add --vendor qwen-token-plan`은 구독 콘솔에 보이는 Qwen Token Plan 호스트(`token-plan.maas.qianwenaiapi.com`)를 씁니다. 종량제 DashScope도, 국제 `qwencloudapi.com` 호스트도 아닙니다. `sk-` 키나 DashScope 호스트는 이 플랜을 깎지 않습니다. Chat은 `https://token-plan.maas.qianwenaiapi.com/compatible-mode/v1`, Responses는 `https://token-plan.maas.qianwenaiapi.com/compatible-mode/v1/responses`, Messages는 `https://token-plan.maas.qianwenaiapi.com/apps/anthropic`입니다. 키 변수는 `QWEN_TOKEN_PLAN_API_KEY`입니다. 이 플랜은 대화형 클라이언트용으로 문서화되어 있습니다. 옛 지역 호스트 `token-plan.cn-beijing.maas.aliyuncs.com`은 아직 응답하지만 콘솔 URL은 아닙니다.

## OpenCode Go

`umsp provider add --vendor opencode-go`는 OpenCode Go 구독 호스트 `https://opencode.ai/inference/go`를 씁니다. 종량제 호스트도, 무료 모델도 아닙니다(무료 모델은 OpenCode 밖에서 403을 돌려줍니다). 키는 Console API key이고 `OPENCODE_GO_API_KEY`로 저장됩니다. Chat과 Responses는 OpenAI 베이스를 쓰고, Messages는 `https://opencode.ai/inference/go/anthropic`을 씁니다. 각 요청은 `User-Agent: umsp/<version>`과 `x-opencode-session`을 보냅니다. 클라이언트가 그 이름의 헤더를 주면 유지하고, 아니면 게이트웨이가 모델과 첫 메시지를 해시해 한 대화가 한 세션에 남게 합니다. Go는 잔액이나 남은 할당량 스냅샷을 내지 않으므로 이 provider는 측정되지 않음으로 정렬됩니다.

## Command Code

`umsp provider add --vendor command-code`는 Provider API `https://api.commandcode.ai/provider/v1`을 씁니다. 월 $1 Go 플랜에는 이 API가 없습니다. GOAT, Pro, Max, Team, Provider 플랜에는 있습니다. 키는 Studio에서 오고 `COMMAND_CODE_API_KEY`로 저장됩니다. Chat, Responses, Messages는 그 베이스를 공유합니다. 모델 목록의 각 행은 `supported_endpoints`를 가집니다. Claude 모델은 Messages만 되므로 Chat 클라이언트는 Chat Completions로 가지 않고 `/messages`로 번역됩니다.

## 문서

| 문서 | 내용 |
|---|---|
| [docs/configuration.md](docs/configuration.md) | 설정 스키마, CLI 참고, provider/model/client 절차 |
| [docs/operations.md](docs/operations.md) | 엔드포인트, 지표, 원장, 종료 코드, systemd, 문제 해결, 측정 |
| [docs/protocols.md](docs/protocols.md) | 업스트림별 quirks, 장애 조치, 투명성, SSE, Responses, Messages |
| [docs/model-routing.md](docs/model-routing.md) | 논리 모델, 백엔드, 용량 등급, 고정, Grok 할당량 |
| [docs/protocol-matrix.md](docs/protocol-matrix.md) | 프로토콜 간 번역 행렬과 손실 필드 |
| [docs/quirks.md](docs/quirks.md) | 업스트림 quirk 목록 |
| [CONTRIBUTING.md](CONTRIBUTING.md) / [SECURITY.md](SECURITY.md) | 개발 규칙 / 취약점 보고 |

## 개발

```bash
make check          # fmt + clippy -D warnings + 테스트 + 영어 전용 언어 검사
./scripts/build-musl.sh
```

저장소 본문은 영어만 허용합니다(`AGENTS.md`). 영어판이 유일한 기준이고, 이 페이지는 그 한국어 번역입니다. `docs/internal/`의 이정표 스냅샷은 기록이며 현재 동작이 아닙니다.

## License

[MIT](LICENSE) © 2026 YiLongMusk
