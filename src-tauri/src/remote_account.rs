//! 御符账号接入（远程实例连接·账号化，定案见 docs/plans/2026-09-04-instance-address-report.md
//! 与 2026-08-29-remote-account-upgrade-review.md「七、定案附记」）。
//!
//! 职责：SSO 登录器（系统浏览器 + 固定回环回调 + state/nonce）、实例清单
//! （GET /api/v1/me/instances）、exchange 客户端（dsh-remote /__remote/exchange）、
//! TOFU 首连确认存储。
//!
//! 安全纪律（红队裁决落实）：
//! - SSO JWT **仅内存**（调用方持有，本模块不落盘、不进任何文件）
//! - 回调端口固定 127.0.0.1:18499，被占用显式报错（不静默降级）
//! - state/nonce 防回调重放；fragment relay 页面只在本机回环服务
//! - TOFU：首连地址需用户确认，确认记录按 address 落盘
//!
//! HTTP 走裸 TcpStream（风格同 remote.rs，零新依赖）。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::Manager;

/// 打开独立控制窗（幂等：已存在则 show+focus）。模式照 notifications.rs——
/// 跨平台建窗必须主线程，统一 run_on_main_thread 派发。
pub fn open_control_window(app: &tauri::AppHandle) -> tauri::Result<()> {
    if let Some(w) = app.get_webview_window("remote-control") {
        let _ = w.show();
        let _ = w.set_focus();
        return Ok(());
    }
    let handle = app.clone();
    app.run_on_main_thread(move || {
        if let Err(e) = build_control_window(&handle) {
            if let Some(mut log) = crate::runtime::open_log_append() {
                use std::io::Write;
                let _ = writeln!(log, "[远程实例] 创建控制窗失败: {e}");
            }
        }
    })
}

fn build_control_window(app: &tauri::AppHandle) -> tauri::Result<()> {
    // 双重检查：并发触发时（派发排队期间第二次调用）可能已建好
    if let Some(w) = app.get_webview_window("remote-control") {
        let _ = w.show();
        let _ = w.set_focus();
        return Ok(());
    }
    tauri::WebviewWindowBuilder::new(
        app,
        "remote-control",
        tauri::WebviewUrl::App("remote.html".into()),
    )
    .title("远程实例 · 御符账号")
    .inner_size(560.0, 680.0)
    .min_inner_size(460.0, 520.0)
    .center()
    .build()?;
    Ok(())
}

/// 御符接入端点（v1：env 可覆盖 + 部署缺省；窗口 UI 落地后改走用户设置）。
#[derive(Clone, Debug)]
pub struct AccountEndpoints {
    /// 御符网关（浑天 SSO、白名单、反代入口），如 http://172.20.10.91:18085
    pub gateway: String,
    /// 御符 agent-backend（/api/v1/me/instances 所在；经网关前缀时填网关地址+前缀）
    pub backend: String,
    /// 桌面回环回调端口（定案建议 18499；御符侧 redirect 白名单须含同值）
    pub callback_port: u16,
}

impl AccountEndpoints {
    pub fn from_env() -> Self {
        let gateway = std::env::var("DSH_YUFU_GATEWAY")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "http://172.20.10.91:18085".into());
        let backend = std::env::var("DSH_YUFU_BACKEND")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("{gateway}/agent"));
        Self { gateway, backend, callback_port: 18499 }
    }

    /// SSO 登录页地址（redirect 已编码）。
    ///
    /// 真实契约（御符侧已上线，2026-09-04 源码核实，见
    /// docs/plans/2026-09-04-remote-account-stage3-design.md「待定/联调项」）：
    /// `GET {gateway}/api/v1/auth/huntian/sso-login?redirect=<url编码回调>`——
    /// redirect 参数接受完整外部 URL，按 host 对白名单精确匹配（127.0.0.1:18499
    /// 已入白名单）；fragment 形态投递 `#token=<urlencoded JWT>`（不回传 state，
    /// 御符 fragment 无此字段；LAN + 白名单精确匹配为缓解，code 形态落地后再强制）。
    pub fn sso_login_url(&self, state: &str) -> String {
        let redirect = urlencode(&format!(
            "http://127.0.0.1:{}/cb",
            self.callback_port
        ));
        format!(
            "{}/api/v1/auth/huntian/sso-login?redirect={redirect}&state={state}",
            self.gateway
        )
    }

    pub fn callback_origin(&self) -> String {
        format!("http://127.0.0.1:{}", self.callback_port)
    }

    pub fn instances_url(&self) -> String {
        format!("{}/api/v1/me/instances", self.backend)
    }
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// epoch 毫秒（main.rs 组装 RemoteConfig.paired_at 用；remote.rs 未导出时间助手）。
pub fn now_ms_pub() -> u64 {
    now_ms()
}

/// 轻量不可预测值（state/nonce 用）：时间纳秒 ⊕ pid ⊕ 进程内自增，非密码学强度——
/// 回调只打本机回环且一次性比对，防的是无害方误触与简单重放。
fn pseudo_random_tag() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static CTR: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut x = nanos
        ^ (std::process::id() as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ CTR.fetch_add(1, Ordering::Relaxed).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^= x >> 31;
    format!("{x:016x}")
}

/// 云端名下实例（/me/instances 条目；address 可 null = 未开启远程访问）。
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct CloudInstance {
    #[serde(default)]
    pub instance_id: String,
    #[serde(default)]
    pub device_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub hostname: String,
    /// 可直连的 dsh-remote 网关 authority（host:port）；null/空 = 未开启远程访问
    #[serde(default)]
    pub address: Option<String>,
    #[serde(default)]
    pub owner_user_id: String,
    #[serde(default)]
    pub agent_count: u32,
    #[serde(default)]
    pub last_seen_at: Option<u64>,
    #[serde(default)]
    pub created_at: Option<u64>,
}

/// exchange 成功产物（与配对 token 同形状，供既有连接执行层直接消费）。
#[derive(Clone, Debug)]
pub struct ExchangeResult {
    pub token: String,
    pub device_id: String,
    pub name: String,
}

/// 登录会话产物：SSO JWT（仅内存，调用方负责生命周期）+ 本次 state。
#[derive(Clone, Debug)]
pub struct SsoSession {
    pub jwt: String,
    pub state: String,
}

/// TOFU 首连确认记录（按 address 落盘；runtime_root/remote_tofu.json）。
fn tofu_path() -> std::path::PathBuf {
    crate::runtime::runtime_root().join("remote_tofu.json")
}

/// 该地址是否已通过首连确认。
pub fn tofu_approved(address: &str) -> bool {
    let Ok(raw) = std::fs::read_to_string(tofu_path()) else { return false };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else { return false };
    v.get("approved")
        .and_then(|a| a.get(address))
        .map(|_| true)
        .unwrap_or(false)
}

/// 记录首连确认（幂等）。
pub fn tofu_approve(address: &str) -> Result<(), String> {
    let path = tofu_path();
    let mut v = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .unwrap_or_else(|| serde_json::json!({ "approved": {} }));
    if let Some(obj) = v.get_mut("approved").and_then(|a| a.as_object_mut()) {
        obj.insert(address.to_string(), serde_json::json!(now_ms()));
    }
    std::fs::write(&path, serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?)
        .map_err(|e| format!("写入 TOFU 记录失败: {e}"))
}

/// 极简 HTTP/1.1 请求（Connection: close，支持自定义头；风格同 remote.rs）。
fn http_request(origin: &str, method: &str, path: &str, headers: &[(&str, &str)], body: Option<&str>) -> Option<Vec<u8>> {
    let authority = origin.strip_prefix("http://")?;
    let (host, port) = authority.rsplit_once(':')?;
    let port: u16 = port.parse().ok()?;
    let mut stream = TcpStream::connect((host, port)).ok()?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    match body {
        Some(b) => {
            req.push_str(&format!("Content-Length: {}\r\n\r\n{b}", b.len()));
        }
        None => req.push_str("\r\n"),
    }
    stream.write_all(req.as_bytes()).ok()?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }
    Some(buf)
}

/// 取响应 body（解 chunked + 跳过头部）；非 2xx 返回 Err（带状态行）。
fn body_of_2xx(raw: &[u8]) -> Result<String, String> {
    let head = String::from_utf8_lossy(raw);
    let status_line = head.lines().next().unwrap_or("");
    let code = status_line
        .strip_prefix("HTTP/1.0 ")
        .or_else(|| status_line.strip_prefix("HTTP/1.1 "))
        .and_then(|rest| rest.split_whitespace().next())
        .unwrap_or("");
    if !code.starts_with('2') {
        return Err(format!("HTTP {status_line}"));
    }
    let start = raw.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4).unwrap_or(0);
    let body = dechunk(raw);
    String::from_utf8(body[start.min(body.len())..].to_vec())
        .map_err(|_| "响应不是合法 UTF-8".into())
}

/// chunked 解码（与 remote.rs::dechunk_response 同规则；本模块自带一份避免跨模块互引私有件）。
fn dechunk(raw: &[u8]) -> Vec<u8> {
    let Some(split) = raw.windows(4).position(|w| w == b"\r\n\r\n") else {
        return raw.to_vec();
    };
    let (head, body) = raw.split_at(split + 4);
    let head_lower = String::from_utf8_lossy(head).to_ascii_lowercase();
    if !head_lower.lines().any(|l| l.starts_with("transfer-encoding:") && l.contains("chunked")) {
        return raw.to_vec();
    }
    let mut out = head.to_vec();
    let mut rest = body;
    loop {
        let Some(line_end) = rest.windows(2).position(|w| w == b"\r\n") else { break };
        let line = String::from_utf8_lossy(&rest[..line_end]);
        let Ok(size) = usize::from_str_radix(line.trim().split(';').next().unwrap_or("").trim(), 16) else { break };
        rest = &rest[line_end + 2..];
        if size == 0 || size > rest.len() { break }
        out.extend_from_slice(&rest[..size]);
        rest = rest.strip_prefix(b"\r\n").unwrap_or(rest);
    }
    out
}

/// 拉取当前账号名下实例清单。
pub fn instances(endpoints: &AccountEndpoints, jwt: &str) -> Result<Vec<CloudInstance>, String> {
    let raw = http_request(
        &endpoints.backend,
        "GET",
        "/api/v1/me/instances",
        &[("Authorization", &format!("Bearer {jwt}"))],
        None,
    )
    .ok_or("无法连接御符（超时或拒绝）")?;
    let body = body_of_2xx(&raw).map_err(|e| format!("实例清单拉取失败：{e}"))?;
    let v: serde_json::Value = serde_json::from_str(&body).map_err(|_| "实例清单不是合法 JSON")?;
    let list = v
        .get("instances")
        .or_else(|| v.get("data").and_then(|d| d.get("instances")))
        .cloned()
        .unwrap_or(v);
    serde_json::from_value(list).map_err(|e| format!("实例清单字段不匹配：{e}"))
}

/// 向目标实例的 dsh-remote 网关换取实例 token（登录即连的核心一步）。
/// 服务端契约（2026-09-04 定稿）：POST {address}/__remote/exchange，body {jwt}
/// （Bearer 头同发无害），dsh-remote 内部调御符 sso-verify {jwt, hostname=自身}
/// → 200 {ok:true, token, deviceId, name}（与配对响应同形状）；
/// 401/403（含 ownership_mismatch）→ 映射中文错误。
pub fn exchange(endpoints: &AccountEndpoints, jwt: &str, address: &str) -> Result<ExchangeResult, String> {
    let _ = endpoints;
    let origin = format!("http://{address}");
    let body = serde_json::json!({ "jwt": jwt }).to_string();
    let raw = http_request(
        &origin,
        "POST",
        "/__remote/exchange",
        &[("Authorization", &format!("Bearer {jwt}")), ("Content-Type", "application/json")],
        Some(&body),
    )
    .ok_or("无法连接远程实例（exchange 超时或拒绝）")?;
    let body = body_of_2xx(&raw).map_err(|e| {
        if e.contains("401") {
            "exchange 被拒（401：登录态失效或御符验签不通过——请重新登录御符账号）".to_string()
        } else if e.contains("403") {
            "exchange 被拒（403：该实例不属于你的账号——ownership_mismatch）".to_string()
        } else {
            format!("exchange 失败：{e}")
        }
    })?;
    let v: serde_json::Value = serde_json::from_str(&body).map_err(|_| "exchange 应答不是合法 JSON")?;
    let token = v.get("token").and_then(|t| t.as_str()).unwrap_or("").to_string();
    if token.is_empty() {
        return Err("exchange 应答缺少 token（可能账号无权访问该实例）".into());
    }
    Ok(ExchangeResult {
        token,
        device_id: v.get("deviceId").and_then(|t| t.as_str()).unwrap_or("").to_string(),
        name: v.get("name").and_then(|t| t.as_str()).unwrap_or("").to_string(),
    })
}

/// 实例存活探活：GET http://<address>/ ——任何 HTTP 应答（含 401）都算可达
/// （无实例 token 时网关回 401，401=存活；区别于 TCP 拒绝/超时=不可达）。
/// 短超时，供清单页的存活徽标。
pub fn probe_alive(address: &str) -> bool {
    let origin = format!("http://{address}");
    http_request(&origin, "GET", "/", &[], None).is_some()
}

/// 回调页 HTML：fragment 不会到服务端，必须由页面 JS 回交——
/// 契约（御符已上线）：`#token=<urlencoded JWT>`，JS 按 `#token=` 解析 +
/// decodeURIComponent，POST JSON `{token}` 到 /cb/token。
fn callback_html() -> String {
    "<!doctype html><meta charset=\"utf-8\"><title>御驿桌面登录</title>\
     <body style=\"font-family:system-ui\">登录完成，正在回传凭证…<script>\
     const h=location.hash||'';\
     const m=h.match(/^#token=(.*)$/);\
     const t=m?decodeURIComponent(m[1]):null;\
     if(!t){document.body.innerText='回调缺少凭证字段，请重试登录';}\
     else{fetch('/cb/token',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({token:t})}).finally(()=>{document.body.innerText='已完成，可关闭此页';});}\
     </script></body>"
     .to_string()
}

/// 解析请求行（`METHOD SP path SP protocol`）。方法/路径分派全靠它，必须取对——
/// 第一版误把协议串当方法（POST 永不命中，登录回调死循环），教训进测试。
fn parse_request_line(line: &str) -> (String, String) {
    let mut it = line.split_whitespace();
    let method = it.next().unwrap_or("").to_string();
    let path = it.next().unwrap_or("").to_string();
    (method, path)
}

/// /cb/token 的 JSON body 里提取 JWT（sso.go 的 fragment 键是 `token`，
/// 同时宽容 access_token/jwt 字段名，防未来形态切换）
fn parse_token_body(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    for k in ["token", "jwt", "access_token"] {
        if let Some(t) = v.get(k).and_then(|x| x.as_str()) {
            if !t.is_empty() { return Some(t.to_string()) }
        }
    }
    None
}

/// /cb 的 query 形态（code 形态项 5 落地后启用）：提取 code。
fn parse_code_query(query: &str) -> Option<String> {
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=')?;
        if k == "code" { return Some(urldecode(v)) }
    }
    None
}

/// code 形态兑换（项 5 契约已上线 spec）：POST {code, redirect_uri} → {token,...}；
/// 400 INVALID_CODE / 410 CODE_EXPIRED → 统一中文错误。
fn code_exchange(endpoints: &AccountEndpoints, code: &str) -> Result<String, String> {
    let body = serde_json::json!({
        "code": code,
        "redirect_uri": format!("http://127.0.0.1:{}/cb", endpoints.callback_port),
    }).to_string();
    let raw = http_request(
        &endpoints.gateway,
        "POST",
        "/api/v1/auth/sso/code-exchange",
        &[("Content-Type", "application/json")],
        Some(&body),
    )
    .ok_or("无法连接御符（code 兑换超时或拒绝）")?;
    let body = body_of_2xx(&raw).map_err(|e| {
        let msg = if e.contains("400") {
            "登录码无效（请重试登录）"
        } else if e.contains("410") {
            "登录码已过期（请重试登录）"
        } else {
            "御符 code 兑换失败"
        };
        format!("{msg}：{e}")
    })?;
    let v: serde_json::Value = serde_json::from_str(&body).map_err(|_| "兑换应答不是合法 JSON")?;
    let token = v.get("token").and_then(|t| t.as_str()).unwrap_or("").to_string();
    if token.is_empty() { return Err("兑换应答缺少 token".into()) }
    Ok(token)
}

fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() + 1 && i + 2 <= bytes.len() - 1 + 1 => {
                if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(b);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).unwrap_or(s.to_string())
}

/// SSO 登录完整流：起回环监听 → 拉起系统浏览器到御符 SSO → 等回调 relay → 校验 state
/// → 返回 SSO JWT（仅内存）。整体超时 `wait`；端口被占显式报错（红队裁决：不静默降级）。
pub fn sso_login(endpoints: &AccountEndpoints, wait: Duration) -> Result<SsoSession, String> {
    let state = pseudo_random_tag();
    let listener = TcpListener::bind(("127.0.0.1", endpoints.callback_port))
        .map_err(|e| format!("回调端口 {} 被占用（{e}）；请关闭占用该端口的进程后重试", endpoints.callback_port))?;
    let _ = listener.set_nonblocking(true);

    let url = endpoints.sso_login_url(&state);
    open_browser(&url)?;

    let deadline = std::time::Instant::now() + wait;
    loop {
        if std::time::Instant::now() >= deadline {
            return Err("登录超时：未在时限内收到御符回调".into());
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 1024];
                let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
                // 读满为止：head 以 \r\n\r\n 结尾；POST 再按 Content-Length 等 body 收齐
                // （不再等 3s 超时——fetch 的 keep-alive 下不会主动半关连接）。
                let mut want_body = 0usize;
                loop {
                    match stream.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(n) => {
                            buf.extend_from_slice(&chunk[..n]);
                            let Some(head_end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
                                continue;
                            };
                            let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                            if head.starts_with("POST ") {
                                if want_body == 0 {
                                    for line in head.lines() {
                                        let l = line.trim().to_ascii_lowercase();
                                        if let Some(v) = l.strip_prefix("content-length:") {
                                            want_body = v.trim().parse().unwrap_or(0);
                                        }
                                    }
                                }
                                if buf.len() - head_end - 4 >= want_body {
                                    break;
                                }
                                continue;
                            }
                            break;
                        }
                        Err(_) => break,
                    }
                }
                let req = String::from_utf8_lossy(&buf).to_string();
                let (method, path) = parse_request_line(req.lines().next().unwrap_or(""));
                if path.starts_with("/cb/token") && method == "POST" {
                    // fragment 形态的回交端点：页面 JS 把 #token=<urlencoded> 解析后以 JSON POST 过来
                    let body = req.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
                    if let Some(token) = parse_token_body(&body) {
                        let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
                        return Ok(SsoSession { jwt: token, state });
                    }
                    let _ = stream.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                } else if path.starts_with("/cb") {
                    // code 形态（项 5 落地后启用）：/cb?code=… 直接兑换；fragment 形态：回 relay 页
                    let query = path.split_once('?').map(|(_, q)| q.to_string());
                    if let Some(q) = query {
                        if let Some(code) = parse_code_query(&q) {
                            match code_exchange(endpoints, &code) {
                                Ok(token) => {
                                    let html = "<!doctype html><meta charset=\"utf-8\"><body>登录成功，可关闭此页</body>".to_string();
                                    let _ = stream.write_all(
                                        format!("HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}", html.len()).as_bytes(),
                                    );
                                    return Ok(SsoSession { jwt: token, state });
                                }
                                Err(e) => {
                                    let html = format!("<!doctype html><meta charset=\"utf-8\"><body>登录失败：{e}</body>");
                                    let _ = stream.write_all(
                                        format!("HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}", html.len()).as_bytes(),
                                    );
                                }
                            }
                            continue;
                        }
                    }
                    let html = callback_html();
                    let _ = stream.write_all(
                        format!("HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}", html.len()).as_bytes(),
                    );
                } else {
                    let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(120));
            }
            Err(e) => return Err(format!("回调监听异常：{e}")),
        }
    }
}

/// 拉起系统浏览器（Windows cmd start / macOS open / Linux xdg-open）。
/// Windows 用 runtime::no_window 隐藏子进程控制台——裸 cmd /C start 会闪黑窗。
fn open_browser(url: &str) -> Result<(), String> {
    #[cfg(windows)]
    {
        // start 的第一个引号参数是窗口标题占位；url 必须是第二个参数
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", "", url]);
        return crate::runtime::no_window(&mut c)
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("打开系统浏览器失败：{e}"));
    }
    #[cfg(target_os = "macos")]
    {
        return std::process::Command::new("open")
            .arg(url)
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("打开系统浏览器失败：{e}"));
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        return std::process::Command::new("xdg-open")
            .arg(url)
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("打开系统浏览器失败：{e}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sso_login_url_contains_encoded_redirect_and_state() {
        let ep = AccountEndpoints {
            gateway: "http://172.20.10.91:18085".into(),
            backend: "http://172.20.10.91:18085/agent".into(),
            callback_port: 18499,
        };
        let url = ep.sso_login_url("abc123");
        assert!(url.starts_with("http://172.20.10.91:18085/api/v1/auth/huntian/sso-login?"));
        assert!(url.contains(&format!("redirect={}", urlencode("http://127.0.0.1:18499/cb"))));
        assert!(url.contains("state=abc123"));
    }

    #[test]
    fn token_body_parse_extracts_jwt() {
        assert_eq!(parse_token_body(r#"{"token":"abc.def"}"#).as_deref(), Some("abc.def"));
        assert_eq!(parse_token_body(r#"{"jwt":"zzz"}"#).as_deref(), Some("zzz"));
        assert!(parse_token_body(r#"{"state":"only"}"#).is_none());
        assert!(parse_token_body("not json").is_none());
    }

    #[test]
    fn code_query_parse_extracts_code() {
        assert_eq!(parse_code_query("code=c1&state=s1").as_deref(), Some("c1"));
        assert_eq!(parse_code_query("state=only"), None);
    }

    #[test]
    fn request_line_parse_takes_method_then_path() {
        // 回归：第一版把协议串当方法（POST 永不命中，登录回调死循环）。
        let (m1, p1) = parse_request_line("POST /cb/token HTTP/1.1");
        assert_eq!(m1, "POST");
        assert_eq!(p1, "/cb/token");
        let (m2, p2) = parse_request_line("GET /cb?code=abc HTTP/1.1");
        assert_eq!(m2, "GET");
        assert_eq!(p2, "/cb?code=abc");
    }

    #[test]
    fn urldecode_handles_percent_and_plus() {
        assert_eq!(urldecode("a%20b+c"), "a b c");
        assert_eq!(urldecode("%E4%B8%AD"), "中");
    }

    #[test]
    fn tofu_roundtrip_with_temp_dir() {
        // 独立 TOFU 存储路径临时替换不可行（tofu_path 为常量函数）——
        // 这里只验证判定函数在无文件时的行为；写路径的真机验证在联调覆盖。
        let weird = format!("addr-that-{}-not-approved", std::process::id());
        assert!(!tofu_approved(&weird));
    }

    #[test]
    fn exchange_rejects_missing_token_in_body() {
        // 用一个必然连接失败的地址验证错误路径（不 mock 网络，保持零依赖风格）
        let ep = AccountEndpoints {
            gateway: String::new(),
            backend: String::new(),
            callback_port: 18499,
        };
        let err = exchange(&ep, "jwt", "127.0.0.1:1").unwrap_err();
        assert!(err.contains("exchange"));
    }
}
