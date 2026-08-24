//! 启动状态：供加载页轮询（轮询为主通道，事件推送为辅，规避事件早于监听器挂载的竞态）。
use tauri::{Emitter, Manager};

#[derive(Default, Clone, serde::Serialize)]
pub struct StartupStatus {
    pub text: String,
    pub error: bool,
    pub ready: bool,
}

pub fn update(app: &tauri::AppHandle, text: &str, error: bool, ready: bool) {
    if let Some(state) = app.try_state::<crate::AppState>() {
        *state.status.lock().unwrap() = StartupStatus {
            text: text.to_string(),
            error,
            ready,
        };
    }
    let _ = app.emit(
        "startup-status",
        serde_json::json!({ "text": text, "error": error, "ready": ready }),
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
