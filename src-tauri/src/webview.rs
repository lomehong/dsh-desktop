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
/// 脚本自带端口守卫（协议 http 且带端口即生效，本地回环与远程网关一视同仁）：
/// 能加载进壳的页面只有导航守卫放行的已配对 origin，因此无需再校验具体 hostname；
/// tauri.localhost 加载页（非 http 协议）仍是空操作。
pub const TITLEBAR_INSET_CSS: &str = r##"
(function () {
  // 端口守卫：任意带端口的 http 页面即视为守卫放行的 harness origin（本地回环或远程网关）。
  // 页面边界在导航守卫（只放行已配对 origin），此处无需也无法枚举具体 hostname；
  // 加载页 tauri.localhost 非 http 协议，天然空操作。
  if (location.protocol !== 'http:' || location.port === '') return;
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

/// 远程页面（http + 非回环主机）不是安全上下文，Web Crypto 的 `crypto.randomUUID`
/// 在其中不存在，dsh 前端（如 Agent 预设页）调用即崩。兜底 polyfill：仅当缺失且
/// `crypto.getRandomValues` 可用时，用同一密码学随机源实现同语义 UUIDv4——只补缺，
/// 安全上下文（本地回环/https）的原生实现永远不被覆盖。
pub const SECURE_CONTEXT_SHIM_JS: &str = r##"
(function () {
  try {
    if (typeof crypto !== 'undefined'
      && typeof crypto.randomUUID !== 'function'
      && typeof crypto.getRandomValues === 'function') {
      var buf = new Uint8Array(16);
      crypto.randomUUID = function () {
        crypto.getRandomValues(buf);
        buf[6] = (buf[6] & 0x0f) | 0x40; // version 4
        buf[8] = (buf[8] & 0x3f) | 0x80; // variant 10
        var h = Array.prototype.map.call(buf, function (x) {
          return x.toString(16).padStart(2, '0');
        }).join('');
        return h.slice(0, 8) + '-' + h.slice(8, 12) + '-' + h.slice(12, 16)
          + '-' + h.slice(16, 20) + '-' + h.slice(20);
      };
    }
  } catch (e) { /* crypto 不可用的极端环境：维持原状 */ }
})();
"##;

/// wry 在 Windows 上给 WebView2 的默认参数。`additional_browser_args` 是覆盖式传入
/// （不给才用默认），显式传参时必须自带这串默认禁用项，否则默认行为丢失。
const WEBVIEW2_DEFAULT_ARGS: &str = "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection";

/// 构造 WebView2 附加参数：已配对远程实例时，把该 origin 标记为安全上下文——
/// 明文 http 的远程页面上 crypto.subtle/randomUUID 等受限 API 才可用（模型/设置页依赖）。
/// 只影响本壳 webview 的 API 可用性判定，不触及导航守卫与 IPC 边界。
pub fn webview_browser_args() -> String {
    #[cfg(windows)]
    if let Some(cfg) = crate::remote::load() {
        return format!(
            "{WEBVIEW2_DEFAULT_ARGS} --unsafely-treat-insecure-origin-as-secure={}",
            cfg.origin
        );
    }
    WEBVIEW2_DEFAULT_ARGS.to_string()
}

/// 创建主窗口（程序化创建以挂导航守卫；配置文件中 windows 留空）。
/// 无边框：decorum 覆盖式标题栏（Windows 悬浮原生风格按钮；macOS Overlay 红绿灯），
/// Harness 页面经 TITLEBAR_INSET_CSS 下移，不被悬浮条遮挡。
/// 脚本自带端口守卫（任意带端口的 http 页面生效——能加载进壳的只有导航守卫放行的
/// 已配对 origin，本地回环或远程网关皆适用），对 tauri.localhost
/// 加载页是空操作——v0.1.5 曾误判它会折坏加载页改为导航后 250ms eval 注入，
/// 那条路径有竞态（eval 可能落在导航完成前的旧页面上），此处恢复为一贯做法。
pub fn create_main_window(app: &tauri::AppHandle) -> tauri::Result<()> {
    let handle = app.clone();
    let builder = tauri::WebviewWindowBuilder::new(
        app,
        "main",
        tauri::WebviewUrl::App("index.html".into()),
    )
    .title("DSH Desktop")
    .inner_size(1280.0, 800.0)
    .min_inner_size(980.0, 640.0)
    .center();
    // WebView2 附加参数：wry 默认禁用项 + （已配对远程实例时）origin 安全上下文标记。
    // Windows 专用设定，其它平台由 tauri 忽略。
    let mut builder = builder.additional_browser_args(&webview_browser_args());
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
        // WebView2 附加参数：默认禁用项 + （已配对远程实例时）origin 安全上下文标记。
        // Windows 专用 API；cfg 门控保参在非 Windows 上不传。
        .initialization_script(SECURE_CONTEXT_SHIM_JS)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// polyfill 的安全属性必须钉死：只补缺（存在原生实现时绝不覆盖）、
    /// 依赖 crypto.getRandomValues 同源随机、实现 UUIDv4 的版本/变体位。
    #[test]
    fn secure_context_shim_only_fills_missing_api() {
        let s = SECURE_CONTEXT_SHIM_JS;
        assert!(s.contains("typeof crypto.randomUUID !== 'function'"), "缺少「仅缺失时定义」守卫");
        assert!(s.contains("typeof crypto.getRandomValues === 'function'"), "缺少随机源可用性守卫");
        assert!(s.contains("crypto.randomUUID = function"), "未定义补缺赋值");
        assert!(s.contains("| 0x40") && s.contains("| 0x80"), "缺少 UUIDv4 版本/变体位");
        // 整体 try/catch 包裹：极端环境不抛错
        assert!(s.trim_start().starts_with("(function () {"));
        assert!(s.contains("} catch (e)"));
    }
}
