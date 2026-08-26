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

/// 无边框窗口的 decorum 顶栏（系统级悬浮按钮 ~40px 高）必须为 harness 内容让位。
/// 用 padding-top 而非 body transform 让位：transform 会把 fixed/absolute 定位的
/// 右侧插件按钮簇等 overlay 一起下移，padding 只让出顶部 40px 高度、harness 内容
/// 仍然铺满窗口左边和中间、仅顶部被装饰按钮区遮挡，保留按钮独立悬浮在装饰区内。
/// 脚本自带 hostname 守卫，仅对 harness origin（127.0.0.1 随机端口）生效，
/// tauri.localhost 加载页是空操作。
pub const TITLEBAR_INSET_CSS: &str = r#"
(function () {
  if (location.protocol !== 'http:' || location.hostname !== '127.0.0.1' || location.port === '') return;
  var TOP = 40;
  var apply = function () {
    var b = document.body;
    if (!b) return;
    // 用 padding-top 给 decorum 顶栏让位；不偏移、不挤压 fixed overlay，
    // 让 harness 主界面（左侧/中间/右侧栏）从 y=40 开始铺满
    b.style.margin = '0';
    b.style.paddingTop = TOP + 'px';
    b.style.boxSizing = 'border-box';
    // decorum 顶栏元素反向拉到 y=0 让按钮浮在窗口最顶端（padding 占用处）
    var s = document.createElement('style');
    s.textContent = '[data-tauri-decorum-tb]{transform:translateY(-' + TOP + 'px) !important;top:0;position:fixed;right:0}';
    (document.head || document.documentElement).appendChild(s);
  };
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', apply);
  } else {
    apply();
  }
})();
"#;

/// decorum 顶栏按钮用 Segoe Fluent Icons 的 PUA 字符渲染，字体缺失/未命中的机器上
/// 显示为豆腐块（实测注册表有字体项但 WebView2 不命中）。以 font-size:0 隐藏字符，
/// 用 SVG data URI 画标准的最小化/最大化/关闭图标——不依赖任何系统字体。
/// 与 TITLEBAR_INSET_CSS 并列注册为 initialization_script，加载页与 harness 页都生效。
pub const DECORUM_ICON_CSS: &str = r##"
(function () {
  var apply = function () {
    var svg = function (body) {
      return "url(\"data:image/svg+xml;charset=utf-8," + encodeURIComponent(
        '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 20 20" stroke="#9aa3af" stroke-width="1.2" fill="none" stroke-linecap="round">' + body + '</svg>'
      ) + "\")";
    };
    var s = document.createElement('style');
    s.textContent =
      '.decorum-tb-btn{font-size:0 !important;position:relative}' +
      '.decorum-tb-btn::before{content:"";position:absolute;inset:0;background-repeat:no-repeat;background-position:center}' +
      '#decorum-tb-minimize::before{background-image:' + svg('<path d="M4 10h12"/>') + '}' +
      '#decorum-tb-maximize::before{background-image:' + svg('<rect x="4.5" y="4.5" width="11" height="11"/>') + '}' +
      '#decorum-tb-close::before{background-image:' + svg('<path d="M5 5l10 10M15 5L5 15"/>') + '}';
    (document.head || document.documentElement).appendChild(s);
  };
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', apply);
  } else {
    apply();
  }
})();
"##;

/// 创建主窗口（程序化创建以挂导航守卫；配置文件中 windows 留空）。
/// 无边框：decorum 覆盖式标题栏（Windows 悬浮原生风格按钮；macOS Overlay 红绿灯），
/// Harness 页面经 TITLEBAR_INSET_CSS 下移，不被悬浮条遮挡。
/// 脚本自带 hostname 守卫（仅 127.0.0.1 的 harness origin 生效），对 tauri.localhost
/// 加载页是空操作——v0.1.5 曾误判它会折坏加载页改为导航后 250ms eval 注入，
/// 那条路径有竞态（eval 可能落在导航完成前的旧页面上），此处恢复为一贯做法。
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
        .initialization_script(TITLEBAR_INSET_CSS)
        .initialization_script(DECORUM_ICON_CSS)
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
