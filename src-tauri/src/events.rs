//! 以非浏览器客户端身份订阅 `<base>/api/events.mux` WebSocket 下行流，
//! 把「回合完成 / 审批请求 / 用户提问」转成 OS 原生通知与任务栏闪烁。
//! 服务端→壳单向，Harness 页面零改动、零权限。
//!
//! 实测帧协议（2026-08 验证）：
//! - 回合生命周期：method="session/event"，event.type 依次 turn/start →
//!   assistant/chunk* → assistant/message → turn/end{data.turn, data.reason.kind}
//! - 审批请求：顶层帧 payload.type="approval/requested"（含 toolName/reason）
//! - 用户提问：顶层帧 payload.type="question/requested"
use serde_json::Value;
use tauri::Manager;

use crate::AppState;

/// 订阅线程：连不上或断线即退出——服务重启后由 supervisor 用新地址重新 spawn；
/// 旧线程经世代号自检退出，不会重复通知。
pub fn spawn(app: &tauri::AppHandle, base_url: &str) {
    let gen = {
        let state: tauri::State<AppState> = app.state();
        state.events_gen.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1
    };
    let app = app.clone();
    let url = format!("{base_url}/api/events.mux").replace("http://", "ws://");
    std::thread::spawn(move || run(app, url, gen));
}

fn run(app: tauri::AppHandle, url: String, gen: u64) {
    let log_note = |msg: &str| {
        use std::io::Write;
        if let Some(mut f) = crate::runtime::open_log_append() {
            let _ = writeln!(f, "[events] {msg}");
        }
    };
    log_note(&format!("连接事件流 {url} (gen={gen})"));
    let Ok(stream) = tungstenite::connect(&url) else {
        log_note("事件流连接失败，本次不订阅（服务重启后会重试）");
        return;
    };
    let (mut socket, _resp) = stream;
    log_note("事件流已连接");
    loop {
        // 世代过期（服务已重启换地址）：立刻退出，避免重复通知
        let current = {
            let state: tauri::State<AppState> = app.state();
            state.events_gen.load(std::sync::atomic::Ordering::SeqCst)
        };
        if current != gen {
            log_note("世代过期，停止旧订阅线程");
            return;
        }
        match socket.read() {
            Ok(tungstenite::Message::Text(text)) => {
                if let Some(notice) = classify(&text) {
                    log_note(&format!("通知: {notice:?}"));
                    present(&app, &notice);
                }
            }
            Ok(tungstenite::Message::Ping(p)) => {
                let _ = socket.send(tungstenite::Message::Pong(p));
            }
            Ok(tungstenite::Message::Close(_)) | Err(_) => {
                log_note("事件流断开，订阅线程退出");
                return;
            }
            Ok(_) => {}
        }
    }
}

/// 需要桌面呈现的事项。
#[derive(Debug)]
pub struct Notice {
    pub title: String,
    pub body: String,
}

fn classify(text: &str) -> Option<Notice> {
    let v: Value = serde_json::from_str(text).ok()?;
    let payload = v.get("payload")?;
    match payload.get("type").and_then(|t| t.as_str())? {
        // 顶层 answerable 帧：审批/提问（无需关心 rpcId，仅提示）
        "approval/requested" => {
            let tool = payload.get("toolName").and_then(|t| t.as_str()).unwrap_or("工具");
            Some(Notice {
                title: "DSH 审批请求".into(),
                body: format!("「{tool}」等待你的批准"),
            })
        }
        "question/requested" => Some(Notice {
            title: "DSH 等待回答".into(),
            body: "Agent 向你提出了问题".into(),
        }),
        // 回合生命周期：只在 method=session/event 且 event.type=turn/end 时提醒
        _ => {
            let method = v.get("method").and_then(|m| m.as_str())?;
            if method != "session/event" {
                return None;
            }
            let event = payload.get("event")?;
            if event.get("type").and_then(|t| t.as_str())? != "turn/end" {
                return None;
            }
            let turn = event.pointer("/data/turn").and_then(|t| t.as_i64()).unwrap_or(0);
            let reason = event
                .pointer("/data/reason/kind")
                .and_then(|t| t.as_str())
                .unwrap_or("ended");
            Some(Notice {
                title: "DSH 回合完成".into(),
                body: format!("回合 #{turn} 已结束（{reason}）"),
            })
        }
    }
}

fn present(app: &tauri::AppHandle, notice: &Notice) {
    use tauri_plugin_notification::NotificationExt;
    let Some(w) = app.get_webview_window("main") else { return };
    let focused = w.is_focused().unwrap_or(false);
    let visible = w.is_visible().unwrap_or(false);
    if !focused {
        let _ = w.request_user_attention(Some(tauri::UserAttentionType::Informational));
    }
    if !(visible && focused) {
        let _ = app
            .notification()
            .builder()
            .title(&notice.title)
            .body(&notice.body)
            .show();
    }
}
