//! 启动状态：供加载页轮询（轮询为主通道，事件推送为辅，规避事件早于监听器挂载的竞态）。
//!
//! 字段流转设计（加载页按 s.wizard / s.connect / s.remote 分支渲染）：
//! - `wizard` / `connect` 是互斥的「界面态」标志：`wizard()`/`connect_screen()` 各自置位，
//!   `update()`（进度/错误/就绪）一律归 false——谁最后写入谁是真相。
//! - `remote` 不是界面态而是「模式投影」：每次写入从 `AppState.mode` 现读，本模块
//!   只如实呈现、绝不修改模式；调用侧改完模式（`*state.mode=…`）后紧跟的 set/fail
//!   自然把新模式随下一帧带给加载页。
//! - 每个入口只构造一次 StartupStatus，state 写入与 emit 用同一帧（克隆），六字段 ×
//!   双通道永不漂移。
use tauri::{Emitter, Manager};

#[derive(Default, Clone, serde::Serialize)]
pub struct StartupStatus {
    pub text: String,
    pub error: bool,
    pub ready: bool,
    /// 便携版首次启动（或托盘重新唤起）：加载页显示分身信息向导而不是进度。
    pub wizard: bool,
    /// 当前处于远程模式（AppState.mode 的只读投影，随每次状态写入同步）。
    pub remote: bool,
    /// 加载页显示「连接远程实例」连接屏（地址/配对码表单）而不是进度。
    pub connect: bool,
}

/// AppState.mode 的只读投影：update / wizard / connect_screen 用它如实呈现当前模式。
fn remote_now(app: &tauri::AppHandle) -> bool {
    app.try_state::<crate::AppState>()
        .is_some_and(|s| *s.mode.lock().unwrap() == "remote")
}

/// 构造一帧并双通道分发：state 写入克隆，emit 走原值（字段永不漂移）。
/// v0.1.28+ 同步刷新托盘 tooltip；v0.1.29+ 同步刷新托盘状态角标（C1）与
/// Windows 任务栏进度（D3——加载页进度条的同源 Rust 镜像）。
fn push_frame(app: &tauri::AppHandle, frame: StartupStatus) {
    if let Some(state) = app.try_state::<crate::AppState>() {
        *state.status.lock().unwrap() = frame.clone();
    }
    let _ = app.emit("startup-status", frame.clone());
    // 状态帧落盘后刷 tooltip / 图标角标 / 任务栏进度：
    // 托盘未初始化（启动早期 / 退出路径）时三者均静默 no-op
    crate::tray::refresh_tooltip(app);
    crate::tray::refresh_tray_icon(app);
    let percent = if frame.ready { Some(100) } else { stage_percent(&frame.text) };
    crate::webview::taskbar_progress(app, percent, frame.error, frame.ready);
}

/// 状态文案关键字 → 任务栏进度百分比（0~100）。与加载页 stagePercent 同一映射；
/// 未识别的阶段返回 None（保持当前进度）。纯函数便于单测。
fn stage_percent(text: &str) -> Option<u8> {
    if text.contains("下载") || text.contains("解压") {
        Some(15)
    } else if text.contains("安装 DSH") || text.contains("正在安装") {
        Some(45)
    } else if text.contains("自愈") || text.contains("修复") || text.contains("迁移") {
        Some(30)
    } else if text.contains("重启服务") || text.contains("切换到本地") || text.contains("重连远程") {
        Some(55)
    } else if text.contains("正在启动") {
        Some(65)
    } else if text.contains("等待服务就绪") || text.contains("配对成功") {
        Some(75)
    } else if text.contains("连接远程实例") {
        Some(80)
    } else {
        None
    }
}

pub fn update(app: &tauri::AppHandle, text: &str, error: bool, ready: bool) {
    // remote 现读现填，与 text/error/ready 同帧；connect/wizard 与常规进度互斥，归 false
    push_frame(
        app,
        StartupStatus {
            text: text.to_string(),
            error,
            ready,
            wizard: false,
            remote: remote_now(app),
            connect: false,
        },
    );
}

/// 记录一条进行中状态（未就绪、非错误）。
pub fn set(app: &tauri::AppHandle, text: &str) {
    update(app, text, false, false);
}

/// 记录一条错误状态。
pub fn fail(app: &tauri::AppHandle, text: &str) {
    update(app, text, true, false);
}

/// 进入分身向导状态：加载页显示向导表单，暂停自动启动。
/// remote 走模式投影而非写死：托盘「重新运行分身向导」（persona::reopen）在远程模式
/// 同样可达，写死 false 会发出与 AppState.mode 矛盾的帧。
pub fn wizard(app: &tauri::AppHandle, text: &str) {
    push_frame(
        app,
        StartupStatus {
            text: text.to_string(),
            error: false,
            ready: false,
            wizard: true,
            remote: remote_now(app),
            connect: false,
        },
    );
}

/// 进入「连接远程实例」连接屏：加载页显示地址/配对码表单（同 wizard 的一次性写入 +
/// emit 同一 JSON）。remote 同样走模式投影：连接失败后的远程模式下表单可被再次唤起。
pub fn connect_screen(app: &tauri::AppHandle, text: &str) {
    push_frame(
        app,
        StartupStatus {
            text: text.to_string(),
            error: false,
            ready: false,
            wizard: false,
            remote: remote_now(app),
            connect: true,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// D3：阶段关键字 → 进度映射（与加载页 stagePercent 同表）。
    #[test]
    fn stage_percent_maps_known_keywords() {
        assert_eq!(stage_percent("正在下载 Node v24（镜像加速）…"), Some(15));
        assert_eq!(stage_percent("正在解压 Node…"), Some(15));
        assert_eq!(stage_percent("正在安装 DSH v0.1.2-alpha.5（首次约 1~3 分钟）…"), Some(45));
        assert_eq!(stage_percent("检测到运行时不兼容，正在自动修复…"), Some(30));
        assert_eq!(stage_percent("正在重启服务…"), Some(55));
        assert_eq!(stage_percent("正在启动 DSH 服务…"), Some(65));
        assert_eq!(stage_percent("等待服务就绪（首次启动约需 10~60 秒）…"), Some(75));
        assert_eq!(stage_percent("连接远程实例 192.168.1.146:3090…"), Some(80));
        // 未识别：None（任务栏保持当前进度）
        assert_eq!(stage_percent("服务已就绪"), None);
        assert_eq!(stage_percent(""), None);
    }
}
