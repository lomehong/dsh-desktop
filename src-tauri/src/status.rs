//! 启动状态：供加载页轮询（轮询为主通道，事件推送为辅，规避事件早于监听器挂载的竞态）。
//!
//! 字段流转设计（加载页按 s.wizard / s.connect / s.remote 分支渲染）：
//! - `wizard` / `connect` 是互斥的「界面态」标志：`wizard()`/`connect_screen()` 各自置位，
//!   `update()`（进度/错误/就绪）一律归 false——谁最后写入谁是真相。state 写入与 emit
//!   永远同帧同值，get_status 轮询与 startup-status 事件两条通道零歧义。
//! - `remote` 不是界面态而是「模式投影」：每次写入从 `AppState.mode` 现读，本模块
//!   只如实呈现、绝不修改模式；调用侧改完模式（`*state.mode=…`）后紧跟的 set/fail
//!   自然把新模式随下一帧带给加载页。唯一例外是 `wizard`（恒 false）：分身向导只在
//!   便携版首启出现，彼时 mode.txt 必不存在、模式必为 local，写死即真值。
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

/// AppState.mode 的只读投影：update / connect_screen 用它如实呈现当前模式。
fn remote_now(app: &tauri::AppHandle) -> bool {
    app.try_state::<crate::AppState>()
        .is_some_and(|s| *s.mode.lock().unwrap() == "remote")
}

pub fn update(app: &tauri::AppHandle, text: &str, error: bool, ready: bool) {
    // remote 现读现填，与 text/error/ready 同帧；connect/wizard 与常规进度互斥，归 false
    let remote = remote_now(app);
    if let Some(state) = app.try_state::<crate::AppState>() {
        *state.status.lock().unwrap() = StartupStatus {
            text: text.to_string(),
            error,
            ready,
            wizard: false,
            remote,
            connect: false,
        };
    }
    let _ = app.emit(
        "startup-status",
        serde_json::json!({ "text": text, "error": error, "ready": ready, "wizard": false, "remote": remote, "connect": false }),
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
/// 恒 remote:false——向导只在便携版首启出现，彼时模式必为 local（见模块注释）。
pub fn wizard(app: &tauri::AppHandle, text: &str) {
    if let Some(state) = app.try_state::<crate::AppState>() {
        *state.status.lock().unwrap() = StartupStatus {
            text: text.to_string(),
            error: false,
            ready: false,
            wizard: true,
            remote: false,
            connect: false,
        };
    }
    let _ = app.emit(
        "startup-status",
        serde_json::json!({ "text": text, "error": false, "ready": false, "wizard": true, "remote": false, "connect": false }),
    );
}

/// 进入「连接远程实例」连接屏：加载页显示地址/配对码表单（同 wizard 的一次性写入 +
/// emit 同一 JSON）。remote 走模式投影而非写死：连接失败后的远程模式下表单可被再次
/// 唤起，此时如实带 remote=true，避免与下一帧 update 的投影自相矛盾。
pub fn connect_screen(app: &tauri::AppHandle, text: &str) {
    let remote = remote_now(app);
    if let Some(state) = app.try_state::<crate::AppState>() {
        *state.status.lock().unwrap() = StartupStatus {
            text: text.to_string(),
            error: false,
            ready: false,
            wizard: false,
            remote,
            connect: true,
        };
    }
    let _ = app.emit(
        "startup-status",
        serde_json::json!({ "text": text, "error": false, "ready": false, "wizard": false, "remote": remote, "connect": true }),
    );
}
