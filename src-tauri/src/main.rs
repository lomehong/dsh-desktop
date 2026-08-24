// DSH Desktop：DeepSeek Harness 桌面应用。
// 壳进程监督 `dsh web --no-open --port 0`（随机 loopback 端口，从 stdout 解析实际地址），
// 加固 WebView 只放行当前 Harness origin；关闭=最小化到托盘，退出才整树结束服务。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod events;
mod readiness;
mod runtime;
mod status;
mod supervisor;
mod tray;
mod webview;

use std::process::Child;
use std::sync::Mutex;

use runtime::Launch;
use status::StartupStatus;

struct AppState {
    child: Mutex<Option<Child>>,
    restarts: Mutex<u32>,
    launch: Mutex<Option<Launch>>,
    /// 当前放行的 Harness origin（如 http://127.0.0.1:4418）；重启换端口时更新。
    origin: Mutex<Option<String>>,
    /// 启动/重启进行中标志：守护线程与手动重启互斥，防止并发双拉。
    restarting: Mutex<bool>,
    status: Mutex<StartupStatus>,
}

/// 自定义命令只服务本地加载页；Harness 远程页面调用一律拒绝（IPC 零授权边界在命令层再拦一道）。
fn caller_is_local(window: &tauri::WebviewWindow) -> bool {
    window
        .url()
        .map(|u| webview::is_local_url(u.as_str()))
        .unwrap_or(false)
}

#[tauri::command]
fn get_status(window: tauri::WebviewWindow, state: tauri::State<AppState>) -> StartupStatus {
    if !caller_is_local(&window) {
        return StartupStatus::default();
    }
    state.status.lock().unwrap().clone()
}

#[tauri::command]
fn open_log(window: tauri::WebviewWindow, app: tauri::AppHandle) {
    if !caller_is_local(&window) {
        return;
    }
    webview::open_external(&app, &runtime::log_file().display().to_string());
}

#[tauri::command]
fn open_runtime_dir(window: tauri::WebviewWindow, app: tauri::AppHandle) {
    if !caller_is_local(&window) {
        return;
    }
    webview::open_external(&app, &runtime::runtime_root().display().to_string());
}

fn main() {
    tauri::Builder::default()
        // 二次启动：聚焦已有窗口（须最先注册）
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.show();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .manage(AppState {
            child: Mutex::new(None),
            restarts: Mutex::new(0),
            launch: Mutex::new(None),
            origin: Mutex::new(None),
            restarting: Mutex::new(false),
            status: Mutex::new(StartupStatus::default()),
        })
        .invoke_handler(tauri::generate_handler![
            get_status,
            open_log,
            open_runtime_dir
        ])
        .setup(|app| {
            let handle = app.handle().clone();
            webview::create_main_window(&handle)?;
            tray::build_tray(&handle)?;
            // 启动序列在后台线程执行，窗口先显示加载页
            std::thread::spawn(move || {
                if let Err(err) = supervisor::start_service(&handle) {
                    status::fail(&handle, &err);
                }
            });
            // 服务守护：异常退出时自动重启
            let watcher = app.handle().clone();
            std::thread::spawn(move || supervisor::watch_child(&watcher));
            Ok(())
        })
        .on_window_event(|window, event| {
            // 关闭按钮 = 最小化到托盘；服务继续运行（IM 渠道/长任务不中断）
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == "main" {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .build(tauri::generate_context!())
        .expect("初始化 DSH Desktop 失败")
        .run(|app, event| {
            if let tauri::RunEvent::Exit = event {
                // 真正退出：杀掉整个 dsh 进程树，不留孤儿 node
                let state: tauri::State<AppState> = app.state();
                if let Some(mut child) = state.child.lock().unwrap().take() {
                    supervisor::kill_tree(child.id() as u32);
                    let _ = child.wait();
                }
            }
        });
}
