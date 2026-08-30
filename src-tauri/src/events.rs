//! 事件流订阅：把「回合完成 / 审批请求 / 用户提问」转成 OS 原生通知与任务栏闪烁。
//! 服务端→壳单向，Harness 页面零改动、零权限。
//!
//! 双协议自适应（按端点连通性，不做版本判断）：
//! - dsh ≤0.1.1（rc.x）：`ws <base>/api/events.mux`，文本 JSON 帧免认证。
//!   实测帧协议（2026-08 验证）：method="session/event" 的 turn/start →
//!   assistant/chunk* → turn/end{data.turn, data.reason.kind}；顶层帧
//!   payload.type="approval/requested" / "question/requested"。
//! - dsh ≥0.1.2：`/api` 全部要求签名 cookie 且 events.mux 端点移除。壳先
//!   GET launch_url（含一次性 token）→ 303 + Set-Cookie，握手带 Cookie 连
//!   `ws <base>/api/remote.mux`，首帧 `{"type":"open","streamId":…,
//!   "endpoint":"$events","payload":{"args":{}}}` 订阅。下游 item.value：
//!   ready / emit（如 api-session/status）/ waterfall（approval/request、
//!   user-questions/request）/ cancel。**壳是通知方不是审批方**：waterfall
//!   一律立即回 `{kind:'next'}`（HTTP POST /api/$events/result）放行给真正的
//!   UI，绝不代替主人批准或回答。
//!
//! 事件语义在 0.1.2 有损：白名单没有 turn 生命周期，回合完成的近似物是
//! emit "api-session/status" (sessionId, running=false) 的真值翻转。
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;

use serde_json::{json, Value};
use tauri::Manager;
use tungstenite::client::IntoClientRequest;
use tungstenite::Message;

use crate::AppState;

/// 订阅线程：连不上或断线即退出——服务重启后由 supervisor 用新地址重新 spawn；
/// 旧线程经世代号自检退出，不会重复通知。
pub fn spawn(app: &tauri::AppHandle, base_url: &str, launch_url: &str) {
    let gen = {
        let state: tauri::State<AppState> = app.state();
        state.events_gen.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1
    };
    let app = app.clone();
    let base = base_url.to_string();
    let launch = launch_url.to_string();
    std::thread::spawn(move || run(app, base, launch, gen));
}

/// tungstenite::connect 的产物类型（MaybeTlsStream 包装，wss 场景自动 TLS）。
type WsStream = tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>;

fn log_note(msg: &str) {
    use std::io::Write;
    if let Some(mut f) = crate::runtime::open_log_append() {
        let _ = writeln!(f, "[events] {msg}");
    }
}

fn run(app: tauri::AppHandle, base_url: String, launch_url: String, gen: u64) {
    log_note(&format!("事件流启动 (gen={gen}): base={base_url}"));
    // dsh ≥0.1.2：token 换 cookie。rc.2 无认证（200 无 Set-Cookie）→ None。
    let cookie = exchange_cookie(&launch_url);
    match &cookie {
        Some(_) => log_note("已用一次性 token 换取会话 cookie（≥0.1.2 认证协议）"),
        None => log_note("服务未返回会话 cookie（≤0.1.1 免认证协议）"),
    }

    // 1) 旧协议：events.mux（rc.x）。0.1.2 上该端点已被移除/要求认证，握手会失败。
    match connect_legacy(&base_url) {
        Ok(socket) => {
            log_note("事件流已连接（events.mux，≤0.1.1 协议）");
            legacy_loop(app, socket, gen);
            return;
        }
        Err(e) => log_note(&format!("events.mux 不可用（{e}）")),
    }

    // 2) 新协议：remote.mux + cookie（≥0.1.2）。
    let Some(cookie) = cookie else {
        log_note("无会话 cookie，无法尝试 remote.mux，放弃订阅（服务重启后会重试）");
        return;
    };
    match connect_remote(&base_url, &cookie) {
        Ok(socket) => {
            log_note("事件流已连接（remote.mux，≥0.1.2 协议）");
            remote_loop(app, base_url, cookie, socket, gen);
        }
        Err(e) => log_note(&format!("remote.mux 连接失败，放弃订阅（服务重启后会重试）: {e}")),
    }
}

/* ════════════════════════ 远程模式（dsh-remote 网关） ════════════════════════ */

/// 远程模式订阅线程：带网关凭证连远程实例的事件流（无本地一次性 token 可换 cookie）。
/// 线程内置断线退避重连（见 run_remote）：世代号不过期则持续重试，模式切换/重连
/// 流程推进世代号即让位退出。
pub fn spawn_remote(app: &tauri::AppHandle, base_url: &str, token: &str) {
    let gen = {
        let state: tauri::State<AppState> = app.state();
        state.events_gen.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1
    };
    let app = app.clone();
    let base = base_url.to_string();
    let token = token.to_string();
    std::thread::spawn(move || run_remote(app, base, token, gen));
}

fn run_remote(app: tauri::AppHandle, base_url: String, token: String, gen: u64) {
    log_note(&format!("远程事件流启动 (gen={gen}): base={base_url}"));
    // 远程网关没有本地 launch_url/一次性 token：跳过 exchange_cookie，直接带凭证头握手。
    // 0.1.17 起断线自动重连（带退避）：连接失败或断流后按 backoff_delay_ms 序列重试，
    // 世代号不过期则按 30s 封顶节奏持续重试——通知路径的守护语义（v1 的已知局限，
    // 见 README 已知限制）。模式切换/重连流程会推进世代号（spawn_remote 每次 +1），
    // 旧线程自检退出，不会重复通知。
    let mut attempt: u32 = 0;
    loop {
        if generation_expired(&app, gen) {
            log_note("世代过期，停止远程订阅线程");
            return;
        }
        match connect_legacy_with_token(&base_url, &token) {
            Ok(socket) => {
                // 连接成功后退避重新起步：「偶发断流」与「持续不可达」分开计数
                attempt = 0;
                log_note("远程事件流已连接（events.mux + x-remote-token）");
                legacy_loop(app.clone(), socket, gen); // 返回即断流（世代过期时循环顶自检退出）
                log_note("远程事件流断开，将按退避序列自动重连");
            }
            Err(e) => log_note(&format!("远程事件流连接失败（将自动重试）: {e}")),
        }
        let delay_ms = backoff_delay_ms(attempt);
        attempt = attempt.saturating_add(1);
        // 分片睡眠：期间世代过期即刻退出，模式切换无须等完整退避（最长 30s）结束
        if !sleep_while_generation_valid(&app, gen, delay_ms) {
            log_note("世代过期，停止远程订阅线程");
            return;
        }
    }
}

/// 远程重连退避序列（毫秒）：2s → 4s → 8s → 16s → 30s 封顶，其后恒 30s。
/// 纯函数便于单测；attempt 从 0 计（首次失败后的等待）。
fn backoff_delay_ms(attempt: u32) -> u64 {
    match attempt {
        0 => 2_000,
        1 => 4_000,
        2 => 8_000,
        3 => 16_000,
        _ => 30_000,
    }
}

/// 分片睡眠 total_ms，每 250ms 检查一次世代号；过期返回 false（调用方退出线程）。
fn sleep_while_generation_valid(app: &tauri::AppHandle, gen: u64, total_ms: u64) -> bool {
    let mut remaining = total_ms;
    while remaining > 0 {
        if generation_expired(app, gen) {
            return false;
        }
        let slice = remaining.min(250);
        std::thread::sleep(std::time::Duration::from_millis(slice));
        remaining -= slice;
    }
    !generation_expired(app, gen)
}

/// 远程模式 WS 握手：请求带 x-remote-token 头（网关据此鉴权；Host 仍按 URL 推导，
/// 手法同 connect_remote 的 Cookie 注入）。
fn connect_legacy_with_token(base_url: &str, token: &str) -> Result<WsStream, String> {
    let url = format!("{base_url}/api/events.mux").replace("http://", "ws://");
    let request = build_handshake_request(&url, token)?;
    tungstenite::connect(request)
        .map(|(s, _)| s)
        .map_err(|e| e.to_string())
}

/// 构造带 x-remote-token 头的 WS 握手请求（纯构造不发起连接，便于单测）。
fn build_handshake_request(
    url: &str,
    token: &str,
) -> Result<tungstenite::http::Request<()>, String> {
    let mut request = url
        .into_client_request()
        .map_err(|e| format!("构造握手请求失败: {e}"))?;
    let value =
        tungstenite::http::HeaderValue::from_str(token).map_err(|e| format!("非法 token 头: {e}"))?;
    request.headers_mut().insert("x-remote-token", value);
    Ok(request)
}

/* ════════════════════════ 旧协议（≤0.1.1 events.mux） ════════════════════════ */

fn connect_legacy(base_url: &str) -> Result<WsStream, String> {
    let url = format!("{base_url}/api/events.mux").replace("http://", "ws://");
    tungstenite::connect(url).map(|(s, _)| s).map_err(|e| e.to_string())
}

fn legacy_loop(app: tauri::AppHandle, mut socket: WsStream, gen: u64) {
    loop {
        if generation_expired(&app, gen) {
            log_note("世代过期，停止旧订阅线程");
            return;
        }
        match socket.read() {
            Ok(Message::Text(text)) => {
                if let Some(notice) = classify_legacy(&text) {
                    log_note(&format!("通知: {notice:?}"));
                    present(&app, &notice);
                }
            }
            Ok(Message::Ping(p)) => {
                let _ = socket.send(Message::Pong(p));
            }
            Ok(Message::Close(_)) | Err(_) => {
                log_note("事件流断开，订阅线程退出");
                return;
            }
            Ok(_) => {}
        }
    }
}

/// 旧协议帧 → 通知。
fn classify_legacy(text: &str) -> Option<Notice> {
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

/* ════════════════════════ 新协议（≥0.1.2 remote.mux） ════════════════════════ */

/// GET launch_url（带一次性 token）：≥0.1.2 返回 303 + Set-Cookie，取 `name=value`；
/// ≤0.1.1 直出 200 无 Set-Cookie → None。cookie 必须整段原样带回
/// （v1.<base64url body>.<hmac>，Host 按 authority 验签）。
fn exchange_cookie(launch_url: &str) -> Option<String> {
    let (authority, path) = split_http_url(launch_url)?;
    let raw = http_request(&authority, "GET", &path, None, None, 4096)?;
    let head = String::from_utf8_lossy(&raw).to_string();
    exchange_cookie_from_head(&head)
}

/// 从响应头块提取认证 cookie（须 3xx 且带 set-cookie；取首个值到第一个 `;` 前）。
fn exchange_cookie_from_head(head: &str) -> Option<String> {
    if !head.starts_with("HTTP/1.1 3") && !head.starts_with("HTTP/1.0 3") {
        return None;
    }
    header_value(head, "set-cookie").map(|sc| sc.split(';').next().unwrap_or("").trim().to_string())
}

fn connect_remote(base_url: &str, cookie: &str) -> Result<WsStream, String> {
    let url = format!("{base_url}/api/remote.mux").replace("http://", "ws://");
    let mut request = url
        .into_client_request()
        .map_err(|e| format!("构造握手请求失败: {e}"))?;
    // Host 由 URL 推导；围栏要求 cookie 的 authority 与 Host 一致（原生客户端不发 Origin）
    let value = tungstenite::http::HeaderValue::from_str(cookie).map_err(|e| format!("非法 cookie 头: {e}"))?;
    request.headers_mut().insert("Cookie", value);
    tungstenite::connect(request)
        .map(|(s, _)| s)
        .map_err(|e| e.to_string())
}

fn remote_loop(
    app: tauri::AppHandle,
    base_url: String,
    cookie: String,
    mut socket: WsStream,
    gen: u64,
) {
    // 订阅事件流：握手后第一条数据帧。streamId 客户端自造，进程内唯一即可。
    let stream_id = next_id("stream");
    let open = json!({"type": "open", "streamId": stream_id, "endpoint": "$events", "payload": {"args": {}}});
    if let Err(e) = socket.send(Message::text(open.to_string())) {
        log_note(&format!("发送 $events 订阅失败: {e}"));
        return;
    }
    log_note(&format!("已订阅 $events（streamId={stream_id}）"));

    let mut running: HashMap<String, bool> = HashMap::new();
    // ready 帧带来的 clientId 必须跨帧保存：$events/result 应答用它关联订阅方，
    // 丢了应答会被 Host 丢弃、审批 waterfall 挂住
    let mut client_id = String::new();
    loop {
        if generation_expired(&app, gen) {
            log_note("世代过期，停止旧订阅线程");
            return;
        }
        match socket.read() {
            Ok(Message::Text(text)) => {
                let outcome = handle_remote_frame(&text, &stream_id, &mut running);
                if !outcome.client_id.is_empty() {
                    client_id = outcome.client_id;
                }
                for notice in outcome.notices {
                    log_note(&format!("通知: {notice:?}"));
                    present(&app, &notice);
                }
                // waterfall 应答走 HTTP POST（不走 WS）：壳永远只放行，不代替主人决定
                for event_id in outcome.waterfall_event_ids {
                    let body = remote_answer_body(&client_id, &event_id);
                    match http_post_json(&base_url, "/api/$events/result", Some(&cookie), &body) {
                        Ok(ok) => log_note(&format!("waterfall {event_id} 已回 next（{ok}）")),
                        Err(e) => log_note(&format!("waterfall {event_id} 应答失败: {e}")),
                    }
                }
            }
            Ok(Message::Ping(p)) => {
                let _ = socket.send(Message::Pong(p));
            }
            Ok(Message::Close(_)) | Err(_) => {
                log_note("事件流断开，订阅线程退出");
                return;
            }
            Ok(_) => {}
        }
    }
}

/// 一帧 remote.mux 下行消息的处理结果。
pub struct RemoteOutcome {
    pub notices: Vec<Notice>,
    pub waterfall_event_ids: Vec<String>,
    pub client_id: String,
}

/// 新协议帧 → （通知列表，需要回 next 的 waterfall 事件列表）。
/// 下行：{"type":"item","streamId","value":{ready|emit|waterfall|cancel}}；
/// error/end 帧不产通知。waterfall 无论是否生成通知都要回 next——壳不参与决策，
/// 不回会挂住 Host 的 waterfall（无人应答时审批一直等待）。
pub fn handle_remote_frame(frame: &str, stream_id: &str, running: &mut HashMap<String, bool>) -> RemoteOutcome {
    let mut outcome = RemoteOutcome { notices: Vec::new(), waterfall_event_ids: Vec::new(), client_id: String::new() };
    let Ok(v) = serde_json::from_str::<Value>(frame) else { return outcome };
    if v.get("type").and_then(|t| t.as_str()) != Some("item") {
        return outcome;
    }
    if v.get("streamId").and_then(|s| s.as_str()) != Some(stream_id) {
        return outcome;
    }
    let Some(value) = v.get("value") else { return outcome };
    outcome.client_id = value
        .get("clientId")
        .and_then(|c| c.as_str())
        .unwrap_or_default()
        .to_string();
    match value.get("type").and_then(|t| t.as_str()) {
        Some("emit") => {
            let event = value.get("event").and_then(|e| e.as_str()).unwrap_or("");
            if event == "api-session/status" {
                // args = [sessionId, running]；true→false 翻转视为“回合结束”。
                // 近似语义：0.1.2 白名单没有 turn 生命周期，拿不到 turn 号与 reason。
                let args = value.get("args").and_then(|a| a.as_array());
                let session = args.and_then(|a| a.first()).and_then(|s| s.as_str()).unwrap_or("").to_string();
                let is_running = args.and_then(|a| a.get(1)).and_then(|r| r.as_bool()).unwrap_or(false);
                if !session.is_empty() {
                    let prev = running.insert(session.clone(), is_running);
                    if prev == Some(true) && !is_running {
                        outcome.notices.push(Notice {
                            title: "DSH 会话空闲".into(),
                            body: "回合已结束".into(),
                        });
                    }
                }
            }
        }
        Some("waterfall") => {
            let event = value.get("event").and_then(|e| e.as_str()).unwrap_or("");
            let event_id = value.get("eventId").and_then(|e| e.as_str()).unwrap_or("").to_string();
            match event {
                "approval/request" => {
                    let tool = value
                        .pointer("/request/toolName")
                        .and_then(|t| t.as_str())
                        .unwrap_or("工具");
                    outcome.notices.push(Notice {
                        title: "DSH 审批请求".into(),
                        body: format!("「{tool}」等待你的批准"),
                    });
                }
                "user-questions/request" => {
                    outcome.notices.push(Notice {
                        title: "DSH 等待回答".into(),
                        body: "Agent 向你提出了问题".into(),
                    });
                }
                _ => {}
            }
            if !event_id.is_empty() {
                outcome.waterfall_event_ids.push(event_id);
            }
        }
        _ => {}
    }
    outcome
}

/// waterfall 应答帧：kind=next 表示“我不决策，下放给其它监听者（真正的 UI）”。
/// 壳绝不能回 allowed-once/result——那是替主人批准。
pub fn remote_answer_body(client_id: &str, event_id: &str) -> String {
    json!({
        "type": "client-request",
        "rpcId": next_id("rpc"),
        "method": "$events/result",
        "payload": { "args": { "clientId": client_id, "eventId": event_id, "outcome": { "kind": "next" } } }
    })
    .to_string()
}

/* ════════════════════════ 通知呈现 ════════════════════════ */

/// 需要桌面呈现的事项。
#[derive(Debug)]
pub struct Notice {
    pub title: String,
    pub body: String,
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

/* ════════════════════════ 小工具 ════════════════════════ */

fn generation_expired(app: &tauri::AppHandle, gen: u64) -> bool {
    let state: tauri::State<AppState> = app.state();
    state.events_gen.load(std::sync::atomic::Ordering::SeqCst) != gen
}

/// 进程内唯一 id（streamId/rpcId 只需本地唯一，无需全局 UUID）。
fn next_id(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{}-{}", std::process::id(), n)
}

/// 拆 http(s) URL：返回 (authority, path+query)。仅支持 http。
fn split_http_url(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("http://").unwrap_or(url);
    match rest.split_once('/') {
        Some((authority, tail)) => Some((authority.to_string(), format!("/{tail}"))),
        None => Some((rest.to_string(), "/".to_string())),
    }
}

/// 在响应头块（含状态行）里找头（大小写不敏感），返回首个值的 trim 结果。
fn header_value(head: &str, name: &str) -> Option<String> {
    for line in head.split("\r\n").skip(1) {
        let Some((k, v)) = line.split_once(':') else { continue };
        if k.trim().eq_ignore_ascii_case(name) {
            return Some(v.trim().to_string());
        }
    }
    None
}

/// 极简 HTTP/1.1 客户端：发请求、读响应头（+ 若有 body 一并读入）。
/// 返回原始字节（状态行 + 头 + 可能的部分 body）。
fn http_request(
    authority: &str,
    method: &str,
    path: &str,
    cookie: Option<&str>,
    body: Option<&str>,
    cap: usize,
) -> Option<Vec<u8>> {
    let (host, port) = authority
        .rsplit_once(':')
        .and_then(|(h, p)| p.parse().ok().map(|p| (h, p)))?;
    let mut stream = TcpStream::connect((host, port)).ok()?;
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(5)));
    let cookie_line = cookie
        .map(|c| format!("Cookie: {c}\r\n"))
        .unwrap_or_default();
    let body_line = body.unwrap_or("");
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {authority}\r\n{cookie_line}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body_line}",
        body_line.len()
    );
    stream.write_all(req.as_bytes()).ok()?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() >= cap {
                    break;
                }
            }
            Err(_) => break, // 超时即认为响应已完（Connection: close 语义）
        }
    }
    Some(buf)
}

/// POST JSON 并检查 2xx。
fn http_post_json(base_url: &str, path: &str, cookie: Option<&str>, body: &str) -> Result<bool, String> {
    let (authority, _) = split_http_url(base_url).ok_or("base_url 无法解析")?;
    let raw = http_request(&authority, "POST", path, cookie, Some(body), 8192).ok_or("应答请求失败")?;
    let head = String::from_utf8_lossy(&raw);
    Ok(head.starts_with("HTTP/1.1 2") || head.starts_with("HTTP/1.0 2"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /* ── 旧协议分类 ── */
    #[test]
    fn legacy_turn_end_yields_notice() {
        let frame = r#"{"method":"session/event","payload":{"type":"session/event","event":{"type":"turn/end","data":{"turn":3,"reason":{"kind":"stop"}}}}}"#;
        let n = classify_legacy(frame).unwrap();
        assert_eq!(n.title, "DSH 回合完成");
        assert!(n.body.contains("#3"));
        assert!(n.body.contains("stop"));
    }

    #[test]
    fn legacy_approval_yields_notice() {
        let frame = r#"{"payload":{"type":"approval/requested","toolName":"bash"}}"#;
        let n = classify_legacy(frame).unwrap();
        assert!(n.body.contains("bash"));
    }

    /* ── 新协议：订阅帧与应答帧 ── */
    #[test]
    fn open_frame_targets_events_endpoint() {
        let f: Value = serde_json::from_str(&json!({
            "type": "open", "streamId": "s1", "endpoint": "$events", "payload": {"args": {}}
        }).to_string()).unwrap();
        assert_eq!(f["endpoint"], "$events");
        assert_eq!(f["payload"]["args"], serde_json::json!({}));
    }

    #[test]
    fn waterfall_answer_is_next_never_decides() {
        let body = remote_answer_body("client-1", "event-9");
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["method"], "$events/result");
        assert_eq!(v["payload"]["args"]["clientId"], "client-1");
        assert_eq!(v["payload"]["args"]["eventId"], "event-9");
        assert_eq!(v["payload"]["args"]["outcome"], serde_json::json!({"kind": "next"}));
    }

    /* ── 新协议：下行帧分类 ── */
    #[test]
    fn ready_frame_records_client_id_only() {
        let frame = r#"{"type":"item","streamId":"s1","value":{"type":"ready","clientId":"c-1","host":{"home":"/h"}}}"#;
        let o = handle_remote_frame(frame, "s1", &mut HashMap::new());
        assert!(o.notices.is_empty() && o.waterfall_event_ids.is_empty());
        assert_eq!(o.client_id, "c-1");
    }

    #[test]
    fn other_stream_id_is_ignored() {
        let frame = r#"{"type":"item","streamId":"other","value":{"type":"emit","event":"api-session/status","args":["a",false]}}"#;
        let o = handle_remote_frame(frame, "s1", &mut HashMap::new());
        assert!(o.notices.is_empty());
    }

    #[test]
    fn session_status_notifies_only_on_true_to_false() {
        let mk = |s: &str| {
            format!(r#"{{"type":"item","streamId":"s1","value":{{"type":"emit","event":"api-session/status","args":["a",{s}]}}}}"#)
        };
        let mut running = HashMap::new();
        // 首帧即 false（连接时本就空闲）→ 不通知
        assert!(handle_remote_frame(&mk("false"), "s1", &mut running).notices.is_empty());
        // 运行中 → 不通知
        assert!(handle_remote_frame(&mk("true"), "s1", &mut running).notices.is_empty());
        // true→false 翻转 → 通知“回合已结束”
        let o = handle_remote_frame(&mk("false"), "s1", &mut running);
        assert_eq!(o.notices.len(), 1);
        assert_eq!(o.notices[0].title, "DSH 会话空闲");
    }

    #[test]
    fn approval_waterfall_yields_notice_and_next_answer() {
        let frame = r#"{"type":"item","streamId":"s1","value":{"type":"waterfall","event":"approval/request","eventId":"e-7","agentId":"a-1","request":{"toolName":"bash","reason":"rm -rf"}}}"#;
        let o = handle_remote_frame(frame, "s1", &mut HashMap::new());
        assert_eq!(o.notices.len(), 1);
        assert!(o.notices[0].body.contains("bash"));
        assert_eq!(o.waterfall_event_ids, vec!["e-7".to_string()]);
        let v: Value = serde_json::from_str(&remote_answer_body(&o.client_id, "e-7")).unwrap();
        assert_eq!(v["payload"]["args"]["outcome"]["kind"], "next");
    }

    #[test]
    fn question_waterfall_yields_notice_and_next_answer() {
        let frame = r#"{"type":"item","streamId":"s1","value":{"type":"waterfall","event":"user-questions/request","eventId":"e-8","agentId":"a-1","request":{"questions":[{"id":"q1","question":"继续吗"}]}}}"#;
        let o = handle_remote_frame(frame, "s1", &mut HashMap::new());
        assert_eq!(o.notices.len(), 1);
        assert_eq!(o.notices[0].title, "DSH 等待回答");
        assert_eq!(o.waterfall_event_ids, vec!["e-8".to_string()]);
    }

    #[test]
    fn unknown_waterfall_still_answers_next_without_notice() {
        let frame = r#"{"type":"item","streamId":"s1","value":{"type":"waterfall","event":"other/request","eventId":"e-9","agentId":"a-1","request":{}}}"#;
        let o = handle_remote_frame(frame, "s1", &mut HashMap::new());
        assert!(o.notices.is_empty());
        assert_eq!(o.waterfall_event_ids, vec!["e-9".to_string()]);
    }

    #[test]
    fn non_item_and_bad_json_are_ignored() {
        let mut running = HashMap::new();
        assert!(handle_remote_frame("not json", "s1", &mut running).notices.is_empty());
        assert!(handle_remote_frame(r#"{"type":"error","streamId":"s1","error":{"code":"x","message":"y","details":{}}}"#, "s1", &mut running)
            .notices
            .is_empty());
    }

    /* ── 头解析 ── */
    #[test]
    fn extracts_cookie_from_set_cookie_header() {
        let head = "HTTP/1.1 303 See Other\r\nlocation: /\r\nset-cookie: dsh-auth-AbC=v1.x.y; Max-Age=2592000; Path=/; HttpOnly; SameSite=Strict\r\ncontent-length: 0\r\n\r\n";
        let cookie = exchange_cookie_from_head(head);
        assert_eq!(cookie.unwrap(), "dsh-auth-AbC=v1.x.y");
    }

    #[test]
    fn set_cookie_header_is_case_insensitive() {
        let head = "HTTP/1.1 303 See Other\r\nSET-COOKIE: a=b\r\n\r\n";
        assert_eq!(exchange_cookie_from_head(head).unwrap(), "a=b");
    }

    /* ── 远程模式：断线退避重连序列 ── */
    #[test]
    fn remote_reconnect_backoff_doubles_then_caps_at_30s() {
        assert_eq!(backoff_delay_ms(0), 2_000);
        assert_eq!(backoff_delay_ms(1), 4_000);
        assert_eq!(backoff_delay_ms(2), 8_000);
        assert_eq!(backoff_delay_ms(3), 16_000);
        // 第 5 次起封顶 30s，此后恒定（守护语义：世代不过期则一直重试）
        assert_eq!(backoff_delay_ms(4), 30_000);
        assert_eq!(backoff_delay_ms(100), 30_000);
        assert_eq!(backoff_delay_ms(u32::MAX), 30_000);
    }

    /* ── 远程模式：握手请求带网关凭证 ── */
    #[test]
    fn remote_handshake_request_carries_token_header() {
        let request =
            build_handshake_request("ws://192.168.1.146:3090/api/events.mux", "tok-1").unwrap();
        assert_eq!(request.headers().get("x-remote-token").unwrap(), "tok-1");
        // Host 仍按 URL 推导（网关按 Host 判定合法 authority）
        assert_eq!(request.headers().get("host").unwrap(), "192.168.1.146:3090");
    }
}
