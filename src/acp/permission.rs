//! 权限审批中介（FR-PERM-1~4、FR-ACP-8）。
//!
//! ACP 的 `session/request_permission` 回调运行在 agent 连接的分发循环里，不能阻塞；
//! `ask` 模式下把 `Responder` 暂存在这里，等飞书卡片回调或超时再回应。
use std::collections::HashMap;
use std::sync::Mutex;

use agent_client_protocol::Responder;
use agent_client_protocol::schema::v1::{
    PermissionOption, PermissionOptionId, PermissionOptionKind, RequestPermissionOutcome,
    RequestPermissionResponse, SelectedPermissionOutcome, SessionId,
};

use crate::config::PermissionPolicy;

/// 一次待审批的权限请求（发给会话层渲染卡片用）。
#[derive(Debug, Clone)]
pub struct PermissionTicket {
    pub ticket_id: String,
    pub session_id: SessionId,
    pub title: String,
    pub kind: Option<String>,
    pub raw_input: Option<serde_json::Value>,
    pub options: Vec<PermissionOption>,
}

pub struct PendingPermission {
    pub responder: Responder<RequestPermissionResponse>,
    pub session_id: SessionId,
    pub options: Vec<PermissionOption>,
    /// 发起该轮次的用户，只有 TA（或管理员）能点按钮。
    pub requester_open_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveOutcome {
    Resolved,
    NotFound,
    Forbidden,
    InvalidOption,
}

#[derive(Default)]
pub struct PermissionBroker {
    pending: Mutex<HashMap<String, PendingPermission>>,
}

impl PermissionBroker {
    pub fn register(&self, ticket_id: String, pending: PendingPermission) {
        self.pending.lock().unwrap().insert(ticket_id, pending);
    }

    pub fn pending_count(&self) -> usize {
        self.pending.lock().unwrap().len()
    }

    /// 用户点击了按钮。`is_admin` 由调用方根据配置判断。
    pub fn resolve(
        &self,
        ticket_id: &str,
        option_id: &str,
        clicker_open_id: &str,
        is_admin: bool,
    ) -> ResolveOutcome {
        let mut map = self.pending.lock().unwrap();
        let Some(p) = map.get(ticket_id) else {
            return ResolveOutcome::NotFound;
        };
        if p.requester_open_id != clicker_open_id && !is_admin {
            return ResolveOutcome::Forbidden;
        }
        if !p.options.iter().any(|o| o.option_id.0.as_ref() == option_id) {
            return ResolveOutcome::InvalidOption;
        }
        let p = map.remove(ticket_id).expect("checked above");
        let _ = p.responder.respond(selected(option_id));
        ResolveOutcome::Resolved
    }

    /// 审批超时：按 reject_once 回应（FR-PERM-3）。返回 true 表示确实有待处理项被超时掉。
    pub fn timeout(&self, ticket_id: &str) -> bool {
        let Some(p) = self.pending.lock().unwrap().remove(ticket_id) else {
            return false;
        };
        let resp = match choose_reject(&p.options) {
            Some(id) => selected(id.0.as_ref()),
            None => RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled),
        };
        let _ = p.responder.respond(resp);
        true
    }

    /// `session/cancel` 后对该会话所有挂起请求回 `cancelled`（FR-ACP-8）。
    pub fn cancel_session(&self, session_id: &SessionId) -> usize {
        let mut map = self.pending.lock().unwrap();
        let ids: Vec<String> = map
            .iter()
            .filter(|(_, p)| &p.session_id == session_id)
            .map(|(k, _)| k.clone())
            .collect();
        for id in &ids {
            if let Some(p) = map.remove(id) {
                let _ = p
                    .responder
                    .respond(RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled));
            }
        }
        ids.len()
    }

    /// agent 进程没了，responder 已无意义，直接丢弃。
    pub fn clear(&self) {
        self.pending.lock().unwrap().clear();
    }
}

fn selected(option_id: &str) -> RequestPermissionResponse {
    RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
        SelectedPermissionOutcome::new(PermissionOptionId::new(option_id)),
    ))
}

/// `auto_allow`：优先 allow_always，其次 allow_once；`auto_reject`：优先 reject_once，其次 reject_always。
/// 返回 None 表示没有合适选项，应回 `cancelled`。
pub fn auto_decision(policy: PermissionPolicy, options: &[PermissionOption]) -> Option<&PermissionOptionId> {
    match policy {
        PermissionPolicy::Ask => None,
        PermissionPolicy::AutoAllow => options
            .iter()
            .find(|o| matches!(o.kind, PermissionOptionKind::AllowAlways))
            .or_else(|| options.iter().find(|o| matches!(o.kind, PermissionOptionKind::AllowOnce)))
            .map(|o| &o.option_id),
        PermissionPolicy::AutoReject => choose_reject(options),
    }
}

fn choose_reject(options: &[PermissionOption]) -> Option<&PermissionOptionId> {
    options
        .iter()
        .find(|o| matches!(o.kind, PermissionOptionKind::RejectOnce))
        .or_else(|| options.iter().find(|o| matches!(o.kind, PermissionOptionKind::RejectAlways)))
        .map(|o| &o.option_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> Vec<PermissionOption> {
        vec![
            PermissionOption::new("once", "允许一次", PermissionOptionKind::AllowOnce),
            PermissionOption::new("always", "总是允许", PermissionOptionKind::AllowAlways),
            PermissionOption::new("no", "拒绝", PermissionOptionKind::RejectOnce),
        ]
    }

    #[test]
    fn auto_allow_prefers_always() {
        let o = opts();
        assert_eq!(auto_decision(PermissionPolicy::AutoAllow, &o).unwrap().0.as_ref(), "always");
    }

    #[test]
    fn auto_allow_falls_back_to_once() {
        let o = vec![
            PermissionOption::new("once", "允许一次", PermissionOptionKind::AllowOnce),
            PermissionOption::new("no", "拒绝", PermissionOptionKind::RejectOnce),
        ];
        assert_eq!(auto_decision(PermissionPolicy::AutoAllow, &o).unwrap().0.as_ref(), "once");
    }

    #[test]
    fn auto_reject_picks_reject_once() {
        let o = opts();
        assert_eq!(auto_decision(PermissionPolicy::AutoReject, &o).unwrap().0.as_ref(), "no");
    }

    #[test]
    fn ask_returns_none_and_no_reject_option_returns_none() {
        assert!(auto_decision(PermissionPolicy::Ask, &opts()).is_none());
        let only_allow = vec![PermissionOption::new("once", "允许", PermissionOptionKind::AllowOnce)];
        assert!(auto_decision(PermissionPolicy::AutoReject, &only_allow).is_none());
    }

    #[test]
    fn resolve_unknown_ticket() {
        let b = PermissionBroker::default();
        assert_eq!(b.resolve("nope", "once", "ou_x", false), ResolveOutcome::NotFound);
        assert!(!b.timeout("nope"));
    }
}
