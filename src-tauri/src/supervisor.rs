//! 子进程监督：spawn（随机端口 + stdout URL 解析）、守护重启、整树击杀。
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::runtime::{self, Launch};
use crate::{install, status, webview, AppState};
use tauri::Manager;

/// 等待 stdout 出现 URL 行的时限（全新 DSH_HOME 首启要装 profile 依赖，给足时间）。
const URL_WAIT_SECS: u64 = 180;
/// URL 出现后等待 HTTP 就绪的时限。
const HEALTH_WAIT_SECS: u64 = 60;
/// 服务异常退出后的自动重启上限（一次会话内）。
pub const MAX_AUTO_RESTARTS: u32 = 3;

/// 流程闸锁（0.1.28 起排队语义）：一条流程（收尾/翻转/探活/启动）从头到尾独占
/// 一把闸锁；其他流程 acquire 时**排队等待**而非静默放弃——旧版「见置位即整段
/// 静默返回」会丢并发请求，用户视角是「点了没反应」（守护线程在流程在途时检测到
/// 子进程退出、随后 start_service 静默 no-op，服务甚至不会重启）。
///
/// 不变式（延续 0.1.17）：一条流程只取放闸锁各一次，中途不释放——否则托盘断开等
/// 并发流程可在流程中途插入 stop/mode 翻转，造成 mode/origin/UI 短暂分叉。锁内
/// 主体一律以 `_locked` 后缀命名，禁止再取本锁。模式读取也移入锁内（restart_by_mode：
/// 排队等到的时点模式可能已被前一流程翻转，锁外读是脏读）。
///
/// 死锁面评估：acquire 阻塞时不持任何其他锁；持闸锁期间调用的 tray::rebuild/status/
/// webview 均在主线程派发且主线程从不取本锁（所有触达本锁的命令/托盘路径都先另起
/// 线程）；watch_child 每 3s tick 见置位即跳过（探测语义不变，守护在流程期间暂停）；
/// 其余 child/proxy 锁的获取顺序恒为 restarting → child/proxy，无环。流程主体内
/// 全部等待有界（URL 180s + HTTP 60s），排队者不会无限饥饿。
pub struct FlowGate {
    held: Mutex<bool>,
    idle: Condvar,
}

impl FlowGate {
    pub const fn new() -> Self {
        Self {
            held: Mutex::new(false),
            idle: Condvar::new(),
        }
    }

    /// 阻塞直到取得闸锁。调用方都在专用线程上（命令层/托盘/守护各自 spawn），
    /// 排队等待不卡主线程；在途流程结束后本流程才整段执行。
    pub fn acquire(&self) {
        let mut held = self.held.lock().unwrap();
        while *held {
            held = self.idle.wait(held).unwrap();
        }
        *held = true;
    }

    /// 释放闸锁并唤醒全部排队者（被唤醒者竞争 Mutex，天然串行取锁）。
    pub fn release(&self) {
        *self.held.lock().unwrap() = false;
        self.idle.notify_all();
    }

    /// 探测（不排队）：仅守护线程 tick 用——流程在途期间跳过检查即可，
    /// 检测到退出后的重启意图走 start_service 的排队 acquire，不会丢。
    pub fn is_held(&self) -> bool {
        *self.held.lock().unwrap()
    }
}

/// 一个已拉起的 dsh web 服务：子进程句柄 + 实际监听地址（随机端口）。
pub struct Running {
    pub child: Child,
    /// 服务 origin（http://127.0.0.1:<port>）：导航放行、事件流订阅、pid 登记用。
    pub base_url: String,
    /// dsh 报告的启动 URL。v0.1.2 起带一次性 token（`/?token=…`），旧版与
    /// base_url 等价——就绪探测与 webview 首航必须走它：无 token 的 GET /
    /// 在 v0.1.2+ 是 401，token 交换（303 + Set-Cookie）才是进入主界面的正门。
    pub launch_url: String,
    /// 启动期输出尾环（spawn_dsh 读线程持续写入，读端只读）：URL 解析成功后
    /// 进程仍可能卡在 HTTP 就绪前（如 profile 依赖安装失败循环报错），
    /// start_service_locked 的「服务未就绪」错误据此附带最近输出。
    pub startup_tail: Arc<Mutex<VecDeque<String>>>,
}

/// URL 等待循环从读线程收到的信号：找到了监听地址，或读到了致命崩溃特征。
#[derive(Debug, PartialEq)]
enum SpawnSignal {
    Ready(String, String),
    /// stderr 命中崩溃特征（Node.js 崩溃横幅）；附错误首行供加载页直接展示。
    Crashed(String),
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

/// 就绪地址一致性（纯函数便于单测）：同一世代内首次解析出的 (origin, url) 是唯一
/// 事实；后续行再解析出不同地址视为冲突——真服务不会中途改端口重报就绪行，冲突
/// 输出不可信（对齐 studio host-supervisor 的 conflicting readiness URL 防御）。
/// 首次命中返回 Ok(Some)；重复同一地址返回 Ok(None)；冲突返回 Err 并保留首次。
fn accept_ready(
    seen: &mut Option<(String, String)>,
    line: &str,
) -> Result<Option<(String, String)>, String> {
    let Some(parsed) = parse_web_url(line) else {
        return Ok(None);
    };
    match seen {
        None => {
            *seen = Some(parsed.clone());
            Ok(Some(parsed))
        }
        Some(prev) if *prev == parsed => Ok(None),
        Some(_) => Err(format!(
            "服务报告了冲突的监听地址 {}（以首次 {} 为准）",
            parsed.0,
            seen.as_ref().unwrap().0
        )),
    }
}

/// 启动期输出尾环容量（行数）：失败时随错误信息附上，加载页错误态直接可读，
/// 不必翻几百行日志。
const TAIL_LINES: usize = 40;
/// 尾部摘要单条上限（字符）：防极端长行撑爆加载页错误态。
const TAIL_MAX_CHARS: usize = 2048;

/// 启动期输出尾部环形缓冲（纯函数便于单测）：超容量丢最旧行。
fn push_tail(tail: &mut VecDeque<String>, line: &str) {
    if tail.len() == TAIL_LINES {
        tail.pop_front();
    }
    tail.push_back(line.to_string());
}

/// 尾环格式化为可读摘要（纯函数）：按字符截断（字节切片会切进多字节 UTF-8 中间），
/// 空尾环返回空串，由调用方决定是否拼接。
fn tail_text(tail: &VecDeque<String>) -> String {
    let joined = tail
        .iter()
        .map(|l| l.trim_end())
        .collect::<Vec<_>>()
        .join("\n");
    let total = joined.chars().count();
    if total > TAIL_MAX_CHARS {
        format!("…{}", joined.chars().skip(total - TAIL_MAX_CHARS).collect::<String>())
    } else {
        joined
    }
}

/// 失败信息拼尾部摘要（纯函数）：空摘要原样返回，不产生空段落。
fn with_tail(msg: &str, tail: &str) -> String {
    if tail.is_empty() {
        return msg.to_string();
    }
    format!("{msg}\n最近输出：\n{tail}")
}

/// 识别 Node.js 致命错误横幅（纯函数便于单测）。崩溃 stderr 以
/// `Node.js v<semver>` 整行收尾（行首 `[err] ` 前缀容忍）；错误信息中途出现
/// 「Node.js」字样不算。摘要把手在 stderr 读线程：横幅前最近一条 `Error:` 行。
fn crash_banner(line: &str) -> Option<String> {
    let t = line.trim_start_matches("[err] ").trim();
    if t.starts_with("Node.js v") && t["Node.js v".len()..].starts_with(|c: char| c.is_ascii_digit()) {
        Some(t.to_string())
    } else {
        None
    }
}

/// 崩溃/退出错误串是否为「profile bundle 无法解析」特征（dsh loadProfile 对失联
/// bundle 硬失败，真实故障：迁移后 dsh-better-sidebar 启动即崩）。匹配整个错误串
/// 而非单行：失败路径会把启动尾环（含原始 Error 行）拼进错误信息。
fn is_profile_bundle_error(msg: &str) -> bool {
    msg.contains("cannot resolve profile bundle")
}

/// 从 bundle 解析错误串提取受影响 profile 名（纯函数便于单测）。优先取错误自带
/// 修复提示 `dsh plugin --profile <name> install` 中的名字，退回 `profiles<sep><name>`
/// 路径段（Windows `\`、Unix `/` 都认）；定位失败返回 None，调用方兜底全量补装。
fn profiles_from_bundle_error(msg: &str) -> Option<Vec<String>> {
    let token_after = |prefix: &str, from: usize| -> Option<String> {
        let rest = msg.get(from + prefix.len()..)?;
        let token: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        (!token.is_empty()).then_some(token)
    };
    // 1) 修复提示：`--profile <name>`（名字在提示里裸出现，无引号）
    let mut search = 0;
    while let Some(rel) = msg[search..].find("--profile ") {
        let abs = search + rel;
        if let Some(name) = token_after("--profile ", abs) {
            return Some(vec![name]);
        }
        search = abs + "--profile ".len();
    }
    // 2) 路径段：profiles\<name> / profiles/<name>
    let mut search = 0;
    while let Some(rel) = msg[search..].find("profiles") {
        let abs = search + rel;
        let rest = &msg[abs + "profiles".len()..];
        let mut chars = rest.chars();
        if matches!(chars.next(), Some('\\') | Some('/')) {
            let token: String = chars
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                .collect();
            if !token.is_empty() {
                return Some(vec![token]);
            }
        }
        search = abs + "profiles".len();
    }
    None
}

/// 进程登记（JSON）：壳 pid + dsh 子进程 pid + 实际端口。
#[derive(serde::Serialize, serde::Deserialize)]
struct PidRecord {
    shell_pid: u32,
    child_pid: u32,
    port: u16,
}

/// pid 记录 JSON（纯函数便于单测）：port 未知时以 0 占位——记录仅用于孤儿识别，
/// cleanup_stale_orphan 只读 pid 字段，端口缺失无碍。
fn pid_record_json(child_pid: u32, port: u16) -> Option<String> {
    let rec = PidRecord {
        shell_pid: std::process::id(),
        child_pid,
        port,
    };
    serde_json::to_string_pretty(&rec).ok()
}

/// 进程登记落盘，供下次启动识别孤儿。冷启动路径会调用两次：
/// spawn 成功即刻（端口 0 占位）与 URL 解析成功后（真实端口覆盖）。
fn write_pid_record(child_pid: u32, port: u16) {
    if let Some(json) = pid_record_json(child_pid, port) {
        let path = runtime::pid_file();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, json);
    }
}

/// 从服务 origin 取端口（纯函数；解析不出按 0 占位，与 pid 记录占位语义一致）。
fn base_url_port(base_url: &str) -> u16 {
    base_url
        .rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .unwrap_or(0)
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
/// 0.1.17 冷启动孤儿缺口修复：spawn 成功即刻 ①写 pid 记录（端口未知 0 占位）
/// ②child 挂入 AppState.child——此前 URL 等待窗口（最长 URL_WAIT_SECS）内两者皆空，
/// 壳退出（RunEvent::Exit）或被强杀会让 dsh 进程树孤儿化，且下次启动无据可查。
/// 需要 app 以访问 AppState；失败路径统一「从 state 槽位取句柄再收尸」：
/// 槽位空 = 已被退出路径收走，天然防双杀（世代内 watch_child 因 restarting 置位
/// 不会触碰槽位，见 start_service 的闸锁注释）。
pub fn spawn_dsh(app: &tauri::AppHandle, launch: Launch) -> Result<Running, String> {
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
            c.arg(&bin).arg("web");
            // 容错：镜像旧包可能缺 --no-open，按实际能力决定是否传，避免「unknown option」崩溃
            if install::web_supports_no_open() {
                c.arg("--no-open");
            }
            c.args(["--port", "0"])
                .env("PATH", format!("{}{}{}", node_dir.display(), sep, sys))
                .current_dir(node_dir);
            // dsh-desktop 专属 DSH home（便携=包内 home，安装版=数据目录 home）：
            // 与系统 dsh/persona 的 ~/.dsh 隔离，杜绝多版本交叉污染 profile 插件树。
            let home = runtime::app_home();
            c.env("DSH_HOME", &home);
            c.env("npm_config_cache", home.join(".npm-cache"));
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
            // 系统 node + 全局 dsh 包：直接 node + bin.js，避免 cmd.exe /C dsh.cmd
            // 启动期弹窗闪烁（v0.1.28 修复）。node / bin.js 路径由 runtime 探测并缓存。
            let node = runtime::find_system_node().ok_or_else(|| {
                "未在 PATH 上找到 node，请先安装 Node.js 后重试".to_string()
            })?;
            let bin = runtime::find_system_dsh_bin().ok_or_else(|| {
                "未找到 @deepseek-ai/dsh 全局安装，请执行 npm install -g @deepseek-ai/dsh".to_string()
            })?;
            let mut c = Command::new(node);
            c.arg(bin).arg("web");
            // 容错：镜像旧包可能缺 --no-open，按实际能力决定是否传，避免「unknown option」崩溃
            if install::web_supports_no_open() {
                c.arg("--no-open");
            }
            c.args(["--port", "0"]);
            c
        }
    };
    // 多 Agent 设备消毒：opencode 安装器会把 opencode 的御符 agent token 写进
    // 用户级环境变量 YUYI_TOKEN（HKCU\Environment）。凭证服务（dsh-credentials-local）
    // 的解析层序里「继承的进程环境」优先级最高、压过 $DSH_HOME/.credentials.yaml，
    // dsh 子进程一旦继承就会被 dsh-yuyi 等插件借 opencode 身份连 hub（hub 侧身份
    // 错配、吊销联动失效）。dsh 适配器 token 的正确来源是 dsh 凭证库（设置 UI 录入）
    // 或 ~/.yuyi/dsh-token（Yuyi 安装器 dsh 分支写入），绝不继承用户级 YUYI_TOKEN。
    // YUYI_HUB / YUYI_DEVICE / YUYI_YUFU_URL 是设备级公共配置（安装器语义），保留继承。
    cmd.env_remove("YUYI_TOKEN");
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

    // 早期登记（时机是本修复的关键：先于可能长达 URL_WAIT_SECS 的 URL 等待）。
    // 此后直到成功返回，child 句柄的唯一事实来源是 state 槽位。
    write_pid_record(child.id() as u32, 0);
    let state: tauri::State<AppState> = app.state();
    *state.child.lock().unwrap() = Some(child);

    let (tx, rx) = std::sync::mpsc::channel::<SpawnSignal>();
    // 启动期输出尾环：stdout/stderr 双线程写入，失败路径读取附进错误信息（诊断留证）
    let tail: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(VecDeque::new()));
    let log_out = Arc::clone(&log);
    let tx_out = tx.clone();
    // 原始 tx 立即丢弃：发送端只剩两个读线程各自的 clone——stdout/stderr 双双 EOF
    // （进程退出）时通道断开，下方 recv_timeout 的 Disconnected 分支才可达。
    // 否则崩溃场景永远白等满 URL_WAIT_SECS（0.1.18 真实故障：node 崩溃后壳仍报
    // 「180 秒未报告监听地址」而非「服务提前退出」）。
    let tx_err = tx.clone();
    drop(tx);
    let tail_out = Arc::clone(&tail);
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        // 同世代就绪地址唯一事实：首次解析结果定格，冲突行忽略并留证日志
        let mut seen: Option<(String, String)> = None;
        for line in reader.lines() {
            let Ok(line) = line else { break };
            if let Ok(mut f) = log_out.lock() {
                let _ = writeln!(f, "[out] {line}");
            }
            if let Ok(mut t) = tail_out.lock() {
                push_tail(&mut t, &line);
            }
            match accept_ready(&mut seen, &line) {
                Ok(Some((origin, url))) => {
                    let _ = tx_out.send(SpawnSignal::Ready(origin, url));
                }
                Ok(None) => {}
                Err(conflict) => {
                    if let Ok(mut f) = log_out.lock() {
                        let _ = writeln!(f, "[warn] {conflict}");
                    }
                }
            }
        }
    });
    // stderr 读线程：整行 tee 进日志之外，识别 Node.js 崩溃横幅（fatal stack 的
    // 收尾行），并把横幅前最近一条 `Error:` 行记作摘要随信号发出——加载页错误
    // 态直接可读，无须用户翻几百行日志（典型：插件 API 不匹配导致启动即崩）。
    let log_err = Arc::clone(&log);
    let tail_err = Arc::clone(&tail);
    std::thread::spawn(move || {
        let reader = BufReader::new(stderr);
        let mut last_error_line: Option<String> = None;
        for line in reader.lines() {
            let Ok(line) = line else { break };
            if let Ok(mut f) = log_err.lock() {
                let _ = writeln!(f, "[err] {line}");
            }
            if let Ok(mut t) = tail_err.lock() {
                push_tail(&mut t, &line);
            }
            let stripped = line.trim_start_matches("[err] ").trim();
            if stripped.starts_with("Error:") || stripped.starts_with("AggregateError:") {
                last_error_line = Some(stripped.to_string());
            }
            if let Some(banner) = crash_banner(&line) {
                let _ = tx_err.send(SpawnSignal::Crashed(match &last_error_line {
                    Some(e) => format!("{e}（{banner}）"),
                    None => banner,
                }));
                break; // 崩溃已确认：等待循环随即收尸，读线程无须继续
            }
        }
        drop(tx_err); // stderr EOF：进程退出，解除 URL 等待（崩溃快速失败路径之一）
    });

    let deadline = std::time::Instant::now() + Duration::from_secs(URL_WAIT_SECS);
    let mut crash_detail: Option<String> = None;
    let (base_url, launch_url) = loop {
        if std::time::Instant::now() > deadline {
            // 经槽位取句柄再整树击杀：槽位空 = 退出流程（RunEvent::Exit）已收尸，防双杀
            if let Some(mut child) = state.child.lock().unwrap().take() {
                kill_tree(child.id() as u32);
                let _ = child.wait();
            }
            let tail = tail.lock().map(|t| tail_text(&t)).unwrap_or_default();
            return Err(with_tail(
                &format!(
                    "服务在 {URL_WAIT_SECS} 秒内未报告监听地址。请查看日志: {}",
                    runtime::log_file().display()
                ),
                &tail,
            ));
        }
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(SpawnSignal::Ready(origin, url)) => break (origin, url),
            Ok(SpawnSignal::Crashed(detail)) => {
                crash_detail = Some(detail);
                // 不立即失败：给 URL 一线机会（横幅偶尔与就绪日志交错时避免误报，
                // e.g. 子线程崩了主进程仍在）。下一轮 recv_timeout 继续等 URL。
                continue;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // 已见崩溃横幅且再无 URL 到来：按崩溃快速失败，附可读摘要
                if let Some(detail) = crash_detail.take() {
                    if let Some(mut child) = state.child.lock().unwrap().take() {
                        kill_tree(child.id() as u32);
                        let _ = child.wait();
                    }
                    return Err(format!("dsh 启动崩溃：{detail}。完整日志: {}", runtime::log_file().display()));
                }
                continue;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                // 输出流先于 URL 结束：进程已退出（同样经槽位取回再 wait 回收）
                if let Some(mut child) = state.child.lock().unwrap().take() {
                    let _ = child.wait();
                }
                let hint = crash_detail
                    .map(|d| format!("（{d}）"))
                    .unwrap_or_default();
                let tail = tail.lock().map(|t| tail_text(&t)).unwrap_or_default();
                return Err(with_tail(
                    &format!(
                        "服务提前退出，未报告监听地址{hint}。请查看日志: {}",
                        runtime::log_file().display()
                    ),
                    &tail,
                ));
            }
        }
    };
    // 成功：把句柄从槽位取回装进 Running（start_service 随即重新登记）。
    // 槽位空只可能是壳退出流程已收走句柄（watch_child 在流程在途期间不碰槽位），
    // 此时进程树归退出路径管，本次启动按失败收场。
    let Some(child) = state.child.lock().unwrap().take() else {
        return Err("服务启动被壳退出流程中断".into());
    };
    Ok(Running { child, base_url, launch_url, startup_tail: tail })
}

/// 读取已记录的启动方式；无记录时重新探测。
/// 自愈机制：当 bootstrap_runtime 返回 NEED_AUTO_REPAIR 信号（系统 Node 版本不兼容），
/// 自动安装便携运行时并修复插件符号链接，然后以 Portable 模式启动。
fn resolve_launch(app: &tauri::AppHandle) -> Result<Launch, String> {
    let state: tauri::State<AppState> = app.state();
    if let Some(l) = *state.launch.lock().unwrap() {
        return Ok(l);
    }
    match runtime::bootstrap_runtime() {
        Ok(launch) => {
            *state.launch.lock().unwrap() = Some(launch);
            Ok(launch)
        }
        Err(e) if e.starts_with(runtime::NEED_AUTO_REPAIR) => {
            // 自愈流程：自动安装便携运行时 + 修复插件符号链接
            status::set(app, "检测到运行时不兼容，正在自动修复…");
            if let Some(mut log) = runtime::open_log_append() {
                use std::io::Write;
                let _ = writeln!(log, "[自愈] 触发原因: {e}");
            }
            install::ensure_runtime_locked(app)?;
            *state.launch.lock().unwrap() = Some(Launch::Portable);
            Ok(Launch::Portable)
        }
        Err(e) => Err(e),
    }
}

/// `restarting` 闸锁已升级为排队语义的 FlowGate（见 FlowGate 文档）：acquire 阻塞
/// 排队，流程原子性不变式与死锁面评估移至该处。

/// 完整启动序列（锁内主体，须持 `restarting` 闸锁调用）：自举 → spawn → URL 解析 →
/// HTTP 就绪 → 放行导航 → 进 Harness UI。
fn start_service_locked(app: &tauri::AppHandle) -> Result<(), String> {
    let state: tauri::State<AppState> = app.state();
    let launch = resolve_launch(app)?;
    // 核心版本预检（仅便携运行时；System 回退由用户自管）：运行时核心若为
    // 超出壳适配线（DSH_MAX_ADAPTED）的版本（如上游已发布 0.1.3+ 但壳尚未放行），
    // 插件 API 代际可能不匹配——与其让服务崩溃后报「180 秒未报告监听地址」，
    // 不如启动前给出可操作文案（0.1.18 真实故障复盘）。解析失败不拦截（宽容：
    // 版本串异常时照常尝试启动，崩了有崩溃摘要兜底）。
    if launch == Launch::Portable {
        if let Some(v) = install::installed_dsh_version() {
            if let Some(triple) = install::version_triple_public(&v) {
                let (max_major, max_minor, max_patch) = install::max_adapted();
                if triple > (max_major, max_minor, max_patch) {
                    let is_pre = v.contains("-alpha") || v.contains("-rc") || v.contains("-beta");
                    let tag = if is_pre { "预发布版" } else { "新版本" };
                    return Err(format!(
                        "运行时 dsh 核心为 v{v}（{tag}），超出当前应用适配线（≤{max_major}.{max_minor}.{max_patch}）。\
                         插件可能因 API 不匹配而启动失败。请升级 dsh-desktop 应用本体以适配此版本。"
                    ));
                }
            }
        }
        // 镜像完整性自愈：已装包若缺 --no-open（npmmirror 滞后旧 tarball），切官方源重装。
        // 重装后能力即恢复；若离线重装失败，spawn 侧容错会省略 --no-open 兜底不崩。
        if !install::web_supports_no_open() {
            status::set(app, "检测到运行时包不完整，切换官方源重装 DSH…");
            install::force_reinstall_official()?;
        }
        // 核心版本变化自愈：清空 profile 插件目录强制按新核心重装，防版本错位崩溃。
        // dsh 启动 loadProfile 只解析 profile bundle 不安装（真实故障：cannot resolve
        // profile bundle "dsh-better-sidebar" 启动即崩），清空后必须主动补装，
        // 不能等 dsh 自己处理。
        let cleared = install::refresh_profile_plugins_if_core_changed();
        if !cleared.is_empty() {
            status::set(app, "正在安装 profile 插件（核心更新后重新解析扩展）…");
            // 失败不阻断启动：bundle 缺失时 spawn 崩溃会走下方响应式自愈再补一轮
            if let Err(e) = install::install_profile_plugins(&cleared, "核心更新") {
                if let Some(mut log) = runtime::open_log_append() {
                    let _ = writeln!(log, "[warn] profile 插件主动补装失败: {e}");
                }
            }
        }
    }
    status::set(app, "正在启动 DSH 服务…");
    // spawn_dsh 内部已把 child 挂入 state（冷启动孤儿修复）；此处拿回 Running 后
    // 用真实端口覆盖占位 pid 记录并重新登记句柄。
    // 响应式自愈：崩溃特征为 profile bundle 失联（profile 配置在场而插件实体缺失：
    // 外部带入的 home、上次安装被打断等）→ 补装后重试一次；仍失败则原样上报
    // （插件与核心 API 代际错位等无自动修法，错误页附带的崩溃摘要可读）。
    // 仅重试一次，配合外层守护重启上限有界。
    let running = match spawn_dsh(app, launch) {
        Ok(r) => r,
        Err(e) if is_profile_bundle_error(&e) => {
            let profiles = profiles_from_bundle_error(&e).unwrap_or_else(install::profile_names);
            status::set(app, "检测到 profile 插件缺失，正在自动修复后重启服务…");
            if let Some(mut log) = runtime::open_log_append() {
                let _ = writeln!(
                    log,
                    "[自愈] 启动崩溃为 profile bundle 失联，补装后重试: {}",
                    profiles.join(",")
                );
            }
            install::install_profile_plugins(&profiles, "启动自愈")
                .map_err(|e2| format!("{e}\n[自愈失败] {e2}"))?;
            spawn_dsh(app, launch)?
        }
        Err(e) => return Err(e),
    };
    *state.origin.lock().unwrap() = Some(running.base_url.clone());
    write_pid_record(running.child.id() as u32, base_url_port(&running.base_url));
    *state.child.lock().unwrap() = Some(running.child);
    status::set(app, "等待服务就绪（首次启动约需 10~60 秒）…");
    if !crate::readiness::wait_http_ok(&running.launch_url, Duration::from_secs(HEALTH_WAIT_SECS)) {
        // URL 已到但 HTTP 未就绪：服务多半卡在启动后半程（profile 依赖安装失败等），
        // 尾环里正是最近报错，附进错误信息免翻日志
        let tail = running.startup_tail.lock().map(|t| tail_text(&t)).unwrap_or_default();
        return Err(with_tail(
            &format!(
                "服务未就绪。请查看日志: {}",
                runtime::log_file().display()
            ),
            &tail,
        ));
    }
    // 就绪后订阅事件流：回合完成/审批请求 → 原生通知与任务栏闪烁
    // （launch_url 供 ≥0.1.2 的 token→cookie 交换；≤0.1.1 用不到也无害）
    crate::events::spawn(app, &running.base_url, &running.launch_url);
    webview::navigate_to_harness(app, &running.launch_url);
    Ok(())
}

/// 完整启动序列：自举 → spawn → URL 解析 → HTTP 就绪 → 放行导航 → 进 Harness UI。
/// 手动重启与守护自动重启共用；整个流程持 `restarting` 闸锁，在途时本调用排队等待
/// （FlowGate 排队语义）——守护线程检测到退出后的重启意图不再被静默丢弃。
pub fn start_service(app: &tauri::AppHandle) -> Result<(), String> {
    let state: tauri::State<AppState> = app.state();
    state.restarting.acquire();
    let result = start_service_locked(app);
    state.restarting.release();
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

/// 收掉当前本地回环反代（若有）：关闭回环监听，活动连接按各自框架自然收尾
/// （尽力而为，见 remote_proxy 模块注释）。与 stop_child 同款手法：锁只护句柄摘取。
pub fn stop_proxy(app: &tauri::AppHandle) {
    let state: tauri::State<AppState> = app.state();
    let proxy = state.proxy.lock().unwrap().take();
    if let Some(proxy) = proxy {
        proxy.stop();
    }
}

/// 远程连接序列：收旧反代 → 起新反代 → 经反代全路径探活 → 代理 origin 入放行表 →
/// 事件流直连网关（带凭证）→ 导航（经 pair?token= 种 cookie）。
/// 页面以 http://127.0.0.1:<port>（反代）加载：dsh 视为本机浏览器（模型/设置完整可用），
/// 回环天然是安全上下文；代理自动注入 x-remote-token，探活无须手工带头。
/// 与 start_service 共用同一把 `restarting` 闸锁，防本地/远程并发双拉；远程模式不落
/// child 句柄（子进程归远端管），守护线程因 child=None 自然失活，不会误拉本地服务。
/// 整个流程持闸锁，在途时本调用排队等待（FlowGate 排队语义）。
pub fn connect_remote_flow(app: &tauri::AppHandle) -> Result<(), String> {
    let state: tauri::State<AppState> = app.state();
    state.restarting.acquire();
    let result = connect_remote_flow_locked(app);
    state.restarting.release();
    result
}

/// 远程连接序列锁内主体（须持 `restarting` 闸锁调用，不再重复取锁）。
fn connect_remote_flow_locked(app: &tauri::AppHandle) -> Result<(), String> {
    let state: tauri::State<AppState> = app.state();
    // 收掉上一轮反代（若有）：重连/换实例时旧监听端口与旧 origin 一并作废
    stop_proxy(app);
    let cfg = crate::remote::load().ok_or("尚未配对远程实例，请先输入地址与配对码")?;
    // 配置在场但 token 为空 ⇒ remote.json 是 tokenEnc 形状且解密失败（DPAPI 绑定
    // 用户+机器：换账户/重装系统后解不开）。与「尚未配对」区分，给出明确的重配指引。
    if cfg.token.is_empty() {
        return Err("凭证无法解密，请重新配对远程实例".into());
    }
    status::set(app, &format!("连接远程实例 {}…", cfg.address));
    // remote_proxy::start 是 async，而本函数是跑在专用 std 线程上的同步流程
    // （命令层/setup 均另起线程调用），用 tauri::async_runtime::block_on 在本线程
    // 内联完成（与 tray.rs 的 block_on 用法一致）：保住「启动→探活→失败即停」
    // 的顺序式错误处理形状，不必把一串调用方改成 async。std::thread 不携带
    // tokio 运行时上下文，这里 block_on 不会踩「runtime 内再起 runtime」。
    let proxy = tauri::async_runtime::block_on(crate::remote_proxy::start(
        crate::remote_proxy::ProxyConfig {
            origin: cfg.origin.clone(),
            token: cfg.token.clone(),
            address: cfg.address.clone(),
        },
    ))?;
    *state.proxy.lock().unwrap() = Some(proxy.clone());
    let proxy_origin = format!("http://127.0.0.1:{}", proxy.port);
    // 探活走代理（全路径体检；代理自动注入凭证头，网关 401 这关照过——凭证失效早暴露）
    if !crate::readiness::wait_http_ok(
        &format!("{proxy_origin}/"),
        Duration::from_secs(HEALTH_WAIT_SECS),
    ) {
        stop_proxy(app);
        return Err(format!("无法连接远程实例 {}（超时或凭证失效）", cfg.address));
    }
    // 凭证有效才放行导航与事件流：origin 换成代理回环 origin，导航守卫同步收紧
    *state.origin.lock().unwrap() = Some(proxy_origin.clone());
    // 壳通知仍直连网关（不经代理：少一跳，事件流 WS 直连）
    crate::events::spawn_remote(app, &cfg.origin, &cfg.token);
    // 经 pair?token= 导航：网关 303 + Set-Cookie 种下 webview 凭证后落到 /
    // （同 dsh 一次性 token 心智；必须原生导航——跨站发起的页面导航不带
    // SameSite=Strict 的 cookie，理由同 navigate_to_harness 注释）。
    // 303 的相对 Location 落回代理 origin，cookie 即种在页面 origin（127.0.0.1:port）。
    webview::navigate_to_harness(
        app,
        &format!("{proxy_origin}/__remote/pair?token={}", cfg.token),
    );
    Ok(())
}

/// 切回本地模式（加载页 switch_to_local 命令与托盘「断开远程，回到本地」共用）：
/// 整个流程持 `restarting` 闸锁：收尾本地 child/反代 → 模式翻转并落盘 → 托盘菜单
/// 重建 → 撤 origin 回加载页 → 重启序列（start_service_locked，锁内主体不再重复取锁）。
/// 并发流程排队等待，流程原子性保证不再有「锁外收尾 vs 在途启动」的中途分叉。
pub fn switch_to_local_flow(app: &tauri::AppHandle) {
    let state: tauri::State<AppState> = app.state();
    state.restarting.acquire();
    let result = (|| -> Result<(), String> {
        // 防御性收尾：正常远程模式 child 恒为 None，但升级运行时等路径可能在远程模式下
        // 留下本地 child——不清掉会让 start_service 双拉本地实例
        stop_child(app);
        // 收掉远程反代：页面即将回本地加载页，回环监听不再有流量
        stop_proxy(app);
        *state.origin.lock().unwrap() = None;
        *state.mode.lock().unwrap() = "local";
        crate::remote::save_mode("local");
        // 模式已翻转：托盘菜单换成本地菜单（set_menu 内部自派发主线程，任意线程可调）
        crate::tray::rebuild(app);
        // 先落一帧本地模式状态再回加载页：首帧轮询不再重放上一帧的远程错误态
        status::set(app, "正在切换到本地模式…");
        webview::navigate_to_loader(app);
        start_service_locked(app)
    })();
    state.restarting.release();
    if let Err(err) = result {
        status::fail(app, &err);
    }
}

/// 连接已保存的远程实例（托盘「已保存的远程实例」子菜单，D2 多实例）：
/// 解密该实例 token → 升为活动 remote.json → 翻模式 → 走既有 restart_by_mode
/// 远程序列（FlowGate 排队/托盘重建/连接屏全复用）。token 解密失败给出重新配对文案。
pub fn connect_saved(app: &tauri::AppHandle, address: &str) {
    let Some(cfg) = crate::remote::load_saved(address) else {
        crate::status::fail(app, "已保存实例的凭证无法读取，请在连接屏重新配对");
        return;
    };
    if crate::remote::save(&cfg).is_err() {
        crate::status::fail(app, "保存活动实例失败");
        return;
    }
    crate::remote::save_mode("remote");
    let state: tauri::State<AppState> = app.state();
    *state.mode.lock().unwrap() = "remote";
    crate::tray::rebuild(app);
    crate::status::set(app, &format!("正在连接已保存的远程实例 {address}…"));
    restart_by_mode(app);
}

/// 托盘「重启服务」/ 重启命令的统一入口：按当前模式分派。整个流程持同一把闸锁
/// （排队 acquire），且**模式读取移入锁内**——排队等到的时点模式可能已被前一流程
/// 翻转，锁外读是脏读。本地/远程分支共用收尾段（stop_child + stop_proxy；本地模式
/// proxy 恒为 None，stop 是无害空转），再按模式进入对应启动序列。
pub fn restart_by_mode(app: &tauri::AppHandle) {
    let state: tauri::State<AppState> = app.state();
    state.restarting.acquire();
    let result = (|| -> Result<(), String> {
        // 远程重连前先收掉残留 child 与旧反代（connect_remote_flow_locked 开头还会再兜一道）
        stop_child(app);
        stop_proxy(app);
        *state.origin.lock().unwrap() = None;
        webview::navigate_to_loader(app);
        if *state.mode.lock().unwrap() == "remote" {
            status::set(app, "正在重连远程实例…");
            connect_remote_flow_locked(app)
        } else {
            status::set(app, "正在重启服务…");
            start_service_locked(app)
        }
    })();
    state.restarting.release();
    if let Err(e) = result {
        status::fail(app, &e);
    }
}

/// 服务守护：意外退出时按次数上限自动重启。
pub fn watch_child(app: &tauri::AppHandle) {
    loop {
        std::thread::sleep(Duration::from_secs(3));
        let state: tauri::State<AppState> = app.state();
        if state.restarting.is_held() {
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

    /* ── 就绪地址一致性（同世代唯一事实） ── */
    #[test]
    fn accept_ready_first_wins_repeat_ignored_conflict_rejected() {
        let mut seen: Option<(String, String)> = None;
        // 首次命中：返回地址并定格
        let first = accept_ready(&mut seen, "dsh web: http://127.0.0.1:4418/?token=t1").unwrap();
        assert_eq!(first, Some(("http://127.0.0.1:4418".into(), "http://127.0.0.1:4418/?token=t1".into())));
        // 同地址重复报（正常日志重放/多行）：忽略，不冲突
        assert_eq!(
            accept_ready(&mut seen, "dsh web: http://127.0.0.1:4418/?token=t1").unwrap(),
            None
        );
        // 不同地址：冲突错误，保留首次事实
        let err = accept_ready(&mut seen, "dsh web: http://127.0.0.1:9999/").unwrap_err();
        assert!(err.contains("4418") && err.contains("9999"), "{err}");
        assert_eq!(seen.as_ref().unwrap().0, "http://127.0.0.1:4418");
        // 非 URL 行：始终 Ok(None)
        assert_eq!(accept_ready(&mut seen, "listening on 8080").unwrap(), None);
    }

    /* ── 启动期输出尾环（诊断留证） ── */
    #[test]
    fn tail_ring_keeps_last_lines_and_truncates_by_chars() {
        let mut tail = VecDeque::new();
        for i in 0..(TAIL_LINES + 10) {
            push_tail(&mut tail, &format!("line-{i}"));
        }
        assert_eq!(tail.len(), TAIL_LINES);
        let text = tail_text(&tail);
        assert!(text.contains(&format!("line-{}", TAIL_LINES + 9)), "保留最新行");
        assert!(!text.contains("line-0"), "最旧行应被挤出");

        // 按字符截断：多字节 UTF-8 不切半
        let mut long = VecDeque::new();
        push_tail(&mut long, &"错".repeat(TAIL_MAX_CHARS + 100));
        let t = tail_text(&long);
        assert_eq!(t.chars().count(), TAIL_MAX_CHARS + 1); // 1 个省略号 + 截断正文
        assert!(t.starts_with('…'));

        // 空尾环 → 空摘要，with_tail 原样返回不拼空段
        let empty = VecDeque::new();
        assert_eq!(tail_text(&empty), "");
        assert_eq!(with_tail("失败原因", &tail_text(&empty)), "失败原因");
        assert!(with_tail("失败原因", &tail_text(&tail)).contains("最近输出"));
    }

    /* ── 进程登记（D3 冷启动孤儿缺口：登记内容与占位语义） ── */
    #[test]
    fn pid_record_json_carries_shell_child_and_port() {
        let rec: PidRecord = serde_json::from_str(&pid_record_json(1234, 44182).unwrap()).unwrap();
        assert_eq!(rec.shell_pid, std::process::id());
        assert_eq!(rec.child_pid, 1234);
        assert_eq!(rec.port, 44182);
    }

    #[test]
    fn cold_start_early_record_uses_zero_port_placeholder() {
        // 冷启动修复：spawn 成功即刻登记，此时端口未知（--port 0 由 OS 分配）以 0 占位；
        // cleanup_stale_orphan 只读 pid 字段做孤儿识别，占位不影响
        let rec: PidRecord = serde_json::from_str(&pid_record_json(42, 0).unwrap()).unwrap();
        assert_eq!(rec.child_pid, 42);
        assert_eq!(rec.port, 0);
    }

    /* ── Node 崩溃横幅识别（0.1.18 崩溃快速失败） ── */
    #[test]
    fn crash_banner_matches_node_version_line() {
        assert_eq!(
            crash_banner("[err] Node.js v24.19.0").as_deref(),
            Some("Node.js v24.19.0")
        );
        assert_eq!(crash_banner("Node.js v20.11.1").as_deref(), Some("Node.js v20.11.1"));
    }

    #[test]
    fn crash_banner_ignores_mentions_and_non_banner_lines() {
        // 错误信息中途出现「Node.js」字样不算横幅
        assert!(crash_banner("[err] Error: requires Node.js v24 or later").is_none());
        assert!(crash_banner("[err]     at async boot (file:///.../boot.js:1:1)").is_none());
        assert!(crash_banner("").is_none());
    }

    #[test]
    fn spawn_signal_ready_and_crashed_are_distinct() {
        let ready = SpawnSignal::Ready("http://127.0.0.1:1".into(), "http://127.0.0.1:1/".into());
        let crashed = SpawnSignal::Crashed("Error: boom（Node.js v24.19.0）".into());
        assert_ne!(ready, crashed);
        assert_eq!(
            ready,
            SpawnSignal::Ready("http://127.0.0.1:1".into(), "http://127.0.0.1:1/".into())
        );
    }

     #[test]
     fn base_url_port_parses_origin_or_falls_back_to_zero() {
         assert_eq!(base_url_port("http://127.0.0.1:44182"), 44182);
         assert_eq!(base_url_port("http://127.0.0.1"), 0);
     }

    /* ── profile bundle 失联的响应式自愈（真实故障 2026-09） ── */
    #[test]
    fn bundle_error_detection_matches_whole_message() {
        assert!(is_profile_bundle_error(
            "服务提前退出，未报告监听地址（Error: dsh: cannot resolve profile bundle \"dsh-better-sidebar\" …）（Node.js v24.19.0）"
        ));
        assert!(is_profile_bundle_error("Error: dsh: cannot resolve profile bundle \"x\""));
        assert!(!is_profile_bundle_error("Error: ECONNREFUSED 127.0.0.1:443"));
        assert!(!is_profile_bundle_error("服务在 180 秒内未报告监听地址"));
    }

    #[test]
    fn bundle_error_extracts_profile_from_fix_hint_first() {
        // 真实崩溃消息（Windows 路径 + 修复提示并存）：提示里的名字优先
        let msg = "Error: dsh: cannot resolve profile bundle \"dsh-better-sidebar\" from the dsh installation or C:\\Users\\lome\\AppData\\Local\\dsh-desktop\\home\\profiles\\web; run 'dsh plugin --profile web install' if its dependency is not installed";
        assert_eq!(profiles_from_bundle_error(msg), Some(vec!["web".to_string()]));
        // Unix 路径无修复提示：从 profiles/<name> 路径段取
        let unix = "Error: dsh: cannot resolve profile bundle \"x\" or /home/lome/Library/Application Support/dsh-desktop/home/profiles/web; dependency missing";
        assert_eq!(profiles_from_bundle_error(unix), Some(vec!["web".to_string()]));
        // 提示存在但紧跟非名字字符：跳过该处继续找路径段
        let odd = "hint: run 'dsh plugin --profile  install'; profiles\\web is the dir";
        assert_eq!(profiles_from_bundle_error(odd), Some(vec!["web".to_string()]));
        // 无关消息：None（调用方兜底全量补装）
        assert_eq!(profiles_from_bundle_error("Error: boom"), None);
    }
}
