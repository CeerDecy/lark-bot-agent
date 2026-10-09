//! 装配：配置 → 飞书客户端 → agent 子进程 supervisor → 会话管理 → 飞书长连接。
use larksuite_oapi_sdk_rs::event::EventDispatcher;
use larksuite_oapi_sdk_rs::events::im::P2MessageReceiveV1;
use larksuite_oapi_sdk_rs::{CardActionTriggerRequest, CardActionTriggerResponse, LarkClient, LarkError, Toast};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use lark_bot_agent::acp::AcpClient;
use lark_bot_agent::agent::AgentLaunch;
use lark_bot_agent::config::{Config, PermissionPolicy};
use lark_bot_agent::lark::LarkApi;
use lark_bot_agent::session::SessionManager;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let cfg = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("配置错误: {e}");
            std::process::exit(2);
        }
    };
    if cfg.allows_everyone() {
        warn!("未配置 ALLOWED_OPEN_IDS：所有能联系到机器人的人都可以驱动 agent，相当于开放远程 shell。强烈建议配置白名单。");
    }
    if cfg.permission_policy == PermissionPolicy::AutoAllow {
        warn!("PERMISSION_POLICY=auto_allow：agent 的所有工具调用将不经审批直接执行。");
    }

    let client = LarkClient::builder(&cfg.app_id, &cfg.app_secret).build()?;
    let lark = LarkApi::new(client.clone());
    let bot_open_id = lark.bot_open_id().await;
    info!(bot_open_id = ?bot_open_id, agent = cfg.agent_kind.as_str(), cwd = %cfg.agent_cwd.display(), "配置加载完成");

    let launch = AgentLaunch::from_config(&cfg);
    let acp = AcpClient::new(launch, cfg.permission_policy, cfg.acp_trace_dir.clone());
    tokio::spawn(acp.clone().run());

    let sessions = SessionManager::new(cfg, acp, lark, bot_open_id);
    tokio::spawn(sessions.clone().watch_agent_status());
    tokio::spawn(sessions.clone().reap_idle());

    // 事件回调里只做入队，尽快 ack（FR-IM-6 / NFR-1）。
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel::<P2MessageReceiveV1>(256);
    let worker_sessions = sessions.clone();
    tokio::spawn(async move {
        while let Some(event) = event_rx.recv().await {
            let s = worker_sessions.clone();
            tokio::spawn(async move { s.handle_event(event).await });
        }
    });

    let card_sessions = sessions.clone();
    let dispatcher = EventDispatcher::new("", "")
        .on_p2_im_message_receive_v1(move |event: P2MessageReceiveV1| {
            let event_tx = event_tx.clone();
            async move {
                event_tx
                    .try_send(event)
                    .map_err(|err| LarkError::Event(format!("event queue is full or closed: {err}")))
            }
        })
        .on_card_action_trigger(move |req: CardActionTriggerRequest| {
            let sessions = card_sessions.clone();
            async move {
                let open_id = req.operator.as_ref().map(|o| o.open_id.clone()).unwrap_or_default();
                let action = req.action.as_ref();
                let get = |k: &str| -> Option<String> {
                    action
                        .and_then(|a| a.value.get(k))
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                };
                let (text, ok) = match (get("action").as_deref(), get("ticket_id"), get("option_id")) {
                    (Some("acp_permission"), Some(ticket), Some(option)) => {
                        sessions.resolve_permission(&ticket, &option, &open_id).await
                    }
                    _ => ("未知的卡片操作。".to_string(), false),
                };
                Ok(CardActionTriggerResponse {
                    toast: Some(Toast::new(text).toast_type(if ok { "success" } else { "error" })),
                    card: None,
                })
            }
        });

    let ws_client = client
        .ws_client(dispatcher)
        .on_ready(|| info!("websocket connected"))
        .on_disconnected(|| warn!("websocket disconnected"))
        .on_error(|err| error!(error = %err, "websocket error"));

    info!("lark-bot-agent started (Ctrl+C to stop)");
    tokio::spawn(ws_client.start()).await??;
    Ok(())
}
