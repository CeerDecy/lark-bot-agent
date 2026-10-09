//! 会话管理层（FR-SESSION-*、FR-IM-6~8、FR-RENDER-1~3/7、FR-PERM-2~4、FR-OPS-1~2）。
//!
//! 以飞书 `chat_id` 为 key。同一 chat 内串行（队列），不同 chat 并发。
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agent_client_protocol::schema::v1::{ContentBlock, ImageContent, SessionId, StopReason, TextContent};
use base64::Engine;
use larksuite_oapi_sdk_rs::events::im::P2MessageReceiveV1;
use tracing::{debug, error, info, warn};

use crate::acp::permission::{PermissionTicket, ResolveOutcome};
use crate::acp::{AcpClient, AcpError, AgentStatus, SessionEvent};
use crate::commands::{self, Command};
use crate::config::Config;
use crate::lark::LarkApi;
use crate::lark::text::{self, Inbound};
use crate::render::card::{message_card, permission_card, permission_result_card};
use crate::render::split::split_markdown;
use crate::render::{StopReasonView, TurnPhase, TurnState};

const SEEN_LIMIT: usize = 2000;

/// 一条待处理的用户消息。
#[derive(Debug, Clone)]
struct Job {
    message_id: String,
    chat_id: String,
    sender_open_id: String,
    in_group: bool,
    inbound: Inbound,
}

#[derive(Default)]
struct ChatState {
    session_id: Option<SessionId>,
    /// 下一个新会话用的 cwd；None 表示用全局默认。
    next_cwd: Option<PathBuf>,
    /// 当前会话实际使用的 cwd。
    session_cwd: Option<PathBuf>,
    /// 当前会话的 agent 侧模式（session/new 返回或 set_mode 之后）。
    session_mode: Option<String>,
    session_generation: u64,
    queue: VecDeque<Job>,
    worker_running: bool,
    running_turn: bool,
    last_active: Option<Instant>,
    last_usage: Option<(u64, u64)>,
}

struct PermCard {
    message_id: String,
    title: String,
    chat_id: String,
}

struct Inner {
    cfg: Config,
    acp: AcpClient,
    lark: LarkApi,
    bot_open_id: Option<String>,
    chats: Mutex<HashMap<String, ChatState>>,
    seen: Mutex<(VecDeque<String>, HashSet<String>)>,
    perm_cards: Mutex<HashMap<String, PermCard>>,
}

#[derive(Clone)]
pub struct SessionManager {
    inner: Arc<Inner>,
}

impl SessionManager {
    pub fn new(cfg: Config, acp: AcpClient, lark: LarkApi, bot_open_id: Option<String>) -> Self {
        Self {
            inner: Arc::new(Inner {
                cfg,
                acp,
                lark,
                bot_open_id,
                chats: Mutex::new(HashMap::new()),
                seen: Mutex::new((VecDeque::new(), HashSet::new())),
                perm_cards: Mutex::new(HashMap::new()),
            }),
        }
    }

    pub fn config(&self) -> &Config {
        &self.inner.cfg
    }

    /// 飞书事件入口（已在回调里 ack，这里可以慢慢处理）。
    pub async fn handle_event(&self, event: P2MessageReceiveV1) {
        let message = &event.message;
        let Some(sender) = event.sender.open_id().map(str::to_string) else {
            debug!(message_id = %message.message_id, "无 sender open_id，忽略");
            return;
        };
        if event.sender.sender_type != "user" && !event.sender.sender_type.is_empty() {
            debug!(message_id = %message.message_id, sender_type = %event.sender.sender_type, "非用户消息，忽略");
            return;
        }
        // FR-IM-7 幂等
        if !self.mark_seen(&message.message_id) {
            debug!(message_id = %message.message_id, "重复投递，忽略");
            return;
        }
        let in_group = message.chat_type == "group";
        // FR-IM-3 群聊未 @ 忽略
        if in_group && !text::mentions_bot(message, self.inner.bot_open_id.as_deref()) {
            return;
        }
        // FR-IM-8 白名单
        if !self.inner.cfg.is_allowed(&sender) {
            warn!(message_id = %message.message_id, chat_id = %message.chat_id, "白名单外用户，拒绝");
            self.reply_text(&message.message_id, "抱歉，你不在允许使用列表中。", in_group).await;
            return;
        }

        let inbound = text::extract(message, self.inner.bot_open_id.as_deref());
        info!(
            chat_id = %message.chat_id,
            message_id = %message.message_id,
            message_type = %message.message_type,
            "收到消息"
        );
        debug!(message_id = %message.message_id, content = ?inbound, "消息内容");

        if let Inbound::Text(t) = &inbound {
            if let Some(cmd) = commands::parse(t) {
                self.handle_command(cmd, &message.chat_id, &message.message_id, &sender, in_group).await;
                return;
            }
            if t.trim().is_empty() {
                self.reply_text(&message.message_id, "消息为空，请输入要交给 agent 的内容。", in_group).await;
                return;
            }
        }
        if let Inbound::Unsupported(kind) = &inbound {
            self.reply_text(&message.message_id, &format!("暂不支持 {kind} 类型的消息，请发送文本。"), in_group).await;
            return;
        }

        self.enqueue(Job {
            message_id: message.message_id.clone(),
            chat_id: message.chat_id.clone(),
            sender_open_id: sender,
            in_group,
            inbound,
        })
        .await;
    }

    fn mark_seen(&self, message_id: &str) -> bool {
        let mut g = self.inner.seen.lock().unwrap();
        if g.1.contains(message_id) {
            return false;
        }
        g.0.push_back(message_id.to_string());
        g.1.insert(message_id.to_string());
        while g.0.len() > SEEN_LIMIT {
            if let Some(old) = g.0.pop_front() {
                g.1.remove(&old);
            }
        }
        true
    }

    async fn enqueue(&self, job: Job) {
        enum Enq {
            Full,
            Queued { ahead: Option<usize>, spawn_worker: bool },
        }
        let enq = {
            let mut chats = self.inner.chats.lock().unwrap();
            let st = chats.entry(job.chat_id.clone()).or_default();
            if st.queue.len() >= self.inner.cfg.queue_limit_per_chat {
                Enq::Full
            } else {
                let busy = st.running_turn || !st.queue.is_empty();
                let position = st.queue.len();
                st.queue.push_back(job.clone());
                st.last_active = Some(Instant::now());
                let spawn_worker = !st.worker_running;
                st.worker_running = true;
                Enq::Queued { ahead: if busy { Some(position) } else { None }, spawn_worker }
            }
        };
        let (position, spawn_worker) = match enq {
            Enq::Full => {
                self.reply_text(&job.message_id, &format!("排队已满（上限 {}），请稍后再发。", self.inner.cfg.queue_limit_per_chat), job.in_group).await;
                return;
            }
            Enq::Queued { ahead, spawn_worker } => (ahead, spawn_worker),
        };
        if let Some(ahead) = position {
            // FR-SESSION-2
            self.reply_text(&job.message_id, &format!("已排队，前面还有 {} 条。", ahead + 1), job.in_group).await;
        }
        if spawn_worker {
            let me = self.clone();
            let chat_id = job.chat_id.clone();
            tokio::spawn(async move { me.worker(chat_id).await });
        }
    }

    /// 每个 chat 一个 worker，串行消费队列，队列空了退出。
    async fn worker(self, chat_id: String) {
        loop {
            let job = {
                let mut chats = self.inner.chats.lock().unwrap();
                let st = chats.entry(chat_id.clone()).or_default();
                match st.queue.pop_front() {
                    Some(j) => {
                        st.running_turn = true;
                        j
                    }
                    None => {
                        st.worker_running = false;
                        st.running_turn = false;
                        return;
                    }
                }
            };
            self.run_turn(job).await;
            let mut chats = self.inner.chats.lock().unwrap();
            if let Some(st) = chats.get_mut(&chat_id) {
                st.running_turn = false;
                st.last_active = Some(Instant::now());
            }
        }
    }

    async fn ensure_session(&self, chat_id: &str) -> Result<SessionId, AcpError> {
        let gen_now = self.inner.acp.generation();
        let (existing, cwd) = {
            let mut chats = self.inner.chats.lock().unwrap();
            let st = chats.entry(chat_id.to_string()).or_default();
            if st.session_generation != gen_now {
                // agent 重启过，旧 sessionId 失效（FR-AGENT-8）。
                st.session_id = None;
            }
            let cwd = st.next_cwd.clone().unwrap_or_else(|| self.inner.cfg.agent_cwd.clone());
            (st.session_id.clone(), cwd)
        };
        if let Some(id) = existing {
            return Ok(id);
        }
        let created = self.inner.acp.new_session(&cwd, &self.inner.cfg.mcp_servers).await?;
        let id = created.session_id;
        let mut mode = created.modes.as_ref().map(|m| m.current_mode_id.0.to_string());
        info!(chat_id, session_id = %id.0, cwd = %cwd.display(), mode = ?mode, "新建 ACP 会话");
        // FR-PERM-5：AGENT_MODE 指定了 agent 侧模式，建会话后立刻切换。
        if let Some(want) = &self.inner.cfg.agent_mode {
            match self.inner.acp.set_mode(&id, want, created.modes.as_ref()).await {
                Ok(()) => {
                    info!(chat_id, session_id = %id.0, mode = %want, "已切换 agent 模式");
                    mode = Some(want.clone());
                }
                Err(e) => {
                    warn!(chat_id, session_id = %id.0, error = %e, "切换 agent 模式失败，继续使用默认模式");
                    self.notify_admin(&format!("AGENT_MODE={want} 切换失败：{}", user_message_for(&e))).await;
                }
            }
        }
        let mut chats = self.inner.chats.lock().unwrap();
        let st = chats.entry(chat_id.to_string()).or_default();
        st.session_id = Some(id.clone());
        st.session_cwd = Some(cwd);
        st.session_mode = mode;
        st.session_generation = gen_now;
        Ok(id)
    }

    async fn build_prompt(&self, job: &Job) -> Result<Vec<ContentBlock>, String> {
        match &job.inbound {
            Inbound::Text(t) => Ok(vec![ContentBlock::Text(TextContent::new(t.clone()))]),
            Inbound::Image { image_key } => {
                let supports = self.inner.acp.agent_info().map(|i| i.supports_image()).unwrap_or(false);
                if !supports {
                    return Err("当前 agent 不支持图片。".into());
                }
                let (bytes, mime) = self
                    .inner
                    .lark
                    .download_image(&job.message_id, image_key)
                    .await
                    .map_err(|e| {
                        warn!(error = %e, "下载图片失败");
                        "下载图片失败，请重试。".to_string()
                    })?;
                let data = base64::engine::general_purpose::STANDARD.encode(bytes);
                Ok(vec![
                    ContentBlock::Text(TextContent::new("请看这张图片。")),
                    ContentBlock::Image(ImageContent::new(data, mime)),
                ])
            }
            Inbound::Unsupported(k) => Err(format!("暂不支持 {k} 类型的消息。")),
        }
    }

    /// 完整一轮（需求 8.1）。
    async fn run_turn(&self, job: Job) {
        let started = Instant::now();
        let cfg = &self.inner.cfg;
        let in_group = job.in_group;

        let blocks = match self.build_prompt(&job).await {
            Ok(b) => b,
            Err(msg) => {
                self.reply_text(&job.message_id, &msg, in_group).await;
                return;
            }
        };

        let session_id = match self.ensure_session(&job.chat_id).await {
            Ok(id) => id,
            Err(e) => {
                let msg = user_message_for(&e);
                error!(chat_id = %job.chat_id, message_id = %job.message_id, error = %e, "session/new 失败");
                if matches!(e, AcpError::AuthRequired) {
                    self.notify_admin("agent 要求认证：请先在开发机上完成登录（codex 可设置 CODEX_API_KEY / OPENAI_API_KEY，或 ChatGPT 登录；claude 需要 ANTHROPIC_API_KEY 或 Claude Code 本机登录）。").await;
                }
                self.reply_text(&job.message_id, &msg, in_group).await;
                return;
            }
        };

        // FR-RENDER-3：占位消息。优先卡片，失败退纯文本（此时不做流式更新）。
        let mut state = TurnState::new(cfg.show_thoughts);
        let mut phase = TurnPhase::Running;
        let placeholder_card = message_card(state.header(&phase).0, state.header(&phase).1, &state.render_markdown(&phase));
        let placeholder = match self.inner.lark.reply_card(&job.message_id, &placeholder_card, in_group, Some(format!("ph-{}", job.message_id))).await {
            Ok(id) => Some((id, true)),
            Err(e) => {
                warn!(error = %e, "占位卡片发送失败，退回纯文本");
                match self.inner.lark.reply_text(&job.message_id, "处理中…", in_group, None).await {
                    Ok(id) => Some((id, false)),
                    Err(e2) => {
                        warn!(error = %e2, "占位文本也失败，仅在结束时回复");
                        None
                    }
                }
            }
        };

        let mut events = self.inner.acp.subscribe(&session_id, job.sender_open_id.clone());
        let acp = self.inner.acp.clone();
        let sid = session_id.clone();
        let timeout = cfg.prompt_timeout;
        let mut prompt_fut = Box::pin(async move { acp.prompt(&sid, blocks, timeout).await });

        let mut last_patch = Instant::now();
        let mut chars_at_patch = 0usize;
        let mut dirty = false;
        let mut tick = tokio::time::interval(cfg.stream_update_interval.max(Duration::from_millis(200)));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        let outcome: Result<StopReason, AcpError> = loop {
            tokio::select! {
                res = &mut prompt_fut => break res,
                ev = events.recv() => {
                    match ev {
                        Some(SessionEvent::Update(u)) => {
                            if let agent_client_protocol::schema::v1::SessionUpdate::CurrentModeUpdate(m) = &u {
                                let mut chats = self.inner.chats.lock().unwrap();
                                if let Some(st) = chats.get_mut(&job.chat_id) {
                                    st.session_mode = Some(m.current_mode_id.0.to_string());
                                }
                            }
                            state.apply(u);
                            dirty = true;
                            if phase == TurnPhase::WaitingPermission && self.inner.acp.broker().pending_count() == 0 {
                                phase = TurnPhase::Running;
                            }
                        }
                        Some(SessionEvent::Permission(ticket)) => {
                            phase = TurnPhase::WaitingPermission;
                            dirty = true;
                            self.send_permission_card(&job, ticket).await;
                        }
                        Some(SessionEvent::AgentDown) => break Err(AcpError::AgentDown),
                        None => break Err(AcpError::AgentDown),
                    }
                }
                _ = tick.tick() => {}
            }
            if dirty {
                let now = Instant::now();
                let grown = state.text.chars().count().saturating_sub(chars_at_patch);
                if now.duration_since(last_patch) >= cfg.stream_update_interval || grown >= cfg.stream_update_chars {
                    if let Some((id, true)) = &placeholder {
                        let md = state.render_markdown(&phase);
                        let (title, color) = state.header(&phase);
                        let card = message_card(title, color, &truncate_for_card(&md, cfg.max_message_chars));
                        if let Err(e) = self.inner.lark.patch_card(id, &card).await {
                            debug!(error = %e, "流式更新失败（可能限流），稍后重试");
                        }
                    }
                    last_patch = now;
                    chars_at_patch = state.text.chars().count();
                    dirty = false;
                }
            }
        };

        // stopReason 和最后几条 session/update 是背靠背到达的；prompt 一返回就 break 会把
        // 已在队列里的尾部文本丢掉。这里把队列排空，并再等一个短暂的宽限期。
        self.inner.acp.unsubscribe(&session_id);
        let drained = drain_remaining(&mut events, &mut state, Duration::from_millis(300)).await;
        if drained > 0 {
            debug!(chat_id = %job.chat_id, drained, "stopReason 之后补收了尾部 update");
        }

        let stop_reason_label;
        match &outcome {
            Ok(stop) => {
                stop_reason_label = format!("{stop:?}");
                phase = TurnPhase::Done(StopReasonView::from(*stop));
                if matches!(stop, StopReason::Cancelled) {
                    self.clear_queue(&job.chat_id);
                }
            }
            Err(AcpError::Timeout) => {
                stop_reason_label = "timeout".into();
                phase = TurnPhase::Done(StopReasonView::Timeout);
            }
            Err(e) => {
                stop_reason_label = "error".into();
                state.failure = Some(user_message_for(e));
                phase = TurnPhase::Failed;
                if matches!(e, AcpError::AgentDown) {
                    let mut chats = self.inner.chats.lock().unwrap();
                    if let Some(st) = chats.get_mut(&job.chat_id) {
                        st.session_id = None;
                    }
                }
                if matches!(e, AcpError::AuthRequired) {
                    self.notify_admin("agent 要求认证，请先在开发机上完成登录。").await;
                }
            }
        }

        if let Some(u) = state.usage {
            let mut chats = self.inner.chats.lock().unwrap();
            if let Some(st) = chats.get_mut(&job.chat_id) {
                st.last_usage = Some(u);
            }
        }

        info!(
            chat_id = %job.chat_id,
            message_id = %job.message_id,
            session_id = %session_id.0,
            elapsed_ms = started.elapsed().as_millis() as u64,
            stop_reason = %stop_reason_label,
            tool_calls = state.tool_call_count(),
            "轮次结束"
        );

        self.deliver_final(&job, placeholder, &state, &phase).await;
    }

    /// 结束时写入最终内容；超长则切分成多条（FR-RENDER-7）。
    async fn deliver_final(&self, job: &Job, placeholder: Option<(String, bool)>, state: &TurnState, phase: &TurnPhase) {
        let md = state.render_markdown(phase);
        let (title, color) = state.header(phase);
        let parts = split_markdown(&md, self.inner.cfg.max_message_chars);
        let total = parts.len();
        let mut iter = parts.into_iter().enumerate();

        match placeholder {
            Some((id, true)) => {
                if let Some((_, first)) = iter.next() {
                    let t = if total > 1 { format!("{title} (1/{total})") } else { title.to_string() };
                    if let Err(e) = self.inner.lark.patch_card(&id, &message_card(&t, color, &first)).await {
                        warn!(error = %e, "最终卡片更新失败，改为新消息");
                        self.reply_text(&job.message_id, &first, job.in_group).await;
                    }
                }
            }
            Some((id, false)) => {
                if let Some((_, first)) = iter.next()
                    && let Err(e) = self.inner.lark.update_text(&id, &first).await {
                        warn!(error = %e, "最终文本更新失败，改为新消息");
                        self.reply_text(&job.message_id, &first, job.in_group).await;
                    }
            }
            None => {}
        }
        for (i, part) in iter {
            let t = format!("{title} ({}/{total})", i + 1);
            let card = message_card(&t, color, &part);
            if let Err(e) = self.inner.lark.reply_card(&job.message_id, &card, job.in_group, None).await {
                warn!(error = %e, "分段卡片发送失败，退回文本");
                self.reply_text(&job.message_id, &part, job.in_group).await;
            }
        }
    }

    async fn send_permission_card(&self, job: &Job, ticket: PermissionTicket) {
        let card = permission_card(&ticket, "发起本轮的用户");
        let sent = self.inner.lark.reply_card(&job.message_id, &card, job.in_group, None).await;
        let card_msg_id = match sent {
            Ok(id) => id,
            Err(e) => {
                warn!(error = %e, "审批卡片发送失败，退回文本");
                let opts: Vec<String> = ticket.options.iter().map(|o| format!("{}={}", o.name, o.option_id.0)).collect();
                self.reply_text(&job.message_id, &format!("agent 请求权限：{}\n可选：{}\n（卡片发送失败，无法点击审批，将按超时处理）", ticket.title, opts.join(" / ")), job.in_group).await;
                String::new()
            }
        };
        self.inner.perm_cards.lock().unwrap().insert(
            ticket.ticket_id.clone(),
            PermCard { message_id: card_msg_id, title: ticket.title.clone(), chat_id: job.chat_id.clone() },
        );
        // FR-PERM-3 超时
        let me = self.clone();
        let ticket_id = ticket.ticket_id.clone();
        let timeout = self.inner.cfg.permission_timeout;
        tokio::spawn(async move {
            tokio::time::sleep(timeout).await;
            if me.inner.acp.broker().timeout(&ticket_id) {
                let card = me.inner.perm_cards.lock().unwrap().remove(&ticket_id);
                if let Some(pc) = card {
                    info!(ticket_id, "审批超时，已按拒绝处理");
                    if !pc.message_id.is_empty() {
                        let _ = me.inner.lark.patch_card(&pc.message_id, &permission_result_card(&pc.title, "⏱ 审批超时，已自动拒绝。", "grey")).await;
                    }
                    let _ = me.inner.lark.send_text(&pc.chat_id, "审批超时，已自动拒绝该操作。").await;
                }
            }
        });
    }

    /// 卡片按钮回调（FR-PERM-2/4）。返回给用户的 toast 文案。
    pub async fn resolve_permission(&self, ticket_id: &str, option_id: &str, clicker_open_id: &str) -> (String, bool) {
        let is_admin = self.inner.cfg.is_admin(clicker_open_id);
        match self.inner.acp.broker().resolve(ticket_id, option_id, clicker_open_id, is_admin) {
            ResolveOutcome::Resolved => {
                let card = self.inner.perm_cards.lock().unwrap().remove(ticket_id);
                if let Some(pc) = card
                    && !pc.message_id.is_empty() {
                        let _ = self
                            .inner
                            .lark
                            .patch_card(&pc.message_id, &permission_result_card(&pc.title, &format!("✅ 已选择 `{option_id}`"), "green"))
                            .await;
                    }
                info!(ticket_id, option_id, "审批已提交给 agent");
                ("已提交".into(), true)
            }
            ResolveOutcome::Forbidden => ("只有发起本轮对话的用户或管理员可以审批。".into(), false),
            ResolveOutcome::NotFound => ("该审批已处理或已过期。".into(), false),
            ResolveOutcome::InvalidOption => ("无效的选项。".into(), false),
        }
    }

    async fn handle_command(&self, cmd: Command, chat_id: &str, message_id: &str, sender: &str, in_group: bool) {
        match cmd {
            Command::New => {
                let old = {
                    let mut chats = self.inner.chats.lock().unwrap();
                    let st = chats.entry(chat_id.to_string()).or_default();
                    st.session_id.take()
                };
                if let Some(id) = old {
                    let acp = self.inner.acp.clone();
                    tokio::spawn(async move {
                        if let Err(e) = acp.close_session(&id).await {
                            debug!(error = %e, "session/close 失败（可忽略）");
                        }
                    });
                }
                self.reply_text(message_id, "好的，下一条消息将开启新会话。", in_group).await;
            }
            Command::Cancel => {
                let (session, cleared) = {
                    let mut chats = self.inner.chats.lock().unwrap();
                    let st = chats.entry(chat_id.to_string()).or_default();
                    let n = st.queue.len();
                    st.queue.clear();
                    (if st.running_turn { st.session_id.clone() } else { None }, n)
                };
                let mut msg = String::new();
                match session {
                    Some(id) => match self.inner.acp.cancel(&id) {
                        Ok(()) => msg.push_str("已向 agent 发送取消。"),
                        Err(e) => msg.push_str(&format!("取消失败：{}", user_message_for(&e))),
                    },
                    None => msg.push_str("当前没有进行中的轮次。"),
                }
                if cleared > 0 {
                    msg.push_str(&format!(" 已清空 {cleared} 条排队消息。"));
                }
                self.reply_text(message_id, &msg, in_group).await;
            }
            Command::Status => {
                let text = self.status_text(chat_id);
                self.reply_text(message_id, &text, in_group).await;
            }
            Command::Cwd(path) => {
                let reply = self.set_cwd(chat_id, &path);
                self.reply_text(message_id, &reply, in_group).await;
            }
            Command::Help => self.reply_text(message_id, commands::HELP_TEXT, in_group).await,
            Command::Unknown(c) => {
                self.reply_text(message_id, &format!("未知指令 {c}。\n{}", commands::HELP_TEXT), in_group).await
            }
        }
        let _ = sender;
    }

    fn set_cwd(&self, chat_id: &str, path: &str) -> String {
        let p = PathBuf::from(path.trim());
        if path.trim().is_empty() || !p.is_absolute() {
            return "用法：/cwd <绝对路径>".into();
        }
        if !p.is_dir() {
            return format!("目录不存在：{}", p.display());
        }
        if !self.inner.cfg.cwd_allowed(&p) {
            return format!(
                "拒绝：{} 不在允许的根目录内（{}）。",
                p.display(),
                self.inner.cfg.allowed_cwd_roots.iter().map(|r| r.display().to_string()).collect::<Vec<_>>().join(", ")
            );
        }
        let mut chats = self.inner.chats.lock().unwrap();
        let st = chats.entry(chat_id.to_string()).or_default();
        st.next_cwd = Some(p.clone());
        st.session_id = None;
        format!("已切换工作目录为 {}，下一条消息将新建会话。", p.display())
    }

    fn status_text(&self, chat_id: &str) -> String {
        let info = self.inner.acp.agent_info();
        let chats = self.inner.chats.lock().unwrap();
        let st = chats.get(chat_id);
        let mut lines = Vec::new();
        match &info {
            Some(i) => lines.push(format!("agent：{} {}（{}）", i.name, i.version, self.inner.cfg.agent_kind.as_str())),
            None => lines.push(format!("agent：{}（未就绪 / 正在重启）", self.inner.cfg.agent_kind.as_str())),
        }
        lines.push(format!("Bot 审批策略：{:?}", self.inner.cfg.permission_policy));
        match st.and_then(|s| s.session_mode.as_deref()) {
            Some(m) => lines.push(format!("agent 模式：{m}")),
            None => lines.push(format!(
                "agent 模式：{}",
                self.inner.cfg.agent_mode.as_deref().map(|m| format!("{m}（建会话后生效）")).unwrap_or_else(|| "默认".into())
            )),
        }
        match st.and_then(|s| s.session_id.as_ref()) {
            Some(id) => lines.push(format!("sessionId：{}", id.0)),
            None => lines.push("sessionId：（无，下一条消息新建）".into()),
        }
        let cwd = st
            .and_then(|s| s.session_cwd.clone().or_else(|| s.next_cwd.clone()))
            .unwrap_or_else(|| self.inner.cfg.agent_cwd.clone());
        lines.push(format!("cwd：{}", cwd.display()));
        lines.push(format!("进行中：{}", if st.map(|s| s.running_turn).unwrap_or(false) { "是" } else { "否" }));
        lines.push(format!("排队：{}", st.map(|s| s.queue.len()).unwrap_or(0)));
        lines.push(format!("待审批：{}", self.inner.acp.broker().pending_count()));
        if let Some((used, size)) = st.and_then(|s| s.last_usage) {
            lines.push(format!("上下文用量：{used} / {size} tokens"));
        }
        lines.join("\n")
    }

    fn clear_queue(&self, chat_id: &str) {
        let mut chats = self.inner.chats.lock().unwrap();
        if let Some(st) = chats.get_mut(chat_id) {
            st.queue.clear();
        }
    }

    async fn reply_text(&self, message_id: &str, text: &str, in_group: bool) {
        if let Err(e) = self.inner.lark.reply_text(message_id, text, in_group, None).await {
            error!(message_id, error = %e, "回复失败");
        }
    }

    pub async fn notify_admin(&self, text: &str) {
        if let Some(admin) = &self.inner.cfg.admin_open_id {
            if let Err(e) = self.inner.lark.send_text_to_user(admin, text).await {
                warn!(error = %e, "通知管理员失败");
            }
        } else {
            warn!("{text}（未配置 ADMIN_OPEN_ID）");
        }
    }

    /// 监听 agent 状态：重启成功后通知管理员（需求 8.3）。
    pub async fn watch_agent_status(self) {
        let mut rx = self.inner.acp.subscribe_status();
        loop {
            match rx.recv().await {
                Ok(AgentStatus::Up { generation, restarted, info }) => {
                    if !info.auth_methods.is_empty() {
                        info!(auth_methods = ?info.auth_methods, "agent 声明了认证方式；若 session/new 报 auth_required 会通知管理员");
                    }
                    if restarted {
                        self.notify_admin(&format!("agent 已自动重启并恢复（{} {}，第 {generation} 代）。", info.name, info.version)).await;
                    }
                }
                Ok(AgentStatus::Down { .. }) => {
                    // 在途轮次已由 run_turn 收到 AgentDown 事件并回复用户。
                    let mut chats = self.inner.chats.lock().unwrap();
                    for st in chats.values_mut() {
                        st.session_id = None;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
        }
    }

    /// 空闲会话回收（FR-SESSION-8）。
    pub async fn reap_idle(self) {
        let idle = self.inner.cfg.session_idle;
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        loop {
            tick.tick().await;
            let mut to_close = Vec::new();
            {
                let mut chats = self.inner.chats.lock().unwrap();
                for (chat_id, st) in chats.iter_mut() {
                    let is_idle = st.last_active.map(|t| t.elapsed() >= idle).unwrap_or(false);
                    if is_idle && !st.running_turn && st.queue.is_empty()
                        && let Some(id) = st.session_id.take() {
                            to_close.push((chat_id.clone(), id));
                        }
                }
            }
            for (chat_id, id) in to_close {
                info!(chat_id, session_id = %id.0, "会话空闲超时，已清理映射");
                if let Err(e) = self.inner.acp.close_session(&id).await {
                    debug!(error = %e, "session/close 失败（可忽略）");
                }
            }
        }
    }
}

/// 排空事件队列里剩余的 update。返回处理条数。
async fn drain_remaining(
    events: &mut tokio::sync::mpsc::UnboundedReceiver<SessionEvent>,
    state: &mut TurnState,
    grace: Duration,
) -> usize {
    let mut n = 0;
    // 先把已经排队的全部取出。
    while let Ok(ev) = events.try_recv() {
        if let SessionEvent::Update(u) = ev {
            state.apply(u);
            n += 1;
        }
    }
    // 再给仍在飞的通知一个宽限期；unsubscribe 后 sender 会被 drop，recv 返回 None 即结束。
    let deadline = tokio::time::sleep(grace);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => break,
            ev = events.recv() => match ev {
                Some(SessionEvent::Update(u)) => { state.apply(u); n += 1; }
                Some(_) => {}
                None => break,
            },
        }
    }
    n
}

fn truncate_for_card(md: &str, max: usize) -> String {
    if md.chars().count() <= max {
        md.to_string()
    } else {
        let head: String = md.chars().take(max.saturating_sub(16)).collect();
        format!("{head}\n…（内容较长，结束后分段发送）")
    }
}

/// FR-OPS-1：面向用户的可读中文错误。
pub fn user_message_for(e: &AcpError) -> String {
    match e {
        AcpError::AgentDown => "agent 尚未就绪或已退出（首次启动需要下载依赖，可能要等一会），请稍后重试。".into(),
        AcpError::AuthRequired => "agent 需要先在开发机上完成登录认证，已通知管理员。".into(),
        AcpError::Timeout => "等待 agent 超时，已发送取消。".into(),
        AcpError::Rpc(msg) => format!("agent 返回错误：{msg}"),
        AcpError::Capability(msg) => format!("配置或能力问题：{msg}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_messages_are_chinese_and_stackless() {
        for e in [AcpError::AgentDown, AcpError::AuthRequired, AcpError::Timeout, AcpError::Rpc("x".into())] {
            let m = user_message_for(&e);
            assert!(!m.contains("panicked"));
            assert!(m.chars().any(|c| c as u32 > 0x4E00));
        }
    }

    #[test]
    fn truncate_for_card_marks_truncation() {
        let s = "a".repeat(100);
        assert_eq!(truncate_for_card(&s, 200), s);
        assert!(truncate_for_card(&s, 50).contains("内容较长"));
    }
}
