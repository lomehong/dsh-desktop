//! 运行时路径发现与自举（只检测、不安装；安装由 scripts/pin-runtime.mjs 与首启引导负责）。
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

/// 便携模式标记：exe 同级的 `Data` 目录（U盘分发包自带）。
/// 存在即把全部运行时数据与 DSH home 收进该目录，绝不读写宿主机用户目录；
/// 所有路径相对 exe 现场解析，U盘换盘符/换目录均有效。
fn portable_root_locked() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let data = dir.join("Data");
    // 用运行时目录里的 node 判定「这是一个真的便携包」而不是碰巧叫 Data 的空目录：
    // 便携包制作器总是预装 runtime；空 Data 目录只可能是用户手工误建，按安装版处理
    // 会把宿主数据写进U盘，宁可忽略。
    if data.is_dir() && node_exe_in(&data).exists() {
        return Some(data);
    }
    None
}

pub fn portable_root() -> Option<PathBuf> {
    static PORTABLE: OnceLock<Option<PathBuf>> = OnceLock::new();
    PORTABLE.get_or_init(portable_root_locked).clone()
}

/// 便携模式的 DSH home（`Data/home`）：分身全部状态（profile/预设/凭证/会话）随包携带。
pub fn portable_home() -> Option<PathBuf> {
    portable_root().map(|r| r.join("home"))
}

/// 运行时根目录（便携模式 = `Data`）：
/// 安装版 Windows: %LOCALAPPDATA%\dsh-desktop-app-data；macOS: ~/Library/Application Support/dsh-desktop-app-data
/// 不用 `dsh-desktop`：NSIS 卸载器会整目录删除 InstallLocation，若应用恰好装在同名目录
/// （历史上以 mainBinaryName 作为默认安装名出现过），卸载会把便携运行时一并删掉。
/// 旧目录（…\dsh-desktop）存在时一次性整体迁移到新目录；迁移失败（如文件被占用）回退旧目录。
fn runtime_root_locked() -> PathBuf {
    if let Some(portable) = portable_root() {
        return portable;
    }
    #[cfg(windows)]
    let (old, new) = {
        let local = PathBuf::from(std::env::var("LOCALAPPDATA").unwrap_or_default());
        (local.join("dsh-desktop"), local.join("dsh-desktop-app-data"))
    };
    #[cfg(not(windows))]
    let (old, new) = {
        let home = std::env::var("HOME").unwrap_or_default();
        let p = PathBuf::from(home).join("Library/Application Support");
        (p.join("dsh-desktop"), p.join("dsh-desktop-app-data"))
    };
    if old.exists() && !new.exists() {
        if std::fs::rename(&old, &new).is_ok() {
            return new;
        }
        return old; // 迁移失败：沿用旧目录，功能不受影响
    }
    new
}

pub fn runtime_root() -> PathBuf {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(runtime_root_locked).clone()
}

fn node_exe_in(root: &PathBuf) -> PathBuf {
    root.join("node").join(if cfg!(windows) { "node.exe" } else { "bin/node" })
}

fn dsh_bin_js_in(root: &PathBuf) -> PathBuf {
    // Windows 便携版 npm -g 装到 node\node_modules；macOS 装到 node/lib/node_modules
    let mut p = root.join("node");
    if !cfg!(windows) {
        p = p.join("lib");
    }
    p.join("node_modules").join("@deepseek-ai").join("dsh").join("lib").join("bin.js")
}

/// 便携运行时候选根：自有目录优先，其次复用 dsh-persona（数字分身）安装的便携运行时，
/// 避免在 persona 机器上重复下载安装（persona 的 node/dsh 不在系统 PATH，System 回退探不到）。
/// 便携模式（U盘包）下只认包内 Data 根，绝不复用宿主机上的任何运行时。
fn portable_roots() -> Vec<PathBuf> {
    let mut roots = vec![runtime_root()];
    if portable_root().is_some() {
        return roots;
    }
    #[cfg(windows)]
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        roots.push(PathBuf::from(local).join("dsh-persona"));
    }
    #[cfg(not(windows))]
    if let Ok(home) = std::env::var("HOME") {
        roots.push(PathBuf::from(&home).join("Library/Application Support/dsh-persona"));
        roots.push(PathBuf::from(
            std::env::var("XDG_DATA_HOME").unwrap_or_else(|_| format!("{home}/.local/share")),
        ).join("dsh-persona"));
    }
    roots
}

/// node 与 dsh 都就绪的便携根目录；都没有时返回 None（bootstrap 走 System 回退或引导安装）。
/// （与 USB 便携包的 `portable_root` 不同：这里指“装好的运行时根”，可能是自有目录或 persona 复用。）
pub fn ready_root() -> Option<PathBuf> {
    portable_roots().into_iter().find(|r| node_exe_in(r).exists() && dsh_bin_js_in(r).exists())
}

pub fn node_exe() -> PathBuf {
    ready_root().map(|r| node_exe_in(&r)).unwrap_or_else(|| node_exe_in(&runtime_root()))
}

pub fn dsh_bin_js() -> PathBuf {
    ready_root().map(|r| dsh_bin_js_in(&r)).unwrap_or_else(|| dsh_bin_js_in(&runtime_root()))
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
