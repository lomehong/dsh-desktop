//! Windows JumpList（D3b）：任务栏图标右键「任务」列表——打开主页面 / 重启服务 /
//! 连接远程实例 / 通知中心 / 打开日志 / 导出诊断包。
//!
//! 工作方式：任务 = 带 CLI 参数的 exe 快捷方式。点击任务时：
//! - 若已有实例在跑：single-instance 插件把 argv 转发给首实例 → handle_cli_action 执行；
//! - 若无实例：冷启动带参 → setup 末尾 handle_cli_action 执行。
//!
//! 实现：ICustomDestinationList COM（windows crate 类型化封装）。三个 coclass GUID
//! （DestinationList / EnumerableObjectCollection / ShellLink）与 PKEY_Title 的 fmtid
//! 均取自 windows-0.61.3 生成源码（Shell/mod.rs:8037/8257/56159、EnhancedStorage:1881），
//! 不手算不猜测。任务标题必须经 IPropertyStore + PKEY_Title 设置（资源管理器据此渲染）。
//! 整体尽力而为：任何 COM 失败仅记日志，绝不影响主流程。
//!
//! 非 Windows 平台 update() 是 no-op。

/// 更新 JumpList 任务列表。在 setup 中调用一次（启动早期）；任务内容与模式无关，无需重调。
pub fn update(app: &tauri::AppHandle) {
    #[cfg(windows)]
    {
        if let Err(e) = update_windows() {
            if let Some(mut log) = crate::runtime::open_log_append() {
                use std::io::Write;
                let _ = writeln!(log, "[jumplist] 更新失败（尽力而为，忽略）: {e}");
            }
        }
        let _ = app;
    }
    #[cfg(not(windows))]
    {
        let _ = app;
    }
}

// PKEY_Title（fmtid/pid 与 Windows SDK / windows crate EnhancedStorage::PKEY_Title 逐位一致）
#[cfg(windows)]
const PKEY_TITLE: windows::Win32::Foundation::PROPERTYKEY = windows::Win32::Foundation::PROPERTYKEY {
    fmtid: windows::core::GUID::from_u128(0xf29f85e0_4ff9_1068_ab91_08002b27b3d9),
    pid: 2,
};

#[cfg(windows)]
fn update_windows() -> Result<(), String> {
    use windows::core::{HSTRING, Interface, PWSTR};
    use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_APARTMENTTHREADED};
    use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
    use windows::Win32::System::Variant::{VT_EMPTY, VT_LPWSTR};
    use windows::Win32::UI::Shell::Common::{IObjectArray, IObjectCollection};
    use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
    use windows::Win32::UI::Shell::{
        DestinationList, EnumerableObjectCollection, ICustomDestinationList, IShellLinkW, ShellLink,
    };


    unsafe {
        // 主线程 COM 初始化。RPC_E_CHANGED_MODE（0x80010106，已在其他套间初始化）可容忍：
        // CoCreateInstance 只要求「已初始化」，不要求特定套间。
        if let Err(e) = CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok() {
            if e.code().0 != 0x8001_0106u32 as i32 {
                return Err(format!("CoInitializeEx 失败: {e}"));
            }
        }

        let exe = std::env::current_exe().map_err(|e| format!("current_exe 失败: {e}"))?;
        let exe_h = HSTRING::from(exe.as_os_str());

        let list: ICustomDestinationList =
            CoCreateInstance(&DestinationList, None, CLSCTX_ALL).map_err(com_err)?;

        // BeginList 必须先调用（语义：取得被用户移除的项；这里不消费但不可省略）
        let mut min_slots: u32 = 0;
        let _removed: IObjectArray = list.BeginList(&mut min_slots).map_err(com_err)?;

        let coll: IObjectCollection =
            CoCreateInstance(&EnumerableObjectCollection, None, CLSCTX_ALL).map_err(com_err)?;

        // (CLI 参数, 标题 i18n key)——动作语义与本地/远程模式无关
        let tasks: &[(&str, &str)] = &[
            ("--open-main", "menu.open_main"),
            ("--restart-service", "menu.restart_local"),
            ("--connect-remote", "menu.connect"),
            ("--open-notifications", "menu.notifications"),
            ("--open-log", "menu.openlog"),
            ("--export-diagnostics", "menu.export_diagnostics"),
        ];
        for (arg, label_key) in tasks {
            let title = crate::i18n::t(label_key);
            let link: IShellLinkW =
                CoCreateInstance(&ShellLink, None, CLSCTX_ALL).map_err(com_err)?;
            link.SetPath(&exe_h).map_err(com_err)?;
            link.SetArguments(&HSTRING::from(*arg)).map_err(com_err)?;
            // 图标沿用 exe 自带图标；索引 0
            link.SetIconLocation(&exe_h, 0).map_err(com_err)?;

            // 任务标题走属性系统（PKEY_Title / VT_LPWSTR）；title_h 必须活过 SetValue。
            // PROPVARIANT 联合体字段经 ManuallyDrop 包裹，赋值需显式可变借用。
            let store: IPropertyStore = link.cast().map_err(com_err)?;
            let title_h = HSTRING::from(title);
            let mut pv = PROPVARIANT::default();
            let fields = &mut pv.Anonymous.Anonymous;
            fields.vt = VT_LPWSTR;
            fields.Anonymous.pwszVal = PWSTR(title_h.as_ptr() as *mut u16);
            store.SetValue(&PKEY_TITLE, &pv).map_err(com_err)?;
            // ⚠ 堆损坏修复（0xc0000374，2026-09-04 真机崩溃复盘）：
            // SetValue 已把值拷进属性存储，但 pv 里的 pwszVal 仍指向 HSTRING 内部缓冲
            // （非 CoTaskMemAlloc 的 COM 堆）。PROPVARIANT 实现了 Drop（内部 PropVariantClear，
            // 对 VT_LPWSTR 走 CoTaskMemFree）——不清 vt 就是每次循环结束一次堆损坏。
            // 立即把 vt 置回 VT_EMPTY，Drop 变为无操作；title_h 自行正常释放。
            let clear = &mut pv.Anonymous.Anonymous;
            clear.vt = VT_EMPTY;
            store.Commit().map_err(com_err)?;

            coll.AddObject(&link).map_err(com_err)?;
        }

        list.AddUserTasks(&coll).map_err(com_err)?;
        list.CommitList().map_err(com_err)?;
        Ok(())
    }
}

/// COM 错误统一转可读字符串（HRESULT + 消息）。
#[cfg(windows)]
fn com_err(e: windows::core::Error) -> String {
    format!("0x{:08X} {}", e.code().0 as u32, e.message())
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    /// 任务表语义自检：参数以 -- 开头且 i18n key 都在语言表里（防改 key 忘改表）。
    #[test]
    fn task_table_keys_exist_in_i18n() {
        for (arg, key) in [
            ("--open-main", "menu.open_main"),
            ("--restart-service", "menu.restart_local"),
            ("--connect-remote", "menu.connect"),
            ("--open-notifications", "menu.notifications"),
            ("--open-log", "menu.openlog"),
            ("--export-diagnostics", "menu.export_diagnostics"),
        ] {
            assert!(arg.starts_with("--"), "参数必须以 -- 开头: {arg}");
            assert!(
                crate::i18n::t(key) != key,
                "i18n 缺 key: {key}（t() 回退返回了 key 本身）"
            );
        }
    }

    /// PKEY_TITLE 与 Windows SDK 定义逐位一致（fmtid F29F85E0-4FF9-1068-AB91-08002B27B3D9, pid=2）。
    #[test]
    fn pkey_title_matches_sdk() {
        let guid = windows::core::GUID::from_u128(0xf29f85e0_4ff9_1068_ab91_08002b27b3d9);
        assert_eq!(PKEY_TITLE.fmtid, guid);
        assert_eq!(PKEY_TITLE.pid, 2);
    }
}
