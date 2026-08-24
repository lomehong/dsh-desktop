//! 子进程监督：spawn（随机端口 + stdout URL 解析）、守护重启、整树击杀。
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::runtime::{self, Launch};
use crate::{status, webview, AppState};

/// 等待 stdout 出现 URL 行的时限（全新 DSH_HOME 首启要装 profile 依赖，给足时间）。
const URL_WAIT_SECS: u64 = 180;
/// URL 出现后等待 HTTP 就绪的时限。
const HEALTH_WAIT_SECS: u64 = 60;
/// 服务异常退出后的自动重启上限（一次会话内）。
pub const MAX_AUTO_RESTARTS: u32 = 3;

/// 一个已拉起的 dsh web 服务：子进程句柄 + 实际监听地址（随机端口）。
pub struct Running {
    pub child: Child,
    pub base_url: String,
}

/// 从一行 stdout 中解析 `dsh web: http://127.0.0.1:<port>`。
fn parse_web_url(line: &str) -> Option<String> {
    let marker = "dsh web: http://127.0.0.1:";
    let idx = line.find(marker)?;
    let rest = &line[idx + marker.len()..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    Some(format!("http://127.0.0.1:{digits}"))
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

    let (tx, rx) = std::sync::mpsc::channel::<String>();
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
    let base_url = loop {
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
    Ok(Running { child, base_url })
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
        *state.child.lock().unwrap() = Some(running.child);
        status::set(app, "等待服务就绪（首次启动约需 10~60 秒）…");
        if !crate::readiness::wait_http_ok(&running.base_url, Duration::from_secs(HEALTH_WAIT_SECS)) {
            return Err(format!(
                "服务未就绪。请查看日志: {}",
                runtime::log_file().display()
            ));
        }
        webview::navigate_to_harness(app, &running.base_url);
        Ok(())
    })();
    *state.restarting.lock().unwrap() = false;
    result
}

/// 重启服务（托盘菜单 / 守护触发）：杀整树 → 回加载页 → 重新走启动序列（端口会变化，导航锁随之更新）。
pub fn restart_service(app: &tauri::AppHandle) {
    let state: tauri::State<AppState> = app.state();
    if let Some(mut child) = state.child.lock().unwrap().take() {
        kill_tree(child.id() as u32);
        let _ = child.wait();
    }
    // 撤销旧 origin 放行，回到加载页
    *state.origin.lock().unwrap() = None;
    status::set(app, "正在重启服务…");
    webview::navigate_to_loader(app);
    if let Err(e) = start_service(app) {
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
