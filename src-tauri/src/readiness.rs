//! 就绪探测：真实 HTTP GET（而非裸 TCP connect），只有服务真正应答才算就绪。
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// 对 url（可带路径与查询，如 http://127.0.0.1:4418/?token=…）发一次 GET。
/// 2xx（旧版直出 200）与 3xx（v0.1.2+ token 交换的 303 → /）都算服务就绪；
/// 4xx/5xx 不算——那说明服务起了但凭证不对（如 token 失效的 401），
/// 照常失败比静默放行更早暴露问题。Host 头必须是完整 authority
/// （127.0.0.1:port）：v0.1.2 的 cookie 按 authority 签发与校验。
pub fn http_ok(url: &str) -> bool {
    let rest = url.strip_prefix("http://").unwrap_or(url);
    let (authority, path) = match rest.split_once('/') {
        Some((authority, tail)) => (authority, format!("/{tail}")),
        None => (rest, "/".to_string()),
    };
    // Host 头与 connect 地址必须拆开：前者要完整 authority（v0.1.2 的 cookie 按
    // authority 签发校验），后者只能是不带端口的裸主机名（带端口的串不是合法地址）
    let Some((host, port)) = authority
        .rsplit_once(':')
        .and_then(|(h, p)| p.parse().ok().map(|p| (h, p)))
    else {
        return false;
    };
    let Ok(mut stream) = TcpStream::connect((host, port)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let req = format!("GET {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n");
    if stream.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let mut buf = [0u8; 128];
    let Ok(n) = stream.read(&mut buf) else {
        return false;
    };
    let head = String::from_utf8_lossy(&buf[..n]);
    status_ok(&head)
}

/// 状态码首字符为 2 或 3 即可（状态行形如 `HTTP/1.1 303 See Other`）。
fn status_ok(head: &str) -> bool {
    let code = head
        .strip_prefix("HTTP/1.0 ")
        .or_else(|| head.strip_prefix("HTTP/1.1 "));
    matches!(code.and_then(|c| c.as_bytes().first()), Some(b'2' | b'3'))
}

/// 轮询直到就绪或超时。
pub fn wait_http_ok(base: &str, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if http_ok(base) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// 起一个一次性 HTTP 服务，应答一个固定状态行后关闭。
    fn serve_once(response: &'static str) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = std::io::Read::read(&mut stream, &mut buf);
                let _ = stream.write_all(response.as_bytes());
            }
        });
        port
    }

    #[test]
    fn accepts_2xx_and_token_exchange_3xx_rejects_401() {
        // v0.1.2+：带 token 的 GET / 得到 303（token 交换重定向）
        let p = serve_once("HTTP/1.1 303 See Other\r\nlocation: /\r\nconnection: close\r\ncontent-length: 0\r\n\r\n");
        assert!(http_ok(&format!("http://127.0.0.1:{p}/?token=AbC-123")));
        // 旧版：无 token 直出 200
        let p = serve_once("HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-length: 2\r\n\r\nok");
        assert!(http_ok(&format!("http://127.0.0.1:{p}/")));
        // 凭证不对（token 失效 / 未带 token 撞上新版 401）不算就绪
        let p = serve_once("HTTP/1.1 401 Unauthorized\r\nconnection: close\r\ncontent-length: 0\r\n\r\n");
        assert!(!http_ok(&format!("http://127.0.0.1:{p}/")));
    }

    #[test]
    fn parses_path_and_query_into_request_line() {
        // serve 端回显请求行首部做校验：路径与查询必须原样到达
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let n = std::io::Read::read(&mut stream, &mut buf).unwrap();
            let head = String::from_utf8_lossy(&buf[..n]).to_string();
            let _ = stream.write_all(b"HTTP/1.1 303 See Other\r\ncontent-length: 0\r\n\r\n");
            head
        });
        assert!(http_ok(&format!("http://127.0.0.1:{port}/?token=AbC-123")));
        let request_line = handle.join().unwrap();
        let request_line = request_line.lines().next().unwrap();
        assert_eq!(request_line, "GET /?token=AbC-123 HTTP/1.1");
    }

    #[test]
    fn status_ok_recognizes_status_lines_only() {
        assert!(status_ok("HTTP/1.1 200 OK"));
        assert!(status_ok("HTTP/1.0 303 See Other"));
        assert!(status_ok("HTTP/1.1 401 Unauthorized") == false);
        assert!(status_ok("garbage") == false);
    }
}
