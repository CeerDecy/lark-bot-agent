//! ACP update → 飞书 Markdown / 卡片（FR-RENDER-*）。
pub mod card;
pub mod split;

use agent_client_protocol::schema::v1::{
    ContentBlock, Plan, PlanEntryStatus, SessionUpdate, StopReason, ToolCall, ToolCallContent,
    ToolCallStatus, ToolCallUpdate, ToolKind, UsageUpdate,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnPhase {
    Running,
    WaitingPermission,
    Done(StopReasonView),
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReasonView {
    EndTurn,
    Cancelled,
    MaxTokens,
    MaxTurnRequests,
    Refusal,
    Timeout,
}

impl From<StopReason> for StopReasonView {
    fn from(s: StopReason) -> Self {
        match s {
            StopReason::EndTurn => Self::EndTurn,
            StopReason::Cancelled => Self::Cancelled,
            StopReason::MaxTokens => Self::MaxTokens,
            StopReason::MaxTurnRequests => Self::MaxTurnRequests,
            StopReason::Refusal => Self::Refusal,
            _ => Self::EndTurn,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ToolLine {
    pub id: String,
    pub title: String,
    pub kind: ToolKind,
    pub status: ToolCallStatus,
    pub diff_paths: Vec<String>,
    pub diffs: Vec<(String, String)>,
}

/// 一轮对话里累积的所有可渲染状态（FR-RENDER-1）。
#[derive(Debug, Default, Clone)]
pub struct TurnState {
    pub text: String,
    /// 上一条 agent_message_chunk 的 messageId；变化时在正文里插入空行分段。
    last_message_id: Option<String>,
    pub thoughts: String,
    pub tools: Vec<ToolLine>,
    pub plan: Option<Plan>,
    pub usage: Option<(u64, u64)>,
    pub show_thoughts: bool,
    pub failure: Option<String>,
}

const MAX_DIFF_CHARS: usize = 1200;

impl TurnState {
    pub fn new(show_thoughts: bool) -> Self {
        Self { show_thoughts, ..Default::default() }
    }

    pub fn tool_call_count(&self) -> usize {
        self.tools.len()
    }

    pub fn apply(&mut self, update: SessionUpdate) {
        match update {
            SessionUpdate::AgentMessageChunk(c) => {
                if let ContentBlock::Text(t) = c.content {
                    let mid = c.message_id.as_ref().map(|m| m.0.to_string());
                    if mid.is_some() && self.last_message_id.is_some() && mid != self.last_message_id && !self.text.is_empty() {
                        let trimmed_len = self.text.trim_end().len();
                        self.text.truncate(trimmed_len);
                        self.text.push_str("\n\n");
                    }
                    if mid.is_some() {
                        self.last_message_id = mid;
                    }
                    self.text.push_str(&t.text);
                }
            }
            SessionUpdate::AgentThoughtChunk(c) => {
                if let ContentBlock::Text(t) = c.content {
                    self.thoughts.push_str(&t.text);
                }
            }
            SessionUpdate::ToolCall(tc) => self.on_tool_call(tc),
            SessionUpdate::ToolCallUpdate(u) => self.on_tool_update(u),
            SessionUpdate::Plan(p) => self.plan = Some(p),
            SessionUpdate::UsageUpdate(UsageUpdate { used, size, .. }) => self.usage = Some((used, size)),
            _ => {}
        }
    }

    fn on_tool_call(&mut self, tc: ToolCall) {
        let mut line = ToolLine {
            id: tc.tool_call_id.0.to_string(),
            title: tc.title,
            kind: tc.kind,
            status: tc.status,
            diff_paths: Vec::new(),
            diffs: Vec::new(),
        };
        absorb_content(&mut line, &tc.content);
        if let Some(existing) = self.tools.iter_mut().find(|t| t.id == line.id) {
            *existing = line;
        } else {
            self.tools.push(line);
        }
    }

    fn on_tool_update(&mut self, u: ToolCallUpdate) {
        let id = u.tool_call_id.0.to_string();
        let idx = match self.tools.iter().position(|t| t.id == id) {
            Some(i) => i,
            None => {
                self.tools.push(ToolLine {
                    id,
                    title: u.fields.title.clone().unwrap_or_else(|| "工具调用".into()),
                    kind: u.fields.kind.unwrap_or(ToolKind::Other),
                    status: u.fields.status.unwrap_or(ToolCallStatus::Pending),
                    diff_paths: Vec::new(),
                    diffs: Vec::new(),
                });
                self.tools.len() - 1
            }
        };
        let line = &mut self.tools[idx];
        if let Some(t) = u.fields.title {
            line.title = t;
        }
        if let Some(k) = u.fields.kind {
            line.kind = k;
        }
        if let Some(s) = u.fields.status {
            line.status = s;
        }
        if let Some(c) = &u.fields.content {
            absorb_content(line, c);
        }
    }

    /// 轮次结束 / 进行中都用这个函数渲染正文 Markdown。
    pub fn render_markdown(&self, phase: &TurnPhase) -> String {
        let mut out = String::new();
        let body = self.text.trim();
        if !body.is_empty() {
            out.push_str(body);
            out.push('\n');
        }
        if let Some(plan) = &self.plan
            && !plan.entries.is_empty() {
                out.push_str("\n**任务清单**\n");
                for e in &plan.entries {
                    let mark = match e.status {
                        PlanEntryStatus::Completed => "✅",
                        PlanEntryStatus::InProgress => "⏳",
                        PlanEntryStatus::Pending => "⬜",
                        _ => "⬜",
                    };
                    out.push_str(&format!("{mark} {}\n", e.content));
                }
            }
        if !self.tools.is_empty() {
            out.push_str("\n**工具调用**\n");
            for t in &self.tools {
                out.push_str(&format!("{} [{}] {} {}\n", status_icon(t.status), kind_label(t.kind), t.title, ""));
                for (path, diff) in &t.diffs {
                    out.push_str(&format!("```diff\n# {path}\n{diff}\n```\n"));
                }
            }
        }
        if self.show_thoughts && !self.thoughts.trim().is_empty() {
            out.push_str("\n**思考过程**\n");
            out.push_str(self.thoughts.trim());
            out.push('\n');
        }
        match phase {
            TurnPhase::Running => {
                if body.is_empty() && self.tools.is_empty() {
                    out.push_str("处理中…");
                }
            }
            TurnPhase::WaitingPermission => out.push_str("\n⏸ 等待审批…"),
            TurnPhase::Done(StopReasonView::EndTurn) => {
                if body.is_empty() {
                    out.push_str("（agent 没有返回文本）");
                }
            }
            TurnPhase::Done(StopReasonView::Cancelled) => out.push_str("\n已取消。"),
            TurnPhase::Done(StopReasonView::MaxTokens) => out.push_str("\n⚠️ 输出达到 token 上限，回复可能不完整。"),
            TurnPhase::Done(StopReasonView::MaxTurnRequests) => out.push_str("\n⚠️ 达到单轮最大模型调用次数，agent 已停止。"),
            TurnPhase::Done(StopReasonView::Refusal) => out.push_str("\n⚠️ agent 拒绝了这个请求。"),
            TurnPhase::Done(StopReasonView::Timeout) => out.push_str("\n⚠️ 本轮超时，已向 agent 发送取消。"),
            TurnPhase::Failed => {
                out.push_str("\n❌ ");
                out.push_str(self.failure.as_deref().unwrap_or("处理失败，请稍后重试。"));
            }
        }
        out.trim_end().to_string()
    }

    pub fn header(&self, phase: &TurnPhase) -> (&'static str, &'static str) {
        match phase {
            TurnPhase::Running => ("处理中", "blue"),
            TurnPhase::WaitingPermission => ("等待审批", "orange"),
            TurnPhase::Done(StopReasonView::EndTurn) => ("完成", "green"),
            TurnPhase::Done(StopReasonView::Cancelled) => ("已取消", "grey"),
            TurnPhase::Done(_) => ("已停止", "orange"),
            TurnPhase::Failed => ("失败", "red"),
        }
    }
}

fn absorb_content(line: &mut ToolLine, content: &[ToolCallContent]) {
    for c in content {
        if let ToolCallContent::Diff(d) = c {
            let path = d.path.display().to_string();
            let diff = unified_diff(d.old_text.as_deref().unwrap_or(""), &d.new_text);
            if !line.diff_paths.contains(&path) {
                line.diff_paths.push(path.clone());
            }
            line.diffs.retain(|(p, _)| p != &path);
            line.diffs.push((path, truncate(&diff, MAX_DIFF_CHARS)));
        }
    }
}

/// 极简逐行 diff：只标记被移除/新增的行，够在聊天里看一眼。
fn unified_diff(old: &str, new: &str) -> String {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    let mut out = String::new();
    let common_prefix = old_lines.iter().zip(&new_lines).take_while(|(a, b)| a == b).count();
    let max_suffix = old_lines.len().min(new_lines.len()) - common_prefix;
    let common_suffix = old_lines
        .iter()
        .rev()
        .zip(new_lines.iter().rev())
        .take(max_suffix)
        .take_while(|(a, b)| a == b)
        .count();
    for l in &old_lines[common_prefix..old_lines.len() - common_suffix] {
        out.push_str("- ");
        out.push_str(l);
        out.push('\n');
    }
    for l in &new_lines[common_prefix..new_lines.len() - common_suffix] {
        out.push_str("+ ");
        out.push_str(l);
        out.push('\n');
    }
    out.trim_end().to_string()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}\n… (已截断)")
}

pub fn status_icon(s: ToolCallStatus) -> &'static str {
    match s {
        ToolCallStatus::Completed => "✅",
        ToolCallStatus::Failed => "❌",
        ToolCallStatus::InProgress | ToolCallStatus::Pending => "⏳",
        _ => "⏳",
    }
}

pub fn kind_label(k: ToolKind) -> &'static str {
    match k {
        ToolKind::Read => "read",
        ToolKind::Edit => "edit",
        ToolKind::Delete => "delete",
        ToolKind::Move => "move",
        ToolKind::Search => "search",
        ToolKind::Execute => "exec",
        ToolKind::Think => "think",
        ToolKind::Fetch => "fetch",
        ToolKind::SwitchMode => "mode",
        _ => "tool",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{
        ContentChunk, Diff, PlanEntry, PlanEntryPriority, TextContent, ToolCallId, ToolCallUpdateFields,
    };

    fn chunk(s: &str) -> ContentChunk {
        ContentChunk::new(ContentBlock::Text(TextContent::new(s)))
    }

    #[test]
    fn accumulates_message_chunks() {
        let mut st = TurnState::new(false);
        st.apply(SessionUpdate::AgentMessageChunk(chunk("Hello, ")));
        st.apply(SessionUpdate::AgentMessageChunk(chunk("world")));
        st.apply(SessionUpdate::AgentThoughtChunk(chunk("secret")));
        let md = st.render_markdown(&TurnPhase::Done(StopReasonView::EndTurn));
        assert_eq!(md, "Hello, world");
        assert!(!md.contains("secret"));
    }

    #[test]
    fn different_message_ids_are_separated_by_blank_line() {
        use agent_client_protocol::schema::v1::MessageId;
        let mut st = TurnState::new(false);
        let c1 = ContentChunk::new(ContentBlock::Text(TextContent::new("我来看一下。"))).message_id(MessageId::new("m1"));
        let c2 = ContentChunk::new(ContentBlock::Text(TextContent::new("这是一个 Rust 项目。"))).message_id(MessageId::new("m2"));
        st.apply(SessionUpdate::AgentMessageChunk(c1));
        st.apply(SessionUpdate::AgentMessageChunk(c2));
        assert_eq!(st.text, "我来看一下。\n\n这是一个 Rust 项目。");
    }

    #[test]
    fn thoughts_shown_when_enabled() {
        let mut st = TurnState::new(true);
        st.apply(SessionUpdate::AgentThoughtChunk(chunk("secret")));
        let md = st.render_markdown(&TurnPhase::Done(StopReasonView::EndTurn));
        assert!(md.contains("思考过程"));
        assert!(md.contains("secret"));
    }

    #[test]
    fn tool_call_lifecycle_and_diff() {
        let mut st = TurnState::new(false);
        let tc = ToolCall::new(ToolCallId::new("t1"), "修改 src/main.rs").kind(ToolKind::Edit);
        st.apply(SessionUpdate::ToolCall(tc));
        let md = st.render_markdown(&TurnPhase::Running);
        assert!(md.contains("⏳ [edit] 修改 src/main.rs"));

        let fields = ToolCallUpdateFields::new()
            .status(ToolCallStatus::Completed)
            .content(vec![ToolCallContent::Diff(Diff::new("/x/src/main.rs", "a\nb\nc").old_text("a\nb"))]);
        st.apply(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(ToolCallId::new("t1"), fields)));
        let md = st.render_markdown(&TurnPhase::Done(StopReasonView::EndTurn));
        assert!(md.contains("✅ [edit] 修改 src/main.rs"));
        assert!(md.contains("```diff"));
        assert!(md.contains("+ c"));
        assert_eq!(st.tool_call_count(), 1);
    }

    #[test]
    fn plan_renders_checklist() {
        let mut st = TurnState::new(false);
        st.apply(SessionUpdate::Plan(Plan::new(vec![
            PlanEntry::new("写测试", PlanEntryPriority::High, PlanEntryStatus::Completed),
            PlanEntry::new("修 bug", PlanEntryPriority::High, PlanEntryStatus::InProgress),
            PlanEntry::new("发版", PlanEntryPriority::Low, PlanEntryStatus::Pending),
        ])));
        let md = st.render_markdown(&TurnPhase::Running);
        assert!(md.contains("✅ 写测试"));
        assert!(md.contains("⏳ 修 bug"));
        assert!(md.contains("⬜ 发版"));
    }

    #[test]
    fn stop_reasons_explained() {
        let st = TurnState::new(false);
        assert!(st.render_markdown(&TurnPhase::Done(StopReasonView::Cancelled)).contains("已取消"));
        assert!(st.render_markdown(&TurnPhase::Done(StopReasonView::MaxTokens)).contains("token"));
        assert!(st.render_markdown(&TurnPhase::Done(StopReasonView::Refusal)).contains("拒绝"));
        assert!(st.render_markdown(&TurnPhase::Done(StopReasonView::Timeout)).contains("超时"));
        assert_eq!(st.header(&TurnPhase::Failed).1, "red");
    }

    #[test]
    fn unified_diff_marks_changes_only() {
        assert_eq!(unified_diff("a\nb\nc", "a\nx\nc"), "- b\n+ x");
        assert_eq!(unified_diff("", "new"), "+ new");
    }
}
