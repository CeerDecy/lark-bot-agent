//! 超长消息按段落切分（FR-RENDER-7），保持代码围栏闭合。

pub fn split_markdown(text: &str, max_chars: usize) -> Vec<String> {
    let max_chars = max_chars.max(64);
    if text.chars().count() <= max_chars {
        return vec![text.to_string()];
    }
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut cur_len = 0usize;
    let mut in_fence: Option<String> = None;

    let flush = |cur: &mut String, cur_len: &mut usize, parts: &mut Vec<String>, in_fence: &Option<String>| {
        if cur.trim().is_empty() {
            cur.clear();
            *cur_len = 0;
            return;
        }
        if in_fence.is_some() {
            cur.push_str("\n```");
        }
        parts.push(cur.trim_end().to_string());
        cur.clear();
        *cur_len = 0;
        if let Some(f) = in_fence {
            cur.push_str(f);
            cur.push('\n');
            *cur_len = f.chars().count() + 1;
        }
    };

    for para in text.split("\n\n") {
        let para_len = para.chars().count() + 2;
        if cur_len + para_len > max_chars && cur_len > 0 {
            flush(&mut cur, &mut cur_len, &mut parts, &in_fence);
        }
        if para_len > max_chars {
            // 单个段落就超限：按行再切。
            for line in para.split('\n') {
                let line_len = line.chars().count() + 1;
                if cur_len + line_len > max_chars && cur_len > 0 {
                    flush(&mut cur, &mut cur_len, &mut parts, &in_fence);
                }
                if line_len > max_chars {
                    let chars: Vec<char> = line.chars().collect();
                    for chunk in chars.chunks(max_chars - 8) {
                        let s: String = chunk.iter().collect();
                        if cur_len + s.chars().count() + 1 > max_chars && cur_len > 0 {
                            flush(&mut cur, &mut cur_len, &mut parts, &in_fence);
                        }
                        cur.push_str(&s);
                        cur.push('\n');
                        cur_len += s.chars().count() + 1;
                    }
                } else {
                    track_fence(line, &mut in_fence);
                    cur.push_str(line);
                    cur.push('\n');
                    cur_len += line_len;
                }
            }
            cur.push('\n');
            cur_len += 1;
        } else {
            for line in para.split('\n') {
                track_fence(line, &mut in_fence);
            }
            cur.push_str(para);
            cur.push_str("\n\n");
            cur_len += para_len;
        }
    }
    if !cur.trim().is_empty() {
        if in_fence.is_some() {
            cur.push_str("\n```");
        }
        parts.push(cur.trim_end().to_string());
    }
    parts
}

fn track_fence(line: &str, in_fence: &mut Option<String>) {
    let t = line.trim_start();
    if t.starts_with("```") {
        if in_fence.is_some() {
            *in_fence = None;
        } else {
            *in_fence = Some(t.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_untouched() {
        assert_eq!(split_markdown("hi", 100), vec!["hi".to_string()]);
    }

    #[test]
    fn splits_on_paragraphs_within_limit() {
        let p = "x".repeat(60);
        let text = format!("{p}\n\n{p}\n\n{p}");
        let parts = split_markdown(&text, 130);
        assert_eq!(parts.len(), 2);
        for part in &parts {
            assert!(part.chars().count() <= 130);
        }
        assert_eq!(parts.concat().matches('x').count(), 180);
    }

    #[test]
    fn keeps_code_fences_balanced() {
        let body = (0..40).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let text = format!("intro\n\n```rust\n{body}\n```\n\noutro");
        let parts = split_markdown(&text, 120);
        assert!(parts.len() >= 2);
        for part in &parts {
            assert_eq!(part.matches("```").count() % 2, 0, "unbalanced fence in {part:?}");
        }
    }

    #[test]
    fn oversized_single_line_is_hard_split() {
        let text = "y".repeat(500);
        let parts = split_markdown(&text, 100);
        assert!(parts.len() >= 5);
        assert_eq!(parts.concat().matches('y').count(), 500);
    }
}
