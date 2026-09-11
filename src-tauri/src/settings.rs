//! 壳级启动配置（launcher.json）+ 独立配置窗（settings.html）。
//! 存储与 window_state/remote.json 同区（runtime_root），当前唯一配置项为
//! 服务固定端口：默认随机（--port 0，OS 分配，永不冲突），配置了固定端口
//! 则在端口空闲时使用之，被占用自动回退随机——绝不因端口冲突起不来。

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::net::TcpListener;
use std::path::PathBuf;
use tauri::Manager;

/// 壳级配置（持久化于 runtime_root/launcher.json）。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct LauncherCfg {
    /// 固定服务端口；None = 随机端口（默认）。仅 1024 及以上可保存（避开系统端口）。
    #[serde(default)]
    pub fixed_port: Option<u16>,
}

pub fn config_path() -> PathBuf {
    crate::runtime::runtime_root().join("launcher.json")
}

/// 读取配置；文件缺失/损坏一律回退默认（配置页是锦上添花，绝不阻断启动）。
pub fn load() -> LauncherCfg {
    load_from(&config_path())
}

fn load_from(path: &PathBuf) -> LauncherCfg {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn save(cfg: &LauncherCfg) -> Result<(), String> {
    save_to(&config_path(), cfg)
}

fn save_to(path: &PathBuf, cfg: &LauncherCfg) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string_pretty(cfg).map_err(|e| e.to_string())?;
    std::fs::write(path, text).map_err(|e| e.to_string())
}

/// 已配置的固定端口（spawn_dsh 决策用）。
pub fn fixed_port() -> Option<u16> {
    load().fixed_port
}

/// 端口空闲探测：临时绑定 127.0.0.1:p 成功即视为可用。探测到 spawn 之间存在
/// 极小竞窗，服务实际端口仍以 stdout 报告为准，误判只导致本次回退随机，无碍。
pub fn port_free(p: u16) -> bool {
    TcpListener::bind(("127.0.0.1", p)).is_ok()
}

/// 纯函数便于单测：端口决策——未配置或被占一律回退随机（0），绝不因端口冲突起不来。
pub fn decide_spawn_port(fixed: Option<u16>, fixed_busy: bool) -> u16 {
    match fixed {
        Some(p) if !fixed_busy => p,
        _ => 0,
    }
}

/// 启动后预热：隐藏建窗，首次打开零渲染进程孵化。失败静默（惰性创建兜底）。
pub fn warm_settings_window(app: &tauri::AppHandle) -> tauri::Result<()> {
    if app.get_webview_window("settings").is_some() {
        return Ok(());
    }
    let handle = app.clone();
    app.run_on_main_thread(move || {
        let _ = build_settings_window(&handle, false);
    })
}

/// 打开独立配置窗（幂等：已存在则 show+focus）。模式照 remote_account::open_control_window——
/// 跨平台建窗必须主线程；不用 decorum（懒创建窗口里按钮注入有 page-load 竞态，真机踩过），
/// 无边框 + settings.html 自绘标题栏（拖拽/最小化/最大化/关闭）。
pub fn open_settings_window(app: &tauri::AppHandle) -> tauri::Result<()> {
    if let Some(w) = app.get_webview_window("settings") {
        let _ = w.show();
        let _ = w.set_focus();
        return Ok(());
    }
    let handle = app.clone();
    app.run_on_main_thread(move || {
        if let Err(e) = build_settings_window(&handle, true) {
            if let Some(mut log) = crate::runtime::open_log_append() {
                let _ = writeln!(log, "[设置] 创建配置窗失败: {e}");
            }
        }
    })
}

fn build_settings_window(app: &tauri::AppHandle, visible: bool) -> tauri::Result<()> {
    // 双重检查：并发触发时（派发排队期间第二次调用）可能已建好
    if let Some(w) = app.get_webview_window("settings") {
        if visible {
            let _ = w.show();
            let _ = w.set_focus();
        }
        return Ok(());
    }
    let mut builder = tauri::WebviewWindowBuilder::new(
        app,
        "settings",
        tauri::WebviewUrl::App("settings.html".into()),
    )
    .title("DSH 设置")
    .inner_size(620.0, 580.0)
    .min_inner_size(540.0, 480.0)
    .center()
    .visible(visible)
    // 本机页面之外一律拒绝：配置窗不承担浏览职责（无外链），被导航即异常
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
    // 关闭 = 隐藏（暖窗常驻）：避免下次打开重新孵化 WebView2 渲染进程（同 about.rs）
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

/* ── Tauri 命令（配置页调用；自定义命令不受 capabilities 约束，无需列出） ── */

/// 配置 + 只读运行时信息（版本/数据目录/日志路径），配置页首屏一次取齐。
/// 带 caller_is_local 守卫：自定义命令不受 capabilities 约束，命令层是最后一道边界
/// （数据目录/版本属主机信息，Harness 页面调用一律拒绝）。
#[tauri::command]
pub fn settings_load(window: tauri::WebviewWindow) -> Result<serde_json::Value, String> {
    if !crate::caller_is_local(&window) {
        return Err("无权限".into());
    }
    let cfg = load();
    Ok(serde_json::json!({
        "fixedPort": cfg.fixed_port,
        "dshVersion": crate::install::installed_dsh_version(),
        "runtimeRoot": crate::runtime::runtime_root().display().to_string(),
        "logFile": crate::runtime::log_file().display().to_string(),
    }))
}

/// 保存端口配置。None = 随机端口。生效时机为下次启动服务（配置页有提示，
/// 立即生效走托盘「重启服务」，由用户主动触发）。守卫理由同 settings_load。
#[tauri::command]
pub fn settings_save(
    window: tauri::WebviewWindow,
    fixed_port: Option<u16>,
) -> Result<(), String> {
    if !crate::caller_is_local(&window) {
        return Err("无权限".into());
    }
    if let Some(p) = fixed_port {
        if p < 1024 {
            return Err("固定端口需 ≥ 1024（避开系统常用端口）".into());
        }
        if !port_free(p) {
            return Err(format!("端口 {p} 当前被占用：仍可保存，启动时会自动回退随机端口"));
        }
    }
    save(&LauncherCfg { fixed_port })?;
    if let Some(mut log) = crate::runtime::open_log_append() {
        use std::io::Write;
        let _ = writeln!(log, "[设置] 端口配置已保存: fixed_port={fixed_port:?}（下次启动服务生效）");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_roundtrip_and_corrupt_tolerated() {
        let dir = std::env::temp_dir().join(format!("dsh-settings-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("launcher.json");

        // 缺省文件 → 默认（随机端口）
        let _ = std::fs::remove_file(&path);
        assert_eq!(load_from(&path), LauncherCfg::default());

        // 写入 → 读回一致
        save_to(&path, &LauncherCfg { fixed_port: Some(3080) }).unwrap();
        assert_eq!(load_from(&path).fixed_port, Some(3080));

        // 损坏文件 → 默认而非报错
        std::fs::write(&path, "{broken").unwrap();
        assert_eq!(load_from(&path), LauncherCfg::default());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn decide_spawn_port_falls_back_on_busy_or_unset() {
        assert_eq!(decide_spawn_port(None, false), 0);
        assert_eq!(decide_spawn_port(Some(3080), false), 3080);
        // 被占：回退随机，绝不因端口冲突起不来
        assert_eq!(decide_spawn_port(Some(3080), true), 0);
    }

    #[test]
    fn port_free_rejects_bound_port() {
        // 自绑一个端口再探测：必然不可用（自证探测真的在探测）
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let bound = listener.local_addr().unwrap().port();
        assert!(!port_free(bound));
    }
}
