//! 本地回环反向代理：把 127.0.0.1:<随机端口> 上的请求转发到远程 dsh-remote 网关，
//! 自动注入 x-remote-token。webview 以代理 origin 加载远程页面——页面 hostname 为
//! 回环，dsh 视为本机浏览器（settings 可用）且天然安全上下文（crypto API 原生可用）。
//! 仅绑回环；token 只注入到已配对网关的流量。
//!
//! 并发结构：accept 循环一个任务；每条客户端连接一个任务。客户端连接内 keep-alive
//! 复用同一条上游连接；WS 升级连接转双向裸拼接。stop 经 Notify 关闭监听——活动
//! 连接按各自响应框架自然收尾（尽力而为：不断开进行中的转发，与浏览器连接池行为
//! 兼容；不再有新请求进入）。两处已注明的简化（chunked 请求兜底、chunked 结束帧
//! 扫描）见对应函数注释——dsh 前端与自家工具全部发 content-length，网关（Node
//! http）应答为 content-length 或无 trailer 的 chunked，均不受影响。

use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// 请求/响应头上限：读到 64KB 仍不见 `\r\n\r\n` 即断开（防恶意/异常客户端占内存）。
const HEAD_CAP: usize = 64 * 1024;

pub struct ProxyConfig {
    /// 网关 origin，如 http://192.168.1.146:3090
    pub origin: String,
    pub token: String,
}

#[derive(Clone)]
pub struct ProxyHandle {
    pub port: u16,
    stop: Arc<tokio::sync::Notify>,
}

/// 启动代理（后台任务）。返回实际监听端口；stop() 关闭监听并断开活动连接（尽力而为）。
pub async fn start(config: ProxyConfig) -> Result<ProxyHandle, String> {
    let gateway_authority = config
        .origin
        .strip_prefix("http://")
        .ok_or_else(|| format!("网关 origin 必须以 http:// 开头：{}", config.origin))?
        .trim_end_matches('/')
        .to_string();
    if gateway_authority.is_empty() {
        return Err("网关 origin 缺少 host:port".into());
    }
    // 只绑回环：代理是免鉴权的 token 注入点，绝不能暴露到外部接口
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| format!("代理监听失败：{e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("代理端口解析失败：{e}"))?
        .port();
    let stop = Arc::new(tokio::sync::Notify::new());
    let stop_flag = Arc::clone(&stop);
    let token = config.token;
    tokio::spawn(async move {
        loop {
            // notify_one 带许可记忆：stop() 在两轮 select 之间触发也不会丢
            tokio::select! {
                _ = stop_flag.notified() => break,
                accepted = listener.accept() => match accepted {
                    Ok((client, _)) => {
                        let authority = gateway_authority.clone();
                        let token = token.clone();
                        // 每条客户端连接一个任务；连接内自行维护 keep-alive 循环
                        tokio::spawn(handle_client(client, authority, token));
                    }
                    Err(err) => {
                        // 瞬时错误必须继续：Windows 上对端连接后未发数据即 RST
                        //（WebView2/Chromium 投机预连接家常便饭）会让 accept 以
                        // WSAECONNRESET 浮出——若因此退出，端口仍在但永远不再接受，
                        // 远程模式整个会话静默变砖。只有 stop 通知才收摊。
                        log_terminal(&format!("accept 出错（继续监听）：{err}"));
                        continue;
                    }
                },
            }
        }
    });
    Ok(ProxyHandle { port, stop })
}

impl ProxyHandle {
    /// 关闭代理：停止接受新连接；活动连接按各自框架自然收尾（尽力而为，见模块注释）。
    pub fn stop(&self) {
        self.stop.notify_one();
    }
}

/// 终态错误路径记一行日志（远程模式失联/异常断开的现场诊断用；正常流量零输出）。
/// 风格同 supervisor.rs：open_log_append 追加一行，日志不可写时静默。
fn log_terminal(msg: &str) {
    use std::io::Write;
    if let Some(mut log) = crate::runtime::open_log_append() {
        let _ = writeln!(log, "[代理] {msg}");
    }
}

/* ── 单客户端连接服务 ── */

/// keep-alive 循环：读请求头 → 改写 → 上游转发 → 响应按框架回传 → 下一请求。
/// 一条客户端连接复用一条上游连接（keep-alive ↔ keep-alive）。任何一步出错直接
/// 断开客户端，不合成应答（浏览器会重试）。
async fn handle_client(mut client: TcpStream, gateway_authority: String, token: String) {
    // 上游连接随客户端连接复用；None = 尚未建立 / 已被透传分支消耗
    let mut upstream_slot: Option<TcpStream> = None;
    loop {
        let Some((head, excess)) = read_head(&mut client).await else {
            return; // 对端先关 / 头超 64KB / 读错误
        };
        let rewritten = rewrite_request_head(&head, &gateway_authority, &token);
        let mut upstream = match upstream_slot.take() {
            Some(u) => u,
            None => match TcpStream::connect(&gateway_authority).await {
                Ok(u) => u,
                Err(err) => {
                    // 网关不可达：断开客户端（浏览器重试）；记一行供远程模式失联诊断
                    log_terminal(&format!("连接网关 {gateway_authority} 失败：{err}"));
                    return;
                }
            },
        };
        if upstream.write_all(&rewritten).await.is_err() {
            log_terminal("发送改写后请求头到网关失败（复用连接已死）");
            return; // 复用的上游连接已死：断开客户端，由其重建
        }

        // WS 升级：头与已读溢出字节透传后双向裸拼接，101 与全部帧原样过线
        if is_upgrade_request(&head) {
            if !excess.is_empty() && upstream.write_all(&excess).await.is_err() {
                return;
            }
            splice_both_ways(client, upstream).await;
            return; // 透传至连接结束，不复用
        }

        let (content_length, chunked_request) = request_framing(&head);
        if chunked_request {
            // 简化：chunked 请求体不解析块帧——已读溢出字节补发后，client→upstream
            // 剩余字节后台裸泵（块帧原样流过）；响应仍精确按框架转发，响应一完即收
            // 尾（不复用连接）。dsh 前端与自家工具全部发 content-length，此分支仅为
            // 异常客户端不挂死的兜底。
            if upstream.write_all(&excess).await.is_err() {
                return;
            }
            let (mut upstream_read, upstream_write) = upstream.into_split();
            let (client_read, mut client_write) = client.into_split();
            let pump_task =
                tokio::spawn(async move { pump(client_read, upstream_write).await });
            let _ = forward_response(&mut upstream_read, &mut client_write).await;
            let _ = client_write.shutdown().await;
            pump_task.abort();
            return;
        }

        // 请求体：content-length → 精确透传 N 字节（excess 里已读到的先行）
        let mut total = 0usize;
        if let Some(n) = content_length {
            let Ok(n) = usize::try_from(n) else { return }; // 超过地址空间：直接断
            total = n;
            let take = total.min(excess.len());
            if upstream.write_all(&excess[..take]).await.is_err() {
                return;
            }
            if copy_exact(&mut client, &mut upstream, (total - take) as u64).await.is_err() {
                return; // 客户端中途断开
            }
        }
        // 浏览器不流水线：excess 比 content-length 长（或 GET 带了尾巴）视为脏连接
        let pipelined_dirty = excess.len() > total;

        let completed = match forward_response(&mut upstream, &mut client).await {
            Ok(c) => c,
            Err(err) => {
                log_terminal(&format!("响应转发中断（上游/客户端断开）：{err}"));
                return; // 上游中断/写客户端失败：断开
            }
        };
        if !completed || pipelined_dirty || client_wants_close(&head) {
            return; // UntilClose 收尾 / 脏连接 / 客户端要求关闭
        }
        upstream_slot = Some(upstream); // keep-alive：循环处理同一连接的下一请求
    }
}

/* ── 纯函数：头解析与改写 ── */

/// 改写请求头：Host 换网关 authority、注入 x-remote-token；其余原样。
/// 输入为完整请求头字节（含结尾空行），输出同形。方法行原样保留。
/// 头名字匹配大小写不敏感；Host 缺失时补一条（HTTP/1.1 上游必需）。
fn rewrite_request_head(head: &[u8], gateway_authority: &str, token: &str) -> Vec<u8> {
    let mut lines = head_lines(head);
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop(); // 结尾空行由重组时统一补回
    }
    let mut rebuilt: Vec<Vec<u8>> = Vec::with_capacity(lines.len() + 2);
    let mut saw_host = false;
    let mut saw_token = false;
    for (i, line) in lines.iter().enumerate() {
        if i == 0 {
            rebuilt.push(line.to_vec()); // 请求行原样保留
            continue;
        }
        let name = header_name(line);
        if name.is_some_and(|n| n.eq_ignore_ascii_case(b"host")) {
            saw_host = true;
            rebuilt.push(format!("Host: {gateway_authority}").into_bytes());
        } else if name.is_some_and(|n| n.eq_ignore_ascii_case(b"x-remote-token")) {
            saw_token = true;
            rebuilt.push(format!("x-remote-token: {token}").into_bytes()); // 已有则替换（旧值丢弃）
        } else {
            rebuilt.push(line.to_vec()); // 其余（cookie/upgrade/content-*…）原样
        }
    }
    if !saw_host {
        rebuilt.insert(
            rebuilt.len().min(1),
            format!("Host: {gateway_authority}").into_bytes(),
        );
    }
    if !saw_token {
        rebuilt.push(format!("x-remote-token: {token}").into_bytes());
    }
    let mut out = rebuilt.join(&b"\r\n"[..]);
    out.extend_from_slice(b"\r\n\r\n");
    out
}

/// 响应体框架分类（供转发循环判断结束条件）。
#[derive(Debug, PartialEq, Eq)]
enum ResponseFraming {
    /// content-length: N → 头后精确转发 N 字节
    Length(u64),
    /// transfer-encoding: chunked → 原样透传直到结束帧 0\r\n\r\n
    Chunked,
    /// 两者皆无 → 读到上游关闭为止（该连接不可复用）
    UntilClose,
}

/// 响应体框架分类：transfer-encoding: chunked 优先（RFC 7230：TE 与 CL 并存时以
/// chunked 为准）；头名字大小写不敏感。
fn classify_response_head(head: &[u8]) -> ResponseFraming {
    let mut length = None;
    let mut chunked = false;
    for line in head_lines(head).iter().skip(1) {
        let Some(name) = header_name(line) else { continue };
        let value = String::from_utf8_lossy(&line[name.len() + 1..]);
        if name.eq_ignore_ascii_case(b"transfer-encoding")
            && value.to_ascii_lowercase().contains("chunked")
        {
            chunked = true;
        } else if name.eq_ignore_ascii_case(b"content-length") {
            length = value.trim().parse().ok();
        }
    }
    if chunked {
        ResponseFraming::Chunked
    } else if let Some(n) = length {
        ResponseFraming::Length(n)
    } else {
        ResponseFraming::UntilClose
    }
}

/// 请求体框架：(content-length 值, 是否 chunked)。两者皆缺 → (None, false)＝无请求体
/// （dsh 前端与自家工具的 GET/POST-JSON 均带 content-length）。
fn request_framing(head: &[u8]) -> (Option<u64>, bool) {
    let mut length = None;
    let mut chunked = false;
    for line in head_lines(head).iter().skip(1) {
        let Some(name) = header_name(line) else { continue };
        let value = String::from_utf8_lossy(&line[name.len() + 1..]);
        if name.eq_ignore_ascii_case(b"content-length") {
            length = value.trim().parse().ok();
        } else if name.eq_ignore_ascii_case(b"transfer-encoding")
            && value.to_ascii_lowercase().contains("chunked")
        {
            chunked = true;
        }
    }
    (length, chunked)
}

/// 请求是否带 Upgrade 头（大小写不敏感）→ 走 WS 双向透传分支。
fn is_upgrade_request(head: &[u8]) -> bool {
    head_lines(head)
        .iter()
        .skip(1)
        .any(|line| header_name(line).is_some_and(|n| n.eq_ignore_ascii_case(b"upgrade")))
}

/// 客户端是否要求响应后关闭：Connection: close，或 HTTP/1.0 未声明 keep-alive。
fn client_wants_close(head: &[u8]) -> bool {
    let lines = head_lines(head);
    let is_http10 = lines
        .first()
        .is_some_and(|l| l.windows(8).any(|w| w == b"HTTP/1.0"));
    let mut connection: Option<String> = None;
    for line in lines.iter().skip(1) {
        let Some(name) = header_name(line) else { continue };
        if name.eq_ignore_ascii_case(b"connection") {
            connection = Some(String::from_utf8_lossy(&line[name.len() + 1..]).to_ascii_lowercase());
        }
    }
    match connection {
        Some(v) => {
            if v.split(',').any(|t| t.trim() == "close") {
                return true;
            }
            if v.split(',').any(|t| t.trim() == "keep-alive") {
                return false;
            }
            is_http10
        }
        None => is_http10, // HTTP/1.0 默认关闭；HTTP/1.1 默认保活
    }
}

/// 头字节按行切分（行不含 CRLF；首行是请求行/状态行）。结尾空行剥掉，
/// 重组函数负责统一补回；头名字匹配一律配合 header_name 大小写不敏感比较。
fn head_lines(head: &[u8]) -> Vec<&[u8]> {
    let mut body = head;
    if body.ends_with(b"\r\n\r\n") {
        body = &body[..body.len() - 2]; // 最后一个 CRLF 是「空行」本身
    }
    body.split(|&b| b == b'\n')
        .map(|l| if l.ends_with(b"\r") { &l[..l.len() - 1] } else { l })
        .collect()
}

/// 头行的名字部分（冒号前）；无冒号 → None（无法解析的残行，调用方原样透传）。
fn header_name(line: &[u8]) -> Option<&[u8]> {
    let colon = line.iter().position(|&b| b == b':')?;
    Some(&line[..colon])
}

/* ── 转发与拼接 ── */

/// 从流读完整 HTTP 头（到 `\r\n\r\n`，含空行），64KB 上限。
/// 返回 (完整头, 头之后已多读的字节)——多读字节属于请求体/WS 帧/响应体，调用方续用。
/// 超限、对端先关或读错误 → None（调用方直接断开）。
async fn read_head<S>(stream: &mut S) -> Option<(Vec<u8>, Vec<u8>)>
where
    S: AsyncRead + Unpin,
{
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let rest = buf.split_off(end + 4);
            return Some((buf, rest));
        }
        if buf.len() > HEAD_CAP {
            log_terminal(&format!("头超过 {}KB 上限，断开", HEAD_CAP / 1024));
            return None; // 找不到终止符还超限 → 断（找到的上面已返回）
        }
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// 读上游响应头并按框架转发（头 + 体）给客户端。
/// Ok(true) = 响应完整（Length 凑满 / chunked 到达结束帧，连接可复用）；
/// Ok(false) = UntilClose 收尾或上游提前断（连接不可复用）；Err = IO 错误。
async fn forward_response<R, W>(upstream: &mut R, client: &mut W) -> std::io::Result<bool>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let Some((head, excess)) = read_head(upstream).await else {
        return Ok(false); // 上游没给完整响应头
    };
    client.write_all(&head).await?;
    if !excess.is_empty() {
        client.write_all(&excess).await?;
    }
    match classify_response_head(&head) {
        ResponseFraming::Length(n) => {
            // 头后多读的 excess 已在上面全量透传（含上游谎报超发的部分——浏览器
            // 场景不存在谎报，取从简不回收）；此处只补足剩余差额
            let done = (excess.len() as u64).min(n);
            copy_exact(upstream, client, n - done).await?;
            Ok(true)
        }
        ResponseFraming::Chunked => copy_until_chunk_terminator(upstream, client).await,
        ResponseFraming::UntilClose => {
            // 无长度无分块：读到上游关闭为止（该连接不可复用）
            let mut buf = [0u8; 8192];
            loop {
                let n = upstream.read(&mut buf).await?;
                if n == 0 {
                    return Ok(false);
                }
                client.write_all(&buf[..n]).await?;
            }
        }
    }
}

/// 精确转发 n 字节（从 from 到 to）；对端提前 EOF → Err(UnexpectedEof)。
async fn copy_exact<R, W>(from: &mut R, to: &mut W, mut n: u64) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = [0u8; 8192];
    while n > 0 {
        let want = (n as usize).min(buf.len());
        let read = from.read(&mut buf[..want]).await?;
        if read == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        to.write_all(&buf[..read]).await?;
        n -= read as u64;
    }
    Ok(())
}

/// chunked 应答原样透传：扫到结束帧 `0\r\n\r\n` 为止，字节不动（客户端自行解块）。
/// 简化（已与 dsh 网关核实不触发）：不解析块大小、不处理 trailer——网关（Node
/// http）JSON 应答无 trailer，JSON 文本字符串内的换行均被转义，不会出现裸
/// `0\r\n\r\n` 误配；并假定网关不对 chunked 应答再叠 gzip 等压缩——二进制压缩流
/// 里可能撞出裸 `0\r\n\r\n` 导致提前截断。上游先于结束帧关闭 → Ok(false)
/// （按 UntilClose 收尾，不复用）。
async fn copy_until_chunk_terminator<R, W>(from: &mut R, to: &mut W) -> std::io::Result<bool>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    const TERM: &[u8] = b"0\r\n\r\n";
    let mut tail: Vec<u8> = Vec::new(); // 末尾最多留 TERM.len()-1 字节，防结束帧被拆半
    let mut buf = [0u8; 8192];
    loop {
        let n = from.read(&mut buf).await?;
        if n == 0 {
            return Ok(false);
        }
        tail.extend_from_slice(&buf[..n]);
        if let Some(pos) = tail.windows(TERM.len()).position(|w| w == TERM) {
            to.write_all(&tail[..pos + TERM.len()]).await?;
            return Ok(true);
        }
        let flush_at = tail.len() - (tail.len()).min(TERM.len() - 1);
        to.write_all(&tail[..flush_at]).await?;
        tail.drain(..flush_at);
    }
}

/// 双向裸拼接：两个方向各一个任务，直到任一侧关闭/出错 → 整条断开。
/// 不解析任何应答（101 与全部 WS 帧原样过线）。
async fn splice_both_ways(client: TcpStream, upstream: TcpStream) {
    let (client_read, client_write) = client.into_split();
    let (upstream_read, upstream_write) = upstream.into_split();
    let to_upstream = tokio::spawn(async move { pump(client_read, upstream_write).await });
    let to_client = tokio::spawn(async move { pump(upstream_read, client_write).await });
    let _ = to_upstream.await;
    let _ = to_client.await;
}

/// 单向搬运直到读侧 EOF/出错；随后关写侧（对端读到 EOF，链式收尾）。
async fn pump<R, W>(mut r: R, mut w: W)
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = [0u8; 8192];
    loop {
        match r.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if w.write_all(&buf[..n]).await.is_err() {
                    break;
                }
            }
        }
    }
    let _ = w.shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::time::timeout;

    /// 上游观察记录：处理器把收到的头/体写进来，断言全部留在测试体内（任务内
    /// panic 会被吞掉导致假绿，一律不依赖）。
    type Seen = Arc<std::sync::Mutex<Vec<u8>>>;

    /* ── 测试工具：假上游 / 单请求收发 ── */

    /// 起一个单连接假上游：bind 127.0.0.1:0，返回端口；连接处理器在后台任务里跑。
    async fn fake_upstream<F, Fut>(handler: F) -> u16
    where
        F: FnOnce(TcpStream) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((sock, _)) = listener.accept().await {
                handler(sock).await;
            }
        });
        port
    }

    /// 对假上游起一个代理（origin 指向该上游）。
    async fn spawn_proxy(gateway_port: u16, token: &str) -> ProxyHandle {
        start(ProxyConfig {
            origin: format!("http://127.0.0.1:{gateway_port}"),
            token: token.to_string(),
        })
        .await
        .expect("启动代理失败")
    }

    /// 从流读完整 HTTP 头（到 \r\n\r\n）。返回 (头, 头后已多读的字节)——多读字节
    /// 可能是请求体/WS 帧开头，断言与续读都要用上。
    async fn read_head_part(stream: &mut TcpStream) -> (Vec<u8>, Vec<u8>) {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        let end = loop {
            if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break p + 4;
            }
            let n = stream.read(&mut chunk).await.expect("读头失败");
            assert!(n > 0, "对端在读到头前就关闭了");
            buf.extend_from_slice(&chunk[..n]);
        };
        let rest = buf.split_off(end);
        (buf, rest)
    }

    /// 单请求收发：连代理 → 发请求 → 半关写（对端读侧 EOF，收尾确定化）→ 读尽应答。
    async fn roundtrip(proxy_port: u16, request: &[u8]) -> Vec<u8> {
        let mut stream = TcpStream::connect(("127.0.0.1", proxy_port))
            .await
            .expect("连接代理失败");
        stream.write_all(request).await.unwrap();
        let _ = stream.shutdown().await;
        let mut out = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            let n = stream.read(&mut chunk).await.expect("读应答失败");
            if n == 0 {
                return out;
            }
            out.extend_from_slice(&chunk[..n]);
        }
    }

    /// 恰好读完一条响应（头 + content-length 体），不多读——保住 keep-alive 后续字节。
    async fn read_one_response(stream: &mut TcpStream) -> Vec<u8> {
        let (head, mut resp) = read_head_part(stream).await;
        let head_text = String::from_utf8_lossy(&head).to_ascii_lowercase();
        let len: usize = head_text
            .lines()
            .find_map(|l| l.strip_prefix("content-length:").map(|v| v.trim().parse().unwrap()))
            .unwrap_or(0);
        let mut chunk = [0u8; 1024];
        while resp.len() < len {
            let n = stream.read(&mut chunk).await.expect("读响应体失败");
            assert!(n > 0, "响应体被提前截断");
            resp.extend_from_slice(&chunk[..n]);
        }
        let mut full = head;
        full.extend_from_slice(&resp);
        full
    }

    /* ── 纯函数：请求头改写 ── */
    #[test]
    fn rewrite_replaces_host_and_injects_token() {
        let out = rewrite_request_head(
            b"GET /a?b=c HTTP/1.1\r\nhost: 127.0.0.1:4418\r\nCookie: sid=abc\r\nAccept: */*\r\n\r\n",
            "192.168.1.146:3090",
            "tok-1",
        );
        let s = String::from_utf8(out).unwrap();
        assert!(s.starts_with("GET /a?b=c HTTP/1.1\r\n"), "请求行被改动：{s}");
        assert!(s.contains("Host: 192.168.1.146:3090\r\n"), "Host 未改写：{s}");
        assert!(!s.contains("127.0.0.1:4418"), "旧 Host 残留：{s}");
        assert!(s.contains("Cookie: sid=abc\r\n"), "其他头被改动：{s}");
        assert!(s.contains("Accept: */*\r\n"));
        assert!(s.contains("x-remote-token: tok-1\r\n"), "token 未注入：{s}");
        assert!(s.ends_with("\r\n\r\n"), "结尾空行丢失：{s}");
    }

    #[test]
    fn rewrite_replaces_existing_token_and_keeps_other_headers_verbatim() {
        let out = rewrite_request_head(
            b"POST /api HTTP/1.1\r\nHost: page.host\r\nX-Remote-Token: stale\r\nUpgrade: WebSocket\r\nContent-Type: application/json\r\n\r\n",
            "10.0.0.9:8080",
            "fresh",
        );
        let s = String::from_utf8(out).unwrap();
        // 已有 token 被替换（不是叠加）
        assert_eq!(
            s.matches("x-remote-token").count() + s.matches("X-Remote-Token").count(),
            1,
            "token 应恰好一份：{s}"
        );
        assert!(s.contains("x-remote-token: fresh\r\n"));
        assert!(!s.contains("stale"));
        // 其余头（upgrade/content-type）与请求行原样
        assert!(s.contains("Upgrade: WebSocket\r\n"));
        assert!(s.contains("Content-Type: application/json\r\n"));
        assert!(s.starts_with("POST /api HTTP/1.1\r\n"));
        assert!(s.contains("Host: 10.0.0.9:8080\r\n"));
    }

    #[test]
    fn rewrite_injects_host_when_missing() {
        let out = rewrite_request_head(b"GET / HTTP/1.1\r\nUser-Agent: t\r\n\r\n", "10.0.0.9:8080", "tok");
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("Host: 10.0.0.9:8080\r\n"), "缺 Host 未补：{s}");
    }

    /* ── 纯函数：响应框架分类 ── */
    #[test]
    fn classify_response_head_picks_framing() {
        assert_eq!(
            classify_response_head(b"HTTP/1.1 200 OK\r\ncontent-length: 42\r\nx-ok: 1\r\n\r\n"),
            ResponseFraming::Length(42)
        );
        // 头名字大小写不敏感
        assert_eq!(
            classify_response_head(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: Chunked\r\n\r\n"),
            ResponseFraming::Chunked
        );
        assert_eq!(
            classify_response_head(b"HTTP/1.1 200 OK\r\nserver: node\r\n\r\n"),
            ResponseFraming::UntilClose
        );
    }

    /* ── 端到端：经代理转发 ── */
    #[tokio::test]
    async fn get_forwards_host_rewrite_and_token() {
        let seen: Seen = Arc::default();
        let upstream_port = fake_upstream({
            let seen = seen.clone();
            move |mut sock| async move {
                let (head, excess) = read_head_part(&mut sock).await;
                seen.lock().unwrap().extend_from_slice(&head);
                assert!(excess.is_empty(), "GET 不应带请求体");
                sock.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\nhello")
                    .await
                    .unwrap();
            }
        })
        .await;
        let proxy = spawn_proxy(upstream_port, "tok-e2e-get").await;
        let raw = roundtrip(
            proxy.port,
            b"GET /abc?q=1 HTTP/1.1\r\nHost: page.host\r\nX-Trace: t1\r\n\r\n",
        )
        .await;
        // 客户端拿到原样应答（头 + body）
        let text = String::from_utf8_lossy(&raw);
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"), "应答头异常：{text}");
        assert!(text.ends_with("hello"), "body 不完整：{text}");
        // 上游看到：Host 改写为自己的 authority、token 注入、其余头/请求行原样
        let got = String::from_utf8_lossy(&seen.lock().unwrap()).to_string();
        let got_lower = got.to_ascii_lowercase();
        assert!(
            got_lower.contains(&format!("host: 127.0.0.1:{upstream_port}")),
            "Host 未改写：{got}"
        );
        assert!(got_lower.contains("x-remote-token: tok-e2e-get"), "token 未注入：{got}");
        assert!(got.contains("X-Trace: t1"), "其他头被改动：{got}");
        assert!(got.starts_with("GET /abc?q=1 HTTP/1.1\r\n"), "请求行被改动：{got}");
    }

    #[tokio::test]
    async fn post_body_passes_through() {
        let seen: Seen = Arc::default();
        let upstream_port = fake_upstream({
            let seen = seen.clone();
            move |mut sock| async move {
                let (head, excess) = read_head_part(&mut sock).await;
                // content-length: 5 → 头后补齐恰好 5 字节体
                let mut body = excess;
                let mut chunk = [0u8; 512];
                while body.len() < 5 {
                    let n = sock.read(&mut chunk).await.unwrap();
                    assert!(n > 0, "请求体被提前截断");
                    body.extend_from_slice(&chunk[..n]);
                }
                seen.lock().unwrap().extend_from_slice(&head);
                seen.lock().unwrap().extend_from_slice(b"|BODY|");
                seen.lock().unwrap().extend_from_slice(&body[..5]);
                sock.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\nworld")
                    .await
                    .unwrap();
            }
        })
        .await;
        let proxy = spawn_proxy(upstream_port, "tok-post").await;
        let raw = roundtrip(
            proxy.port,
            b"POST /api HTTP/1.1\r\nHost: page.host\r\nContent-Type: application/json\r\nContent-Length: 5\r\n\r\nhello",
        )
        .await;
        assert!(String::from_utf8_lossy(&raw).ends_with("world"), "响应未回传");
        let got = String::from_utf8_lossy(&seen.lock().unwrap()).to_string();
        let (head, body) = got.split_once("|BODY|").expect("上游未记录到请求体");
        assert_eq!(body, "hello", "请求体不完整或不一致：{body}");
        assert!(head.to_ascii_lowercase().contains("content-length: 5"));
    }

    #[tokio::test]
    async fn chunked_response_passes_through() {
        let seen: Seen = Arc::default();
        let upstream_port = fake_upstream({
            let seen = seen.clone();
            move |mut sock| async move {
                let (head, _) = read_head_part(&mut sock).await;
                seen.lock().unwrap().extend_from_slice(&head);
                // 网关（Node http）风格：无 content-length，分两块的 chunked 应答
                sock.write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ntransfer-encoding: chunked\r\n\r\n4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n",
                )
                .await
                .unwrap();
            }
        })
        .await;
        let proxy = spawn_proxy(upstream_port, "tok-chunked").await;
        let raw = roundtrip(proxy.port, b"GET /p HTTP/1.1\r\nHost: page.host\r\n\r\n").await;
        let text = String::from_utf8_lossy(&raw);
        // 分块帧原样直达客户端（客户端自行解块）
        assert!(
            text.contains("4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n"),
            "chunk 帧未原样透传：{text}"
        );
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.contains("transfer-encoding: chunked"));
        // 解块后语义正确（Wiki + pedia = Wikipedia）
        assert!(text.contains("Wiki") && text.contains("pedia"));
    }

    #[tokio::test]
    async fn keep_alive_two_requests_one_connection() {
        let seen: Seen = Arc::default();
        let accepted = Arc::new(AtomicUsize::new(0));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_port = listener.local_addr().unwrap().port();
        tokio::spawn({
            let seen = seen.clone();
            let accepted = accepted.clone();
            async move {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                accepted.fetch_add(1, Ordering::SeqCst);
                for i in 0..2u8 {
                    let (head, excess) = read_head_part(&mut sock).await;
                    assert!(excess.is_empty(), "GET 不应带请求体");
                    seen.lock().unwrap().extend_from_slice(&head);
                    let body = format!("resp-{i}");
                    let resp =
                        format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{body}", body.len());
                    sock.write_all(resp.as_bytes()).await.unwrap();
                }
                // 两个响应发完即收（drop = 关闭；后续由客户端断开收尾）
            }
        });
        let proxy = spawn_proxy(upstream_port, "tok-ka").await;
        // 同一条客户端 TCP 连接先后发两个请求
        let mut c = TcpStream::connect(("127.0.0.1", proxy.port)).await.unwrap();
        c.write_all(b"GET /one HTTP/1.1\r\nHost: page.host\r\n\r\n")
            .await
            .unwrap();
        let r1 = read_one_response(&mut c).await;
        assert!(r1.ends_with(b"resp-0"), "第一个响应不完整");
        c.write_all(b"GET /two HTTP/1.1\r\nHost: page.host\r\n\r\n")
            .await
            .unwrap();
        let r2 = read_one_response(&mut c).await;
        assert!(r2.ends_with(b"resp-1"), "第二个响应不完整");
        // 连接被复用：上游只接受了一条连接；两次请求 Host 均被改写
        assert_eq!(accepted.load(Ordering::SeqCst), 1, "上游应只接受一条连接");
        let got = String::from_utf8_lossy(&seen.lock().unwrap()).to_string();
        assert_eq!(
            got.to_ascii_lowercase()
                .matches(&format!("host: 127.0.0.1:{upstream_port}"))
                .count(),
            2,
            "两次请求的 Host 都应改写：{got}"
        );
        assert_eq!(got.matches("GET /one").count() + got.matches("GET /two").count(), 2);
    }

    #[tokio::test]
    async fn websocket_upgrade_splices_both_directions() {
        let seen: Seen = Arc::default();
        let upstream_port = fake_upstream({
            let seen = seen.clone();
            move |mut sock| async move {
                let (head, _) = read_head_part(&mut sock).await;
                seen.lock().unwrap().extend_from_slice(&head);
                sock.write_all(b"HTTP/1.1 101 Switching Protocols\r\nupgrade: websocket\r\n\r\n")
                    .await
                    .unwrap();
                // 逐行回显两帧（各 7 字节，定长读保证确定性）
                for _ in 0..2 {
                    let mut frame = [0u8; 7];
                    sock.read_exact(&mut frame).await.unwrap();
                    seen.lock().unwrap().extend_from_slice(&frame);
                    sock.write_all(&frame).await.unwrap();
                }
            }
        })
        .await;
        let proxy = spawn_proxy(upstream_port, "tok-ws").await;
        let mut c = TcpStream::connect(("127.0.0.1", proxy.port)).await.unwrap();
        c.write_all(
            b"GET /ws HTTP/1.1\r\nHost: page.host\r\nUpgrade: WebSocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n",
        )
        .await
        .unwrap();
        // 101 原样直达
        let (resp_head, excess) = read_head_part(&mut c).await;
        assert!(excess.is_empty());
        assert!(
            String::from_utf8_lossy(&resp_head).starts_with("HTTP/1.1 101"),
            "未收到 101：{}",
            String::from_utf8_lossy(&resp_head)
        );
        // 双向各一来回
        for (i, payload) in [&b"ping-a\n"[..], &b"ping-b\n"[..]].into_iter().enumerate() {
            c.write_all(payload).await.unwrap();
            let mut echo = [0u8; 7];
            timeout(Duration::from_secs(5), c.read_exact(&mut echo))
                .await
                .expect("回显超时")
                .unwrap();
            assert_eq!(&echo, payload, "第 {i} 个回显不符");
        }
        // 上游看到的头：Host 改写、Upgrade 保留、token 注入；两帧均到达
        let got = String::from_utf8_lossy(&seen.lock().unwrap()).to_string();
        let got_lower = got.to_ascii_lowercase();
        assert!(got_lower.contains(&format!("host: 127.0.0.1:{upstream_port}")), "Host 未改写：{got}");
        assert!(got.contains("Upgrade: WebSocket"), "Upgrade 头被改动：{got}");
        assert!(got_lower.contains("x-remote-token: tok-ws"), "token 未注入：{got}");
        assert!(got.ends_with("ping-a\nping-b\n"), "两帧未全部到达上游：{got}");
    }

    #[tokio::test]
    async fn oversized_head_closes_connection() {
        // 头超限在读阶段就该断，永远走不到连上游这步
        let gate = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = spawn_proxy(gate.local_addr().unwrap().port(), "t").await;
        let mut c = TcpStream::connect(("127.0.0.1", proxy.port)).await.unwrap();
        let big = vec![b'a'; 64 * 1024 + 1];
        let _ = c.write_all(&big).await; // 代理中途关闭时写侧可能报错，忽略
        let mut buf = [0u8; 8];
        let closed = timeout(Duration::from_secs(5), async {
            loop {
                match c.read(&mut buf).await {
                    Ok(0) | Err(_) => return true,
                    Ok(_) => continue,
                }
            }
        })
        .await;
        assert!(matches!(closed, Ok(true)), "超长头未导致连接被关闭（超时挂起）");
    }

    #[tokio::test]
    async fn upstream_unreachable_closes_client_promptly() {
        // 占住一个端口再释放：得到一个确定无人监听的端口（比写死端口可靠）
        let dead = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead_port = dead.local_addr().unwrap().port();
        drop(dead);
        let proxy = spawn_proxy(dead_port, "t").await;
        let mut c = TcpStream::connect(("127.0.0.1", proxy.port)).await.unwrap();
        c.write_all(b"GET / HTTP/1.1\r\nHost: page.host\r\n\r\n")
            .await
            .unwrap();
        // 网关连不上 → 代理干净断开（EOF 或 RST 都算），绝不挂起；不发合成应答
        let mut buf = [0u8; 8];
        let closed = timeout(Duration::from_secs(5), c.read(&mut buf)).await;
        assert!(
            matches!(closed, Ok(Ok(0)) | Ok(Err(_))),
            "上游不可达时应立即断开客户端（超时挂起：{closed:?}）"
        );
    }

    #[tokio::test]
    async fn stop_closes_listener() {
        let gate = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = spawn_proxy(gate.local_addr().unwrap().port(), "t").await;
        proxy.stop();
        // 端口最终不再可连（监听被关闭）；轮询等待避免时序脆弱
        for _ in 0..200 {
            if TcpStream::connect(("127.0.0.1", proxy.port)).await.is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("stop 后端口仍可连接");
    }
}
