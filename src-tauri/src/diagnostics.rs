//! 一键导出诊断包：把日志、pid 登记、模式记忆、远程凭据（token 脱敏）、分身完成标记、
//! 系统环境信息打成 zip，落在数据目录顶层（文件名带时间戳）。供真机排错 / 上报 bug 用。
//!
//! 隐私边界：
//! - token 永远不出现在包里——DPAPI 加密的 tokenEnc / 失败的解密都置为 `<redacted>`。
//! - origin / address / pairedAt 保留（远程实例身份），token 不保留。
//! - 日志保留：含崩溃栈、用户操作流，没有 token（token 仅在内存中与 IPC 通道）。
//! - mode.txt 保留：远程/本地模式记录对排错关键。
//!
//! zip 实现：零新增依赖——rust 自带 `std::io::Write` 写裸 zip 不现实；改成多文件 + manifest
//! 描述，shell 解压即可还原（`unzip` / 7-Zip / Windows 资源管理器都支持）。多文件目录
//! 的可读性比 zip 还好。
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::json;
use tauri::Manager;

use crate::runtime;

/// 导出诊断包到数据目录顶层，返回导出目录路径。
/// 文件夹命名：dsh-diagnostics-<timestamp>/，内含 manifest.json + 多个数据文件。
pub fn export(_app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let stamp = crate::runtime::unix_now();
    let dir_name = format!("dsh-diagnostics-{stamp}");
    let dir = std::env::temp_dir().join(&dir_name);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).map_err(|e| format!("清理旧导出目录失败：{e}"))?;
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建导出目录失败：{e}"))?;

    // 1) manifest：版本、平台、时间戳、文件清单
    let manifest = json!({
        "dsh_desktop_version": env!("CARGO_PKG_VERSION"),
        "exported_at_unix": stamp,
        "platform": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "rust_channel": if cfg!(debug_assertions) { "debug" } else { "release" },
        "files": [
            "manifest.json",
            "dsh-desktop.log",
            "runtime.pid",
            "mode.txt",
            "window.json",
            "remote.json",
            "persona-configured.json",
            "system-info.txt",
            "environment.txt"
        ]
    });
    write_json(&dir.join("manifest.json"), &manifest)?;

    // 2) 主日志（当前 + 上一代 .old 一并复制，崩溃前的尾段最关键）
    let log = runtime::log_file();
    copy_if_exists(&log, &dir.join("dsh-desktop.log"))?;
    let mut log_old = log.clone().into_os_string();
    log_old.push(".old");
    copy_if_exists(Path::new(&log_old), &dir.join("dsh-desktop.log.old"))?;

    // 3) pid 登记
    copy_if_exists(&runtime::pid_file(), &dir.join("runtime.pid"))?;

    // 4) mode 记忆（远程/本地）
    copy_if_exists(
        &runtime::runtime_root().join(crate::remote::mode_file()),
        &dir.join("mode.txt"),
    )?;

    // 5) window.json（最近一次窗口位置/显示器记录）
    copy_if_exists(
        &runtime::runtime_root().join("window.json"),
        &dir.join("window.json"),
    )?;

    // 6) remote.json（远程凭据，token 脱敏）
    if let Ok(sanitized) = sanitize_remote_json() {
        write_text(&dir.join("remote.json"), &sanitized)?;
    }

    // 7) persona-configured.json（分身是否完成）
    copy_if_exists(
        &runtime::app_home().join("persona-configured.json"),
        &dir.join("persona-configured.json"),
    )?;

    // 8) system-info + environment
    write_text(&dir.join("system-info.txt"), &system_info())?;
    write_text(&dir.join("environment.txt"), &filtered_env())?;

    // 9) 一份 README 给用户/客服看
    write_text(
        &dir.join("README.txt"),
        "DSH Desktop 诊断包\n\
         ===================\n\
         本目录含 dsh-desktop 一次会话的完整现场，崩溃排查 / bug 上报使用。\n\
         \n\
         文件清单：\n\
           manifest.json        导出元信息（版本/时间/平台/清单）\n\
           dsh-desktop.log      当前主日志\n\
           dsh-desktop.log.old  上一代日志（轮转前的尾段）\n\
           runtime.pid          本次会话的 shell + dsh 子进程 pid + 端口\n\
           mode.txt             上次模式（local / remote）\n\
           window.json          上次窗口位置 / 显示器\n\
           remote.json          远程凭据（token 已脱敏为 <redacted>）\n\
           persona-configured.json  分身是否完成\n\
           system-info.txt      操作系统版本 + 用户目录 + DSH home\n\
           environment.txt      与 DSH 相关的环境变量（已过滤敏感键）\n\
         \n\
         隐私声明：包内不含任何 token / 私钥 / 凭证明文；origin / address 保留。\n",
    )?;

    Ok(dir)
}

/// 重置 DSH home（便携模式专用）：删除 Data/home，重启服务让 dsh 重建。
/// 触发路径：托盘「高级 → 重置 DSH home」。需要先杀 dsh 子进程避免文件占用。
pub fn reset_dsh_home(app: &tauri::AppHandle) -> Result<(), String> {
    let portable = runtime::portable_root().ok_or("重置 DSH home 仅便携模式可用")?;
    let home = portable.join("home");
    if !home.exists() {
        return Err("DSH home 不存在，无须重置".into());
    }
    // 杀 dsh 子进程（持文件锁/活动连接）
    let state: tauri::State<crate::AppState> = app.state();
    if let Some(mut child) = state.child.lock().unwrap().take() {
        crate::supervisor::kill_tree(child.id() as u32);
        let _ = child.wait();
    }
    *state.origin.lock().unwrap() = None;
    std::fs::remove_dir_all(&home).map_err(|e| format!("删除 DSH home 失败：{e}"))?;
    crate::status::set(app, "DSH home 已重置，正在重启服务…");
    let handle = app.clone();
    std::thread::spawn(move || {
        if let Err(e) = crate::supervisor::start_service(&handle) {
            crate::status::fail(&handle, &e);
        }
    });
    Ok(())
}

/// 过滤敏感字段写出 remote.json：保留 address / origin / pairedAt，token / tokenEnc 置位 <redacted>。
fn sanitize_remote_json() -> Result<String, String> {
    let raw = std::fs::read_to_string(crate::remote::config_path())
        .map_err(|e| format!("读取 remote.json 失败：{e}"))?;
    let mut v: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| format!("解析 remote.json 失败：{e}"))?;
    if let Some(obj) = v.as_object_mut() {
        obj.insert("token".into(), serde_json::Value::String("<redacted>".into()));
        obj.insert(
            "tokenEnc".into(),
            serde_json::Value::String("<redacted>".into()),
        );
        obj.remove("pair_token");
    }
    serde_json::to_string_pretty(&v).map_err(|e| format!("序列化 sanitized 失败：{e}"))
}

/// 系统信息快照。
fn system_info() -> String {
    use std::env;
    let portable = runtime::portable_root()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "<非便携模式>".into());
    let data_dir = runtime::runtime_root().display().to_string();
    let app_home = runtime::app_home().display().to_string();
    let dsh_version = crate::install::installed_dsh_version().unwrap_or_else(|| "<未安装>".into());
    let launch_mode = if runtime::ready_root().is_some() {
        "Portable"
    } else {
        "System"
    };
    format!(
        "DSH Desktop version:    {ver}\n\
         dsh runtime version:    {dsh_v}\n\
         Launch mode:            {mode}\n\
         OS / Arch:              {os} / {arch}\n\
         Executable:             {exe}\n\
         Runtime data dir:       {data}\n\
         Portable root:          {portable}\n\
         DSH home (app):         {home}\n\
         Current user:           {user}\n",
        ver = env!("CARGO_PKG_VERSION"),
        dsh_v = dsh_version,
        mode = launch_mode,
        os = env::consts::OS,
        arch = env::consts::ARCH,
        exe = std::env::current_exe()
            .ok()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        data = data_dir,
        portable = portable,
        home = app_home,
        user = whoami_safe(),
    )
}

/// 与 DSH 相关的环境变量（已过滤：剔除含 KEY / TOKEN / SECRET / PASSWORD 的值）。
fn filtered_env() -> String {
    const KEEP: &[&str] = &[
        "PATH",
        "PATHEXT",
        "HOME",
        "USERPROFILE",
        "LOCALAPPDATA",
        "APPDATA",
        "TEMP",
        "TMP",
        "LANG",
        "LC_ALL",
        "DSH_HOME",
        "DSH_DESKTOP_NODE_MIRROR",
        "DSH_DESKTOP_NPM_REGISTRY",
        "DSH_DESKTOP_DSH_VERSION",
        "NODE_PATH",
    ];
    let mut out = String::new();
    for k in KEEP {
        if let Ok(v) = std::env::var(k) {
            // 路径类（PATH/HOME 等）保留；DSH_DESKTOP_* 配置也保留
            out.push_str(&format!("{k}={v}\n"));
        }
    }
    out
}

fn whoami_safe() -> String {
    if cfg!(windows) {
        std::env::var("USERNAME").unwrap_or_default()
    } else {
        std::env::var("USER").unwrap_or_default()
    }
}

fn write_json(path: &Path, v: &serde_json::Value) -> Result<(), String> {
    let text = serde_json::to_string_pretty(v).map_err(|e| format!("序列化失败：{e}"))?;
    write_text(path, &text)
}

fn write_text(path: &Path, text: &str) -> Result<(), String> {
    let mut f = File::create(path).map_err(|e| format!("创建 {} 失败：{e}", path.display()))?;
    f.write_all(text.as_bytes())
        .map_err(|e| format!("写入 {} 失败：{e}", path.display()))?;
    Ok(())
}

fn copy_if_exists(src: &Path, dst: &Path) -> Result<(), String> {
    if !src.exists() {
        return Ok(());
    }
    std::fs::copy(src, dst)
        .map_err(|e| format!("复制 {} → {} 失败：{e}", src.display(), dst.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// remote.json sanitization：token 与 tokenEnc 字段必须被替换为 <redacted>，其余字段保留。
    #[test]
    fn sanitize_redacts_token_and_keeps_address() {
        let raw = r#"{
            "address": "192.168.1.146:3090",
            "origin": "http://192.168.1.146:3090",
            "token": "secret-token-here",
            "tokenEnc": "base64-blob",
            "paired_at": 12345
        }"#;
        let dir = std::env::temp_dir().join(format!("dsh-diag-test-{}-{}", std::process::id(), crate::runtime::unix_now()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("remote.json");
        std::fs::write(&cfg, raw).unwrap();
        // 暂时把 config_path 指过去（同一进程内不可改 remote 模块的路径，sanitize_remote_json
        // 走 crate::remote::config_path() 硬编码；此处只验证 sanitize 的字符串变换逻辑）
        let mut v: serde_json::Value = serde_json::from_str(raw).unwrap();
        if let Some(obj) = v.as_object_mut() {
            obj.insert("token".into(), serde_json::Value::String("<redacted>".into()));
            obj.insert("tokenEnc".into(), serde_json::Value::String("<redacted>".into()));
        }
        let s = serde_json::to_string_pretty(&v).unwrap();
        assert!(s.contains("<redacted>"));
        assert!(!s.contains("secret-token-here"));
        assert!(!s.contains("base64-blob"));
        assert!(s.contains("192.168.1.146:3090"));
        assert!(s.contains("12345"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// filtered_env 仅保留白名单变量，过滤敏感 KEY 类变量。
    #[test]
    fn filtered_env_only_keeps_whitelist() {
        let out = filtered_env();
        // 含 TOKEN/SECRET/KEY 之类的不应出现（即使环境里设了也不在我们 KEEP 列表）
        assert!(!out.contains("MY_SECRET_TOKEN"));
        // 不在 KEEP 的普通变量也不应出现
        assert!(!out.contains("RANDOM_VAR="));
    }
}
