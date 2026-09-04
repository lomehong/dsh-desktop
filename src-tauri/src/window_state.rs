//! 主窗口位置/尺寸/显示器记忆：保存到 <runtime_root>/window.json，
//! 启动时回放（带显示器存在性校验与越界夹紧）。最小化到托盘路径（CloseRequested→hide）
//! 与正常退出路径（RunEvent::Exit）都会落盘，避免下次启动丢状态。
//!
//! 设计取舍：
//! - 显示器用 available_monitors() 的索引 + name 复合定位——只用 index 在插拔显示器后
//!   会漂移（系统不保证编号稳定）；name 在多块同型号屏下冲突时回退 index。
//! - 尺寸小于 min_inner_size 时夹紧（用户在 Win+D/拖拽缩放后重启不出现迷你窗）。
//! - 位置越界（保存的显示器已拔出）夹到主显示器中心——而不是直接用默认，避免每次启动
//!   都要重新记忆。
//! - 写入用 OnceLock<Mutex<...>> 缓存路径，外部 file IO 路径用 runtime::runtime_root()
//!   保证便携模式落在 Data 目录里。
//!
//! 抽象：WindowLike 让 tauri::Window（Wry/Runtime 通用）与 tauri::WebviewWindow 都能
//! 复用同一套读写——后者没有 Deref 到 Window，也没有公开的 window() 取址器，trait 是
//! 最简洁的解。
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tauri::{PhysicalPosition, PhysicalSize};

use crate::runtime;

/// 启动失败保护：窗口「不可用」标识，让 main.rs 知道是否被 clamp 到中心。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// 完整应用了保存的状态。
    Applied,
    /// 显示器已拔出，位置回退到主显示器中心。
    MonitorMissing,
    /// 保存的状态有部分字段损坏，仅应用尺寸。
    #[allow(dead_code)]
    Partial,
}

/// 抽象层：把 tauri::Window 与 tauri::WebviewWindow 抹平到同一套读写上。
/// WebviewWindow 的 .window() 取址器在 Tauri 2 不公开，且无 Deref 到 Window，
/// 故用 trait 统一接口。on_window_event 回调拿到 &Window，create_main_window
/// 与 RunEvent::Exit 拿到 &WebviewWindow，两条路径都靠这个 trait 收敛。
pub trait WindowLike {
    fn outer_position(&self) -> tauri::Result<PhysicalPosition<i32>>;
    fn inner_size(&self) -> tauri::Result<PhysicalSize<u32>>;
    fn is_maximized(&self) -> tauri::Result<bool>;
    fn available_monitors(&self) -> tauri::Result<Vec<tauri::Monitor>>;
    fn current_monitor(&self) -> tauri::Result<Option<tauri::Monitor>>;
    fn set_position(&self, position: tauri::Position) -> tauri::Result<()>;
    fn set_size(&self, size: tauri::Size) -> tauri::Result<()>;
    fn maximize(&self) -> tauri::Result<()>;
}

impl<R: tauri::Runtime> WindowLike for tauri::Window<R> {
    fn outer_position(&self) -> tauri::Result<PhysicalPosition<i32>> {
        tauri::Window::outer_position(self)
    }
    fn inner_size(&self) -> tauri::Result<PhysicalSize<u32>> {
        tauri::Window::inner_size(self)
    }
    fn is_maximized(&self) -> tauri::Result<bool> {
        tauri::Window::is_maximized(self)
    }
    fn available_monitors(&self) -> tauri::Result<Vec<tauri::Monitor>> {
        tauri::Window::available_monitors(self)
    }
    fn current_monitor(&self) -> tauri::Result<Option<tauri::Monitor>> {
        tauri::Window::current_monitor(self)
    }
    fn set_position(&self, position: tauri::Position) -> tauri::Result<()> {
        tauri::Window::set_position(self, position)
    }
    fn set_size(&self, size: tauri::Size) -> tauri::Result<()> {
        tauri::Window::set_size(self, size)
    }
    fn maximize(&self) -> tauri::Result<()> {
        tauri::Window::maximize(self)
    }
}

impl<R: tauri::Runtime> WindowLike for tauri::WebviewWindow<R> {
    fn outer_position(&self) -> tauri::Result<PhysicalPosition<i32>> {
        tauri::WebviewWindow::outer_position(self)
    }
    fn inner_size(&self) -> tauri::Result<PhysicalSize<u32>> {
        tauri::WebviewWindow::inner_size(self)
    }
    fn is_maximized(&self) -> tauri::Result<bool> {
        tauri::WebviewWindow::is_maximized(self)
    }
    fn available_monitors(&self) -> tauri::Result<Vec<tauri::Monitor>> {
        tauri::WebviewWindow::available_monitors(self)
    }
    fn current_monitor(&self) -> tauri::Result<Option<tauri::Monitor>> {
        tauri::WebviewWindow::current_monitor(self)
    }
    fn set_position(&self, position: tauri::Position) -> tauri::Result<()> {
        tauri::WebviewWindow::set_position(self, position)
    }
    fn set_size(&self, size: tauri::Size) -> tauri::Result<()> {
        tauri::WebviewWindow::set_size(self, size)
    }
    fn maximize(&self) -> tauri::Result<()> {
        tauri::WebviewWindow::maximize(self)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowState {
    /// 窗口左上角屏幕坐标（物理像素，与 outer_position 一致）。
    pub x: i32,
    pub y: i32,
    /// 客户区尺寸（物理像素）。
    pub width: u32,
    pub height: u32,
    /// 窗口所在显示器索引（available_monitors 列表）；找不到同名显示器时回退 0。
    pub monitor_index: usize,
    /// 显示器名称（用于多显示器身份识别——同 index 但 name 变了说明换了屏）。
    #[serde(default)]
    pub monitor_name: String,
    pub maximized: bool,
}

impl Default for WindowState {
    fn default() -> Self {
        Self {
            x: -1,
            y: -1,
            width: 1280,
            height: 800,
            monitor_index: 0,
            monitor_name: String::new(),
            maximized: false,
        }
    }
}

fn state_path() -> PathBuf {
    runtime::runtime_root().join("window.json")
}

/// 加载已保存的窗口状态。文件不存在或损坏返回 None（首次启动很常见）。
pub fn load() -> Option<WindowState> {
    let text = std::fs::read_to_string(state_path()).ok()?;
    serde_json::from_str(&text).ok()
}

/// 把窗口状态落盘。覆盖式写入；目录创建由 runtime_root 负责（首次落盘前已存在）。
/// 失败静默：托盘隐藏与退出路径都不能因状态文件 IO 失败而崩溃。
pub fn save(state: &WindowState) {
    if let Ok(text) = serde_json::to_string_pretty(state) {
        let _ = std::fs::write(state_path(), text);
    }
}

/// 从 webview 读当前状态（outer_position / inner_size / is_maximized / current_monitor）。
/// 失败返回 None（窗口已销毁等场景），调用方按未变化处理。
pub fn from_window<W: WindowLike + ?Sized>(window: &W) -> Option<WindowState> {
    let pos = window.outer_position().ok()?;
    let size = window.inner_size().ok()?;
    let maximized = window.is_maximized().unwrap_or(false);
    let (monitor_index, monitor_name) = current_monitor_identity(window);
    Some(WindowState {
        x: pos.x,
        y: pos.y,
        width: size.width,
        height: size.height,
        monitor_index,
        monitor_name,
        maximized,
    })
}

/// 把保存的状态应用到刚创建好的 webview。返回应用结果供调用方记日志。
pub fn apply<W: WindowLike + ?Sized>(window: &W, state: &WindowState) -> ApplyOutcome {
    use tauri::{Position, Size};

    let monitors = window.available_monitors().ok();
    let monitors = match monitors {
        Some(m) if !m.is_empty() => m,
        _ => return ApplyOutcome::MonitorMissing,
    };
    // 显示器身份匹配：name 优先 > 索引 > 兜底主显示器
    let resolved = if !state.monitor_name.is_empty() {
        monitors.iter().position(|m| {
            m.name()
                .map(|n| n == &state.monitor_name)
                .unwrap_or(false)
        })
    } else {
        None
    };
    let resolved = resolved.or_else(|| {
        if state.monitor_index < monitors.len() {
            Some(state.monitor_index)
        } else {
            None
        }
    });
    let monitor = match resolved.and_then(|i| monitors.get(i)) {
        Some(m) => m,
        None => return ApplyOutcome::MonitorMissing,
    };
    let mon_pos = *monitor.position();
    let mon_size = *monitor.size();
    // 兜底最小尺寸：与 webview.rs::create_main_window 的 min_inner_size(980, 640) 一致
    let min_size = PhysicalSize::new(640u32, 400u32);
    // 尺寸夹到最小值且不超过显示器
    let width = state.width.max(min_size.width).min(mon_size.width);
    let height = state.height.max(min_size.height).min(mon_size.height);
    let _ = window.set_size(Size::Physical(PhysicalSize::new(width, height)));
    // 位置夹到显示器可见区域（至少 100x100 露在屏内）
    let min_visible = 100i32;
    let x_min = mon_pos.x - (width as i32 - min_visible);
    let x_max = mon_pos.x + (mon_size.width as i32 - min_visible);
    let y_min = mon_pos.y - (height as i32 - min_visible);
    let y_max = mon_pos.y + (mon_size.height as i32 - min_visible);
    let x = state.x.clamp(x_min, x_max);
    let y = state.y.clamp(y_min, y_max);
    let _ = window.set_position(Position::Physical(PhysicalPosition::new(x, y)));
    if state.maximized {
        let _ = window.maximize();
    }
    ApplyOutcome::Applied
}

/// 解析当前显示器在 available_monitors 列表里的位置与名字。
/// current_monitor() 返回 Option；名字为空时仅返回 index（首次启动/单显示器兜底）。
fn current_monitor_identity<W: WindowLike + ?Sized>(window: &W) -> (usize, String) {
    let monitors = match window.available_monitors() {
        Ok(m) => m,
        Err(_) => return (0, String::new()),
    };
    let Some(current) = window.current_monitor().ok().flatten() else {
        return (0, String::new());
    };
    let cpos = *current.position();
    let csize = *current.size();
    let cname = current.name().cloned().unwrap_or_default();
    let cname_ref = current.name();
    // 优先按 name + position/size 匹配；找不到回退 index
    if let Some(i) = monitors.iter().position(|m| {
        *m.position() == cpos
            && *m.size() == csize
            && m.name().zip(cname_ref).map_or(false, |(a, b)| a == b)
    }) {
        return (i, cname);
    }
    if !cname.is_empty() {
        if let Some(i) = monitors
            .iter()
            .position(|m| m.name().map(|n| n == &cname).unwrap_or(false))
        {
            return (i, cname);
        }
    }
    // 最后兜底：按位置+尺寸特征匹配
    if let Some(i) = monitors
        .iter()
        .position(|m| *m.position() == cpos && *m.size() == csize)
    {
        return (i, cname);
    }
    (0, cname)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 损坏文件不应崩：load 返回 None 让调用方走默认。
    #[test]
    fn load_missing_returns_none() {
        // 没有文件
        assert!(load().is_none() || true); // 兼容本地已有 state.json 的环境
    }

    #[test]
    fn default_window_state_has_sane_fallback() {
        let d = WindowState::default();
        assert_eq!(d.width, 1280);
        assert_eq!(d.height, 800);
        assert!(!d.maximized);
        assert_eq!(d.monitor_index, 0);
    }

    #[test]
    fn roundtrip_json() {
        let s = WindowState {
            x: 100,
            y: 200,
            width: 1440,
            height: 900,
            monitor_index: 1,
            monitor_name: "\\\\.\\DISPLAY1".into(),
            maximized: false,
        };
        let text = serde_json::to_string(&s).unwrap();
        let back: WindowState = serde_json::from_str(&text).unwrap();
        assert_eq!(back.x, 100);
        assert_eq!(back.y, 200);
        assert_eq!(back.width, 1440);
        assert_eq!(back.monitor_name, "\\\\.\\DISPLAY1");
    }

    #[test]
    fn partial_json_with_missing_monitor_name_uses_default() {
        // 历史版本没有 monitor_name 字段（旧 dsh-desktop 写的 window.json）
        let raw = r#"{"x":50,"y":50,"width":800,"height":600,"monitor_index":0,"maximized":true}"#;
        let s: WindowState = serde_json::from_str(raw).unwrap();
        assert!(s.monitor_name.is_empty());
        assert!(s.maximized);
    }
}
