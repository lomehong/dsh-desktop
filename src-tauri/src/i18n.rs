//! i18n：轻量双语（zh-CN 默认 / en-US）。零依赖字符串表。
//!
//! 语言决策（一次性，进程内缓存）：runtime_root/lang.txt（"en" / "zh"）> 系统 locale
//! （LANG / LC_ALL / Windows UI Culture 探测失败即 zh-CN）。
//! 设置入口（加载页/托盘）后续写 lang.txt 并提示重启托盘 rebuild 即可生效。
//!
//! 用法：`i18n::t("menu.quit")`。缺 key 返回 zh-CN 值兜底，再缺返回 key 本身（可排查）。
use std::sync::OnceLock;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    ZhCn,
    EnUs,
}

impl Lang {
    fn from_str(s: &str) -> Lang {
        let s = s.trim().to_ascii_lowercase();
        if s.starts_with("en") {
            Lang::EnUs
        } else {
            Lang::ZhCn
        }
    }
}

static LANG: OnceLock<Lang> = OnceLock::new();

/// 当前语言（进程内只解析一次）。
pub fn lang() -> Lang {
    *LANG.get_or_init(detect)
}

fn detect() -> Lang {
    // 1) 显式设置文件
    let p = crate::runtime::runtime_root().join("lang.txt");
    if let Ok(s) = std::fs::read_to_string(&p) {
        return Lang::from_str(&s);
    }
    // 2) 系统 locale（Unix：LANG/LC_ALL；Windows：无 libc 依赖，探测常见 env 兜底）
    for k in ["LC_ALL", "LANG"] {
        if let Ok(v) = std::env::var(k) {
            if !v.is_empty() && !v.starts_with("C.") {
                return Lang::from_str(&v);
            }
        }
    }
    Lang::ZhCn
}

/// 查表。key 形如 "menu.quit"。缺 key 回退 zh-CN，再缺回 key 本身（可排查）。
pub fn t(key: &str) -> String {
    match lang() {
        Lang::ZhCn => zh(key).unwrap_or(key).to_string(),
        Lang::EnUs => en(key)
            .map(str::to_string)
            .unwrap_or_else(|| zh(key).unwrap_or(key).to_string()),
    }
}

fn zh(key: &str) -> Option<&'static str> {
    Some(match key {
        "menu.show" => "显示 / 隐藏",
        "menu.open_main" => "打开主页面",
        "menu.notifications" => "通知中心",
        "menu.copy_address" => "复制当前地址",
        "menu.restart_local" => "重启服务",
        "menu.restart_remote" => "重连远程实例",
        "menu.connect" => "连接远程实例…",
        "menu.tolocal" => "断开远程，回到本地",
        "menu.upgrade" => "升级 DSH 运行时",
        "menu.wizard" => "重新运行分身向导",
        "menu.check_app_update" => "检查 DSH Desktop 应用更新",
        "menu.check_dsh_update" => "立即检查 DSH 运行时更新",
        "menu.openlog" => "打开日志",
        "menu.opendir" => "打开数据目录",
        "menu.usb_opendir" => "打开U盘数据目录",
        "menu.saved_instances" => "已保存的远程实例",
        "menu.settings" => "设置",
        "menu.autostart" => "开机自启",
        "menu.close_to_tray" => "关闭按钮 = 最小化到托盘",
        "menu.advanced" => "高级",
        "menu.qrcode" => "二维码配对…",
        "menu.export_diagnostics" => "导出诊断包…",
        "menu.reset_home" => "重置 DSH home…",
        "menu.quit" => "退出",
        "tooltip.ready" => "✓ 服务已就绪",
        "tooltip.error" => "⚠ 服务异常",
        "tooltip.wizard" => "正在配置分身信息…",
        "tooltip.connect" => "正在连接远程实例…",
        "tooltip.starting" => "正在启动…",
        "tooltip.mode_local" => "本地模式",
        "tooltip.mode_remote" => "远程模式",
        "status.copied" => "已复制远程地址",
        _ => return None,
    })
}

fn en(key: &str) -> Option<&'static str> {
    Some(match key {
        "menu.show" => "Show / Hide",
        "menu.open_main" => "Open Main Window",
        "menu.copy_address" => "Copy Address",
        "menu.notifications" => "Notification Center",
        "menu.restart_local" => "Restart Service",
        "menu.restart_remote" => "Reconnect Remote",
        "menu.connect" => "Connect to Remote…",
        "menu.tolocal" => "Disconnect, Back to Local",
        "menu.upgrade" => "Upgrade DSH Runtime",
        "menu.wizard" => "Rerun Persona Wizard",
        "menu.check_app_update" => "Check for App Updates",
        "menu.check_dsh_update" => "Check DSH Runtime Updates Now",
        "menu.openlog" => "Open Log",
        "menu.opendir" => "Open Data Folder",
        "menu.usb_opendir" => "Open USB Data Folder",
        "menu.saved_instances" => "Saved Remote Instances",
        "menu.settings" => "Settings",
        "menu.autostart" => "Launch at Login",
        "menu.close_to_tray" => "Close Button = Minimize to Tray",
        "menu.advanced" => "Advanced",
        "menu.qrcode" => "Pairing QR Code…",
        "menu.export_diagnostics" => "Export Diagnostics…",
        "menu.reset_home" => "Reset DSH Home…",
        "menu.quit" => "Quit",
        "tooltip.ready" => "✓ Service Ready",
        "tooltip.error" => "⚠ Service Error",
        "tooltip.wizard" => "Configuring persona…",
        "tooltip.connect" => "Connecting to remote…",
        "tooltip.starting" => "Starting…",
        "tooltip.mode_local" => "Local",
        "tooltip.mode_remote" => "Remote",
        "status.copied" => "Remote address copied",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zh_table_has_all_menu_keys() {
        for k in [
            "menu.show", "menu.open_main", "menu.notifications", "menu.copy_address", "menu.restart_local",
            "menu.restart_remote", "menu.connect", "menu.tolocal", "menu.upgrade",
            "menu.wizard", "menu.check_app_update", "menu.check_dsh_update",
            "menu.openlog", "menu.opendir", "menu.usb_opendir", "menu.saved_instances",
            "menu.settings", "menu.autostart", "menu.close_to_tray", "menu.advanced",
            "menu.qrcode", "menu.export_diagnostics", "menu.reset_home", "menu.quit",
            "tooltip.ready", "tooltip.error", "tooltip.wizard", "tooltip.connect",
            "tooltip.starting", "tooltip.mode_local", "tooltip.mode_remote",
        ] {
            assert!(zh(k).is_some(), "zh 缺 key: {k}");
            assert!(en(k).is_some(), "en 缺 key: {k}");
        }
    }

    #[test]
    fn missing_key_falls_back_to_key_itself() {
        assert_eq!(zh("nope.nope"), None);
        assert_eq!(t("nope.nope"), "nope.nope");
    }

    #[test]
    fn lang_parse_accepts_en_and_defaults_zh() {
        assert_eq!(Lang::from_str("en-US"), Lang::EnUs);
        assert_eq!(Lang::from_str("en"), Lang::EnUs);
        assert_eq!(Lang::from_str("zh_CN.UTF-8"), Lang::ZhCn);
        assert_eq!(Lang::from_str("garbage"), Lang::ZhCn);
    }
}
