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

/// 无边框窗口：decorum 顶栏三个按钮（最小化/最大化/关闭）作为 floating 覆盖
/// 层悬浮在窗口最顶端——不挤压、不偏移 harness 内容。
/// harness 主界面（左侧栏、中间工作区、右侧插件按钮簇）从窗口 y=0 开始铺满，
/// 装饰按钮区域与 dsh 内容完全独立（z-index 上按钮在最上）。
/// 脚本自带 hostname 守卫，仅对 harness origin 生效；tauri.localhost 加载页空操作。
pub const TITLEBAR_INSET_CSS: &str = r##"
(function () {
  if (location.protocol !== 'http:' || location.hostname !== '127.0.0.1' || location.port === '') return;
  var apply = function () {
    // harness 不让位、不偏移：铺满整个窗口，让 decorum 浮动按钮独占顶部 ~40px 区域
    document.body.style.margin = '0';
    // decorum 顶栏元素 pinned 到窗口最顶端右侧
    var s = document.createElement('style');
    s.textContent = '[data-tauri-decorum-tb]{position:fixed;top:0;right:0;z-index:2147483647}';
    (document.head || document.documentElement).appendChild(s);
  };
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', apply);
  } else {
    apply();
  }
})();
"##;

/// decorum 顶栏按钮原本用 Segoe Fluent Icons 的 PUA 字符（\uE921 最小化、
/// \uE922/\uE923 最大化、\uE8BB 关闭），该字体在很多机器上不命中而显示豆腐块。
/// 替换策略：保持 decorum 自己注入 PUA 字符不变（最大化按钮在窗口最大化时
/// decorum 已经会自动切换 \uE922 ↔ \uE923），只通过 CSS 把字符缩小成更接近
/// 原生 Windows 标题栏按钮的视觉密度——避免占满 58x32 按钮中心。
pub const DECORUM_ICON_CSS: &str = r##"
(function () {
  var apply = function () {
    var s = document.createElement('style');
    s.textContent =
      '.decorum-tb-btn{font-size:10px !important;line-height:1;display:flex !important;' +
      'align-items:center !important;justify-content:center !important;' +
      'color:#9aa3af !important;' +
      // decorum 默认 font-family: 'Segoe Fluent Icons', 'Segoe MDL2 Assets'
      // 缺一即豆腐块；改为多级回退链确保任意 Windows 都命中（Segoe MDL2 Assets 至少 Win7+ 必有）
      'font-family:"Segoe Fluent Icons","Segoe MDL2 Assets","SegoeIcons","Segoe Symbol","Segoe UI Symbol",sans-serif !important}' +
      '.decorum-tb-btn:hover{color:#e8ecf1 !important}' +
      '#decorum-tb-close:hover{background-color:rgba(232,17,35,0.85) !important;color:#fff !important}';
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

/// 把主窗口导航到就绪的 Harness 服务。v0.1.2+ 的 launch_url 带一次性 token：
/// webview 跟随 303 → 服务端种下 HttpOnly cookie（默认 30 天，密钥存 DSH_HOME，
/// 跨重启有效）→ 落到干净的主界面；导航守卫按 origin 放行，`?token=` 属于
/// 同 origin 的路径/查询，不受影响。旧版无 token，等价于直接导航 origin。
/// 必须用原生 `navigate` 而非页面内 location.replace：后者是从 tauri.localhost
/// 发起的跨站导航，浏览器对跨站发起的请求不携带 SameSite=Strict 的 cookie，
/// 303 跟随会 401；原生导航等价于地址栏打开（无发起方），Strict 放行。
pub fn navigate_to_harness(app: &tauri::AppHandle, launch_url: &str) {
    crate::status::update(app, "服务已就绪", false, true);
    if let Some(w) = app.get_webview_window("main") {
        match launch_url.parse() {
            Ok(url) => {
                if w.navigate(url).is_err() {
                    let _ = w.eval(&format!("location.replace('{launch_url}')"));
                }
            }
            Err(_) => {
                let _ = w.eval(&format!("location.replace('{launch_url}')"));
            }
        }
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
