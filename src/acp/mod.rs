//! ACP 客户端层（FR-ACP-*、FR-AGENT-4~8）。
//!
//! 一个 Bot 进程只维护一个 agent 子进程（FR-AGENT-9）。`AcpClient::run` 是常驻的
//! supervisor：拉起子进程 → `initialize` → 等待连接关闭 → 指数退避重启。
//! 其他任务通过 `AcpClient` 的方法发请求；`session/update` 按 sessionId 路由到
//! 订阅者；`session/request_permission` 交给 `PermissionBroker`。
pub mod permission;
pub mod trace;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    AgentCapabilities, CancelNotification, ClientCapabilities, CloseSessionRequest, ContentBlock,
    ErrorCode, FileSystemCapabilities, Implementation, InitializeRequest, McpServer,
    NewSessionRequest, PromptRequest, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, SelectedPermissionOutcome, SessionId, SessionModeId,
    SessionModeState, SessionNotification, SessionUpdate, SetSessionModeRequest, StopReason,
};
use agent_client_protocol::{AcpAgent, AcpAgentConfig, Agent, ConnectionTo, LineDirection};
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, error, info, warn};

use crate::agent::AgentLaunch;
use crate::config::PermissionPolicy;
use permission::{PendingPermission, PermissionBroker, PermissionTicket, auto_decision};
use trace::AcpTrace;

#[derive(Debug, thiserror::Error)]
pub enum AcpError {
    #[error("agent 未就绪或已退出")]
    AgentDown,
    #[error("agent 需要先完成登录认证")]
    AuthRequired,
    #[error("等待 agent 超时")]
    Timeout,
    #[error("agent 返回错误: {0}")]
    Rpc(String),
    #[error("{0}")]
    Capability(String),
}

impl From<agent_client_protocol::Error> for AcpError {
    fn from(e: agent_client_protocol::Error) -> Self {
        if agent_client_protocol::is_incoming_transport_closed(&e) {
            return AcpError::AgentDown;
        }
        if e.code == ErrorCode::AuthRequired {
            return AcpError::AuthRequired;
        }
        AcpError::Rpc(format!("{} ({})", e.message, e.code))
    }
}

/// `initialize` 结果里我们关心的部分。
#[derive(Debug, Clone)]
pub struct AgentInfo {
    pub name: String,
    pub version: String,
    pub capabilities: AgentCapabilities,
    pub auth_methods: Vec<String>,
}

impl AgentInfo {
    pub fn supports_image(&self) -> bool {
        self.capabilities.prompt_capabilities.image
    }
    pub fn supports_close(&self) -> bool {
        self.capabilities.session_capabilities.close.is_some()
    }
}

/// `session/new` 的结果。
#[derive(Debug, Clone)]
pub struct NewSession {
    pub session_id: SessionId,
    pub modes: Option<SessionModeState>,
}

/// agent 子进程状态变化，广播给会话层与管理员通知。
#[derive(Debug, Clone)]
pub enum AgentStatus {
    Up { generation: u64, restarted: bool, info: Arc<AgentInfo> },
    Down { generation: u64 },
}

/// 路由给某个会话订阅者的事件。
#[derive(Debug)]
pub enum SessionEvent {
    Update(SessionUpdate),
    Permission(PermissionTicket),
    /// agent 进程退出；在途轮次应立刻失败。
    AgentDown,
}

struct Route {
    tx: mpsc::UnboundedSender<SessionEvent>,
    requester_open_id: String,
}

struct Live {
    cx: ConnectionTo<Agent>,
    info: Arc<AgentInfo>,
    generation: u64,
}

struct Inner {
    launch: AgentLaunch,
    policy: PermissionPolicy,
    trace_dir: Option<PathBuf>,
    live: Mutex<Option<Live>>,
    routes: Mutex<HashMap<SessionId, Route>>,
    broker: PermissionBroker,
    status_tx: broadcast::Sender<AgentStatus>,
    generation: AtomicU64,
}

#[derive(Clone)]
pub struct AcpClient {
    inner: Arc<Inner>,
}

const INIT_TIMEOUT: Duration = Duration::from_secs(120);
/// `session/new` 时 agent 还没 initialize 完（典型：首次 `npx` 下载）最多等这么久。
pub const AGENT_READY_TIMEOUT: Duration = Duration::from_secs(90);
const NEW_SESSION_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_BACKOFF: Duration = Duration::from_secs(60);

impl AcpClient {
    pub fn new(launch: AgentLaunch, policy: PermissionPolicy, trace_dir: Option<PathBuf>) -> Self {
        let (status_tx, _) = broadcast::channel(16);
        Self {
            inner: Arc::new(Inner {
                launch,
                policy,
                trace_dir,
                live: Mutex::new(None),
                routes: Mutex::new(HashMap::new()),
                broker: PermissionBroker::default(),
                status_tx,
                generation: AtomicU64::new(0),
            }),
        }
    }

    pub fn subscribe_status(&self) -> broadcast::Receiver<AgentStatus> {
        self.inner.status_tx.subscribe()
    }

    pub fn broker(&self) -> &PermissionBroker {
        &self.inner.broker
    }

    pub fn agent_info(&self) -> Option<Arc<AgentInfo>> {
        self.inner.live.lock().unwrap().as_ref().map(|l| l.info.clone())
    }

    pub fn is_up(&self) -> bool {
        self.inner.live.lock().unwrap().is_some()
    }

    pub fn generation(&self) -> u64 {
        self.inner.generation.load(Ordering::SeqCst)
    }

    /// 常驻 supervisor。永不返回（除非 tokio 运行时关闭）。
    pub async fn run(self) {
        let mut backoff = Duration::from_secs(1);
        let mut restarted = false;
        loop {
            let started = Instant::now();
            match self.run_once(restarted).await {
                Ok(()) => info!("agent 连接正常关闭"),
                Err(e) => error!(error = %e, "agent 连接异常结束"),
            }
            self.on_down();
            // 存活超过 2 分钟视为曾经正常工作过，退避重新从 1s 开始。
            if started.elapsed() > Duration::from_secs(120) {
                backoff = Duration::from_secs(1);
            }
            warn!(backoff_secs = backoff.as_secs(), "将重启 agent 子进程");
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(MAX_BACKOFF);
            restarted = true;
        }
    }

    async fn run_once(&self, restarted: bool) -> Result<(), AcpError> {
        let inner = self.inner.clone();
        let mut config = AcpAgentConfig::new(&inner.launch.command);
        config = config.args(inner.launch.args.iter().cloned());
        // 凭证（OPENAI_API_KEY / ANTHROPIC_API_KEY 等）由子进程继承父进程环境，Bot 不读取。
        let trace = match &inner.trace_dir {
            Some(dir) => match AcpTrace::open(dir) {
                Ok(t) => Some(Arc::new(t)),
                Err(e) => {
                    warn!(error = %e, dir = %dir.display(), "无法打开 ACP_TRACE_DIR，跳过落盘");
                    None
                }
            },
            None => None,
        };
        info!(command = %inner.launch.display(), "拉起 agent 子进程");
        let agent = AcpAgent::new(config).with_debug(move |line: &str, dir: LineDirection| {
            if let LineDirection::Stderr = dir {
                debug!(target: "agent", "{line}");
            }
            if let Some(t) = &trace {
                t.record(line, dir);
            }
        });

        let notif_inner = inner.clone();
        let perm_inner = inner.clone();
        let main_inner = inner.clone();

        agent_client_protocol::Client
            .builder()
            .name("lark-bot-agent")
            .on_receive_notification(
                move |n: SessionNotification, _cx: ConnectionTo<Agent>| {
                    let inner = notif_inner.clone();
                    async move {
                        inner.route_update(n);
                        Ok(())
                    }
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .on_receive_request(
                move |req: RequestPermissionRequest,
                      responder: agent_client_protocol::Responder<RequestPermissionResponse>,
                      _cx: ConnectionTo<Agent>| {
                    let inner = perm_inner.clone();
                    async move {
                        inner.handle_permission(req, responder);
                        Ok(())
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(agent, |cx: ConnectionTo<Agent>| async move {
                let inner = main_inner;
                let caps = ClientCapabilities::new()
                    .fs(FileSystemCapabilities::new().read_text_file(false).write_text_file(false))
                    .terminal(false);
                let req = InitializeRequest::new(ProtocolVersion::V1)
                    .client_capabilities(caps)
                    .client_info(Implementation::new("lark-bot-agent", env!("CARGO_PKG_VERSION")));
                let init = tokio::time::timeout(INIT_TIMEOUT, cx.send_request(req).block_task())
                    .await
                    .map_err(|_| agent_client_protocol::Error::internal_error().data("initialize 超时"))??;
                if init.protocol_version != ProtocolVersion::V1 {
                    return Err(agent_client_protocol::Error::internal_error()
                        .data(format!("agent 协议版本不匹配: {:?}", init.protocol_version)));
                }
                let (name, version) = init
                    .agent_info
                    .as_ref()
                    .map(|i| (i.name.clone(), i.version.clone()))
                    .unwrap_or_else(|| ("unknown".into(), "unknown".into()));
                let info = Arc::new(AgentInfo {
                    name,
                    version,
                    capabilities: init.agent_capabilities.clone(),
                    auth_methods: init.auth_methods.iter().map(|m| m.id().0.to_string()).collect(),
                });
                let generation = inner.generation.fetch_add(1, Ordering::SeqCst) + 1;
                info!(
                    agent = %info.name,
                    version = %info.version,
                    generation,
                    load_session = info.capabilities.load_session,
                    image = info.supports_image(),
                    auth_methods = ?info.auth_methods,
                    "agent initialize 完成"
                );
                *inner.live.lock().unwrap() = Some(Live { cx: cx.clone(), info: info.clone(), generation });
                let _ = inner.status_tx.send(AgentStatus::Up { generation, restarted, info });
                cx.incoming_closed().await;
                Ok(())
            })
            .await
            .map_err(AcpError::from)
    }

    fn on_down(&self) {
        let inner = &self.inner;
        let generation = inner.live.lock().unwrap().take().map(|l| l.generation).unwrap_or(0);
        inner.broker.clear();
        let routes: Vec<Route> = inner.routes.lock().unwrap().drain().map(|(_, r)| r).collect();
        for r in routes {
            let _ = r.tx.send(SessionEvent::AgentDown);
        }
        let _ = inner.status_tx.send(AgentStatus::Down { generation });
    }

    /// 等待 agent 就绪（initialize 完成）。已就绪时立刻返回。
    pub async fn wait_until_up(&self, timeout: Duration) -> Result<(), AcpError> {
        if self.is_up() {
            return Ok(());
        }
        let mut rx = self.inner.status_tx.subscribe();
        // 订阅后再查一次，避免在订阅前刚好 Up 的竞态。
        if self.is_up() {
            return Ok(());
        }
        let deadline = tokio::time::sleep(timeout);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut deadline => return Err(AcpError::AgentDown),
                ev = rx.recv() => match ev {
                    Ok(AgentStatus::Up { .. }) => return Ok(()),
                    Ok(AgentStatus::Down { .. }) => continue,
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        if self.is_up() { return Ok(()); }
                    }
                    Err(broadcast::error::RecvError::Closed) => return Err(AcpError::AgentDown),
                },
            }
        }
    }

    fn cx(&self) -> Result<(ConnectionTo<Agent>, Arc<AgentInfo>), AcpError> {
        self.inner
            .live
            .lock()
            .unwrap()
            .as_ref()
            .map(|l| (l.cx.clone(), l.info.clone()))
            .ok_or(AcpError::AgentDown)
    }

    /// `session/new`（FR-ACP-5、FR-ACP-10）。返回 sessionId 与 agent 声明的模式信息。
    pub async fn new_session(&self, cwd: &Path, mcp_servers: &[serde_json::Value]) -> Result<NewSession, AcpError> {
        if !self.is_up() {
            info!("agent 尚未就绪，等待 initialize 完成后再建会话");
            self.wait_until_up(AGENT_READY_TIMEOUT).await?;
        }
        let (cx, _) = self.cx()?;
        let mut servers = Vec::with_capacity(mcp_servers.len());
        for v in mcp_servers {
            let s: McpServer = serde_json::from_value(v.clone())
                .map_err(|e| AcpError::Capability(format!("AGENT_MCP_SERVERS_JSON 条目无法解析: {e}")))?;
            servers.push(s);
        }
        let req = NewSessionRequest::new(cwd).mcp_servers(servers);
        let resp = tokio::time::timeout(NEW_SESSION_TIMEOUT, cx.send_request(req).block_task())
            .await
            .map_err(|_| AcpError::Timeout)??;
        Ok(NewSession { session_id: resp.session_id, modes: resp.modes })
    }

    /// `session/set_mode`：切换 agent 侧模式（FR-PERM-5）。`modes` 为 `session/new` 返回的可用模式；
    /// 若 agent 没有声明该模式，返回 Capability 错误并列出可用项。
    pub async fn set_mode(&self, session_id: &SessionId, mode: &str, modes: Option<&SessionModeState>) -> Result<(), AcpError> {
        if let Some(m) = modes {
            if !m.available_modes.iter().any(|am| am.id.0.as_ref() == mode) {
                let avail: Vec<&str> = m.available_modes.iter().map(|am| am.id.0.as_ref()).collect();
                return Err(AcpError::Capability(format!(
                    "agent 不支持模式 {mode:?}，可用：{}",
                    if avail.is_empty() { "（无）".to_string() } else { avail.join(", ") }
                )));
            }
            if m.current_mode_id.0.as_ref() == mode {
                return Ok(());
            }
        }
        let (cx, _) = self.cx()?;
        tokio::time::timeout(
            Duration::from_secs(30),
            cx.send_request(SetSessionModeRequest::new(session_id.clone(), SessionModeId::new(mode))).block_task(),
        )
        .await
        .map_err(|_| AcpError::Timeout)??;
        Ok(())
    }

    /// 订阅某个会话的事件流。重复订阅会替换旧的 sender。
    pub fn subscribe(&self, session_id: &SessionId, requester_open_id: String) -> mpsc::UnboundedReceiver<SessionEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.inner
            .routes
            .lock()
            .unwrap()
            .insert(session_id.clone(), Route { tx, requester_open_id });
        rx
    }

    pub fn unsubscribe(&self, session_id: &SessionId) {
        self.inner.routes.lock().unwrap().remove(session_id);
    }

    /// `session/prompt`，等待 stopReason（FR-ACP-6/7）。超时后发送 `session/cancel` 并返回 Timeout。
    pub async fn prompt(&self, session_id: &SessionId, blocks: Vec<ContentBlock>, timeout: Duration) -> Result<StopReason, AcpError> {
        let (cx, _) = self.cx()?;
        let req = PromptRequest::new(session_id.clone(), blocks);
        match tokio::time::timeout(timeout, cx.send_request(req).block_task()).await {
            Ok(Ok(resp)) => Ok(resp.stop_reason),
            Ok(Err(e)) => Err(e.into()),
            Err(_) => {
                let _ = self.cancel(session_id);
                Err(AcpError::Timeout)
            }
        }
    }

    /// `session/cancel` 通知 + 对挂起审批回 cancelled（FR-SESSION-5、FR-ACP-8）。
    pub fn cancel(&self, session_id: &SessionId) -> Result<(), AcpError> {
        let (cx, _) = self.cx()?;
        self.inner.broker.cancel_session(session_id);
        cx.send_notification(CancelNotification::new(session_id.clone()))?;
        Ok(())
    }

    /// 若 agent 支持 `session/close` 则调用（FR-SESSION-8）。
    pub async fn close_session(&self, session_id: &SessionId) -> Result<(), AcpError> {
        let (cx, info) = self.cx()?;
        if !info.supports_close() {
            return Ok(());
        }
        tokio::time::timeout(Duration::from_secs(10), cx.send_request(CloseSessionRequest::new(session_id.clone())).block_task())
            .await
            .map_err(|_| AcpError::Timeout)??;
        Ok(())
    }
}

impl Inner {
    fn route_update(&self, n: SessionNotification) {
        let routes = self.routes.lock().unwrap();
        match routes.get(&n.session_id) {
            Some(r) => {
                let _ = r.tx.send(SessionEvent::Update(n.update));
            }
            None => debug!(session_id = %n.session_id.0, "收到无人订阅的 session/update，丢弃"),
        }
    }

    fn handle_permission(
        &self,
        req: RequestPermissionRequest,
        responder: agent_client_protocol::Responder<RequestPermissionResponse>,
    ) {
        // 非 ask 策略：直接回应，不打扰用户（FR-PERM-1）。
        if self.policy != PermissionPolicy::Ask {
            let resp = match auto_decision(self.policy, &req.options) {
                Some(id) => RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
                    SelectedPermissionOutcome::new(id.clone()),
                )),
                None => RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled),
            };
            info!(session_id = %req.session_id.0, policy = ?self.policy, "权限请求已自动处理");
            let _ = responder.respond(resp);
            return;
        }

        let routes = self.routes.lock().unwrap();
        let Some(route) = routes.get(&req.session_id) else {
            warn!(session_id = %req.session_id.0, "权限请求找不到会话订阅者，回 cancelled");
            let _ = responder.respond(RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled));
            return;
        };
        let ticket_id = uuid::Uuid::new_v4().simple().to_string();
        let ticket = PermissionTicket {
            ticket_id: ticket_id.clone(),
            session_id: req.session_id.clone(),
            title: req.tool_call.fields.title.clone().unwrap_or_else(|| "工具调用".to_string()),
            kind: req.tool_call.fields.kind.map(|k| format!("{k:?}").to_ascii_lowercase()),
            raw_input: req.tool_call.fields.raw_input.clone(),
            options: req.options.clone(),
        };
        self.broker.register(
            ticket_id,
            PendingPermission {
                responder,
                session_id: req.session_id.clone(),
                options: req.options,
                requester_open_id: route.requester_open_id.clone(),
            },
        );
        if route.tx.send(SessionEvent::Permission(ticket)).is_err() {
            warn!(session_id = %req.session_id.0, "会话订阅者已关闭，权限请求将等待超时");
        }
    }
}

/// 把 `AGENT_COMMAND`/`AGENT_ARGS` 之外的字符串形式（例如测试里）解析为 AcpAgent。
pub fn agent_from_command_line(cmdline: &str) -> Result<AcpAgent, AcpError> {
    AcpAgent::from_str(cmdline).map_err(|e| AcpError::Capability(format!("无法解析 agent 命令行: {e}")))
}
