# dsh-desktop 远程连接（阶段2）实施计划

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** dsh-desktop（v0.1.16）作为客户端连接远程 dsh-remote 网关：配对换 token → 导航守卫放行远程 origin（经 `GET /__remote/pair?token=` 种 cookie）→ 事件流带 `x-remote-token` → 托盘/加载页本地远程互切。

**Architecture:** 直连导航（路线 A）。webview 只受导航守卫约束（复用 `origin` allowlist）；capability `http://*:*` 仅为已加载页面提供窗口控制 IPC；壳侧凭据（remote.json）驱动就绪探测/事件流/cookie 种植。设计文档：`docs/plans/2026-08-29-remote-connect-design.md`。

**Tech Stack:** Tauri 2 + Rust（零新依赖：HTTP 用 events.rs 同款裸 TcpStream 模式，WS 头注入用 tungstenite IntoClientRequest）。

**参考实现（动手前先读）：**
- `src-tauri/src/main.rs` — AppState、caller_is_local、命令注册、启动序列
- `src-tauri/src/supervisor.rs` — start_service/restart_service/watch_child、parse_web_url 测试风格
- `src-tauri/src/events.rs` — spawn/exchange_cookie/connect_remote(头注入范例)/http_request/tests
- `src-tauri/src/readiness.rs` — http_ok/wait_http_ok 及其测试
- `src-tauri/src/webview.rs` — same_origin、TITLEBAR_INSET_CSS、navigate_to_harness
- `src-tauri/src/tray.rs` — build_tray（按便携模式动态构建菜单的先例）
- `src-tauri/src/status.rs` — StartupStatus、update/set/fail/wizard
- `src-tauri/src/runtime.rs` — runtime_root()、pid_file()、open_log_append

**约定：**
- 工作目录 `F:/Development/workspace/dsh/dsh-desktop`（git 仓库，main 分支）；提交信息中文，风格随仓库（最终版本提交 `0.1.16：<描述>`，过程提交 `remote: <描述>`）。
- Rust 测试：`cd src-tauri && cargo test`；编译：`cargo build`；冒烟：`cargo run -- --quit-after-secs 60`（本地模式路径必须保持不回归）。
- Task 1 在 **dsh-remote 仓库**（`F:/Development/workspace/dsh/dsh-remote`）执行，是契约前置。
- 每个任务结束即提交。

---

## Task 1（dsh-remote 仓库）：网关支持 `GET /__remote/pair?token=` 种浏览器 cookie

**背景**：桌面壳用 POST 配对消费一次性 code 拿 token 后，webview 没有网关 cookie，导航远程 origin 会被 401。补一个"持有 token 即凭证"的浏览器流入口（与 dsh v0.1.2 一次性 token 心智一致）：`GET /__remote/pair?token=<已有设备令牌>` → 校验 token → 303 + Set-Cookie（同 token 值）。**不创建新设备**（token 已对应存在设备），但 touch 该设备。

**Files:**
- Modify: `F:/Development/workspace/dsh/dsh-remote/src/gateway.ts`（handlePair）
- Modify: `F:/Development/workspace/dsh/dsh-remote/src/index.ts`（无改动——生成配对链接不受影响）
- Test: `F:/Development/workspace/dsh/dsh-remote/tests/gateway.test.ts`

**Step 1: 失败测试**（加入 gateway.test.ts）

```ts
it('GET /__remote/pair?token=<已有设备令牌> → 303 + Set-Cookie 同值，不新建设备', async () => {
  // 先 POST 配对得到合法 token 与设备
  const first = await fetch(`${base}/__remote/pair`, {
    method: 'POST', headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ code: await freshCode() }),
  })
  const { token, deviceId } = (await first.json()) as { token: string; deviceId: string }
  const before = CURRENT_STORE.list().length

  const res = await fetch(`${base}/__remote/pair?token=${encodeURIComponent(token)}`, { redirect: 'manual' })
  expect(res.status).toBe(303)
  const cookie = res.headers.get('set-cookie') ?? ''
  expect(cookie).toContain(`${REMOTE_COOKIE}=${token}`)
  expect(cookie).toContain('HttpOnly')
  expect(CURRENT_STORE.list().length).toBe(before) // 不新建
  expect(CURRENT_STORE.verify(token)?.id).toBe(deviceId) // touch 原设备

  // 无效 token → 403（与错误码同一失败分支，计入 pairLimiter）
  const bad = await fetch(`${base}/__remote/pair?token=${encodeURIComponent('nope')}`, { redirect: 'manual' })
  expect(bad.status).toBe(403)
})
```

**Step 2:** `cd F:/Development/workspace/dsh/dsh-remote && npx vitest run tests/gateway.test.ts` → 新用例 FAIL（token 参数被当 code 消费失败返回 403 但无 Set-Cookie）。

**Step 3: 实现**（handlePair 内，`pairings.consume(code)` 失败分支之前增加 token 支路）：

```ts
// 桌面壳等原生客户端已持 token（POST 配对所得）：GET ?token= 等价“种浏览器 cookie”，
// 不新建设备、仅 touch；无效 token 走与错误码相同的 403 分支（计入限速）
const tokenParam = url.searchParams.get('token')
if (!wantsJson && tokenParam !== null && tokenParam !== '') {
  const device = store.verify(tokenParam)
  if (device === undefined) {
    log(`配对失败（token 无效）来自 ${clientKey(req)}`)
    deny(res, 403, '凭证无效或已被吊销：请在 dsh 设置页重新生成配对链接。')
    return
  }
  store.touch(device.id, now())
  res.writeHead(303, {
    location: '/',
    'set-cookie': `${REMOTE_COOKIE}=${tokenParam}; HttpOnly; SameSite=Lax; Path=/; Max-Age=31536000`,
    'cache-control': 'no-store',
  })
  res.end()
  return
}
```

**Step 4:** 全量 `npm test`（41 预期）+ `npm run typecheck` + `npm run build`。

**Step 5:** 版本号 `package.json` → `0.1.1`，`git add -A && git commit -m "0.1.1：网关支持 GET /__remote/pair?token= 种浏览器 cookie（桌面壳对接）"`，`git push`。

> 注：服务器部署的升级在最后真机验收时进行（git pull 后重装插件），此处只出代码。

---

## Task 2（dsh-desktop）：remote.rs — 解析与凭据存取（TDD）

**Files:**
- Create: `src-tauri/src/remote.rs`
- Modify: `src-tauri/src/main.rs`（`mod remote;` 一行，挂到 mod 列表）

**Step 1: 失败测试**（remote.rs 底部 `#[cfg(test)]`，风格随 readiness.rs/supervisor.rs）

```rust
use super::*;

#[test]
fn parses_bare_address_and_url_form() {
    let (addr, origin) = parse_address("192.168.1.146:3090").unwrap();
    assert_eq!(addr, "192.168.1.146:3090");
    assert_eq!(origin, "http://192.168.1.146:3090");
    // 带 scheme 也接受；https / 带路径 / 缺端口 拒绝
    assert_eq!(parse_address("http://10.0.0.2:8080").unwrap().0, "10.0.0.2:8080");
    assert!(parse_address("https://10.0.0.2:8080").is_err());
    assert!(parse_address("http://10.0.0.2:8080/foo").is_err());
    assert!(parse_address("10.0.0.2").is_err());        // 缺端口
    assert!(parse_address("host:abcd").is_err());       // 端口非数字
    assert!(parse_address("").is_err());
    assert!(parse_address("127.0.0.1:0").is_err());     // 端口 0 无意义
}

#[test]
fn parses_pairing_link_into_address_and_code() {
    let (addr, code) = parse_pairing_link(
        "http://192.168.1.146:3090/__remote/pair?code=V0P7coA9FA5jgD86wdaIDg",
    ).unwrap();
    assert_eq!(addr, "192.168.1.146:3090");
    assert_eq!(code, "V0P7coA9FA5jgD86wdaIDg");
    // 前后带空白/整段文本中含链接也接受
    let (a2, c2) = parse_pairing_link("  请打开 http://10.1.2.3:3090/__remote/pair?code=xY-z_9 注册  ").unwrap();
    assert_eq!(a2, "10.1.2.3:3090");
    assert_eq!(c2, "xY-z_9");
    assert!(parse_pairing_link("http://10.1.2.3:3090/other?code=x").is_err());
    assert!(parse_pairing_link("随便一段话").is_err());
}

#[test]
fn config_roundtrip_and_corrupt_tolerated() {
    let dir = std::env::temp_dir().join(format!("dsh-remote-test-{}-{}", std::process::id(), std::time::unix_now()));
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
```

**Step 2:** `cd src-tauri && cargo test remote` → FAIL（模块不存在——先建空文件 + `mod remote;` 后测试编译失败即 TDD 红）。

**Step 3: 实现** `src-tauri/src/remote.rs`：

```rust
//! 远程实例凭据与配对（对接 dsh-remote 网关，契约见其 README）：
//! - POST /__remote/pair {code} → {ok,token,…}；403 码无效；429 限速
//! - GET  /__remote/pair?token=<token> → 303 + Set-Cookie（给 webview 种凭证）
//! 凭据明文落盘 remote.json（与 dsh 宿主会话密钥同威胁模型，README 声明）。
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
pub struct RemoteConfig {
    pub address: String,   // 裸 host:port
    pub origin: String,    // http://host:port
    pub token: String,
    pub paired_at: u64,
}

pub fn config_path() -> std::path::PathBuf {
    crate::runtime::runtime_root().join("remote.json")
}

/// 上次模式（"local"/"remote"）：与凭据独立，断开远程不清凭据。
pub fn last_mode() -> &'static str { "mode.txt" }

pub fn load_mode() -> &'static str {
    match std::fs::read_to_string(crate::runtime::runtime_root().join(last_mode())) {
        Ok(s) if s.trim() == "remote" => "remote",
        _ => "local",
    }
}

pub fn save_mode(mode: &str) {
    let _ = std::fs::write(crate::runtime::runtime_root().join(last_mode()), mode);
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
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string(cfg).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

/// 解析地址输入：裸 `host:port` 或 `http://host:port`；只支持 http、端口必填。
/// 返回 (address, origin)。
pub fn parse_address(input: &str) -> Result<(String, String), String> {
    let s = input.trim();
    let s = s.strip_prefix("http://").ok_or_else(|| "只支持 http:// 地址（网关不带 TLS）".to_string())?;
    if s.contains('/') || s.contains('?') || s.contains('#') {
        return Err("地址只能是 host:port，不含路径".into());
    }
    let (host, port) = s.rsplit_once(':').ok_or("地址缺少端口（如 192.168.1.146:3090）")?;
    let port: u16 = port.parse().map_err(|_| "端口必须是数字".to_string())?;
    if port == 0 || host.is_empty() || host.contains(':') {
        return Err("地址无效".into());
    }
    Ok((s.to_string(), format!("http://{s}")))
}

/// 从粘贴文本中提取配对链接的 (address, code)。
pub fn parse_pairing_link(text: &str) -> Result<(String, String), String> {
    let marker = "/__remote/pair?code=";
    let start = text.find("http://").ok_or("未找到配对链接（应以 http:// 开头或包含）")?;
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
    let head = String::from_utf8_lossy(&raw);
    let status_line = head.lines().next().unwrap_or("");
    if status_line.contains(" 403") {
        return Err("配对码无效或已过期：请在远端 dsh 设置页重新生成配对链接".into());
    }
    if status_line.contains(" 429") {
        return Err("配对尝试过于频繁，请稍后再试".into());
    }
    if !status_line.contains(" 2") && !status_line.contains(" 200") {
        return Err(format!("配对失败：{}", status_line));
    }
    // body 从首个 \r\n\r\n 后取（小 JSON，一次读入足够）
    let json_start = raw.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4).unwrap_or(0);
    let value: serde_json::Value = serde_json::from_slice(&raw[json_start..])
        .map_err(|_| "网关应答不是合法 JSON")?;
    let token = value.get("token").and_then(|t| t.as_str()).unwrap_or("");
    if token.is_empty() {
        return Err("网关应答缺少 token".into());
    }
    Ok(token.to_string())
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
```

依赖检查：Cargo.toml 已有 `serde`（events.rs 用 `serde_json`，status.rs derive `serde::Serialize`）；若 `serde` 的 `derive` feature 未开，在 Cargo.toml `serde = { version = "1", features = ["derive"] }` 调整（以编译为准，不加新 crate）。

**Step 4:** `cargo test remote` PASS；`cargo test` 全量不回归。

**Step 5:** `git add -A && git commit -m "remote: 远程凭据与解析模块（地址/配对链接/remote.json/配对调用）"`

---

## Task 3（dsh-desktop）：readiness.rs — 带凭证探活（TDD）

**Files:**
- Modify: `src-tauri/src/readiness.rs`

**Step 1: 失败测试**（readiness.rs tests 追加）

```rust
#[test]
fn forwards_extra_header_and_accepts_200() {
    // 服务端校验 x-remote-token 头必须等于预期值才回 200，否则 401
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = [0u8; 1024];
        let n = std::io::Read::read(&mut stream, &mut buf).unwrap();
        let head = String::from_utf8_lossy(&buf[..n]).to_string();
        let ok = head.contains("x-remote-token: tok-abc");
        let _ = stream.write_all(if ok {
            b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n".as_slice()
        } else {
            b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\n\r\n".as_slice()
        });
    });
    assert!(http_ok_hdr(&format!("http://127.0.0.1:{port}/"), Some("tok-abc")));
    let listener2 = TcpListener::bind("127.0.0.1:0").unwrap();
    let port2 = listener2.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let (mut stream, _) = listener2.accept().unwrap();
        let mut buf = [0u8; 1024];
        let _ = std::io::Read::read(&mut stream, &mut buf);
        let _ = stream.write_all(b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\n\r\n");
    });
    assert!(!http_ok_hdr(&format!("http://127.0.0.1:{port2}/"), Some("wrong")));
}

#[test]
fn wait_http_ok_hdr_times_out_without_match() {
    assert!(!wait_http_ok_hdr("http://127.0.0.1:1/", Some("t"), Duration::from_millis(200)));
}
```

**Step 2:** FAIL（函数不存在）。

**Step 3: 实现**——把 `http_ok` 的请求行构造抽出加头参数，保留旧签名作薄包装：

```rust
/// 同 http_ok，但可附加额外请求头（远程模式：x-remote-token 网关凭证）。
pub fn http_ok_hdr(url: &str, extra_header: Option<(&str, &str)>) -> bool {
    http_ok_inner(url, extra_header)
}
pub fn wait_http_ok_hdr(base: &str, token: Option<&str>, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if http_ok_hdr(base, token.map(|t| ("x-remote-token", t))) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    false
}
```

把现 `http_ok` 主体改名为 `http_ok_inner(url, extra_header: Option<(&str,&str)>)`，`req` 格式串中插入：

```rust
let extra_line = extra_header
    .map(|(k, v)| format!("{k}: {v}\r\n"))
    .unwrap_or_default();
// format!("GET {path} HTTP/1.1\r\nHost: {authority}\r\n{extra_line}Connection: close\r\n\r\n")
pub fn http_ok(url: &str) -> bool { http_ok_inner(url, None) }
```

**Step 4:** `cargo test readiness` PASS；全量不回归。

**Step 5:** `git add -A && git commit -m "remote: 就绪探测支持附加凭证头"`

---

## Task 4（dsh-desktop）：events.rs — 远程订阅带 token（TDD）

**Files:**
- Modify: `src-tauri/src/events.rs`

**要点**：
1. `pub fn spawn_remote(app: &tauri::AppHandle, base_url: &str, token: &str)`：世代号自增（复用 spawn 的逻辑）→ 线程跑 `run_remote(app, base_url.to_string(), token.to_string(), gen)`。
2. `run_remote`：**跳过 exchange_cookie**；直接 `connect_legacy_with_token(&base_url, token)`；成功 → `legacy_loop(...)`；失败只 log（远程模式断流不影响页面，服务切换时世代号接管）。
3. `connect_legacy_with_token`：按 `connect_remote` 的头注入范例构造请求：

```rust
fn connect_legacy_with_token(base_url: &str, token: &str) -> Result<WsStream, String> {
    let url = format!("{base_url}/api/events.mux").replace("http://", "ws://");
    let mut request = url.into_client_request().map_err(|e| format!("构造握手请求失败: {e}"))?;
    let value = tungstenite::http::HeaderValue::from_str(token).map_err(|e| format!("非法 token 头: {e}"))?;
    request.headers_mut().insert("x-remote-token", value);
    tungstenite::connect(request).map(|(s, _)| s).map_err(|e| e.to_string())
}
```

4. 本地路径（spawn/run）零改动。

**Step 1: 失败测试**（events.rs tests 追加——只测头构造，不起真 WS）：

```rust
#[test]
fn remote_handshake_request_carries_token_header() {
    let url = "ws://192.168.1.146:3090/api/events.mux";
    let mut request = url.into_client_request().unwrap();
    let value = tungstenite::http::HeaderValue::from_str("tok-1").unwrap();
    request.headers_mut().insert("x-remote-token", value);
    assert_eq!(request.headers().get("x-remote-token").unwrap(), "tok-1");
    // Host 仍按 URL 推导（网关按 Host 判定合法 authority）
    assert_eq!(request.headers().get("host").unwrap(), "192.168.1.146:3090");
}
```

**Step 2:** FAIL（缺函数/未用路径）→ **Step 3** 按上述实现 → **Step 4** `cargo test events` + 全量 PASS → **Step 5** `git add -A && git commit -m "remote: 事件流订阅支持 x-remote-token（远程模式）"`

---

## Task 5（dsh-desktop）：模式状态机与命令层（main.rs + supervisor.rs）

**Files:**
- Modify: `src-tauri/src/main.rs`、`src-tauri/src/supervisor.rs`、`src-tauri/src/status.rs`

**status.rs**：`StartupStatus` 增加两个字段 `remote: bool`（当前远程模式）、`connect: bool`（加载页显示连接屏）；`update/set/fail/wizard` 的 emit JSON 同步补字段；新增：

```rust
/// 进入远程连接屏状态。
pub fn connect_screen(app: &tauri::AppHandle, text: &str) { /* 同 wizard 模式：state.status = StartupStatus{ text, connect:true, …}; emit */ }
```

**main.rs**：
- `AppState` 增加 `mode: Mutex<&'static str>`（"local"/"remote"，初值随启动读取）。
- 启动序列分叉（setup 后台线程，在 persona 分支之后）：

```rust
if crate::remote::load_mode() == "remote" {
    match supervisor::connect_remote_flow(&handle) {
        Ok(()) => {}
        Err(err) => status::fail(&handle, &err),
    }
} else if let Err(err) = supervisor::start_service(&handle) {
    status::fail(&handle, &err);
}
```

- 新命令（全部 `caller_is_local` 门禁）：
  - `connect_remote(window, app, address: String, code: String) -> Result<(), String>`：`remote::parse_pairing_link` 先试（粘贴整链），失败则按「地址+码」分别 `parse_address`/原样 code → `remote::pair` → `remote::save(RemoteConfig{…, paired_at: unix_now_ms})` → `remote::save_mode("remote")` → `*state.mode="remote"` → 后台线程 `supervisor::connect_remote_flow`（错误转 fail 状态）→ Ok。输入 code 字段本身含链接时以链接为准。
  - `retry_connect(window, app)`：按当前模式重试（remote → connect_remote_flow；local → start_service）。
  - `show_connect(window, app)`：`status::connect_screen(&app, "连接远程实例")`。
  - `get_remote_address(window, app) -> String`：`remote::load().map(|c| c.address).unwrap_or_default()`（连接屏预填）。
  - `switch_to_local(window, app)`：`*state.origin=None`、`*state.mode="local"`、`remote::save_mode("local")` → `navigate_to_loader` → 后台 `start_service`。
  - `restart_service_cmd`（托盘重启用，见 tray 改动）：按 mode 分派 `connect_remote_flow` 或 `restart_service`。
- `generate_handler!` 注册全部新命令。

**supervisor.rs** 新增：

```rust
/// 远程连接序列：凭据探活 → origin 入放行表 → 事件流(带凭证) → 导航（经 pair?token= 种 cookie）。
pub fn connect_remote_flow(app: &tauri::AppHandle) -> Result<(), String> {
    let state: tauri::State<AppState> = app.state();
    {
        let mut r = state.restarting.lock().unwrap();
        if *r { return Ok(()); }
        *r = true;
    }
    let result = (|| -> Result<(), String> {
        let cfg = crate::remote::load().ok_or("尚未配对远程实例，请先输入地址与配对码")?;
        status::set(app, &format!("连接远程实例 {}…", cfg.address));
        if !crate::readiness::wait_http_ok_hdr(&format!("{}/", cfg.origin), Some(&cfg.token), Duration::from_secs(HEALTH_WAIT_SECS)) {
            return Err(format!("无法连接远程实例 {}（超时或凭证失效）", cfg.address));
        }
        *state.origin.lock().unwrap() = Some(cfg.origin.clone());
        crate::events::spawn_remote(app, &cfg.origin, &cfg.token);
        // 经 pair?token= 导航：网关 303 + Set-Cookie 种下 webview 凭证后落到 /（同 dsh 一次性 token 心智）
        webview::navigate_to_harness(app, &format!("{}/__remote/pair?token={}", cfg.origin, cfg.token));
        Ok(())
    })();
    *state.restarting.lock().unwrap() = false;
    result;
}
```

（注意：`restarting` 与本地启动互斥天然成立——同一把锁。`watch_child` 在远程模式 child=None 已空转，无需改。）

**Step:** 本任务以编译 + 既有测试不回归为主（状态机属集成行为，真机验收覆盖）：`cargo build` 零错误零新警告；`cargo test` 全量 PASS。**Commit:** `git add -A && git commit -m "remote: 模式状态机与命令层（connect_remote/switch_to_local/retry）"`

---

## Task 6（dsh-desktop）：导航样式与 capability（路线 A 收口）

**Files:**
- Modify: `src-tauri/capabilities/remote-harness.json`、`src-tauri/src/webview.rs`

1. `remote-harness.json`：`"remote": { "urls": ["http://*:*"] }`（描述同步改「已配对远程 Harness 页面」）。构建后确认 schema 接受该模式（`cargo build` 即校验；若 `http://*:*` 非法则改用 `["http://*"]` 并注释说明）。
2. `webview.rs` `TITLEBAR_INSET_CSS` 首行守卫：

```js
// 旧：if (location.protocol !== 'http:' || location.hostname !== '127.0.0.1' || location.port === '') return;
// 新：任意带端口的 http 页面都套顶栏让位样式——能加载的页面只有导航守卫放行的 harness origin（本地或远程）
if (location.protocol !== 'http:' || location.port === '') return;
```

3. 冒烟：`cargo run -- --quit-after-secs 60` 本地路径正常（窗口、加载页、服务拉起不回归）。
**Commit:** `git add -A && git commit -m "remote: capability 放宽至任意 http origin + 顶栏样式适配远程页面（边界仍在导航守卫）"`

---

## Task 7（dsh-desktop）：加载页连接屏 + 托盘动态菜单

**Files:**
- Modify: `ui/index.html`、`src-tauri/src/tray.rs`

**ui/index.html**：
- 新增 `#connect` 表单（复用 `#wizard` 的样式类，改 id）：字段 `远程地址`（name=remoteAddr，placeholder `192.168.1.146:3090`）、`配对码`（name=remoteCode，placeholder `粘贴配对链接或输入码`）、说明行（码从远端 dsh 设置页「远程访问」Tab 生成，10 分钟单次）、按钮「连接」+「取消」。
- `render(s)` 分支：`s.connect` → 显示 #connect（`get_remote_address` 预填）；`s.remote && !s.error` 时按钮文案体现远程语义。
- code 输入框 `input` 事件：值包含 `http://` 时客户端解析 URL 拆填 address+code（`new URL()` + `/__remote/pair?code=` 前缀判断）。
- 提交 → `invoke('connect_remote', { address, code })`，期间禁用按钮 + 状态文案「配对中…」；异常显示在 status 区。
- 错误态按钮扩展（#actions 之外新增 #remote-actions，按 `s.remote`/`s.connect` 显隐）：`重试`→`invoke('retry_connect')`；`修改远程配置`→`invoke('show_connect')`；`回到本地模式`→`invoke('switch_to_local')`。远程失败时隐藏「安装运行环境」（那是本地语义）。

**tray.rs**：
- `build_tray` 拆出 `menu_items(app, mode)`；远程模式：`重启连接`（原「重启服务」id 复用）、`断开远程，回到本地`（新 id "tolocal"）；本地模式：现有菜单 + `连接远程实例…`（新 id "connect"，调 `status::connect_screen` + 显示窗口 + `navigate_to_loader`?——若已在 harness 页，先回加载页再弹连接屏）。
- 新增 `pub fn rebuild(app)`：移除旧托盘（`app.remove_tray_by_id("dsh-tray")`，API 名以当前 tauri 版本为准）→ `build_tray_for_mode(app, mode)`；模式切换处（connect_remote 成功后 / switch_to_local）调用。菜单事件按 id 分派到命令同款逻辑（复用 supervisor 函数）。

**Step:** `cargo build` + `cargo run -- --quit-after-secs 60` 冒烟（本地模式托盘含「连接远程实例…」，点击回加载页出现连接屏——人工确认）；`cargo test` 全量 PASS。
**Commit:** `git add -A && git commit -m "remote: 加载页连接屏与托盘本地/远程互切"`

---

## Task 8（dsh-desktop）：版本收口 + README + CI 核验

1. `src-tauri/tauri.conf.json` 与 `src-tauri/Cargo.toml` 版本 → `0.1.16`。
2. README 增补「远程连接」章节：前置（远端部署 dsh-remote ≥0.1.1）、配对流程（含服务器无浏览器时 `curl -X POST http://127.0.0.1:3080/dsh-remote/api/pairing` 生成码）、模式记忆与托盘切换、安全边界（导航守卫/capability/凭据明文/仅 http/≥0.1.2 remote.mux-over-gateway 已知限制）、凭据文件位置。
3. `cargo build --release` 通过；`cargo test` 全绿；CI 工作流无需改动（确认 .github 现有 workflow 会跑 cargo test/build）。
4. `git add -A && git commit -m "0.1.16：远程连接——桌面壳作为 dsh-remote 网关客户端（配对/导航守卫/事件流/托盘互切）"`
5. **不**打 tag（发布由用户决定）。

---

## Task 9：真机验收（需用户配合：先把服务器上 dsh-remote 升级到 0.1.1）

服务器（192.168.1.146）升级：`cd` 到部署方式对应的目录拉取最新（git pull / 重装），重启 dsh。然后：

1. `cargo run`（或安装包）启动桌面壳 → 托盘「连接远程实例…」→ 粘贴服务器生成的配对链接（`curl -X POST http://127.0.0.1:3080/dsh-remote/api/pairing`）→ 连接。
2. 断言：进入完整远程 UI；顶栏按钮可用；远端跑一个回合 → 桌面原生通知弹出。
3. 重启桌面壳 → 按「上次模式」直连远程（不再要码）。
4. 托盘「断开远程，回到本地」→ 本地服务拉起进本地 UI；再「连接远程实例」→ 用已存凭据直接连（不用重配对）。
5. 服务器吊销该设备（管理 API 或设置页）→ 桌面刷新/操作 → 网关 401 → 错误态按钮出现且「回到本地模式」可用。
6. 断网（拔网线/断 Tailscale）→ 加载错误态；恢复后「重试」成功。

---

## 风险与实现注意

1. **pair?token= 的限速**：GET pair 与 POST pair 共用 pairLimiter——桌面重试风暴可能 429，connect_remote_flow 的错误文案已含「过于频繁」映射，验收时注意。
2. **webview cookie 种植依赖 303 跟随**：WebView2/WKWebView 对同源 303 + Set-Cookie 的处理与浏览器一致（navigate_to_harness 注释已论证原生导航的 cookie 语义），验收第 1 步专门覆盖。
3. **capability 模式合法性**：`http://*:*` 若 schema 拒绝，退 `http://*`（tauri URL pattern 对端口的通配以编译结果为准）。
4. **托盘重建 API**：`remove_tray_by_id` 在 tauri 2 的确切签名以 `cargo doc`/编译为准；若不存在则保留 TrayIcon 句柄调 `set_menu`。
5. **零新依赖**：HTTP 配对用裸 TcpStream（events.rs 同款）；WS 头注入 tungstenite 现有能力。
6. **本地路径零回归**：start_service/watch_child/现有冒烟是保护对象；Task 5/7 每步跑 `--quit-after-secs 60` 冒烟。
