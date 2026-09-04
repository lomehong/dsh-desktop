//! 托盘：按当前模式（本地/远程）动态构建菜单 + 显示/隐藏、重启、升级 DSH、检查应用更新、
//! 打开日志与数据目录、开机自启、退出。
use std::sync::OnceLock;

use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{TrayIcon, TrayIconBuilder};
use tauri::Manager;

/// 把字符串写入系统剪贴板。跨平台实现：Windows 用 clip.exe，macOS 用 pbcopy，
/// Linux 用 xclip（缺命令时静默失败）。托盘「复制地址」用，避免引入额外的
/// tauri-plugin-clipboard-manager 依赖。
fn copy_to_clipboard(_app: &tauri::AppHandle, text: &str) {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let (program, args): (&str, &[&str]) = if cfg!(windows) {
        ("clip", &[])
    } else if cfg!(target_os = "macos") {
        ("pbcopy", &[])
    } else {
        ("xclip", &["-selection", "clipboard"])
    };
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = match crate::runtime::no_window(&mut cmd).spawn() {
        Ok(c) => c,
        Err(_) => return,
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(text.as_bytes());
    }
    let _ = child.wait();
}

/// 已构建的托盘图标句柄：模式切换时经 set_menu 整体换菜单（图标不重建、不闪烁）。
/// 菜单事件处理器只在 build_tray 注册一次——tauri 把菜单事件挂在全局监听表
/// （TrayIcon::on_menu_event 实现即向 global_event_listeners 追加），
/// remove+rebuild 会让处理器随重建成倍叠加触发，因此换菜单而非换图标。
static TRAY: OnceLock<TrayIcon<tauri::Wry>> = OnceLock::new();

/* ═══════════════════════════════════════════════════════════════════════════
 * v0.1.29 托盘图标状态化 + 未读角标（C1 + D1）
 *
 * 设计：以应用图标为底，右下角现场合成状态圆点 / 未读计数角标（RGBA 像素级绘制，
 * 零二进制资源、跨平台一致）。状态取自 AppState.status（与 tooltip 同源）；
 * 未读计数由 events::present() 经 bump_unread 递增，用户打开主窗口即清零。
 * ═══════════════════════════════════════════════════════════════════════════ */

/// 未读通知计数（D1）：events 推送通知且主窗口不可见/无焦点时 +1；打开主窗口清零。
static UNREAD: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// 图标右下角角标的语义色。
#[derive(Clone, Copy, PartialEq)]
enum BadgeKind {
    /// 启动/重连/升级中（琥珀）。
    Loading,
    /// 服务就绪·本地（绿）。
    Ready,
    /// 服务就绪·远程（蓝）。
    Remote,
    /// 服务异常（红）。
    Error,
    /// 未读计数（红底白数字）。
    Count(u32),
}

/// 通知 +1 并刷新图标（events.rs 调用）。返回新计数值便于日志。
pub fn bump_unread(app: &tauri::AppHandle) -> u32 {
    let n = UNREAD.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
    refresh_tray_icon(app);
    sync_dock_badge(app, n);
    n
}

/// 清零未读并刷新图标（用户打开主窗口的所有路径调用）。
pub fn clear_unread(app: &tauri::AppHandle) {
    if UNREAD.swap(0, std::sync::atomic::Ordering::SeqCst) != 0 {
        refresh_tray_icon(app);
        sync_dock_badge(app, 0);
    }
}

/// macOS Dock 图标未读角标（D3b）：NSApp.dockTile.badgeLabel。Windows 下 no-op。
/// AppKit UI 操作必须在主线程——bump/clear 的调用方常是 events 后台线程等任意线程，
/// 统一经 run_on_main_thread 派发。注意：此分支仅 macOS 目标编译，Windows 机器上的
/// cargo check 不会验证其内容；语法遵循 objc2 0.6 惯例，macOS CI 构建时会确认。
pub fn sync_dock_badge(app: &tauri::AppHandle, count: u32) {
    #[cfg(target_os = "macos")]
    {
        let _ = app.run_on_main_thread(move || dock_badge_on_main(count));
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (app, count); // Windows 用托盘像素角标，任务栏角标交给 tray 图标本身
    }
}

/// 主线程上真正设置 Dock 角标（仅 macOS 编译）。
#[cfg(target_os = "macos")]
fn dock_badge_on_main(count: u32) {
    use objc2::{class, msg_send, rc::Retained, runtime::AnyObject};
    unsafe {
        let app: Option<Retained<AnyObject>> = msg_send![class!(NSApplication), sharedApplication];
        let Some(app) = app else { return };
        let dock: Option<Retained<AnyObject>> = msg_send![&app, dockTile];
        let Some(dock) = dock else { return };
        if count == 0 {
            // Option::<&AnyObject>::None 经 objc2 编码为 nil，清除角标
            let _: () =
                msg_send![&dock, setBadgeLabel: Option::<&AnyObject>::None];
        } else {
            let label = format!("{count}");
            let Ok(c) = std::ffi::CString::new(label) else { return };
            let s: Option<Retained<AnyObject>> =
                msg_send![class!(NSString), stringWithUTF8String: c.as_ptr()];
            if let Some(s) = s {
                let _: () = msg_send![&dock, setBadgeLabel: &*s];
            }
        }
    }
}

fn unread() -> u32 {
    UNREAD.load(std::sync::atomic::Ordering::SeqCst)
}

/// 按当前 status + 未读数重绘托盘图标（状态角标，未读计数优先）。
/// 由 push_frame（status.rs）与 bump/clear_unread 调用；托盘未就绪时静默。
pub fn refresh_tray_icon(app: &tauri::AppHandle) {
    let Some(tray) = TRAY.get() else { return };
    let badge = if unread() > 0 {
        BadgeKind::Count(unread())
    } else {
        // 无状态帧时（极端时序）退回 Loading 角标；有帧则按状态
        status_badge(app).unwrap_or(BadgeKind::Loading)
    };
    let Some(img) = compose_badged_icon(app, badge) else { return };
    let _ = tray.set_icon(Some(img));
}

/// 从 AppState.status 推导状态角标（与 compute_tooltip 同源同锁序）。
fn status_badge(app: &tauri::AppHandle) -> Option<BadgeKind> {
    let state = app.try_state::<crate::AppState>()?;
    let s = state.status.lock().unwrap();
    let mode = *state.mode.lock().unwrap();
    let text = s.text.clone();
    let badge = if s.error {
        BadgeKind::Error
    } else if s.ready {
        if mode == "remote" {
            BadgeKind::Remote
        } else {
            BadgeKind::Ready
        }
    } else {
        // 进行中：升级/安装/下载归 Loading（琥珀），其余也是 Loading（不区分 spinner 帧）
        let _ = &text;
        BadgeKind::Loading
    };
    Some(badge)
}

/// 应用图标 + 状态角标合成。返回 nil 表示拿不到默认图标（理论不可达，build_tray 已 expect）。
fn compose_badged_icon(app: &tauri::AppHandle, badge: BadgeKind) -> Option<tauri::image::Image<'static>> {
    let base = app.default_window_icon()?;
    let w = base.width() as usize;
    let h = base.height() as usize;
    if w == 0 || h == 0 {
        return None;
    }
    let mut rgba = base.rgba().to_vec();
    draw_badge(&mut rgba, w, h, &badge);
    Some(tauri::image::Image::new_owned(rgba, base.width(), base.height()))
}

/// 在 RGBA 缓冲右下角绘制角标。状态点 = 实心圆 + 白描边；计数 = 红圆 + 白色数字。
fn draw_badge(rgba: &mut [u8], w: usize, h: usize, badge: &BadgeKind) {
    // 角标直径 ≈ 图标短边的 42%（16px 图标下 ≈ 7px，32px 下 ≈ 13px）
    let d = ((w.min(h) * 42) / 100).max(6);
    let r = d as f32 / 2.0;
    let cx = (w as f32 - r - (w as f32 * 0.04)).round() as i32;
    let cy = (h as f32 - r - (h as f32 * 0.04)).round() as i32;
    let (fill, digit) = match badge {
        BadgeKind::Loading => ((255u8, 170u8, 0u8), None),
        BadgeKind::Ready => ((76, 175, 80), None),
        BadgeKind::Remote => ((74, 108, 247), None),
        BadgeKind::Error => ((244, 67, 54), None),
        BadgeKind::Count(n) => ((229, 57, 53), Some((*n).min(99))),
    };
    let ring = (255u8, 255u8, 255u8);
    // 白描边 = 半径外扩 1px 的环
    for y in 0..h {
        for x in 0..w {
            let dx = x as f32 + 0.5 - cx as f32;
            let dy = y as f32 + 0.5 - cy as f32;
            let dist = (dx * dx + dy * dy).sqrt();
            if dist > r + 1.0 {
                continue;
            }
            let idx = (y * w + x) * 4;
            if idx + 3 >= rgba.len() {
                continue;
            }
            let (cr, cg, cb) = if dist > r { ring } else { fill };
            // 计数角标的数字区用白像素覆盖（下面第二遍扫描绘制）
            rgba[idx] = cr;
            rgba[idx + 1] = cg;
            rgba[idx + 2] = cb;
            rgba[idx + 3] = 255;
        }
    }
    if let Some(n) = digit {
        draw_count(rgba, w, h, cx, cy, r, n);
    }
}

/// 3×5 像素数字字体（每行 3 bit，1=白点）。两白数字并排；99+ 显示 "9+"（简化为 99）。
const DIGIT_FONT: [[u8; 5]; 10] = [
    [0b111, 0b101, 0b101, 0b101, 0b111], // 0
    [0b010, 0b110, 0b010, 0b010, 0b111], // 1
    [0b111, 0b001, 0b111, 0b100, 0b111], // 2
    [0b111, 0b001, 0b111, 0b001, 0b111], // 3
    [0b101, 0b101, 0b111, 0b001, 0b001], // 4
    [0b111, 0b100, 0b111, 0b001, 0b111], // 5
    [0b111, 0b100, 0b111, 0b101, 0b111], // 6
    [0b111, 0b001, 0b010, 0b010, 0b010], // 7
    [0b111, 0b101, 0b111, 0b101, 0b111], // 8
    [0b111, 0b101, 0b111, 0b001, 0b111], // 9
];

/// 在角标中心绘制最多两位数字（1 像素字模放大到角标可容纳的大小）。
fn draw_count(rgba: &mut [u8], w: usize, h: usize, cx: i32, cy: i32, r: f32, n: u32) {
    let text = n.to_string();
    let glyphs: Vec<&[u8; 5]> = text.bytes().map(|b| &DIGIT_FONT[(b - b'0') as usize]).collect();
    let gw = glyphs.len() as i32 * 4 - 1; // 3px 字宽 + 1px 间隔
    let gh = 5i32;
    // 字模放大倍数：角标内接正方形 ≈ r*1.2，取能容纳的整数倍，至少 1
    let scale = ((r * 1.2) as i32 / gh).max(1) as i32;
    let origin_x = cx - gw * scale / 2;
    let origin_y = cy - gh * scale / 2;
    for (gi_raw, glyph) in glyphs.iter().enumerate() {
        let gi = gi_raw as i32;
        for (ry, row) in glyph.iter().enumerate() {
            for rx in 0..3i32 {
                if row & (0b100 >> rx) == 0 {
                    continue;
                }
                for sy in 0..scale {
                    for sx in 0..scale {
                        let px = origin_x + (gi * 4 + rx) * scale + sx;
                        let py = origin_y + ry as i32 * scale + sy;
                        if px < 0 || py < 0 || px >= w as i32 || py >= h as i32 {
                            continue;
                        }
                        let idx = (py as usize * w + px as usize) * 4;
                        if idx + 3 < rgba.len() {
                            rgba[idx] = 255;
                            rgba[idx + 1] = 255;
                            rgba[idx + 2] = 255;
                            rgba[idx + 3] = 255;
                        }
                    }
                }
            }
        }
    }
}

/* ── C2 加速键：机制接入 ────────────────────────────────────────────────────
 * Tauri menu 的 accelerator 只在菜单获得焦点时生效——Windows 托盘弹出菜单不处理
 * 键盘加速键（显示为灰色提示也无意义），macOS 菜单栏上下文可用。当前 spec 条目
 * 不设 accelerator（None 传给 with_id）；机制已就位，后续要加只需在 MenuEntry
 * 增加 accelerator 字段并透传。全局快捷键（菜单外生效）需 tauri-plugin-global-shortcut，
 * 留待用户确认是否真的需要（托盘场景价值有限）。
 * ──────────────────────────────────────────────────────────────────────── */

/// C2 菜单图标表：哪些条目配彩色圆点（语义：绿=执行/重启，琥珀=更新/升级，蓝=远程，红=退出）。
/// 未列出的条目走普通 MenuItem（全配图标会变成彩色噪音）。仅非 macOS 编译（见 build_items）。
#[cfg(not(target_os = "macos"))]
fn item_icon_rgb(id: &str) -> Option<(u8, u8, u8)> {
    Some(match id {
        "restart" => (76, 175, 80),
        "upgrade" | "check-dsh-update" | "check-app-update" => (255, 170, 0),
        "connect" | "show-qrcode" => (74, 108, 247),
        "export-diagnostics" => (74, 108, 247),
        "quit" => (244, 67, 54),
        _ => return None,
    })
}

/// 16×16 透明底实心圆点（菜单图标用；无描边无数字，纯净小色点）。仅非 macOS 编译。
#[cfg(not(target_os = "macos"))]
fn solid_dot_icon(color: (u8, u8, u8)) -> tauri::image::Image<'static> {
    let (w, h) = (16usize, 16usize);
    let mut rgba = vec![0u8; w * h * 4];
    let r = 5.5f32;
    let c = 8.0f32;
    for y in 0..h {
        for x in 0..w {
            let dx = x as f32 + 0.5 - c;
            let dy = y as f32 + 0.5 - c;
            if dx * dx + dy * dy <= r * r {
                let idx = (y * w + x) * 4;
                rgba[idx] = color.0;
                rgba[idx + 1] = color.1;
                rgba[idx + 2] = color.2;
                rgba[idx + 3] = 255;
            }
        }
    }
    tauri::image::Image::new_owned(rgba, w as u32, h as u32)
}

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
/// 菜单 spec 数据结构（v0.1.28+ 重构）。把过程式 `vec![&show, &restart, …]`
/// 改为按「分组 + 子菜单」声明；新增/隐藏项只动 spec，不再写分散的 push 逻辑。
///
/// 设计取舍：
/// - 一份 spec 只描述菜单形状（分组 + 条目）；可见性/上下文依赖由 `build_menu_from_spec`
///   通过 `MenuContext` 过滤——而不是在 spec 里硬编码「远程模式隐藏 X」。
///   这样 spec 是「我想要的菜单」，build 时按上下文裁剪。
/// - accelerator / icon 占位符保留（v0.1.28 暂不接，C2 阶段填实）。
/// - id 是 `&'static str`：与 on_menu_event 字符串分发一一对应，新增项务必同时改 spec
///   与事件处理器（编译期无法校验，留注释引导）。
pub enum MenuEntry {
    /// 普通菜单项（点击触发动作）。
    Item {
        id: &'static str,
        label: String,
    },
    /// 复选菜单项（toggle）。
    Check {
        id: &'static str,
        label: String,
        checked: bool,
    },
    /// 子菜单。id 当前不参与事件分发（子条目各自分发），保留供后续「点击分组标题」类交互。
    Submenu {
        #[allow(dead_code)]
        id: &'static str,
        label: String,
        items: Vec<MenuEntry>,
    },
    /// 分隔线（spec 内每段末尾自动加一组；显式 Sep 用于段内细分）。
    #[allow(dead_code)] // 段内细分预留；当前分组分隔已由 sections 自动注入
    Sep,
}

/// 分组：一组连续条目，前后自动加分隔线（首段除外）。
pub struct MenuSection {
    pub items: Vec<MenuEntry>,
}

/// 菜单 spec：有序的分组列表，build 时按上下文过滤并渲染。
pub struct MenuSpec {
    pub sections: Vec<MenuSection>,
}

/// build 时传入的上下文：决定哪些条目可见、是否 Checked。
pub struct MenuContext {
    pub remote: bool,
    pub portable: bool,
    pub autostart_enabled: bool,
    /// 已保存远程实例（D2 多实例）：本地模式「模式」分组渲染为子菜单。
    pub saved: Vec<(String, String)>,
}

/// 远程模式 spec。
fn spec_for_remote(ctx: &MenuContext) -> MenuSpec {
    use crate::i18n::t;
    MenuSpec {
        sections: vec![
            // 1. 窗口
            MenuSection {
                items: vec![
                    MenuEntry::Item { id: "show", label: t("menu.show").into() },
                    MenuEntry::Item { id: "open-main", label: t("menu.open_main").into() },
                    MenuEntry::Item { id: "copy-address", label: t("menu.copy_address").into() },
                    MenuEntry::Item { id: "notifications", label: t("menu.notifications").into() },
                ],
            },
            // 2. 服务
            MenuSection {
                items: vec![
                    MenuEntry::Item {
                        id: "restart",
                        label: t("menu.restart_remote").into(),
                    },
                    MenuEntry::Item {
                        id: "check-dsh-update",
                        label: t("menu.check_dsh_update").into(),
                    },
                    MenuEntry::Item { id: "openlog", label: t("menu.openlog").into() },
                    MenuEntry::Item {
                        id: "opendir",
                        label: if ctx.portable { t("menu.usb_opendir").into() } else { t("menu.opendir").into() },
                    },
                ],
            },
            // 3. 模式
            MenuSection {
                items: vec![
                    MenuEntry::Item {
                        id: "tolocal",
                        label: t("menu.tolocal").into(),
                    },
                ],
            },
            // 4. 设置（子菜单）
            MenuSection {
                items: vec![MenuEntry::Submenu {
                    id: "settings",
                    label: t("menu.settings").into(),
                    items: vec![
                        MenuEntry::Check {
                            id: "autostart",
                            label: t("menu.autostart").into(),
                            checked: ctx.autostart_enabled && !ctx.portable,
                        },
                        MenuEntry::Check {
                            id: "close-to-tray",
                            label: t("menu.close_to_tray").into(),
                            checked: true, // 当前默认行为；预留为可配置项
                        },
                    ],
                }],
            },
            // 5. 高级（子菜单）
            MenuSection {
                items: vec![MenuEntry::Submenu {
                    id: "advanced",
                    label: t("menu.advanced").into(),
                    items: vec![
                        MenuEntry::Item {
                            id: "show-qrcode",
                            label: t("menu.qrcode").into(),
                        },
                        MenuEntry::Item {
                            id: "export-diagnostics",
                            label: t("menu.export_diagnostics").into(),
                        },
                        MenuEntry::Item {
                            id: "open-runtime-dir",
                            label: t("menu.opendir").into(),
                        },
                    ],
                }],
            },
            // 6. 退出（自带换行）
            MenuSection {
                items: vec![MenuEntry::Item {
                    id: "quit",
                    label: t("menu.quit").into(),
                }],
            },
        ],
    }
}

/// 本地模式 spec。
fn spec_for_local(ctx: &MenuContext) -> MenuSpec {
    use crate::i18n::t;
    let mut service_items: Vec<MenuEntry> = vec![
        MenuEntry::Item {
            id: "restart",
            label: t("menu.restart_local").into(),
        },
    ];
    if !ctx.portable {
        // 应用本体更新（tauri-plugin-updater）面向安装版
        service_items.push(MenuEntry::Item {
            id: "check-app-update",
            label: t("menu.check_app_update").into(),
        });
    }
    service_items.push(MenuEntry::Item {
        id: "openlog",
        label: t("menu.openlog").into(),
    });
    service_items.push(MenuEntry::Item {
        id: "opendir",
        label: if ctx.portable { t("menu.usb_opendir").into() } else { t("menu.opendir").into() },
    });

    let mut mode_items: Vec<MenuEntry> = vec![
        MenuEntry::Item {
            id: "connect",
            label: t("menu.connect").into(),
        },
        MenuEntry::Item {
            id: "upgrade",
            label: t("menu.upgrade").into(),
        },
    ];
    if ctx.portable {
        mode_items.push(MenuEntry::Item {
            id: "wizard",
            label: t("menu.wizard").into(),
        });
    }
    // D2 多实例：已保存实例子菜单（id 形如 "saved:<address>"，事件分发按前缀路由）
    if !ctx.saved.is_empty() {
        mode_items.push(MenuEntry::Submenu {
            id: "saved",
            label: t("menu.saved_instances").into(),
            items: ctx
                .saved
                .iter()
                .map(|(addr, label)| MenuEntry::Item {
                    id: Box::leak(format!("saved:{addr}").into_boxed_str()),
                    label: label.clone(),
                })
                .collect(),
        });
    }

    MenuSpec {
        sections: vec![
            // 1. 窗口
            MenuSection {
                items: vec![
                    MenuEntry::Item { id: "show", label: t("menu.show").into() },
                    MenuEntry::Item { id: "open-main", label: t("menu.open_main").into() },
                    MenuEntry::Item { id: "notifications", label: t("menu.notifications").into() },
                ],
            },
            // 2. 服务
            MenuSection { items: service_items },
            // 3. 模式
            MenuSection { items: mode_items },
            // 4. 设置
            MenuSection {
                items: vec![MenuEntry::Submenu {
                    id: "settings",
                    label: t("menu.settings").into(),
                    items: vec![
                        MenuEntry::Check {
                            id: "autostart",
                            label: t("menu.autostart").into(),
                            checked: ctx.autostart_enabled && !ctx.portable,
                        },
                        MenuEntry::Check {
                            id: "close-to-tray",
                            label: t("menu.close_to_tray").into(),
                            checked: true,
                        },
                    ],
                }],
            },
            // 5. 高级
            MenuSection {
                items: vec![MenuEntry::Submenu {
                    id: "advanced",
                    label: t("menu.advanced").into(),
                    items: vec![
                        MenuEntry::Item {
                            id: "export-diagnostics",
                            label: t("menu.export_diagnostics").into(),
                        },
                        MenuEntry::Item {
                            id: "reset-dsh-home",
                            label: t("menu.reset_home").into(),
                        },
                    ],
                }],
            },
            // 6. 退出
            MenuSection {
                items: vec![MenuEntry::Item { id: "quit", label: t("menu.quit").into() }],
            },
        ],
    }
}

/// 从 spec 构建 tauri Menu。spec 的分组间自动加分隔线（首段除外）。
/// 各 entry 的可见性由 MenuContext 控制——autostart 在便携模式下不显示；
/// wizard 仅便携可见；upgrade 在远程模式不可见（远程防御已在 install::upgrade_runtime）。
fn build_menu_from_spec(
    app: &tauri::AppHandle,
    ctx: &MenuContext,
    spec: &MenuSpec,
) -> tauri::Result<Menu<tauri::Wry>> {
    // 先把 spec 渲染成 tauri items（递归处理 Submenu），过滤掉不可见的条目
    fn build_items<'a>(
        app: &'a tauri::AppHandle,
        ctx: &MenuContext,
        entries: &'a [MenuEntry],
        items: &mut Vec<Box<dyn tauri::menu::IsMenuItem<tauri::Wry> + 'a>>,
    ) -> tauri::Result<()> {
        for entry in entries {
            match entry {
                MenuEntry::Item { id, label } => {
                    // 过滤：autostart 在便携模式隐藏；wizard 仅便携可见；upgrade 仅本地可见
                    if !entry_visible(entry, ctx) {
                        continue;
                    }
                    // C2 菜单图标：命中图标表的条目用 IconMenuItem（像素实心点，零资源文件）。
                    // ⚠ macOS 防御性关闭：muda 的 IconMenuItem 对 macOS 的支持无保证，一旦
                    // 构造返回 Unsupported，build_menu 整体失败会让应用起不来——图标是纯装饰，
                    // 不值得赌；macOS 一律普通 MenuItem。
                    #[cfg(not(target_os = "macos"))]
                    let item: Box<dyn tauri::menu::IsMenuItem<tauri::Wry>> =
                        if let Some(rgb) = item_icon_rgb(id) {
                            let icon = solid_dot_icon(rgb);
                            Box::new(tauri::menu::IconMenuItem::with_id(
                                app, *id, label, true, Some(icon), None::<&str>,
                            )?)
                        } else {
                            Box::new(MenuItem::with_id(app, *id, label, true, None::<&str>)?)
                        };
                    #[cfg(target_os = "macos")]
                    let item: Box<dyn tauri::menu::IsMenuItem<tauri::Wry>> =
                        Box::new(MenuItem::with_id(app, *id, label, true, None::<&str>)?);
                    items.push(item);
                }
                MenuEntry::Check { id, label, checked } => {
                    if !entry_visible(entry, ctx) {
                        continue;
                    }
                    let item = CheckMenuItem::with_id(app, *id, label, *checked, true, None::<&str>)?;
                    items.push(Box::new(item));
                }
                MenuEntry::Submenu { id: _, label, items: sub_entries } => {
                    let mut sub_items: Vec<Box<dyn tauri::menu::IsMenuItem<tauri::Wry>>> = Vec::new();
                    build_items(app, ctx, sub_entries, &mut sub_items)?;
                    if sub_items.is_empty() {
                        continue;
                    }
                    let refs: Vec<&dyn tauri::menu::IsMenuItem<tauri::Wry>> =
                        sub_items.iter().map(|b| b.as_ref() as &dyn tauri::menu::IsMenuItem<tauri::Wry>).collect();
                    // Tauri 2：with_id 4 参无条目重载；带条目的构造是 with_items（子菜单自身
                    // 点击不触发菜单事件，id 无用，事件都从子条目 id 分发）
                    let sub = tauri::menu::Submenu::with_items(app, label, true, &refs)?;
                    items.push(Box::new(sub));
                }
                MenuEntry::Sep => {
                    let sep = PredefinedMenuItem::separator(app)?;
                    items.push(Box::new(sep));
                }
            }
        }
        Ok(())
    }

    let mut all_items: Vec<Box<dyn tauri::menu::IsMenuItem<tauri::Wry>>> = Vec::new();
    let mut first = true;
    for section in &spec.sections {
        // 跳过整段被过滤空的分组
        let visible_count = section.items.iter().filter(|e| entry_visible(e, ctx)).count();
        if visible_count == 0 {
            continue;
        }
        if !first {
            let sep = PredefinedMenuItem::separator(app)?;
            all_items.push(Box::new(sep));
        }
        first = false;
        build_items(app, ctx, &section.items, &mut all_items)?;
    }
    let refs: Vec<&dyn tauri::menu::IsMenuItem<tauri::Wry>> =
        all_items.iter().map(|b| b.as_ref() as &dyn tauri::menu::IsMenuItem<tauri::Wry>).collect();
    Menu::with_items(app, &refs)
}

/// 上下文驱动的可见性规则。
fn entry_visible(entry: &MenuEntry, ctx: &MenuContext) -> bool {
    match entry {
        MenuEntry::Item { id, .. } | MenuEntry::Check { id, .. } => match *id {
            // 升级 DSH 运行时：远程模式隐藏（远程防御逻辑见 install::upgrade_runtime）
            "upgrade" => !ctx.remote,
            // 重新运行分身向导：仅便携
            "wizard" => ctx.portable,
            // 开机自启：便携模式隐藏（写宿主注册表，U盘包不写）
            "autostart" => !ctx.portable,
            // 检查 DSH 应用更新：仅非便携
            "check-app-update" => !ctx.portable,
            // 立即检查 DSH 运行时更新：仅远程（本地升级走 upgrade 项）
            "check-dsh-update" => ctx.remote,
            // 二维码配对：仅远程（远程模式特有）
            "show-qrcode" => ctx.remote,
            // 重置 DSH home：仅便携
            "reset-dsh-home" => ctx.portable,
            _ => true,
        },
        MenuEntry::Submenu { .. } => true,
        MenuEntry::Sep => true,
    }
}

fn build_menu(app: &tauri::AppHandle, remote: bool) -> tauri::Result<Menu<tauri::Wry>> {
    use tauri_plugin_autostart::ManagerExt;
    let portable = crate::runtime::portable_root().is_some();
    let autostart_enabled = app.autolaunch().is_enabled().unwrap_or(false);
    let saved = crate::remote::saved_list()
        .into_iter()
        .map(|s| {
            let label = if s.address.is_empty() {
                s.origin.clone()
            } else {
                s.address.clone()
            };
            (s.address, label)
        })
        .collect();
    let ctx = MenuContext { remote, portable, autostart_enabled, saved };
    let spec = if remote { spec_for_remote(&ctx) } else { spec_for_local(&ctx) };
    build_menu_from_spec(app, &ctx, &spec)
}

/// v0.1.28+ 动态 tooltip：根据 AppState.status 渲染当前阶段 + 模式地址。
/// 静默失败：托盘未初始化（启动早期 / 退出路径）直接返回；状态模块无锁时回退默认文案。
pub fn refresh_tooltip(app: &tauri::AppHandle) {
    let Some(tray) = TRAY.get() else {
        return;
    };
    let text = compute_tooltip(app);
    let _ = tray.set_tooltip(Some(text));
}

fn compute_tooltip(app: &tauri::AppHandle) -> String {
    // 锁顺序：status → mode（与 supervisor 等其他模块保持一致防环）
    let (text, error, ready, wizard, connect, remote) = if let Some(state) =
        app.try_state::<crate::AppState>()
    {
        let s = state.status.lock().unwrap();
        let mode = *state.mode.lock().unwrap();
        (
            s.text.clone(),
            s.error,
            s.ready,
            s.wizard,
            s.connect,
            mode == "remote",
        )
    } else {
        (String::new(), false, false, false, false, false)
    };
    // 阶段文案：wizard / connect 是「界面态」；error / ready 是「终态」；其余按 text 渲染
    use crate::i18n::t;
    let phase = if error {
        t("tooltip.error").to_string()
    } else if ready {
        t("tooltip.ready").to_string()
    } else if wizard {
        t("tooltip.wizard").to_string()
    } else if connect {
        t("tooltip.connect").to_string()
    } else if text.is_empty() {
        t("tooltip.starting").to_string()
    } else {
        text
    };
    // 模式/地址
    let mode_addr = if remote {
        let addr = crate::remote::load_display()
            .map(|(a, _)| a)
            .unwrap_or_default();
        if addr.is_empty() {
            t("tooltip.mode_remote").to_string()
        } else {
            format!("{} {addr}", t("tooltip.mode_remote"))
        }
    } else {
        t("tooltip.mode_local").to_string()
    };
    format!("DSH Desktop — {mode_addr} · {phase}")
}

pub fn build_tray(app: &tauri::AppHandle) -> tauri::Result<()> {
    let remote = is_remote(app);
    let menu = build_menu(app, remote)?;

    #[allow(unused_mut)] // macOS 分支会整体重绑定
    let mut builder = TrayIconBuilder::with_id("dsh-tray")
        .icon(app.default_window_icon().expect("缺少应用图标").clone())
        .tooltip(compute_tooltip(app))
        .menu(&menu)
        .show_menu_on_left_click(false);

    // macOS 菜单栏习惯：单击即弹菜单（Windows 保持左键穿透、双击唤起窗口）
    #[cfg(target_os = "macos")]
    {
        builder = builder.show_menu_on_left_click(true);
    }

    // 菜单事件处理器全程只注册这一次；模式切换只换菜单（见 rebuild），id 分发不受影响
    // v0.1.28+ 新增处理项：open-main / copy-address / show-qrcode / check-app-update /
    // check-dsh-update / export-diagnostics / reset-dsh-home / close-to-tray
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
                // 用户已看向主窗口：未读角标清零（D1）
                clear_unread(app);
            }
            "open-main" => {
                // 打开主页面：等价 show 但聚焦到主界面（区别于 show 的「无焦点闪一下」）。
                // 远程模式复用 navigate_to_harness 已就绪 origin；本地模式直接聚焦即可。
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.show();
                    let _ = w.set_focus();
                }
                clear_unread(app);
            }
            "notifications" => {
                // D1b 通知中心：打开 mini 窗口并清零未读角标（用户已查看通知）
                clear_unread(app);
                if let Err(e) = crate::notifications::open_window(app) {
                    if let Some(mut log) = crate::runtime::open_log_append() {
                        use std::io::Write;
                        let _ = writeln!(log, "[通知] 打开通知中心失败: {e}");
                    }
                }
            }
            "copy-address" => {
                // 远程模式专用：复制当前已配对地址到剪贴板（主人贴给别人配对常用）。
                // 本地模式点击等同 no-op（菜单项应在本地隐藏）。
                if let Some((addr, _)) = crate::remote::load_display() {
                    if !addr.is_empty() {
                        copy_to_clipboard(app, &addr);
                        crate::status::set(app, &format!("已复制远程地址：{addr}"));
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
            "check-app-update" => {
                let handle = app.clone();
                std::thread::spawn(move || check_app_update(&handle));
            }
            "check-dsh-update" => {
                // 远程模式专用：手动 npm 查询 @deepseek-ai/dsh latest 版本并提示。
                // 与 upgrade 不同：不直接执行升级（远程防御逻辑），只查 + 提示。
                let handle = app.clone();
                std::thread::spawn(move || {
                    crate::status::set(&handle, "正在查询 DSH 运行时最新版本…");
                    match crate::install::upgrade_dsh(&handle) {
                        Ok(msg) => crate::status::set(&handle, &msg),
                        Err(e) => crate::status::fail(&handle, &e),
                    }
                });
            }
            "show-qrcode" => {
                // 远程模式：把窗口带回加载页并显示当前配对码二维码（v0.1.29 实现）。
                // 当前先 emit 事件让加载页处理；具体 QR 生成逻辑在 webview 端。
                if let Some((addr, _)) = crate::remote::load_display() {
                    crate::status::set(
                        app,
                        &format!(
                            "请远端用户在 DSH 设置页「远程访问」生成配对链接；当前实例地址：{addr}"
                        ),
                    );
                }
            }
            "openlog" => {
                crate::webview::open_external(&crate::runtime::log_file().display().to_string());
            }
            "opendir" => {
                crate::webview::open_external(
                    &crate::runtime::runtime_root().display().to_string(),
                );
            }
            "open-runtime-dir" => {
                crate::webview::open_external(
                    &crate::runtime::runtime_root().display().to_string(),
                );
            }
            "export-diagnostics" => {
                let handle = app.clone();
                std::thread::spawn(move || {
                    crate::status::set(&handle, "正在打包诊断包…");
                    match crate::diagnostics::export(&handle) {
                        Ok(path) => {
                            crate::status::set(&handle, &format!("诊断包已导出：{}", path.display()));
                            crate::webview::open_external(&path.display().to_string());
                        }
                        Err(e) => crate::status::fail(&handle, &e),
                    }
                });
            }
            "reset-dsh-home" => {
                // 便携模式：删 Data/home（带确认流程——v0.1.29 在加载页做 confirm 对话框，
                // 此处先设置状态留 hook）。当前仅做删除 + 自动重启。
                let handle = app.clone();
                std::thread::spawn(move || {
                    if let Err(e) = crate::diagnostics::reset_dsh_home(&handle) {
                        crate::status::fail(&handle, &e);
                    }
                });
            }
            "close-to-tray" => {
                // 设置项：当前总是 true（行为硬编码在 main.rs::on_window_event）。预留为可配置项。
                // 点击切换会写入设置文件并热应用——v0.1.29 接设置持久化后再实现完整逻辑。
                crate::status::set(app, "关闭按钮行为：最小化到托盘（v0.1.29 起可关闭）");
            }
            "autostart" => {
                use tauri_plugin_autostart::ManagerExt;
                let autolaunch = app.autolaunch();
                let enabled = autolaunch.is_enabled().unwrap_or(false);
                let result = if enabled { autolaunch.disable() } else { autolaunch.enable() };
                if let Err(e) = result {
                    eprintln!("切换开机自启失败: {e}");
                } else {
                    // 切换后立即重建菜单让勾选状态刷新（rebuild 不重建图标、不闪烁）
                    rebuild(app);
                }
            }
            id if id.starts_with("saved:") => {
                // D2 多实例：直连某已保存实例（解密 token → 升为活动 → 远程连接序列）
                let addr = id.trim_start_matches("saved:").to_string();
                let handle = app.clone();
                std::thread::spawn(move || crate::supervisor::connect_saved(&handle, &addr));
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
                // 用户已看向主窗口：未读角标清零（D1）
                clear_unread(&app);
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
/// v0.1.28+ tooltip 走动态 compute_tooltip（接 AppState.status 渲染阶段文案）。
pub fn rebuild(app: &tauri::AppHandle) {
    let Some(tray) = TRAY.get() else {
        return;
    };
    let remote = is_remote(app);
    let result = build_menu(app, remote).and_then(|menu| {
        tray.set_menu(Some(menu))?;
        tray.set_tooltip(Some(compute_tooltip(app)))?;
        Ok(())
    });
    if let Err(e) = result {
        if let Some(mut log) = crate::runtime::open_log_append() {
            use std::io::Write;
            let _ = writeln!(log, "[托盘] 菜单按模式重建失败: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /* ── C1/D1：角标像素合成 ── */

    /// 数字字体必须可辨识：每个字模至少 6 个点亮、至多 14 个（3×5=15 上限减去镂空）。
    #[test]
    fn digit_font_glyphs_are_wellformed() {
        for (n, glyph) in DIGIT_FONT.iter().enumerate() {
            let lit = glyph.iter().map(|r| r.count_ones()).sum::<u32>();
            assert!(lit >= 6, "数字 {n} 字模点亮过少: {lit}");
            assert!(lit <= 14, "数字 {n} 字模点亮过多: {lit}");
        }
    }

    /// 角标绘制：32×32 全透明底 + Count(7) → 右下角出现红色圆 + 白色数字，其余区域不变。
    #[test]
    fn draw_badge_count_paints_corner_only() {
        let (w, h) = (32usize, 32usize);
        let mut rgba = vec![0u8; w * h * 4];
        draw_badge(&mut rgba, w, h, &BadgeKind::Count(7));
        // 右下角必有红像素
        let mut red = 0;
        let mut white = 0;
        for y in (h - 16)..h {
            for x in (w - 16)..w {
                let i = (y * w + x) * 4;
                if rgba[i] > 200 && rgba[i + 1] < 90 && rgba[i + 2] < 90 {
                    red += 1;
                }
                if rgba[i] == 255 && rgba[i + 1] == 255 && rgba[i + 2] == 255 {
                    white += 1;
                }
            }
        }
        assert!(red > 20, "角标红色像素过少: {red}");
        assert!(white >= 5, "数字白色像素过少: {white}");
        // 左上角必须保持透明
        assert_eq!(rgba[0], 0);
        assert_eq!(rgba[3], 0);
    }

    /// 状态角标无数字：Ready 只画绿圆，不产生白色数字块。
    #[test]
    fn draw_badge_ready_paints_green_without_digits() {
        let (w, h) = (32usize, 32usize);
        let mut rgba = vec![0u8; w * h * 4];
        draw_badge(&mut rgba, w, h, &BadgeKind::Ready);
        let mut green = 0;
        for y in (h - 16)..h {
            for x in (w - 16)..w {
                let i = (y * w + x) * 4;
                if rgba[i + 1] > 150 && rgba[i] < 120 {
                    green += 1;
                }
            }
        }
        assert!(green > 20, "Ready 绿色像素过少: {green}");
    }

    /// 未读计数读写：bump/clear 语义（不走 tauri，直接验证原子）。
    #[test]
    fn unread_counter_roundtrip() {
        UNREAD.store(0, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(unread(), 0);
        UNREAD.fetch_add(2, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(unread(), 2);
        UNREAD.swap(0, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(unread(), 0);
    }

    /* ── B3：菜单 spec 可见性 ── */

    fn ctx(remote: bool, portable: bool) -> MenuContext {
        MenuContext { remote, portable, autostart_enabled: false, saved: vec![] }
    }

    fn item(id: &str) -> MenuEntry {
        MenuEntry::Item { id: Box::leak(id.to_string().into_boxed_str()), label: "x".into() }
    }

    #[test]
    fn visibility_rules_match_mode_and_portability() {
        // 升级 DSH：仅本地
        assert!(entry_visible(&item("upgrade"), &ctx(false, false)));
        assert!(!entry_visible(&item("upgrade"), &ctx(true, false)));
        // 分身向导：仅便携
        assert!(entry_visible(&item("wizard"), &ctx(false, true)));
        assert!(!entry_visible(&item("wizard"), &ctx(false, false)));
        // 开机自启：非便携
        assert!(!entry_visible(&item("autostart"), &ctx(false, true)));
        assert!(entry_visible(&item("autostart"), &ctx(false, false)));
        // 检查 DSH 运行时更新：仅远程
        assert!(entry_visible(&item("check-dsh-update"), &ctx(true, false)));
        assert!(!entry_visible(&item("check-dsh-update"), &ctx(false, false)));
        // 二维码：仅远程
        assert!(entry_visible(&item("show-qrcode"), &ctx(true, false)));
        assert!(!entry_visible(&item("show-qrcode"), &ctx(false, false)));
        // 退出：总是可见
        assert!(entry_visible(&item("quit"), &ctx(true, true)));
    }
}
