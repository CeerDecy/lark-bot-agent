//! 用假 agent 验证 ACP 客户端层：initialize、session/new、prompt 流、权限审批、取消、崩溃重启。
use std::path::Path;
use std::time::Duration;

use agent_client_protocol::schema::v1::{ContentBlock, StopReason, TextContent};
use lark_bot_agent::acp::permission::ResolveOutcome;
use lark_bot_agent::acp::{AcpClient, AcpError, AgentStatus, SessionEvent};
use lark_bot_agent::agent::AgentLaunch;
use lark_bot_agent::config::PermissionPolicy;
use lark_bot_agent::render::{TurnPhase, TurnState, StopReasonView};
use tokio::sync::broadcast;

fn launch() -> AgentLaunch {
    AgentLaunch { command: env!("CARGO_BIN_EXE_fake_agent").to_string(), args: vec![] }
}

async fn wait_up(rx: &mut broadcast::Receiver<AgentStatus>) -> (bool, u64) {
    loop {
        match tokio::time::timeout(Duration::from_secs(30), rx.recv()).await.expect("agent 未在 30s 内就绪") {
            Ok(AgentStatus::Up { restarted, generation, .. }) => return (restarted, generation),
            Ok(AgentStatus::Down { .. }) => continue,
            Err(e) => panic!("status channel: {e}"),
        }
    }
}

fn text(s: &str) -> Vec<ContentBlock> {
    vec![ContentBlock::Text(TextContent::new(s))]
}

#[tokio::test]
async fn hello_turn_streams_updates_and_ends() {
    let acp = AcpClient::new(launch(), PermissionPolicy::AutoAllow, None);
    let mut status = acp.subscribe_status();
    tokio::spawn(acp.clone().run());
    let (restarted, _) = wait_up(&mut status).await;
    assert!(!restarted);
    let info = acp.agent_info().unwrap();
    assert_eq!(info.name, "fake-agent");
    assert!(info.supports_image());

    let sid = acp.new_session(Path::new("/tmp"), &[]).await.unwrap().session_id;
    let mut events = acp.subscribe(&sid, "ou_user".into());
    let stop = acp.prompt(&sid, text("hello"), Duration::from_secs(10)).await.unwrap();
    assert_eq!(stop, StopReason::EndTurn);

    let mut state = TurnState::new(true);
    while let Ok(ev) = events.try_recv() {
        if let SessionEvent::Update(u) = ev {
            state.apply(u);
        }
    }
    let md = state.render_markdown(&TurnPhase::Done(StopReasonView::EndTurn));
    assert!(md.starts_with("Hi there"), "{md}");
    assert!(md.contains("✅ [read] 读取 README.md"), "{md}");
    assert!(md.contains("thinking"));
    assert_eq!(state.tool_call_count(), 1);
}

#[tokio::test]
async fn auto_allow_policy_answers_permission_without_user() {
    let acp = AcpClient::new(launch(), PermissionPolicy::AutoAllow, None);
    let mut status = acp.subscribe_status();
    tokio::spawn(acp.clone().run());
    wait_up(&mut status).await;
    let sid = acp.new_session(Path::new("/tmp"), &[]).await.unwrap().session_id;
    let mut events = acp.subscribe(&sid, "ou_user".into());
    let stop = acp.prompt(&sid, text("perm"), Duration::from_secs(10)).await.unwrap();
    assert_eq!(stop, StopReason::EndTurn);
    let mut state = TurnState::new(false);
    while let Ok(SessionEvent::Update(u)) = events.try_recv() {
        state.apply(u);
    }
    // allow_always 优先
    assert!(state.text.contains("approved:always"), "{}", state.text);
}

#[tokio::test]
async fn ask_policy_routes_ticket_and_enforces_requester() {
    let acp = AcpClient::new(launch(), PermissionPolicy::Ask, None);
    let mut status = acp.subscribe_status();
    tokio::spawn(acp.clone().run());
    wait_up(&mut status).await;
    let sid = acp.new_session(Path::new("/tmp"), &[]).await.unwrap().session_id;
    let mut events = acp.subscribe(&sid, "ou_requester".into());

    let acp2 = acp.clone();
    let sid2 = sid.clone();
    let prompt = tokio::spawn(async move { acp2.prompt(&sid2, text("perm"), Duration::from_secs(10)).await });

    let ticket = loop {
        match tokio::time::timeout(Duration::from_secs(5), events.recv()).await.unwrap().unwrap() {
            SessionEvent::Permission(t) => break t,
            SessionEvent::Update(_) => continue,
            SessionEvent::AgentDown => panic!("agent down"),
        }
    };
    assert_eq!(ticket.title, "运行 rm -rf build");
    assert_eq!(ticket.kind.as_deref(), Some("execute"));
    assert_eq!(ticket.options.len(), 3);
    assert_eq!(acp.broker().pending_count(), 1);

    // 其他人点击：无权限
    assert_eq!(acp.broker().resolve(&ticket.ticket_id, "allow", "ou_other", false), ResolveOutcome::Forbidden);
    // 非法选项
    assert_eq!(acp.broker().resolve(&ticket.ticket_id, "nope", "ou_requester", false), ResolveOutcome::InvalidOption);
    // 管理员代为审批
    assert_eq!(acp.broker().resolve(&ticket.ticket_id, "allow", "ou_admin", true), ResolveOutcome::Resolved);
    assert_eq!(acp.broker().pending_count(), 0);

    let stop = prompt.await.unwrap().unwrap();
    assert_eq!(stop, StopReason::EndTurn);
    let mut state = TurnState::new(false);
    while let Ok(SessionEvent::Update(u)) = events.try_recv() {
        state.apply(u);
    }
    assert!(state.text.contains("approved:allow"), "{}", state.text);
}

#[tokio::test]
async fn cancel_sends_session_cancel_and_returns_cancelled() {
    let acp = AcpClient::new(launch(), PermissionPolicy::Ask, None);
    let mut status = acp.subscribe_status();
    tokio::spawn(acp.clone().run());
    wait_up(&mut status).await;
    let sid = acp.new_session(Path::new("/tmp"), &[]).await.unwrap().session_id;
    let mut events = acp.subscribe(&sid, "ou_user".into());

    let acp2 = acp.clone();
    let sid2 = sid.clone();
    let prompt = tokio::spawn(async move { acp2.prompt(&sid2, text("slow"), Duration::from_secs(10)).await });
    // 等到第一段输出再取消
    match tokio::time::timeout(Duration::from_secs(5), events.recv()).await.unwrap().unwrap() {
        SessionEvent::Update(_) => {}
        other => panic!("unexpected {other:?}"),
    }
    acp.cancel(&sid).unwrap();
    assert_eq!(prompt.await.unwrap().unwrap(), StopReason::Cancelled);
}

#[tokio::test]
async fn prompt_timeout_cancels_and_reports() {
    let acp = AcpClient::new(launch(), PermissionPolicy::Ask, None);
    let mut status = acp.subscribe_status();
    tokio::spawn(acp.clone().run());
    wait_up(&mut status).await;
    let sid = acp.new_session(Path::new("/tmp"), &[]).await.unwrap().session_id;
    let _events = acp.subscribe(&sid, "ou_user".into());
    let res = acp.prompt(&sid, text("slow"), Duration::from_millis(300)).await;
    assert!(matches!(res, Err(AcpError::Timeout)), "{res:?}");
}

#[tokio::test]
async fn crash_is_reported_and_agent_restarts() {
    let acp = AcpClient::new(launch(), PermissionPolicy::AutoAllow, None);
    let mut status = acp.subscribe_status();
    tokio::spawn(acp.clone().run());
    let (_, gen1) = wait_up(&mut status).await;
    let sid = acp.new_session(Path::new("/tmp"), &[]).await.unwrap().session_id;
    let mut events = acp.subscribe(&sid, "ou_user".into());

    let res = acp.prompt(&sid, text("crash"), Duration::from_secs(10)).await;
    // 子进程退出时，在途请求通常以 incoming_transport_closed 失败（→ AgentDown）；
    // 极少数情况下 SDK 会先以别的内部错误失败，这里只要求是 Err 且随后能看到 Down/重启。
    assert!(matches!(res, Err(AcpError::AgentDown) | Err(AcpError::Rpc(_))), "{res:?}");
    // 订阅者也会收到 AgentDown
    let ev = tokio::time::timeout(Duration::from_secs(5), events.recv()).await.unwrap();
    assert!(matches!(ev, Some(SessionEvent::AgentDown) | None), "{ev:?}");

    // 指数退避 1s 后重启
    let (restarted, gen2) = wait_up(&mut status).await;
    assert!(restarted);
    assert_eq!(gen2, gen1 + 1);
    assert!(acp.is_up());
    // 新进程可以正常建会话
    let sid2 = acp.new_session(Path::new("/tmp"), &[]).await.unwrap().session_id;
    assert!(!sid2.0.is_empty());
    // 新进程的会话必须能正常跑完一轮
    let _events = acp.subscribe(&sid2, "ou_user".into());
    assert_eq!(acp.prompt(&sid2, text("hello"), Duration::from_secs(10)).await.unwrap(), StopReason::EndTurn);
}

#[tokio::test]
async fn new_session_waits_for_agent_to_come_up() {
    let acp = AcpClient::new(launch(), PermissionPolicy::AutoAllow, None);
    tokio::spawn(acp.clone().run());
    // 不等 Up 事件，直接建会话：应阻塞到 initialize 完成而不是报 AgentDown。
    let sid = acp.new_session(Path::new("/tmp"), &[]).await.unwrap().session_id;
    assert!(!sid.0.is_empty());
    assert!(acp.is_up());
}

#[tokio::test]
async fn set_mode_bypass_skips_permission_requests() {
    let acp = AcpClient::new(launch(), PermissionPolicy::Ask, None);
    tokio::spawn(acp.clone().run());
    let created = acp.new_session(Path::new("/tmp"), &[]).await.unwrap();
    let modes = created.modes.clone().expect("fake agent advertises modes");
    assert_eq!(modes.current_mode_id.0.as_ref(), "default");

    // 不存在的模式：本地校验直接拒绝，不发请求
    let err = acp.set_mode(&created.session_id, "yolo", Some(&modes)).await.unwrap_err();
    assert!(matches!(err, AcpError::Capability(ref m) if m.contains("bypassPermissions")), "{err:?}");

    acp.set_mode(&created.session_id, "bypassPermissions", Some(&modes)).await.unwrap();
    let mut events = acp.subscribe(&created.session_id, "ou_user".into());
    // ask 策略下本应弹审批；agent 侧 bypass 后根本不会来问。
    let stop = acp.prompt(&created.session_id, text("perm"), Duration::from_secs(10)).await.unwrap();
    assert_eq!(stop, StopReason::EndTurn);
    assert_eq!(acp.broker().pending_count(), 0);
    let mut state = TurnState::new(false);
    while let Ok(SessionEvent::Update(u)) = events.try_recv() {
        state.apply(u);
    }
    assert!(state.text.contains("bypassed:executed"), "{}", state.text);
}

/// 复现 run_turn 的读法：select 到 stopReason 就停，然后排空队列。stopReason 之后到达的尾部文本不能丢。
#[tokio::test]
async fn trailing_text_after_stop_reason_is_not_lost() {
    let acp = AcpClient::new(launch(), PermissionPolicy::AutoAllow, None);
    tokio::spawn(acp.clone().run());
    let sid = acp.new_session(Path::new("/tmp"), &[]).await.unwrap().session_id;
    let mut events = acp.subscribe(&sid, "ou_user".into());
    let acp2 = acp.clone();
    let sid2 = sid.clone();
    let mut prompt = Box::pin(async move { acp2.prompt(&sid2, text("summary"), Duration::from_secs(10)).await });

    let mut state = TurnState::new(false);
    let mut seen_before_stop = 0;
    let stop = loop {
        tokio::select! {
            res = &mut prompt => break res.unwrap(),
            ev = events.recv() => {
                if let Some(SessionEvent::Update(u)) = ev { state.apply(u); seen_before_stop += 1; }
            }
        }
    };
    assert_eq!(stop, StopReason::EndTurn);
    // 排空（和 session::drain_remaining 同样的做法）
    acp.unsubscribe(&sid);
    let deadline = tokio::time::sleep(Duration::from_millis(300));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => break,
            ev = events.recv() => match ev {
                Some(SessionEvent::Update(u)) => state.apply(u),
                Some(_) => {}
                None => break,
            },
        }
    }
    let md = state.render_markdown(&TurnPhase::Done(StopReasonView::EndTurn));
    assert!(md.contains("这是一个 Rust 项目。"), "尾部文本丢失 (before_stop={seen_before_stop}):\n{md}");
    assert!(md.contains("我来看一下项目结构。\n\n这是一个 Rust 项目。"), "不同 messageId 应分段:\n{md}");
    assert!(md.contains("✅ [read] Read README.md"), "{md}");
}

