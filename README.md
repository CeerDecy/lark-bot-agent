# lark-bot-agent

飞书远程 Coding Agent 代理机器人：在飞书里和机器人对话，机器人通过
[Agent Client Protocol (ACP)](https://agentclientprotocol.com) 驱动本机的
Codex（`codex-acp`）或 Claude Code（`claude-agent-acp`），再把结果回到飞书。

> **⚠️ 安全提示（请务必阅读）**
>
> 白名单里的用户实质上拿到了这台开发机的**远程执行能力**：agent 可以读写文件、执行命令。
> - 生产环境务必配置 `ALLOWED_OPEN_IDS`，不配时所有能联系到机器人的人都能用（启动时会打 warn）。
> - `PERMISSION_POLICY=auto_allow` 会跳过所有审批，等价于把远程 shell 直接交给白名单用户，只建议在隔离机器上使用。
> - `/cwd` 和 `ALLOWED_CWD_ROOTS` 只约束 agent 的「主」工作目录，**不能**阻止 agent 通过 shell 访问其他路径。真正的隔离要靠 agent 自身的沙箱（如 codex 的 `INITIAL_AGENT_MODE=workspace-write`）。

## 架构

```
飞书客户端 ──(长连接 ws 事件)──> Bot ──(stdio JSON-RPC / ACP)──> codex-acp / claude-agent-acp
     ^                            │
     └──────(im.message API)──────┘
```

- `src/lark/`：飞书事件接收、消息发送 / 更新、文本提取。
- `src/session/`：`chat_id → ACP sessionId` 映射、同会话串行队列、`/new` `/cancel` `/status` `/cwd`、审批卡片。
- `src/acp/`：agent 子进程 supervisor（指数退避重启）、`initialize` / `session/new` / `session/prompt` / `session/cancel`、`session/update` 路由、`session/request_permission` 中介。
- `src/agent.rs`：`AGENT_KIND` → 默认命令行。
- `src/render/`：ACP update → Markdown 卡片、工具调用摘要、diff、任务清单、超长切分。

## 运行

```sh
export APP_ID=cli_xxx APP_SECRET=xxx
export AGENT_KIND=codex            # 或 claude
export AGENT_CWD=/abs/path/to/repo
export ALLOWED_OPEN_IDS=ou_xxx,ou_yyy
export ADMIN_OPEN_ID=ou_xxx
# agent 凭证按需透传（Bot 不读取、不记录）：
export OPENAI_API_KEY=...          # codex；或先在本机完成 ChatGPT 登录
export ANTHROPIC_API_KEY=...       # claude；或复用 Claude Code 本机登录态
cargo run --release
```

首次运行 `npx -y @agentclientprotocol/codex-acp` 需要联网下载，可提前执行一次。
无头机器用 codex 时建议 `NO_BROWSER=1`，并用 API key 或提前登录。

### Docker

镜像里已经 `npm install -g` 了 `codex-acp` 和 `claude-agent-acp`（含各自的 Codex / Claude Code 平台二进制）。
Bot 启动时若在 `PATH` 里找到对应的适配器二进制就直接用，不再走 `npx`、不需要联网下载；本机
`npm install -g` 过的同样受益。显式设置 `AGENT_COMMAND` 时以用户值为准。

```sh
make image REGISTRY=registry.example.com/team   # linux/amd64 构建并 --push，tag = <git short sha>-<YYYYMMDDHHmm>
make image REGISTRY=... LATEST=1                # 同时推 latest
make image LOAD=1                               # 不推送，--load 到本地 docker（只能单平台）
make tag REGISTRY=...                           # 只打印完整镜像名
docker run --rm \
  -e APP_ID=cli_xxx -e APP_SECRET=xxx \
  -e AGENT_KIND=claude -e ANTHROPIC_API_KEY=... \
  -e ALLOWED_OPEN_IDS=ou_xxx \
  -v /path/to/repo:/workspace \
  lark-bot-agent
```

- `AGENT_CWD` 默认 `/workspace`，把要操作的仓库挂到这里。
- 容器以 root 运行。`claude-agent-acp` 在 root 下默认拒绝 `bypassPermissions` 模式，镜像里设了 `IS_SANDBOX=1`
  放开这个限制；这意味着 agent 对容器内文件系统有完全控制，请只挂载需要它操作的目录。
- 不用 API key、想复用登录态时，把 `/root/.claude` 或 `/root/.codex` 挂成卷，在宿主机登录后把
  `~/.claude/.credentials.json`（或 `~/.codex/auth.json`）拷进卷里。
- 适配器版本通过 build-arg 锁定，Makefile 里对应 `CODEX_ACP_VERSION` / `CLAUDE_AGENT_ACP_VERSION` 变量：
  `make image CLAUDE_AGENT_ACP_VERSION=x.y.z`。
- `PLATFORM` 默认 `linux/amd64`；在 Apple Silicon 上会走 QEMU 模拟编译 Rust，很慢，想本地快速验证可用 `make image PLATFORM=linux/arm64`。
- 拉 Docker Hub 基础镜像不稳定时，用 `BASE_REGISTRY` 指向镜像加速站，值要带到 `library` 这一级，例如
  `make image REGISTRY=... BASE_REGISTRY=docker.m.daocloud.io/library`；或者在 Docker Desktop 的
  Settings → Docker Engine 里配置 `registry-mirrors`，这样不用改命令。
- `BUILDER` 可指定 buildx builder（`docker buildx ls` 查看）。`docker-container` 类型的 builder 有独立缓存，
  不复用本机 `docker pull` 过的镜像，也不走 Docker Desktop 配置的代理；换成 `BUILDER=default` 可以复用
  daemon 缓存和代理。
- `docker-compose.example.yml` 是一份可直接复制的 compose 示例。

飞书应用侧需要：机器人能力、`im:message` 相关权限、事件订阅 `im.message.receive_v1`（长连接模式）、
以及**卡片回调**（`card.action.trigger`，审批按钮依赖它，同样走长连接）。

## 配置项

| 环境变量 | 必填 | 默认 | 说明 |
| --- | --- | --- | --- |
| `APP_ID` / `APP_SECRET` | 是 | - | 飞书应用凭证 |
| `RUST_LOG` | 否 | `info` | 日志级别；`RUST_LOG=debug,agent=debug` 可看到 agent stderr |
| `AGENT_KIND` | 是 | - | `codex` 或 `claude` |
| `AGENT_COMMAND` | 否 | 按 kind 推导 | 覆盖可执行文件（如 `codex-acp`、本地构建路径） |
| `AGENT_ARGS` | 否 | 按 kind 推导 | 覆盖参数，空格分隔 |
| `AGENT_CWD` | 是 | - | 默认工作目录（绝对路径） |
| `ALLOWED_CWD_ROOTS` | 否 | `AGENT_CWD` | `/cwd` 可切换的根目录集合，冒号分隔 |
| `ALLOWED_OPEN_IDS` | 否 | 空（允许所有人） | 用户白名单，逗号分隔 |
| `ADMIN_OPEN_ID` | 否 | - | 接收认证 / 重启通知，可代为审批 |
| `PERMISSION_POLICY` | 否 | `ask` | `ask` / `auto_allow` / `auto_reject` |
| `PERMISSION_TIMEOUT_SECS` | 否 | `600` | 审批超时后按 reject_once 处理 |
| `PROMPT_TIMEOUT_SECS` | 否 | `1800` | 单轮超时后发送 `session/cancel` |
| `SESSION_IDLE_SECS` | 否 | `7200` | 会话空闲回收 |
| `QUEUE_LIMIT_PER_CHAT` | 否 | `10` | 每会话排队上限 |
| `STREAM_UPDATE_INTERVAL_MS` | 否 | `2000` | 流式更新最小间隔 |
| `STREAM_UPDATE_CHARS` | 否 | `500` | 累积多少字符触发一次更新 |
| `MAX_MESSAGE_CHARS` | 否 | `4000` | 单条消息字符上限，超过按段落切分 |
| `AGENT_MODE` | 否 | - | 建会话后通过 `session/set_mode` 切到的 agent 侧模式。claude：`default` / `acceptEdits` / `plan` / `auto` / `bypassPermissions`；codex 用 `INITIAL_AGENT_MODE` 代替 |
| `SHOW_THOUGHTS` | 否 | `false` | 是否在结尾展示思考内容 |
| `AGENT_MCP_SERVERS_JSON` | 否 | `[]` | 透传给 `session/new` 的 `mcpServers`（ACP 格式的 JSON 数组） |
| `ACP_TRACE_DIR` | 否 | - | 把 ACP 原始消息按行落盘（jsonl）用于排障 |
| `OPENAI_API_KEY` / `CODEX_API_KEY` / `NO_BROWSER` / `INITIAL_AGENT_MODE` | 否 | - | 透传给 codex-acp |
| `ANTHROPIC_API_KEY` / `CLAUDE_MODEL_CONFIG` | 否 | - | 透传给 claude-agent-acp |

### 权限的两层控制

1. **agent 侧模式**：决定 agent 哪些操作会来问。
   - claude：`AGENT_MODE=bypassPermissions` 让 Claude Code 跳过所有审批（等价于 `claude --dangerously-skip-permissions`）；
     `acceptEdits` 只自动放行文件编辑，命令仍会问。以 root 身份在非沙箱环境运行时适配器会拒绝 bypass。
   - codex：用适配器自己的 `INITIAL_AGENT_MODE`（`read-only` / `workspace-write` / `agent` / `agent-full-access`）。
2. **Bot 侧** `PERMISSION_POLICY`：决定真正到达 Bot 的 `session/request_permission` 怎么回。
   `auto_allow` 自动选 allow_always / allow_once；`ask` 发审批卡片。

agent 侧先生效：agent 认为不需要问的操作根本不会到 Bot；到了 Bot 的再按 `PERMISSION_POLICY` 处理。
想完全免审批，二者任选其一即可（`AGENT_MODE=bypassPermissions` 或 `PERMISSION_POLICY=auto_allow`）；
想最小权限就两者都收紧。`/status` 会显示当前会话实际生效的 agent 模式。

## 指令

| 指令 | 作用 |
| --- | --- |
| `/new` | 丢弃当前会话映射，下一条消息新建会话 |
| `/cancel` | 对进行中的轮次发 `session/cancel`，并清空排队 |
| `/status` | agent 名称 / 版本、sessionId、cwd、是否进行中、排队数、待审批数、上下文用量 |
| `/cwd <绝对路径>` | 切换下一个新会话的工作目录（必须在 `ALLOWED_CWD_ROOTS` 内） |
| `/help` | 帮助 |

群聊里只响应 @机器人 的消息；支持文本、富文本（提取纯文本）、图片（agent 声明 `promptCapabilities.image` 时）。

## 测试

```sh
cargo test
```

- 单测：配置解析、命令行推导、指令解析、文本提取、渲染、切分、审批策略。
- 集成测试 `tests/acp_integration.rs`：用 `tests/support/fake_agent.rs`（一个按脚本回放的假 ACP agent）
  覆盖 initialize、session/new、prompt 流、权限审批（auto / ask / 权限校验）、取消、超时、崩溃重启。

## 与需求文档的差异 / 待实测

- `agent-client-protocol` crate 现为 3.2.0，API 是 builder 风格（`Client.builder().on_receive_*().connect_with(AcpAgent)`），
  不是文档假定的 `Client` trait；实现仍按 FR-ACP-9 用官方 crate，未手写 JSON-RPC。
- FR-SESSION-9（重启后 `session/load` 恢复）未实现：需要持久化 sessionId，且两个适配器的 `loadSession` 支持未确认。
- 飞书消息更新的限流阈值与卡片 Markdown 长度上限需要实测；默认 2s / 500 字 / 4000 字可调。
- 审批卡片依赖应用开通「卡片回调」；SDK 已在长连接上支持 `card.action.trigger`，未在真实应用上验证。
