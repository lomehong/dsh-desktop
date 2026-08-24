//! 运行时路径发现与自举（只检测、不安装；安装由 scripts/pin-runtime.mjs 与首启引导负责）。
use std::path::PathBuf;
use std::process::Command;

/// 便携版运行时根目录（与 scripts/pin-runtime.mjs 一致）：
/// Windows: %LOCALAPPDATA%\dsh-desktop；macOS: ~/Library/Application Support/dsh-desktop
#[cfg(windows)]
pub fn runtime_root() -> PathBuf {
    let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
    PathBuf::from(local).join("dsh-desktop")
}

#[cfg(not(windows))]
pub fn runtime_root() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join("Library/Application Support/dsh-desktop")
}

pub fn node_exe() -> PathBuf {
    runtime_root().join("node").join(if cfg!(windows) { "node.exe" } else { "bin/node" })
}

pub fn dsh_bin_js() -> PathBuf {
    // Windows 便携版 npm -g 装到 node\node_modules；macOS 装到 node/lib/node_modules
    let mut p = runtime_root().join("node");
    if !cfg!(windows) {
        p = p.join("lib");
    }
    p.join("node_modules").join("@deepseek-ai").join("dsh").join("lib").join("bin.js")
}

pub fn log_file() -> PathBuf {
    runtime_root().join("dsh-desktop.log")
}

/// 进程登记文件：记录本壳进程与 dsh 子进程的 pid（含端口），
/// 供下次启动时识别「壳被强杀后残留的孤儿 dsh 进程树」。
pub fn pid_file() -> PathBuf {
    runtime_root().join("runtime.pid")
}

/// Windows：tasklist 查询指定 pid 的进程名（不存在返回 None）。
#[cfg(windows)]
pub fn process_name(pid: u32) -> Option<String> {
    let mut c = Command::new("tasklist.exe");
    c.args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"]);
    let out = no_window(&mut c).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        let mut fields = line.split(',').map(|f| f.trim_matches('"'));
        let name = fields.next()?.to_string();
        let pid: u32 = fields.next()?.parse().ok()?;
        if pid == pid {
            return Some(name);
        }
    }
    None
}

/// Unix：kill -0 探活 + /proc 读名（macOS 无 /proc 时仅探活返回空名）。
#[cfg(unix)]
pub fn process_name(pid: u32) -> Option<String> {
    let alive = Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !alive {
        return None;
    }
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|s| s.trim().to_string())
}

pub fn process_alive(pid: u32) -> bool {
    process_name(pid).is_some()
}

/// dsh 的启动方式：便携版运行时（node + bin.js）或系统 node + 全局 dsh 命令。
#[derive(Clone, Copy, PartialEq)]
pub enum Launch {
    Portable,
    System,
}

/// 检查命令是否可用（PATH 上能找到）。
#[cfg(windows)]
fn command_exists(name: &str) -> bool {
    let mut c = Command::new("where.exe");
    c.arg(name);
    no_window(&mut c).output().map(|o| o.status.success()).unwrap_or(false)
}

#[cfg(not(windows))]
fn command_exists(name: &str) -> bool {
    let mut c = Command::new("sh");
    c.args(["-c", &format!("command -v {name} >/dev/null 2>&1")]);
    c.status().map(|s| s.success()).unwrap_or(false)
}

/// Windows 下隐藏子进程的控制台窗口。
#[cfg(windows)]
pub fn no_window(cmd: &mut Command) -> &mut Command {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW)
}

#[cfg(not(windows))]
pub fn no_window(cmd: &mut Command) -> &mut Command {
    cmd
}

/// 只检测、不安装：node 与 dsh 都就绪才返回启动方式，否则给出安装指引。
pub fn bootstrap_runtime() -> Result<Launch, String> {
    let node_path = node_exe();
    let bin_path = dsh_bin_js();
    let portable = node_path.exists() && bin_path.exists();
    // 诊断日志：记录检测到的路径与结果，便于排查环境差异
    if let Some(log) = open_log_append() {
        use std::io::Write;
        let mut log = log;
        let _ = writeln!(
            log,
            "[检测] runtime_root={:?} node={:?} exists={} bin={:?} exists={} portable={}",
            runtime_root(),
            node_path,
            node_path.exists(),
            bin_path,
            bin_path.exists(),
            portable
        );
    }
    if portable {
        return Ok(Launch::Portable);
    }
    if !command_exists("node") {
        return Err("未检测到 Node.js。\n请点击下方「安装运行环境」，或先安装 Node.js（https://nodejs.org/）。".into());
    }
    if !command_exists("dsh") {
        return Err("未检测到 DSH。\n请点击下方「安装运行环境」，或先执行 npm install -g @deepseek-ai/dsh。".into());
    }
    Ok(Launch::System)
}

/// 以追加模式打开日志文件（目录不存在时创建；失败时静默返回 None，诊断日志不阻断主流程）。
pub fn open_log_append() -> Option<std::fs::File> {
    use std::io::Write;
    let path = log_file();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    match std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        Ok(mut f) => {
            let _ = writeln!(f, "\n===== DSH Desktop {} =====", unix_now());
            Some(f)
        }
        Err(_) => None,
    }
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
