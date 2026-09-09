//! 主窗口与 WebView 加固：导航只放行 本地加载页 与 当前 Harness origin（随机端口），
//! 其余 http(s) 一律交给系统浏览器；Harness 页面不持有任何 Tauri IPC 权限。
use tauri::Manager;

use crate::runtime;

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

/// 无边框窗口：保留式顶栏带（v2，2026-09-09）。decorum 顶栏（全宽拖拽层 +
/// 三个 58×32 窗控钮）独占窗口顶部 40px，harness 页面整体让位到带下。
///
/// 为什么从 overlay 改回让位：0.1.11 曾以「主界面顶到 y=0」为由移除让位，当时
/// dsh 顶部两角没有功能 UI，overlay 相安无事；dsh 0.1.5+ 右侧栏的 dockkit 条带
/// 专职占据窗口右上角（「开始」tab、tab 关闭点、全屏/收起钮都在 y<40），overlay
/// 的窗控钮与全宽拖拽层和它结构性重叠——两套 ✕ 叠在一起，且点击被劫持（点
/// 「开始」tab = 最小化窗口，点侧栏收起 = 关窗口；decorum 容器 z 序最大，页面
/// 收不到事件）。只要页面顶部两角有功能 UI，带就是唯一让「拖拽区」与「页面
/// UI」不竞争的格局。设计文档：docs/plans/2026-09-09-titlebar-right-sidebar-collision-design.md
///
/// 实现要点（v0.1.9 transform 方案的硬化版）：
/// 1. 注入带 id 的 stylesheet 而非 inline style——对页面运行时 DOM 操作免疫；
/// 2. 带高单一来源 `--dsh-titlebar-h`：html 高度收缩、body 平移、decorum 反向
///    平移、模式角标回移共用，改带高只动一处；
/// 3. `html` 高度收缩到 `100% - 带` + `overflow:hidden`——dsh 前端是
///    `html,body,#root{height:100%}` 链（实测无 100vh 根），收缩后正好铺满带下
///    区域，底部状态栏零裁切；transform 的 40px 视觉溢出由 overflow:hidden
///    消除（v0.1.9 同款教训）；
/// 4. `body transform` 而非 padding：fixed 定位 overlay（模式角标、dockkit
///    floatHost `fixed inset:0` 浮窗）随之让位——浮窗标题不会藏进带里点不到；
///    body 成为 fixed 后代的包含块，`bottom:0` 仍贴窗口底；
/// 5. decorum 容器反向平移回窗口顶、高度抬到整带（拖拽区=整带）、按钮
///    flex-start 贴顶（decorum inline 是 end）；带底色继承 app 的
///    `--dsw-alias-bg-base`（定义在 body 上，随 dsh 深浅主题自动切换）；
///    底部 1px hairline（继承 `--dsw-alias-border-l1`）把顶带与页面分开——
///    用户草图指定：没有分隔线时顶带与页面连成一片，读不出标题栏。
///
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
    var s = document.createElement('style');
    s.id = 'dsh-desktop-titlebar-inset';
    s.textContent =
      ':root{--dsh-titlebar-h:40px}' +
      // 100% 链收缩 + overflow:hidden：页面正好铺满带下区域，无底部裁切、无溢出滚动条
      'html{height:calc(100% - var(--dsh-titlebar-h)) !important;overflow:hidden !important}' +
      // body transform：fixed/absolute overlay（模式角标、dockkit 浮窗）一并让位；
      // body 成为 fixed 后代的包含块，bottom:0 仍贴窗口底
      'body{margin:0 !important;height:100% !important;transform:translateY(var(--dsh-titlebar-h))}' +
      // decorum 容器随 body 平移了 +带，须反向平移回窗口顶；高度抬到整带（拖拽区=整带），
      // 按钮 flex-start 贴顶（decorum inline 是 end）；底色继承 app 的 bg-base token；
      // 底部 hairline（2026-09-09 用户草图）：让顶带读作独立标题栏而非页面空白，
      // box-sizing 使 1px 线画在 40px 带内（y=39..40），颜色继承 app 边框 token 随主题
      '[data-tauri-decorum-tb]{position:fixed !important;top:0 !important;left:0 !important;' +
      'width:100% !important;height:var(--dsh-titlebar-h) !important;box-sizing:border-box !important;' +
      'align-items:flex-start !important;' +
      'border-bottom:1px solid var(--dsw-alias-border-l1,rgba(0,0,0,.08)) !important;' +
      'transform:translateY(calc(0px - var(--dsh-titlebar-h)));' +
      'background:var(--dsw-alias-bg-base,#fff);z-index:2147483647 !important}';
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

/// 兜底 polyfill：仅当缺失且 `crypto.getRandomValues` 可用时，用同一密码学随机源
/// 实现同语义 UUIDv4——只补缺，安全上下文（本地回环/https）的原生实现永远不被覆盖。
/// 远程页面现已经本地回环反代加载（origin 天然安全上下文），此脚本平时为空操作；
/// 保留作保险，覆盖极老内核等意外场景。
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

/// 模式角标：窗口顶部居中常驻小徽标（「本地」/「远程 · 地址」），让用户一眼分辨
/// 当前连的是哪个实例。远程模式由代理本地应答 `/__remote/badge`（含展示地址）；
/// 本地模式该路径在本地 dsh 上 404 → 保持「本地」。仅装饰，pointer-events 关闭。
/// 居中定位的原因（2026-09-04）：macOS Overlay 红绿灯浮在窗口左上，角标原
/// `top:0;left:0` 与其重叠拥挤；顶部居中与任何平台窗口装饰都不冲突（Windows
/// decorum 按钮在右上，macOS 红绿灯在左上），因此不分平台统一居中 + 底部圆角。
/// 配色（2026-09-05）：深色主题下旧版 55% 透明深底几乎隐形（真实反馈），改为
/// 高不透明深玻璃底 + 1px 亮边框 + 投影（深浅主题都有轮廓），并加模式色点——
/// 本地绿点（本机实例）、远程蓝点 + 蓝底（网络实例），色彩语义不依赖文字。
/// 住进顶栏带（2026-09-09）：顶栏让位后 body 整体下移，角标挂在 body 下会随之
/// 落到页面首行上；cssText 用 `--dsh-titlebar-h` 反向平移回窗口顶，正好独居带内
/// （与 TITLEBAR_INSET_CSS 共用同一变量，见该常量文档）。
pub const MODE_BADGE_JS: &str = r##"
(function () {
  if (location.protocol !== 'http:' || location.port === '') return;
  var b = null, dot = null, label = null;
  function render(text, remote) {
    if (!b) {
      b = document.createElement('div');
      dot = document.createElement('span');
      dot.style.cssText = 'display:inline-block;width:6px;height:6px;border-radius:50%;' +
        'margin-right:5px;vertical-align:1px;';
      label = document.createElement('span');
      b.appendChild(dot);
      b.appendChild(label);
      // body 让位平移了 +带：角标须反向平移回窗口顶——住进顶栏带，不与页面首行同层
      b.style.cssText = 'position:fixed;top:0;left:50%;' +
        'transform:translate(-50%,calc(0px - var(--dsh-titlebar-h,0px)));' +
        'height:20px;line-height:20px;font-size:11px;padding:0 10px;border-radius:0 0 8px 8px;' +
        'border:1px solid rgba(255,255,255,.22);border-top:none;color:#eef3f8;' +
        'box-shadow:0 2px 8px rgba(0,0,0,.35);z-index:2147483646;pointer-events:none;' +
        'font-family:system-ui,"Microsoft YaHei",sans-serif;user-select:none;';
      document.body.appendChild(b);
    }
    dot.style.background = remote ? '#8ab4ff' : '#3ddc97';
    label.textContent = text;
    b.style.background = remote ? 'rgba(43,84,227,.88)' : 'rgba(13,18,26,.85)';
  }
  var apply = function () {
    render('本地', false);
    fetch('/__remote/badge').then(function (r) {
      return r.ok ? r.json() : null;
    }).then(function (j) {
      if (j && j.mode === 'remote') {
        render('远程 · ' + (j.address || ''), true);
      }
    }).catch(function () {});
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
/// 脚本自带端口守卫（任意带端口的 http 页面生效——能加载进壳的只有导航守卫放行的
/// 已配对 origin，本地回环或远程网关皆适用），对 tauri.localhost
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
        .initialization_script(MODE_BADGE_JS)
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
    // v0.1.28+ 应用保存的窗口位置/尺寸/显示器。失败静默：状态文件损坏或显示器已拔出
    // 等场景走默认（窗口构造器已设的 1280×800 center）；apply 已做显示器/越界夹紧。
    if let Some(state) = crate::window_state::load() {
        let outcome = crate::window_state::apply(&window, &state);
        if outcome != crate::window_state::ApplyOutcome::Applied {
            if let Some(mut log) = runtime::open_log_append() {
                use std::io::Write;
                let _ = writeln!(
                    log,
                    "[窗口] 状态回放降级: {:?}（保存的显示器可能已拔出或位置越界）",
                    outcome
                );
            }
        }
    }
    Ok(())
}

/// 从 launch_url 提取 cookie 的 domain（纯函数便于单测）：剥 scheme/path，端口不属于
/// cookie domain（dsh 的 authority 绑定体现在 cookie 名的 sha256 里，domain 只需命中主机）。
/// 仅认 http（auth cookie 只属于 http 的 harness origin）。
fn cookie_domain_of(url: &str) -> Option<String> {
    let rest = url.strip_prefix("http://")?;
    let authority = rest.split('/').next()?;
    let host = authority.rsplit_once(':').map_or(authority, |(h, _)| h);
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

/// 由壳侧换证得到的 `name=value` 对构造 auth cookie（纯函数便于单测）。
/// domain 用裸主机名（host-only），Path=/；SameSite 对齐服务端签发（Strict）。
fn build_auth_cookie(launch_url: &str, pair: &str) -> Option<tauri::webview::Cookie<'static>> {
    let domain = cookie_domain_of(launch_url)?;
    let (name, value) = pair.split_once('=')?;
    if name.is_empty() || value.is_empty() {
        return None;
    }
    use tauri::webview::cookie::SameSite;
    Some(
        tauri::webview::Cookie::build((name.to_string(), value.to_string()))
            .domain(domain)
            .path("/")
            .same_site(SameSite::Strict)
            .http_only(true)
            .build(),
    )
}

/// 把主窗口导航到就绪的 Harness 服务。v0.1.2+ 的 launch_url 带一次性 token：
/// webview 跟随 303 → 服务端种下 HttpOnly cookie（默认 30 天，密钥存 DSH_HOME，
/// 跨重启有效）→ 落到干净的主界面；导航守卫按 origin 放行，`?token=` 属于
/// 同 origin 的路径/查询，不受影响。旧版无 token，等价于直接导航 origin。
/// 必须用原生 `navigate` 而非页面内 location.replace：后者是从 tauri.localhost
/// 发起的跨站导航，浏览器对跨站发起的请求不携带 SameSite=Strict 的 cookie，
/// 303 跟随会 401；原生导航等价于地址栏打开（无发起方），Strict 放行。
///
/// `auth_cookie`（本地模式）：壳侧换证得到的 `name=value` 对（readiness::exchange_cookie）。
/// 导航前 set_cookie 直接种进 webview 的 cookie store——macOS WKWebView 对「303 重定向
/// 响应携带的 Set-Cookie」落盘不可靠（实机故障：装后首启 401 裸文本页「authentication
/// required; reopen URL…」，Windows 正常），与其赌存储行为，壳自己换好证再导航；
/// Windows 上幂等无害（同值 cookie 覆盖）。远程模式传 None（网关凭证走自己的 pair?token 流程）。
pub fn navigate_to_harness(app: &tauri::AppHandle, launch_url: &str, auth_cookie: Option<&str>) {
    crate::status::update(app, "服务已就绪", false, true);
    if let Some(w) = app.get_webview_window("main") {
        if let Some(pair) = auth_cookie {
            match build_auth_cookie(launch_url, pair) {
                Some(cookie) => {
                    if let Err(e) = w.set_cookie(cookie) {
                        // 注入失败不阻断：webview 自行换证兜底（Windows 路径本来就能自愈）
                        if let Some(mut log) = crate::runtime::open_log_append() {
                            use std::io::Write;
                            let _ = writeln!(log, "[warn] auth cookie 注入失败: {e}");
                        }
                    } else if let Some(mut log) = crate::runtime::open_log_append() {
                        use std::io::Write;
                        let _ = writeln!(log, "[info] auth cookie 已注入（壳侧换证，pair 长度={}）", pair.len());
                    }
                }
                None => {
                    if let Some(mut log) = crate::runtime::open_log_append() {
                        use std::io::Write;
                        let _ = writeln!(
                            log,
                            "[warn] auth cookie 解析失败，跳过注入（url={launch_url} pair 长度={}）",
                            pair.len()
                        );
                    }
                }
            }
        } else if let Some(mut log) = crate::runtime::open_log_append() {
            use std::io::Write;
            let _ = writeln!(log, "[info] 未取到 Set-Cookie，跳过 cookie 注入（旧版 dsh 或换证请求失败）");
        }
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
        // JS 兜底自愈：macOS 实测 WebKit 对「303 重定向 Set-Cookie」与 store 注入的
        // cookie 都不用于后续请求（v0.1.36/37 实机定案），页面会短暂落在 401 裸文本页。
        // 兜底用 document.cookie 直种同名 cookie（jar 里无同名 HttpOnly cookie 时浏览器
        // 允许）并 location.replace('/') 自愈。分 150ms/500ms/1200ms/2500ms 四次递进
        // 检查（各次幂等：命中一次即跳转，后续检查看到 text/html 零打扰），把 401 闪屏
        // 压到 ~150ms 量级；Windows 上 cookie 正常生效，页面是 html，检查全部空转。
        if let Some(pair) = auth_cookie {
            let w2 = w.clone();
            let escaped = serde_json_string(&format!("{pair}; path=/"));
            let url_owned = launch_url.to_string();
            std::thread::spawn(move || {
                for delay_ms in [150u64, 500, 1200, 2500] {
                    std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                    let script = format!(
                        "(function(){{if(document.contentType==='text/plain'){{document.cookie={escaped};location.replace('/');}}}})()"
                    );
                    let _ = w2.eval(&script);
                }
                if let Some(mut log) = crate::runtime::open_log_append() {
                    use std::io::Write;
                    let _ = writeln!(log, "[info] JS cookie 兜底已布防（url={url_owned}）");
                }
            });
        }
    }
    // 主窗口带到前台：未读角标清零（D1）
    crate::tray::clear_unread(app);
}

/// 字符串转 JSON 字符串字面量（eval 内嵌转义）。
fn serde_json_string(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
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

/// 任务栏进度（D3）：由 status::push_frame 每帧调用。
/// percent=None 且非就绪 → 保持当前；ready → 推满后隐藏；error → 错误红态。
/// 跨平台语义（tauri 内建）：Windows=任务栏进度条，macOS=Dock 图标进度，Linux 按
/// Normal 处理（Indeterminate/Paused/Error 在部分平台降级）——无需平台分支。
pub fn taskbar_progress(app: &tauri::AppHandle, percent: Option<u8>, error: bool, ready: bool) {
    // tauri::runtime 模块是私有的；ProgressBarStatus 经 tauri::window 再导出
    use tauri::window::{ProgressBarState, ProgressBarStatus};
    let Some(w) = app.get_webview_window("main") else { return };
    let state = if ready {
        // 就绪：推满一帧绿色，再隐藏（避免停留在任务栏上的残影）
        let _ = w.set_progress_bar(ProgressBarState {
            status: Some(ProgressBarStatus::Normal),
            progress: Some(100),
        });
        ProgressBarState { status: Some(ProgressBarStatus::None), progress: None }
    } else if error {
        ProgressBarState {
            status: Some(ProgressBarStatus::Error),
            progress: Some(percent.unwrap_or(0) as u64),
        }
    } else {
        match percent {
            Some(p) => ProgressBarState {
                status: Some(ProgressBarStatus::Normal),
                progress: Some(p as u64),
            },
            // 未识别阶段：保持现状（None 状态会隐藏进度条，比闪烁更差）
            None => return,
        }
    };
    let _ = w.set_progress_bar(state);
}

/// 用系统默认程序打开 URL / 路径。
/// Windows 走 rundll32 FileProtocolHandler 而非 cmd /C start：URL 里的 & 裸传给
/// cmd 会被当命令分隔符截断（Rust 只对含空白的参数加引号；remote_account.rs
/// open_browser 同款教训——sso-login 的 &sid= 被截导致登录流程失败）。
pub fn open_external(target: &str) {    #[cfg(windows)]
    let mut cmd = {
        let mut c = std::process::Command::new("rundll32");
        c.args(["url.dll,FileProtocolHandler", target]);
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

    #[test]
    fn cookie_domain_of_strips_scheme_path_and_port() {
        assert_eq!(
            cookie_domain_of("http://127.0.0.1:4418/?token=t1").as_deref(),
            Some("127.0.0.1")
        );
        assert_eq!(cookie_domain_of("http://localhost:3080/").as_deref(), Some("localhost"));
        // 端口缺失（异常形态）也不误吞主机名
        assert_eq!(cookie_domain_of("http://127.0.0.1/").as_deref(), Some("127.0.0.1"));
        assert_eq!(cookie_domain_of("tauri://localhost/index.html"), None);
    }

    #[test]
    fn build_auth_cookie_parses_pair_and_sets_strict_host_only() {
        let url = "http://127.0.0.1:4418/?token=tok";
        let c = build_auth_cookie(url, "dsh-auth-abc=v1.x.y").expect("合法 pair 应构造成功");
        assert_eq!(c.name(), "dsh-auth-abc");
        assert_eq!(c.value(), "v1.x.y");
        assert_eq!(c.path().map(|p| p.to_string()).as_deref(), Some("/"));
        assert_eq!(c.domain().map(|d| d.to_string()).as_deref(), Some("127.0.0.1"));
        assert_eq!(c.http_only(), Some(true));
        // 空段/缺等号 → None（不注入，走 webview 自行换证兜底）
        assert!(build_auth_cookie(url, "novalue").is_none());
        assert!(build_auth_cookie(url, "=v1.x").is_none());
    }

    /// 顶栏带 v2 契约：页面让位（100% 链收缩 + body transform）、decorum 容器反向
    /// 平移回窗口顶并保持最高层。任一断言失败都会让 dsh 右侧栏 dockkit 条带
    /// （0.1.5+ 专职占据窗口右上角）重新与窗控钮/拖拽层重叠——点「开始」tab 会
    /// 最小化窗口、点侧栏收起会关窗口。设计文档：
    /// docs/plans/2026-09-09-titlebar-right-sidebar-collision-design.md
    #[test]
    fn titlebar_inset_css_reserves_titlebar_band() {
        let s = TITLEBAR_INSET_CSS;
        // 端口守卫必须保留：加载页（tauri.localhost，非 http 或无端口）是空操作
        assert!(
            s.contains("location.protocol !== 'http:' || location.port === ''"),
            "缺少端口守卫"
        );
        // 带高单一来源 + 页面让位：100% 链收缩（不收缩则底部裁切 40px）+ body 下移
        assert!(s.contains("--dsh-titlebar-h:40px"), "缺少带高变量（单一来源）");
        assert!(
            s.contains("html{height:calc(100% - var(--dsh-titlebar-h))"),
            "html 高度未收缩（dsh 是 height:100% 链，不收缩底部会被裁 40px）"
        );
        assert!(
            s.contains("body{margin:0 !important;height:100% !important;transform:translateY(var(--dsh-titlebar-h))}"),
            "body 未整体下移让位"
        );
        // decorum 容器：反向平移回窗口顶 + 最高层 + 按钮贴顶 + app 底色随主题
        assert!(
            s.contains("transform:translateY(calc(0px - var(--dsh-titlebar-h)))"),
            "decorum 容器未反向平移回窗口顶"
        );
        assert!(s.contains("z-index:2147483647"), "decorum 容器未钉在最高层");
        assert!(s.contains("align-items:flex-start !important"), "窗控钮未贴顶（decorum inline 是 end）");
        assert!(
            s.contains("background:var(--dsw-alias-bg-base,#fff)"),
            "顶栏带未垫 app 底色（深浅主题下会是异色空条）"
        );
        // 底部 hairline 分隔线（用户草图）：无分隔线时顶带与页面连成一片，读不出标题栏
        assert!(
            s.contains("border-bottom:1px solid var(--dsw-alias-border-l1,rgba(0,0,0,.08))"),
            "顶栏带缺少底部 hairline 分隔线"
        );
        assert!(
            s.contains("box-sizing:border-box"),
            "分隔线会画到带外（应含在 40px 带内）"
        );
    }

    /// 模式角标挂在 body 下，body 让位后必须反向平移回窗口顶——否则角标叠在
    /// harness 页首行上；与顶栏带契约共用同一变量。
    #[test]
    fn mode_badge_counter_shifts_with_titlebar_band() {
        let s = MODE_BADGE_JS;
        assert!(
            s.contains("translate(-50%,calc(0px - var(--dsh-titlebar-h,0px)))"),
            "角标未随顶栏带反向平移回窗口顶"
        );
        assert!(
            s.contains("location.protocol !== 'http:' || location.port === ''"),
            "缺少端口守卫"
        );
    }
}
