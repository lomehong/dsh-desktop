//! 关于窗口（about.html）：版本信息、项目链接、日志/数据目录快捷入口与检查更新。
//!
//! 窗口形态照 settings.rs 的既有结论：**懒创建窗口不用 decorum**（真机踩过 page-load
//! 注入竞态），无边框 + HTML 自绘标题栏；About 为固定尺寸面板，标题栏只保留关闭钮
//! （最小化/最大化对固定尺寸窗口无意义）。另加 on_navigation 守卫：About 窗永远只能
//! 停在本机页面上，绝不会被导航成浏览器窗口。
//!
//! 四个命令全部带 `caller_is_local` 守卫——自定义命令不受 capabilities 约束，命令层
//! 是最后一道边界（Harness 页面调用一律拒绝）。
//!
//! 「打开日志 / 数据目录」直接复用已注册的 `open_log` / `open_runtime_dir`，零新增。

use std::io::Write;
use tauri::Manager;

/// 项目主页（与 git remote 一致；自动更新端点同仓库 Releases）。
pub const REPO_URL: &str = "https://github.com/lomehong/dsh-desktop";

/// 平台标签（纯函数便于单测）：编译期 OS/ARCH 常量映射为可读串，未知组合原样透出。
fn platform_label(os: &str, arch: &str) -> String {
    let os_name = match os {
        "macos" => "macOS",
        "windows" => "Windows",
        "linux" => "Linux",
        other => other,
    };
    let arch_name = match arch {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        "x86" => "x86",
        other => other,
    };
    format!("{os_name} {arch_name}")
}

/// 启动后预热：隐藏建窗，首次打开零渲染进程孵化。失败静默（惰性创建兜底）。
pub fn warm_about_window(app: &tauri::AppHandle) -> tauri::Result<()> {
    if app.get_webview_window("about").is_some() {
        return Ok(());
    }
    let handle = app.clone();
    app.run_on_main_thread(move || {
        let _ = build_about_window(&handle, false);
    })
}

/// 打开关于窗口（幂等：已存在则 show+focus）。
pub fn open_about_window(app: &tauri::AppHandle) -> tauri::Result<()> {
    if let Some(w) = app.get_webview_window("about") {
        let _ = w.show();
        let _ = w.set_focus();
        return Ok(());
    }
    let handle = app.clone();
    app.run_on_main_thread(move || {
        if let Err(e) = build_about_window(&handle, true) {
            if let Some(mut log) = crate::runtime::open_log_append() {
                let _ = writeln!(log, "[关于] 创建关于窗口失败: {e}");
            }
        }
    })
}

fn build_about_window(app: &tauri::AppHandle, visible: bool) -> tauri::Result<()> {
    // 双重检查：并发触发时（派发排队期间第二次调用）可能已建好
    if let Some(w) = app.get_webview_window("about") {
        if visible {
            let _ = w.show();
            let _ = w.set_focus();
        }
        return Ok(());
    }
    let mut builder = tauri::WebviewWindowBuilder::new(
        app,
        "about",
        tauri::WebviewUrl::App("about.html".into()),
    )
    .title("关于 DSH Desktop")
    .inner_size(460.0, 560.0)
    .resizable(false)
    .center()
    .visible(visible)
    // 本机页面之外一律拒绝（About 窗不承担浏览职责；外链走 about_open_repo 交系统浏览器）
    .on_navigation(|url| crate::webview::is_local_url(url.as_str()));
    #[cfg(not(target_os = "macos"))]
    {
        builder = builder.decorations(false);
    }
    #[cfg(target_os = "macos")]
    {
        builder = builder
            .title_bar_style(tauri::TitleBarStyle::Overlay)
            .hidden_title(true);
    }
    let window = builder.build()?;
    // 关闭 = 隐藏（暖窗常驻）：关闭即销毁会让下次打开重新孵化 WebView2 渲染进程
    // （Windows 上 1~3s，杀软扫描加重——「关于窗口打开非常慢」的根因）。页面自绘
    // 关闭钮走 selfWin.close()，同样触发 CloseRequested，此处统一拦成隐藏。
    let win = window.clone();
    window.on_window_event(move |event| {
        if let tauri::WindowEvent::CloseRequested { api, .. } = event {
            api.prevent_close();
            let _ = win.hide();
        }
    });
    if visible {
        let _ = window.show();
        let _ = window.set_focus();
    }
    Ok(())
}

/* ── Tauri 命令（关于页调用；自定义命令不受 capabilities 约束，守卫在命令层） ── */

/// 版本与路径信息（关于页首屏一次取齐）。appVersion 来自编译期 Cargo 包版本
/// （CI 用 scripts/sync-version.py 从 tag 写入，发行版准确）。
#[tauri::command]
pub fn about_info(window: tauri::WebviewWindow) -> Result<serde_json::Value, String> {
    if !crate::caller_is_local(&window) {
        return Err("无权限".into());
    }
    Ok(serde_json::json!({
        "appVersion": env!("CARGO_PKG_VERSION"),
        "dshVersion": crate::install::installed_dsh_version(),
        "platform": platform_label(std::env::consts::OS, std::env::consts::ARCH),
        "repoUrl": REPO_URL,
        "runtimeRoot": crate::runtime::runtime_root().display().to_string(),
        "logFile": crate::runtime::log_file().display().to_string(),
    }))
}

/// 检查应用更新：只查不装（是否下载安装由用户再点一次决定）。
/// 返回 `{latest: string|null}`——非 null 即存在更新版本（updater.check 已做版本比较）。
#[tauri::command]
pub async fn about_check_update(
    window: tauri::WebviewWindow,
) -> Result<serde_json::Value, String> {
    if !crate::caller_is_local(&window) {
        return Err("无权限".into());
    }
    use tauri_plugin_updater::UpdaterExt;
    let app = window.app_handle().clone();
    let updater = app
        .updater_builder()
        .build()
        .map_err(|e| format!("更新器初始化失败：{e}"))?;
    match updater.check().await {
        Ok(Some(update)) => Ok(serde_json::json!({ "latest": update.version })),
        Ok(None) => Ok(serde_json::json!({ "latest": null })),
        Err(e) => Err(format!("检查更新失败：{e}")),
    }
}

/// 下载并安装更新，完成后自动重启应用（不返回：进程已重启）。
/// 前端在此期间显示不确定态文案；失败经 Result 回显。
#[tauri::command]
pub async fn about_install_update(window: tauri::WebviewWindow) -> Result<(), String> {
    if !crate::caller_is_local(&window) {
        return Err("无权限".into());
    }
    use tauri_plugin_updater::UpdaterExt;
    let app = window.app_handle().clone();
    let updater = app
        .updater_builder()
        .build()
        .map_err(|e| format!("更新器初始化失败：{e}"))?;
    // 重新 check：区间内状态可能已变（对齐托盘 check_app_update 的做法）
    let Some(update) = updater
        .check()
        .await
        .map_err(|e| format!("检查更新失败：{e}"))?
    else {
        return Err("已是最新版本".into());
    };
    if let Some(mut log) = crate::runtime::open_log_append() {
        let _ = writeln!(log, "[关于] 开始下载安装更新 v{}", update.version);
    }
    let on_done = || app.restart();
    update
        .download_and_install(|_, _| {}, on_done)
        .await
        .map_err(|e| format!("更新安装失败：{e}"))?;
    Ok(())
}

/// 用系统浏览器打开项目主页。无参窄接口：不做通用 open_url(String)，
/// 避免把「打开任意 URL」的能力暴露给页面。
#[tauri::command]
pub fn about_open_repo(window: tauri::WebviewWindow) -> Result<(), String> {
    if !crate::caller_is_local(&window) {
        return Err("无权限".into());
    }
    crate::webview::open_external(REPO_URL);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_label_maps_known_combos_and_passes_through_unknown() {
        assert_eq!(platform_label("macos", "aarch64"), "macOS arm64");
        assert_eq!(platform_label("macos", "x86_64"), "macOS x64");
        assert_eq!(platform_label("windows", "x86_64"), "Windows x64");
        assert_eq!(platform_label("linux", "aarch64"), "Linux arm64");
        // 未知组合原样透出，不丢信息
        assert_eq!(platform_label("freebsd", "riscv64"), "freebsd riscv64");
        // 实际运行平台的组合必然可读（非空、无下划线残留）
        let actual = platform_label(std::env::consts::OS, std::env::consts::ARCH);
        assert!(!actual.is_empty() && !actual.contains('_'), "{actual}");
    }
}
