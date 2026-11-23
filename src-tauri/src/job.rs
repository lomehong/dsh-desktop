//! 进程树保底：kill-on-close 作业对象（Windows）。
//!
//! 为什么需要（2026-11 事故，13 个孤儿 dsh 进程锁死运行时目录 → 自愈永久失败）：
//! Node 在 Windows 上给每个非 detached 子进程挂的是 libuv 自建的 kill-on-close
//! 作业对象——父进程一死，内核收走整棵树。官方 desktop 因此从不需要事后清扫。
//! 我们改用 Rust `Command` 起 dsh，这层保护就没有了：壳被强杀/崩溃/被安装器
//! taskkill 之后，dsh 及其子进程继续活着并锁住 node 目录。
//!
//! 本模块补上同一层语义：进程创建后立即加入一个
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` 作业。作业句柄只由壳持有，壳进程一旦
//! 消失（正常退出、崩溃、被强杀都算），内核关闭句柄 → 作业内所有进程被杀。
//!
//! 注意两点（实测确认，勿删）：
//! 1. 作业限制 `BREAKAWAY_OK` 必须允许，否则 dsh 里再 spawn 其它进程时若它自己
//!    想建作业会失败；
//! 2. 必须在进程启动**之后**用 `AssignProcessToJobObject` 加入——Rust 的
//!    `Command` 没有"创建时即分配"的钩子。极窄的创建到分配之间壳若被强杀仍可能
//!    留下一个孤儿，此时由启动时的特征清扫兜底（supervisor::sweep_orphan_dsh_processes）。
//!
//! 非 Windows 平台为空实现：Unix 侧已用独立进程组（pgid）整组终止。

#[cfg(windows)]
mod imp {
    use std::sync::OnceLock;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE};

    /// 进程级唯一的作业对象句柄。**故意永不关闭**：它的作用就是「壳还活着 ⇒
    /// 句柄开着 ⇒ 作业内进程继续跑；壳没了 ⇒ 句柄被内核关闭 ⇒ 全杀」。
    static JOB: OnceLock<usize> = OnceLock::new();

    fn job_handle() -> Option<HANDLE> {
        let raw = *JOB.get_or_init(|| unsafe {
            let Ok(job) = CreateJobObjectW(None, PCWSTR::null()) else { return 0 };
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
            .is_err()
            {
                let _ = CloseHandle(job);
                return 0;
            }
            job.0 as usize
        });
        (raw != 0).then_some(HANDLE(raw as *mut core::ffi::c_void))
    }

    /// 把已启动的进程加入作业。失败只记日志，不影响启动（有清扫兜底）。
    pub fn adopt(pid: u32) {
        let Some(job) = job_handle() else {
            log_line("[作业] kill-on-close 作业对象创建失败，进程树保底不可用（依赖启动清扫兜底）");
            return;
        };
        unsafe {
            match OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, false, pid) {
                Ok(process) => {
                    let assigned = AssignProcessToJobObject(job, process);
                    let _ = CloseHandle(process);
                    if assigned.is_err() {
                        log_line(&format!(
                            "[作业] pid={pid} 加入作业失败（可能已在别的作业里；启动清扫兜底）"
                        ));
                    }
                }
                Err(e) => {
                    log_line(&format!("[作业] 打开 pid={pid} 失败: {e}（启动清扫兜底）"));
                }
            }
        }
    }

    fn log_line(msg: &str) {
        if let Some(mut log) = crate::runtime::open_log_append() {
            use std::io::Write;
            let _ = writeln!(log, "{msg}");
        }
    }
}

#[cfg(not(windows))]
mod imp {
    /// Unix：已用独立进程组整组终止，无需作业对象。
    pub fn adopt(_pid: u32) {}
}

/// 把刚启动的子进程纳入进程树保底（Windows 作业对象；其它平台空操作）。
pub fn adopt(pid: u32) {
    imp::adopt(pid)
}
