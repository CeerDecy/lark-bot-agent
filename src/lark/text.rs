//! 飞书消息内容提取（FR-IM-2/3/4/5）。
use larksuite_oapi_sdk_rs::events::im::Message;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inbound {
    Text(String),
    Image { image_key: String },
    Unsupported(String),
}

/// 根据消息类型提取可转发的内容。群聊里会去掉 @ 片段。
pub fn extract(message: &Message, bot_open_id: Option<&str>) -> Inbound {
    match message.message_type.as_str() {
        "text" => {
            let raw = extract_text("text", &message.content);
            Inbound::Text(strip_mentions(&raw, message, bot_open_id))
        }
        "post" => Inbound::Text(strip_mentions(&extract_post_text(&message.content), message, bot_open_id)),
        "image" => match serde_json::from_str::<Value>(&message.content)
            .ok()
            .and_then(|v| v.get("image_key").and_then(Value::as_str).map(str::to_string))
        {
            Some(image_key) => Inbound::Image { image_key },
            None => Inbound::Unsupported("image".into()),
        },
        other => Inbound::Unsupported(other.to_string()),
    }
}

/// 文本消息取 `text` 字段，其他类型直接返回原始 content JSON。
pub fn extract_text(message_type: &str, content: &str) -> String {
    if message_type == "text"
        && let Ok(value) = serde_json::from_str::<Value>(content)
            && let Some(text) = value.get("text").and_then(|t| t.as_str()) {
                return text.to_string();
            }
    content.to_string()
}

/// 富文本（post）：把所有段落里的 text/a/code 等拼成纯文本，`at` 节点保留 @占位 key 以便后续剔除。
pub fn extract_post_text(content: &str) -> String {
    let Ok(v) = serde_json::from_str::<Value>(content) else {
        return content.to_string();
    };
    // 两种形态：{"zh_cn": {"title":..,"content":[[..]]}} 或 {"title":..,"content":[[..]]}
    let body = if v.get("content").is_some() {
        v.clone()
    } else {
        v.as_object()
            .and_then(|m| m.values().find(|x| x.get("content").is_some()).cloned())
            .unwrap_or(Value::Null)
    };
    let mut out = String::new();
    if let Some(title) = body.get("title").and_then(Value::as_str)
        && !title.is_empty() {
            out.push_str(title);
            out.push('\n');
        }
    if let Some(paras) = body.get("content").and_then(Value::as_array) {
        for para in paras {
            if let Some(nodes) = para.as_array() {
                for node in nodes {
                    match node.get("tag").and_then(Value::as_str) {
                        Some("text") | Some("code_block") | Some("md") => {
                            if let Some(t) = node.get("text").and_then(Value::as_str) {
                                out.push_str(t);
                            }
                        }
                        Some("a") => {
                            let t = node.get("text").and_then(Value::as_str).unwrap_or("");
                            let href = node.get("href").and_then(Value::as_str).unwrap_or("");
                            if t.is_empty() {
                                out.push_str(href);
                            } else {
                                out.push_str(&format!("{t} ({href})"));
                            }
                        }
                        Some("at") => {
                            if let Some(id) = node.get("user_id").and_then(Value::as_str) {
                                out.push_str(&format!("<at user_id=\"{id}\"></at>"));
                            }
                        }
                        _ => {}
                    }
                }
            }
            out.push('\n');
        }
    }
    out.trim().to_string()
}

/// 去掉所有 @ 片段。文本里 @ 以 `@_user_1` 这样的 key 出现，mentions 里给出 key→id 对应。
pub fn strip_mentions(text: &str, message: &Message, _bot_open_id: Option<&str>) -> String {
    let mut t = text.to_string();
    for m in &message.mentions {
        if !m.key.is_empty() {
            t = t.replace(&m.key, "");
        }
        if let Some(id) = m.open_id() {
            t = t.replace(&format!("<at user_id=\"{id}\"></at>"), "");
        }
    }
    // post 里的 at 可能用 open_id 以外的 id；兜底清掉所有 <at ...></at>
    while let Some(start) = t.find("<at ") {
        match t[start..].find("</at>") {
            Some(end) => t.replace_range(start..start + end + 5, ""),
            None => break,
        }
    }
    t.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 群聊里是否 @ 了机器人（FR-IM-3）。
pub fn mentions_bot(message: &Message, bot_open_id: Option<&str>) -> bool {
    match bot_open_id {
        Some(id) => message.mentions.iter().any(|m| m.open_id() == Some(id)),
        // 拿不到自己的 open_id 时退化为“有任何 @ 就算”，宁可多回不可漏回。
        None => !message.mentions.is_empty(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(message_type: &str, content: &str, mentions_json: &str) -> Message {
        let v = serde_json::json!({
            "message_id": "om_1",
            "chat_id": "oc_1",
            "chat_type": "group",
            "message_type": message_type,
            "content": content,
            "mentions": serde_json::from_str::<Value>(mentions_json).unwrap(),
        });
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn text_message_returns_inner_text() {
        assert_eq!(extract_text("text", r#"{"text":"hi"}"#), "hi");
    }

    #[test]
    fn non_text_message_returns_raw_content() {
        let raw = r#"{"image_key":"img_v2_abc"}"#;
        assert_eq!(extract_text("image", raw), raw);
    }

    #[test]
    fn malformed_text_content_falls_back_to_raw() {
        assert_eq!(extract_text("text", "not json"), "not json");
    }

    #[test]
    fn strips_mention_key_from_group_text() {
        let m = msg(
            "text",
            r#"{"text":"@_user_1 帮我修 bug"}"#,
            r#"[{"key":"@_user_1","id":{"open_id":"ou_bot"},"name":"bot"}]"#,
        );
        assert_eq!(extract(&m, Some("ou_bot")), Inbound::Text("帮我修 bug".into()));
        assert!(mentions_bot(&m, Some("ou_bot")));
        assert!(!mentions_bot(&m, Some("ou_other")));
    }

    #[test]
    fn post_message_is_flattened() {
        let content = r#"{"zh_cn":{"title":"标题","content":[[{"tag":"text","text":"第一段 "},{"tag":"a","text":"链接","href":"https://x.y"}],[{"tag":"at","user_id":"ou_bot"},{"tag":"text","text":"第二段"}]]}}"#;
        let m = msg("post", content, r#"[{"key":"@_user_1","id":{"open_id":"ou_bot"},"name":"bot"}]"#);
        assert_eq!(extract(&m, Some("ou_bot")), Inbound::Text("标题 第一段 链接 (https://x.y) 第二段".into()));
    }

    #[test]
    fn image_message_yields_key() {
        let m = msg("image", r#"{"image_key":"img_v2_abc"}"#, "[]");
        assert_eq!(extract(&m, None), Inbound::Image { image_key: "img_v2_abc".into() });
    }

    #[test]
    fn unsupported_type() {
        let m = msg("sticker", r#"{}"#, "[]");
        assert_eq!(extract(&m, None), Inbound::Unsupported("sticker".into()));
    }
}
