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

/// 便携运行时候选根：仅认自有目录（dsh-desktop-app-data 或便携包 Data）。
/// 不再回退 dsh-persona——桌面应用完全拥有自己的运行时生命周期，
/// 不依赖任何外部目录状态。运行时缺失时由自愈机制自动重装。
fn portable_roots() -> Vec<PathBuf> {
    vec![runtime_root()]
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
        let row_pid: u32 = fields.next()?.parse().ok()?;
        if row_pid == pid {
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

/// DSH 要求的最低 Node 主版本号（zstd 解压需要 Node ≥ 24 的 createZstdDecompress）。
const MIN_NODE_MAJOR: u32 = 24;

/// 自愈信号前缀：bootstrap_runtime 返回此错误表示"系统 Node 不兼容，需自动重装便携运行时"。
/// supervisor 检测此前缀触发 install::ensure_runtime_locked() 自愈流程。
pub const NEED_AUTO_REPAIR: &str = "[auto-repair]";

/// 检测系统 node 的主版本号。返回 None 表示无法执行或解析失败。
fn system_node_major() -> Option<u32> {
    let out = Command::new("node").arg("--version").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let ver = String::from_utf8_lossy(&out.stdout).trim().to_string();
    // "v24.19.0" → 24
    ver.strip_prefix('v')
        .and_then(|v| v.split('.').next())
        .and_then(|n| n.parse::<u32>().ok())
}

/// 系统 node 是否满足 DSH 运行要求（版本 ≥ MIN_NODE_MAJOR）。
pub fn system_node_capable() -> bool {
    system_node_major().map_or(false, |major| major >= MIN_NODE_MAJOR)
}

/// 只检测、不安装：node 与 dsh 都就绪才返回启动方式，否则返回自愈信号。
/// 自愈触发条件（均由 supervisor 检测 NEED_AUTO_REPAIR 前缀）：
/// - 系统 Node 不存在
/// - 系统 Node 存在但版本 < MIN_NODE_MAJOR（如 nvm v23 缺 zstd）
/// - 系统 dsh 命令不存在
/// 便携模式（U盘包）下不触发自愈（离线场景无法下载），仍走安装指引。
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
            "[检测] runtime_root={:?} node={:?} exists={} bin={:?} exists={} portable={} sys_node_capable={}",
            runtime_root(),
            node_path,
            node_path.exists(),
            bin_path,
            bin_path.exists(),
            portable,
            system_node_capable()
        );
    }
    if portable {
        return Ok(Launch::Portable);
    }
    // 便携模式（U盘包）：离线场景无法自动下载，仍走手动安装指引
    if portable_root().is_some() {
        if !command_exists("node") {
            return Err("未检测到 Node.js。\n请点击下方「安装运行环境」，或先安装 Node.js（https://nodejs.org/）。".into());
        }
        if !command_exists("dsh") {
            return Err("未检测到 DSH。\n请点击下方「安装运行环境」，或先执行 npm install -g @deepseek-ai/dsh。".into());
        }
        return Ok(Launch::System);
    }
    // 安装版：系统 Node 不存在或版本不兼容 → 自愈信号
    if !command_exists("node") {
        return Err(format!("{NEED_AUTO_REPAIR} 系统未安装 Node.js，将自动安装便携运行时。"));
    }
    if !system_node_capable() {
        let major = system_node_major().map_or("未知".into(), |m| format!("v{m}"));
        return Err(format!(
            "{NEED_AUTO_REPAIR} 系统 Node 版本 {major} 不满足 DSH 要求（需 ≥ v{MIN_NODE_MAJOR}），将自动安装便携运行时。"
        ));
    }
    // 系统 Node 兼容但 dsh 命令不存在 → 也触发自愈（装便携运行时比要求用户手动 npm i -g 更可靠）
    if !command_exists("dsh") {
        return Err(format!("{NEED_AUTO_REPAIR} 系统未安装 DSH，将自动安装便携运行时。"));
    }
    Ok(Launch::System)
}

/// 日志轮转阈值：超过即把当前日志改名为 `.old`（覆盖上一代）再重新开始。
/// 崩溃场景一次可写数百 KB stack trace，无轮转会无限膨胀。只保留一代 `.old`，
/// 足够回溯最近一次问题；改名失败（如被占用）就地续写，不阻断主流程。
const LOG_ROTATE_BYTES: u64 = 10 * 1024 * 1024;

/// 以追加模式打开日志文件（目录不存在时创建；失败时静默返回 None，诊断日志不阻断主流程）。
/// 超过 LOG_ROTATE_BYTES 先轮转：dsh-desktop.log → dsh-desktop.log.old。
pub fn open_log_append() -> Option<std::fs::File> {
    use std::io::Write;
    let path = log_file();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() >= LOG_ROTATE_BYTES {
            let mut old = path.clone().into_os_string();
            old.push(".old");
            let _ = std::fs::rename(&path, &old);
        }
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
