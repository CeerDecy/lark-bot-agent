//! 飞书卡片（schema 2.0）JSON 构造（FR-RENDER-8、FR-PERM-2）。
use serde_json::{Value, json};

use crate::acp::permission::PermissionTicket;

/// 回复/进度卡片：标题 + Markdown 正文。
pub fn message_card(title: &str, template: &str, markdown: &str) -> Value {
    json!({
        "schema": "2.0",
        "config": { "wide_screen_mode": true, "update_multi": true },
        "header": {
            "title": { "tag": "plain_text", "content": title },
            "template": template
        },
        "body": {
            "elements": [
                { "tag": "markdown", "content": markdown }
            ]
        }
    })
}

/// 审批卡片：标题、kind、rawInput 摘要 + 每个 option 一个按钮。
pub fn permission_card(ticket: &PermissionTicket, requester_name: &str) -> Value {
    let mut lines = vec![format!("**{}**", escape_md(&ticket.title))];
    if let Some(k) = &ticket.kind {
        lines.push(format!("类型：`{k}`"));
    }
    if let Some(raw) = &ticket.raw_input {
        let summary = summarize_raw_input(raw, 600);
        if !summary.is_empty() {
            lines.push(format!("```\n{summary}\n```"));
        }
    }
    lines.push(format!("只有 {requester_name} 或管理员可以审批。"));

    let buttons: Vec<Value> = ticket
        .options
        .iter()
        .map(|o| {
            let kind = format!("{:?}", o.kind).to_ascii_lowercase();
            let style = if kind.starts_with("allow") { "primary" } else { "danger" };
            json!({
                "tag": "button",
                "text": { "tag": "plain_text", "content": o.name },
                "type": style,
                "behaviors": [{
                    "type": "callback",
                    "value": {
                        "action": "acp_permission",
                        "ticket_id": ticket.ticket_id,
                        "option_id": o.option_id.0.as_ref()
                    }
                }]
            })
        })
        .collect();

    json!({
        "schema": "2.0",
        "config": { "wide_screen_mode": true, "update_multi": true },
        "header": {
            "title": { "tag": "plain_text", "content": "agent 请求权限" },
            "template": "orange"
        },
        "body": {
            "elements": [
                { "tag": "markdown", "content": lines.join("\n") },
                { "tag": "column_set", "flex_mode": "none", "columns": [{
                    "tag": "column", "width": "auto", "elements": buttons
                }] }
            ]
        }
    })
}

/// 审批完成后把卡片改成只读结果。
pub fn permission_result_card(ticket_title: &str, result: &str, template: &str) -> Value {
    json!({
        "schema": "2.0",
        "config": { "wide_screen_mode": true, "update_multi": true },
        "header": {
            "title": { "tag": "plain_text", "content": "agent 请求权限" },
            "template": template
        },
        "body": {
            "elements": [
                { "tag": "markdown", "content": format!("**{}**\n\n{}", escape_md(ticket_title), result) }
            ]
        }
    })
}

pub fn summarize_raw_input(raw: &Value, max_chars: usize) -> String {
    let s = match raw {
        Value::String(s) => s.clone(),
        Value::Object(map) => {
            // 常见字段优先：command / path / file_path。
            for key in ["command", "cmd", "path", "file_path", "filePath", "pattern", "url"] {
                if let Some(v) = map.get(key) {
                    let v = match v {
                        Value::String(s) => s.clone(),
                        Value::Array(a) => a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(" "),
                        other => other.to_string(),
                    };
                    return truncate(&format!("{key}: {v}"), max_chars);
                }
            }
            serde_json::to_string_pretty(raw).unwrap_or_default()
        }
        other => other.to_string(),
    };
    truncate(&s, max_chars)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

fn escape_md(s: &str) -> String {
    s.replace('*', "\\*").replace('_', "\\_")
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{PermissionOption, PermissionOptionKind, SessionId};

    #[test]
    fn message_card_has_markdown_element() {
        let c = message_card("处理中", "blue", "**hi**");
        assert_eq!(c["header"]["template"], "blue");
        assert_eq!(c["body"]["elements"][0]["tag"], "markdown");
        assert_eq!(c["body"]["elements"][0]["content"], "**hi**");
    }

    #[test]
    fn permission_card_lists_options_as_buttons() {
        let t = PermissionTicket {
            ticket_id: "abc".into(),
            session_id: SessionId::new("s"),
            title: "运行 cargo test".into(),
            kind: Some("execute".into()),
            raw_input: Some(json!({"command": ["cargo", "test"]})),
            options: vec![
                PermissionOption::new("y", "允许", PermissionOptionKind::AllowOnce),
                PermissionOption::new("n", "拒绝", PermissionOptionKind::RejectOnce),
            ],
        };
        let c = permission_card(&t, "张三");
        let buttons = c["body"]["elements"][1]["columns"][0]["elements"].as_array().unwrap();
        assert_eq!(buttons.len(), 2);
        assert_eq!(buttons[0]["behaviors"][0]["value"]["ticket_id"], "abc");
        assert_eq!(buttons[0]["behaviors"][0]["value"]["option_id"], "y");
        assert_eq!(buttons[0]["type"], "primary");
        assert_eq!(buttons[1]["type"], "danger");
        let md = c["body"]["elements"][0]["content"].as_str().unwrap();
        assert!(md.contains("command: cargo test"));
        assert!(md.contains("execute"));
    }

    #[test]
    fn raw_input_summary_truncates() {
        let s = summarize_raw_input(&json!({"command": "x".repeat(50)}), 20);
        assert!(s.ends_with('…'));
        assert!(s.chars().count() <= 21);
    }
}
