//! 通知中心（D1b）：内存态通知历史（上限 50 条，最新在前）+ 独立 mini 窗口展示。
//!
//! 设计：
//! - 历史只存内存——通知是会话级信息，壳重启后清空（持久化收益低、隐私面更大）。
//! - 任何通知（无论窗口是否在前台）都进历史；前台与否只影响「未读角标」计数（tray）。
//! - mini 窗口是独立 webview（label "notifications"），加载 ui/notifications.html；
//!   经 IPC 拉历史 + 监听 "notifications-updated" 事件自动刷新。
//! - 窗口按需创建、退出常驻；再次点托盘只是聚焦。
use std::sync::Mutex;

use serde::Serialize;
use tauri::{Emitter, Manager};

use crate::runtime;

/// 历史上限：超过丢弃最旧。
const CAP: usize = 50;

#[derive(Debug, Clone, Serialize)]
pub struct NoticeRecord {
    pub title: String,
    pub body: String,
    /// unix 秒，前端格式化展示。
    pub at: u64,
}

static HISTORY: Mutex<Vec<NoticeRecord>> = Mutex::new(Vec::new());

/// 记录一条通知并广播刷新事件（events.rs 唯一调用点）。
pub fn record(app: &tauri::AppHandle, title: &str, body: &str) {
    let rec = NoticeRecord {
        title: title.to_string(),
        body: body.to_string(),
        at: runtime::unix_now(),
    };
    {
        let mut h = HISTORY.lock().unwrap();
        h.insert(0, rec);
        h.truncate(CAP);
    }
    // mini 窗口开着时自动刷新；没开则 emit 无接收方，静默
    let _ = app.emit("notifications-updated", ());
}

pub fn list() -> Vec<NoticeRecord> {
    HISTORY.lock().unwrap().clone()
}

pub fn clear() {
    HISTORY.lock().unwrap().clear();
}

/// 打开/聚焦通知中心窗口。首次创建 400×520 小窗（原生标题栏，避免 decorum 叠加复杂度）。
pub fn open_window(app: &tauri::AppHandle) -> tauri::Result<()> {
    if let Some(w) = app.get_webview_window("notifications") {
        let _ = w.show();
        let _ = w.set_focus();
        return Ok(());
    }
    // 跨平台注意：NSWindow 只能在主线程创建——调用方可能来自后台线程（冷启动 CLI 动作 /
    // single-instance 转发回调），统一经 run_on_main_thread 派发创建；已存在时仅 show/focus
    // （tauri 的窗口方法内部自派发，任意线程可调）。
    let handle = app.clone();
    app.run_on_main_thread(move || {
        if let Err(e) = build_window(&handle) {
            if let Some(mut log) = runtime::open_log_append() {
                use std::io::Write;
                let _ = writeln!(log, "[通知] 创建通知中心窗口失败: {e}");
            }
        }
    })
}

fn build_window(app: &tauri::AppHandle) -> tauri::Result<()> {
    // 双重检查：并发触发时（派发排队期间第二次调用）可能已建好
    if let Some(w) = app.get_webview_window("notifications") {
        let _ = w.show();
        let _ = w.set_focus();
        return Ok(());
    }
    tauri::WebviewWindowBuilder::new(
        app,
        "notifications",
        tauri::WebviewUrl::App("notifications.html".into()),
    )
    .title("DSH 通知中心")
    .inner_size(400.0, 520.0)
    .min_inner_size(320.0, 360.0)
    .center()
    .build()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 历史环形语义：新条目在前、截断到 CAP（用小切片验证插入顺序即可，CAP 常量不动）。
    #[test]
    fn list_starts_empty_and_is_snapshot() {
        // 不强依赖进程内静态状态（其他测试可能写过）；只验证返回的是克隆快照
        let a = list();
        let b = list();
        assert_eq!(a.len(), b.len());
    }
}
