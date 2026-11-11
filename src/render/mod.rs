//! ACP update → 飞书 Markdown / 卡片（FR-RENDER-*）。
//!
//! 只把 agent 的最终回复文本展示给用户：思考过程、工具调用、任务清单都不渲染。
pub mod card;
pub mod split;

use agent_client_protocol::schema::v1::{ContentBlock, SessionUpdate, StopReason, UsageUpdate};

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

/// 一轮对话里累积的状态（FR-RENDER-1）。对外只暴露回复正文与失败原因。
#[derive(Debug, Default, Clone)]
pub struct TurnState {
    pub text: String,
    /// 上一条 agent_message_chunk 的 messageId；变化时在正文里插入空行分段。
    last_message_id: Option<String>,
    /// 本轮出现过的工具调用 id，只用于结束日志里的计数。
    tools: Vec<String>,
    pub usage: Option<(u64, u64)>,
    pub failure: Option<String>,
}

impl TurnState {
    pub fn new() -> Self {
        Self::default()
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
            SessionUpdate::ToolCall(tc) => self.note_tool(tc.tool_call_id.0.to_string()),
            SessionUpdate::ToolCallUpdate(u) => self.note_tool(u.tool_call_id.0.to_string()),
            SessionUpdate::UsageUpdate(UsageUpdate { used, size, .. }) => self.usage = Some((used, size)),
            _ => {}
        }
    }

    fn note_tool(&mut self, id: String) {
        if !self.tools.contains(&id) {
            self.tools.push(id);
        }
    }

    /// 渲染回复正文。只包含 agent 的文本与本轮状态提示（处理中/等待审批/取消/失败等）。
    pub fn render_markdown(&self, phase: &TurnPhase) -> String {
        let body = self.text.trim();
        let status = match phase {
            TurnPhase::Running => body.is_empty().then_some("处理中…"),
            TurnPhase::WaitingPermission => Some("⏸ 等待审批…"),
            TurnPhase::Done(StopReasonView::EndTurn) => body.is_empty().then_some("（agent 没有返回文本）"),
            TurnPhase::Done(StopReasonView::Cancelled) => Some("已取消。"),
            TurnPhase::Done(StopReasonView::MaxTokens) => Some("⚠️ 输出达到 token 上限，回复可能不完整。"),
            TurnPhase::Done(StopReasonView::MaxTurnRequests) => Some("⚠️ 达到单轮最大模型调用次数，agent 已停止。"),
            TurnPhase::Done(StopReasonView::Refusal) => Some("⚠️ agent 拒绝了这个请求。"),
            TurnPhase::Done(StopReasonView::Timeout) => Some("⚠️ 本轮超时，已向 agent 发送取消。"),
            TurnPhase::Failed => Some(self.failure.as_deref().unwrap_or("处理失败，请稍后重试。")),
        };

        let mut out = String::new();
        if !body.is_empty() {
            out.push_str(body);
        }
        if let Some(s) = status {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            if matches!(phase, TurnPhase::Failed) {
                out.push_str("❌ ");
            }
            out.push_str(s);
        }
        out
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

    /// 结束回复用的卡片标题。正常完成不带“完成”之类的状态标题，异常结束仍显示提示。
    pub fn final_header(&self, phase: &TurnPhase) -> Option<(&'static str, &'static str)> {
        match phase {
            TurnPhase::Done(StopReasonView::EndTurn) => None,
            _ => Some(self.header(phase)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{
        ContentChunk, Plan, PlanEntry, PlanEntryPriority, PlanEntryStatus, TextContent, ToolCall,
        ToolCallId, ToolCallUpdate, ToolCallUpdateFields,
    };

    fn chunk(s: &str) -> ContentChunk {
        ContentChunk::new(ContentBlock::Text(TextContent::new(s)))
    }

    #[test]
    fn accumulates_message_chunks_only() {
        let mut st = TurnState::new();
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
        let mut st = TurnState::new();
        let c1 = ContentChunk::new(ContentBlock::Text(TextContent::new("我来看一下。"))).message_id(MessageId::new("m1"));
        let c2 = ContentChunk::new(ContentBlock::Text(TextContent::new("这是一个 Rust 项目。"))).message_id(MessageId::new("m2"));
        st.apply(SessionUpdate::AgentMessageChunk(c1));
        st.apply(SessionUpdate::AgentMessageChunk(c2));
        assert_eq!(st.text, "我来看一下。\n\n这是一个 Rust 项目。");
    }

    #[test]
    fn tool_calls_are_counted_but_not_rendered() {
        let mut st = TurnState::new();
        st.apply(SessionUpdate::ToolCall(ToolCall::new(ToolCallId::new("t1"), "修改 src/main.rs")));
        st.apply(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            ToolCallId::new("t1"),
            ToolCallUpdateFields::new(),
        )));
        assert_eq!(st.tool_call_count(), 1);
        let md = st.render_markdown(&TurnPhase::Running);
        assert_eq!(md, "处理中…");
        let md = st.render_markdown(&TurnPhase::Done(StopReasonView::EndTurn));
        assert!(!md.contains("修改 src/main.rs"));
        assert!(md.contains("（agent 没有返回文本）"));
    }

    #[test]
    fn plan_is_not_rendered() {
        let mut st = TurnState::new();
        st.apply(SessionUpdate::Plan(Plan::new(vec![PlanEntry::new(
            "写测试",
            PlanEntryPriority::High,
            PlanEntryStatus::InProgress,
        )])));
        let md = st.render_markdown(&TurnPhase::Running);
        assert_eq!(md, "处理中…");
        assert!(!md.contains("写测试"));
    }

    #[test]
    fn stop_reasons_explained() {
        let st = TurnState::new();
        assert!(st.render_markdown(&TurnPhase::Done(StopReasonView::Cancelled)).contains("已取消"));
        assert!(st.render_markdown(&TurnPhase::Done(StopReasonView::MaxTokens)).contains("token"));
        assert!(st.render_markdown(&TurnPhase::Done(StopReasonView::Refusal)).contains("拒绝"));
        assert!(st.render_markdown(&TurnPhase::Done(StopReasonView::Timeout)).contains("超时"));
        assert_eq!(st.header(&TurnPhase::Failed).1, "red");
    }

    #[test]
    fn failure_text_is_rendered() {
        let mut st = TurnState::new();
        st.failure = Some("出错了".into());
        let md = st.render_markdown(&TurnPhase::Failed);
        assert_eq!(md, "❌ 出错了");
    }

    #[test]
    fn final_header_omits_done_status() {
        let st = TurnState::new();
        assert_eq!(st.final_header(&TurnPhase::Done(StopReasonView::EndTurn)), None);
        assert_eq!(st.final_header(&TurnPhase::Done(StopReasonView::Cancelled)), Some(("已取消", "grey")));
        assert_eq!(st.final_header(&TurnPhase::Failed), Some(("失败", "red")));
    }
}
