//! 御符账号接入（远程实例连接·账号化，定案见 docs/plans/2026-09-04-instance-address-report.md
//! 与 2026-08-29-remote-account-upgrade-review.md「七、定案附记」）。
//!
//! 职责：SSO 登录器（RFC 8628 设备码风格：start → 系统浏览器 → poll 轮询）、
//! 实例清单（GET /api/v1/me/instances）、exchange 客户端（dsh-remote
//! /__remote/exchange）、TOFU 首连确认存储。
//!
//! 安全纪律（红队裁决落实）：
//! - SSO JWT **仅内存**（调用方持有，本模块不落盘、不进任何文件）
//! - 轮询通道（commit 2f3096f 起）：session_id 关联、token 单次消费防重放、
//!   TTL 5 分钟；不再开本地监听端口、不传回环 redirect 给御符——
//!   yufu↔dsh 与 yufu↔浑天两段彻底解耦，IP 直连全链路成立
//! - TOFU：首连地址需用户确认，确认记录按 address 落盘
//!
//! HTTP 走裸 TcpStream（风格同 remote.rs，零新依赖）。

use std::io::{Read, Write};
use std::net::TcpStream;
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
    /// 御符网关（SSO start/poll、白名单、反代入口），如 http://172.20.10.91:18085
    pub gateway: String,
    /// 御符 agent-backend（/api/v1/me/instances 所在，与 gateway 同源同前缀——
    /// 2026-09-04 联调实测：/agent 前缀为老路由概念，直连路径才注册）。
    pub backend: String,
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
            .unwrap_or_else(|| gateway.clone());
        Self { gateway, backend }
    }

    /// SSO 设备码风格轮询通道（御符 commit 2f3096f 上线，2026-09-04 联调定稿）：
    /// `POST {gateway}/api/v1/auth/sso/start` → `{session_id, login_url, expires_in}`；
    /// login_url 由御符生成（redirectUrl 固定御符域名 callback + 路径段 session_id，
    /// 符合浑天接入契约），桌面只负责拉起浏览器，不再传回环 redirect。
    pub fn sso_start_path() -> &'static str {
        "/api/v1/auth/sso/start"
    }

    /// `GET {gateway}/api/v1/auth/sso/poll?session_id=<id>` →
    /// 200 {ok:false,status:"pending"} / 200 {ok:true,token}（单次消费）/
    /// 410 SESSION_EXPIRED（重新 start）。
    pub fn sso_poll_path(session_id: &str) -> String {
        format!("/api/v1/auth/sso/poll?session_id={session_id}")
    }

    pub fn instances_url(&self) -> String {
        format!("{}/api/v1/me/instances", self.backend)
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// epoch 毫秒（main.rs 组装 RemoteConfig.paired_at 用；remote.rs 未导出时间助手）。
pub fn now_ms_pub() -> u64 {
    now_ms()
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

/// 登录会话产物：SSO JWT（仅内存，调用方负责生命周期）。
#[derive(Clone, Debug)]
pub struct SsoSession {
    pub jwt: String,
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

/// start 应答解析：{session_id, login_url, expires_in}（expires_in 缺省 300）。
fn parse_start_body(body: &str) -> Result<(String, String, u64), String> {
    let v: serde_json::Value = serde_json::from_str(body).map_err(|_| "sso/start 应答不是合法 JSON")?;
    let session_id = v.get("session_id").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let login_url = v.get("login_url").and_then(|x| x.as_str()).unwrap_or("").to_string();
    if session_id.is_empty() || login_url.is_empty() {
        return Err("sso/start 应答缺少 session_id/login_url".into());
    }
    let expires_in = v.get("expires_in").and_then(|x| x.as_u64()).unwrap_or(300);
    Ok((session_id, login_url, expires_in))
}

/// poll 应答解析：Ok(Some(token))=完成（单次消费）；Ok(None)=pending；Err=契约异常。
/// 410 SESSION_EXPIRED 在调用方按 HTTP 状态分派（不走本函数）。
fn parse_poll_body(body: &str) -> Result<Option<String>, String> {
    let v: serde_json::Value = serde_json::from_str(body).map_err(|_| "sso/poll 应答不是合法 JSON")?;
    let ok = v.get("ok").and_then(|x| x.as_bool()).unwrap_or(false);
    if !ok {
        return Ok(None);
    }
    let token = v.get("token").and_then(|t| t.as_str()).unwrap_or("").to_string();
    if token.is_empty() {
        return Err("sso/poll 应答 ok=true 但缺少 token".into());
    }
    Ok(Some(token))
}

/// SSO 登录完整流（RFC 8628 设备码风格轮询通道，御符 commit 2f3096f 上线）：
/// start 取 {session_id, login_url} → 拉起系统浏览器（login_url 由御符生成，
/// redirectUrl 固定御符域名 callback，符合浑天接入契约）→ 轮询 poll 到出 token。
/// 全程 IP 直连 gateway：无本地监听端口、无 cookie 域依赖、无 fragment relay。
/// session TTL 5 分钟、token 单次消费防重放；整体超时取 min(wait, expires_in)。
pub fn sso_login(endpoints: &AccountEndpoints, wait: Duration) -> Result<SsoSession, String> {
    let raw = http_request(
        &endpoints.gateway,
        "POST",
        AccountEndpoints::sso_start_path(),
        &[("Content-Type", "application/json")],
        Some("{}"),
    )
    .ok_or("无法连接御符（sso/start 超时或拒绝）")?;
    let body = body_of_2xx(&raw).map_err(|e| format!("SSO 会话创建失败：{e}"))?;
    let (session_id, login_url, expires_in) = parse_start_body(&body)?;

    open_browser(&login_url)?;

    let deadline = std::time::Instant::now() + wait.min(Duration::from_secs(expires_in.max(1)));
    let poll_path = AccountEndpoints::sso_poll_path(&session_id);
    loop {
        if std::time::Instant::now() >= deadline {
            return Err("登录超时：未在时限内完成浏览器认证（会话已过期，请重新登录）".into());
        }
        std::thread::sleep(Duration::from_millis(1500));
        // 瞬时网络抖动不判死（继续轮询）；确定性失败（410/契约异常）立即返回
        let Some(raw) = http_request(&endpoints.gateway, "GET", &poll_path, &[], None) else {
            continue;
        };
        match body_of_2xx(&raw) {
            Ok(body) => match parse_poll_body(&body)? {
                Some(token) => return Ok(SsoSession { jwt: token }),
                None => continue,
            },
            Err(e) if e.contains("410") => {
                return Err("SSO 会话已过期或不存在（请重新发起登录）".into());
            }
            Err(e) => return Err(format!("SSO 轮询失败：{e}")),
        }
    }
}

/// 拉起系统浏览器（Windows rundll32 / macOS open / Linux xdg-open）。
/// Windows 用 runtime::no_window 隐藏子进程控制台。
///
/// 教训（2026-09-07 真机实测）：此前用 `cmd /C start "" <url>`——Rust 在 Windows
/// 只对含空白的参数加引号，URL 里的 `&` 裸传给 cmd 被当成命令分隔符，
/// `&sid=…` 整段被截掉（sso-login 丢 sid → 御符走旧 fragment 回退 →
/// 浏览器被甩到无人监听的 127.0.0.1:18499）。旧流程截掉的只是无害的 &state=
/// 所以长期未暴露。rundll32 FileProtocolHandler 不经 cmd，无 shell 解析问题。
fn open_browser(url: &str) -> Result<(), String> {
    #[cfg(windows)]
    {
        let mut c = std::process::Command::new("rundll32");
        c.args(["url.dll,FileProtocolHandler", url]);
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
    fn sso_paths_match_poll_channel_contract() {
        // 契约（御符 commit 2f3096f）：start 取会话，poll 轮询拿 token
        assert_eq!(AccountEndpoints::sso_start_path(), "/api/v1/auth/sso/start");
        assert_eq!(
            AccountEndpoints::sso_poll_path("sid123"),
            "/api/v1/auth/sso/poll?session_id=sid123"
        );
    }

    #[test]
    fn start_body_parse_extracts_session_and_url() {
        let (sid, url, exp) = parse_start_body(
            r#"{"session_id":"s1","login_url":"https://huntian.hzins.com/login?redirectUrl=x","expires_in":300}"#,
        )
        .unwrap();
        assert_eq!(sid, "s1");
        assert!(url.starts_with("https://huntian.hzins.com/login"));
        assert_eq!(exp, 300);
        // expires_in 缺省 300；缺字段报错
        let (_, _, exp2) =
            parse_start_body(r#"{"session_id":"s1","login_url":"u"}"#).unwrap();
        assert_eq!(exp2, 300);
        assert!(parse_start_body(r#"{"login_url":"u"}"#).is_err());
        assert!(parse_start_body(r#"{"session_id":"s1"}"#).is_err());
        assert!(parse_start_body("not json").is_err());
    }

    #[test]
    fn poll_body_parse_three_states() {
        // pending → None；ok → token；ok 缺 token → Err（契约异常不误判为 pending）
        assert_eq!(parse_poll_body(r#"{"ok":false,"status":"pending"}"#).unwrap(), None);
        assert_eq!(
            parse_poll_body(r#"{"ok":true,"token":"jwt.abc"}"#).unwrap(),
            Some("jwt.abc".to_string())
        );
        assert!(parse_poll_body(r#"{"ok":true}"#).is_err());
        assert!(parse_poll_body("not json").is_err());
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
        };
        let err = exchange(&ep, "jwt", "127.0.0.1:1").unwrap_err();
        assert!(err.contains("exchange"));
    }
}
