//! 远程实例凭据与配对（对接 dsh-remote 网关，契约见其 README）：
//! - POST /__remote/pair {code} → {ok,token,…}；403 码无效；429 限速
//! - GET  /__remote/pair?token=<token> → 303 + Set-Cookie（给 webview 种凭证）
//!
//! 凭据明文落盘 remote.json（与 dsh 宿主会话密钥同威胁模型，README 声明）。
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
pub struct RemoteConfig {
    pub address: String, // 裸 host:port
    pub origin: String,  // http://host:port
    pub token: String,
    pub paired_at: u64,
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

pub fn load() -> Option<RemoteConfig> {
    load_config_from(&config_path())
}

pub fn save(cfg: &RemoteConfig) -> Result<(), String> {
    save_config_to(&config_path(), cfg)
}

fn load_config_from(path: &std::path::Path) -> Option<RemoteConfig> {
    // 损坏/缺失一律 None（配对即覆盖，无需恢复语义）
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

fn save_config_to(path: &std::path::Path, cfg: &RemoteConfig) -> Result<(), String> {
    // tmp + rename：写一半崩溃不会留下半个 remote.json
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string(cfg).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
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
        assert!(load_config_from(&path).is_none());
        let cfg = RemoteConfig {
            address: "192.168.1.146:3090".into(),
            origin: "http://192.168.1.146:3090".into(),
            token: "tok-1".into(),
            paired_at: 12345,
        };
        save_config_to(&path, &cfg).unwrap();
        let loaded = load_config_from(&path).unwrap();
        assert_eq!(loaded.token, "tok-1");
        assert_eq!(loaded.origin, cfg.origin);
        let _ = std::fs::remove_dir_all(&dir);
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
