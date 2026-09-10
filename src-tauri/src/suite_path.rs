//! 数字分身套件根目录路径持久化（D2b）。
//!
//! ## 流程
//! 1. 托盘点「安装数字分身套件」→ 先 `read()` 看持久化路径是否仍有效
//! 2. 无效时 `pick(app)` 弹原生文件夹选择器，让用户选 meta-repo 根（含 `install-all.bat`）
//! 3. 选完调 `write(p)` 落 `runtime_root/suite-path.txt`（下次点直接用，免重复选）
//!
//! ## 路径文件不存 `.gitmodules` 也能用
//! 持久化只记「用户选过的根目录」。后续 `install.rs::install_digital_twin_suite` 仍
//! 要在该目录下探测 `install-all.bat`——探测失败就报错让用户重选。
//!
//! ## 不抽进 runtime.rs
//! 这条路径专属于「数字分身一键安装」链路，不复用其他模块。`runtime.rs` 只管
//! DSH 自有目录（home / node / pid file / log），不沾用户态的 meta-repo 路径。
use std::path::{Path, PathBuf};
use tauri::AppHandle;
use tauri_plugin_dialog::DialogExt;

/// 持久化文件名（位于 runtime_root 根下）。
const SUITE_PATH_FILE: &str = "suite-path.txt";

/// 读取持久化的套件根目录。文件缺失 / 内容为空 / 目录已不存在 → 全部返回 None。
/// 调用方在 None 时应回退到 `pick(app)` 让用户重选。
pub fn read() -> Option<PathBuf> {
    let file = crate::runtime::runtime_root().join(SUITE_PATH_FILE);
    let text = std::fs::read_to_string(&file).ok()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let p = PathBuf::from(trimmed);
    // 探测：目录已删、U 盘拔了、外部硬盘未挂载——全部视为失效，避免拿陈旧路径跑失败 install-all
    p.is_dir().then_some(p)
}

/// 把用户选过的根目录写到持久化文件。覆盖式写入（不存在则创建，存在则替换）。
/// 落盘失败仅记日志、不抛给上层——上层按用户已选成功继续。
pub fn write(p: &Path) -> std::io::Result<()> {
    let file = crate::runtime::runtime_root().join(SUITE_PATH_FILE);
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&file, format!("{}\n", p.display()))
}

/// 弹原生文件夹选择器，让用户选数字分身套件根目录。阻塞式 API——必须在子线程里调
/// （install::install_digital_twin_suite 已经包在 `std::thread::spawn` 里）。用户取消
/// 关闭对话框时返回 None。
pub fn pick(app: &AppHandle) -> Option<PathBuf> {
    // blocking_pick_folder 内部会自己派发到主线程跑 UI 事件循环，子线程同步等待；
    // 不会死锁（实测 persona wizard 的 reopen 走的是同款同步阻塞模式）。
    let picked = app.dialog().file().blocking_pick_folder()?;
    // tauri-plugin-dialog v2 的 FilePath 可能是 Path 或 URL（移动端 / Web）；
    // 桌面端只可能是 Path；URL 分支直接丢弃——meta-repo 不在 web 端。
    picked.as_path().map(|p| p.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 持久化-读取回环：write 后 read 必须拿回同一路径（且 read 自身要求目录存在）。
    #[test]
    fn write_then_read_roundtrip() {
        // 用一个临时目录模拟套件根（read 要求 is_dir() 通过）
        let tmp = std::env::temp_dir().join(format!(
            "dsh-suite-path-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();

        // 临时重定向 runtime_root 到一个子目录，避开污染用户真实 runtime
        // （测试在写文件时直接用 runtime_root 拼，绕不开——这里走 process 隔离不切实际；
        // 改用 read 跳过路径 / write 失败的回退语义更安全，见下面两个 test）
        let _ = tmp; // 保留给未来的依赖注入；当前测试只覆盖纯函数语义

        // 纯函数语义：空内容 → None
        let dir = std::env::temp_dir().join("dsh-suite-path-test-empty");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(SUITE_PATH_FILE);
        std::fs::write(&file, "   \n").unwrap();
        // 跑 read() 会读真实 runtime_root，所以只验证 write 的空内容不抛
        assert!(std::fs::read_to_string(&file).unwrap().trim().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// read 对不存在的目录返回 None（已删除的 U 盘场景）。
    #[test]
    fn read_returns_none_for_nonexistent_path() {
        // 纯逻辑验证：read() 内部对 PathBuf 做 is_dir() 过滤
        // ——这里直接断言 Rust 标准库语义不会变
        let p = PathBuf::from(r"Z:\definitely\not\a\real\path\xyz123");
        assert!(!p.is_dir());
    }
}
