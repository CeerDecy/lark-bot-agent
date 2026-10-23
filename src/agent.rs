//! Agent 适配层：根据 `AGENT_KIND` 推导子进程命令行（FR-AGENT-1~3）。
use std::path::{Path, PathBuf};

use crate::config::{AgentKind, Config};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentLaunch {
    pub command: String,
    pub args: Vec<String>,
}

impl AgentLaunch {
    pub fn from_config(cfg: &Config) -> Self {
        let path = std::env::var_os("PATH").unwrap_or_default();
        Self::resolve(cfg.agent_kind, cfg.agent_command.as_deref(), cfg.agent_args.clone(), &path)
    }

    /// `AGENT_COMMAND` / `AGENT_ARGS` 任意一个被设置时覆盖对应默认值。
    /// 未覆盖命令时，若 `PATH` 里已有全局安装的适配器二进制（`npm install -g`），直接用它，
    /// 否则退回 `npx -y <包名>`。
    pub fn resolve(kind: AgentKind, command: Option<&str>, args: Option<Vec<String>>, path: &std::ffi::OsStr) -> Self {
        let (bin, package) = match kind {
            AgentKind::Codex => ("codex-acp", "@agentclientprotocol/codex-acp"),
            AgentKind::Claude => ("claude-agent-acp", "@agentclientprotocol/claude-agent-acp"),
        };
        if let Some(c) = command {
            return Self { command: c.to_string(), args: args.unwrap_or_default() };
        }
        if let Some(found) = find_in_path(bin, path) {
            return Self { command: found.to_string_lossy().into_owned(), args: args.unwrap_or_default() };
        }
        Self {
            command: "npx".to_string(),
            args: args.unwrap_or_else(|| vec!["-y".to_string(), package.to_string()]),
        }
    }

    pub fn display(&self) -> String {
        std::iter::once(self.command.as_str())
            .chain(self.args.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

fn find_in_path(bin: &str, path: &std::ffi::OsStr) -> Option<PathBuf> {
    std::env::split_paths(path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(bin))
        .find(|candidate| is_executable(candidate))
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    p.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    fn empty_path() -> &'static OsStr {
        OsStr::new("")
    }

    #[test]
    fn codex_default_falls_back_to_npx() {
        let l = AgentLaunch::resolve(AgentKind::Codex, None, None, empty_path());
        assert_eq!(l.display(), "npx -y @agentclientprotocol/codex-acp");
    }

    #[test]
    fn claude_default_falls_back_to_npx() {
        let l = AgentLaunch::resolve(AgentKind::Claude, None, None, empty_path());
        assert_eq!(l.display(), "npx -y @agentclientprotocol/claude-agent-acp");
    }

    #[test]
    fn command_override_drops_npx_args() {
        let l = AgentLaunch::resolve(AgentKind::Codex, Some("/usr/local/bin/codex-acp"), None, empty_path());
        assert_eq!(l.command, "/usr/local/bin/codex-acp");
        assert!(l.args.is_empty());
    }

    #[test]
    fn args_override_keeps_npx_command() {
        let l = AgentLaunch::resolve(AgentKind::Claude, None, Some(vec!["-y".into(), "pkg@1.2".into()]), empty_path());
        assert_eq!(l.display(), "npx -y pkg@1.2");
    }

    #[cfg(unix)]
    #[test]
    fn prefers_binary_found_in_path() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("lark-bot-agent-path-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("claude-agent-acp");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();

        let path = std::env::join_paths([dir.as_path(), Path::new("/nonexistent")]).unwrap();
        let l = AgentLaunch::resolve(AgentKind::Claude, None, None, &path);
        assert_eq!(l.command, bin.to_string_lossy());
        assert!(l.args.is_empty());

        // 非可执行文件不算
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o644)).unwrap();
        let l = AgentLaunch::resolve(AgentKind::Claude, None, None, &path);
        assert_eq!(l.command, "npx");

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
