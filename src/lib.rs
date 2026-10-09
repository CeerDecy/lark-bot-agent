//! 飞书远程 Coding Agent 代理机器人。
//!
//! 分层：`lark`（飞书接入）→ `session`（会话管理）→ `acp`（ACP 客户端）→ agent 子进程。
pub mod acp;
pub mod agent;
pub mod commands;
pub mod config;
pub mod lark;
pub mod render;
pub mod session;
