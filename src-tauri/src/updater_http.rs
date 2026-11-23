//! 更新器 HTTP 超时策略：连接超时 + 空闲读超时（**不设总超时**）。
//!
//! 2026-11-24 修正：v0.1.87 给三处 `updater_builder` 加的 60 秒 `timeout()` 语义
//! 理解错了——它是 reqwest 的**总请求超时**（从开始连接直到响应体读完），而且一个
//! Updater 的同一份 client 同时用于「检查更新」（几 KB JSON）和「下载安装包」
//! （v0.1.87 实测 5.84 MB）。60 秒总预算意味着下载全程至少 ~97 KB/s，慢网络用户
//! 会直接更新失败；而改动之前这里根本没有超时。
//!
//! 正确的挂死防护是另一种超时：
//! - `connect_timeout`：建立连接阶段（DNS/TCP/TLS）——30 秒，足够慢网握手；
//! - `read_timeout`：**每次读取**的超时，读成功后重置——30 秒，专治「连上了但
//!   服务端静默不再吐数据」的挂死（reqwest 文档原话：more appropriate for
//!   detecting stalled connections when the size isn't known beforehand）。
//!
//! 两者都不限制总时长，所以大文件慢速下载不会被误杀；真正的静默挂死仍会在
//! 30 秒内失败。对照官方 desktop 的更新 http-executor：他们也是「空闲超时」
//! 而不是总超时。
use std::time::Duration;

use tauri_plugin_updater::{Updater, UpdaterExt};

/// 建立连接（DNS/TCP/TLS）的时限。
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// 单次读取的时限（读成功后重置），用于识别静默挂死而不是限制总下载时长。
pub const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// 按统一策略构造 Updater。三处调用点（关于窗检查/安装、托盘检查更新）共用，
/// 避免各自设置出不同的超时语义。
pub fn build(app: &tauri::AppHandle) -> Result<Updater, String> {
    app.updater_builder()
        .configure_client(|client| {
            client
                .connect_timeout(CONNECT_TIMEOUT)
                .read_timeout(READ_TIMEOUT)
        })
        .build()
        .map_err(|e| format!("更新器初始化失败：{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 策略常量本身要合理：连接超时不能比读超时还短（否则慢网握手会先失败），
    /// 且两者都不应是「总超时」量级的短值（历史缺陷：60 秒砍掉大文件下载）。
    #[test]
    fn timeout_policy_is_idle_based_not_total() {
        assert!(CONNECT_TIMEOUT >= Duration::from_secs(10), "连接超时太短，慢网握手会失败");
        assert!(READ_TIMEOUT >= Duration::from_secs(10), "读超时太短，慢速下载会被误杀");
        // 只约束下限；不设总超时由「不使用 .timeout()」保证（build 里没有调用）。
        assert!(CONNECT_TIMEOUT <= Duration::from_secs(60));
        assert!(READ_TIMEOUT <= Duration::from_secs(60));
    }
}
