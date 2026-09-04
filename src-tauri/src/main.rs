// DSH Desktop：DeepSeek Harness 桌面应用。
// 壳进程监督 `dsh web --no-open --port 0`（随机 loopback 端口，从 stdout 解析实际地址），
// 加固 WebView 只放行当前 Harness origin；关闭=最小化到托盘，退出才整树结束服务。
// 无条件 GUI 子系统（debug 也不弹控制台）；诊断输出全部走日志文件。
#![windows_subsystem = "windows"]

mod diagnostics;
mod events;
mod i18n;
mod install;
mod jumplist;
mod notifications;
mod persona;
mod readiness;
mod remote;
mod remote_account;
mod remote_proxy;
mod runtime;
mod status;
mod supervisor;
mod tray;
mod webview;
mod window_state;

use std::process::Child;
use std::sync::Mutex;

use runtime::Launch;
use status::StartupStatus;
use tauri::Manager;

struct AppState {
    child: Mutex<Option<Child>>,
    /// 远程模式的本地回环反代句柄：远程页面以 http://127.0.0.1:<port>（代理）origin
    /// 加载。切回本地 / 远程重连 / 每轮连接序列开头 / 退出时经 supervisor::stop_proxy 收掉。
    proxy: Mutex<Option<remote_proxy::ProxyHandle>>,
    restarts: Mutex<u32>,
    launch: Mutex<Option<Launch>>,
    /// 当前放行的 Harness origin（如 http://127.0.0.1:4418）；重启换端口时更新。
    origin: Mutex<Option<String>>,
    /// 启动/重启流程闸锁：一条流程（收尾/翻转/探活/启动）从头到尾持锁，其他流程
    /// acquire 时**排队等待**（FlowGate，0.1.28 排队语义）——不再静默放弃并发请求；
    /// 守护线程每 3s tick 用 is_held 探测跳过。不变式见 supervisor.rs 的 FlowGate 注释。
    restarting: supervisor::FlowGate,
    /// 事件流订阅世代号：每次服务启动 +1，旧订阅线程自检出局。
    events_gen: std::sync::atomic::AtomicU64,
    status: Mutex<StartupStatus>,
    /// 当前模式（"local"/"remote"）：manage 时从 mode.txt 读入初值，连接/切回命令
    /// 改写内存值并落盘；status::update 把它投影进 StartupStatus.remote 供加载页区分语境。
    mode: Mutex<&'static str>,
    /// 御符账号登录态（远程实例连接·账号化）：SSO JWT **仅内存**（红队裁决：账号
    /// 长期凭据不落盘），登出/进程退出即清空。None = 未登录。
    sso: Mutex<Option<remote_account::SsoSession>>,
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

/// 便携版分身向导：保存字段 → 写预设/patch/凭证 → 按当前模式自动（重）启动/重连。
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
        // 按模式分派：本地重启服务 / 远程重连远程实例（保存向导不再是模式盲的本地重启，
        // 否则远程模式下保存向导会把本地服务拉起来顶掉远程页面）
        supervisor::restart_by_mode(&app);
    });
    Ok(())
}

/// 连接远程实例：解析输入 → 配对换 token → 凭据/模式落盘 → 后台走远程连接序列。
/// 地址填裸 `host:port`（配对码单独填），或把整条配对链接粘进配对码栏（address 忽略）。
/// async 标记：函数体是同步网络调用（pair 最长 ~16s），必须离开主线程跑（否则窗口、
/// 托盘与 400ms 的 get_status 轮询全部卡死），同时保留 Err 直接回到表单的 UX。
#[tauri::command(async)]
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
    let cfg = remote::RemoteConfig {
        address: addr.clone(),
        origin: format!("http://{addr}"),
        token,
        paired_at: crate::runtime::unix_now() * 1000,
    };
    remote::save(&cfg)?;
    // v0.1.29 D2：归档到多实例列表（去重、最新在前），供托盘「已保存的远程实例」直连
    remote::remember_saved(&cfg);
    remote::save_mode("remote");
    let state: tauri::State<AppState> = app.state();
    *state.mode.lock().unwrap() = "remote";
    // 模式已翻转：托盘菜单换成远程菜单（重连/断开；set_menu 内部自派发主线程，
    // async 命令线程可直接调）
    tray::rebuild(&app);
    // 本地服务若有在跑先整树收掉：远程模式下 child 恒为 None，守护线程自然失活，
    // 不会出现「本地子进程退出 → 守护误拉本地服务把远程页面顶掉」的串台
    supervisor::stop_child(&app);
    *state.origin.lock().unwrap() = None;
    // 远程页面经壳内本地回环反代加载（origin 恒为 127.0.0.1:<随机端口>），首次配对
    // 或换实例都不需要重启 webview——连接序列直接拉起新代理
    status::connect_screen(&app, "配对成功，正在连接远程实例…");
    let handle = app.clone();
    std::thread::spawn(move || {
        if let Err(err) = supervisor::connect_remote_flow(&handle) {
            status::fail(&handle, &err);
        }
    });
    Ok(())
}

/// 加载页「重试」：按当前模式经 restart_by_mode 重新走对应连接序列。统一走它而非
/// start_service/connect_remote_flow：本地分支先杀残留 child 再拉起——探活超时后
/// child 句柄仍是 Some，直接 start_service 会双拉孤儿 dsh；远程分支顺带回加载页，
/// 两个错误态按钮（重试/重启）行为收敛。
#[tauri::command]
fn retry_connect(window: tauri::WebviewWindow, app: tauri::AppHandle) {
    if !caller_is_local(&window) {
        return;
    }
    let handle = app.clone();
    std::thread::spawn(move || supervisor::restart_by_mode(&handle));
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

// ── 远程实例连接·账号化（御符登录 + 实例清单 + exchange；定案见 docs/plans/2026-09-04）──

/// 打开远程实例独立控制窗（托盘「连接远程实例…」的新入口）。
#[tauri::command]
fn remote_open_window(app: tauri::AppHandle) {
    if let Err(e) = remote_account::open_control_window(&app) {
        if let Some(mut log) = crate::runtime::open_log_append() {
            use std::io::Write;
            let _ = writeln!(log, "[远程实例] 打开控制窗失败: {e}");
        }
    }
}

/// 登录态查询（控制窗首帧渲染用）。
#[tauri::command]
fn remote_login_state(state: tauri::State<AppState>) -> serde_json::Value {
    let guard = state.sso.lock().unwrap();
    serde_json::json!({ "loggedIn": guard.is_some() })
}

/// SSO 登录：系统浏览器走浑天 SSO → 回环回调（带 state/nonce）→ 返回 JWT（仅内存）。
/// 登录器最长等 120s（用户在浏览器完成认证的窗口）；blocking 放线程池执行。
#[tauri::command(async)]
async fn remote_login(
    state: tauri::State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    let endpoints = remote_account::AccountEndpoints::from_env();
    let session = tauri::async_runtime::spawn_blocking(move || {
        remote_account::sso_login(&endpoints, std::time::Duration::from_secs(120))
    })
    .await
    .map_err(|e| format!("登录线程失败: {e}"))??;
    *state.sso.lock().unwrap() = Some(session);
    Ok(serde_json::json!({ "loggedIn": true }))
}

/// 登出：清内存 JWT（浏览器侧浑天会话由控制窗文案引导用户自行登出）。
#[tauri::command]
fn remote_logout(state: tauri::State<AppState>) {
    *state.sso.lock().unwrap() = None;
}

/// 双段清单：缓存实例（本机已有凭据，永远可连）+ 云端名下实例（未登录返回空云段）。
#[tauri::command(async)]
async fn remote_instances(
    window: tauri::WebviewWindow,
    state: tauri::State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    if !caller_is_local(&window) {
        return Err("拒绝非本地调用".into());
    }
    let mut cached: Vec<serde_json::Value> = Vec::new();
    let current = remote::load();
    if let Some(cfg) = &current {
        cached.push(serde_json::json!({
            "address": cfg.address, "origin": cfg.origin, "current": true,
        }));
    }
    for s in remote::saved_list() {
        if current.as_ref().map(|c| c.address.clone()) == Some(s.address.clone()) {
            continue;
        }
        cached.push(serde_json::json!({ "address": s.address, "origin": s.origin, "current": false }));
    }
    let jwt = state.sso.lock().unwrap().as_ref().map(|s| s.jwt.clone());
    match jwt {
        None => Ok(serde_json::json!({ "cached": cached, "loggedIn": false, "cloud": [] })),
        Some(jwt) => {
            let endpoints = remote_account::AccountEndpoints::from_env();
            let (list, alive_map) = tauri::async_runtime::spawn_blocking(move || {
                let list = remote_account::instances(&endpoints, &jwt)?;
                // 存活徽标：有 address 的实例逐个探活（401 也算可达）
                let mut alive_map = std::collections::HashMap::new();
                for inst in &list {
                    if let Some(addr) = &inst.address {
                        alive_map.insert(addr.clone(), remote_account::probe_alive(addr));
                    }
                }
                Ok::<_, String>((list, alive_map))
            })
            .await
            .map_err(|e| format!("清单线程失败: {e}"))??;
            let cloud: Vec<serde_json::Value> = list
                .iter()
                .map(|inst| {
                    let mut v = serde_json::to_value(inst).unwrap_or_default();
                    if let Some(addr) = &inst.address {
                        v.as_object_mut().map(|o| {
                            o.insert("alive".into(), serde_json::json!(alive_map.get(addr).copied().unwrap_or(false)))
                        });
                    }
                    v
                })
                .collect();
            Ok(serde_json::json!({ "cached": cached, "loggedIn": true, "cloud": cloud }))
        }
    }
}

/// 点选实例连接：TOFU（未确认地址需 confirm_tofu=true）→ exchange 换实例 token
/// → 落凭据（remote.json + saved）→ 切 remote 模式 → 走既有连接执行流
/// （探活→反代→事件订阅→导航），连接进度经状态屏呈现。
/// 返回 Err("TOFU_REQUIRED") 时 UI 弹首连确认框，用户批准后带 confirm_tofu 重发。
#[tauri::command(async)]
async fn remote_connect_instance(
    window: tauri::WebviewWindow,
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    address: String,
    confirm_tofu: Option<bool>,
) -> Result<String, String> {
    if !caller_is_local(&window) {
        return Err("拒绝非本地调用".into());
    }
    let jwt = state
        .sso
        .lock()
        .unwrap()
        .as_ref()
        .map(|s| s.jwt.clone())
        .ok_or("尚未登录御符账号，请先登录")?;
    if !remote_account::tofu_approved(&address) && confirm_tofu != Some(true) {
        return Err("TOFU_REQUIRED".into());
    }
    remote_account::tofu_approve(&address)?;
    // 本机已有凭据的实例（缓存段）：跳过 exchange 直接复用；云端新实例才走换票。
    let already = remote::load_saved(&address)
        .or_else(|| remote::load().filter(|c| c.address == address));
    if let Some(cfg) = already {
        remote::save(&cfg).map_err(|e| format!("凭据落盘失败: {e}"))?;
        remote::save_mode("remote");
        let handle = app.clone();
        std::thread::spawn(move || {
            crate::webview::navigate_to_loader(&handle);
            if let Some(w) = handle.get_webview_window("main") {
                let _ = w.set_focus();
            }
            if let Err(e) = crate::supervisor::connect_remote_flow(&handle) {
                if let Some(mut log) = crate::runtime::open_log_append() {
                    use std::io::Write;
                    let _ = writeln!(log, "[远程实例] 缓存凭据连接失败: {e}");
                }
            }
        });
        return Ok("复用本机既有凭据，正在连接…".into());
    }
    let endpoints = remote_account::AccountEndpoints::from_env();
    let addr_for_call = address.clone();
    let res = tauri::async_runtime::spawn_blocking(move || {
        remote_account::exchange(&endpoints, &jwt, &addr_for_call)
    })
    .await
    .map_err(|e| format!("exchange 线程失败: {e}"))??;
    let cfg = remote::RemoteConfig {
        address: address.clone(),
        origin: format!("http://{address}"),
        token: res.token,
        paired_at: remote_account::now_ms_pub(),
    };
    remote::save(&cfg).map_err(|e| format!("凭据落盘失败: {e}"))?;
    remote::remember_saved(&cfg);
    remote::save_mode("remote");
    // 连接执行流放后台线程（探活数秒）；导航与状态反馈与托盘「连接」分派同款
    let handle = app.clone();
    std::thread::spawn(move || {
        crate::webview::navigate_to_loader(&handle);
        if let Some(w) = handle.get_webview_window("main") {
            let _ = w.set_focus();
        }
        if let Err(e) = crate::supervisor::connect_remote_flow(&handle) {
            if let Some(mut log) = crate::runtime::open_log_append() {
                use std::io::Write;
                let _ = writeln!(log, "[远程实例] 连接失败: {e}");
            }
        }
    });
    Ok(format!(
        "已换取实例凭据（deviceId={} name={}），正在连接…",
        res.device_id, res.name
    ))
}

/// 清除远程凭据（remote.json；断开连接不清凭据，此命令是显式的「忘掉这台实例」）。
#[tauri::command]
fn remote_clear_credentials(window: tauri::WebviewWindow) -> Result<(), String> {
    if !caller_is_local(&window) {
        return Err("拒绝非本地调用".into());
    }
    let p = remote::config_path();
    if p.exists() {
        std::fs::remove_file(&p).map_err(|e| format!("清除凭据失败: {e}"))?;
    }
    Ok(())
}

/// 手动配对（兼容旧流程的豁免入口）：导航主窗口到既有连接屏。
#[tauri::command]
fn remote_show_legacy_connect(window: tauri::WebviewWindow, app: tauri::AppHandle) {
    if !caller_is_local(&window) {
        return;
    }
    crate::webview::navigate_to_loader(&app);
    crate::status::connect_screen(&app, "手动配对（兼容旧流程）");
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.set_focus();
    }
}

/// 切回本地模式（加载页「取消」按钮 / 远程错误态「回到本地模式」按钮用）；
/// 完整流程见 supervisor::switch_to_local_flow（托盘「断开远程，回到本地」走同一路径）。
#[tauri::command]
fn switch_to_local(window: tauri::WebviewWindow, app: tauri::AppHandle) {
    if !caller_is_local(&window) {
        return;
    }
    let handle = app.clone();
    std::thread::spawn(move || supervisor::switch_to_local_flow(&handle));
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

/* ── 通知中心（D1b）：IPC 只服务 mini 窗口（tauri.localhost 本地页，caller_is_local 天然放行） ── */

/// 通知历史（最新在前，上限 50）。
#[tauri::command]
fn get_notifications(window: tauri::WebviewWindow) -> Vec<notifications::NoticeRecord> {
    if !caller_is_local(&window) {
        return vec![];
    }
    notifications::list()
}

/// 清空通知历史（通知中心「清空」按钮）。
#[tauri::command]
fn clear_notifications(window: tauri::WebviewWindow) {
    if !caller_is_local(&window) {
        return;
    }
    notifications::clear();
}

/// JumpList / 二次启动转发过来的动作分派（D3b）。
/// 返回 true 表示 argv 命中了动作参数（调用方据此跳过默认的「聚焦窗口」逻辑——动作本身
/// 已含正确的窗口行为）。
fn handle_cli_action(app: &tauri::AppHandle, argv: &[String]) -> bool {
    let mut handled = false;
    for a in argv {
        let matched = match a.as_str() {
            "--open-main" => {
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.show();
                    let _ = w.set_focus();
                }
                tray::clear_unread(app);
                true
            }
            "--restart-service" => {
                let handle = app.clone();
                std::thread::spawn(move || supervisor::restart_by_mode(&handle));
                true
            }
            "--connect-remote" => {
                webview::navigate_to_loader(app);
                status::connect_screen(app, "连接远程实例");
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.show();
                    let _ = w.set_focus();
                }
                true
            }
            "--open-log" => {
                webview::open_external(&runtime::log_file().display().to_string());
                true
            }
            "--export-diagnostics" => {
                let handle = app.clone();
                std::thread::spawn(move || match diagnostics::export(&handle) {
                    Ok(path) => {
                        status::set(&handle, &format!("诊断包已导出：{}", path.display()));
                        webview::open_external(&path.display().to_string());
                    }
                    Err(e) => status::fail(&handle, &e),
                });
                true
            }
            "--open-notifications" => {
                tray::clear_unread(app);
                if let Err(e) = notifications::open_window(app) {
                    if let Some(mut log) = runtime::open_log_append() {
                        use std::io::Write;
                        let _ = writeln!(log, "[通知] 打开通知中心失败: {e}");
                    }
                }
                true
            }
            _ => false,
        };
        handled |= matched;
    }
    handled
}

fn main() {
    tauri::Builder::default()
        // 二次启动：聚焦已有窗口（须最先注册）。argv 若带 JumpList 动作参数则执行动作
        // （D3b：任务栏右键「打开主页面/重启服务/连接远程实例/…」都经二次启动转发）。
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            let args: Vec<String> = argv.iter().cloned().collect();
            if !handle_cli_action(app, &args[1.min(args.len())..]) {
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.show();
                    let _ = w.set_focus();
                }
                // 用户已看向主窗口：未读角标清零（D1）
                tray::clear_unread(app);
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
            proxy: Mutex::new(None),
            restarts: Mutex::new(0),
            launch: Mutex::new(None),
            origin: Mutex::new(None),
            restarting: supervisor::FlowGate::new(),
            events_gen: std::sync::atomic::AtomicU64::new(0),
            status: Mutex::new(StartupStatus::default()),
            // manage 在 setup 前：load_mode 是纯文件读（无 Tauri 依赖），此处初始化安全
            mode: Mutex::new(remote::load_mode()),
            sso: Mutex::new(None),
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
            remote_open_window,
            remote_login_state,
            remote_login,
            remote_logout,
            remote_instances,
            remote_connect_instance,
            remote_clear_credentials,
            remote_show_legacy_connect,
            switch_to_local,
            restart_service_cmd,
            get_notifications,
            clear_notifications
        ])
        .setup(|app| {
            let handle = app.handle().clone();
            webview::create_main_window(&handle)?;
            tray::build_tray(&handle)?;
            // D3b：Windows 任务栏右键任务列表（尽力而为，失败仅记日志）
            jumplist::update(&handle);
            // 冷启动动作参数（JumpList 任务在无实例时点击 = 冷启动带参）：延后到线程执行，
            // 让 setup 先完成、加载页先起来
            let cold_args: Vec<String> = std::env::args().skip(1).collect();
            if !cold_args.is_empty() {
                let h = handle.clone();
                std::thread::spawn(move || {
                    handle_cli_action(&h, &cold_args);
                });
            }
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
                //（manage 时已从 mode.txt 读入 AppState.mode，读内存值，单一事实来源）
                let remote_mode = {
                    let state: tauri::State<AppState> = handle.state();
                    let mode = *state.mode.lock().unwrap();
                    mode == "remote"
                };
                if remote_mode {
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
                    // v0.1.28+ 保存窗口状态：hide 之前快照（位置/尺寸/显示器/最大化）
                    if let Some(state) = window_state::from_window(window) {
                        window_state::save(&state);
                    }
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
            // Moved/Resized 也即时刷新状态（不落盘——避免拖拽期频繁 IO；
            // 落盘集中在 CloseRequested 与 Exit，与启动回放的写盘分离）。
        })
        .build(tauri::generate_context!())
        .expect("初始化 DSH Desktop 失败")
        .run(|app, event| {
            if let tauri::RunEvent::Exit = event {
                // v0.1.28+ 退出路径补存一次窗口状态（托盘「退出」不经 CloseRequested，
                // 这里兜底——不然上次启动的位置记忆会丢）。
                if let Some(w) = app.get_webview_window("main") {
                    if let Some(state) = window_state::from_window(&w) {
                        window_state::save(&state);
                    }
                }
                // 真正退出：杀掉整个 dsh 进程树，不留孤儿 node；同时清掉进程登记
                let state: tauri::State<AppState> = app.state();
                let child = state.child.lock().unwrap().take();
                let _ = std::fs::remove_file(runtime::pid_file());
                if let Some(mut child) = child {
                    supervisor::kill_tree(child.id() as u32);
                    let _ = child.wait();
                }
                // 反代收尾：本地回环监听关闭（尽力而为——进程退出本身也会终结它）
                supervisor::stop_proxy(app);
            }
        });
}
