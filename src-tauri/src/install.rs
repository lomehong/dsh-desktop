//! 运行时安装与升级：便携 Node + 固定版本 dsh 装入应用数据目录
//! （Windows %LOCALAPPDATA%\dsh-desktop，macOS ~/Library/Application Support/dsh-desktop）。
//! 全程使用系统自带工具（curl 下载、tar 解压：Windows/macOS 为 bsdtar，Linux 为 gnu tar），
//! 零新增 Rust 依赖；下载走 npmmirror 镜像，nodejs.org / npm 官方源兜底。
use std::path::PathBuf;
use std::process::Command;

use crate::runtime::{self, no_window};
use crate::{status, supervisor};
use tauri::Manager;

/// 固定的 dsh 基线版本（全新环境首装用；升级走 alpha/latest 双 tag 择新，可用
/// DSH_DESKTOP_DSH_VERSION 固定）。基线必须跟上插件生态的 API 代际：profile 插件
/// （如 dsh-better-sidebar@0.18）的 peer 依赖按 rc.1 构建，基线落后会让插件全部
/// 因 API 缺符号（settingsNamespace）加载失败（真实故障 2026-09）。
pub const DSH_VERSION: &str = "0.1.2-rc.1";
/// 便携 Node 版本（dsh rc.x 的 zstd 要求需要 Node 24）。
const NODE_VERSION: &str = "24.19.0";
/// 壳已适配的 dsh 最高版本（语义化三元组）。v0.1.18 起真机复核了 0.1.2-alpha.2
/// 的双协议适配（events.rs remote.mux + token→cookie 在 alpha.2 端点下工作；
/// persona 插件同步迁移到 SettingsProvider.installSection API）——升到 (0,1,2)
/// 放行 0.1.2 系列（含 alpha/rc 预发布与未来的正式版）。npm latest 超出此版本时
/// 仍拒绝升级并引导先升级应用本体；DSH_DESKTOP_DSH_VERSION 显式指定视为知情
/// 强制，不受限。
const DSH_MAX_ADAPTED: (u64, u64, u64) = (0, 1, 2);

/// 壳已适配的 dsh 最高版本三元组（supervisor 启动预检用）。
pub fn max_adapted() -> (u64, u64, u64) {
    DSH_MAX_ADAPTED
}

/// 解析语义化版本三元组（忽略 `-rc.x`/`-alpha.x`/`+build` 等后缀；解析失败返回 None）。
/// supervisor 启动预检复用，故公开。
pub fn version_triple_public(v: &str) -> Option<(u64, u64, u64)> {
    version_triple(v)
}

/// 解析语义化版本三元组（忽略 `-rc.x`/`-alpha.x`/`+build` 等后缀；解析失败返回 None）。
fn version_triple(v: &str) -> Option<(u64, u64, u64)> {
    let mut it = v.split(['-', '+']).next()?.split('.');
    Some((
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
    ))
}

/// 拆出版本的预发布段（`-` 后、`+build` 前；无则 None）。
fn prerelease_of(v: &str) -> Option<&str> {
    v.split_once('-').map(|(_, rest)| rest.split('+').next().unwrap_or(rest))
}

/// 预发布标识符比较：纯数字按数值且小于字母标识符，其余按 ASCII（语义化规范 §11）。
fn cmp_prerelease_ident(a: &str, b: &str) -> std::cmp::Ordering {
    match (a.parse::<u64>(), b.parse::<u64>()) {
        (Ok(x), Ok(y)) => x.cmp(&y),
        (Ok(_), Err(_)) => std::cmp::Ordering::Less,
        (Err(_), Ok(_)) => std::cmp::Ordering::Greater,
        (Err(_), Err(_)) => a.cmp(b),
    }
}

/// 预发布段比较：无预发布（正式版）> 有预发布；有则逐标识符，前缀短者小。
fn cmp_prerelease(a: Option<&str>, b: Option<&str>) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    match (a, b) {
        (None, None) => Equal,
        (None, Some(_)) => Greater,
        (Some(_), None) => Less,
        (Some(x), Some(y)) => {
            let mut xi = x.split('.');
            let mut yi = y.split('.');
            loop {
                match (xi.next(), yi.next()) {
                    (None, None) => return Equal,
                    (None, Some(_)) => return Less,
                    (Some(_), None) => return Greater,
                    (Some(p), Some(q)) => {
                        let ord = cmp_prerelease_ident(p, q);
                        if ord != Equal {
                            return ord;
                        }
                    }
                }
            }
        }
    }
}

/// 预发布感知的完整版本比较（升级通道择新用；`version_triple` 会把 alpha/rc 抹平，
/// 无法区分 0.1.2-alpha.5 与 0.1.2-rc.1 谁新——真实故障：latest 已是 rc.1 而通道逻辑
/// 把 alpha 用户锁死在 alpha.5）。解析失败的版本对按相等处理（调用方容忍）。
pub fn cmp_versions(a: &str, b: &str) -> std::cmp::Ordering {
    match (version_triple(a), version_triple(b)) {
        (Some(x), Some(y)) => {
            let ord = x.cmp(&y);
            if ord != std::cmp::Ordering::Equal {
                return ord;
            }
            cmp_prerelease(prerelease_of(a), prerelease_of(b))
        }
        _ => std::cmp::Ordering::Equal,
    }
}

/// 给命令前置便携 node 目录到 PATH（Unix 的 npm 脚本用 `#!/usr/bin/env node` 找解释器）。
fn prepend_node_path(c: &mut Command) {
    let node_bin = runtime::runtime_root().join("node").join("bin");
    let sep = if cfg!(windows) { ";" } else { ":" };
    let sys = std::env::var("PATH").unwrap_or_default();
    c.env("PATH", format!("{}{}{}", node_bin.display(), sep, sys));
}

/// Node 发行版平台标签（win-x64 / darwin-arm64 / darwin-x64 / linux-x64 / linux-arm64）。
fn node_platform_tag() -> Result<&'static str, String> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => Ok("win-x64"),
        ("macos", "aarch64") => Ok("darwin-arm64"),
        ("macos", "x86_64") => Ok("darwin-x64"),
        ("linux", "x86_64") => Ok("linux-x64"),
        ("linux", "aarch64") => Ok("linux-arm64"),
        (os, arch) => Err(format!("暂不支持的平台 {os}-{arch}")),
    }
}

/// Node 发行版压缩包文件名（win 为 zip，mac 为 tar.gz，linux 为 tar.xz）。
fn node_archive_name() -> Result<String, String> {
    let tag = node_platform_tag()?;
    let ext = if cfg!(windows) {
        "zip"
    } else if cfg!(target_os = "macos") {
        "tar.gz"
    } else {
        "tar.xz"
    };
    Ok(format!("node-v{NODE_VERSION}-{tag}.{ext}"))
}

/// 解压后 Node 顶层目录名（不含扩展名）。
fn node_inner_dir() -> Result<String, String> {
    Ok(format!("node-v{NODE_VERSION}-{}", node_platform_tag()?))
}

fn node_mirror_urls() -> Vec<String> {
    let Ok(name) = node_archive_name() else {
        return vec![];
    };
    let mut urls = vec![format!("https://npmmirror.com/mirrors/node/v{NODE_VERSION}/{name}")];
    if let Ok(custom) = std::env::var("DSH_DESKTOP_NODE_MIRROR") {
        urls.insert(0, format!("{custom}/v{NODE_VERSION}/{name}"));
    }
    urls.push(format!("https://nodejs.org/dist/v{NODE_VERSION}/{name}"));
    urls
}

fn npm_registry() -> Vec<String> {
    match std::env::var("DSH_DESKTOP_NPM_REGISTRY") {
        Ok(r) if !r.is_empty() => vec![format!("--registry={r}")],
        _ => vec![
            "--registry=https://registry.npmmirror.com".to_string(),
            "--registry=https://registry.npmjs.org".to_string(),
        ],
    }
}

/// 升级/查询所用的运行时根：优先解析到的便携根（含 dsh-persona 复用），
/// 没有便携运行时时回退自有目录（此时 install_runtime 会先装基线）。
fn active_root() -> PathBuf {
    runtime::ready_root().unwrap_or_else(|| runtime::runtime_root())
}

/// 便携运行时中的 npm 可执行入口（Windows 为 npm.cmd，Unix 为 bin/npm）。
fn npm_tool() -> Option<PathBuf> {
    let npm = active_root().join("node").join(if cfg!(windows) {
        "npm.cmd"
    } else {
        "bin/npm"
    });
    npm.exists().then_some(npm)
}

/// 读取便携运行时中已安装的 dsh 版本（package.json 的 version 字段）。
pub fn installed_dsh_version() -> Option<String> {
    let pj = active_root()
        .join("node")
        .join(if cfg!(windows) { "node_modules" } else { "lib/node_modules" })
        .join("@deepseek-ai")
        .join("dsh")
        .join("package.json");
    let text = std::fs::read_to_string(pj).ok()?;
    serde_json::from_str::<serde_json::Value>(&text)
        .ok()?
        .get("version")?
        .as_str()
        .map(String::from)
}

/// 构造一条运行 npm 的 Command。
/// Windows：直接 node + npm-cli.js，避开 cmd.exe /C npm.cmd 的窗口闪烁（v0.1.28 修复）；
/// 找不到 npm-cli.js 时回退到 cmd.exe /C npm.cmd 兼容路径。
/// Unix：直接 bin/npm（已是真二进制）。
fn npm_command() -> Result<Command, String> {
    #[cfg(windows)]
    {
        let node = runtime::node_exe();
        if let Some(cli) = runtime::portable_npm_cli_js().filter(|_| node.exists()) {
            let mut c = Command::new(node);
            c.arg(cli);
            return Ok(c);
        }
        // 回退：cmd.exe /C npm.cmd
        let npm = npm_tool().ok_or("便携运行时未安装，无法构造 npm 命令")?;
        let mut c = Command::new("cmd.exe");
        c.args(["/D", "/C"]).arg(&npm);
        return Ok(c);
    }
    #[cfg(not(windows))]
    {
        let npm = npm_tool().ok_or("便携运行时未安装，无法构造 npm 命令")?;
        Ok(Command::new(&npm))
    }
}

/// 查询 npm registry 上 @deepseek-ai/dsh 指定 dist-tag 的版本。
fn dist_tag_version(tag: &str) -> Result<String, String> {
    let mut last_err = String::new();
    for registry in npm_registry() {
        let mut c = npm_command()?;
        c.args(["view", "@deepseek-ai/dsh", &format!("dist-tags.{tag}")]).arg(&registry);
        prepend_node_path(&mut c);
        match no_window(&mut c).output() {
            Ok(o) if o.status.success() => {
                let v = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if !v.is_empty() {
                    return Ok(v);
                }
                last_err = "registry 返回空版本".into();
            }
            Ok(_) => last_err = "npm view 退出码非零".into(),
            Err(e) => last_err = e.to_string(),
        }
    }
    Err(format!("查询最新版本失败：{last_err}（可设置 DSH_DESKTOP_NPM_REGISTRY）"))
}

/// 升级目标版本：DSH_DESKTOP_DSH_VERSION 显式指定优先；否则查询 alpha/latest 两个
/// dist-tag 取**较高**者（预发布感知比较，cmp_versions）。旧逻辑「装了 alpha 就只跟
/// alpha tag」是为防「升级按钮变降级」（latest 曾指向旧稳定线），但 latest 反超 alpha
/// 时形成死锁——真实故障：用户被锁死 0.1.2-alpha.5，npm latest 已是 0.1.2-rc.1，插件
/// 生态按 rc.1 构建，升级按钮永远提示「已是最新」。单 tag 查询失败时用另一个兜底。
fn target_version() -> Result<String, String> {
    if let Ok(v) = std::env::var("DSH_DESKTOP_DSH_VERSION") {
        if !v.is_empty() {
            return Ok(v);
        }
    }
    let mut last_err = String::new();
    let mut best: Option<String> = None;
    for tag in ["alpha", "latest"] {
        match dist_tag_version(tag) {
            Ok(v) => {
                let is_newer = best
                    .as_deref()
                    .map_or(true, |cur| cmp_versions(&v, cur) == std::cmp::Ordering::Greater);
                if is_newer {
                    best = Some(v);
                }
            }
            Err(e) => last_err = e,
        }
    }
    best.ok_or(last_err)
}

/// 安装指定版本的 dsh 到活动便携运行时（输出落日志）。
fn npm_install_dsh(version: &str) -> Result<(), String> {
    let mut last_err = String::new();
    for registry in npm_registry() {
        match npm_install_dsh_once(version, &registry) {
            Ok(()) => return Ok(()),
            Err(e) => last_err = e,
        }
    }
    Err(format!("DSH v{version} 安装失败：{last_err}"))
}

/// 探测已装便携 dsh 的 web 命令是否接受 --no-open。
/// 防 npmmirror 镜像滞后返回缺该 flag 的旧 tarball（真实故障：装到 alpha.4 但无 --no-open）。
pub fn web_supports_no_open() -> bool {    let node = runtime::node_exe();
    let bin = runtime::dsh_bin_js();
    if !node.exists() || !bin.exists() {
        return true; // 无可探测对象，视为支持（后续启动自检会兜底）
    }
    let mut c = Command::new(&node);
    c.arg(&bin).args(["web", "--help"]);
    prepend_node_path(&mut c);
    match no_window(&mut c).output() {
        Ok(o) => {
            let s = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
            s.contains("--no-open")
        }
        Err(_) => true,
    }
}

/// 强制从官方源重装基线 dsh（镜像包不完整时的自愈手段）。
pub fn force_reinstall_official() -> Result<(), String> {
    npm_install_dsh_once(DSH_VERSION, "--registry=https://registry.npmjs.org")
}

/// 把捕获的子进程输出原样补写进日志（此前直接 Stdio 继承tee，因需解析内容改为捕获）。
fn tee_bytes(log: &mut Option<std::fs::File>, bytes: &[u8]) {
    if let Some(f) = log.as_mut() {
        use std::io::Write;
        let _ = f.write_all(bytes);
    }
}

fn npm_install_dsh_once(version: &str, registry: &str) -> Result<(), String> {
    let node_dir = active_root().join("node");
    let mut log = runtime::open_log_append();
    if let Some(f) = log.as_mut() {
        use std::io::Write;
        let _ = writeln!(f, "[npm] registry={registry} target={version}");
    }
    let mut c = npm_command()?;
    c.args(["install", "-g", &format!("@deepseek-ai/dsh@{version}"), "--prefix"])
        .arg(&node_dir)
        .arg(registry);
    prepend_node_path(&mut c);
    // 捕获输出再落日志：要解析 allow-scripts 拦截清单，直接 Stdio 继承拿不到内容
    let result = no_window(&mut c).output();
    if let Ok(o) = &result {
        tee_bytes(&mut log, &o.stdout);
        tee_bytes(&mut log, &o.stderr);
    }
    match result {
        Ok(o) if o.status.success() => {
            // npm 11.16+ 对未在 allowScripts 策略内的依赖安装脚本告警、npm 12 起默认拦截：
            // koffi/node-pty 等原生模块缺构建脚本要到运行时才炸，解析拦截清单立即补跑。
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            );
            let skipped = parse_allow_scripts_skipped(&text);
            if !skipped.is_empty() {
                if let Err(e) = rerun_blocked_install_scripts(&skipped) {
                    // 补跑失败不阻断启动：多数关键包自带 prebuild，服务仍可起；留证日志
                    if let Some(f) = log.as_mut() {
                        use std::io::Write;
                        let _ = writeln!(f, "[warn] 安装脚本补跑失败（原生模块可能不可用）: {e}");
                    }
                }
            }
            Ok(())
        }
        Ok(_) => Err("npm 退出码非零（详见日志）".into()),
        Err(e) => Err(e.to_string()),
    }
}

/// 从 npm 输出解析「安装脚本被 allow-scripts 策略跳过」的包名清单。
/// 真实输出形态：
/// ```text
/// npm warn allow-scripts 5 packages have install scripts not yet covered by allowScripts:
/// npm warn allow-scripts   @deepseek-ai/dsh-subprocess-local@0.1.2-rc.1 (postinstall: node scripts/ensure-spawn-helper.mjs)
/// npm warn allow-scripts   koffi@3.2.1 (install: node ./cnoke.cjs -P . -D src/koffi --prebuild --release)
/// ```
/// 只取 `包名@版本` 条目行，头部统计行与建议行忽略；同名去重保序。
fn parse_allow_scripts_skipped(output: &str) -> Vec<String> {
    const MARKER: &str = "npm warn allow-scripts";
    let mut names: Vec<String> = Vec::new();
    for line in output.lines() {
        let Some(idx) = line.find(MARKER) else { continue };
        let rest = line[idx + MARKER.len()..].trim();
        if rest.is_empty()
            || rest.contains("packages have install scripts")
            || rest.starts_with("Run `npm")
        {
            continue;
        }
        let token = rest.split_whitespace().next().unwrap_or("");
        // `name@version`：scope 包名本身含 @（@deepseek-ai/dsh@1.0.0），取最后一个 @ 剥版本
        if let Some(at) = token.rfind('@') {
            if at > 0 {
                let name = &token[..at];
                if !name.is_empty() && !names.iter().any(|n| n == name) {
                    names.push(name.to_string());
                }
            }
        }
    }
    names
}

/// npm 拦截了依赖安装脚本时补跑：`npm rebuild -g <pkgs> --allow-scripts=…` 显式执行
/// 被拦截包的 install 脚本（rebuild 就是「对已装树补跑脚本」的官方通道，且仅在本函数
/// 被调用的前提——npm 输出里出现了 allow-scripts 警告——下才走，该配置必然被识别）。
fn rerun_blocked_install_scripts(skipped: &[String]) -> Result<(), String> {
    let node_dir = active_root().join("node");
    let mut log = runtime::open_log_append();
    let mut c = npm_command()?;
    c.arg("rebuild").arg("-g").args(skipped.iter().map(String::as_str));
    c.arg(format!("--allow-scripts={}", skipped.join(",")));
    c.arg("--prefix").arg(&node_dir);
    prepend_node_path(&mut c);
    let result = no_window(&mut c).output();
    if let Ok(o) = &result {
        tee_bytes(&mut log, &o.stdout);
        tee_bytes(&mut log, &o.stderr);
    }
    match result {
        Ok(o) if o.status.success() => {
            if let Some(f) = log.as_mut() {
                use std::io::Write;
                let _ = writeln!(
                    f,
                    "[自愈] 已补跑 {} 个被 allow-scripts 拦截的安装脚本: {}",
                    skipped.len(),
                    skipped.join(",")
                );
            }
            Ok(())
        }
        Ok(_) => Err("npm rebuild 退出码非零（详见日志）".into()),
        Err(e) => Err(e.to_string()),
    }
}

/// 下载单个文件：优先 curl（各平台自带），Windows 用 PowerShell、Unix 用 wget 兜底。
fn download(url: &str, dest: &PathBuf) -> Result<(), String> {
    let mut c = if cfg!(windows) {
        Command::new("curl.exe")
    } else {
        Command::new("curl")
    };
    c.args(["-L", "--fail", "--connect-timeout", "20", "-o"]);
    c.arg(dest);
    c.arg(url);
    if matches!(no_window(&mut c).status(), Ok(s) if s.success()) {
        return Ok(());
    }
    #[cfg(windows)]
    {
        let mut p = Command::new("powershell");
        p.args(["-NoProfile", "-Command", &format!(
            "Invoke-WebRequest -Uri '{}' -OutFile '{}'",
            url,
            dest.display()
        )]);
        if matches!(no_window(&mut p).status(), Ok(s) if s.success()) {
            return Ok(());
        }
    }
    #[cfg(unix)]
    {
        let mut w = Command::new("wget");
        w.args(["-q", "--timeout=30", "-O"]).arg(dest).arg(url);
        if w.status().map(|s| s.success()).unwrap_or(false) {
            return Ok(());
        }
    }
    Err(format!("下载失败 ({url})"))
}

/// 自愈入口（锁内调用）：由 supervisor 在 bootstrap_runtime 返回 NEED_AUTO_REPAIR 时触发。
/// 与 install_runtime 共享安装逻辑，但不取 restarting 闸锁（调用方已持锁）。
pub fn ensure_runtime_locked(app: &tauri::AppHandle) -> Result<(), String> {
    install_runtime_inner(app)
}

/// 当前 DSH home：始终用 dsh-desktop 专属 home（与系统 dsh/persona 的 ~/.dsh 隔离）。
pub fn dsh_home() -> PathBuf {
    runtime::app_home()
}

fn core_version_marker() -> PathBuf {
    runtime::runtime_root().join("last-core-version")
}

/// 核心 dsh 版本变化时清空各 profile 的 node_modules，强制按新核心重新解析插件。
/// 修复「核心升级但 profile 插件仍是旧版/符号链接指向残留安装」的版本错位（真实故障：
/// alpha.4 核心 + 旧 dsh-tool-subagent 缺 exports / 旧树 import .css 崩溃）。
/// 返回被清空的 profile 名单——清空后 bundle 实体全部失联，dsh 启动只解析不安装，
/// 调用方须对名单主动补装（install_profile_plugins）。首次运行（无标记）只记录不清理。
pub fn refresh_profile_plugins_if_core_changed() -> Vec<String> {
    let Some(cur) = installed_dsh_version() else { return Vec::new() };
    let marker = core_version_marker();
    let prev = std::fs::read_to_string(&marker).unwrap_or_default();
    let prev = prev.trim().to_string();
    if prev.is_empty() {
        let _ = std::fs::write(&marker, &cur);
        return Vec::new(); // 首次运行：仅记录，避免误清健康环境
    }
    if prev == cur {
        return Vec::new();
    }
    let _ = std::fs::write(&marker, &cur);
    let profiles = dsh_home().join("profiles");
    let Ok(entries) = std::fs::read_dir(&profiles) else { return Vec::new() };
    let mut cleared = Vec::new();
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let nm = e.path().join("node_modules");
        if nm.is_dir() && std::fs::remove_dir_all(&nm).is_ok() {
            cleared.push(name);
        }
    }
    if !cleared.is_empty() {
        if let Some(mut log) = runtime::open_log_append() {
            use std::io::Write;
            let _ = writeln!(
                log,
                "[自愈] 核心 {prev} -> {cur}，清空 {} 个 profile 插件目录强制重装: {}",
                cleared.len(),
                cleared.join(",")
            );
        }
    }
    cleared
}

/// home/profiles 下含 package.json 的 profile 名单（插件补装的目标集合）。
pub fn profile_names() -> Vec<String> {
    let profiles = dsh_home().join("profiles");
    let Ok(entries) = std::fs::read_dir(&profiles) else { return Vec::new() };
    entries
        .flatten()
        .filter(|e| e.path().is_dir() && e.path().join("package.json").is_file())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect()
}

/// 构造调用便携 dsh CLI 的 Command（环境与 supervisor::spawn_dsh 的 Portable 分支对齐：
/// DSH_HOME 指向专属 home、npm 缓存收进 home、PATH 前置便携 node、cwd 在 node 目录；
/// 非 Windows 补 HOME/NODE_PATH 保 ESM 解析）。运行时未就绪返回 None。
fn dsh_cli_command() -> Option<Command> {
    let node = runtime::node_exe();
    let bin = runtime::dsh_bin_js();
    if !node.exists() || !bin.exists() {
        return None;
    }
    let node_dir = node.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    let sep = if cfg!(windows) { ";" } else { ":" };
    let sys = std::env::var("PATH").unwrap_or_default();
    let mut c = Command::new(&node);
    c.arg(&bin);
    c.env("PATH", format!("{}{}{}", node_dir.display(), sep, sys));
    let home = dsh_home();
    c.env("DSH_HOME", &home);
    c.env("npm_config_cache", home.join(".npm-cache"));
    c.current_dir(&node_dir);
    #[cfg(not(windows))]
    {
        let home_env = std::env::var("HOME").unwrap_or_default();
        let nm = runtime::runtime_root().join("node").join("lib").join("node_modules");
        let dsh_nm = nm.join("@deepseek-ai").join("dsh").join("node_modules");
        c.env("HOME", home_env)
            .env("NODE_PATH", format!("{}:{}", nm.display(), dsh_nm.display()));
    }
    Some(c)
}

/// 逐个执行 `dsh plugin --profile <name> install`：把「profile 配置在场而插件实体
/// 失联」（核心更新清空了插件目录、profile 配置外部带入、上次安装被打断等）的
/// bundle 按当前清单重新装回 profile 目录。输出 tee 进日志；单个 profile 失败
/// 不影响其余，全部结束后汇总报错。
pub fn install_profile_plugins(profiles: &[String], why: &str) -> Result<(), String> {
    if profiles.is_empty() {
        return Ok(());
    }
    let mut log = runtime::open_log_append();
    let mut failures: Vec<String> = Vec::new();
    for name in profiles {
        let Some(mut c) = dsh_cli_command() else {
            return Err("便携运行时未就绪，无法补装 profile 插件".into());
        };
        c.args(["plugin", "--profile", name, "install"]);
        if let Some(f) = log.as_mut() {
            use std::io::Write;
            let _ = writeln!(f, "[自愈] {why}：补装 profile 插件（{name}）…");
        }
        let result = no_window(&mut c).output();
        match result {
            Ok(o) => {
                tee_bytes(&mut log, &o.stdout);
                tee_bytes(&mut log, &o.stderr);
                if !o.status.success() {
                    let stderr = String::from_utf8_lossy(&o.stderr);
                    let lines: Vec<&str> = stderr.lines().collect();
                    let start = lines.len().saturating_sub(5);
                    let tail = lines[start..].join(" | ");
                    failures.push(format!("{name}: npm 退出码非零（{tail}）"));
                }
            }
            Err(e) => failures.push(format!("{name}: {e}")),
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{} 个 profile 插件补装失败: {}",
            failures.len(),
            failures.join("；")
        ))
    }
}

/// 安装便携运行时（幂等）：Node 缺则下载解压，dsh 缺则 npm -g 安装固定版本。
/// 每步经 status 更新到加载页。供首启引导与托盘升级共用。
/// 与 supervisor 流程共用同一把 FlowGate 闸锁：在途流程未结束时排队等待而非静默
/// 放弃——用户在服务启动中途点「安装运行环境」，安装请求不再凭空消失。
pub fn install_runtime(app: &tauri::AppHandle) -> Result<(), String> {
    let state: tauri::State<crate::AppState> = app.state();
    state.restarting.acquire();
    let result = install_runtime_inner(app);
    state.restarting.release();
    result
}

fn install_runtime_inner(app: &tauri::AppHandle) -> Result<(), String> {
    let root = runtime::runtime_root();
    let node_dir = root.join("node");
    std::fs::create_dir_all(&root).map_err(|e| format!("无法创建数据目录: {e}"))?;

    // 1) 便携 Node：缺失则下载解压（按平台选发行版，顶层目录改名为 node）
    if !runtime::node_exe().exists() {
        let archive = node_archive_name()?;
        status::set(app, &format!("正在下载 Node v{NODE_VERSION}（镜像加速）…"));
        let downloads = root.join("downloads");
        std::fs::create_dir_all(&downloads).map_err(|e| format!("{e}"))?;
        let zip = downloads.join(&archive);
        let mut last_err = String::new();
        let mut ok = false;
        for url in node_mirror_urls() {
            match download(&url, &zip) {
                Ok(()) => {
                    ok = true;
                    break;
                }
                Err(e) => last_err = e,
            }
        }
        if !ok {
            return Err(format!("Node 下载失败：{last_err}"));
        }
        status::set(app, "正在解压 Node…");
        let extract_to = root.join("node-extract");
        let _ = std::fs::remove_dir_all(&extract_to);
        std::fs::create_dir_all(&extract_to).map_err(|e| format!("{e}"))?;
        // bsdtar（win/mac）与 gnu tar 均可直接解 zip/tar.gz/tar.xz
        let tar = if cfg!(windows) { "tar.exe" } else { "tar" };
        let mut c = Command::new(tar);
        c.args(["-xf"]).arg(&zip).arg("-C").arg(&extract_to);
        no_window(&mut c)
            .status()
            .map_err(|e| format!("解压失败: {e}"))
            .and_then(|s| if s.success() { Ok(()) } else { Err("解压失败".into()) })?;
        let inner = extract_to.join(node_inner_dir()?);
        let _ = std::fs::remove_dir_all(&node_dir);
        std::fs::rename(&inner, &node_dir).map_err(|e| format!("安装 Node 失败: {e}"))?;
        let _ = std::fs::remove_dir_all(&extract_to);
        let _ = std::fs::remove_file(&zip);
    }

    // 2) dsh 固定基线版本：便携 npm -g 装入 node 目录（升级走 upgrade_dsh 的远程清单）
    if !runtime::dsh_bin_js().exists() {
        status::set(app, &format!("正在安装 DSH v{DSH_VERSION}（首次约 1~3 分钟）…"));
        npm_install_dsh(DSH_VERSION)?;
    }
    // 3) 镜像完整性校验：npmmirror 可能滞后返回缺 --no-open 的旧 tarball（真实故障）。
    // 装完探测能力，不完整则切官方源强制重装，保证启动参数与包能力一致。
    if !web_supports_no_open() {
        status::set(app, "镜像包不完整，切换官方源重装 DSH…");
        if let Some(mut log) = runtime::open_log_append() {
            use std::io::Write;
            let _ = writeln!(log, "[自愈] 镜像 tarball 缺 --no-open，切 npmjs 重装 v{DSH_VERSION}");
        }
        npm_install_dsh_once(DSH_VERSION, "--registry=https://registry.npmjs.org")?;
    }
    Ok(())
}

/// 升级检查与安装（不含服务重启）：在**活动**便携运行时上就地升级
/// （含 dsh-persona 复用的运行时）；完全没有便携运行时时先装基线到自有目录。
/// 返回给用户的状态文案。
pub fn upgrade_dsh(app: &tauri::AppHandle) -> Result<String, String> {
    if npm_tool().is_none() {
        // 无便携运行时（System 回退或全新）：先装基线，之后活动根即自有目录
        install_runtime(app)?;
    }
    let target = target_version()?;
    // 升级护栏：npm latest 超出壳已适配的版本线时拒绝，防止“升级按钮变砖”。
    // DSH_DESKTOP_DSH_VERSION 显式指定视为知情强制，绕过护栏（逃生门）。
    if std::env::var("DSH_DESKTOP_DSH_VERSION").map_or(true, |v| v.is_empty()) {
        if let Some(t) = version_triple(&target) {
            if t > DSH_MAX_ADAPTED {
                return Err(format!(
                    "DSH v{target} 超出当前应用已适配的运行时版本（≤0.{}.{}.x）：该版本线启用了 Web 一次性 token 认证并更换了事件流端点。请先把 dsh-desktop 应用本体升级到配套版本；如确需强制，可设环境变量 DSH_DESKTOP_DSH_VERSION 指定目标版本。",
                    DSH_MAX_ADAPTED.0, DSH_MAX_ADAPTED.1
                ));
            }
        }
    }
    let installed = installed_dsh_version();
    if installed.as_deref() == Some(target.as_str()) {
        return Ok(format!("DSH 运行时已是最新 v{target}"));
    }
    status::set(app, &format!("正在安装 DSH 运行时 v{target}…"));
    npm_install_dsh(&target)?;
    let from = installed.unwrap_or_else(|| "无".into());
    Ok(format!("DSH 运行时已升级到 v{target}（原 {from}）"))
}

/// 托盘「升级 DSH 运行时」：停服务 → 检查并安装 → 重新启动。
/// 模式盲项收编：远程模式下不「停服务」（本地 child 恒为 None）、不撤 origin，
/// 升级完成后按远程模式重连——绝不 start_service 把本地服务拉起来顶掉远程页面
/// （升级的本地运行时等下次回到本地时生效）。
pub fn upgrade_runtime(app: &tauri::AppHandle) {
    status::set(app, "正在查询 npm 上 DSH 运行时的最新版本…");
    let state: tauri::State<crate::AppState> = app.state();
    let remote_mode = *state.mode.lock().unwrap() == "remote";
    if !remote_mode {
        // 先停服务，避免替换运行中的文件
        if let Some(mut child) = state.child.lock().unwrap().take() {
            supervisor::kill_tree(child.id() as u32);
            let _ = child.wait();
        }
        *state.origin.lock().unwrap() = None;
    }
    // 回加载页显示升级进度（远程模式仅导航不撤 origin：升级失败仍可直接重连）
    crate::webview::navigate_to_loader(app);
    match upgrade_dsh(app) {
        Ok(msg) => {
            let next = if remote_mode { "正在重连远程实例…" } else { "正在启动服务…" };
            status::set(app, &format!("{msg}，{next}"));
        }
        Err(e) => {
            status::fail(app, &e);
            return;
        }
    }
    if remote_mode {
        if let Err(e) = supervisor::connect_remote_flow(app) {
            status::fail(app, &e);
        }
    } else if let Err(e) = supervisor::start_service(app) {
        status::fail(app, &e);
    }
}

/// 首启安装入口：安装完成后自动续跑启动序列。
/// 模式盲项收编：「安装运行环境」按钮理论上只在本地错误态出现，仍按模式防御性分派——
/// 远程模式下安装完成后重连远程实例，不拉本地服务。
pub fn install_and_start(app: &tauri::AppHandle) {
    if let Err(e) = install_runtime(app) {
        status::fail(app, &e);
        return;
    }
    let remote_mode = {
        let state: tauri::State<crate::AppState> = app.state();
        // 先落局部变量再比较：块尾表达式会让 MutexGuard 临时值活过 state 的析构（E0597）
        let mode = *state.mode.lock().unwrap();
        mode == "remote"
    };
    if remote_mode {
        status::set(app, "运行环境就绪，正在重连远程实例…");
        if let Err(e) = supervisor::connect_remote_flow(app) {
            status::fail(app, &e);
        }
        return;
    }
    status::set(app, "运行环境就绪，正在启动服务…");
    if let Err(e) = supervisor::start_service(app) {
        status::fail(app, &e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_triple_strips_prerelease_and_build_suffixes() {
        assert_eq!(version_triple("0.1.1-rc.2"), Some((0, 1, 1)));
        assert_eq!(version_triple("0.1.2-alpha.1"), Some((0, 1, 2)));
        assert_eq!(version_triple("0.1.2"), Some((0, 1, 2)));
        assert_eq!(version_triple("1.2.3+build.5"), Some((1, 2, 3)));
        assert_eq!(version_triple("junk"), None);
    }

    /* ── 预发布感知版本比较（升级通道择新） ── */
    #[test]
    fn cmp_versions_orders_prereleases_semver_style() {
        use std::cmp::Ordering::*;
        // 真实故障方向：latest(rc.1) 反超 alpha 通道钉死的 alpha.5
        assert_eq!(cmp_versions("0.1.2-rc.1", "0.1.2-alpha.5"), Greater);
        assert_eq!(cmp_versions("0.1.2-alpha.5", "0.1.2-rc.1"), Less);
        // 正式版 > 同三元组任何预发布
        assert_eq!(cmp_versions("0.1.2", "0.1.2-rc.1"), Greater);
        assert_eq!(cmp_versions("0.1.2-rc.1", "0.1.2"), Less);
        // 三元组优先于预发布段
        assert_eq!(cmp_versions("0.1.3-alpha.1", "0.1.2-rc.9"), Greater);
        // rc 字母序 > alpha；同段数字按数值
        assert_eq!(cmp_versions("1.0.0-beta.2", "1.0.0-alpha.10"), Greater);
        assert_eq!(cmp_versions("1.0.0-alpha.10", "1.0.0-alpha.9"), Greater);
        // 相等与 build 元数据忽略
        assert_eq!(cmp_versions("1.2.3-rc.1", "1.2.3-rc.1"), Equal);
        assert_eq!(cmp_versions("1.2.3+build.7", "1.2.3"), Equal);
    }

    /* ── npm allow-scripts 拦截清单解析 ── */
    #[test]
    fn parses_allow_scripts_skipped_packages_from_real_warning_block() {
        let output = "\
npm warn deprecated node-domexception@1.0.0: Use your platform's native DOMException instead\n\
\n\
added 520 packages in 28s\n\
\n\
npm warn allow-scripts 5 packages have install scripts not yet covered by allowScripts:\n\
npm warn allow-scripts   @deepseek-ai/dsh-subprocess-local@0.1.2-rc.1 (postinstall: node scripts/ensure-spawn-helper.mjs)\n\
npm warn allow-scripts   koffi@3.2.1 (install: node ./cnoke.cjs -P . -D src/koffi --prebuild --release)\n\
npm warn allow-scripts   node-pty@1.2.0-beta.15 (install: node scripts/prebuild.js || node-gyp rebuild; postinstall: node scripts/post-install.js)\n\
npm warn allow-scripts   @google/genai@1.52.0 (preinstall: echo 'preinstall: no-op')\n\
npm warn allow-scripts   protobufjs@7.6.6 (postinstall: node scripts/postinstall)\n\
npm warn allow-scripts\n\
npm warn allow-scripts Run `npm install -g --allow-scripts=@deepseek-ai/dsh-subprocess-local,koffi,node-pty,@google/genai,protobufjs` to allow these scripts once, or `npm config set allow-scripts=… --location=user` to allow them for all global installs.\n\
";
        assert_eq!(
            parse_allow_scripts_skipped(output),
            vec![
                "@deepseek-ai/dsh-subprocess-local",
                "koffi",
                "node-pty",
                "@google/genai",
                "protobufjs"
            ]
        );
    }

    #[test]
    fn allow_scripts_parse_ignores_unrelated_output_and_dedups() {
        assert!(parse_allow_scripts_skipped("added 5 packages\nnpm warn cleanup foo").is_empty());
        // 同包出现两次只留一份（npm install 与 rebuild 输出拼接场景）
        let twice = "npm warn allow-scripts   koffi@3.2.1 (install: x)\n\
                     npm warn allow-scripts   koffi@3.2.1 (install: x)\n";
        assert_eq!(parse_allow_scripts_skipped(twice), vec!["koffi"]);
    }

    #[test]
    fn guard_blocks_next_minor_line_allows_current() {
        // 预发布段按其所属三元组参与比较：0.1.3-alpha.1 起视为需要壳配套适配
        // （0.1.3+ 可能引入新的 settings API/事件流端点变更）——必须拦
        assert!(version_triple("0.1.3-alpha.1").unwrap() > DSH_MAX_ADAPTED);
        assert!(version_triple("0.1.3").unwrap() > DSH_MAX_ADAPTED);
        assert!(version_triple("0.2.0").unwrap() > DSH_MAX_ADAPTED);
        // 0.1.2 系列（含 alpha/rc/正式）≤ 当前适配线 (0,1,2)：放行
        assert!(version_triple("0.1.2-alpha.2").unwrap() <= DSH_MAX_ADAPTED);
        assert!(version_triple("0.1.2-rc.1").unwrap() <= DSH_MAX_ADAPTED);
        assert!(version_triple("0.1.2").unwrap() <= DSH_MAX_ADAPTED);
        // 旧 0.1.1.x 也放行
        assert!(version_triple("0.1.1-rc.3").unwrap() <= DSH_MAX_ADAPTED);
        assert!(version_triple("0.1.1").unwrap() <= DSH_MAX_ADAPTED);
    }
}
