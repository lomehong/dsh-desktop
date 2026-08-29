//! 子进程监督：spawn（随机端口 + stdout URL 解析）、守护重启、整树击杀。
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::runtime::{self, Launch};
use crate::{status, webview, AppState};
use tauri::Manager;

/// 等待 stdout 出现 URL 行的时限（全新 DSH_HOME 首启要装 profile 依赖，给足时间）。
const URL_WAIT_SECS: u64 = 180;
/// URL 出现后等待 HTTP 就绪的时限。
const HEALTH_WAIT_SECS: u64 = 60;
/// 服务异常退出后的自动重启上限（一次会话内）。
pub const MAX_AUTO_RESTARTS: u32 = 3;

/// 一个已拉起的 dsh web 服务：子进程句柄 + 实际监听地址（随机端口）。
pub struct Running {
    pub child: Child,
    /// 服务 origin（http://127.0.0.1:<port>）：导航放行、事件流订阅、pid 登记用。
    pub base_url: String,
    /// dsh 报告的启动 URL。v0.1.2 起带一次性 token（`/?token=…`），旧版与
    /// base_url 等价——就绪探测与 webview 首航必须走它：无 token 的 GET /
    /// 在 v0.1.2+ 是 401，token 交换（303 + Set-Cookie）才是进入主界面的正门。
    pub launch_url: String,
}

/// 从一行 stdout 解析 `dsh web: <url>[ (LAN: <url>)]`，返回 (origin, 启动 URL)。
/// v0.1.2 起 URL 携带一次性 token，必须原样保留；只认 loopback 地址，
/// LAN 候选（括号内）一律忽略。解析不出 loopback 地址的行直接跳过，
/// 继续等下一行（如 `--no-open` 提示行）。
fn parse_web_url(line: &str) -> Option<(String, String)> {
    let marker = "dsh web: ";
    let idx = line.find(marker)?;
    let url = line[idx + marker.len()..].split_whitespace().next()?;
    let rest = url.strip_prefix("http://127.0.0.1:")?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    Some((format!("http://127.0.0.1:{digits}"), url.to_string()))
}

/// 进程登记（JSON）：壳 pid + dsh 子进程 pid + 实际端口。
#[derive(serde::Serialize, serde::Deserialize)]
struct PidRecord {
    shell_pid: u32,
    child_pid: u32,
    port: u16,
}

/// 服务拉起成功后登记，供下次启动识别孤儿。
fn write_pidrecord(app: &tauri::AppHandle, running: &Running) {
    let shell_pid = std::process::id();
    let port = running
        .base_url
        .rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .unwrap_or(0);
    let rec = PidRecord {
        shell_pid,
        child_pid: running.child.id() as u32,
        port,
    };
    if let Ok(json) = serde_json::to_string_pretty(&rec) {
        let path = runtime::pid_file();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, json);
    }
    let _ = app; // 预留：如需向壳上报登记结果
}

/// 启动时清理「壳被强杀后残留的孤儿 dsh 进程树」：
/// 登记存在、壳 pid 已死、子 pid 仍活且进程名符合 dsh 启动链（node/cmd）→ 整树击杀。
/// 壳 pid 仍存活说明是并行实例（单实例插件之外的兜底），不动。
pub fn cleanup_stale_orphan() {
    let Ok(text) = std::fs::read_to_string(runtime::pid_file()) else {
        return;
    };
    let _ = std::fs::remove_file(runtime::pid_file());
    let Ok(rec) = serde_json::from_str::<PidRecord>(&text) else {
        return;
    };
    if runtime::process_alive(rec.shell_pid) {
        // 另一个壳实例仍在运行（其子进程归它管）
        return;
    }
    let Some(name) = runtime::process_name(rec.child_pid) else {
        return; // 子进程已不在
    };
    let launcher_chain = ["node.exe", "cmd.exe", "node", "sh"];
    if !launcher_chain.contains(&name.as_str()) {
        return; // pid 被复用成了无关进程，不动
    }
    if let Some(mut log) = runtime::open_log_append() {
        let _ = writeln!(log, "[清理] 击杀上次强杀残留的孤儿进程树 pid={} name={}", rec.child_pid, name);
    }
    kill_tree(rec.child_pid);
}

/// 杀掉整个进程树（dsh 会派生工作线程/子进程，必须整树清理）。
pub fn kill_tree(pid: u32) {
    #[cfg(windows)]
    {
        let mut cmd = Command::new("taskkill");
        cmd.args(["/PID", &pid.to_string(), "/T", "/F"]);
        let _ = runtime::no_window(&mut cmd).status();
    }
    #[cfg(unix)]
    {
        // spawn 时启用了独立进程组（pgid == pid），负号表示整组信号
        let _ = Command::new("kill").args(["-TERM", &format!("-{pid}")]).status();
        std::thread::sleep(Duration::from_millis(1500));
        let _ = Command::new("kill").args(["-KILL", &format!("-{pid}")]).status();
    }
}

/// 拉起 `dsh web --no-open --port 0`：随机端口由 OS 分配，实际地址从 stdout 的 URL 行解析；
/// 全部输出同步 tee 到日志文件。未能在时限内解析出 URL 视为启动失败。
pub fn spawn_dsh(launch: Launch) -> Result<Running, String> {
    let log = runtime::open_log_append().ok_or_else(|| "无法写入日志文件".to_string())?;
    let log = Arc::new(Mutex::new(log));
    let mut cmd = match launch {
        Launch::Portable => {
            let node = runtime::node_exe();
            let bin = runtime::dsh_bin_js();
            if !node.exists() || !bin.exists() {
                return Err("便携运行时就绪检查失败，请重新安装运行环境。".into());
            }
            let node_dir = node.parent().map(|p| p.to_path_buf()).unwrap_or_default();
            let sep = if cfg!(windows) { ";" } else { ":" };
            let sys = std::env::var("PATH").unwrap_or_default();
            let mut c = Command::new(&node);
            c.arg(&bin)
                .args(["web", "--no-open", "--port", "0"])
                .env("PATH", format!("{}{}{}", node_dir.display(), sep, sys))
                .current_dir(node_dir);
            // 便携模式（U盘包）：DSH home 重定向到包内 Data/home，分身状态随U盘走，
            // 宿主机 ~/.dsh 零读写；npm 缓存同样留在包内，不在宿主留痕
            if let Some(home) = runtime::portable_home() {
                c.env("DSH_HOME", &home);
                c.env("npm_config_cache", home.join(".npm-cache"));
            }
            // macOS .app bundle 环境缺少 HOME / NODE_PATH，Node.js ESM 模块解析依赖它们
            #[cfg(not(windows))]
            {
                let home = std::env::var("HOME").unwrap_or_default();
                let nm = runtime::runtime_root().join("node").join("lib").join("node_modules");
                let dsh_nm = nm.join("@deepseek-ai").join("dsh").join("node_modules");
                c.env("HOME", home)
                    .env("NODE_PATH", format!("{}:{}", nm.display(), dsh_nm.display()));
            }
            c
        }
        Launch::System => {
            // 系统 node + 全局 dsh 命令
            #[cfg(windows)]
            {
                // 经 cmd 调用以解析 PATH 上的 dsh.cmd
                let mut c = Command::new("cmd.exe");
                c.args(["/C", "dsh", "web", "--no-open", "--port", "0"]);
                c
            }
            #[cfg(not(windows))]
            {
                let mut c = Command::new("dsh");
                c.args(["web", "--no-open", "--port", "0"]);
                c
            }
        }
    };
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // macOS/Linux：独立进程组，整组终止不波及父进程（App）
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = runtime::no_window(&mut cmd).spawn().map_err(|e| format!("启动 dsh web 失败: {e}"))?;

    let stdout = child.stdout.take().ok_or("无法捕获 dsh 输出")?;
    let stderr = child.stderr.take().ok_or("无法捕获 dsh 错误输出")?;

    let (tx, rx) = std::sync::mpsc::channel::<(String, String)>();
    let log_out = Arc::clone(&log);
    let tx_out = tx.clone();
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            if let Ok(mut f) = log_out.lock() {
                let _ = writeln!(f, "[out] {line}");
            }
            if let Some(url) = parse_web_url(&line) {
                let _ = tx_out.send(url);
            }
        }
    });
    let log_err = Arc::clone(&log);
    std::thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            if let Ok(mut f) = log_err.lock() {
                let _ = writeln!(f, "[err] {line}");
            }
        }
    });

    let deadline = std::time::Instant::now() + Duration::from_secs(URL_WAIT_SECS);
    let (base_url, launch_url) = loop {
        if std::time::Instant::now() > deadline {
            kill_tree(child.id() as u32);
            let _ = child.wait();
            return Err(format!(
                "服务在 {URL_WAIT_SECS} 秒内未报告监听地址。请查看日志: {}",
                runtime::log_file().display()
            ));
        }
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(url) => break url,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                // 输出流先于 URL 结束：进程已退出
                let _ = child.wait();
                return Err(format!(
                    "服务提前退出，未报告监听地址。请查看日志: {}",
                    runtime::log_file().display()
                ));
            }
        }
    };
    Ok(Running { child, base_url, launch_url })
}

/// 读取已记录的启动方式；无记录时重新探测。
fn resolve_launch(app: &tauri::AppHandle) -> Result<Launch, String> {
    let state: tauri::State<AppState> = app.state();
    if let Some(l) = *state.launch.lock().unwrap() {
        return Ok(l);
    }
    let launch = runtime::bootstrap_runtime()?;
    *state.launch.lock().unwrap() = Some(launch);
    Ok(launch)
}

/// 完整启动序列：自举 → spawn → URL 解析 → HTTP 就绪 → 放行导航 → 进 Harness UI。
/// 手动重启与守护自动重启共用；`restarting` 标志防止并发双拉。
pub fn start_service(app: &tauri::AppHandle) -> Result<(), String> {
    let state: tauri::State<AppState> = app.state();
    {
        let mut r = state.restarting.lock().unwrap();
        if *r {
            return Ok(());
        }
        *r = true;
    }
    let result = (|| -> Result<(), String> {
        let launch = resolve_launch(app)?;
        status::set(app, "正在启动 DSH 服务…");
        let running = spawn_dsh(launch)?;
        *state.origin.lock().unwrap() = Some(running.base_url.clone());
        write_pidrecord(app, &running);
        *state.child.lock().unwrap() = Some(running.child);
        status::set(app, "等待服务就绪（首次启动约需 10~60 秒）…");
        if !crate::readiness::wait_http_ok(&running.launch_url, Duration::from_secs(HEALTH_WAIT_SECS)) {
            return Err(format!(
                "服务未就绪。请查看日志: {}",
                runtime::log_file().display()
            ));
        }
        // 就绪后订阅事件流：回合完成/审批请求 → 原生通知与任务栏闪烁
        // （launch_url 供 ≥0.1.2 的 token→cookie 交换；≤0.1.1 用不到也无害）
        crate::events::spawn(app, &running.base_url, &running.launch_url);
        webview::navigate_to_harness(app, &running.launch_url);
        Ok(())
    })();
    *state.restarting.lock().unwrap() = false;
    result
}

/// 收掉当前子进程（若有）：整树击杀并 wait 回收，child 句柄清空（守护线程随之失活）。
/// 锁只护句柄摘取（guard 随 let 语句即释放）：击杀/回收可能耗时秒级，不占着 child 锁挡住守护线程。
pub fn stop_child(app: &tauri::AppHandle) {
    let state: tauri::State<AppState> = app.state();
    let child = state.child.lock().unwrap().take();
    if let Some(mut child) = child {
        kill_tree(child.id() as u32);
        let _ = child.wait();
    }
}

/// 重启服务（托盘菜单 / 守护触发）：杀整树 → 回加载页 → 重新走启动序列（端口会变化，导航锁随之更新）。
pub fn restart_service(app: &tauri::AppHandle) {
    let state: tauri::State<AppState> = app.state();
    stop_child(app);
    // 撤销旧 origin 放行，回到加载页
    *state.origin.lock().unwrap() = None;
    status::set(app, "正在重启服务…");
    webview::navigate_to_loader(app);
    if let Err(e) = start_service(app) {
        status::fail(app, &e);
    }
}

/// 远程连接序列：凭据探活 → origin 入放行表 → 事件流(带凭证) → 导航（经 pair?token= 种 cookie）。
/// 与 start_service 共用同一把 `restarting` 互斥锁，防本地/远程并发双拉；远程模式不落
/// child 句柄（子进程归远端管），守护线程因 child=None 自然失活，不会误拉本地服务。
pub fn connect_remote_flow(app: &tauri::AppHandle) -> Result<(), String> {
    let state: tauri::State<AppState> = app.state();
    {
        let mut r = state.restarting.lock().unwrap();
        if *r {
            return Ok(());
        }
        *r = true;
    }
    let result = (|| -> Result<(), String> {
        let cfg = crate::remote::load().ok_or("尚未配对远程实例，请先输入地址与配对码")?;
        status::set(app, &format!("连接远程实例 {}…", cfg.address));
        // 探活带网关凭证头：过不了网关 401 这关就不算就绪（凭证失效早暴露）
        if !crate::readiness::wait_http_ok_hdr(
            &format!("{}/", cfg.origin),
            Some(&cfg.token),
            Duration::from_secs(HEALTH_WAIT_SECS),
        ) {
            return Err(format!("无法连接远程实例 {}（超时或凭证失效）", cfg.address));
        }
        // 凭证有效才放行导航与事件流：origin 换成远程网关，导航守卫同步收紧
        *state.origin.lock().unwrap() = Some(cfg.origin.clone());
        crate::events::spawn_remote(app, &cfg.origin, &cfg.token);
        // 经 pair?token= 导航：网关 303 + Set-Cookie 种下 webview 凭证后落到 /
        // （同 dsh 一次性 token 心智；必须原生导航——跨站发起的页面导航不带
        // SameSite=Strict 的 cookie，理由同 navigate_to_harness 注释）
        webview::navigate_to_harness(
            app,
            &format!("{}/__remote/pair?token={}", cfg.origin, cfg.token),
        );
        Ok(())
    })();
    *state.restarting.lock().unwrap() = false;
    result
}

/// 切回本地模式（加载页 switch_to_local 命令与托盘「断开远程，回到本地」共用）：
/// 收尾本地 child → 模式翻转并落盘 → 托盘菜单重建 → 撤 origin 回加载页 → 重新走本地启动序列。
pub fn switch_to_local_flow(app: &tauri::AppHandle) {
    let state: tauri::State<AppState> = app.state();
    // 防御性收尾：正常远程模式 child 恒为 None，但升级运行时等路径可能在远程模式下
    // 留下本地 child——不清掉会让 start_service 双拉本地实例
    stop_child(app);
    *state.origin.lock().unwrap() = None;
    *state.mode.lock().unwrap() = "local";
    crate::remote::save_mode("local");
    // 模式已翻转：托盘菜单换成本地菜单（set_menu 内部自派发主线程，任意线程可调）
    crate::tray::rebuild(app);
    // 先落一帧本地模式状态再回加载页：首帧轮询不再重放上一帧的远程错误态
    status::set(app, "正在切换到本地模式…");
    webview::navigate_to_loader(app);
    let handle = app.clone();
    std::thread::spawn(move || {
        if let Err(err) = start_service(&handle) {
            status::fail(&handle, &err);
        }
    });
}

/// 托盘「重启服务」/ 重启命令的统一入口：按当前模式分派。
/// 本地走 restart_service（杀树重启，行为不变）；远程先收掉残留 child（如升级运行时
/// 等路径在远程模式下拉起过的本地服务）、撤 origin 回加载页，再走 connect_remote_flow。
pub fn restart_by_mode(app: &tauri::AppHandle) {
    let state: tauri::State<AppState> = app.state();
    if *state.mode.lock().unwrap() != "remote" {
        restart_service(app);
        return;
    }
    // 远程重连前先收掉残留 child（如升级运行时等路径在远程模式下拉起过的本地服务）
    stop_child(app);
    *state.origin.lock().unwrap() = None;
    webview::navigate_to_loader(app);
    status::set(app, "正在重连远程实例…");
    if let Err(e) = connect_remote_flow(app) {
        status::fail(app, &e);
    }
}

/// 服务守护：意外退出时按次数上限自动重启。
pub fn watch_child(app: &tauri::AppHandle) {
    loop {
        std::thread::sleep(Duration::from_secs(3));
        let state: tauri::State<AppState> = app.state();
        if *state.restarting.lock().unwrap() {
            continue;
        }
        let exited = {
            let mut guard = state.child.lock().unwrap();
            match guard.as_mut() {
                None => false, // 尚未启动或已被接管/退出
                Some(child) => matches!(child.try_wait(), Ok(Some(_))),
            }
        };
        if !exited {
            continue;
        }
        // 子进程意外退出：清空句柄，按次数上限自动重启
        state.child.lock().unwrap().take();
        let mut restarts = state.restarts.lock().unwrap();
        if *restarts >= MAX_AUTO_RESTARTS {
            drop(restarts);
            status::fail(app, "服务多次异常退出，已停止自动重启，请查看日志。");
            return;
        }
        *restarts += 1;
        let n = *restarts;
        drop(restarts);
        status::set(app, &format!("服务异常退出，正在自动重启（第 {n} 次）…"));
        if let Err(e) = start_service(app) {
            status::fail(app, &e);
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_v0_1_2_token_url_keeping_token_and_origin() {
        let line = "dsh web: http://127.0.0.1:44182/?token=AbC-123_xYz0 (LAN: http://192.168.1.5:44182/?token=AbC-123_xYz0)";
        let (base, launch) = parse_web_url(line).unwrap();
        assert_eq!(base, "http://127.0.0.1:44182");
        assert_eq!(launch, "http://127.0.0.1:44182/?token=AbC-123_xYz0");
    }

    #[test]
    fn parses_plain_url_without_token() {
        let (base, launch) = parse_web_url("dsh web: http://127.0.0.1:8080/").unwrap();
        assert_eq!(base, "http://127.0.0.1:8080");
        assert_eq!(launch, "http://127.0.0.1:8080/");
    }

    #[test]
    fn skips_non_loopback_and_hint_lines() {
        assert!(parse_web_url("dsh web: http://192.168.1.5:8080/").is_none());
        assert!(parse_web_url("dsh web: opening the default browser; pass --no-open to disable").is_none());
        assert!(parse_web_url("listening on port 8080").is_none());
    }
}
