//! 托盘：显示/隐藏、重启服务、升级 DSH、检查应用更新、打开日志与数据目录、开机自启、退出。
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::Manager;

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

pub fn build_tray(app: &tauri::AppHandle) -> tauri::Result<()> {
    use tauri_plugin_autostart::ManagerExt;

    // 便携模式（U盘包）：应用更新与开机自启都面向安装版（更新会装到宿主机、
    // 自启会把U盘路径写进宿主注册表），一并隐藏；升级 DSH 运行时仍然可用（就地升级包内 runtime）
    let portable = crate::runtime::portable_root().is_some();

    let show = MenuItem::with_id(app, "show", "显示 / 隐藏", true, None::<&str>)?;
    let restart = MenuItem::with_id(app, "restart", "重启服务", true, None::<&str>)?;
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
    if portable {
        items.push(&wizard);
    }
    items.push(&upgrade);
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
    let menu = Menu::with_items(app, &items)?;

    #[allow(unused_mut)] // macOS 分支会整体重绑定
    let mut tray = TrayIconBuilder::with_id("dsh-tray")
        .icon(app.default_window_icon().expect("缺少应用图标").clone())
        .tooltip("DSH Desktop — 双击打开；右键菜单退出")
        .menu(&menu)
        .show_menu_on_left_click(false);

    // macOS 菜单栏习惯：单击即弹菜单（Windows 保持左键穿透、双击唤起窗口）
    #[cfg(target_os = "macos")]
    {
        tray = tray.show_menu_on_left_click(true);
    }

    tray.on_menu_event(|app, event| match event.id().as_ref() {
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
                std::thread::spawn(move || crate::supervisor::restart_service(&handle));
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
    Ok(())
}
