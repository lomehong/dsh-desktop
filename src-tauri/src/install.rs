//! 运行时安装与升级：便携 Node + 固定版本 dsh 装入 %LOCALAPPDATA%\dsh-desktop。
//! 全程使用系统自带工具（curl.exe 下载、tar.exe 解压），零新增 Rust 依赖；
//! 下载走 npmmirror 镜像，nodejs.org / npm 官方源兜底。
use std::path::PathBuf;
use std::process::Command;

use crate::runtime::{self, no_window};
use crate::{status, supervisor};
use tauri::Manager;

/// 固定的 dsh 版本（与今日全链路验证的版本一致；升级随应用版本走）。
pub const DSH_VERSION: &str = "0.1.1-rc.2";
/// 便携 Node 版本（dsh rc.x 的 zstd 要求需要 Node 24）。
const NODE_VERSION: &str = "24.19.0";

fn node_zip_name() -> String {
    format!("node-v{NODE_VERSION}-win-x64.zip")
}

fn node_mirror_urls() -> Vec<String> {
    let name = node_zip_name();
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

/// 下载单个文件：优先系统 curl.exe，PowerShell Invoke-WebRequest 兜底。
fn download(url: &str, dest: &PathBuf) -> Result<(), String> {
    let mut c = Command::new("curl.exe");
    c.args(["-L", "--fail", "--connect-timeout", "20", "-o"]);
    c.arg(dest);
    c.arg(url);
    match no_window(&mut c).status() {
        Ok(s) if s.success() => return Ok(()),
        _ => {}
    }
    let mut p = Command::new("powershell");
    p.args(["-NoProfile", "-Command", &format!(
        "Invoke-WebRequest -Uri '{}' -OutFile '{}'",
        url,
        dest.display()
    )]);
    no_window(&mut p)
        .status()
        .map_err(|e| format!("下载失败 ({url}): {e}"))
        .and_then(|s| if s.success() { Ok(()) } else { Err(format!("下载失败 ({url})")) })
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

    // 1) 便携 Node：缺失则下载解压（zip 顶层目录改名为 node）
    if !runtime::node_exe().exists() {
        status::set(app, &format!("正在下载 Node v{NODE_VERSION}（约 30MB，镜像加速）…"));
        let downloads = root.join("downloads");
        std::fs::create_dir_all(&downloads).map_err(|e| format!("{e}"))?;
        let zip = downloads.join(node_zip_name());
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
        let mut c = Command::new("tar.exe");
        c.args(["-xf"]).arg(&zip).arg("-C").arg(&extract_to);
        no_window(&mut c)
            .status()
            .map_err(|e| format!("解压失败: {e}"))
            .and_then(|s| if s.success() { Ok(()) } else { Err("解压失败".into()) })?;
        let inner = extract_to.join(format!("node-v{NODE_VERSION}-win-x64"));
        let _ = std::fs::remove_dir_all(&node_dir);
        std::fs::rename(&inner, &node_dir).map_err(|e| format!("安装 Node 失败: {e}"))?;
        let _ = std::fs::remove_dir_all(&extract_to);
        let _ = std::fs::remove_file(&zip);
    }

    // 2) dsh 固定版本：便携 npm -g 装入 node 目录（输出追加到日志，便于诊断镜像/网络故障）
    if !runtime::dsh_bin_js().exists() {
        status::set(app, &format!("正在安装 DSH v{DSH_VERSION}（首次约 1~3 分钟）…"));
        let npm_cmd = node_dir.join("npm.cmd");
        if !npm_cmd.exists() {
            return Err("便携 Node 缺少 npm，请删除数据目录后重试。".into());
        }
        let mut last_err = String::new();
        let mut ok = false;
        for registry in npm_registry() {
            let mut log = runtime::open_log_append();
            let mut c = Command::new("cmd.exe");
            c.args(["/C"])
                .arg(&npm_cmd)
                .args(["install", "-g", &format!("@deepseek-ai/dsh@{DSH_VERSION}"), "--prefix"])
                .arg(&node_dir)
                .arg(&registry);
            if let Some(f) = log.as_mut() {
                use std::io::Write;
                let _ = writeln!(f, "[npm] registry={registry}");
                let out = f.try_clone().ok();
                let err = f.try_clone().ok();
                if let Some(o) = out { c.stdout(std::process::Stdio::from(o)); }
                if let Some(e) = err { c.stderr(std::process::Stdio::from(e)); }
            }
            match no_window(&mut c).status() {
                Ok(s) if s.success() => {
                    ok = true;
                    break;
                }
                Ok(_) => last_err = "npm 退出码非零（详见日志）".into(),
                Err(e) => last_err = e.to_string(),
            }
        }
        if !ok {
            return Err(format!("DSH 安装失败：{last_err}（可重试；网络受限可设置 DSH_DESKTOP_NPM_REGISTRY）"));
        }
    }
    Ok(())
}

/// 托盘「升级 DSH」：停服务 → 重装固定版本 → 重新启动。
pub fn upgrade_runtime(app: &tauri::AppHandle) {
    status::set(app, "正在升级 DSH 运行时…");
    // 先停服务，避免替换运行中的文件
    let state: tauri::State<crate::AppState> = app.state();
    if let Some(mut child) = state.child.lock().unwrap().take() {
        supervisor::kill_tree(child.id() as u32);
        let _ = child.wait();
    }
    *state.origin.lock().unwrap() = None;
    crate::webview::navigate_to_loader(app);
    // 删除已装 dsh 让安装流程重装（Node 保留）
    let dsh_dir = runtime::runtime_root()
        .join("node")
        .join(if cfg!(windows) { "node_modules" } else { "lib/node_modules" })
        .join("@deepseek-ai")
        .join("dsh");
    let _ = std::fs::remove_dir_all(dsh_dir);
    if let Err(e) = install_runtime(app) {
        status::fail(app, &e);
        return;
    }
    status::set(app, "升级完成，正在启动服务…");
    if let Err(e) = supervisor::start_service(app) {
        status::fail(app, &e);
    }
}

/// 首启安装入口：安装完成后自动续跑启动序列。
pub fn install_and_start(app: &tauri::AppHandle) {
    if let Err(e) = install_runtime(app) {
        status::fail(app, &e);
        return;
    }
    status::set(app, "运行环境就绪，正在启动服务…");
    if let Err(e) = supervisor::start_service(app) {
        status::fail(app, &e);
    }
}
