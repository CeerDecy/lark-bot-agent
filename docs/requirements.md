# 飞书远程 Coding Agent 代理机器人 需求文档

- 版本：v0.1（草稿）
- 日期：2026-10-09
- 状态：待评审

## 1. 背景与目标

当前仓库是一个飞书机器人自动回复 demo（`src/main.rs`）：通过飞书长连接接收 `im.message.receive_v1` 事件，把文本原样回给用户。

本需求在此基础上，把机器人升级为「远程 Coding Agent 代理」：用户在飞书里和机器人对话，机器人把消息转发给本机运行的 Codex 或 Claude Code，等待 agent 处理后，把结果再通过飞书回给用户。机器人与 agent 之间使用 Agent Client Protocol（ACP，https://agentclientprotocol.com）通信，具体使用哪个 agent 由环境变量决定。

目标：

1. 用手机或任意飞书客户端，就能远程驱动一台开发机上的 Codex / Claude Code 干活。
2. 对 agent 零侵入：只依赖 ACP 官方适配器，不改 Codex / Claude Code 本体。
3. 切换 agent 只需改环境变量重启，代码不感知具体是哪个 agent。

非目标（本期不做）：

- 多台开发机 / 多仓库调度。
- Web 控制台。
- 自建飞书事件回调 HTTP 服务（继续用长连接）。
- 支持除 Codex / Claude Code 以外的 agent（但设计上不应阻止后续接入，ACP 本身是通用的）。

## 2. 术语

| 术语 | 含义 |
| --- | --- |
| Bot / 机器人 | 本项目进程，既是飞书应用，又是 ACP Client |
| Agent | 通过 ACP 对接的 coding agent 进程，即 codex-acp 或 claude-agent-acp |
| ACP | Agent Client Protocol，基于 JSON-RPC 2.0 的编辑器 / agent 通信协议 |
| 会话（Session） | ACP 中的 `sessionId`，对应一段连续的 agent 上下文 |
| 轮次（Turn） | 一次 `session/prompt` 到收到 `stopReason` 的完整过程 |
| 工作目录（cwd） | agent 操作的项目根目录，`session/new` 时指定 |

## 3. 现状

- Rust 2024 edition，依赖 `larksuite-oapi-sdk-rs`（ws + axum feature）、tokio、serde_json、tracing。
- 事件回调里只做入队，worker 异步处理并调用 `im.message.reply` 回复。
- 配置只有 `APP_ID`、`APP_SECRET`、`RUST_LOG` 三个环境变量。
- 无会话、无状态、无 agent 相关逻辑。

## 4. 整体架构

```
飞书客户端 ──(长连接 ws 事件)──> Bot 进程 ──(stdio, JSON-RPC)──> codex-acp / claude-agent-acp
     ^                               │                                   │
     └──────(im.message API)─────────┘                                   └──> Codex / Claude Agent SDK
```

Bot 内部分层：

1. 飞书接入层：接收消息事件、回复 / 更新消息、把 ACP 内容渲染成飞书消息。
2. 会话管理层：维护「飞书会话 → ACP sessionId」映射、串行化同一会话的请求、处理 `/new`、`/cancel` 等指令。
3. ACP 客户端层：拉起 agent 子进程、`initialize`、`session/new`、`session/prompt`、消费 `session/update`、响应 `session/request_permission`。
4. Agent 适配层：根据环境变量决定子进程命令行、默认参数和环境变量。

## 5. 功能需求

编号规则：FR-模块-序号。优先级 P0 必须、P1 应该、P2 可以。

### 5.1 飞书消息接入（IM）

| 编号 | 优先级 | 描述 |
| --- | --- | --- |
| FR-IM-1 | P0 | 继续使用飞书长连接接收 `im.message.receive_v1`，不依赖公网回调。 |
| FR-IM-2 | P0 | 支持单聊文本消息。 |
| FR-IM-3 | P1 | 支持群聊中 @机器人 的文本消息，去掉 @ 片段后再转发。未 @ 的群消息忽略。 |
| FR-IM-4 | P1 | 支持富文本（post）消息，提取纯文本后转发。 |
| FR-IM-5 | P2 | 支持图片消息：下载后作为 ACP `image` content block 发送（需 agent 的 `promptCapabilities.image` 为 true，否则回复「当前 agent 不支持图片」）。 |
| FR-IM-6 | P0 | 收到消息后立刻 ack 事件（沿用现有入队方式），避免飞书重推。 |
| FR-IM-7 | P0 | 同一 `message_id` 重复投递时幂等，不重复提交 prompt。 |
| FR-IM-8 | P1 | 白名单：仅允许 `ALLOWED_OPEN_IDS` 中的用户使用；未配置时允许所有人（启动时打 warn 日志）。 |

### 5.2 会话管理（SESSION）

| 编号 | 优先级 | 描述 |
| --- | --- | --- |
| FR-SESSION-1 | P0 | 以飞书 `chat_id` 为 key 维护 ACP 会话。首次收到消息时调用 `session/new` 创建会话，后续消息复用同一 `sessionId`。 |
| FR-SESSION-2 | P0 | 同一会话内串行：上一轮未结束时收到新消息，排队等待（队列上限可配，默认 10），并回复「已排队，前面还有 N 条」。 |
| FR-SESSION-3 | P0 | 不同会话之间并发，互不阻塞。 |
| FR-SESSION-4 | P0 | 指令 `/new`：丢弃当前映射，下一条消息新建会话。 |
| FR-SESSION-5 | P0 | 指令 `/cancel`：对正在进行的轮次发送 `session/cancel`，并清空排队消息。 |
| FR-SESSION-6 | P1 | 指令 `/status`：回复当前 agent 名称与版本（来自 `initialize` 响应的 `agentInfo`）、sessionId、cwd、是否有进行中的轮次、排队数。 |
| FR-SESSION-7 | P1 | 指令 `/cwd <绝对路径>`：切换下一个新会话的工作目录。路径必须在 `ALLOWED_CWD_ROOTS` 之内，否则拒绝。 |
| FR-SESSION-8 | P2 | 会话空闲超时（默认 2 小时）后自动清理映射；若 agent 支持 `session/close` 则调用。 |
| FR-SESSION-9 | P2 | Bot 重启后，若 agent `loadSession` 能力为 true，则尝试用持久化的 sessionId 调 `session/load` 恢复；否则新建。 |

### 5.3 Agent 选择与进程管理（AGENT）

| 编号 | 优先级 | 描述 |
| --- | --- | --- |
| FR-AGENT-1 | P0 | 通过环境变量 `AGENT_KIND` 选择 agent，取值 `codex` 或 `claude`。缺失或非法值时启动失败并给出明确错误。 |
| FR-AGENT-2 | P0 | `codex` 对应命令 `npx -y @agentclientprotocol/codex-acp`（或全局安装后的 `codex-acp`）；`claude` 对应 `npx -y @agentclientprotocol/claude-agent-acp`（或 `claude-agent-acp`）。 |
| FR-AGENT-3 | P0 | 允许用 `AGENT_COMMAND` / `AGENT_ARGS` 完全覆盖默认命令行，便于使用本地构建版本或其他 ACP agent。 |
| FR-AGENT-4 | P0 | Bot 作为父进程拉起 agent，stdin/stdout 用于 ACP，stderr 转到 Bot 日志（带 `agent` 前缀）。 |
| FR-AGENT-5 | P0 | agent 进程所需的凭证（如 `OPENAI_API_KEY` / `CODEX_API_KEY`、`ANTHROPIC_API_KEY`）由 Bot 的环境透传，Bot 不解析、不记录其值。 |
| FR-AGENT-6 | P0 | 启动后立即完成 `initialize`（`protocolVersion: 1`），校验返回版本一致；记录 `agentCapabilities` 用于后续能力判断。 |
| FR-AGENT-7 | P1 | 若 `initialize` 返回非空 `authMethods` 且 agent 要求认证，Bot 向配置的管理员（`ADMIN_OPEN_ID`）发送提示，并在日志中说明需要先在开发机上完成登录（Codex 需设置 `NO_BROWSER=1` 时的处理见风险章节）。 |
| FR-AGENT-8 | P1 | agent 进程崩溃或 stdout 关闭时：标记所有会话失效，向有进行中轮次的会话回复失败消息，按指数退避（1s、2s、4s…，最多 60s）自动重启。 |
| FR-AGENT-9 | P2 | 一个 Bot 进程只维护一个 agent 子进程，所有会话共享（多会话由 ACP sessionId 区分）。 |

### 5.4 ACP 客户端（ACP）

| 编号 | 优先级 | 描述 |
| --- | --- | --- |
| FR-ACP-1 | P0 | 传输：stdio，每条 JSON-RPC 消息一行、UTF-8、无内嵌换行。 |
| FR-ACP-2 | P0 | 实现 Client → Agent：`initialize`、`session/new`、`session/prompt`、`session/cancel`（通知）。 |
| FR-ACP-3 | P0 | 实现 Agent → Client：`session/request_permission`、`session/update`（通知）。 |
| FR-ACP-4 | P0 | `initialize` 时 `clientCapabilities.fs` 与 `terminal` 均声明为 false，让 agent 使用自身能力读写文件和执行命令（Bot 不实现 `fs/*`、`terminal/*`）。 |
| FR-ACP-5 | P0 | `session/new` 的 `cwd` 来自配置 `AGENT_CWD`（或 `/cwd` 指令），`mcpServers` 默认为空数组。 |
| FR-ACP-6 | P0 | `session/prompt` 的 `prompt` 至少包含一个 `text` block；每个在途请求有超时（默认 30 分钟），超时后发送 `session/cancel` 并回复用户。 |
| FR-ACP-7 | P0 | 正确处理 `stopReason`：`end_turn` 正常结束；`cancelled` 回复「已取消」；`max_tokens` / `max_turn_requests` / `refusal` 向用户说明原因。 |
| FR-ACP-8 | P1 | 收到 `session/cancel` 后，Bot 对所有尚未回应的 `session/request_permission` 以 `cancelled` outcome 响应。 |
| FR-ACP-9 | P1 | 优先使用官方 Rust crate `agent-client-protocol` 实现 `Client` trait；若 API 不满足，再手写 JSON-RPC 层。 |
| FR-ACP-10 | P2 | 透传 `mcpServers` 配置（`AGENT_MCP_SERVERS_JSON`），让 agent 连接额外 MCP 服务。 |

### 5.5 响应回传与渲染（RENDER）

| 编号 | 优先级 | 描述 |
| --- | --- | --- |
| FR-RENDER-1 | P0 | 累积 `agent_message_chunk`，轮次结束后把完整回复发给用户。 |
| FR-RENDER-2 | P0 | 回复以「回复原消息」方式发送（`im.message.reply`），群聊中 `reply_in_thread` 为 true。 |
| FR-RENDER-3 | P1 | 流式体验：轮次开始时先发一条「处理中…」消息，之后每 2 秒或每累积 500 字符更新一次该消息（`im.message.patch` / update），结束时写入最终内容。更新频率受飞书限流约束，需可配置。 |
| FR-RENDER-4 | P1 | 工具调用摘要：把 `tool_call` / `tool_call_update` 渲染成一行状态，例如 `[edit] 修改 src/main.rs ✅`。`kind` 用于选择图标，`status` 用于选择 ✅ / ❌ / ⏳。 |
| FR-RENDER-5 | P1 | `agent_thought_chunk` 默认不展示；`SHOW_THOUGHTS=true` 时折叠展示在结尾。 |
| FR-RENDER-6 | P1 | `plan` 更新渲染为任务清单（带状态）。 |
| FR-RENDER-7 | P1 | 消息长度超过飞书单条限制时按段落切分为多条，顺序发送。 |
| FR-RENDER-8 | P1 | 回复优先使用飞书 Markdown 卡片（interactive / post），保留代码块；若渲染失败回退为纯文本。 |
| FR-RENDER-9 | P2 | `tool_call_update.content` 中的 `diff` 渲染为代码块；`terminal` 类型只显示「执行命令」摘要。 |
| FR-RENDER-10 | P2 | `usage_update` 在 `/status` 中展示 token 用量。 |

### 5.6 权限审批（PERM）

| 编号 | 优先级 | 描述 |
| --- | --- | --- |
| FR-PERM-1 | P0 | 收到 `session/request_permission` 时，按 `PERMISSION_POLICY` 处理：`auto_allow` 自动选择 `allow_once`（有 `allow_always` 时优先）；`auto_reject` 自动拒绝；`ask` 转给用户。 |
| FR-PERM-2 | P1 | `ask` 模式：向用户发送交互卡片，列出 `title`、`kind`、`rawInput` 摘要，以及 agent 给出的 options 按钮；用户点击后把 `optionId` 回传给 agent。 |
| FR-PERM-3 | P1 | 审批等待超时（默认 10 分钟）后按 `reject_once` 响应，并通知用户。 |
| FR-PERM-4 | P1 | 只允许发起该轮次的用户（或 `ADMIN_OPEN_ID`）点击审批按钮，其他人点击提示无权限。 |
| FR-PERM-5 | P2 | 支持在 agent 侧用模式控制权限（如 codex-acp 的 `INITIAL_AGENT_MODE`），文档中说明两种方式的优先关系。 |

### 5.7 错误处理与可观测性（OPS）

| 编号 | 优先级 | 描述 |
| --- | --- | --- |
| FR-OPS-1 | P0 | 所有面向用户的失败都有一条可读的中文回复，不暴露堆栈。 |
| FR-OPS-2 | P0 | 日志（tracing）至少包含：chat_id、message_id、sessionId、轮次耗时、stopReason、工具调用数。 |
| FR-OPS-3 | P0 | 日志中不输出用户消息全文（debug 级别除外），不输出任何 API key。 |
| FR-OPS-4 | P1 | agent stderr 以 debug 级别逐行记录。 |
| FR-OPS-5 | P2 | 可选保存每轮 ACP 原始消息到本地文件（`ACP_TRACE_DIR`），用于排障。 |

## 6. 非功能需求

| 编号 | 描述 |
| --- | --- |
| NFR-1 | 收到飞书事件到 ack 的时间 < 1s（现有入队机制保证）。 |
| NFR-2 | 单个 Bot 进程支持至少 10 个并发会话。 |
| NFR-3 | agent 子进程异常不导致 Bot 进程退出。 |
| NFR-4 | 所有网络与子进程 IO 均为异步（tokio），不阻塞运行时。 |
| NFR-5 | 安全：默认开启用户白名单提示；cwd 受 `ALLOWED_CWD_ROOTS` 限制；agent 可执行命令的事实要在 README 中显著说明，因为这等价于给白名单用户一个远程 shell。 |
| NFR-6 | 可测试性：ACP 客户端层对 trait 编程，可用假 agent（读写 stdio 的测试二进制）做集成测试。 |

## 7. 配置项

| 环境变量 | 必填 | 默认 | 说明 |
| --- | --- | --- | --- |
| `APP_ID` / `APP_SECRET` | 是 | - | 飞书应用凭证（已有） |
| `RUST_LOG` | 否 | `info` | 日志级别（已有） |
| `AGENT_KIND` | 是 | - | `codex` 或 `claude` |
| `AGENT_COMMAND` | 否 | 按 `AGENT_KIND` 推导 | 覆盖 agent 可执行文件 |
| `AGENT_ARGS` | 否 | 按 `AGENT_KIND` 推导 | 覆盖 agent 参数，空格分隔 |
| `AGENT_CWD` | 是 | - | 默认工作目录，绝对路径 |
| `ALLOWED_CWD_ROOTS` | 否 | `AGENT_CWD` | `/cwd` 可切换的根目录集合，冒号分隔 |
| `ALLOWED_OPEN_IDS` | 否 | 空（允许所有人） | 用户白名单，逗号分隔 |
| `ADMIN_OPEN_ID` | 否 | - | 接收认证提醒、可代为审批的管理员 |
| `PERMISSION_POLICY` | 否 | `ask` | `ask` / `auto_allow` / `auto_reject` |
| `PERMISSION_TIMEOUT_SECS` | 否 | `600` | 审批超时 |
| `PROMPT_TIMEOUT_SECS` | 否 | `1800` | 单轮超时 |
| `SESSION_IDLE_SECS` | 否 | `7200` | 会话空闲回收 |
| `QUEUE_LIMIT_PER_CHAT` | 否 | `10` | 每会话排队上限 |
| `STREAM_UPDATE_INTERVAL_MS` | 否 | `2000` | 流式更新间隔 |
| `SHOW_THOUGHTS` | 否 | `false` | 是否展示思考内容 |
| `AGENT_MCP_SERVERS_JSON` | 否 | `[]` | 透传给 `session/new` 的 mcpServers |
| `ACP_TRACE_DIR` | 否 | - | ACP 原始消息落盘目录 |
| `OPENAI_API_KEY` / `CODEX_API_KEY` | codex 时视情况 | - | 透传给 codex-acp；也可提前在开发机完成 ChatGPT 登录 |
| `NO_BROWSER` | 否 | - | 透传给 codex-acp，无头机器建议设为 `1` |
| `INITIAL_AGENT_MODE` | 否 | - | 透传给 codex-acp：`read-only` / `workspace-write` / `agent` / `agent-full-access` |
| `ANTHROPIC_API_KEY` | claude 时视情况 | - | 透传给 claude-agent-acp（Claude Agent SDK）；具体接受的认证方式以适配器 README 为准，见 10.3 |

## 8. 关键交互流程

### 8.1 正常对话

1. 用户在飞书发送「帮我修复 tests 里失败的用例」。
2. Bot 收到事件、ack、入队；校验白名单与去重。
3. 查 chat_id 映射，无会话则 `session/new {cwd, mcpServers: []}` 得到 sessionId。
4. 发送「处理中…」占位消息。
5. `session/prompt {sessionId, prompt: [{type: text, text}]}`。
6. 持续消费 `session/update`：message chunk 追加到缓冲，tool_call 追加到状态列表，按间隔更新占位消息。
7. 收到 `session/request_permission`：按策略自动处理或发审批卡片，等待用户点击，回传 `optionId`。
8. `session/prompt` 返回 `stopReason`，用最终内容更新占位消息；如超长则追加多条。

### 8.2 取消

1. 用户发送 `/cancel`。
2. Bot 发送 `session/cancel {sessionId}`，并对挂起的权限请求回 `cancelled`。
3. agent 返回 `stopReason: cancelled`，Bot 更新占位消息为「已取消」，清空排队。

### 8.3 agent 崩溃

1. stdout EOF 或进程退出。
2. 所有在途轮次回复「agent 异常退出，请稍后重试」，会话映射清空。
3. 指数退避重启，重新 `initialize`；成功后向管理员发一条恢复通知。

## 9. 技术方案要点

建议模块划分（`src/`）：

- `main.rs`：装配、启动飞书 ws 与 agent 子进程。
- `config.rs`：环境变量解析与校验。
- `lark/`：事件接收、消息发送 / 更新、卡片构造、文本提取（现有 `extract_text` 迁入）。
- `session/`：chat_id → 会话状态（sessionId、队列、当前轮次句柄、占位消息 id）。
- `acp/`：子进程管理、JSON-RPC 编解码、`Client` trait 实现、update 事件流（`tokio::sync::broadcast` 或 mpsc 分发给对应会话）。
- `agent/`：`AgentKind` 枚举与默认命令行 / 环境变量推导。
- `render/`：ACP update → 飞书消息文本 / 卡片。

依赖新增（固定版本）：`agent-client-protocol`、`serde`（derive）、`tokio-util`（可选，用于行编解码）、`anyhow` 或 `thiserror`。

测试：

- 单测：配置解析、文本提取、渲染函数、指令解析。
- 集成测试：一个假 ACP agent 二进制，按脚本回放 `session/update` 与权限请求，验证 Bot 的消息序列。
- 手工验收：分别用 `AGENT_KIND=codex` 与 `AGENT_KIND=claude` 跑通 8.1、8.2 流程。

## 10. 风险与待确认

### 10.1 飞书侧

- 消息更新接口的频率限制与可更新时效需要实测，决定 FR-RENDER-3 的默认间隔，必要时退化为「只在结束时发一条」。
- 交互卡片按钮回调需要应用开启卡片回调能力，确认长连接模式是否能收到 `card.action.trigger`。

### 10.2 ACP 侧

- Bot 不实现 `fs/*` 和 `terminal/*`，依赖 agent 自身的文件与命令能力；需确认两个适配器在 client 不提供这些能力时都能正常工作（codex-acp README 提到后台终端任务是能力协商可选项，预计可行）。
- `loadSession` 在两个适配器上的支持情况未确认，FR-SESSION-9 排在 P2。
- ACP v2 处于草案阶段，本期锁定 v1。

### 10.3 认证

- codex-acp 支持 `CODEX_API_KEY` / `OPENAI_API_KEY` 或 ChatGPT 浏览器登录；无头机器需 `NO_BROWSER=1`，此时若没有 API key 需要人工先在开发机登录一次。
- claude-agent-acp 包装 Claude Agent SDK，README 未明确列出环境变量；需在实现前阅读其 docs 确认是走 `ANTHROPIC_API_KEY` 还是复用 Claude Code 本机登录态。

### 10.4 安全

- 白名单用户实质上拿到了开发机的远程执行能力。默认 `PERMISSION_POLICY=ask` 并要求配置白名单，README 中需用醒目方式说明 `auto_allow` 的风险。
- cwd 限制只能约束 agent 的「主」工作目录，不能阻止 agent 通过 shell 访问其他路径；真正的隔离要依赖 agent 自身的沙箱模式（如 codex 的 `INITIAL_AGENT_MODE`）。

## 11. 里程碑

| 阶段 | 内容 | 对应需求 |
| --- | --- | --- |
| M1 | 跑通单聊 → ACP → 单条回复，`AGENT_KIND` 切换 | FR-IM-1/2/6/7、FR-SESSION-1/2/3、FR-AGENT-1~6、FR-ACP-1~7、FR-RENDER-1/2、FR-PERM-1（仅 auto 模式）、FR-OPS-1~3 |
| M2 | 流式更新、工具调用展示、`/new` `/cancel` `/status`、agent 自动重启 | FR-RENDER-3~8、FR-SESSION-4~6、FR-AGENT-7/8、FR-ACP-8/9 |
| M3 | 审批卡片、群聊、白名单、`/cwd` | FR-PERM-2~4、FR-IM-3/4/8、FR-SESSION-7 |
| M4 | 图片、会话恢复、MCP 透传、trace 落盘 | 其余 P2 |

## 12. 参考

- ACP 文档索引：https://agentclientprotocol.com/llms.txt
- ACP v1 概览 / 初始化 / 会话 / 轮次 / 工具调用 / 取消 / 传输：https://agentclientprotocol.com/protocol/v1/
- ACP Rust SDK：https://agentclientprotocol.com/libraries/rust.md （crate `agent-client-protocol`）
- Codex 适配器：https://github.com/agentclientprotocol/codex-acp （npm `@agentclientprotocol/codex-acp`）
- Claude 适配器：https://github.com/agentclientprotocol/claude-agent-acp （npm `@agentclientprotocol/claude-agent-acp`，binary `claude-agent-acp`）
- ACP Agent 注册表：https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json
