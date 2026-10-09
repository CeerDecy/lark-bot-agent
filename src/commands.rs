//! 用户指令解析（FR-SESSION-4~7）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    New,
    Cancel,
    Status,
    Cwd(String),
    Help,
    Unknown(String),
}

/// 以 `/` 开头的消息视为指令；否则返回 None。
pub fn parse(text: &str) -> Option<Command> {
    let t = text.trim();
    if !t.starts_with('/') {
        return None;
    }
    let mut parts = t.splitn(2, char::is_whitespace);
    let head = parts.next().unwrap_or("").to_ascii_lowercase();
    let rest = parts.next().map(str::trim).unwrap_or("");
    Some(match head.as_str() {
        "/new" => Command::New,
        "/cancel" => Command::Cancel,
        "/status" => Command::Status,
        "/help" => Command::Help,
        "/cwd" => Command::Cwd(rest.to_string()),
        _ => Command::Unknown(head),
    })
}

pub const HELP_TEXT: &str = "可用指令：\n\
/new — 结束当前会话，下一条消息新建会话\n\
/cancel — 取消正在进行的轮次并清空排队\n\
/status — 查看 agent、会话、cwd、排队情况\n\
/cwd <绝对路径> — 切换下一个新会话的工作目录\n\
/help — 显示本帮助";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_is_not_command() {
        assert_eq!(parse("hello /new"), None);
        assert_eq!(parse(""), None);
    }

    #[test]
    fn known_commands() {
        assert_eq!(parse("/new"), Some(Command::New));
        assert_eq!(parse("  /CANCEL "), Some(Command::Cancel));
        assert_eq!(parse("/status"), Some(Command::Status));
        assert_eq!(parse("/help"), Some(Command::Help));
    }

    #[test]
    fn cwd_with_argument() {
        assert_eq!(parse("/cwd /srv/app"), Some(Command::Cwd("/srv/app".into())));
        assert_eq!(parse("/cwd"), Some(Command::Cwd(String::new())));
    }

    #[test]
    fn unknown_command() {
        assert_eq!(parse("/foo bar"), Some(Command::Unknown("/foo".into())));
    }
}
