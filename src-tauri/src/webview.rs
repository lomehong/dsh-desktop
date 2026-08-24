//! 主窗口与 WebView 加固：导航只放行 本地加载页 与 当前 Harness origin（随机端口），
//! 其余 http(s) 一律交给系统浏览器；Harness 页面不持有任何 Tauri IPC 权限。
use tauri::Manager;

/// 本地加载页 / Tauri 内部地址（Windows 默认 app origin 为 http://tauri.localhost）。
pub fn is_local_url(u: &str) -> bool {
    u.starts_with("tauri://localhost")
        || u.starts_with("http://tauri.localhost")
        || u.starts_with("https://tauri.localhost")
        || u.starts_with("http://ipc.localhost")
        || u.starts_with("https://ipc.localhost")
        || u == "about:blank"
}

/// 前缀必须是完整 origin：后面只能跟结尾、路径、查询或锚点，
/// 防止 `http://127.0.0.1:44182.evil.com` 这类前缀伪装。
fn same_origin(u: &str, origin: &str) -> bool {
    let Some(rest) = u.strip_prefix(origin) else {
        return false;
    };
    rest.is_empty() || rest.starts_with('/') || rest.starts_with('?') || rest.starts_with('#')
}

/// 创建主窗口（程序化创建以挂导航守卫；配置文件中 windows 留空）。
pub fn create_main_window(app: &tauri::AppHandle) -> tauri::Result<()> {
    let handle = app.clone();
    tauri::WebviewWindowBuilder::new(
        app,
        "main",
        tauri::WebviewUrl::App("index.html".into()),
    )
    .title("DSH Desktop")
    .inner_size(1280.0, 800.0)
    .min_inner_size(980.0, 640.0)
    .center()
    .on_navigation(move |url| {
        let u = url.as_str().to_string();
        if is_local_url(&u) {
            return true;
        }
        let allowed = handle
            .state::<crate::AppState>()
            .origin
            .lock()
            .unwrap()
            .clone();
        if let Some(origin) = allowed {
            if same_origin(&u, &origin) {
                return true;
            }
        }
        // 外部链接（含旧端口的失效地址）交给系统浏览器，绝不留在壳内
        if url.scheme() == "http" || url.scheme() == "https" {
            open_external(&handle, &u);
        }
        false
    })
    .build()?;
    Ok(())
}

/// 把主窗口导航到就绪的 Harness 服务。
pub fn navigate_to_harness(app: &tauri::AppHandle, base_url: &str) {
    crate::status::update(app, "服务已就绪", false, true);
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.eval(&format!("location.replace('{base_url}')"));
        let _ = w.show();
        let _ = w.set_focus();
    }
}

/// 把主窗口导回本地加载页（重启期间显示进度）。
pub fn navigate_to_loader(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let loader = if cfg!(windows) {
            "http://tauri.localhost/index.html"
        } else {
            "tauri://localhost/index.html"
        };
        let _ = w.eval(&format!("location.replace('{loader}')"));
        let _ = w.show();
    }
}

/// 用系统默认程序打开 URL / 路径。
pub fn open_external(app: &tauri::AppHandle, target: &str) {
    #[cfg(windows)]
    let mut cmd = {
        let mut c = std::process::Command::new("cmd.exe");
        c.args(["/C", "start", "", target]);
        c
    };
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = std::process::Command::new("open");
        c.arg(target);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(target);
        c
    };
    let _ = crate::runtime::no_window(&mut cmd).spawn();
}
