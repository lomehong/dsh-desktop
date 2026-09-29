fn main() {
    // 壳静态页（ui/）在编译期经 generate_context! 嵌入二进制：ui/ 变更必须触发
    // 重编译，否则改了页面 exe 里还是旧版（2026-09-26 守护报告窗按钮三轮"改不动"
    // 事故的根因排查点）。
    println!("cargo:rerun-if-changed=../ui");
    // 图标资源（exe 内嵌 .ico + 托盘底图）同理：icons/ 变更必须触发 build script
    // 重跑。注意：一旦声明任何 rerun-if-changed，cargo 就不再走"包内任意文件变更
    // 即重跑"的默认行为——漏了这条会出现图标换了 exe 里仍旧图标的事故。
    println!("cargo:rerun-if-changed=icons");
    // UI 构建时间戳：注入给 whale.rs 用作覆盖层页面的缓存穿透查询参数
    // （WebView2 对 tauri.localhost 的 HTTP 缓存跨进程持久——URL 不变可能一直
    // 给旧页面，实机 2026-09-29 连续三轮改动用户全看到旧版的排查结论）。
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    println!("cargo:rustc-env=DSH_UI_BUILD_TS={ts}");
    tauri_build::build()
}
