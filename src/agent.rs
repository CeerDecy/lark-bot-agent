//! Agent 适配层：根据 `AGENT_KIND` 推导子进程命令行（FR-AGENT-1~3）。
use crate::config::{AgentKind, Config};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentLaunch {
    pub command: String,
    pub args: Vec<String>,
}

impl AgentLaunch {
    pub fn from_config(cfg: &Config) -> Self {
        Self::resolve(cfg.agent_kind, cfg.agent_command.as_deref(), cfg.agent_args.clone())
    }

    /// `AGENT_COMMAND` / `AGENT_ARGS` 任意一个被设置时覆盖对应默认值。
    pub fn resolve(kind: AgentKind, command: Option<&str>, args: Option<Vec<String>>) -> Self {
        let (default_cmd, default_args) = match kind {
            AgentKind::Codex => ("npx", vec!["-y", "@agentclientprotocol/codex-acp"]),
            AgentKind::Claude => ("npx", vec!["-y", "@agentclientprotocol/claude-agent-acp"]),
        };
        let command = command.unwrap_or(default_cmd).to_string();
        let args = match (command.as_str() != default_cmd, args) {
            // 覆盖了命令但没给参数：默认参数只对 npx 有意义，清空。
            (true, None) => Vec::new(),
            (_, Some(a)) => a,
            (false, None) => default_args.into_iter().map(str::to_string).collect(),
        };
        Self { command, args }
    }

    pub fn display(&self) -> String {
        std::iter::once(self.command.as_str())
            .chain(self.args.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_default() {
        let l = AgentLaunch::resolve(AgentKind::Codex, None, None);
        assert_eq!(l.display(), "npx -y @agentclientprotocol/codex-acp");
    }

    #[test]
    fn claude_default() {
        let l = AgentLaunch::resolve(AgentKind::Claude, None, None);
        assert_eq!(l.display(), "npx -y @agentclientprotocol/claude-agent-acp");
    }

    #[test]
    fn command_override_drops_npx_args() {
        let l = AgentLaunch::resolve(AgentKind::Codex, Some("/usr/local/bin/codex-acp"), None);
        assert_eq!(l.command, "/usr/local/bin/codex-acp");
        assert!(l.args.is_empty());
    }

    #[test]
    fn args_override_keeps_command() {
        let l = AgentLaunch::resolve(AgentKind::Claude, None, Some(vec!["-y".into(), "pkg@1.2".into()]));
        assert_eq!(l.display(), "npx -y pkg@1.2");
    }
}
