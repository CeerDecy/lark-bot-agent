//! 环境变量解析与校验（需求文档第 7 章）。
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentKind {
    Codex,
    Claude,
}

impl FromStr for AgentKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "codex" => Ok(Self::Codex),
            "claude" => Ok(Self::Claude),
            other => Err(format!("AGENT_KIND 取值非法: {other:?}，只接受 codex 或 claude")),
        }
    }
}

impl AgentKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionPolicy {
    Ask,
    AutoAllow,
    AutoReject,
}

impl FromStr for PermissionPolicy {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "ask" => Ok(Self::Ask),
            "auto_allow" => Ok(Self::AutoAllow),
            "auto_reject" => Ok(Self::AutoReject),
            other => Err(format!(
                "PERMISSION_POLICY 取值非法: {other:?}，只接受 ask / auto_allow / auto_reject"
            )),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub app_id: String,
    pub app_secret: String,
    pub agent_kind: AgentKind,
    pub agent_command: Option<String>,
    pub agent_args: Option<Vec<String>>,
    pub agent_cwd: PathBuf,
    pub allowed_cwd_roots: Vec<PathBuf>,
    pub allowed_open_ids: Vec<String>,
    pub admin_open_id: Option<String>,
    pub permission_policy: PermissionPolicy,
    pub permission_timeout: Duration,
    pub prompt_timeout: Duration,
    pub session_idle: Duration,
    pub queue_limit_per_chat: usize,
    pub stream_update_interval: Duration,
    pub stream_update_chars: usize,
    pub show_thoughts: bool,
    pub mcp_servers: Vec<serde_json::Value>,
    pub acp_trace_dir: Option<PathBuf>,
    pub max_message_chars: usize,
    /// 新会话建立后通过 `session/set_mode` 切到的 agent 侧模式（如 claude 的 `bypassPermissions`）。
    pub agent_mode: Option<String>,
}

impl Config {
    /// 从进程环境读取配置。返回 Err 时应直接启动失败。
    pub fn from_env() -> Result<Self, String> {
        let vars: Vec<(String, String)> = std::env::vars().collect();
        Self::from_pairs(&vars)
    }

    pub fn from_pairs(pairs: &[(String, String)]) -> Result<Self, String> {
        let get = |k: &str| -> Option<String> {
            pairs
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let required = |k: &str| get(k).ok_or_else(|| format!("缺少必填环境变量 {k}"));

        let agent_kind: AgentKind = required("AGENT_KIND")?.parse()?;
        let agent_cwd = PathBuf::from(required("AGENT_CWD")?);
        if !agent_cwd.is_absolute() {
            return Err(format!("AGENT_CWD 必须是绝对路径: {}", agent_cwd.display()));
        }

        let allowed_cwd_roots = match get("ALLOWED_CWD_ROOTS") {
            Some(v) => v
                .split(':')
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .collect(),
            None => vec![agent_cwd.clone()],
        };
        for root in &allowed_cwd_roots {
            if !root.is_absolute() {
                return Err(format!("ALLOWED_CWD_ROOTS 必须是绝对路径: {}", root.display()));
            }
        }

        let agent_args = get("AGENT_ARGS")
            .map(|v| v.split_whitespace().map(str::to_string).collect::<Vec<_>>());

        let mcp_servers = match get("AGENT_MCP_SERVERS_JSON") {
            None => Vec::new(),
            Some(raw) => serde_json::from_str::<serde_json::Value>(&raw)
                .map_err(|e| format!("AGENT_MCP_SERVERS_JSON 不是合法 JSON: {e}"))?
                .as_array()
                .cloned()
                .ok_or_else(|| "AGENT_MCP_SERVERS_JSON 必须是 JSON 数组".to_string())?,
        };

        Ok(Self {
            app_id: required("APP_ID")?,
            app_secret: required("APP_SECRET")?,
            agent_kind,
            agent_command: get("AGENT_COMMAND"),
            agent_args,
            agent_cwd,
            allowed_cwd_roots,
            allowed_open_ids: get("ALLOWED_OPEN_IDS")
                .map(|v| v.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect())
                .unwrap_or_default(),
            admin_open_id: get("ADMIN_OPEN_ID"),
            permission_policy: get("PERMISSION_POLICY")
                .map(|v| v.parse())
                .transpose()?
                .unwrap_or(PermissionPolicy::Ask),
            permission_timeout: Duration::from_secs(parse_num(&get, "PERMISSION_TIMEOUT_SECS", 600)?),
            prompt_timeout: Duration::from_secs(parse_num(&get, "PROMPT_TIMEOUT_SECS", 1800)?),
            session_idle: Duration::from_secs(parse_num(&get, "SESSION_IDLE_SECS", 7200)?),
            queue_limit_per_chat: parse_num(&get, "QUEUE_LIMIT_PER_CHAT", 10)? as usize,
            stream_update_interval: Duration::from_millis(parse_num(&get, "STREAM_UPDATE_INTERVAL_MS", 2000)?),
            stream_update_chars: parse_num(&get, "STREAM_UPDATE_CHARS", 500)? as usize,
            show_thoughts: parse_bool(&get, "SHOW_THOUGHTS", false)?,
            mcp_servers,
            acp_trace_dir: get("ACP_TRACE_DIR").map(PathBuf::from),
            max_message_chars: parse_num(&get, "MAX_MESSAGE_CHARS", 4000)? as usize,
            agent_mode: get("AGENT_MODE"),
        })
    }

    /// 允许所有人时返回 true（需求 FR-IM-8：启动时打 warn）。
    pub fn allows_everyone(&self) -> bool {
        self.allowed_open_ids.is_empty()
    }

    pub fn is_allowed(&self, open_id: &str) -> bool {
        self.allows_everyone()
            || self.allowed_open_ids.iter().any(|id| id == open_id)
            || self.admin_open_id.as_deref() == Some(open_id)
    }

    pub fn is_admin(&self, open_id: &str) -> bool {
        self.admin_open_id.as_deref() == Some(open_id)
    }

    /// `/cwd` 切换目标必须在 `ALLOWED_CWD_ROOTS` 之内（按规范化后的路径前缀判断）。
    pub fn cwd_allowed(&self, path: &std::path::Path) -> bool {
        let Ok(canon) = std::fs::canonicalize(path) else {
            return false;
        };
        self.allowed_cwd_roots.iter().any(|root| match std::fs::canonicalize(root) {
            Ok(r) => canon.starts_with(&r),
            Err(_) => canon.starts_with(root),
        })
    }
}

fn parse_num(
    get: &dyn Fn(&str) -> Option<String>,
    key: &str,
    default: u64,
) -> Result<u64, String> {
    match get(key) {
        None => Ok(default),
        Some(v) => v.parse::<u64>().map_err(|_| format!("{key} 必须是非负整数: {v:?}")),
    }
}

fn parse_bool(
    get: &dyn Fn(&str) -> Option<String>,
    key: &str,
    default: bool,
) -> Result<bool, String> {
    match get(key) {
        None => Ok(default),
        Some(v) => match v.to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Ok(true),
            "0" | "false" | "no" | "off" => Ok(false),
            other => Err(format!("{key} 必须是布尔值: {other:?}")),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Vec<(String, String)> {
        vec![
            ("APP_ID".into(), "cli_x".into()),
            ("APP_SECRET".into(), "sec".into()),
            ("AGENT_KIND".into(), "codex".into()),
            ("AGENT_CWD".into(), "/tmp".into()),
        ]
    }

    #[test]
    fn minimal_config_has_documented_defaults() {
        let c = Config::from_pairs(&base()).unwrap();
        assert_eq!(c.agent_kind, AgentKind::Codex);
        assert_eq!(c.permission_policy, PermissionPolicy::Ask);
        assert_eq!(c.permission_timeout, Duration::from_secs(600));
        assert_eq!(c.prompt_timeout, Duration::from_secs(1800));
        assert_eq!(c.session_idle, Duration::from_secs(7200));
        assert_eq!(c.queue_limit_per_chat, 10);
        assert_eq!(c.stream_update_interval, Duration::from_millis(2000));
        assert!(!c.show_thoughts);
        assert_eq!(c.allowed_cwd_roots, vec![PathBuf::from("/tmp")]);
        assert!(c.allows_everyone());
        assert!(c.mcp_servers.is_empty());
        assert!(c.agent_mode.is_none());
    }

    #[test]
    fn agent_mode_is_passed_through() {
        let mut p = base();
        p.push(("AGENT_MODE".into(), " bypassPermissions ".into()));
        assert_eq!(Config::from_pairs(&p).unwrap().agent_mode.as_deref(), Some("bypassPermissions"));
    }

    #[test]
    fn missing_agent_kind_fails() {
        let mut p = base();
        p.retain(|(k, _)| k != "AGENT_KIND");
        assert!(Config::from_pairs(&p).unwrap_err().contains("AGENT_KIND"));
    }

    #[test]
    fn invalid_agent_kind_fails() {
        let mut p = base();
        p.iter_mut().find(|(k, _)| k == "AGENT_KIND").unwrap().1 = "gemini".into();
        assert!(Config::from_pairs(&p).unwrap_err().contains("gemini"));
    }

    #[test]
    fn relative_cwd_fails() {
        let mut p = base();
        p.iter_mut().find(|(k, _)| k == "AGENT_CWD").unwrap().1 = "rel".into();
        assert!(Config::from_pairs(&p).unwrap_err().contains("绝对路径"));
    }

    #[test]
    fn whitelist_and_admin() {
        let mut p = base();
        p.push(("ALLOWED_OPEN_IDS".into(), "ou_a, ou_b".into()));
        p.push(("ADMIN_OPEN_ID".into(), "ou_admin".into()));
        let c = Config::from_pairs(&p).unwrap();
        assert!(c.is_allowed("ou_a"));
        assert!(c.is_allowed("ou_b"));
        assert!(c.is_allowed("ou_admin"));
        assert!(!c.is_allowed("ou_c"));
        assert!(c.is_admin("ou_admin"));
    }

    #[test]
    fn mcp_servers_must_be_array() {
        let mut p = base();
        p.push(("AGENT_MCP_SERVERS_JSON".into(), "{}".into()));
        assert!(Config::from_pairs(&p).is_err());
        p.last_mut().unwrap().1 = r#"[{"name":"x","command":"y","args":[]}]"#.into();
        assert_eq!(Config::from_pairs(&p).unwrap().mcp_servers.len(), 1);
    }

    #[test]
    fn cwd_allowed_respects_roots() {
        let dir = std::env::temp_dir().join(format!("lark-bot-cfg-{}", std::process::id()));
        let inner = dir.join("inner");
        std::fs::create_dir_all(&inner).unwrap();
        let mut p = base();
        p.iter_mut().find(|(k, _)| k == "AGENT_CWD").unwrap().1 = dir.to_string_lossy().into();
        let c = Config::from_pairs(&p).unwrap();
        assert!(c.cwd_allowed(&inner));
        assert!(!c.cwd_allowed(std::path::Path::new("/")));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
