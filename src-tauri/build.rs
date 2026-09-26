fn main() {
    // 壳静态页（ui/）在编译期经 generate_context! 嵌入二进制：ui/ 变更必须触发
    // 重编译，否则改了页面 exe 里还是旧版（2026-09-26 守护报告窗按钮三轮"改不动"
    // 事故的根因排查点）。
    println!("cargo:rerun-if-changed=../ui");
    tauri_build::build()
}
