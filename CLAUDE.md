# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A Feishu (Lark) bot that proxies chat messages to a local coding agent (Codex or Claude Code) over the
Agent Client Protocol (ACP). Rust 2024, tokio. The requirements document is `docs/requirements.md`;
FR-xxx / NFR-x references in code comments point to it.

## Commands

```sh
cargo build
cargo test                                   # unit + integration (integration spawns tests/support/fake_agent.rs)
cargo test --test acp_integration            # only the fake-agent integration suite
cargo test --test acp_integration crash_     # one integration test by name prefix
cargo test --lib render::                    # unit tests of one module
cargo clippy --all-targets
```

Run locally (needs a Feishu app and either Claude Code login or an API key for the chosen agent):

```sh
APP_ID=... APP_SECRET=... AGENT_KIND=claude AGENT_CWD=/abs/path ALLOWED_OPEN_IDS=ou_... cargo run
RUST_LOG=info,agent=debug cargo run          # also show the agent subprocess's stderr
```

Startup refuses to run (exit code 2) on missing/invalid `AGENT_KIND` or non-absolute `AGENT_CWD`.
Full config table is in `README.md`.

## Architecture

Data flow: Feishu ws event → `SessionManager::handle_event` → per-chat queue → `run_turn` →
`AcpClient::prompt` → agent subprocess (stdio JSON-RPC) → `session/update` routed back by
`SessionId` → `TurnState` → Feishu card patched in place.

Layers (one directory each under `src/`):

- `lark/` — thin wrapper over `larksuite-oapi-sdk-rs`. `LarkApi` does reply/send/patch/update/download;
  `text.rs` extracts text from text/post/image messages and strips @-mentions.
- `session/` — the orchestrator. `SessionManager` owns `chat_id → ChatState` (ACP `SessionId`, cwd,
  queue, running flag). One worker task per chat drains its queue serially; chats run concurrently.
  Also handles `/new /cancel /status /cwd /help`, approval-card callbacks, the idle reaper, and
  agent-status notifications. All user-facing failure text goes through `user_message_for`.
- `acp/` — `AcpClient` is a cheap-to-clone handle. `run()` is a supervisor loop: spawn agent →
  `initialize` → wait for EOF → exponential backoff (1s…60s) → respawn. A `generation` counter bumps
  on every successful `initialize`; `ensure_session` compares it to invalidate stale `SessionId`s.
  `session/update` notifications are routed to whoever called `subscribe(session_id, ..)`.
  `permission.rs` holds `session/request_permission` `Responder`s for `ask` mode so the ACP dispatch
  loop is never blocked; auto policies answer inline.
- `render/` — `TurnState` accumulates one turn (text, thoughts, tool calls, plan, usage) and renders
  Markdown; `card.rs` builds Feishu card JSON (schema 2.0); `split.rs` splits long Markdown without
  breaking code fences.
- `agent.rs` — `AGENT_KIND` → command line. Order: explicit `AGENT_COMMAND`, else the adapter binary
  (`codex-acp` / `claude-agent-acp`) if found on `PATH`, else `npx -y <package>`. The Docker image relies on
  the `PATH` lookup; there is no entrypoint script.
- `config.rs` — all env parsing. `Config::from_pairs` exists so tests avoid touching the process env.

## Non-obvious constraints

- `agent-client-protocol` is 3.x: builder API (`Client.builder().on_receive_*().connect_with(AcpAgent)`),
  no `Client` trait. `on_receive_*` callbacks run inside the connection's dispatch loop and must return
  quickly; never `block_task()` a request from inside one. `ConnectionTo<Agent>` is Clone+Send+Sync and
  is stored in `AcpClient` so other tasks can send requests.
- Agent credentials (`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, …) are inherited by the subprocess from the
  bot's environment. The bot must not read or log them (FR-AGENT-5 / FR-OPS-3).
- Feishu `im.message.patch` only works on `interactive` (card) messages; text messages need `update`.
  Streaming progress therefore always goes through a card; text is a fallback when the card send fails.
- Card button callbacks (`card.action.trigger`) arrive over the same ws long connection; there is no
  HTTP callback server. Button payload shape is `{action:"acp_permission", ticket_id, option_id}`.
- Never hold a `std::sync::Mutex` guard across an `.await` in `session/` (futures must be `Send` for
  `tokio::spawn`). The `enqueue` function shows the pattern: compute a decision enum under the lock,
  drop it, then await.
- `tests/support/fake_agent.rs` is a scripted ACP agent built as a `[[bin]]` (`test = false`) so the
  integration test can reference it via `env!("CARGO_BIN_EXE_fake_agent")`. Prompt text selects the
  script: `hello`, `perm`, `slow`, `crash`. Extend it rather than mocking the ACP crate.
- Not implemented: `session/load` restore after restart (FR-SESSION-9); session state is in-memory only.
- `larksuite-oapi-sdk-rs` with the `ws` feature runs `prost-build` in its build script, so `protoc` must be on
  `PATH` at compile time (macOS: `brew install protobuf`; the Dockerfile builder stage installs
  `protobuf-compiler`). A failure that reads `protoc failed:` is this, not a code error.
