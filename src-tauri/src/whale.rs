//! 全屏鲸鱼动画覆盖层：AI 诊断 / 修复执行期间的可见形态。
//! 用户指定形态（2026-09-28）：覆盖整个应用屏幕、主角为官方「白砖黑鲸」徽章、
//! 诊断 / 修复两种可辨动画状态、完成 / 失败有区分收尾。
//!
//! 实现：独立透明置顶窗口（ui/whale.html，纯动画零 IPC，无需 capability），
//! 状态由壳经 eval 调页面上的 window.__whaleState(state, text, sub) 驱动；
//! 窗口懒创建（首次 show 时主线程建，透明 + 置顶 + 无装饰 + 全屏 + 不进任务栏），
//! 之后常驻隐藏复用。结束形态播放约 1.4s 后由壳隐藏。
use tauri::{Manager, WebviewUrl};

const LABEL: &str = "whale";

fn js_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

fn log_line(msg: &str) {
    if let Some(mut log) = crate::runtime::open_log_append() {
        use std::io::Write;
        let _ = writeln!(log, "[守护] {msg}");
    }
}

/// 在主线程确保覆盖窗存在，然后下发状态并显示。
fn ensure_and_show(app: &tauri::AppHandle, js: &str) {
    let handle = app.clone();
    let js = js.to_string();
    let _ = app.run_on_main_thread(move || {
        let w = match handle.get_webview_window(LABEL) {
            Some(w) => w,
            None => match build_window(&handle) {
                Ok(w) => w,
                Err(e) => {
                    log_line(&format!("创建鲸鱼动画层失败: {e}"));
                    return;
                }
            },
        };
        // 先下发状态再显示：避免闪一帧默认空态
        let _ = w.eval(&js);
        let _ = w.show();
    });
}

fn build_window(
    app: &tauri::AppHandle,
) -> tauri::Result<tauri::WebviewWindow<tauri::Wry>> {
    // ⚠ WebView2 透明窗体在 fullscreen 模式下会失效（整层变不透明灰罩，实机
    // 2026-09-28）——绝不能用 fullscreen，按主显示器尺寸自建并贴 (0,0)。
    // 鼠标穿透（set_ignore_cursor_events）：覆盖层纯展示，鼠标/键盘全部直达下层，
    // 用户在动画期间可正常操作屏幕上的任何东西。
    let mut phys_w = 1920.0f64;
    let mut phys_h = 1080.0f64;
    let mut scale = 1.0f64;
    if let Ok(Some(m)) = app.primary_monitor() {
        let size = m.size();
        phys_w = size.width as f64;
        phys_h = size.height as f64;
        scale = m.scale_factor();
    }
    let lw = (phys_w / scale).max(1.0);
    let lh = (phys_h / scale).max(1.0);
    let w = tauri::WebviewWindowBuilder::new(
        app,
        LABEL,
        // 构建时间戳作查询参数：URL 每次构建必变，穿透 WebView2 跨进程的
        // HTTP 缓存（否则页面改版用户永远看到旧页，2026-09-29 三连零变化事故）
        WebviewUrl::App(concat!("whale.html?v=", env!("DSH_UI_BUILD_TS")).into()),
    )
    .title("DSH 守护 Agent")
    .decorations(false)
    .transparent(true)
    .always_on_top(true)
    .skip_taskbar(true)
    .focused(false)
    .resizable(false)
    .maximizable(false)
    .minimizable(false)
    .shadow(false)
    .visible(false)
    .inner_size(lw, lh)
    .position(0.0, 0.0)
    .build()?;
    let _ = w.set_ignore_cursor_events(true);
    Ok(w)
}

/// AI 动作开始：以指定状态显示覆盖层（诊断 / 修复两种动画形态）。
pub fn show(app: &tauri::AppHandle, state: &str, text: &str, sub: &str) {
    let js = format!(
        "window.__whaleState({},{},{})",
        js_str(state),
        js_str(text),
        js_str(sub)
    );
    ensure_and_show(app, &js);
}

/// AI 动作结束：播放完成 / 失败收尾形态，约 1.4s 后自动隐藏（独立线程定时，
/// 不占主线程）。
pub fn finish(app: &tauri::AppHandle, ok: bool, text: &str) {
    let state = if ok { "done" } else { "failed" };
    let js = format!(
        "window.__whaleState({},{},\"\")",
        js_str(state),
        js_str(text)
    );
    ensure_and_show(app, &js);
    let handle = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(1400));
        hide(&handle);
    });
}

pub fn hide(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window(LABEL) {
        let _ = w.hide();
    }
}
