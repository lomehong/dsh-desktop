//! 运行时路径发现与自举（只检测、不安装；安装由 scripts/pin-runtime.mjs 与首启引导负责）。
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

/// 便携模式标记：exe 同级的 `Data` 目录（U盘分发包自带）。
/// 存在即把全部运行时数据与 DSH home 收进该目录，绝不读写宿主机用户目录；
/// 所有路径相对 exe 现场解析，U盘换盘符/换目录均有效。
fn portable_root_locked() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    portable_root_next_to(&exe)
}

fn portable_root_next_to(exe: &std::path::Path) -> Option<PathBuf> {
    let dir = exe.parent()?;
    let data = dir.join("Data");
    // 模式标记与完整性分开：Node 缺失时仍在 Data 内修复，不能切换到宿主目录。
    if data.is_dir() {
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

/// dsh-desktop 专属 DSH home：便携=包内 `Data/home`，安装版=数据目录下 `home`。
/// 与系统 dsh / persona 的 `~/.dsh` 隔离，避免多版本安装交叉污染同一套 profile 插件
/// （真实故障：alpha.4 核心 + 共享 ~/.dsh 旧插件 → 版本错位崩溃）。
pub fn app_home() -> PathBuf {
    portable_home().unwrap_or_else(|| runtime_root().join("home"))
}

/// 运行时根目录（便携模式 = `Data`）：
/// 安装版 Windows: %LOCALAPPDATA%\dsh-desktop-app-data；macOS: ~/Library/Application Support/dsh-desktop-app-data
/// 不用 `dsh-desktop`：NSIS 卸载器会整目录删除 InstallLocation，若应用恰好装在同名目录
/// （历史上以 mainBinaryName 作为默认安装名出现过），卸载会把便携运行时一并删掉。
/// 固定使用独立数据目录，不迁移、不复用安装目录，避免卸载/升级与运行时互相破坏。
fn runtime_root_locked() -> PathBuf {
    if let Some(portable) = portable_root() {
        return portable;
    }
    #[cfg(windows)]
    let base = PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap_or_default());
    #[cfg(not(windows))]
    let base = PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
        .join("Library/Application Support");
    base.join("dsh-desktop-app-data")
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

/// 自带 Node、npm、DSH 全部在场才算就绪；缺失交由自愈，不查找系统工具链。
pub fn ready_root() -> Option<PathBuf> {
    portable_roots().into_iter().find(|r| {
        node_exe_in(r).is_file() && dsh_bin_js_in(r).is_file() && npm_cli_js_in(r).is_file()
    })
}

pub fn node_exe() -> PathBuf {
    ready_root().map(|r| node_exe_in(&r)).unwrap_or_else(|| node_exe_in(&runtime_root()))
}

pub fn dsh_bin_js() -> PathBuf {
    ready_root().map(|r| dsh_bin_js_in(&r)).unwrap_or_else(|| dsh_bin_js_in(&runtime_root()))
}

/// 已装 dsh 包根目录（node_modules/@deepseek-ai/dsh）：webserver keep-alive 补丁等
/// 后安装自愈用。便携运行时缺失时返回 None。
pub fn dsh_package_dir() -> Option<PathBuf> {
    let root = ready_root()?;
    let mut p = root.join("node");
    if !cfg!(windows) {
        p = p.join("lib");
    }
    Some(p.join("node_modules").join("@deepseek-ai").join("dsh"))
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

/// 子进程使用应用私有工具链、配置和缓存；不修改父进程或用户级环境。
pub fn configure_command(cmd: &mut Command) -> Result<(), String> {
    let root = runtime_root();
    let home = app_home();
    let cache = root.join("cache");
    let config = root.join("config");
    let tmp = cache.join("tmp");
    let node = node_exe();
    let node_bin = node.parent().ok_or("自带 Node 路径无父目录")?;
    for dir in [&home, &cache, &config, &tmp] {
        std::fs::create_dir_all(dir).map_err(|e| format!("创建应用运行目录失败（{}）：{e}", dir.display()))?;
    }
    // npm 的继承配置可能指向用户全局 prefix/cache，不能让其覆盖应用隔离边界。
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().to_ascii_lowercase().starts_with("npm_config_") {
            cmd.env_remove(key);
        }
    }
    let inherited = std::env::var_os("PATH").unwrap_or_default();
    let paths = std::iter::once(node_bin.to_path_buf()).chain(std::env::split_paths(&inherited));
    let path = std::env::join_paths(paths).map_err(|e| format!("构造运行时 PATH 失败：{e}"))?;
    let modules = root.join("node").join(if cfg!(windows) { "node_modules" } else { "lib/node_modules" });
    let node_path = std::env::join_paths([modules.clone(), modules.join("@deepseek-ai/dsh/node_modules")])
        .map_err(|e| format!("构造自有 NODE_PATH 失败：{e}"))?;
    cmd.env("PATH", path)
        .env("DSH_HOME", &home).env("DSH_NODE_EXE", &node)
        .env("npm_config_prefix", root.join("node"))
        .env("npm_config_cache", cache.join("npm"))
        .env("npm_config_devdir", cache.join("node-gyp"))
        .env("npm_config_userconfig", config.join("npmrc"))
        .env("npm_config_globalconfig", config.join("npmrc-global"))
        .env("npm_config_store_dir", cache.join("pnpm-store"))
        .env("npm_config_cache_dir", cache.join("pnpm"))
        .env("npm_config_state_dir", root.join("pnpm-state"))
        .env("npm_config_global_dir", root.join("pnpm-global"))
        .env("npm_config_global_bin_dir", node_bin)
        .env("npm_config_manage_package_manager_versions", "false")
        .env("PNPM_HOME", node_bin).env("COREPACK_HOME", cache.join("corepack"))
        .env("XDG_CONFIG_HOME", &config).env("XDG_CACHE_HOME", &cache)
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("NODE_OPTIONS", "--dns-result-order=ipv4first")
        .env("NODE_USE_ENV_PROXY", "1").env("NODE_PATH", node_path)
        .env("TEMP", &tmp).env("TMP", &tmp).env("TMPDIR", &tmp);
    Ok(())
}

/// 本地服务仅允许使用应用自有运行时。
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Launch {
    Portable,
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

/// 自带环境不完整时由调用方在流程锁内准备运行时。
pub const NEED_AUTO_REPAIR: &str = "[auto-repair]";

fn npm_cli_js_in(root: &PathBuf) -> PathBuf {
    let modules = if cfg!(windows) { "node_modules" } else { "lib/node_modules" };
    root.join("node").join(modules).join("npm/bin/npm-cli.js")
}

/// 所有平台都通过自带 Node 直接执行 npm CLI，避免 PATH 解析到系统 npm。
pub fn portable_npm_cli_js() -> Option<PathBuf> {
    let cli = npm_cli_js_in(&runtime_root());
    cli.is_file().then_some(cli)
}

/// 只检测自带环境。安装版缺失时自动准备，USB 离线包缺失时给出修复指引。
pub fn bootstrap_runtime() -> Result<Launch, String> {
    let node_path = node_exe();
    let bin_path = dsh_bin_js();
    let portable = ready_root().is_some();
    // 诊断日志：记录检测到的路径与结果，便于排查环境差异
    if let Some(log) = open_log_append() {
        use std::io::Write;
        let mut log = log;
        let _ = writeln!(
            log,
            "[检测] runtime_root={:?} node={:?} exists={} bin={:?} exists={} portable={} npm_ready={}",
            runtime_root(),
            node_path,
            node_path.exists(),
            bin_path,
            bin_path.exists(),
            portable,
            portable_npm_cli_js().is_some()
        );
    }
    if portable {
        return Ok(Launch::Portable);
    }
    if portable_root().is_some() {
        return Err("便携包内 Node/npm/DSH 不完整，请联网后点击「安装运行环境」修复；不会使用系统运行时。".into());
    }
    Err(format!("{NEED_AUTO_REPAIR} 应用自带 Node/npm/DSH 不完整，将自动准备独立运行时。"))
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

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    // 子进程隔离环境变量与 OnceLock，测试不能迁移或改写开发机的真实数据。
    pub(crate) fn isolated_case(name: &str, setup: impl FnOnce(&std::path::Path)) -> Option<PathBuf> {
        if std::env::var("DSH_TEST_CASE").as_deref() == Ok(name) {
            return Some(PathBuf::from(std::env::var_os("DSH_TEST_ROOT").unwrap()));
        }
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-data")
            .join(format!("{}-{}-{}", name.replace(':', "-"), std::process::id(),
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&dir).unwrap();
        setup(&dir);
        let out = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture", "--include-ignored"])
            .env("DSH_TEST_CASE", name).env("DSH_TEST_ROOT", &dir)
            .env("LOCALAPPDATA", &dir).env("HOME", &dir)
            .current_dir(&dir).output().unwrap();
        if out.status.success() { let _ = std::fs::remove_dir_all(&dir); }
        assert!(out.status.success(), "隔离测试失败（诊断数据保留在 {}）：{}\n{}", dir.display(),
            String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        None
    }

    pub(crate) fn data_base(dir: &std::path::Path) -> PathBuf {
        if cfg!(windows) { dir.to_path_buf() } else { dir.join("Library/Application Support") }
    }

    #[test]
    fn incomplete_usb_runtime_stays_inside_data() {
        let Some(dir) = isolated_case("runtime::tests::incomplete_usb_runtime_stays_inside_data", |_| {}) else { return };
        let data = dir.join("usb/Data");
        std::fs::create_dir_all(&data).unwrap();
        assert_eq!(portable_root_next_to(&dir.join("usb/dsh-desktop.exe")), Some(data),
            "Data 是便携模式标记，Node 丢失不能使数据写回宿主机");
    }

    #[test]
    fn native_build_cache_and_module_paths_are_owned() {
        let Some(_) = isolated_case("runtime::tests::native_build_cache_and_module_paths_are_owned", |_| {}) else { return };
        let mut cmd = Command::new("unused");
        configure_command(&mut cmd).unwrap();
        let env: std::collections::HashMap<_, _> = cmd.get_envs().collect();
        let devdir = env.get(std::ffi::OsStr::new("npm_config_devdir")).and_then(|v| *v)
            .expect("node-gyp 缓存必须在应用内部");
        assert!(std::path::Path::new(devdir).starts_with(runtime_root()));
        let modules = env.get(std::ffi::OsStr::new("NODE_PATH")).and_then(|v| *v)
            .expect("保留指向自有包的 NODE_PATH，不能继承系统包目录");
        assert!(std::env::split_paths(modules).all(|p| p.starts_with(runtime_root())));
    }

    #[test]
    fn legacy_install_directory_is_never_moved_or_reused() {
        let Some(dir) = isolated_case("runtime::tests::legacy_install_directory_is_never_moved_or_reused", |dir| {
            let old = data_base(dir).join("dsh-desktop");
            std::fs::create_dir_all(old.join("node")).unwrap();
            std::fs::write(old.join("dsh-desktop.exe"), "应用文件").unwrap();
        }) else { return };
        let base = data_base(&dir);
        assert_eq!(runtime_root(), base.join("dsh-desktop-app-data"));
        assert!(base.join("dsh-desktop/dsh-desktop.exe").is_file(), "不能搬走安装目录");
        assert!(!runtime_root().join("dsh-desktop.exe").exists());
    }

    #[test]
    fn missing_owned_runtime_requires_repair_even_with_system_tools() {
        let Some(_) = isolated_case("runtime::tests::missing_owned_runtime_requires_repair_even_with_system_tools", |_| {}) else { return };
        let result = bootstrap_runtime();
        assert!(matches!(result, Err(ref e) if e.starts_with(NEED_AUTO_REPAIR)),
            "自带环境缺失时必须请求自愈，不能使用系统环境");
    }

    #[test]
    fn node_and_dsh_without_npm_are_not_ready() {
        let Some(_) = isolated_case("runtime::tests::node_and_dsh_without_npm_are_not_ready", |dir| {
            let root = data_base(dir).join("dsh-desktop-app-data");
            for file in [node_exe_in(&root), dsh_bin_js_in(&root)] {
                std::fs::create_dir_all(file.parent().unwrap()).unwrap();
                std::fs::write(file, "测试占位").unwrap();
            }
        }) else { return };
        assert!(ready_root().is_none(), "缺少自带 npm 时不能宣告运行时完整");
        assert!(matches!(bootstrap_runtime(), Err(e) if e.starts_with(NEED_AUTO_REPAIR)));
    }
}
