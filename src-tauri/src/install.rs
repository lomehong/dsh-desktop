//! 运行时安装与升级：便携 Node + 固定版本 dsh 装入应用数据目录
//! （Windows %LOCALAPPDATA%\dsh-desktop，macOS ~/Library/Application Support/dsh-desktop）。
//! 全程使用系统自带工具（curl 下载、tar 解压：Windows/macOS 为 bsdtar，Linux 为 gnu tar），
//! 零新增 Rust 依赖；下载走 npmmirror 镜像，nodejs.org / npm 官方源兜底。
use std::path::PathBuf;
use std::process::Command;

use crate::runtime::{self, no_window};
use crate::{status, supervisor};
use tauri::Manager;

/// 固定的 dsh 基线版本（全新环境首装用；升级走 npm latest，可用 DSH_DESKTOP_DSH_VERSION 固定）。
pub const DSH_VERSION: &str = "0.1.2-alpha.4";
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

/// 查询 npm registry 上 @deepseek-ai/dsh 指定 dist-tag 的版本。
fn dist_tag_version(tag: &str) -> Result<String, String> {
    let npm = npm_tool().ok_or("便携运行时未安装，无法查询版本")?;
    let mut last_err = String::new();
    for registry in npm_registry() {
        let mut c = if cfg!(windows) {
            let mut c = Command::new("cmd.exe");
            c.args(["/C"]).arg(&npm);
            c
        } else {
            Command::new(&npm)
        };
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

/// 升级通道选择：已装 alpha 的用户跟随 alpha tag，其余跟随 latest。
/// 避免「升级按钮变降级」——latest 目前指向旧稳定线，alpha 用户点升级会被拉回去。
fn upgrade_channel() -> &'static str {
    match installed_dsh_version() {
        Some(v) if v.contains("-alpha") => "alpha",
        _ => "latest",
    }
}

/// 升级目标版本：DSH_DESKTOP_DSH_VERSION 显式指定优先；否则按当前通道取 tag。
/// 通道 tag 查询失败（如 alpha tag 不存在）时回退 latest。
fn target_version() -> Result<String, String> {
    if let Ok(v) = std::env::var("DSH_DESKTOP_DSH_VERSION") {
        if !v.is_empty() {
            return Ok(v);
        }
    }
    let channel = upgrade_channel();
    match dist_tag_version(channel) {
        Ok(v) => Ok(v),
        Err(e) if channel != "latest" => dist_tag_version("latest").map_err(|_| e),
        Err(e) => Err(e),
    }
}

/// 安装指定版本的 dsh 到活动便携运行时（输出落日志）。
fn npm_install_dsh(version: &str) -> Result<(), String> {
    let node_dir = active_root().join("node");
    let npm_cmd = npm_tool().ok_or("便携 Node 缺少 npm")?;
    let mut last_err = String::new();
    for registry in npm_registry() {
        let mut log = runtime::open_log_append();
        let mut c = if cfg!(windows) {
            let mut c = Command::new("cmd.exe");
            c.args(["/C"]).arg(&npm_cmd);
            c
        } else {
            Command::new(&npm_cmd)
        };
        c.args(["install", "-g", &format!("@deepseek-ai/dsh@{version}"), "--prefix"])
            .arg(&node_dir)
            .arg(&registry);
        prepend_node_path(&mut c);
        if let Some(f) = log.as_mut() {
            use std::io::Write;
            let _ = writeln!(f, "[npm] registry={registry} target={version}");
            if let Some(o) = f.try_clone().ok() { c.stdout(std::process::Stdio::from(o)); }
            if let Some(e) = f.try_clone().ok() { c.stderr(std::process::Stdio::from(e)); }
        }
        match no_window(&mut c).status() {
            Ok(s) if s.success() => return Ok(()),
            Ok(_) => last_err = "npm 退出码非零（详见日志）".into(),
            Err(e) => last_err = e.to_string(),
        }
    }
    Err(format!("DSH v{version} 安装失败：{last_err}"))
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
/// 安装完成后自动修复插件符号链接。
pub fn ensure_runtime_locked(app: &tauri::AppHandle) -> Result<(), String> {
    install_runtime_inner(app)?;
    repair_plugin_symlinks();
    Ok(())
}

/// 修复插件符号链接：扫描 DSH_HOME 下的自定义插件目录，将断裂的
/// node_modules/@deepseek-ai 符号链接重新指向当前便携运行时的包目录。
/// 静默执行——修复失败不阻断启动（插件加载失败由 DSH 自身报错）。
fn repair_plugin_symlinks() {
    let dsh_home = match std::env::var("DSH_HOME") {
        Ok(h) if !h.is_empty() => PathBuf::from(h),
        _ => {
            let home = std::env::var(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).unwrap_or_default();
            PathBuf::from(home).join(".dsh")
        }
    };
    if !dsh_home.is_dir() {
        return;
    }
    // 便携运行时中 @deepseek-ai 包的实际位置
    let root = runtime::runtime_root();
    let target = {
        let mut p = root.join("node");
        if !cfg!(windows) {
            p = p.join("lib");
        }
        p.join("node_modules").join("@deepseek-ai")
    };
    if !target.is_dir() {
        return;
    }
    // 扫描 DSH_HOME 下的子目录，找含 node_modules/@deepseek-ai 的插件
    let entries = match std::fs::read_dir(&dsh_home) {
        Ok(e) => e,
        Err(_) => return,
    };
    let mut repaired = 0u32;
    for entry in entries.flatten() {
        let plugin_dir = entry.path();
        if !plugin_dir.is_dir() {
            continue;
        }
        let nm_scope = plugin_dir.join("node_modules").join("@deepseek-ai");
        // 只处理符号链接（目录或文件形式均可能）
        let is_symlink = std::fs::symlink_metadata(&nm_scope)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false);
        if !is_symlink {
            continue;
        }
        // 检查链接目标是否有效
        if nm_scope.exists() {
            continue; // 链接仍有效，跳过
        }
        // 断链：删除旧链接，重建指向当前运行时
        let _ = std::fs::remove_file(&nm_scope);
        #[cfg(windows)]
        {
            use std::os::windows::fs::symlink_dir;
            if symlink_dir(&target, &nm_scope).is_ok() {
                repaired += 1;
            }
        }
        #[cfg(unix)]
        {
            if std::os::unix::fs::symlink(&target, &nm_scope).is_ok() {
                repaired += 1;
            }
        }
    }
    if repaired > 0 {
        if let Some(mut log) = runtime::open_log_append() {
            use std::io::Write;
            let _ = writeln!(log, "[自愈] 修复了 {repaired} 个插件符号链接 → {}", target.display());
        }
    }
}

/// 安装便携运行时（幂等）：Node 缺则下载解压，dsh 缺则 npm -g 安装固定版本。
/// 每步经 status 更新到加载页。供首启引导与托盘升级共用。
pub fn install_runtime(app: &tauri::AppHandle) -> Result<(), String> {
    let state: tauri::State<crate::AppState> = app.state();
    {
        let mut r = state.restarting.lock().unwrap();
        if *r {
            return Ok(());
        }
        *r = true;
    }
    let result = install_runtime_inner(app);
    *state.restarting.lock().unwrap() = false;
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
