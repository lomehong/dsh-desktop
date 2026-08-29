// DSH Desktop：DeepSeek Harness 桌面应用。
// 壳进程监督 `dsh web --no-open --port 0`（随机 loopback 端口，从 stdout 解析实际地址），
// 加固 WebView 只放行当前 Harness origin；关闭=最小化到托盘，退出才整树结束服务。
// 无条件 GUI 子系统（debug 也不弹控制台）；诊断输出全部走日志文件。
#![windows_subsystem = "windows"]

mod events;
mod install;
mod persona;
mod readiness;
mod remote;
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
    /// 当前模式（"local"/"remote"）：manage 时从 mode.txt 读入初值，连接/切回命令
    /// 改写内存值并落盘；status::update 把它投影进 StartupStatus.remote 供加载页区分语境。
    mode: Mutex<&'static str>,
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

/// 便携版分身向导：保存字段 → 写预设/patch/凭证 → 自动（重）启动服务。
#[tauri::command]
fn persona_wizard_save(
    window: tauri::WebviewWindow,
    app: tauri::AppHandle,
    fields: persona::WizardFields,
) -> Result<(), String> {
    if !caller_is_local(&window) {
        return Err("无权限".into());
    }
    persona::save(&fields)?;
    std::thread::spawn(move || {
        status::set(&app, "分身配置完成，正在启动服务…");
        supervisor::restart_service(&app);
    });
    Ok(())
}

/// 连接远程实例：解析输入 → 配对换 token → 凭据/模式落盘 → 后台走远程连接序列。
/// 地址填裸 `host:port`（配对码单独填），或把整条配对链接粘进配对码栏（address 忽略）。
#[tauri::command]
fn connect_remote(
    window: tauri::WebviewWindow,
    app: tauri::AppHandle,
    address: String,
    code: String,
) -> Result<(), String> {
    if !caller_is_local(&window) {
        return Err("无权限".into());
    }
    let (addr, code) = if code.contains("http://") {
        // 粘的是整条配对链接：地址以链接为准，address 参数忽略
        remote::parse_pairing_link(&code)?
    } else {
        let (addr, _) = remote::parse_address(&address)?;
        (addr, code)
    };
    // 配对请求（网络往返最长 ~16s）先在轮询通道可见，避免连接屏停在旧状态
    status::connect_screen(&app, "正在配对远程实例…");
    let token = remote::pair(&addr, &code)?;
    let origin = format!("http://{addr}");
    remote::save(&remote::RemoteConfig {
        address: addr,
        origin,
        token,
        paired_at: crate::runtime::unix_now() * 1000,
    })?;
    remote::save_mode("remote");
    let state: tauri::State<AppState> = app.state();
    *state.mode.lock().unwrap() = "remote";
    // 本地服务若有在跑先整树收掉：远程模式下 child 恒为 None，守护线程自然失活，
    // 不会出现「本地子进程退出 → 守护误拉本地服务把远程页面顶掉」的串台
    if let Some(mut child) = state.child.lock().unwrap().take() {
        supervisor::kill_tree(child.id() as u32);
        let _ = child.wait();
    }
    *state.origin.lock().unwrap() = None;
    let handle = app.clone();
    std::thread::spawn(move || {
        if let Err(err) = supervisor::connect_remote_flow(&handle) {
            status::fail(&handle, &err);
        }
    });
    Ok(())
}

/// 加载页「重试」：按当前模式重新走对应连接序列（后台线程，错误落状态）。
#[tauri::command]
fn retry_connect(window: tauri::WebviewWindow, app: tauri::AppHandle) {
    if !caller_is_local(&window) {
        return;
    }
    let handle = app.clone();
    std::thread::spawn(move || {
        let state: tauri::State<AppState> = handle.state();
        let result = if *state.mode.lock().unwrap() == "remote" {
            supervisor::connect_remote_flow(&handle)
        } else {
            supervisor::start_service(&handle)
        };
        if let Err(err) = result {
            status::fail(&handle, &err);
        }
    });
}

/// 加载页切到「连接远程实例」连接屏（地址/配对码表单）。
#[tauri::command]
fn show_connect(window: tauri::WebviewWindow, app: tauri::AppHandle) {
    if !caller_is_local(&window) {
        return;
    }
    status::connect_screen(&app, "连接远程实例");
}

/// 已配对远程实例的地址（连接屏预填用；未配对返回空串）。
#[tauri::command]
fn get_remote_address(window: tauri::WebviewWindow, _app: tauri::AppHandle) -> String {
    if !caller_is_local(&window) {
        return String::new();
    }
    remote::load().map(|c| c.address).unwrap_or_default()
}

/// 切回本地模式：改模式并落盘 → 撤 origin 回加载页 → 重新走本地启动序列。
#[tauri::command]
fn switch_to_local(window: tauri::WebviewWindow, app: tauri::AppHandle) {
    if !caller_is_local(&window) {
        return;
    }
    let state: tauri::State<AppState> = app.state();
    // 防御性收尾：正常远程模式 child 恒为 None，但升级运行时等路径可能在远程模式下
    // 留下本地 child——不清掉会让 start_service 双拉本地实例
    if let Some(mut child) = state.child.lock().unwrap().take() {
        supervisor::kill_tree(child.id() as u32);
        let _ = child.wait();
    }
    *state.origin.lock().unwrap() = None;
    *state.mode.lock().unwrap() = "local";
    remote::save_mode("local");
    webview::navigate_to_loader(&app);
    let handle = app.clone();
    std::thread::spawn(move || {
        if let Err(err) = supervisor::start_service(&handle) {
            status::fail(&handle, &err);
        }
    });
}

/// 重启服务（加载页按钮用，语义同托盘「重启服务」）：按模式分派。
#[tauri::command]
fn restart_service_cmd(window: tauri::WebviewWindow, app: tauri::AppHandle) {
    if !caller_is_local(&window) {
        return;
    }
    let handle = app.clone();
    std::thread::spawn(move || supervisor::restart_by_mode(&handle));
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
            // manage 在 setup 前：load_mode 是纯文件读（无 Tauri 依赖），此处初始化安全
            mode: Mutex::new(remote::load_mode()),
        })
        .invoke_handler(tauri::generate_handler![
            get_status,
            open_log,
            open_runtime_dir,
            install_runtime,
            persona_wizard_save,
            connect_remote,
            retry_connect,
            show_connect,
            get_remote_address,
            switch_to_local,
            restart_service_cmd
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
                // 便携版首启（或重置后）：先走分身信息向导，保存后再启动服务
                if persona::needed() {
                    status::wizard(&handle, "首次使用：请配置分身信息");
                    return;
                }
                // 模式分叉：上次为远程模式则直接重连远程实例，否则走本地启动序列
                if remote::load_mode() == "remote" {
                    if let Err(err) = supervisor::connect_remote_flow(&handle) {
                        status::fail(&handle, &err);
                    }
                } else if let Err(err) = supervisor::start_service(&handle) {
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
