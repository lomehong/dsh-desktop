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

/// 无边框窗口顶栏（decorum 悬浮按钮区）高度。Harness 页整体下移让出顶栏：
/// 用 body transform 而非 html padding——transform 会把 fixed/absolute 定位的
/// overlay（右侧插件按钮簇、机器人状态栏等）一并下移，padding 移不动它们；
/// decorum 悬浮条自身反向平移回窗口顶部；body 背景镜像到 html 防止顶栏露白。
/// transform 的溢出贡献会让视口出现滚动条，dsh 为全高布局，html overflow 收紧。
/// 仅在 harness origin 注入，**不作用于本地加载页**——loader 是裸 logo/状态，居中
/// 显示即可；强加 transform 会把 body 折成 calc(100% - 40px)，再被 html overflow:hidden
/// 裁掉大半，最终只剩中间压扁的一坨。
pub const TITLEBAR_INSET_CSS: &str = r#"
(function () {
  if (location.protocol !== 'http:' || location.hostname !== '127.0.0.1' || location.port === '') return;
  var INSET = 40;
  var apply = function () {
    var b = document.body;
    if (!b) return;
    var de = document.documentElement;
    de.style.height = '100%';
    de.style.overflow = 'hidden';
    var bg = getComputedStyle(b).backgroundColor;
    if (bg && bg !== 'rgba(0, 0, 0, 0)') de.style.backgroundColor = bg;
    b.style.margin = '0';
    b.style.height = 'calc(100% - ' + INSET + 'px)';
    b.style.transform = 'translateY(' + INSET + 'px)';
    var s = document.createElement('style');
    s.textContent = '[data-tauri-decorum-tb]{transform:translateY(-' + INSET + 'px) !important}';
    (document.head || de).appendChild(s);
  };
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', apply);
  } else {
    apply();
  }
})();
"#;

/// 把 harness 内容下移，让出 decorum 顶栏（harness origin 专用）。
/// 留给外部手工 eval 注入（navigate_to_harness 默认已调；外部可视需要再触发）。
#[allow(dead_code)]
pub fn apply_titlebar_inset(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.eval(TITLEBAR_INSET_CSS);
    }
}

/// 创建主窗口（程序化创建以挂导航守卫；配置文件中 windows 留空）。
/// 无边框：decorum 覆盖式标题栏（Windows 悬浮原生风格按钮；macOS Overlay 红绿灯）。
/// 加载页与 harness 均不加 TITLEBAR_INSET_CSS——由 navigate_to_harness 在切到 harness
/// origin 时再单独 eval 注入，避免加载页被 transform 折坏布局。
pub fn create_main_window(app: &tauri::AppHandle) -> tauri::Result<()> {
    let handle = app.clone();
    let mut builder = tauri::WebviewWindowBuilder::new(
        app,
        "main",
        tauri::WebviewUrl::App("index.html".into()),
    )
    .title("DSH Desktop")
    .inner_size(1280.0, 800.0)
    .min_inner_size(980.0, 640.0)
    .center();
    // Windows/Linux：创建期即去掉原生边框（decorum 的运行时 set_decorations 在程序化
    // 建窗场景下不生效，原生标题栏会与自定义按钮并存）；macOS 走 Overlay 红绿灯路线。
    #[cfg(not(target_os = "macos"))]
    {
        builder = builder.decorations(false);
    }
    #[cfg(target_os = "macos")]
    {
        builder = builder
            .title_bar_style(tauri::TitleBarStyle::Overlay)
            .hidden_title(true);
    }
    let window = builder
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
                open_external(&u);
            }
            false
        })
        .build()?;
    use tauri_plugin_decorum::WebviewWindowExt;
    window.create_overlay_titlebar()?;
    Ok(())
}

/// 把主窗口导航到就绪的 Harness 服务。navigation 完成后注入标题栏下移 CSS（harness origin 专用）。
pub fn navigate_to_harness(app: &tauri::AppHandle, base_url: &str) {
    crate::status::update(app, "服务已就绪", false, true);
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.eval(&format!("location.replace('{base_url}')"));
        // 等 harness 文档 ready 后注入 CSS（initialization_script 对所有页注入会折坏加载页）
        let app_clone = app.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(250));
            if let Some(w2) = app_clone.get_webview_window("main") {
                let _ = w2.eval(TITLEBAR_INSET_CSS);
            }
        });
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
pub fn open_external(target: &str) {
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
