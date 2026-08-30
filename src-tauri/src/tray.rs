//! 托盘：按当前模式（本地/远程）动态构建菜单 + 显示/隐藏、重启、升级 DSH、检查应用更新、
//! 打开日志与数据目录、开机自启、退出。
use std::sync::OnceLock;

use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{TrayIcon, TrayIconBuilder};
use tauri::Manager;

/// 已构建的托盘图标句柄：模式切换时经 set_menu 整体换菜单（图标不重建、不闪烁）。
/// 菜单事件处理器只在 build_tray 注册一次——tauri 把菜单事件挂在全局监听表
/// （TrayIcon::on_menu_event 实现即向 global_event_listeners 追加），
/// remove+rebuild 会让处理器随重建成倍叠加触发，因此换菜单而非换图标。
static TRAY: OnceLock<TrayIcon<tauri::Wry>> = OnceLock::new();

const TOOLTIP_LOCAL: &str = "DSH Desktop — 双击打开；右键菜单退出";
/// 远程模式提示带实例地址，一眼分清连的是谁。

/// 检查应用自更新（tauri-plugin-updater，签名公钥内置于 tauri.conf.json）。
fn check_app_update(app: &tauri::AppHandle) {
    use tauri_plugin_updater::UpdaterExt;
    crate::status::set(app, "正在检查应用更新…");
    tauri::async_runtime::block_on(async move {
        let updater = match app.updater_builder().build() {
            Ok(u) => u,
            Err(e) => {
                crate::status::set(&app, &format!("更新器初始化失败：{e}"));
                return;
            }
        };
        match updater.check().await {
            Ok(Some(update)) => {
                crate::status::set(&app, &format!("发现新版本 {}，正在下载…", update.version));
                let on_done = || app.restart();
                if let Err(e) = update
                    .download_and_install(|_, _| {}, on_done)
                    .await
                {
                    crate::status::set(&app, &format!("更新安装失败：{e}"));
                }
            }
            Ok(None) => crate::status::set(&app, "已是最新版本"),
            Err(e) => crate::status::set(&app, &format!("检查更新失败：{e}")),
        }
    });
}

/// 当前模式（AppState.mode 现读）；state 未就绪时按本地处理（与 status.rs 的投影一致）。
fn is_remote(app: &tauri::AppHandle) -> bool {
    app.try_state::<crate::AppState>()
        .is_some_and(|s| *s.mode.lock().unwrap() == "remote")
}

/// 按模式构建托盘菜单：
/// - 本地：显示/隐藏、重启服务、连接远程实例…、[便携]分身向导、升级 DSH、[非便携]检查应用更新、
///   日志、数据目录、[非便携]开机自启、退出
/// - 远程：显示/隐藏、重连远程实例、断开远程回到本地、[非便携]检查应用更新、
///   日志、数据目录、[非便携]开机自启、退出
///
/// 远程菜单只隐藏「会拉起本地服务」的项：升级 DSH 装完要重拉本地服务、分身向导保存后
/// 重启本地流——两者都应先「断开远程」再操作（升级路径的远程防御分支见
/// install.rs::upgrade_runtime）。检查应用更新与开机自启是模式无关项：应用更新装完
/// app.restart() 会按 mode.txt 重新进入上次模式（远程停远程），自启只是 OS 开关；
/// 便携隐藏规则叠加在模式条件之上，远程模式同样适用。
fn build_menu(app: &tauri::AppHandle, remote: bool) -> tauri::Result<Menu<tauri::Wry>> {
    use tauri_plugin_autostart::ManagerExt;

    // 便携模式（U盘包）：应用更新与开机自启都面向安装版（更新会装到宿主机、
    // 自启会把U盘路径写进宿主注册表），一并隐藏；升级 DSH 运行时仍然可用（就地升级包内 runtime）
    let portable = crate::runtime::portable_root().is_some();

    let show = MenuItem::with_id(app, "show", "显示 / 隐藏", true, None::<&str>)?;
    let restart = MenuItem::with_id(
        app,
        "restart",
        if remote { "重连远程实例" } else { "重启服务" },
        true,
        None::<&str>,
    )?;
    let connect = MenuItem::with_id(app, "connect", "连接远程实例…", true, None::<&str>)?;
    let tolocal = MenuItem::with_id(app, "tolocal", "断开远程，回到本地", true, None::<&str>)?;
    let upgrade = MenuItem::with_id(app, "upgrade", "升级 DSH 运行时（npm 最新版）", true, None::<&str>)?;
    let wizard = MenuItem::with_id(app, "wizard", "重新运行分身向导", true, None::<&str>)?;
    let check_update = MenuItem::with_id(app, "check-update", "检查 DSH Desktop 应用更新", true, None::<&str>)?;
    let open_log = MenuItem::with_id(app, "openlog", "打开日志", true, None::<&str>)?;
    let open_dir = MenuItem::with_id(app, "opendir", if portable { "打开U盘数据目录" } else { "打开数据目录" }, true, None::<&str>)?;
    let autostart = CheckMenuItem::with_id(
        app,
        "autostart",
        "开机自启",
        app.autolaunch().is_enabled().unwrap_or(false),
        true,
        None::<&str>,
    )?;
    let sep = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;

    let mut items: Vec<&dyn tauri::menu::IsMenuItem<_>> = vec![&show, &restart];
    if remote {
        items.push(&tolocal);
    } else {
        items.push(&connect);
        if portable {
            items.push(&wizard);
        }
        items.push(&upgrade);
    }
    // 检查应用更新/开机自启与模式无关（更新装完 app.restart() 重读 mode.txt 回到原模式；
    // 自启是纯 OS 开关），本地/远程菜单都保留，只按便携规则隐藏
    if !portable {
        items.push(&check_update);
    }
    items.push(&open_log);
    items.push(&open_dir);
    if !portable {
        items.push(&autostart);
    }
    items.push(&sep);
    items.push(&quit);
    Menu::with_items(app, &items)
}

/// 托盘提示：远程模式带实例地址（读已存凭据；缺失则退回通用文案）。
fn tooltip_text(remote: bool) -> String {
    if !remote {
        return TOOLTIP_LOCAL.to_string();
    }
    let addr = crate::remote::load_display().map(|(a, _)| a).unwrap_or_default();
    if addr.is_empty() {
        "DSH Desktop（远程模式）— 双击打开；右键菜单退出".to_string()
    } else {
        format!("DSH Desktop — 远程 {addr}；双击打开；右键菜单退出")
    }
}

pub fn build_tray(app: &tauri::AppHandle) -> tauri::Result<()> {
    let remote = is_remote(app);
    let menu = build_menu(app, remote)?;

    #[allow(unused_mut)] // macOS 分支会整体重绑定
    let mut builder = TrayIconBuilder::with_id("dsh-tray")
        .icon(app.default_window_icon().expect("缺少应用图标").clone())
        .tooltip(tooltip_text(remote))
        .menu(&menu)
        .show_menu_on_left_click(false);

    // macOS 菜单栏习惯：单击即弹菜单（Windows 保持左键穿透、双击唤起窗口）
    #[cfg(target_os = "macos")]
    {
        builder = builder.show_menu_on_left_click(true);
    }

    // 菜单事件处理器全程只注册这一次；模式切换只换菜单（见 rebuild），id 分发不受影响
    let icon = builder
        .on_menu_event(|app, event| match event.id().as_ref() {
            "show" => {
                if let Some(w) = app.get_webview_window("main") {
                    if w.is_visible().unwrap_or(false) {
                        let _ = w.hide();
                    } else {
                        let _ = w.show();
                        let _ = w.set_focus();
                    }
                }
            }
            "restart" => {
                let handle = app.clone();
                // 按模式分派（本地 restart_service / 远程 connect_remote_flow）；文案随菜单变化，id 复用
                std::thread::spawn(move || crate::supervisor::restart_by_mode(&handle));
            }
            "connect" => {
                // 连接远程实例：先把壳带回加载页（若正停在 harness 页，eval 导航回加载页），
                // 再落一帧连接屏状态（新加载页轮询 get_status 首帧即渲染表单），最后窗口置前
                crate::webview::navigate_to_loader(app);
                crate::status::connect_screen(app, "连接远程实例");
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.set_focus();
                }
            }
            "tolocal" => {
                let handle = app.clone();
                // 与加载页 switch_to_local 命令同一流程（模式翻转 + 托盘重建 + 本地启动序列）
                std::thread::spawn(move || crate::supervisor::switch_to_local_flow(&handle));
            }
            "upgrade" => {
                let handle = app.clone();
                std::thread::spawn(move || crate::install::upgrade_runtime(&handle));
            }
            "wizard" => {
                crate::persona::reopen(app);
            }
            "check-update" => {
                let handle = app.clone();
                std::thread::spawn(move || check_app_update(&handle));
            }
            "openlog" => {
                crate::webview::open_external(&crate::runtime::log_file().display().to_string());
            }
            "opendir" => {
                crate::webview::open_external(
                    &crate::runtime::runtime_root().display().to_string(),
                );
            }
            "autostart" => {
                use tauri_plugin_autostart::ManagerExt;
                let autolaunch = app.autolaunch();
                let enabled = autolaunch.is_enabled().unwrap_or(false);
                let result = if enabled { autolaunch.disable() } else { autolaunch.enable() };
                if let Err(e) = result {
                    eprintln!("切换开机自启失败: {e}");
                }
            }
            "quit" => {
                app.exit(0);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            // 左键双击托盘：显示窗口
            if let tauri::tray::TrayIconEvent::DoubleClick { .. } = event {
                let app = tray.app_handle().clone();
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.show();
                    let _ = w.set_focus();
                }
            }
        })
        .build(app)?;
    let _ = TRAY.set(icon);
    Ok(())
}

/// 模式翻转后按当前模式重建托盘菜单（connect_remote 成功落凭据、switch_to_local 两处调用）。
/// MenuItem/Menu 的创建与 set_menu/set_tooltip 在 tauri 内部均经 run_main_thread 派发
/// （已在主线程则就地执行、否则排队并阻塞等待结果），因此 async 命令线程可直接调用，
/// 无需调用侧再包 run_on_main_thread。
pub fn rebuild(app: &tauri::AppHandle) {
    let Some(tray) = TRAY.get() else {
        return;
    };
    let remote = is_remote(app);
    let result = build_menu(app, remote).and_then(|menu| {
        tray.set_menu(Some(menu))?;
        tray.set_tooltip(Some(tooltip_text(remote)))?;
        Ok(())
    });
    if let Err(e) = result {
        if let Some(mut log) = crate::runtime::open_log_append() {
            use std::io::Write;
            let _ = writeln!(log, "[托盘] 菜单按模式重建失败: {e}");
        }
    }
}
