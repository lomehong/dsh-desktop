//! 远程实例凭据与配对（对接 dsh-remote 网关，契约见其 README）：
//! - POST /__remote/pair {code} → {ok,token,…}；403 码无效；429 限速
//! - GET  /__remote/pair?token=<token> → 303 + Set-Cookie（给 webview 种凭证）
//!
//! 凭据落盘 remote.json：安装版 Windows 以 DPAPI（CurrentUser）加密 token（`tokenEnc`，
//! 经 PowerShell 调用，零新依赖），明文 `token` 字段省略；便携模式与非 Windows 保持明文
//! （与 dsh 宿主会话密钥同威胁模型，README 声明）——DPAPI 绑定用户+机器，U盘换机永远
//! 解不开，加密反成死档，便利优先回退明文。加密失败也回退明文（本模块无日志器，静默；
//! 有明文可用好过丢凭据要求重配）。读取双形状兼容：旧明文照读并惰性迁移为加密；
//! `tokenEnc` 解密失败（换用户/重装系统）→ token 置空照常返回，连接流程给出重新配对文案。
use std::io::{Read, Write};
use std::net::TcpStream;
#[cfg(windows)]
use std::process::{Command, Stdio};
use std::time::Duration;
#[cfg(windows)]
use std::time::Instant;

use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
pub struct RemoteConfig {
    pub address: String, // 裸 host:port
    pub origin: String,  // http://host:port
    pub token: String,
    pub paired_at: u64,
}

/// 落盘形状（`StoredConfig` 只在读写文件时存在，内存形态始终是 `RemoteConfig` 明文 token）：
/// 安装版 Windows → `{"address","origin","tokenEnc","paired_at"}`；便携/非 Windows/回退 →
/// `{"address","origin","token","paired_at"}`（与历史文件逐字节同形状）。两形状均可读。
#[derive(Serialize, Deserialize)]
struct StoredConfig {
    address: String,
    origin: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    token: String,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "tokenEnc")]
    token_enc: Option<String>,
    paired_at: u64,
}

pub fn config_path() -> std::path::PathBuf {
    crate::runtime::runtime_root().join("remote.json")
}

/// 模式记忆文件（"mode.txt"）：上次模式（"local"/"remote"）与凭据独立，断开远程不清凭据。
pub fn mode_file() -> &'static str {
    "mode.txt"
}

pub fn load_mode() -> &'static str {
    match std::fs::read_to_string(crate::runtime::runtime_root().join(mode_file())) {
        Ok(s) if s.trim() == "remote" => "remote",
        _ => "local",
    }
}

pub fn save_mode(mode: &str) {
    let _ = std::fs::write(crate::runtime::runtime_root().join(mode_file()), mode);
}

/// 是否按加密落盘：仅安装版 Windows（便携 = U盘换机 DPAPI 永远解不开，保持明文）。
fn encrypt_enabled() -> bool {
    cfg!(windows) && crate::runtime::portable_root().is_none()
}

/// 仅取展示信息（address/origin）——不解密 token：托盘提示等主线程消费方
/// 用它避免每次重建都付一次 PowerShell 解密（典型百毫秒、最坏 10 秒）。
pub fn load_display() -> Option<(String, String)> {
    let raw = std::fs::read_to_string(config_path()).ok()?;
    let stored: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let address = stored.get("address")?.as_str()?.to_string();
    let origin = stored.get("origin")?.as_str()?.to_string();
    Some((address, origin))
}

pub fn load() -> Option<RemoteConfig> {
    let (cfg, legacy_plaintext) = load_config_detailed(&config_path(), crypto_impl())?;
    // 惰性迁移：安装版 Windows 读到旧明文 → 立即重写为加密（尽力而为，失败不阻塞使用；
    // 一次性代价：本进程内最多多一次 PowerShell 调用）
    if legacy_plaintext && !cfg.token.is_empty() && encrypt_enabled() {
        let _ = save(&cfg);
    }
    Some(cfg)
}

pub fn save(cfg: &RemoteConfig) -> Result<(), String> {
    save_config_to(&config_path(), cfg, encrypt_enabled())
}

/* ── 多实例管理（v0.1.29+，D2）────────────────────────────────────────────
 * remotes.json = Vec<StoredConfig>（与 remote.json 同形状、同加密规则）。
 * 活动实例仍是 remote.json（向后兼容，supervisor/连接流程零改动）；
 * 配对成功时 remember_saved 归档一份；托盘「已保存的远程实例」子菜单点击
 * → load_saved 解密 token → save 为活动 → restart_by_mode 走既有远程序列。
 * ──────────────────────────────────────────────────────────────────────── */

/// 已保存实例列表条目（展示用，不含 token）。
#[derive(Clone, serde::Serialize)]
pub struct SavedRemote {
    pub address: String,
    pub origin: String,
    pub paired_at: u64,
}

/// 列表上限：托盘子菜单滚屏可读性优先。
const SAVED_CAP: usize = 8;

fn saved_path() -> std::path::PathBuf {
    crate::runtime::runtime_root().join("remotes.json")
}

/// 列出已保存实例（新→旧）。损坏文件按空列表处理（下次 remember_saved 覆写重建）。
pub fn saved_list() -> Vec<SavedRemote> {
    let Ok(text) = std::fs::read_to_string(saved_path()) else {
        return vec![];
    };
    let Ok(list) = serde_json::from_str::<Vec<StoredConfig>>(&text) else {
        return vec![];
    };
    list.into_iter()
        .map(|s| SavedRemote { address: s.address, origin: s.origin, paired_at: s.paired_at })
        .collect()
}

/// 读取某已保存实例的完整凭据（token 解密失败 → None，提示重新配对）。
pub fn load_saved(address: &str) -> Option<RemoteConfig> {
    let text = std::fs::read_to_string(saved_path()).ok()?;
    let list = serde_json::from_str::<Vec<StoredConfig>>(&text).ok()?;
    let stored = list.into_iter().find(|s| s.address == address)?;
    let crypto = crypto_impl();
    let token = match stored.token_enc {
        Some(blob) => crypto(&blob, Direction::Unprotect).unwrap_or_default(),
        None => stored.token,
    };
    if token.is_empty() {
        return None;
    }
    Some(RemoteConfig {
        address: stored.address,
        origin: stored.origin,
        token,
        paired_at: stored.paired_at,
    })
}

/// 归档一份已配对实例（去重按 address，最新在前，截断到 SAVED_CAP）。
/// 尽力而为：失败静默——多实例列表缺失不影响单实例主流程。
pub fn remember_saved(cfg: &RemoteConfig) {
    let mut list: Vec<StoredConfig> = std::fs::read_to_string(saved_path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    list.retain(|s| s.address != cfg.address);
    let crypto = crypto_impl();
    let encrypt = encrypt_enabled();
    let mut stored = StoredConfig {
        address: cfg.address.clone(),
        origin: cfg.origin.clone(),
        token: cfg.token.clone(),
        token_enc: None,
        paired_at: cfg.paired_at,
    };
    if encrypt {
        if let Some(blob) = crypto(&cfg.token, Direction::Protect) {
            stored.token_enc = Some(blob);
            stored.token.clear();
        }
    }
    list.insert(0, stored);
    list.truncate(SAVED_CAP);
    let tmp = saved_path().with_extension("json.tmp");
    if let Ok(text) = serde_json::to_string_pretty(&list) {
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, saved_path());
        }
    }
}

/// 读配置并报告 token 是否来自旧明文形状（无 `tokenEnc` 字段）——`load()` 据此惰性迁移。
/// crypto 为加解密 seam（测试注入假实现；真实现 = DPAPI）。
fn load_config_detailed(
    path: &std::path::Path,
    crypto: CryptoFn,
) -> Option<(RemoteConfig, bool)> {
    // 损坏/缺失一律 None（配对即覆盖，无需恢复语义）
    let stored: StoredConfig = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let legacy_plaintext = stored.token_enc.is_none();
    let token = match stored.token_enc {
        // 解密失败（换用户/换机/字段损毁）→ token 置空：配置照常返回，
        // 连接流程对「配置在场但 token 为空」给出重新配对文案（见 supervisor connect_remote_flow）
        Some(blob) => crypto(&blob, Direction::Unprotect).unwrap_or_default(),
        None => stored.token,
    };
    Some((
        RemoteConfig {
            address: stored.address,
            origin: stored.origin,
            token,
            paired_at: stored.paired_at,
        },
        legacy_plaintext,
    ))
}

fn save_config_to(path: &std::path::Path, cfg: &RemoteConfig, encrypt: bool) -> Result<(), String> {
    save_config_with(path, cfg, encrypt, crypto_impl())
}

/// crypto 为加解密 seam（测试注入假实现；真实现 = DPAPI）。encrypt=false 落明文
/// （便携/非 Windows）；encrypt=true 但加密失败也回退明文（便利优先，见模块注释）。
fn save_config_with(
    path: &std::path::Path,
    cfg: &RemoteConfig,
    encrypt: bool,
    crypto: CryptoFn,
) -> Result<(), String> {
    let mut stored = StoredConfig {
        address: cfg.address.clone(),
        origin: cfg.origin.clone(),
        token: cfg.token.clone(),
        token_enc: None,
        paired_at: cfg.paired_at,
    };
    if encrypt {
        if let Some(blob) = crypto(&cfg.token, Direction::Protect) {
            stored.token_enc = Some(blob);
            stored.token.clear(); // 密文在场，明文字段省略（skip_serializing_if）
        }
    }
    // tmp + rename：写一半崩溃不会留下半个 remote.json
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string(&stored).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

/* ── token 加解密（DPAPI via PowerShell，零新依赖） ── */

/// 加解密方向：Protect（明文→base64 密文）/ Unprotect（base64 密文→明文）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Direction {
    Protect,
    Unprotect,
}

/// 加解密 seam：真实现为 Windows DPAPI，测试注入假实现（真 DPAPI 无法在 CI 跑）。
type CryptoFn = fn(&str, Direction) -> Option<String>;

fn crypto_impl() -> CryptoFn {
    #[cfg(windows)]
    return dpapi_windows;
    #[cfg(not(windows))]
    // 非 Windows 无 DPAPI：恒 None。save 从不请求加密（encrypt_enabled恒false），
    // load 读到 tokenEnc（如手工拷贝文件）→ token 置空 → 提示重新配对
    return |_: &str, _: Direction| -> Option<String> { None };
}

/// DPAPI 超时上限：挂死的 PowerShell 不能冻结 connect 流程（同 connect_remote 的教训）。
#[cfg(windows)]
const DPAPI_TIMEOUT: Duration = Duration::from_secs(10);

/// 组装 PowerShell 脚本（纯函数，便于单测转义与形状）：
/// - `Add-Type -AssemblyName System.Security` 必需——没有它 ProtectedData 解析不可靠
///   （本机实测曾静默产出垃圾）；
/// - try/catch 保证失败也走 stdout（`ERR: ` 前缀）而非空输出/难排查；
/// - `$p` 用单引号字符串内嵌：`'` 双写成 `''`（PS 单引号串唯一转义），其余字符全字面量。
#[cfg_attr(not(windows), allow(dead_code))]
fn dpapi_script(input: &str, dir: Direction) -> String {
    let escaped = input.replace('\'', "''");
    let call = match dir {
        Direction::Protect => "[Convert]::ToBase64String([System.Security.Cryptography.ProtectedData]::Protect([Text.Encoding]::UTF8.GetBytes($p),$null,[System.Security.Cryptography.DataProtectionScope]::CurrentUser))",
        Direction::Unprotect => "[Text.Encoding]::UTF8.GetString([System.Security.Cryptography.ProtectedData]::Unprotect([Convert]::FromBase64String($p),$null,[System.Security.Cryptography.DataProtectionScope]::CurrentUser))",
    };
    format!(
        "$p='{escaped}'; try {{ Add-Type -AssemblyName System.Security; Write-Output ({call}) }} catch {{ Write-Output ('ERR: ' + $_.Exception.Message) }}"
    )
}

/// Windows 真实现：PowerShell + DPAPI(CurrentUser)。stdout 起始 `ERR: ` / 空输出 / 超时
/// / spawn 失败一律 None（调用方决定回退明文或置空 token）。
/// v0.1.28+ 加 -WindowStyle Hidden：除 CREATE_NO_WINDOW 之外的第二道防线，
/// 防止 PowerShell 加载时偶发弹窗闪烁（远程模式首次配对 / 凭据轮转路径）。
#[cfg(windows)]
fn dpapi_windows(input: &str, dir: Direction) -> Option<String> {
    let script = dpapi_script(input, dir);
    let mut command = Command::new("powershell");
    command.args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", &script]);
    let mut child = crate::runtime::no_window(&mut command)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // 轮询 + 超时击杀（std 无 wait_timeout；~15 行换掉「一次挂死冻结整条 connect 流」的坑）
    let deadline = Instant::now() + DPAPI_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Err(_) => return None,
        }
    }
    // 进程已退出再读管道：token/密文量级远小于管道缓冲，不构成子进程写阻塞（否则走超时）
    let mut raw = Vec::new();
    if let Some(mut pipe) = child.stdout.take() {
        let _ = pipe.read_to_end(&mut raw);
    }
    // lossy：stdout 编码随控制台代码页，base64/token 均为 ASCII，非 ASCII 字节不致命
    let out = String::from_utf8_lossy(&raw);
    let out = out.trim();
    if out.is_empty() || out.starts_with("ERR: ") {
        return None;
    }
    Some(out.to_string())
}

/// 解析地址输入：裸 `host:port` 或 `http://host:port`；只支持 http、端口必填。
/// 返回 (address, origin)。
pub fn parse_address(input: &str) -> Result<(String, String), String> {
    let s = input.trim();
    if s.starts_with("https://") {
        return Err("只支持 http:// 地址（网关不带 TLS）".into());
    }
    // 裸地址与带 http:// 前缀都接受，内部统一按裸 host:port 处理
    let s = s.strip_prefix("http://").unwrap_or(s);
    if s.contains('/') || s.contains('?') || s.contains('#') {
        return Err("地址只能是 host:port，不含路径".into());
    }
    let (host, port) = s
        .rsplit_once(':')
        .ok_or("地址缺少端口（如 192.168.1.146:3090）")?;
    let port: u16 = port.parse().map_err(|_| "端口必须是数字".to_string())?;
    if port == 0 || host.is_empty() || host.contains(':') {
        return Err("地址无效".into());
    }
    Ok((s.to_string(), format!("http://{s}")))
}

/// 从粘贴文本中提取配对链接的 (address, code)。
pub fn parse_pairing_link(text: &str) -> Result<(String, String), String> {
    let marker = "/__remote/pair?code=";
    let start = text
        .find("http://")
        .ok_or("未找到配对链接（应以 http:// 开头或包含）")?;
    let rest = &text[start..];
    let end = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
    let url = &rest[..end];
    let idx = url.find(marker).ok_or("链接缺少配对码参数")?;
    let addr_part = &url[..idx];
    let code = url[idx + marker.len()..].trim_end_matches('/');
    if code.is_empty() {
        return Err("链接中配对码为空".into());
    }
    let (addr, _) = parse_address(addr_part)?;
    Ok((addr, code.to_string()))
}

/// POST /__remote/pair 换 token（裸 TcpStream，风格同 events.rs，零新依赖）。
/// 状态映射：2xx→token；403→码无效；429→限速；其余/超时→连接失败。
pub fn pair(address: &str, code: &str) -> Result<String, String> {
    let origin = parse_address(address)?.1;
    let body = serde_json::json!({ "code": code }).to_string();
    let raw = http_post_json(&origin, "/__remote/pair", &body).ok_or("无法连接远程实例（超时或拒绝）")?;
    // 真机验收缺陷：网关（Node http 无 content-length）以 chunked 应答，先解码分块帧
    let raw = dechunk_response(&raw);
    let head = String::from_utf8_lossy(&raw);
    let status_line = head.lines().next().unwrap_or("");
    let Some(resp_code) = status_code(status_line) else {
        // 空应答/非 HTTP 应答（端口上跑的不是网关）：不把空串拼进「配对失败：」文案
        return Err("无法连接远程实例（无有效应答）".into());
    };
    if resp_code.starts_with("403") {
        return Err("配对码无效或已过期：请在远端 dsh 设置页重新生成配对链接".into());
    }
    if resp_code.starts_with("429") {
        return Err("配对尝试过于频繁，请稍后再试".into());
    }
    if !resp_code.starts_with('2') {
        return Err(format!("配对失败：{}", status_line));
    }
    // 2xx：body 从首个 \r\n\r\n 后取（小 JSON，一次读入足够）
    let json_start = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4)
        .unwrap_or(0);
    let value: serde_json::Value =
        serde_json::from_slice(&raw[json_start..]).map_err(|_| "网关应答不是合法 JSON")?;
    let token = value.get("token").and_then(|t| t.as_str()).unwrap_or("");
    if token.is_empty() {
        return Err("网关应答缺少 token".into());
    }
    Ok(token.to_string())
}

/// 状态行形如 `HTTP/1.1 200 OK` → 响应码（如 "200"）；空应答/形状不符返回 None。
/// （形状解析与 readiness.rs 的 status_ok 一致。）
fn status_code(status_line: &str) -> Option<&str> {
    let rest = status_line
        .strip_prefix("HTTP/1.0 ")
        .or_else(|| status_line.strip_prefix("HTTP/1.1 "))?;
    rest.split_whitespace().next()
}

/// 若应答带 `transfer-encoding: chunked`，在内存中把分块帧解码回原始 body
/// （`1f\r\n…31字节…\r\n1f\r\n…\r\n0\r\n\r\n` → `…62字节…`）。head 原样保留，
/// 状态码映射不受影响；非 chunked 应答原样返回。纯内存操作，读循环不变。
fn dechunk_response(raw: &[u8]) -> Vec<u8> {
    let Some(split) = raw.windows(4).position(|w| w == b"\r\n\r\n") else {
        return raw.to_vec();
    };
    let (head, body) = raw.split_at(split + 4);
    let head_lower = String::from_utf8_lossy(head).to_ascii_lowercase();
    let chunked = head_lower
        .lines()
        .any(|l| l.starts_with("transfer-encoding:") && l.contains("chunked"));
    if !chunked {
        return raw.to_vec();
    }
    let mut out = head.to_vec();
    let mut rest = body;
    loop {
        // 读一行块大小（到 \r\n；容忍 `1f;ext` 形式的 chunk 扩展；十六进制大小写均可）
        let Some(line_end) = rest.windows(2).position(|w| w == b"\r\n") else {
            break;
        };
        let line = String::from_utf8_lossy(&rest[..line_end]);
        let Ok(size) = usize::from_str_radix(line.trim().split(';').next().unwrap_or("").trim(), 16)
        else {
            break;
        };
        rest = &rest[line_end + 2..];
        if size == 0 || size > rest.len() {
            break; // 末块 0 → 到此为止（容忍尾随字节）；块长溢出按截断容错
        }
        out.extend_from_slice(&rest[..size]);
        rest = &rest[size..];
        rest = rest.strip_prefix(b"\r\n").unwrap_or(rest); // 块后 CRLF（末块可能缺失）
    }
    out
}

/// 极简 HTTP/1.1 POST JSON（Connection: close，读满或超时为止）。
fn http_post_json(origin: &str, path: &str, body: &str) -> Option<Vec<u8>> {
    let authority = origin.strip_prefix("http://")?;
    let (host, port) = authority.rsplit_once(':')?;
    let port: u16 = port.parse().ok()?;
    let mut stream = TcpStream::connect((host, port)).ok()?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(8)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(8)));
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: {authority}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /* ── 解析与凭据存取 ── */
    #[test]
    fn parses_bare_address_and_url_form() {
        let (addr, origin) = parse_address("192.168.1.146:3090").unwrap();
        assert_eq!(addr, "192.168.1.146:3090");
        assert_eq!(origin, "http://192.168.1.146:3090");
        // 带 scheme 也接受；https / 带路径 / 缺端口 拒绝
        assert_eq!(parse_address("http://10.0.0.2:8080").unwrap().0, "10.0.0.2:8080");
        assert!(parse_address("https://10.0.0.2:8080").is_err());
        assert!(parse_address("http://10.0.0.2:8080/foo").is_err());
        assert!(parse_address("10.0.0.2").is_err()); // 缺端口
        assert!(parse_address("host:abcd").is_err()); // 端口非数字
        assert!(parse_address("").is_err());
        assert!(parse_address("127.0.0.1:0").is_err()); // 端口 0 无意义
    }

    #[test]
    fn parses_pairing_link_into_address_and_code() {
        let (addr, code) = parse_pairing_link(
            "http://192.168.1.146:3090/__remote/pair?code=V0P7coA9FA5jgD86wdaIDg",
        )
        .unwrap();
        assert_eq!(addr, "192.168.1.146:3090");
        assert_eq!(code, "V0P7coA9FA5jgD86wdaIDg");
        // 前后带空白/整段文本中含链接也接受
        let (a2, c2) =
            parse_pairing_link("  请打开 http://10.1.2.3:3090/__remote/pair?code=xY-z_9 注册  ").unwrap();
        assert_eq!(a2, "10.1.2.3:3090");
        assert_eq!(c2, "xY-z_9");
        assert!(parse_pairing_link("http://10.1.2.3:3090/other?code=x").is_err());
        assert!(parse_pairing_link("随便一段话").is_err());
    }

    #[test]
    fn config_roundtrip_and_corrupt_tolerated() {
        let dir = std::env::temp_dir().join(format!(
            "dsh-remote-test-{}-{}",
            std::process::id(),
            crate::runtime::unix_now()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("remote.json");
        // 用可注入路径的内部函数测；损坏文件 → None
        std::fs::write(&path, "{broken").unwrap();
        assert!(load_config_detailed(&path, crypto_impl()).is_none());
        let cfg = RemoteConfig {
            address: "192.168.1.146:3090".into(),
            origin: "http://192.168.1.146:3090".into(),
            token: "tok-1".into(),
            paired_at: 12345,
        };
        // encrypt=false：与历史文件同形状的明文往返（加密路径由 seam 注入的专项测试覆盖）
        save_config_to(&path, &cfg, false).unwrap();
        let loaded = load_config_detailed(&path, crypto_impl()).unwrap().0;
        assert_eq!(loaded.token, "tok-1");
        assert_eq!(loaded.origin, cfg.origin);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /* ── token 落盘加密（DPAPI seam 注入；真 DPAPI 只做真机验收，测试恒注入假加解密） ── */
    /// 假加密器：Protect 加 `enc::` 前缀，Unprotect 去前缀；对含 `'` 的 token 也原样往返。
    fn fake_crypto(input: &str, dir: Direction) -> Option<String> {
        match dir {
            Direction::Protect => Some(format!("enc::{input}")),
            Direction::Unprotect => input.strip_prefix("enc::").map(str::to_string),
        }
    }

    /// 恒失败的加密器：注入「加密失败」与「解密失败」两种情形。
    fn failing_crypto(_input: &str, _dir: Direction) -> Option<String> {
        None
    }

    fn secret_config() -> RemoteConfig {
        RemoteConfig {
            address: "192.168.1.146:3090".into(),
            origin: "http://192.168.1.146:3090".into(),
            token: "tok-sec'ret".into(),
            paired_at: 42,
        }
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dsh-remote-enc-{tag}-{}-{}",
            std::process::id(),
            crate::runtime::unix_now()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn save_encrypted_writes_tokenenc_and_omits_plaintext_token() {
        let dir = temp_dir("save-enc");
        let path = dir.join("remote.json");
        save_config_with(&path, &secret_config(), true, fake_crypto).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("tokenEnc"), "应写入 tokenEnc 字段：{text}");
        assert!(text.contains("enc::"), "密文应来自注入的加密器：{text}");
        assert!(!text.contains("\"token\""), "明文 token 字段应省略：{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_plaintext_when_encrypt_disabled() {
        let dir = temp_dir("save-plain");
        let path = dir.join("remote.json");
        save_config_with(&path, &secret_config(), false, fake_crypto).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"token\":\"tok-sec'ret\""), "应为明文：{text}");
        assert!(!text.contains("tokenEnc"), "不应出现密文字段：{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn encrypt_failure_falls_back_to_plaintext() {
        // 便利优先：加密失败（PowerShell 出错等）→ 回退明文落盘，不丢凭据
        let dir = temp_dir("enc-fallback");
        let path = dir.join("remote.json");
        save_config_with(&path, &secret_config(), true, failing_crypto).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"token\":\"tok-sec'ret\""), "应回退明文：{text}");
        assert!(!text.contains("tokenEnc"), "不应留半截密文字段：{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_encrypted_restores_token_via_decryptor() {
        let dir = temp_dir("load-enc");
        let path = dir.join("remote.json");
        save_config_with(&path, &secret_config(), true, fake_crypto).unwrap();
        let (cfg, plaintext) = load_config_detailed(&path, fake_crypto).unwrap();
        assert_eq!(cfg.token, "tok-sec'ret");
        assert!(!plaintext, "tokenEnc 形状不算旧明文（不触发迁移）");
        assert_eq!(cfg.address, secret_config().address);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_corrupt_tokenenc_degrades_to_empty_token() {
        // 解密失败（换用户/换机/损毁）：配置照常载入但 token 为空 → 下游给出重新配对文案
        let dir = temp_dir("load-corrupt");
        let path = dir.join("remote.json");
        save_config_with(&path, &secret_config(), true, fake_crypto).unwrap();
        let (cfg, plaintext) = load_config_detailed(&path, failing_crypto).unwrap();
        assert!(cfg.token.is_empty());
        assert_eq!(cfg.address, secret_config().address, "其余字段不受影响");
        assert!(!plaintext);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_plaintext_file_loads_as_before() {
        // 旧版文件形状（无 tokenEnc）：原样读出，且标记为「旧明文」（供惰性迁移判断）
        let dir = temp_dir("load-legacy");
        let path = dir.join("remote.json");
        std::fs::write(
            &path,
            r#"{"address":"192.168.1.146:3090","origin":"http://192.168.1.146:3090","token":"tok-legacy","paired_at":7}"#,
        )
        .unwrap();
        let (cfg, plaintext) = load_config_detailed(&path, fake_crypto).unwrap();
        assert_eq!(cfg.token, "tok-legacy");
        assert!(plaintext, "旧形状应报告明文以触发惰性迁移");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dpapi_script_escapes_and_selects_direction() {
        // 单引号双写是 PS 单引号字符串唯一的转义；方向决定 Protect/Unprotect 形状
        let protect = dpapi_script("a'b", Direction::Protect);
        assert!(protect.contains("$p='a''b'"), "单引号须双写：{protect}");
        assert!(protect.contains("::Protect("));
        assert!(!protect.contains("::Unprotect("));
        let unprotect = dpapi_script("a'b", Direction::Unprotect);
        assert!(unprotect.contains("$p='a''b'"));
        assert!(unprotect.contains("::Unprotect("));
        assert!(unprotect.contains("FromBase64String"));
        // try/catch 兜底：失败走 `ERR: ` 前缀而非静默/非零码
        assert!(protect.contains("'ERR: '"));
        assert!(protect.contains("Add-Type -AssemblyName System.Security"));
    }

    /* ── pair()：对假网关的 canned 应答做状态解析 ── */
    /// 起一个一次性 HTTP 服务，读入请求后回一段固定字节即关闭（风格同 readiness.rs）。
    fn serve_once(response: impl Into<Vec<u8>>) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let response = response.into();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(&response);
            }
        });
        port
    }

    #[test]
    fn pair_extracts_token_from_ok_response() {
        let p = serve_once(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 30\r\nconnection: close\r\n\r\n{\"ok\":true,\"token\":\"tok-pair\"}",
        );
        let token = pair(&format!("127.0.0.1:{p}"), "any-code").unwrap();
        assert_eq!(token, "tok-pair");
    }

    #[test]
    fn pair_maps_403_to_invalid_code_error() {
        let p = serve_once("HTTP/1.1 403 Forbidden\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
        let err = pair(&format!("127.0.0.1:{p}"), "bad-code").unwrap_err();
        assert!(err.contains("配对码无效"));
    }

    #[test]
    fn pair_empty_response_maps_to_no_valid_answer() {
        // 端口上跑的不是网关：accept 后立即关闭，零字节应答 → 明确的「无有效应答」
        let p = serve_once("");
        let err = pair(&format!("127.0.0.1:{p}"), "any-code").unwrap_err();
        assert!(err.contains("无有效应答"));
    }

    #[test]
    fn pair_parses_chunked_ok_response() {
        // 真机验收缺陷：网关（Node http 无 content-length）以 chunked 传输编码应答，
        // body 分两块（块大小十六进制大小写各一），末尾 0\r\n\r\n。
        let body = r#"{"ok":true,"token":"tok-chunked-1","deviceId":"d1","name":"n"}"#;
        let (a, b) = body.split_at(body.len() / 2);
        let canned = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n{:X}\r\n{a}\r\n{:x}\r\n{b}\r\n0\r\n\r\n",
            a.len(),
            b.len()
        );
        let p = serve_once(canned);
        let token = pair(&format!("127.0.0.1:{p}"), "any-code").unwrap();
        assert_eq!(token, "tok-chunked-1");
    }

    #[test]
    fn pair_maps_403_with_chunked_encoding() {
        // 403 + chunked：状态映射读的是 head，不受分块影响 → 配对码无效
        let body = r#"{"ok":false,"error":"invalid code"}"#;
        let canned = format!(
            "HTTP/1.1 403 Forbidden\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n{:x}\r\n{body}\r\n0\r\n\r\n",
            body.len()
        );
        let p = serve_once(canned);
        let err = pair(&format!("127.0.0.1:{p}"), "bad-code").unwrap_err();
        assert!(err.contains("配对码无效"));
    }
}
