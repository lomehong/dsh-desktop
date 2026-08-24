//! 就绪探测：真实 HTTP GET（而非裸 TCP connect），只有服务真正应答 200 才算就绪。
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// 对 base（如 http://127.0.0.1:4418）发一次 GET /，返回是否得到 2xx。
pub fn http_ok(base: &str) -> bool {
    let rest = base.strip_prefix("http://").unwrap_or(base);
    let Some((host, port)) = rest.split_once(':') else {
        return false;
    };
    let Ok(port) = port.parse::<u16>() else {
        return false;
    };
    let Ok(mut stream) = TcpStream::connect((host, port)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let req = format!("GET / HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n\r\n");
    if stream.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let mut buf = [0u8; 128];
    let Ok(n) = stream.read(&mut buf) else {
        return false;
    };
    let head = String::from_utf8_lossy(&buf[..n]);
    head.starts_with("HTTP/1.1 2") || head.starts_with("HTTP/1.0 2")
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
