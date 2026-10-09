//! 假 ACP agent（仅用于集成测试，NFR-6）：按脚本回放 session/update 与权限请求。
//!
//! prompt 文本约定：
//! - `hello`：两段文本 + 一次工具调用，end_turn
//! - `perm`：发 session/request_permission，按用户选择回复
//! - `slow`：等待 session/cancel 后回 cancelled
//! - `crash`：直接 exit(1)
//! - `session/set_mode`：接受 `default` / `bypassPermissions`；bypass 下 `perm` 不发权限请求
//! - `summary`：第一段文本(m1) → 工具调用 → 第二段文本(m2) → end_turn，模拟真实 agent 的末尾顺序
use std::io::{BufRead, Write};

use serde_json::{Value, json};

fn send(v: Value) {
    let mut out = std::io::stdout().lock();
    writeln!(out, "{v}").unwrap();
    out.flush().unwrap();
}

fn update(session_id: &str, update: Value) {
    send(json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":session_id,"update":update}}));
}

fn main() {
    let stdin = std::io::stdin();
    let mut sessions = 0u32;
    let mut next_req_id = 1000i64;
    // 等待中的权限请求：(request id, session id, 原 prompt 请求 id)
    let mut pending_perm: Option<(i64, String, Value)> = None;
    // 等待 cancel 的慢 prompt：(session id, 原请求 id)
    let mut pending_slow: Option<(String, Value)> = None;
    // bypassPermissions 模式下 `perm` 不再发权限请求，直接执行。
    let mut bypass = false;

    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let method = msg.get("method").and_then(Value::as_str);
        let id = msg.get("id").cloned();
        let params = msg.get("params").cloned().unwrap_or(Value::Null);

        match method {
            Some("initialize") => send(json!({"jsonrpc":"2.0","id":id,"result":{
                "protocolVersion":1,
                "agentCapabilities":{"loadSession":false,"promptCapabilities":{"image":true,"audio":false,"embeddedContext":false}},
                "agentInfo":{"name":"fake-agent","version":"0.0.1"},
                "authMethods":[]
            }})),
            Some("session/new") => {
                sessions += 1;
                send(json!({"jsonrpc":"2.0","id":id,"result":{
                    "sessionId":format!("sess-{sessions}"),
                    "modes":{"currentModeId":"default","availableModes":[
                        {"id":"default","name":"Manual"},
                        {"id":"bypassPermissions","name":"Bypass permissions"}
                    ]}
                }}));
            }
            Some("session/set_mode") => {
                let mode = params["modeId"].as_str().unwrap_or("");
                let sid = params["sessionId"].as_str().unwrap_or("").to_string();
                if mode == "default" || mode == "bypassPermissions" {
                    bypass = mode == "bypassPermissions";
                    update(&sid, json!({"sessionUpdate":"current_mode_update","currentModeId":mode}));
                    send(json!({"jsonrpc":"2.0","id":id,"result":{}}));
                } else {
                    send(json!({"jsonrpc":"2.0","id":id,"error":{"code":-32602,"message":format!("Mode {mode} is not available in this session")}}));
                }
            }
            Some("session/prompt") => {
                let sid = params["sessionId"].as_str().unwrap_or("").to_string();
                let text = params["prompt"][0]["text"].as_str().unwrap_or("").to_string();
                match text.as_str() {
                    "crash" => std::process::exit(1),
                    "perm" if bypass => {
                        update(&sid, json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"bypassed:executed"}}));
                        send(json!({"jsonrpc":"2.0","id":id,"result":{"stopReason":"end_turn"}}));
                    }
                    "perm" => {
                        update(&sid, json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"need permission. "}}));
                        next_req_id += 1;
                        send(json!({"jsonrpc":"2.0","id":next_req_id,"method":"session/request_permission","params":{
                            "sessionId": sid,
                            "toolCall": {"toolCallId":"tc-perm","title":"运行 rm -rf build","kind":"execute","rawInput":{"command":["rm","-rf","build"]}},
                            "options": [
                                {"optionId":"allow","name":"允许一次","kind":"allow_once"},
                                {"optionId":"always","name":"总是允许","kind":"allow_always"},
                                {"optionId":"reject","name":"拒绝","kind":"reject_once"}
                            ]
                        }}));
                        pending_perm = Some((next_req_id, sid, id.unwrap_or(Value::Null)));
                    }
                    "summary" => {
                        update(&sid, json!({"sessionUpdate":"agent_message_chunk","messageId":"m1","content":{"type":"text","text":"我来看一下项目结构。"}}));
                        update(&sid, json!({"sessionUpdate":"tool_call","toolCallId":"tc-s","title":"Read README.md","kind":"read","status":"completed"}));
                        update(&sid, json!({"sessionUpdate":"agent_message_chunk","messageId":"m2","content":{"type":"text","text":"这是一个 "}}));
                        update(&sid, json!({"sessionUpdate":"agent_message_chunk","messageId":"m2","content":{"type":"text","text":"Rust 项目。"}}));
                        send(json!({"jsonrpc":"2.0","id":id,"result":{"stopReason":"end_turn"}}));
                    }
                    "slow" => {
                        update(&sid, json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"working..."}}));
                        pending_slow = Some((sid, id.unwrap_or(Value::Null)));
                    }
                    _ => {
                        update(&sid, json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Hi "}}));
                        update(&sid, json!({"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"thinking"}}));
                        update(&sid, json!({"sessionUpdate":"tool_call","toolCallId":"tc-1","title":"读取 README.md","kind":"read","status":"in_progress"}));
                        update(&sid, json!({"sessionUpdate":"tool_call_update","toolCallId":"tc-1","status":"completed"}));
                        update(&sid, json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"there"}}));
                        send(json!({"jsonrpc":"2.0","id":id,"result":{"stopReason":"end_turn"}}));
                    }
                }
            }
            Some("session/cancel") => {
                let sid = params["sessionId"].as_str().unwrap_or("").to_string();
                if let Some((s, req_id)) = pending_slow.take() {
                    if s == sid {
                        send(json!({"jsonrpc":"2.0","id":req_id,"result":{"stopReason":"cancelled"}}));
                    } else {
                        pending_slow = Some((s, req_id));
                    }
                }
            }
            Some(_) if id.is_some() => {
                send(json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Method not found"}}));
            }
            Some(_) => {}
            None => {
                // 这是对我们请求的回应（权限请求）。
                if let Some((req_id, sid, prompt_id)) = pending_perm.take() {
                    if id.as_ref().and_then(Value::as_i64) == Some(req_id) {
                        let outcome = &msg["result"]["outcome"];
                        if outcome["outcome"].as_str() == Some("selected") {
                            let opt = outcome["optionId"].as_str().unwrap_or("?");
                            update(&sid, json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":format!("approved:{opt}")}}));
                            send(json!({"jsonrpc":"2.0","id":prompt_id,"result":{"stopReason":"end_turn"}}));
                        } else {
                            send(json!({"jsonrpc":"2.0","id":prompt_id,"result":{"stopReason":"cancelled"}}));
                        }
                    } else {
                        pending_perm = Some((req_id, sid, prompt_id));
                    }
                }
            }
        }
    }
}
