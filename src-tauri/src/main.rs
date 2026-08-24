// DSH Desktop：DeepSeek Harness 桌面应用。
// 壳进程监督 `dsh web --no-open --port 0`（随机 loopback 端口，从 stdout 解析实际地址），
// 加固 WebView 只放行当前 Harness origin；关闭=最小化到托盘，退出才整树结束服务。
// 无条件 GUI 子系统（debug 也不弹控制台）；诊断输出全部走日志文件。
#![windows_subsystem = "windows"]

mod events;
mod install;
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
use tauri::Manager;

struct AppState {
    child: Mutex<Option<Child>>,
    restarts: Mutex<u32>,
    launch: Mutex<Option<Launch>>,
    /// 当前放行的 Harness origin（如 http://127.0.0.1:4418）；重启换端口时更新。
    origin: Mutex<Option<String>>,
    /// 启动/重启进行中标志：守护线程与手动重启互斥，防止并发双拉。
    restarting: Mutex<bool>,
    /// 事件流订阅世代号：每次服务启动 +1，旧订阅线程自检出局。
    events_gen: std::sync::atomic::AtomicU64,
    status: Mutex<StartupStatus>,
}

/// 自定义命令只服务本地加载页；Harness 远程页面调用一律拒绝（IPC 零授权边界在命令层再拦一道）。
fn caller_is_local(window: &tauri::WebviewWindow) -> bool {
    window
        .url()
        .map(|u| webview::is_local_url(u.as_str()))
        .unwrap_or(false)
}

/// 解析 `--quit-after-secs N`（CI 冒烟/自动验收用）。
fn quit_after_secs() -> Option<u64> {
    let args: Vec<String> = std::env::args().collect();
    let idx = args.iter().position(|a| a == "--quit-after-secs")?;
    args.get(idx + 1)?.parse().ok()
}

/// 解析 `--upgrade-dsh`（CI/脚本用：检查并升级 DSH 后退出，不启动服务）。
fn upgrade_dsh_flag() -> bool {
    std::env::args().any(|a| a == "--upgrade-dsh")
}

#[tauri::command]
fn get_status(window: tauri::WebviewWindow, state: tauri::State<AppState>) -> StartupStatus {
    if !caller_is_local(&window) {
        return StartupStatus::default();
    }
    state.status.lock().unwrap().clone()
}

#[tauri::command]
fn open_log(window: tauri::WebviewWindow, _app: tauri::AppHandle) {
    if !caller_is_local(&window) {
        return;
    }
    webview::open_external(&runtime::log_file().display().to_string());
}

#[tauri::command]
fn open_runtime_dir(window: tauri::WebviewWindow, _app: tauri::AppHandle) {
    if !caller_is_local(&window) {
        return;
    }
    webview::open_external(&runtime::runtime_root().display().to_string());
}

/// 首启安装引导：下载便携 Node + 安装固定版本 dsh，完成后自动启动服务。
#[tauri::command]
fn install_runtime(window: tauri::WebviewWindow, app: tauri::AppHandle) {
    if !caller_is_local(&window) {
        return;
    }
    std::thread::spawn(move || install::install_and_start(&app));
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
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_decorum::init())
        .manage(AppState {
            child: Mutex::new(None),
            restarts: Mutex::new(0),
            launch: Mutex::new(None),
            origin: Mutex::new(None),
            restarting: Mutex::new(false),
            events_gen: std::sync::atomic::AtomicU64::new(0),
            status: Mutex::new(StartupStatus::default()),
        })
        .invoke_handler(tauri::generate_handler![
            get_status,
            open_log,
            open_runtime_dir,
            install_runtime
        ])
        .setup(|app| {
            let handle = app.handle().clone();
            webview::create_main_window(&handle)?;
            tray::build_tray(&handle)?;
            // CI/脚本用：--upgrade-dsh 检查并升级 DSH 后直接退出（不启动服务）
            if upgrade_dsh_flag() {
                let handle = app.handle().clone();
                std::thread::spawn(move || {
                    let msg = install::upgrade_dsh(&handle).unwrap_or_else(|e| format!("升级失败：{e}"));
                    if let Some(mut log) = runtime::open_log_append() {
                        use std::io::Write;
                        let _ = writeln!(log, "[upgrade-dsh] {msg}");
                    }
                    handle.exit(0);
                });
                return Ok(());
            }
            // 启动序列在后台线程执行，窗口先显示加载页；先清理上次强杀残留的孤儿进程树
            std::thread::spawn(move || {
                supervisor::cleanup_stale_orphan();
                if let Err(err) = supervisor::start_service(&handle) {
                    status::fail(&handle, &err);
                }
            });
            // 服务守护：异常退出时自动重启
            let watcher = app.handle().clone();
            std::thread::spawn(move || supervisor::watch_child(&watcher));
            // CI/自动验收用：--quit-after-secs N 到时走真实退出路径（RunEvent::Exit，含整树清理）
            if let Some(secs) = quit_after_secs() {
                let handle = app.handle().clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_secs(secs));
                    handle.exit(0);
                });
            }
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
                // 真正退出：杀掉整个 dsh 进程树，不留孤儿 node；同时清掉进程登记
                let state: tauri::State<AppState> = app.state();
                let child = state.child.lock().unwrap().take();
                let _ = std::fs::remove_file(runtime::pid_file());
                if let Some(mut child) = child {
                    supervisor::kill_tree(child.id() as u32);
                    let _ = child.wait();
                }
            }
        });
}
