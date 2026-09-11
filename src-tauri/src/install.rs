//! 运行时安装与升级：便携 Node + 固定版本 dsh 装入应用数据目录
//! （Windows %LOCALAPPDATA%\dsh-desktop，macOS ~/Library/Application Support/dsh-desktop）。
//! 全程使用系统自带工具（curl 下载、tar 解压：Windows/macOS 为 bsdtar，Linux 为 gnu tar），
//! 零新增 Rust 依赖；下载走 npmmirror 镜像，nodejs.org / npm 官方源兜底。
use std::path::PathBuf;
use std::process::Command;

use crate::runtime::{self, no_window};
use crate::{status, supervisor};
use tauri::Manager;

/// 固定的 dsh 基线版本（全新环境首装用；升级走 alpha/latest 双 tag 择新，可用
/// DSH_DESKTOP_DSH_VERSION 固定）。基线必须跟上插件生态的 API 代际：profile 插件
/// （如 dsh-better-sidebar@0.18）的 peer 依赖按 rc.1 构建，基线落后会让插件全部
/// 因 API 缺符号（settingsNamespace）加载失败（真实故障 2026-09）。
pub const DSH_VERSION: &str = "0.1.5-alpha.1";
/// 便携 Node 版本（dsh rc.x 的 zstd 要求需要 Node 24）。
const NODE_VERSION: &str = "24.19.0";
/// 壳已适配的 dsh 最高版本（语义化三元组）。0.1.5-alpha.1 真机复核：六依赖点
/// （web 启动/stdout、token 认证、remote.mux、trustedHosts、webserver 注入、CLI
/// 转发）rc.1→0.1.5 零破坏（多角色评估，2026-09-08）；session v3 迁移单向是
/// 已知取舍。升到 (0,1,5) 放行 0.1.3/0.1.4/0.1.5 系列。npm 超出此版本时仍拒绝
/// 升级并引导先升级应用本体；DSH_DESKTOP_DSH_VERSION 显式指定视为知情强制。
const DSH_MAX_ADAPTED: (u64, u64, u64) = (0, 1, 5);

/// 壳已适配的 dsh 最高版本三元组（supervisor 启动预检用）。
pub fn max_adapted() -> (u64, u64, u64) {
    DSH_MAX_ADAPTED
}

/// 解析语义化版本三元组（忽略 `-rc.x`/`-alpha.x`/`+build` 等后缀；解析失败返回 None）。
/// supervisor 启动预检复用，故公开。
pub fn version_triple_public(v: &str) -> Option<(u64, u64, u64)> {
    version_triple(v)
}

/// 解析语义化版本三元组（忽略 `-rc.x`/`-alpha.x`/`+build` 等后缀；解析失败返回 None）。
fn version_triple(v: &str) -> Option<(u64, u64, u64)> {
    let mut it = v.split(['-', '+']).next()?.split('.');
    Some((
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
    ))
}

/// 拆出版本的预发布段（`-` 后、`+build` 前；无则 None）。
fn prerelease_of(v: &str) -> Option<&str> {
    v.split_once('-').map(|(_, rest)| rest.split('+').next().unwrap_or(rest))
}

/// 预发布标识符比较：纯数字按数值且小于字母标识符，其余按 ASCII（语义化规范 §11）。
fn cmp_prerelease_ident(a: &str, b: &str) -> std::cmp::Ordering {
    match (a.parse::<u64>(), b.parse::<u64>()) {
        (Ok(x), Ok(y)) => x.cmp(&y),
        (Ok(_), Err(_)) => std::cmp::Ordering::Less,
        (Err(_), Ok(_)) => std::cmp::Ordering::Greater,
        (Err(_), Err(_)) => a.cmp(b),
    }
}

/// 预发布段比较：无预发布（正式版）> 有预发布；有则逐标识符，前缀短者小。
fn cmp_prerelease(a: Option<&str>, b: Option<&str>) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    match (a, b) {
        (None, None) => Equal,
        (None, Some(_)) => Greater,
        (Some(_), None) => Less,
        (Some(x), Some(y)) => {
            let mut xi = x.split('.');
            let mut yi = y.split('.');
            loop {
                match (xi.next(), yi.next()) {
                    (None, None) => return Equal,
                    (None, Some(_)) => return Less,
                    (Some(_), None) => return Greater,
                    (Some(p), Some(q)) => {
                        let ord = cmp_prerelease_ident(p, q);
                        if ord != Equal {
                            return ord;
                        }
                    }
                }
            }
        }
    }
}

/// 预发布感知的完整版本比较（升级通道择新用；`version_triple` 会把 alpha/rc 抹平，
/// 无法区分 0.1.2-alpha.5 与 0.1.2-rc.1 谁新——真实故障：latest 已是 rc.1 而通道逻辑
/// 把 alpha 用户锁死在 alpha.5）。解析失败的版本对按相等处理（调用方容忍）。
pub fn cmp_versions(a: &str, b: &str) -> std::cmp::Ordering {
    match (version_triple(a), version_triple(b)) {
        (Some(x), Some(y)) => {
            let ord = x.cmp(&y);
            if ord != std::cmp::Ordering::Equal {
                return ord;
            }
            cmp_prerelease(prerelease_of(a), prerelease_of(b))
        }
        _ => std::cmp::Ordering::Equal,
    }
}

/// 给命令前置便携 node 目录到 PATH（Unix 的 npm 脚本用 `#!/usr/bin/env node` 找解释器）。
fn prepend_node_path(c: &mut Command) {
    let node_bin = runtime::runtime_root().join("node").join("bin");
    let sep = if cfg!(windows) { ";" } else { ":" };
    let sys = std::env::var("PATH").unwrap_or_default();
    c.env("PATH", format!("{}{}{}", node_bin.display(), sep, sys));
}

/// Node 发行版平台标签（win-x64 / darwin-arm64 / darwin-x64 / linux-x64 / linux-arm64）。
fn node_platform_tag() -> Result<&'static str, String> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => Ok("win-x64"),
        ("macos", "aarch64") => Ok("darwin-arm64"),
        ("macos", "x86_64") => Ok("darwin-x64"),
        ("linux", "x86_64") => Ok("linux-x64"),
        ("linux", "aarch64") => Ok("linux-arm64"),
        (os, arch) => Err(format!("暂不支持的平台 {os}-{arch}")),
    }
}

/// Node 发行版压缩包文件名（win 为 zip，mac 为 tar.gz，linux 为 tar.xz）。
fn node_archive_name() -> Result<String, String> {
    let tag = node_platform_tag()?;
    let ext = if cfg!(windows) {
        "zip"
    } else if cfg!(target_os = "macos") {
        "tar.gz"
    } else {
        "tar.xz"
    };
    Ok(format!("node-v{NODE_VERSION}-{tag}.{ext}"))
}

/// 解压后 Node 顶层目录名（不含扩展名）。
fn node_inner_dir() -> Result<String, String> {
    Ok(format!("node-v{NODE_VERSION}-{}", node_platform_tag()?))
}

fn node_mirror_urls() -> Vec<String> {
    let Ok(name) = node_archive_name() else {
        return vec![];
    };
    let mut urls = vec![format!("https://npmmirror.com/mirrors/node/v{NODE_VERSION}/{name}")];
    if let Ok(custom) = std::env::var("DSH_DESKTOP_NODE_MIRROR") {
        urls.insert(0, format!("{custom}/v{NODE_VERSION}/{name}"));
    }
    urls.push(format!("https://nodejs.org/dist/v{NODE_VERSION}/{name}"));
    urls
}

fn npm_registry() -> Vec<String> {
    match std::env::var("DSH_DESKTOP_NPM_REGISTRY") {
        Ok(r) if !r.is_empty() => vec![format!("--registry={r}")],
        _ => vec![
            "--registry=https://registry.npmmirror.com".to_string(),
            "--registry=https://registry.npmjs.org".to_string(),
        ],
    }
}

/// 升级/查询所用的运行时根：优先解析到的便携根（含 dsh-persona 复用），
/// 没有便携运行时时回退自有目录（此时 install_runtime 会先装基线）。
fn active_root() -> PathBuf {
    runtime::ready_root().unwrap_or_else(|| runtime::runtime_root())
}

/// 便携运行时中的 npm 可执行入口（Windows 为 npm.cmd，Unix 为 bin/npm）。
fn npm_tool() -> Option<PathBuf> {
    let npm = active_root().join("node").join(if cfg!(windows) {
        "npm.cmd"
    } else {
        "bin/npm"
    });
    npm.exists().then_some(npm)
}

/// 读取便携运行时中已安装的 dsh 版本（package.json 的 version 字段）。
pub fn installed_dsh_version() -> Option<String> {
    let pj = active_root()
        .join("node")
        .join(if cfg!(windows) { "node_modules" } else { "lib/node_modules" })
        .join("@deepseek-ai")
        .join("dsh")
        .join("package.json");
    let text = std::fs::read_to_string(pj).ok()?;
    serde_json::from_str::<serde_json::Value>(&text)
        .ok()?
        .get("version")?
        .as_str()
        .map(String::from)
}

/// 构造一条运行 npm 的 Command。
/// Windows：直接 node + npm-cli.js，避开 cmd.exe /C npm.cmd 的窗口闪烁（v0.1.28 修复）；
/// 找不到 npm-cli.js 时回退到 cmd.exe /C npm.cmd 兼容路径。
/// Unix：直接 bin/npm（已是真二进制）。
fn npm_command() -> Result<Command, String> {
    #[cfg(windows)]
    {
        let node = runtime::node_exe();
        if let Some(cli) = runtime::portable_npm_cli_js().filter(|_| node.exists()) {
            let mut c = Command::new(node);
            c.arg(cli);
            return Ok(c);
        }
        // 回退：cmd.exe /C npm.cmd
        let npm = npm_tool().ok_or("便携运行时未安装，无法构造 npm 命令")?;
        let mut c = Command::new("cmd.exe");
        c.args(["/D", "/C"]).arg(&npm);
        return Ok(c);
    }
    #[cfg(not(windows))]
    {
        let npm = npm_tool().ok_or("便携运行时未安装，无法构造 npm 命令")?;
        Ok(Command::new(&npm))
    }
}

/// 查询 npm registry 上 @deepseek-ai/dsh 指定 dist-tag 的版本。
fn dist_tag_version(tag: &str) -> Result<String, String> {
    let mut last_err = String::new();
    for registry in npm_registry() {
        let mut c = npm_command()?;
        c.args(["view", "@deepseek-ai/dsh", &format!("dist-tags.{tag}")]).arg(&registry);
        prepend_node_path(&mut c);
        match no_window(&mut c).output() {
            Ok(o) if o.status.success() => {
                let v = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if !v.is_empty() {
                    return Ok(v);
                }
                last_err = "registry 返回空版本".into();
            }
            Ok(_) => last_err = "npm view 退出码非零".into(),
            Err(e) => last_err = e.to_string(),
        }
    }
    Err(format!("查询最新版本失败：{last_err}（可设置 DSH_DESKTOP_NPM_REGISTRY）"))
}

/// 升级目标版本：DSH_DESKTOP_DSH_VERSION 显式指定优先；否则查询 alpha/latest 两个
/// dist-tag 取**较高**者（预发布感知比较，cmp_versions）。旧逻辑「装了 alpha 就只跟
/// alpha tag」是为防「升级按钮变降级」（latest 曾指向旧稳定线），但 latest 反超 alpha
/// 时形成死锁——真实故障：用户被锁死 0.1.2-alpha.5，npm latest 已是 0.1.2-rc.1，插件
/// 生态按 rc.1 构建，升级按钮永远提示「已是最新」。单 tag 查询失败时用另一个兜底。
fn target_version() -> Result<String, String> {
    if let Ok(v) = std::env::var("DSH_DESKTOP_DSH_VERSION") {
        if !v.is_empty() {
            return Ok(v);
        }
    }
    let mut last_err = String::new();
    let mut best: Option<String> = None;
    for tag in ["alpha", "latest"] {
        match dist_tag_version(tag) {
            Ok(v) => {
                let is_newer = best
                    .as_deref()
                    .map_or(true, |cur| cmp_versions(&v, cur) == std::cmp::Ordering::Greater);
                if is_newer {
                    best = Some(v);
                }
            }
            Err(e) => last_err = e,
        }
    }
    best.ok_or(last_err)
}

/// 安装指定版本的 dsh 到活动便携运行时（输出落日志）。
fn npm_install_dsh(version: &str) -> Result<(), String> {
    let mut last_err = String::new();
    for registry in npm_registry() {
        match npm_install_dsh_once(version, &registry) {
            Ok(()) => return Ok(()),
            Err(e) => last_err = e,
        }
    }
    Err(format!("DSH v{version} 安装失败：{last_err}"))
}

/// 探测已装便携 dsh 的 web 命令是否接受 --no-open。
/// 防 npmmirror 镜像滞后返回缺该 flag 的旧 tarball（真实故障：装到 alpha.4 但无 --no-open）。
pub fn web_supports_no_open() -> bool {    let node = runtime::node_exe();
    let bin = runtime::dsh_bin_js();
    if !node.exists() || !bin.exists() {
        return true; // 无可探测对象，视为支持（后续启动自检会兜底）
    }
    let mut c = Command::new(&node);
    c.arg(&bin).args(["web", "--help"]);
    prepend_node_path(&mut c);
    match no_window(&mut c).output() {
        Ok(o) => {
            let s = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
            s.contains("--no-open")
        }
        Err(_) => true,
    }
}

/// 强制从官方源重装基线 dsh（镜像包不完整时的自愈手段）。
pub fn force_reinstall_official() -> Result<(), String> {
    npm_install_dsh_once(DSH_VERSION, "--registry=https://registry.npmjs.org")
}

/// 把捕获的子进程输出原样补写进日志（此前直接 Stdio 继承tee，因需解析内容改为捕获）。
fn tee_bytes(log: &mut Option<std::fs::File>, bytes: &[u8]) {
    if let Some(f) = log.as_mut() {
        use std::io::Write;
        let _ = f.write_all(bytes);
    }
}

fn npm_install_dsh_once(version: &str, registry: &str) -> Result<(), String> {
    let node_dir = active_root().join("node");
    let mut log = runtime::open_log_append();
    if let Some(f) = log.as_mut() {
        use std::io::Write;
        let _ = writeln!(f, "[npm] registry={registry} target={version}");
    }
    let mut c = npm_command()?;
    c.args(["install", "-g", &format!("@deepseek-ai/dsh@{version}"), "--prefix"])
        .arg(&node_dir)
        .arg(registry);
    prepend_node_path(&mut c);
    // 捕获输出再落日志：要解析 allow-scripts 拦截清单，直接 Stdio 继承拿不到内容
    let result = no_window(&mut c).output();
    if let Ok(o) = &result {
        tee_bytes(&mut log, &o.stdout);
        tee_bytes(&mut log, &o.stderr);
    }
    match result {
        Ok(o) if o.status.success() => {
            // npm 11.16+ 对未在 allowScripts 策略内的依赖安装脚本告警、npm 12 起默认拦截：
            // koffi/node-pty 等原生模块缺构建脚本要到运行时才炸，解析拦截清单立即补跑。
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            );
            let skipped = parse_allow_scripts_skipped(&text);
            if !skipped.is_empty() {
                if let Err(e) = rerun_blocked_install_scripts(&skipped) {
                    // 补跑失败不阻断启动：多数关键包自带 prebuild，服务仍可起；留证日志
                    if let Some(f) = log.as_mut() {
                        use std::io::Write;
                        let _ = writeln!(f, "[warn] 安装脚本补跑失败（原生模块可能不可用）: {e}");
                    }
                }
            }
            Ok(())
        }
        Ok(_) => Err("npm 退出码非零（详见日志）".into()),
        Err(e) => Err(e.to_string()),
    }
}

/// 从 npm 输出解析「安装脚本被 allow-scripts 策略跳过」的包名清单。
/// 真实输出形态：
/// ```text
/// npm warn allow-scripts 5 packages have install scripts not yet covered by allowScripts:
/// npm warn allow-scripts   @deepseek-ai/dsh-subprocess-local@0.1.2-rc.1 (postinstall: node scripts/ensure-spawn-helper.mjs)
/// npm warn allow-scripts   koffi@3.2.1 (install: node ./cnoke.cjs -P . -D src/koffi --prebuild --release)
/// ```
/// 只取 `包名@版本` 条目行，头部统计行与建议行忽略；同名去重保序。
fn parse_allow_scripts_skipped(output: &str) -> Vec<String> {
    const MARKER: &str = "npm warn allow-scripts";
    let mut names: Vec<String> = Vec::new();
    for line in output.lines() {
        let Some(idx) = line.find(MARKER) else { continue };
        let rest = line[idx + MARKER.len()..].trim();
        if rest.is_empty()
            || rest.contains("packages have install scripts")
            || rest.starts_with("Run `npm")
        {
            continue;
        }
        let token = rest.split_whitespace().next().unwrap_or("");
        // `name@version`：scope 包名本身含 @（@deepseek-ai/dsh@1.0.0），取最后一个 @ 剥版本
        if let Some(at) = token.rfind('@') {
            if at > 0 {
                let name = &token[..at];
                if !name.is_empty() && !names.iter().any(|n| n == name) {
                    names.push(name.to_string());
                }
            }
        }
    }
    names
}

/// npm 拦截了依赖安装脚本时补跑：`npm rebuild -g <pkgs> --allow-scripts=…` 显式执行
/// 被拦截包的 install 脚本（rebuild 就是「对已装树补跑脚本」的官方通道，且仅在本函数
/// 被调用的前提——npm 输出里出现了 allow-scripts 警告——下才走，该配置必然被识别）。
fn rerun_blocked_install_scripts(skipped: &[String]) -> Result<(), String> {
    let node_dir = active_root().join("node");
    let mut log = runtime::open_log_append();
    let mut c = npm_command()?;
    c.arg("rebuild").arg("-g").args(skipped.iter().map(String::as_str));
    c.arg(format!("--allow-scripts={}", skipped.join(",")));
    c.arg("--prefix").arg(&node_dir);
    prepend_node_path(&mut c);
    let result = no_window(&mut c).output();
    if let Ok(o) = &result {
        tee_bytes(&mut log, &o.stdout);
        tee_bytes(&mut log, &o.stderr);
    }
    match result {
        Ok(o) if o.status.success() => {
            if let Some(f) = log.as_mut() {
                use std::io::Write;
                let _ = writeln!(
                    f,
                    "[自愈] 已补跑 {} 个被 allow-scripts 拦截的安装脚本: {}",
                    skipped.len(),
                    skipped.join(",")
                );
            }
            Ok(())
        }
        Ok(_) => Err("npm rebuild 退出码非零（详见日志）".into()),
        Err(e) => Err(e.to_string()),
    }
}

/// 下载单个文件：优先 curl（各平台自带），Windows 用 PowerShell、Unix 用 wget 兜底。
fn download(url: &str, dest: &PathBuf) -> Result<(), String> {
    let mut c = if cfg!(windows) {
        Command::new("curl.exe")
    } else {
        Command::new("curl")
    };
    c.args(["-L", "--fail", "--connect-timeout", "20", "-o"]);
    c.arg(dest);
    c.arg(url);
    if matches!(no_window(&mut c).status(), Ok(s) if s.success()) {
        return Ok(());
    }
    #[cfg(windows)]
    {
        let mut p = Command::new("powershell");
        p.args(["-NoProfile", "-Command", &format!(
            "Invoke-WebRequest -Uri '{}' -OutFile '{}'",
            url,
            dest.display()
        )]);
        if matches!(no_window(&mut p).status(), Ok(s) if s.success()) {
            return Ok(());
        }
    }
    #[cfg(unix)]
    {
        let mut w = Command::new("wget");
        w.args(["-q", "--timeout=30", "-O"]).arg(dest).arg(url);
        if w.status().map(|s| s.success()).unwrap_or(false) {
            return Ok(());
        }
    }
    Err(format!("下载失败 ({url})"))
}

/// 自愈入口（锁内调用）：由 supervisor 在 bootstrap_runtime 返回 NEED_AUTO_REPAIR 时触发。
/// 与 install_runtime 共享安装逻辑，但不取 restarting 闸锁（调用方已持锁）。
pub fn ensure_runtime_locked(app: &tauri::AppHandle) -> Result<(), String> {
    install_runtime_inner(app)
}

/// 凭证文件权限自愈（POSIX）：dsh 0.1.3+ 的 credentials-local 对
/// `<home>/.credentials.yaml` 强制 owner-only（组/他人可读即抛错），webserver
/// 处理器抛错后 res.destroy() 掐断连接，WebKit 侧表现为 credentials/set
/// "Load failed"（Mac 实机：旧版 dsh/迁移拷贝留下 644 的凭证文件）。
/// 组/他人位非零则归位为 600；文件缺失或已合规则空操作。
pub fn enforce_credentials_owner_mode() {
    enforce_owner_mode(&runtime::app_home().join(".credentials.yaml"));
}

/* ── webserver keep-alive 补丁 ─────────────────────────────────────────── */

/// 注入的 keep-alive 参数：65s 覆盖 WebKit 连接池的保留时长，消除「停顿后
/// 首个请求撞陈旧连接」的竞态窗口；headersTimeout 须 > keepAliveTimeout（Node 约束）。
const KEEPALIVE_PATCH: &str = "this.server.keepAliveTimeout = 65000;\n\t\t\tthis.server.headersTimeout = 66000;\n\t\t\t";
/// 注入锚点：已装包为未压缩 tsc 产物，该串在 dsh-host-webserver/lib/index.js 内唯一。
const KEEPALIVE_ANCHOR: &str = "this.server.listen(this.config.port";

/// webserver keep-alive 自愈补丁：dsh 用 Node 默认 keepAliveTimeout=5s，空闲连接
/// 5 秒即被服务端关闭；WebKit(macOS) 连接池保留更久且不自动重试 POST——用户停顿后
/// 首个请求（发消息/存凭证/加载预设）撞上陈旧连接即报
/// 「client api:... failed: Load failed」，重试才恢复（Mac 实机：多处偶发）。
/// 幂等（marker 检查）；锚点缺失（上游改版）时留证跳过，不影响启动。
pub fn patch_webserver_keepalive() {
    use std::io::Write;
    let Some(dsh_dir) = runtime::dsh_package_dir() else {
        return;
    };
    // npm 嵌套布局（实测）：dsh/node_modules/@deepseek-ai/dsh-host-webserver/...；
    // hoisted 布局兜底：node_modules/@deepseek-ai/dsh-host-webserver/...
    let nested = dsh_dir.join("node_modules/@deepseek-ai/dsh-host-webserver/lib/index.js");
    let hoisted = dsh_dir
        .parent()
        .map(|scope| scope.join("dsh-host-webserver/lib/index.js"));
    for candidate in [Some(nested), hoisted].into_iter().flatten() {
        let Ok(content) = std::fs::read_to_string(&candidate) else {
            continue;
        };
        if content.contains("keepAliveTimeout = 65000") {
            return; // 已打补丁
        }
        if content.matches(KEEPALIVE_ANCHOR).count() != 1 {
            if let Some(mut log) = runtime::open_log_append() {
                let _ = writeln!(
                    log,
                    "[warn] keep-alive 补丁锚点不唯一/缺失（上游改版？），跳过: {}",
                    candidate.display()
                );
            }
            return;
        }
        let patched = content.replacen(
            KEEPALIVE_ANCHOR,
            &format!("{KEEPALIVE_PATCH}{KEEPALIVE_ANCHOR}"),
            1,
        );
        let tmp = candidate.with_extension(format!("js.patch-{}", std::process::id()));
        if std::fs::write(&tmp, patched).is_ok() && std::fs::rename(&tmp, &candidate).is_ok() {
            if let Some(mut log) = runtime::open_log_append() {
                let _ = writeln!(log, "[自愈] webserver keepAliveTimeout 5s -> 65s（{}）", candidate.display());
            }
            return;
        }
    }
    if let Some(mut log) = runtime::open_log_append() {
        let _ = writeln!(log, "[warn] 未找到 dsh-host-webserver，keep-alive 补丁跳过");
    }
}

/// 单文件补丁核心（纯函数便于单测）：返回注入后的内容；已打补丁/锚点异常返回 None。
fn patch_keepalive_contents(content: &str) -> Option<String> {
    if content.contains("keepAliveTimeout = 65000") {
        return None;
    }
    if content.matches(KEEPALIVE_ANCHOR).count() != 1 {
        return None;
    }
    Some(content.replacen(
        KEEPALIVE_ANCHOR,
        &format!("{KEEPALIVE_PATCH}{KEEPALIVE_ANCHOR}"),
        1,
    ))
}

/// 单文件归位（路径参数化便于单测）：返回 true 表示实际修改了权限。
#[cfg(unix)]
fn enforce_owner_mode(path: &std::path::Path) -> bool {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    let mode = meta.permissions().mode();
    if mode & 0o077 == 0 {
        return false;
    }
    let mut perms = meta.permissions();
    perms.set_mode(0o600);
    let changed = std::fs::set_permissions(path, perms).is_ok();
    if changed {
        if let Some(mut log) = runtime::open_log_append() {
            let _ = writeln!(
                log,
                "[自愈] 凭证文件权限 {:o} -> 600（{}）",
                mode & 0o777,
                path.display()
            );
        }
    }
    changed
}

#[cfg(not(unix))]
fn enforce_owner_mode(_path: &std::path::Path) -> bool {
    false // Windows 无 POSIX 权限位，dsh 也跳过该检查（win32 直接 return）
}

/// 当前 DSH home：始终用 dsh-desktop 专属 home（与系统 dsh/persona 的 ~/.dsh 隔离）。
pub fn dsh_home() -> PathBuf {
    runtime::app_home()
}

fn core_version_marker() -> PathBuf {
    runtime::runtime_root().join("last-core-version")
}

/// 核心 dsh 版本变化时清空各 profile 的 node_modules，强制按新核心重新解析插件。
/// 修复「核心升级但 profile 插件仍是旧版/符号链接指向残留安装」的版本错位（真实故障：
/// alpha.4 核心 + 旧 dsh-tool-subagent 缺 exports / 旧树 import .css 崩溃）。
/// 返回被清空的 profile 名单——清空后 bundle 实体全部失联，dsh 启动只解析不安装，
/// 调用方须对名单主动补装（install_profile_plugins）。首次运行（无标记）只记录不清理。
pub fn refresh_profile_plugins_if_core_changed() -> Vec<String> {
    let Some(cur) = installed_dsh_version() else { return Vec::new() };
    let marker = core_version_marker();
    let prev = std::fs::read_to_string(&marker).unwrap_or_default();
    let prev = prev.trim().to_string();
    if prev.is_empty() {
        let _ = std::fs::write(&marker, &cur);
        return Vec::new(); // 首次运行：仅记录，避免误清健康环境
    }
    if prev == cur {
        return Vec::new();
    }
    let _ = std::fs::write(&marker, &cur);
    let profiles = dsh_home().join("profiles");
    let Ok(entries) = std::fs::read_dir(&profiles) else { return Vec::new() };
    let mut cleared = Vec::new();
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let nm = e.path().join("node_modules");
        if nm.is_dir() && std::fs::remove_dir_all(&nm).is_ok() {
            cleared.push(name);
        }
    }
    if !cleared.is_empty() {
        if let Some(mut log) = runtime::open_log_append() {
            use std::io::Write;
            let _ = writeln!(
                log,
                "[自愈] 核心 {prev} -> {cur}，清空 {} 个 profile 插件目录强制重装: {}",
                cleared.len(),
                cleared.join(",")
            );
        }
    }
    cleared
}

/// home/profiles 下含 package.json 的 profile 名单（插件补装的目标集合）。
pub fn profile_names() -> Vec<String> {
    let profiles = dsh_home().join("profiles");
    let Ok(entries) = std::fs::read_dir(&profiles) else { return Vec::new() };
    entries
        .flatten()
        .filter(|e| e.path().is_dir() && e.path().join("package.json").is_file())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect()
}

/// 构造调用便携 dsh CLI 的 Command（环境与 supervisor::spawn_dsh 的 Portable 分支对齐：
/// DSH_HOME 指向专属 home、npm 缓存收进 home、PATH 前置便携 node、cwd 在 node 目录；
/// 非 Windows 补 HOME/NODE_PATH 保 ESM 解析）。运行时未就绪返回 None。
fn dsh_cli_command() -> Option<Command> {
    let node = runtime::node_exe();
    let bin = runtime::dsh_bin_js();
    if !node.exists() || !bin.exists() {
        return None;
    }
    let node_dir = node.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    let sep = if cfg!(windows) { ";" } else { ":" };
    let sys = std::env::var("PATH").unwrap_or_default();
    let mut c = Command::new(&node);
    c.arg(&bin);
    c.env("PATH", format!("{}{}{}", node_dir.display(), sep, sys));
    let home = dsh_home();
    c.env("DSH_HOME", &home);
    c.env("npm_config_cache", home.join(".npm-cache"));
    c.current_dir(&node_dir);
    #[cfg(not(windows))]
    {
        let home_env = std::env::var("HOME").unwrap_or_default();
        let nm = runtime::runtime_root().join("node").join("lib").join("node_modules");
        let dsh_nm = nm.join("@deepseek-ai").join("dsh").join("node_modules");
        c.env("HOME", home_env)
            .env("NODE_PATH", format!("{}:{}", nm.display(), dsh_nm.display()));
    }
    Some(c)
}

/// 逐个执行 `dsh plugin --profile <name> install`：把「profile 配置在场而插件实体
/// 失联」（核心更新清空了插件目录、profile 配置外部带入、上次安装被打断等）的
/// bundle 按当前清单重新装回 profile 目录。输出 tee 进日志；单个 profile 失败
/// 不影响其余，全部结束后汇总报错。
pub fn install_profile_plugins(profiles: &[String], why: &str) -> Result<(), String> {
    if profiles.is_empty() {
        return Ok(());
    }
    let mut log = runtime::open_log_append();
    let mut failures: Vec<String> = Vec::new();
    for name in profiles {
        let Some(mut c) = dsh_cli_command() else {
            return Err("便携运行时未就绪，无法补装 profile 插件".into());
        };
        c.args(["plugin", "--profile", name, "install"]);
        if let Some(f) = log.as_mut() {
            use std::io::Write;
            let _ = writeln!(f, "[自愈] {why}：补装 profile 插件（{name}）…");
        }
        let result = no_window(&mut c).output();
        match result {
            Ok(o) => {
                tee_bytes(&mut log, &o.stdout);
                tee_bytes(&mut log, &o.stderr);
                if !o.status.success() {
                    let stderr = String::from_utf8_lossy(&o.stderr);
                    let lines: Vec<&str> = stderr.lines().collect();
                    let start = lines.len().saturating_sub(5);
                    let tail = lines[start..].join(" | ");
                    failures.push(format!("{name}: npm 退出码非零（{tail}）"));
                }
            }
            Err(e) => failures.push(format!("{name}: {e}")),
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{} 个 profile 插件补装失败: {}",
            failures.len(),
            failures.join("；")
        ))
    }
}

/// 安装便携运行时（幂等）：Node 缺则下载解压，dsh 缺则 npm -g 安装固定版本。
/// 每步经 status 更新到加载页。供首启引导与托盘升级共用。
/// 与 supervisor 流程共用同一把 FlowGate 闸锁：在途流程未结束时排队等待而非静默
/// 放弃——用户在服务启动中途点「安装运行环境」，安装请求不再凭空消失。
pub fn install_runtime(app: &tauri::AppHandle) -> Result<(), String> {
    let state: tauri::State<crate::AppState> = app.state();
    state.restarting.acquire();
    let result = install_runtime_inner(app);
    state.restarting.release();
    result
}

fn install_runtime_inner(app: &tauri::AppHandle) -> Result<(), String> {
    let root = runtime::runtime_root();
    let node_dir = root.join("node");
    std::fs::create_dir_all(&root).map_err(|e| format!("无法创建数据目录: {e}"))?;

    // 1) 便携 Node：缺失则下载解压（按平台选发行版，顶层目录改名为 node）
    if !runtime::node_exe().exists() {
        let archive = node_archive_name()?;
        status::set(app, &format!("正在下载 Node v{NODE_VERSION}（镜像加速）…"));
        let downloads = root.join("downloads");
        std::fs::create_dir_all(&downloads).map_err(|e| format!("{e}"))?;
        let zip = downloads.join(&archive);
        let mut last_err = String::new();
        let mut ok = false;
        for url in node_mirror_urls() {
            match download(&url, &zip) {
                Ok(()) => {
                    ok = true;
                    break;
                }
                Err(e) => last_err = e,
            }
        }
        if !ok {
            return Err(format!("Node 下载失败：{last_err}"));
        }
        status::set(app, "正在解压 Node…");
        let extract_to = root.join("node-extract");
        let _ = std::fs::remove_dir_all(&extract_to);
        std::fs::create_dir_all(&extract_to).map_err(|e| format!("{e}"))?;
        // bsdtar（win/mac）与 gnu tar 均可直接解 zip/tar.gz/tar.xz
        let tar = if cfg!(windows) { "tar.exe" } else { "tar" };
        let mut c = Command::new(tar);
        c.args(["-xf"]).arg(&zip).arg("-C").arg(&extract_to);
        no_window(&mut c)
            .status()
            .map_err(|e| format!("解压失败: {e}"))
            .and_then(|s| if s.success() { Ok(()) } else { Err("解压失败".into()) })?;
        let inner = extract_to.join(node_inner_dir()?);
        let _ = std::fs::remove_dir_all(&node_dir);
        std::fs::rename(&inner, &node_dir).map_err(|e| format!("安装 Node 失败: {e}"))?;
        let _ = std::fs::remove_dir_all(&extract_to);
        let _ = std::fs::remove_file(&zip);
    }

    // 2) dsh 固定基线版本：便携 npm -g 装入 node 目录（升级走 upgrade_dsh 的远程清单）
    if !runtime::dsh_bin_js().exists() {
        status::set(app, &format!("正在安装 DSH v{DSH_VERSION}（首次约 1~3 分钟）…"));
        npm_install_dsh(DSH_VERSION)?;
    }
    // 3) 镜像完整性校验：npmmirror 可能滞后返回缺 --no-open 的旧 tarball（真实故障）。
    // 装完探测能力，不完整则切官方源强制重装，保证启动参数与包能力一致。
    if !web_supports_no_open() {
        status::set(app, "镜像包不完整，切换官方源重装 DSH…");
        if let Some(mut log) = runtime::open_log_append() {
            use std::io::Write;
            let _ = writeln!(log, "[自愈] 镜像 tarball 缺 --no-open，切 npmjs 重装 v{DSH_VERSION}");
        }
        npm_install_dsh_once(DSH_VERSION, "--registry=https://registry.npmjs.org")?;
    }
    Ok(())
}

/// 升级检查与安装（不含服务重启）：在**活动**便携运行时上就地升级
/// （含 dsh-persona 复用的运行时）；完全没有便携运行时时先装基线到自有目录。
/// 返回给用户的状态文案。
pub fn upgrade_dsh(app: &tauri::AppHandle) -> Result<String, String> {
    if npm_tool().is_none() {
        // 无便携运行时（System 回退或全新）：先装基线，之后活动根即自有目录
        install_runtime(app)?;
    }
    let target = target_version()?;
    // 升级护栏：npm latest 超出壳已适配的版本线时拒绝，防止“升级按钮变砖”。
    // DSH_DESKTOP_DSH_VERSION 显式指定视为知情强制，绕过护栏（逃生门）。
    if std::env::var("DSH_DESKTOP_DSH_VERSION").map_or(true, |v| v.is_empty()) {
        if let Some(t) = version_triple(&target) {
            if t > DSH_MAX_ADAPTED {
                return Err(format!(
                    "DSH v{target} 超出当前应用已适配的运行时版本（≤0.{}.{}.x）：该版本线启用了 Web 一次性 token 认证并更换了事件流端点。请先把 dsh-desktop 应用本体升级到配套版本；如确需强制，可设环境变量 DSH_DESKTOP_DSH_VERSION 指定目标版本。",
                    DSH_MAX_ADAPTED.0, DSH_MAX_ADAPTED.1
                ));
            }
        }
    }
    let installed = installed_dsh_version();
    if installed.as_deref() == Some(target.as_str()) {
        return Ok(format!("DSH 运行时已是最新 v{target}"));
    }
    status::set(app, &format!("正在安装 DSH 运行时 v{target}…"));
    npm_install_dsh(&target)?;
    let from = installed.unwrap_or_else(|| "无".into());
    Ok(format!("DSH 运行时已升级到 v{target}（原 {from}）"))
}

/// 托盘「升级 DSH 运行时」：停服务 → 检查并安装 → 重新启动。
/// 模式盲项收编：远程模式下不「停服务」（本地 child 恒为 None）、不撤 origin，
/// 升级完成后按远程模式重连——绝不 start_service 把本地服务拉起来顶掉远程页面
/// （升级的本地运行时等下次回到本地时生效）。
pub fn upgrade_runtime(app: &tauri::AppHandle) {
    status::set(app, "正在查询 npm 上 DSH 运行时的最新版本…");
    let state: tauri::State<crate::AppState> = app.state();
    let remote_mode = *state.mode.lock().unwrap() == "remote";
    if !remote_mode {
        // 先停服务，避免替换运行中的文件
        if let Some(mut child) = state.child.lock().unwrap().take() {
            supervisor::kill_tree(child.id() as u32);
            let _ = child.wait();
        }
        *state.origin.lock().unwrap() = None;
    }
    // 回加载页显示升级进度（远程模式仅导航不撤 origin：升级失败仍可直接重连）
    crate::webview::navigate_to_loader(app);
    match upgrade_dsh(app) {
        Ok(msg) => {
            let next = if remote_mode { "正在重连远程实例…" } else { "正在启动服务…" };
            status::set(app, &format!("{msg}，{next}"));
        }
        Err(e) => {
            status::fail(app, &e);
            return;
        }
    }
    if remote_mode {
        if let Err(e) = supervisor::connect_remote_flow(app) {
            status::fail(app, &e);
        }
    } else if let Err(e) = supervisor::start_service(app) {
        status::fail(app, &e);
    }
}

/// 首启安装入口：安装完成后自动续跑启动序列。
/// 模式盲项收编：「安装运行环境」按钮理论上只在本地错误态出现，仍按模式防御性分派——
/// 远程模式下安装完成后重连远程实例，不拉本地服务。
pub fn install_and_start(app: &tauri::AppHandle) {
    if let Err(e) = install_runtime(app) {
        status::fail(app, &e);
        return;
    }
    let remote_mode = {
        let state: tauri::State<crate::AppState> = app.state();
        // 先落局部变量再比较：块尾表达式会让 MutexGuard 临时值活过 state 的析构（E0597）
        let mode = *state.mode.lock().unwrap();
        mode == "remote"
    };
    if remote_mode {
        status::set(app, "运行环境就绪，正在重连远程实例…");
        if let Err(e) = supervisor::connect_remote_flow(app) {
            status::fail(app, &e);
        }
        return;
    }
    status::set(app, "运行环境就绪，正在启动服务…");
    if let Err(e) = supervisor::start_service(app) {
        status::fail(app, &e);
    }
}

/* ── 数字分身套件一键安装（D2b） ── */

/** 托盘「安装数字分身套件」流程：

    1) 读 `suite_path::read()` 拿持久化的套件根；失效（文件无 / 目录删了）则
       调 `suite_path::pick(app)` 弹原生文件夹选择器让用户选；选完 `write()` 落盘
    2) 校验目录下存在 `install-all.bat`——缺失立即报错并弹系统通知
    2.5) 装前快照 profile 清单与锁文件（`suite-install-backup/`）——脚本失败或
       探针失败时回滚用，防止半成品 manifest 让下次启动变 boot 毒药
       （2026-09-10 dsh-memory 解析断裂事故的教训）
    3) `cmd /c install-all.bat` 同步跑，捕获 stdout/stderr 全量写日志
    3.5) 探针校验：逐个 link: 依赖用宿主 Node import 其主机侧入口——复现 dsh
       启动的加载路径，把「装完才发现解析断裂」拦在重启之前；失败则回滚 +
       报错 + 不重启
    4) 成功 → 写 `installed-suite.json` 标记 + 系统通知 + `supervisor::restart_by_mode`
       让 dsh 重新加载 11 个插件 + 物化 digital-twin preset
    5) 失败 → 回滚 manifest + `status::fail` + 系统通知 + stderr 尾部透传（≤8 行）

    与 `install_runtime` / `upgrade_runtime` 共用同一把 FlowGate 闸锁——在途流程未
    结束时点会排队而非静默放弃。

    不抽去新文件 / 不开新 module——install.rs 已经是"安装子进程并与 dsh 交互"的
    收敛点，加新函数自然符合既有阅读路径。

    远程模式由 `tray::entry_visible` 在菜单层隐藏（远程实例装不到本地 dsh）。
*/
/// 套件安装通道（2026-09-10 需求方确认的双通道设计）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SuiteChannel {
    /// 本地调试：link: 链接本地 meta-repo 根，改源码重启即生效——面向套件开发者。
    /// 需要本机有仓库 + 子模块 checkout + 插件已构建（lib/ 存在）。
    Local,
    /// 生产：从各插件仓库的 GitHub Release 拉构建物 tarball——面向最终用户，
    /// 目标机无需仓库与工具链；重跑一次 = 升级到最新 Release（安装器按 tag 刷新 URL）。
    Release,
}

impl SuiteChannel {
    /// 状态栏/通知里的人话标签。
    fn label(self) -> &'static str {
        match self {
            SuiteChannel::Local => "本地调试",
            SuiteChannel::Release => "生产（GitHub Release）",
        }
    }

    /// 安装器仓库与分支（生产通道运行时拉取官方安装器——单一实现、零漂移：
    /// 壳不内置副本，套件仓库更新安装器后所有机器即刻受益）。
    /// 附 `?cb=<时间戳>`：raw 走 CDN 缓存（实测推送后需数分钟才全网刷新），
    /// 带唯一查询串强制回源，避免「刚修好的安装器拉到的还是旧副本」。
    fn installer_urls(self) -> Vec<String> {
        let cb = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        match self {
            SuiteChannel::Local => Vec::new(),
            SuiteChannel::Release => vec![
                format!("https://raw.githubusercontent.com/lomehong/digital-twin/main/install-all.bat?cb={cb}"),
                // ghfast 镜像兜底：与更新器 endpoints 同款国内可达性策略
                format!("https://ghfast.top/https://raw.githubusercontent.com/lomehong/digital-twin/main/install-all.bat?cb={cb}"),
            ],
        }
    }
}

/** 托盘「安装数字分身套件」流程（双通道共用主体，仅"安装器从哪来 + 传什么参数"不同）：

    1) 定位安装器：
       - 本地调试：读 `suite_path::read()` 拿持久化的套件根；失效则弹文件夹选择器，选完落盘；
         校验目录下存在 `install-all.bat`。
       - 生产：从官方仓库 raw 拉最新 `install-all.bat`（含 ghfast 镜像兜底）落到
         `runtime_root/suite-installer/`，不经用户选目录——目标机无需任何本地仓库。
    2.5) 装前快照 profile 清单与锁文件（`suite-install-backup/`）——脚本失败或探针
       失败时回滚用，防止半成品 manifest 让下次启动变 boot 毒药（2026-09-10 教训）
    3) 同步跑官方安装器（双平台共用同一份，零漂移）：
       - Windows：`cmd /c install-all.bat [-Release]`；
       - macOS/Linux：安装器是「bat 壳 + 内嵌 ESM」的多语言文件，壳提取内嵌 JS 用
         便携 Node 执行（见 run_suite_installer；首次自动准备 pnpm、附 cmd 兼容垫片
         兜住 local 通道的 junction 修复调用）。
       捕获 stdout/stderr 全量写日志
    3.5) 探针校验：对清单里的套件依赖逐个用宿主 Node import 其主机侧入口——复现 dsh
       启动的加载路径，把「装完才发现解析断裂」拦在重启之前；失败则回滚 + 报错 + 不重启
    4) 成功 → 写 `installed-suite.json` 标记（含通道）+ 系统通知 + `supervisor::restart_by_mode`
    5) 失败 → 回滚 manifest + `status::fail` + 系统通知 + stderr 尾部透传（≤8 行）

    两通道共享同一套安全网（快照/回滚/探针）与同一把 FlowGate 闸锁；互相切换时
    「以最后一次安装为准」——安装器负责清理另一形态的残留（link: ↔ tarball URL、
    pnpm.overrides 只在 release 形态存在）。

    远程模式由 `tray::entry_visible` 在菜单层隐藏（远程实例装不到本地 dsh）。
*/
pub fn install_digital_twin_suite(app: &tauri::AppHandle, channel: SuiteChannel) {
    let state: tauri::State<crate::AppState> = app.state();
    state.restarting.acquire();
    suite_progress(
        app,
        "show",
        &format!("通道：{}——准备中…", channel.label()),
    );

    // 1) 定位安装器：本地通道走用户目录，生产通道拉官方最新
    let (installer_dir, bat_args, suite_root) = match channel {
        SuiteChannel::Local => {
            let root = match crate::suite_path::read() {
                Some(p) => p,
                None => match crate::suite_path::pick(app) {
                    Some(p) => {
                        if let Err(e) = crate::suite_path::write(&p) {
                            if let Some(mut log) = crate::runtime::open_log_append() {
                                use std::io::Write;
                                let _ = writeln!(log, "[数字分身] 路径持久化失败: {e}");
                            }
                        }
                        p
                    }
                    None => {
                        status::set(app, "已取消：未选择数字分身套件目录");
                        suite_progress(app, "hide", "");
                        state.restarting.release();
                        return;
                    }
                },
            };
            (root.clone(), Vec::new(), Some(root))
        }
        SuiteChannel::Release => {
            let dir = runtime::runtime_root().join("suite-installer");
            status::set(app, "正在获取数字分身套件安装器（GitHub）…");
            suite_progress(app, "step", "正在从 GitHub 获取安装器…");
            if let Err(e) = fetch_suite_installer(&dir, &channel.installer_urls()) {
                let msg = format!("获取套件安装器失败（网络不通？）：{e}");
                status::fail(app, &msg);
                suite_progress(app, "fail", &msg);
                notify_digital_twin(app, "数字分身安装失败", &msg);
                state.restarting.release();
                return;
            }
            (dir, vec!["-Release"], None)
        }
    };

    // 2) 校验安装器存在（本地通道：用户选的目录；生产通道：刚拉下来的目录）
    let bat = installer_dir.join("install-all.bat");
    if !bat.is_file() {
        let msg = match channel {
            SuiteChannel::Local => format!(
                "选定的目录不是数字分身套件根（未发现 install-all.bat）：{}",
                installer_dir.display()
            ),
            SuiteChannel::Release => format!(
                "拉取到的安装器不完整（未发现 install-all.bat）：{}",
                installer_dir.display()
            ),
        };
        status::fail(app, &msg);
        suite_progress(app, "fail", &msg);
        notify_digital_twin(app, "数字分身安装失败", &msg);
        state.restarting.release();
        return;
    }

    // 2.5) 行尾规范化：cmd 在 LF-only 批处理上会解析跑飞（详见 normalize_bat_eol 文档）。
    //      生产通道在 fetch 内已做过，此处幂等；本地通道的副本可能来自 Linux 克隆/编辑。
    match normalize_bat_eol(&bat) {
        Ok(true) => {
            if let Some(mut log) = crate::runtime::open_log_append() {
                use std::io::Write;
                let _ = writeln!(
                    log,
                    "[数字分身] 安装器为 LF-only，已就地规范化为 CRLF（cmd 批处理解析需要）"
                );
            }
        }
        Ok(false) => {}
        Err(e) => {
            status::fail(app, &e);
            suite_progress(app, "fail", &e);
            notify_digital_twin(app, "数字分身安装失败", &e);
            state.restarting.release();
            return;
        }
    }

    // 3) 跑 install-all.bat：cmd /C 包装以支持 bat；DSH_HOME 显式注入，避开脚本
    //    探测 %LOCALAPPDATA%\dsh-desktop-app-data\home 与我们的 runtime 路径不一致的隐患。
    //    装前快照：bat 先改 manifest 再跑 pnpm——pnpm 失败时 manifest 已指向未就位的
    //    依赖，下次启动必崩；快照让失败路径能回到装前状态。
    let home = runtime::app_home();
    let backup_dir = runtime::runtime_root().join("suite-install-backup");
    snapshot_web_profile(&home, &backup_dir);
    // 官方安装器需要 pnpm；macOS 无系统 pnpm 时首次自动备一份（corepack 只是安装器
    // 的最后兜底，常备一份后 `dsh plugin` 补装路径也不会再撞 "pnpm not found on PATH"）。
    #[cfg(not(windows))]
    if let Err(e) = ensure_pnpm_available() {
        if let Some(mut log) = crate::runtime::open_log_append() {
            use std::io::Write;
            let _ = writeln!(log, "[warn] pnpm 准备失败（继续，安装器将走 corepack 兜底）: {e}");
        }
    }
    status::set(
        app,
        &format!("正在安装数字分身套件（{} 通道）…", channel.label()),
    );
    suite_progress(
        app,
        "step",
        "正在安装插件（下载 Release/链接本地目录 → pnpm install）…首次可能需要 1-2 分钟",
    );
    let output = match run_suite_installer(&installer_dir, &bat, &bat_args) {
        Ok(o) => o,
        Err(e) => {
            // 失败必须清浮层 + 留日志：早期此分支只发系统通知，浮层永久残留，用户
            // 看到的就是「一直卡在准备中」（2026-09-11 实机事故）。
            if let Some(mut log) = crate::runtime::open_log_append() {
                use std::io::Write;
                let _ = writeln!(log, "[数字分身] {e}");
            }
            status::fail(app, &e);
            suite_progress(app, "fail", &e);
            notify_digital_twin(app, "数字分身安装失败", &e);
            state.restarting.release();
            return;
        }
    };

    // 4) 全量日志落盘——install-all.bat 的 link: 阶段、pnpm install 阶段、junction
    //    修复阶段都可能失败，无 stdout/stderr 留证等于让用户盲调
    if let Some(mut log) = crate::runtime::open_log_append() {
        use std::io::Write;
        let _ = writeln!(
            log,
            "\n[数字分身] install-all.bat 退出码={:?}",
            output.status.code()
        );
        if !output.stdout.is_empty() {
            let _ = writeln!(
                log,
                "--- stdout ---\n{}",
                String::from_utf8_lossy(&output.stdout)
            );
        }
        if !output.stderr.is_empty() {
            let _ = writeln!(
                log,
                "--- stderr ---\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    if !output.status.success() {
        let stdout_text = readable_output(&output.stdout);
        let stderr_text = readable_output(&output.stderr);
        // 取 stderr（空则退 stdout）最后 8 行做错误摘要（太长 status 文本会爆）；
        // 同款"tail N 行"模式在 install_runtime_inner 里用于 npm 错误摘要
        let tail: String = {
            let source = if stderr_text.trim().is_empty() { &stdout_text } else { &stderr_text };
            let lines: Vec<&str> = source.lines().filter(|l| !l.trim().is_empty()).collect();
            let start = lines.len().saturating_sub(8);
            lines[start..].join("\n")
        };
        // bat 先改 manifest 再跑 pnpm：脚本失败时 manifest 可能已指向未就位的
        // 链接（下次启动必崩）——回滚到装前快照，服务留在旧组合上继续跑。
        restore_web_profile(&home, &backup_dir);
        let msg = format!(
            "数字分身安装失败（退出码 {:?}，已回滚 profile 清单）stderr 尾部：{tail}",
            output.status.code()
        );
        status::fail(app, &msg);
        suite_progress(app, "fail", &tail);
        notify_digital_twin(app, "数字分身安装失败", &tail);
        state.restarting.release();
        return;
    }

    // 3.5) 探针校验：对清单里的套件依赖逐个用宿主 Node import 其主机侧入口——复现
    //      dsh 启动的加载路径（link 通道按真实路径解析，缺 @deepseek-ai/<pkg> 会
    //      ERR_MODULE_NOT_FOUND；release 通道则为 pnpm 解出的真实目录）。把
    //      「装完一重启就崩」拦在重启之前：失败则回滚 manifest、不重启、给出精确原因。
    suite_progress(app, "step", "正在校验插件可加载性（防「装完一重启就崩」）…");
    let failures = probe_suite_plugins(&home);
    if !failures.is_empty() {
        restore_web_profile(&home, &backup_dir);
        let msg = format!(
            "数字分身套件安装后校验未通过（已回滚，未重启）：{}",
            failures.join("；")
        );
        status::fail(app, &msg);
        suite_progress(app, "fail", &failures.join("\n"));
        notify_digital_twin(app, "数字分身安装失败", &failures.join(" | "));
        state.restarting.release();
        return;
    }
    suite_progress(app, "step", "插件校验通过，正在重启 DSH 加载套件…");

    // 5) 落 installed-suite.json 标记（diagnostics.rs 后续可读这一项给诊断包）：
    //    - 通道（本地调试 / 生产）——用户看到「改了源码不生效」时先查这里是哪种形态
    //    - 本地调试通道的套件根（让"重装/升级"能默认填好）
    //    - 装好的时间戳
    let marker = crate::runtime::runtime_root().join("installed-suite.json");
    if let Some(parent) = marker.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let channel_name = match channel {
        SuiteChannel::Local => "local",
        SuiteChannel::Release => "release",
    };
    let root_field = match &suite_root {
        Some(p) => format!(
            r#""suite_root":"{}","#,
            p.display().to_string().replace('\\', "\\\\")
        ),
        None => String::new(),
    };
    let payload = format!(
        r#"{{"channel":"{channel_name}",{root_field}"installed_at":{}}}"#,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    );
    if let Err(e) = std::fs::write(&marker, payload) {
        if let Some(mut log) = crate::runtime::open_log_append() {
            use std::io::Write;
            let _ = writeln!(log, "[数字分身] 写 installed-suite.json 失败: {e}");
        }
    }

    status::set(
        app,
        &format!(
            "数字分身套件已安装（{} 通道），正在重启 DSH 让预设物化…",
            channel.label()
        ),
    );
    notify_digital_twin(
        app,
        "数字分身已安装",
        match channel {
            SuiteChannel::Local => "以本地调试形态装入（改源码重启即生效）；DSH 正在重启，约 10-30 秒后就绪",
            SuiteChannel::Release => "以生产形态装入（GitHub Release 构建物）；再点一次本项即可升级到最新 Release。DSH 正在重启，约 10-30 秒后就绪",
        },
    );

    // 6) 重启：让 dsh web 重新加载 11 个插件，物化 digital-twin preset
    //    必须本地模式——远程模式该菜单项已隐藏（tray::entry_visible 守门）。
    //    先释放闸锁再走公开重启流程（restart_by_mode 自己 acquire）：闸锁非重入
    //    （布尔 + Condvar），持锁跨调会自我死锁——首版真实缺陷：装完永远卡在
    //    「正在重启」。与 upgrade_runtime / install_and_start 的编排同款。
    suite_progress(app, "done", "安装完成，正在重启 DSH（约 10-30 秒；期间页面会短暂切到加载页）…");
    state.restarting.release();
    supervisor::restart_by_mode(app);
}

/// 安装进度浮层驱动（页面内 API 由 webview.rs 的 SUITE_PROGRESS_JS 注入）：
/// 壳侧只负责把阶段文案推进去——托盘点击后长时间无可见反馈是用户实测的体验缺口。
/// 页面导航中（导航后文档重建，API 暂不存在）调用是安全的空操作。
fn suite_progress(app: &tauri::AppHandle, method: &str, text: &str) {
    let Some(w) = app.get_webview_window("main") else { return };
    let arg = serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_string());
    let _ = w.eval(&format!(
        "window.__dshSuiteProgress__&&window.__dshSuiteProgress__.{method}({arg})"
    ));
}

/// 子进程输出 → 可读文本。bat 已 `chcp 65001`，正常路径即 UTF-8；若仍出现大量
/// 替换符（历史版 bat / 非 UTF-8 工具链），显式告诉用户"编码异常、详见日志"，
/// 而不是把乱码原样丢进通知（2026-09-10 用户实测：失败提示全乱码看不懂）。
fn readable_output(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes).to_string();
    if text.matches('\u{FFFD}').count() >= 5 {
        format!("（原始输出编码非 UTF-8，以下可能乱码；完整输出见壳日志）\n{text}")
    } else {
        text
    }
}

/// 系统通知统一封装（tauri-plugin-notification 走 app.notification()；
/// 失败仅记日志，不抛——通知是体验优化而非链路必需）。
fn notify_digital_twin(app: &tauri::AppHandle, title: &str, body: &str) {    use tauri_plugin_notification::NotificationExt;
    if let Err(e) = app
        .notification()
        .builder()
        .title(title)
        .body(body)
        .show()
    {
        if let Some(mut log) = crate::runtime::open_log_append() {
            use std::io::Write;
            let _ = writeln!(log, "[数字分身] 系统通知失败: {e}");
        }
    }
}

/* ── D2b 安装安全网：快照/回滚 + 装后解析探针 ──
   背景（2026-09-10 真实事故）：install-all.bat 先改 manifest 再跑 pnpm，pnpm 失败
   时 manifest 已指向未就位的链接 → 下次 dsh 启动必崩；即使脚本成功，junction 链接
   的插件按真实路径解析依赖，其自身 node_modules 缺 @deepseek-ai/<pkg> 时同样在启动时
   ERR_MODULE_NOT_FOUND。两道防线：装前快照（失败回滚）+ 装后探针（拦在重启前）。
   （注：Rust 块注释可嵌套，注释正文里不得出现斜杠星序列——曾因此踩坑。） */

/// web profile 的清单/锁文件路径对（快照与回滚共用同一份清单）。
/// 锁文件位置随 pnpm 布局变化：当前在 `profiles/web/`（profile 自身即项目根，
/// 实测 2026-09-10：`profiles/web/pnpm-lock.yaml` 53KB），历史上在 `profiles/`
/// （工作区根）。两个位置都纳入、不存在的自动跳过——快照必须覆盖真实那一份，
/// 否则回滚只还原 manifest、锁文件仍停在失败态。
fn web_profile_files(home: &std::path::Path) -> [(std::path::PathBuf, &'static str); 3] {
    [
        (home.join("profiles").join("web").join("package.json"), "package.json"),
        (home.join("profiles").join("web").join("pnpm-lock.yaml"), "web-pnpm-lock.yaml"),
        (home.join("profiles").join("pnpm-lock.yaml"), "pnpm-lock.yaml"),
    ]
}

/// 装前快照（覆盖式，单代够用）。任一环节失败只记日志——快照是保险丝不是链路。
fn snapshot_web_profile(home: &std::path::Path, backup: &std::path::Path) {
    if let Err(e) = std::fs::create_dir_all(backup) {
        if let Some(mut log) = crate::runtime::open_log_append() {
            use std::io::Write;
            let _ = writeln!(log, "[数字分身] 快照目录创建失败（回滚不可用）: {e}");
        }
        return;
    }
    for (src, name) in web_profile_files(home) {
        if !src.is_file() {
            continue;
        }
        if let Err(e) = std::fs::copy(&src, backup.join(name)) {
            if let Some(mut log) = crate::runtime::open_log_append() {
                use std::io::Write;
                let _ = writeln!(log, "[数字分身] 快照 {name} 失败（回滚不可用）: {e}");
            }
        }
    }
}

/// 失败回滚：只覆盖快照里存在的文件（装前本就没有的文件不造出来）。
fn restore_web_profile(home: &std::path::Path, backup: &std::path::Path) {
    for (dst, name) in web_profile_files(home) {
        let b = backup.join(name);
        if !b.is_file() {
            continue;
        }
        let outcome = std::fs::copy(&b, &dst);
        if let Some(mut log) = crate::runtime::open_log_append() {
            use std::io::Write;
            match outcome {
                Ok(_) => {
                    let _ = writeln!(log, "[数字分身] 已回滚 {name} 至装前快照");
                }
                Err(e) => {
                    let _ = writeln!(log, "[数字分身] 回滚 {name} 失败: {e}");
                }
            }
        }
    }
}

/// 套件包名判定：安装器的 PLUGINS 清单就是这批（`@dsh-extra/*` 前缀 + `dsh-yuyi`）。
/// 只探测套件自己的包——宿主 harness 包（`@deepseek-ai/*`）由 dsh 自身加载，不在此列。
fn is_suite_package(name: &str) -> bool {
    name.starts_with("@dsh-extra/") || name == "dsh-yuyi"
}

/// 某个套件依赖的安装目录：
/// - `link:` 形态（本地调试通道）→ 链接目标（meta-repo 里的插件目录）；
/// - tarball 形态（生产通道）→ profile 的 node_modules（pnpm 解出的真实目录）。
fn suite_package_dir(home: &std::path::Path, spec: &str, name: &str) -> std::path::PathBuf {
    match spec.strip_prefix("link:") {
        Some(p) => std::path::PathBuf::from(p),
        None => home
            .join("profiles")
            .join("web")
            .join("node_modules")
            .join(name),
    }
}

/// 插件的主机侧入口（dsh 加载构建产物）：exports["."]（字符串或 .default）→
/// main → index.js。浏览器端 "./client" 不在此列——它不经 Node 解析。
fn suite_entry_file(pkg_dir: &std::path::Path) -> Result<std::path::PathBuf, String> {
    let text = std::fs::read_to_string(pkg_dir.join("package.json"))
        .map_err(|e| format!("读 package.json 失败: {e}"))?;
    let v: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("解析 package.json 失败: {e}"))?;
    let entry = v
        .get("exports")
        .and_then(|e| e.get("."))
        .and_then(|e| {
            if let Some(s) = e.as_str() {
                Some(s.to_string())
            } else {
                e.get("default").and_then(|d| d.as_str()).map(str::to_string)
            }
        })
        .or_else(|| v.get("main").and_then(|m| m.as_str()).map(str::to_string))
        .unwrap_or_else(|| "index.js".to_string());
    Ok(pkg_dir.join(entry))
}

/// 探针：用宿主 Node import 插件主机侧入口（与 dsh 启动加载同一条路径）。
/// None=通过；Some=单行失败描述（缺包时附可操作的指引）。
fn probe_suite_plugin(
    node: &std::path::Path,
    name: &str,
    pkg_dir: &std::path::Path,
) -> Option<String> {
    let entry = match suite_entry_file(pkg_dir) {
        Ok(e) => e,
        Err(e) => return Some(format!("{name}: {e}")),
    };
    let uri = format!("file:///{}", entry.display().to_string().replace('\\', "/"));
    // uri 走 argv 而非内联进脚本：路径引号/转义不归我们管；e.code+e.message 首行即结论
    let script = "import(process.argv[1]).then(()=>{},e=>{console.error((e.code??'')+' '+(e.message??''));process.exit(1)})";
    let mut cmd = std::process::Command::new(node);
    cmd.args(["-e", script, &uri]).current_dir(pkg_dir);
    crate::runtime::no_window(&mut cmd);
    match cmd.output() {
        Err(e) => Some(format!("{name}: 探针进程启动失败: {e}")),
        Ok(o) if o.status.success() => None,
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr);
            let first = err.lines().next().unwrap_or("未知错误").trim();
            let mut line = format!("{name}: {first}");
            if first.contains("Cannot find package") {
                line.push_str("（插件目录内解析不到该依赖——请更新套件 install-all.bat（新版自动修复），或到该插件目录补 junction 至宿主 store）");
            }
            Some(line)
        }
    }
}

/// 装后校验：对清单里的套件依赖逐个跑入口探针（两个通道共用）。清单读不到 → 通过。
fn probe_suite_plugins(home: &std::path::Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(home.join("profiles").join("web").join("package.json"))
    else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    let Some(deps) = v.get("dependencies").and_then(|d| d.as_object()) else {
        return Vec::new();
    };
    let targets: Vec<(&String, std::path::PathBuf)> = deps
        .iter()
        .filter(|(name, _)| is_suite_package(name))
        .filter_map(|(name, spec)| {
            let spec = spec.as_str()?;
            Some((name, suite_package_dir(home, spec, name)))
        })
        .collect();
    if targets.is_empty() {
        return Vec::new();
    }
    let node = runtime::node_exe();
    if !node.exists() {
        return vec!["内置 Node 运行时缺失，无法做安装后校验".to_string()];
    }
    targets
        .iter()
        .filter_map(|(name, dir)| probe_suite_plugin(&node, name, dir))
        .collect()
}

/// 生产通道：拉取官方安装器到本地目录（壳不内置副本——套件仓库更新安装器后所有
/// 机器即刻受益，零漂移）。用宿主 Node 做 HTTPS 下载（Node 24 自带 fetch，零新增
/// Rust 依赖），逐个来源尝试（官方 raw → ghfast 镜像），校验嵌入式 JS 标记后落盘。
fn fetch_suite_installer(dir: &std::path::Path, urls: &[String]) -> Result<(), String> {
    if urls.is_empty() {
        return Err("未配置安装器来源".to_string());
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("创建安装器目录失败: {e}"))?;
    let node = runtime::node_exe();
    if !node.exists() {
        return Err("内置 Node 运行时缺失".to_string());
    }
    let out = dir.join("install-all.bat");
    // argv: [1]=输出路径, [2..]=候选 URL。校验 JS-START 标记，避免把 404 页面/代理页当安装器。
    // 注意：失败/成功后都用 process.exitCode 交还控制权、让事件循环自然收干——
    // 紧跟 fetch 调 process.exit() 会触发 libuv 断言（win async.c）崩溃退出码非 0，
    // 明明下载成功的文件会被判失败（2026-09-10 实测）。
    let script = "const fs=require('fs');const out=process.argv[1];const urls=process.argv.slice(2);\
(async()=>{let ok=false;for(const u of urls){try{const r=await fetch(u);if(!r.ok)continue;const t=await r.text();\
if(!t.includes('//==JS-START=='))continue;fs.writeFileSync(out,t);ok=true;break}catch{}}\
process.exitCode=ok?0:2})()";
    let mut cmd = std::process::Command::new(&node);
    // --dns-result-order=ipv4first：Node 的 fetch/undici 默认 IPv6 优先，且不读 Windows
    // 系统代理——受限网络下直连 github.com 会超时 10s（2026-09-10 实测：系统代理已启用
    // 但无 HTTPS_PROXY，加此参数立即 200；PowerShell 因走系统代理一直正常 → 假故障）。
    cmd.arg("--dns-result-order=ipv4first").arg("-e").arg(script).arg(&out);
    for u in urls {
        cmd.arg(u);
    }
    cmd.current_dir(dir);
    crate::runtime::no_window(&mut cmd);
    let output = cmd
        .output()
        .map_err(|e| format!("下载器启动失败: {e}"))?;
    if !output.status.success() || !out.is_file() {
        let err = String::from_utf8_lossy(&output.stderr);
        let first = err.lines().next().unwrap_or("网络不可达").trim();
        return Err(format!("所有来源均不可用（{first}）"));
    }
    // 下载回来的行尾不可信：raw CDN 按 git blob 下发，blob 为 LF 时 cmd 会解析跑飞。
    normalize_bat_eol(&out)?;
    Ok(())
}

/// 把 .bat 的行尾规范化为 CRLF（返回是否发生了改写）。
///
/// 为什么必须：cmd.exe 的批处理解析器在 LF-only 文件上会跑飞——变量展开被粘连成
/// 单个 token、`exit /b` 失效，最终越过脚本末尾执行到 install-all.bat 里的嵌入式
/// JS 文本，表现为满屏「不是内部或外部命令」+ 中文乱码（GBK 被按 UTF-8 读）。
/// 2026-09-10 用户实测的生产通道失败即此：**本地工作区副本是 CRLF（git 检出转换）
/// 所以跑得通，而从 GitHub raw 下载回来是 LF 所以跑不通**。
/// 仓库已用 `.gitattributes` 的 `-text` 让 blob 保留 CRLF；这里再兜一层，
/// 对 CDN 缓存陈旧 / 其他 LF 来源（Linux 上编辑、镜像站）一律免疫。
fn normalize_bat_eol(path: &std::path::Path) -> Result<bool, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("读取安装器失败: {e}"))?;
    if bytes.contains(&b'\r') {
        return Ok(false); // 已含 CR（CRLF 或混合）：保守不动
    }
    let mut out = Vec::with_capacity(bytes.len() + bytes.len() / 16);
    for byte in bytes {
        if byte == b'\n' {
            out.push(b'\r');
        }
        out.push(byte);
    }
    std::fs::write(path, out).map_err(|e| format!("规范化安装器行尾失败: {e}"))?;
    Ok(true)
}

/// 提取官方安装器（install-all.bat）内嵌的 JS 段——与 bat 自身的提取器同语义
/// （按 `//==JS-START==` / `//==JS-END==` 标记切割）。安装器是「bat 壳 + 内嵌 ESM」
/// 的多语言文件：Windows 走 cmd，macOS/Linux 由壳提取后用便携 Node 执行同一份逻辑，
/// 双平台单一实现、零漂移。
///
/// Windows 运行时由 cmd 直接执行整个文件，提取函数仅测试与非 Windows 分支使用。
#[cfg_attr(windows, allow(dead_code))]
fn extract_embedded_js(bat_text: &str) -> Result<&str, String> {
    const START: &str = "//==JS-START==";
    const END: &str = "//==JS-END==";
    let start = bat_text
        .find(START)
        .ok_or_else(|| "安装器缺少 //==JS-START== 标记（非官方安装器？）".to_string())?
        + START.len();
    let end = bat_text
        .find(END)
        .ok_or_else(|| "安装器缺少 //==JS-END== 标记（文件被截断？）".to_string())?;
    if end <= start {
        return Err("安装器 JS 段标记顺序异常".to_string());
    }
    Ok(&bat_text[start..end])
}

/// POSIX cmd 兼容垫片：官方安装器修复 link: 依赖时调用 `cmd /c rmdir <path>` 与
/// `cmd /c mklink /J <link> <target>`（Windows junction 特权）。macOS 没有 cmd，
/// 这里提供只覆盖这两个操作的等价物（删符号链接/空目录、建目录符号链接），
/// 其余命令一律非零退出——最小暴露面。
#[cfg(not(windows))]
const CMD_SHIM_SH: &str = r##"#!/bin/sh
# dsh-desktop POSIX cmd 垫片：仅支持 `cmd /c rmdir <path>` 与 `cmd /c mklink /J <link> <target>`。
[ "$1" = "/c" ] || exit 2
shift
op="$1"
shift
case "$op" in
  rmdir)
    p="$1"
    if [ -L "$p" ]; then rm -f -- "$p"; exit 0; fi
    if [ -d "$p" ]; then rmdir -- "$p" 2>/dev/null; exit $?; fi
    exit 1
    ;;
  mklink)
    [ "$1" = "/J" ] || exit 2
    link="$2"; target="$3"
    mkdir -p -- "$(dirname -- "$link")" 2>/dev/null || true
    ln -s -- "$target" "$link" 2>/dev/null || exit 1
    exit 0
    ;;
  *)
    echo "dsh-desktop cmd shim: unsupported: $op" >&2
    exit 2
    ;;
esac
"##;

#[cfg(not(windows))]
fn write_cmd_shim(dir: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir)?;
    let path = dir.join("cmd");
    std::fs::write(&path, CMD_SHIM_SH)?;
    let mut perms = std::fs::metadata(&path)?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms)
}

/// 跑官方套件安装器（双通道共用）：
/// - Windows：`cmd /c install-all.bat <args>`（原路径，行为不变）；
/// - macOS/Linux：提取内嵌 JS 到临时文件 → 便携 Node 执行。argv 约定与 bat 相同
///   （`<repo-root> [flags]`）。环境对齐 bat 头：DSH_HOME 注入（bat 自带的兜底只探测
///   %LOCALAPPDATA%，macOS 会落到 ~/.dsh 装错家）、ipv4 优先、PATH 前置便携 node 的
///   bin（pnpm/corepack 解析）、NODE_USE_ENV_PROXY 透传，另加 cmd 垫片兜住 local
///   通道的 junction 修复调用。
fn run_suite_installer(
    installer_dir: &std::path::Path,
    bat: &std::path::Path,
    bat_args: &[&str],
) -> Result<std::process::Output, String> {
    let home = runtime::app_home();
    #[cfg(windows)]
    {
        let mut cmd = std::process::Command::new("cmd");
        cmd.args(["/c", "install-all.bat"])
            .args(bat_args)
            .current_dir(installer_dir)
            .env("DSH_HOME", home.display().to_string());
        runtime::no_window(&mut cmd);
        return cmd
            .output()
            .map_err(|e| format!("启动 install-all.bat 失败: {e}"));
    }
    #[cfg(not(windows))]
    {
        let text = std::fs::read_to_string(bat).map_err(|e| format!("读取安装器失败: {e}"))?;
        let js = extract_embedded_js(&text)?;
        let node = runtime::node_exe();
        if !node.exists() {
            return Err("内置 Node 运行时缺失，无法执行套件安装器".to_string());
        }
        let node_bin = node
            .parent()
            .unwrap_or(std::path::Path::new(""))
            .to_path_buf();
        let pid = std::process::id();
        let js_file = std::env::temp_dir().join(format!("dsh-install-all-{pid}.mjs"));
        std::fs::write(&js_file, js).map_err(|e| format!("写出安装器 JS 失败: {e}"))?;
        let shim_dir = std::env::temp_dir().join(format!("dsh-cmd-shim-{pid}"));
        let shim_ok = write_cmd_shim(&shim_dir).is_ok();
        let sys_path = std::env::var("PATH").unwrap_or_default();
        let path = if shim_ok {
            format!("{}:{}:{}", shim_dir.display(), node_bin.display(), sys_path)
        } else {
            format!("{}:{}", node_bin.display(), sys_path)
        };
        let mut cmd = std::process::Command::new(&node);
        cmd.arg(&js_file)
            .arg(installer_dir)
            .args(bat_args)
            .current_dir(installer_dir)
            .env("DSH_HOME", home.display().to_string())
            .env("PATH", path)
            .env("NODE_USE_ENV_PROXY", "1");
        let opts = std::env::var("NODE_OPTIONS").unwrap_or_default();
        if !opts.contains("--dns-result-order") {
            cmd.env(
                "NODE_OPTIONS",
                format!("{opts} --dns-result-order=ipv4first").trim().to_string(),
            );
        }
        runtime::no_window(&mut cmd);
        let out = cmd
            .output()
            .map_err(|e| format!("启动套件安装器失败: {e}"));
        let _ = std::fs::remove_file(&js_file);
        let _ = std::fs::remove_dir_all(&shim_dir);
        out
    }
}

/// POSIX 下确保 pnpm 可用（官方安装器与 `dsh plugin` 都依赖它）。缺失时用便携 npm
/// 装一份到便携运行时（镜像优先，与 dsh 安装同源）——之后 `dsh plugin` 补装路径也
/// 能找到它（`dsh_cli_command` 的 PATH 前置正是便携 node 的 bin 目录）。幂等：
/// PATH 上已有 pnpm 直接返回；失败只报错不抛（安装器自身还有 corepack 兜底）。
#[cfg(not(windows))]
fn ensure_pnpm_available() -> Result<(), String> {
    let node = runtime::node_exe();
    if !node.exists() {
        return Err("内置 Node 运行时缺失".to_string());
    }
    let node_bin = node
        .parent()
        .unwrap_or(std::path::Path::new(""))
        .to_path_buf();
    let path = format!(
        "{}:{}",
        node_bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let has_pnpm = std::process::Command::new("pnpm")
        .arg("--version")
        .env("PATH", &path)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if has_pnpm {
        return Ok(());
    }
    let mut last_err = String::new();
    for registry in npm_registry() {
        let Ok(mut cmd) = npm_command() else {
            return Err("便携 npm 不可用".to_string());
        };
        cmd.args(["install", "-g", "pnpm@latest", "--prefix"])
            .arg(active_root().join("node"))
            .arg(&registry)
            .env("PATH", &path);
        match runtime::no_window(&mut cmd).output() {
            Ok(o) if o.status.success() => {
                if let Some(mut log) = runtime::open_log_append() {
                    use std::io::Write;
                    let _ = writeln!(log, "[数字分身] 已为套件安装准备 pnpm（{registry}）");
                }
                return Ok(());
            }
            Ok(o) => last_err = format!("npm 退出码 {:?}（{registry}）", o.status.code()),
            Err(e) => last_err = format!("npm 启动失败: {e}"),
        }
    }
    Err(format!("pnpm 安装失败：{last_err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 行尾规范化：LF-only 就地改写成 CRLF（cmd 需要）；已是 CRLF 时幂等不动。
    #[test]
    fn normalize_bat_eol_rewrites_only_lf() {
        let dir = std::env::temp_dir().join(format!("dsh-eol-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let lf = dir.join("lf.bat");
        std::fs::write(&lf, b"@echo off\nset RC=\nexit /b %RC%\n").unwrap();
        assert!(normalize_bat_eol(&lf).unwrap(), "LF-only 应被改写");
        let bytes = std::fs::read(&lf).unwrap();
        assert_eq!(bytes.iter().filter(|b| **b == b'\r').count(), 3, "三行都应补上 CR");
        assert!(!normalize_bat_eol(&lf).unwrap(), "已是 CRLF：再调必须幂等");

        let crlf = dir.join("crlf.bat");
        std::fs::write(&crlf, b"@echo off\r\nexit /b 0\r\n").unwrap();
        let before = std::fs::read(&crlf).unwrap();
        assert!(!normalize_bat_eol(&crlf).unwrap(), "CRLF 不应被改动");
        assert_eq!(std::fs::read(&crlf).unwrap(), before, "CRLF 必须逐字节原样保留");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 乱码降级：正常 UTF-8 原样返回；大量替换符（非 UTF-8 工具链输出）时
    /// 显式提示编码异常，而不是把乱码当错误原因丢给用户（2026-09-10 实测）。
    #[test]
    fn readable_output_flags_non_utf8() {
        assert_eq!(readable_output("安装成功".as_bytes()), "安装成功");
        let gbk_like: Vec<u8> = vec![0xB0, 0xA1, 0xB0, 0xA1, 0xB0, 0xA1, 0xB0, 0xA1, 0xB0, 0xA1, 0xB0, 0xA1];
        let text = readable_output(&gbk_like);
        assert!(text.contains("编码非 UTF-8"), "非 UTF-8 输出应显式标注: {text}");
    }

    /// 套件包名判定 + 双通道目录解析：
    /// link:（本地调试）→ 链接目标；tarball URL（生产）→ profile node_modules。
    #[test]
    fn suite_package_resolution_covers_both_channels() {
        assert!(is_suite_package("@dsh-extra/dsh-memory"));
        assert!(is_suite_package("dsh-yuyi"));
        assert!(!is_suite_package("@deepseek-ai/dsh-tools"));
        assert!(!is_suite_package("esbuild"));

        let home = std::path::PathBuf::from("C:/home");
        assert_eq!(
            suite_package_dir(&home, "link:E:/code/nodejs/dsh/dsh-memory", "@dsh-extra/dsh-memory"),
            std::path::PathBuf::from("E:/code/nodejs/dsh/dsh-memory")
        );
        assert_eq!(
            suite_package_dir(
                &home,
                "https://github.com/lomehong/dsh-memory/releases/latest/download/dsh-memory-latest.tgz?release=v0.1.1",
                "@dsh-extra/dsh-memory"
            ),
            home.join("profiles").join("web").join("node_modules").join("@dsh-extra/dsh-memory")
        );
    }

    /// 生产通道安装器来源：两条候选（官方 raw + 镜像）顺序稳定，且都带缓存击穿参数
    /// （raw 的 CDN 缓存会让刚修好的安装器拉不到，`?cb=` 强制回源）。
    #[test]
    fn release_channel_has_installer_sources() {
        let urls = SuiteChannel::Release.installer_urls();
        assert_eq!(urls.len(), 2, "应有官方 raw 与镜像两条来源");
        assert!(urls[0].starts_with("https://raw.githubusercontent.com/lomehong/digital-twin/"));
        assert!(urls[1].contains("ghfast.top"));
        assert!(urls.iter().all(|u| u.contains("?cb=")), "两条来源都必须带缓存击穿参数: {urls:?}");
        assert!(SuiteChannel::Local.installer_urls().is_empty(), "本地通道用用户目录里的安装器");
        assert_eq!(SuiteChannel::Local.label(), "本地调试");
        assert!(SuiteChannel::Release.label().contains("生产"));
        // 空来源必须明确失败而不是静默跳过
        assert!(fetch_suite_installer(std::path::Path::new("."), &[]).is_err());
    }

    /// 官方安装器是「bat 壳 + 内嵌 ESM」：提取器必须与 bat 自身的标记切割同语义，
    /// 缺失/乱序标记要明确报错（双平台共用同一份官方实现的关键装载点）。
    #[test]
    fn extract_embedded_js_slices_between_markers() {
        let sample = "cmd header\r\n//==JS-START==\nconsole.log('hi');\n//==JS-END==\r\ncmd tail";
        assert_eq!(extract_embedded_js(sample).unwrap(), "\nconsole.log('hi');\n");
        assert!(extract_embedded_js("no markers at all").is_err());
        assert!(extract_embedded_js("//==JS-END== x //==JS-START==").is_err());
    }

    /// cmd 垫片契约（仅 POSIX）：只覆盖 rmdir 与 mklink /J 两个操作、其余非零退出——
    /// 官方安装器 local 通道的 junction 修复依赖这两个调用在 macOS 上可用。
    #[cfg(not(windows))]
    #[test]
    fn cmd_shim_covers_only_rmdir_and_mklink() {
        let s = CMD_SHIM_SH;
        assert!(s.contains("\"/c\""), "缺少 /c 解析");
        assert!(s.contains("rmdir)"), "缺少 rmdir 分支");
        assert!(s.contains("mklink)"), "缺少 mklink 分支");
        assert!(s.contains("ln -s"), "mklink 未映射为符号链接");
        assert!(s.contains("exit 2"), "未覆盖的命令必须非零退出");
    }

    /// 跨平台装载契约（仅 POSIX）：run_suite_installer 必须提取内嵌 JS、按 bat 约定
    /// 传 argv（`<installer-dir> [flags]`）、注入 DSH_HOME、并把 cmd 垫片放进 PATH——
    /// 用假安装器把观察到的 argv/env 落盘断言，不碰任何真实状态。
    #[cfg(not(windows))]
    #[test]
    fn run_suite_installer_executes_embedded_js_with_bat_conventions() {
        if !runtime::node_exe().exists() {
            return; // 无便携运行时的裸环境跳过（提取逻辑已有独立单测）
        }
        let dir = std::env::temp_dir().join(format!("dsh-runner-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("observed.json");
        let js = format!(
            "import {{ writeFileSync }} from 'node:fs';\nwriteFileSync({out:?}, JSON.stringify({{ root: process.argv[2], flags: process.argv.slice(3), home: process.env.DSH_HOME, path: process.env.PATH }}));\n",
            out = out.display().to_string()
        );
        std::fs::write(
            dir.join("install-all.bat"),
            format!("@echo off\r\nrem header\r\n//==JS-START==\n{js}//==JS-END==\r\nrem tail\r\n"),
        )
        .unwrap();
        let result = run_suite_installer(&dir, &dir.join("install-all.bat"), &["-Release"]);
        let o = result.expect("runner 应能启动安装器");
        assert!(o.status.success(), "stderr: {}", String::from_utf8_lossy(&o.stderr));
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
        assert_eq!(v["root"].as_str().unwrap(), dir.to_str().unwrap());
        assert_eq!(v["flags"], serde_json::json!(["-Release"]));
        assert!(v["home"].as_str().unwrap().ends_with("home"));
        assert!(
            v["path"].as_str().unwrap().contains("dsh-cmd-shim-"),
            "cmd 垫片未进入 PATH"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 入口解析：exports["."] 字符串 / 对象.default / main / 全无 → index.js。
    #[test]
    fn suite_entry_file_resolution_variants() {
        let dir = std::env::temp_dir()
            .join(format!("dsh-entry-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let write = |text: &str| std::fs::write(dir.join("package.json"), text).unwrap();

        write(r#"{"main":"lib/index.js"}"#);
        assert_eq!(suite_entry_file(&dir).unwrap(), dir.join("lib/index.js"));

        write(r#"{"exports":{".":"./lib/main.js"}}"#);
        assert_eq!(suite_entry_file(&dir).unwrap(), dir.join("./lib/main.js"));

        write(r#"{"exports":{".":{"types":"./lib/types/index.d.ts","default":"./lib/index.js"}}}"#);
        assert_eq!(suite_entry_file(&dir).unwrap(), dir.join("lib/index.js"));

        write(r#"{"exports":{".":{"types":"./lib/types/index.d.ts"}},"main":"lib/fallback.js"}"#);
        assert_eq!(suite_entry_file(&dir).unwrap(), dir.join("lib/fallback.js"));

        write(r#"{"name":"no-entry"}"#);
        assert_eq!(suite_entry_file(&dir).unwrap(), dir.join("index.js"));

        // 损坏 JSON → Err 而非 panic
        write("{ not json");
        assert!(suite_entry_file(&dir).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 快照/回滚回环：快照后改动原件，回滚必须逐字恢复；装前缺失的文件回滚不造。
    #[test]
    fn snapshot_and_restore_roundtrip() {
        let stamp = format!("dsh-snap-test-{}", std::process::id());
        let home = std::env::temp_dir().join(&stamp).join("home");
        let backup = std::env::temp_dir().join(&stamp).join("backup");
        let profile_dir = home.join("profiles").join("web");
        std::fs::create_dir_all(&profile_dir).unwrap();
        std::fs::create_dir_all(home.join("profiles")).unwrap();
        std::fs::write(profile_dir.join("package.json"), "{\"v\":1}").unwrap();
        std::fs::write(home.join("profiles").join("pnpm-lock.yaml"), "lockfile: true").unwrap();

        snapshot_web_profile(&home, &backup);
        // 装后状态：原件被改动
        std::fs::write(profile_dir.join("package.json"), "{\"v\":2,\"poisoned\":true}").unwrap();
        std::fs::write(home.join("profiles").join("pnpm-lock.yaml"), "lockfile: changed").unwrap();

        restore_web_profile(&home, &backup);
        assert_eq!(
            std::fs::read_to_string(profile_dir.join("package.json")).unwrap(),
            "{\"v\":1}"
        );
        assert_eq!(
            std::fs::read_to_string(home.join("profiles").join("pnpm-lock.yaml")).unwrap(),
            "lockfile: true"
        );

        // 装前缺失 lock 的场景：回滚不得把它造出来
        let stamp2 = format!("dsh-snap-test2-{}", std::process::id());
        let home2 = std::env::temp_dir().join(&stamp2).join("home");
        let backup2 = std::env::temp_dir().join(&stamp2).join("backup");
        std::fs::create_dir_all(home2.join("profiles").join("web")).unwrap();
        std::fs::write(home2.join("profiles/web/package.json"), "{}").unwrap();
        snapshot_web_profile(&home2, &backup2);
        restore_web_profile(&home2, &backup2);
        assert!(!home2.join("profiles/pnpm-lock.yaml").exists());

        let _ = std::fs::remove_dir_all(std::env::temp_dir().join(&stamp));
        let _ = std::fs::remove_dir_all(std::env::temp_dir().join(&stamp2));
    }

    /// POSIX 权限自愈：644 → 600，600/缺失不动作。（win 开发机无 POSIX 位，测试随 CI mac 跑）
    #[cfg(unix)]
    #[test]
    fn enforce_owner_mode_heals_group_readable_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("dsh-perm-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".credentials.yaml");
        std::fs::write(&path, "deepseek:\n  apiKey: sk-test\n").unwrap();
        let mut loose = std::fs::metadata(&path).unwrap().permissions();
        loose.set_mode(0o644);
        std::fs::set_permissions(&path, loose).unwrap();

        assert!(enforce_owner_mode(&path));
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        // 已合规 → 不再动作
        assert!(!enforce_owner_mode(&path));
        // 缺失文件 → 不动作
        assert!(!enforce_owner_mode(&dir.join("nope.yaml")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// keep-alive 补丁核心：锚点注入一次、幂等、锚点缺失/多命中拒绝。
    #[test]
    fn patch_keepalive_contents_injects_once_and_is_idempotent() {
        let src = "this.server = createServer((req, res) => {});\n\t\t\tthis.server.listen(this.config.port, this.config.host, () => {";
        let patched = patch_keepalive_contents(src).expect("合法锚点应注入成功");
        assert!(patched.contains("keepAliveTimeout = 65000"));
        assert!(patched.contains("headersTimeout = 66000"));
        // 只在 listen 前注入一次，原锚点语句保留
        assert_eq!(patched.matches("this.server.listen(this.config.port").count(), 1);
        // 幂等：已打补丁的内容再处理 → None
        assert!(patch_keepalive_contents(&patched).is_none());
        // 锚点缺失/多命中 → None（上游改版容错，绝不盲改）
        assert!(patch_keepalive_contents("no anchor here").is_none());
        let two = format!("{src}\n{src}");
        assert!(patch_keepalive_contents(&two).is_none());
    }

    #[test]
    fn version_triple_strips_prerelease_and_build_suffixes() {
        assert_eq!(version_triple("0.1.1-rc.2"), Some((0, 1, 1)));
        assert_eq!(version_triple("0.1.2-alpha.1"), Some((0, 1, 2)));
        assert_eq!(version_triple("0.1.2"), Some((0, 1, 2)));
        assert_eq!(version_triple("1.2.3+build.5"), Some((1, 2, 3)));
        assert_eq!(version_triple("junk"), None);
    }

    /* ── 预发布感知版本比较（升级通道择新） ── */
    #[test]
    fn cmp_versions_orders_prereleases_semver_style() {
        use std::cmp::Ordering::*;
        // 真实故障方向：latest(rc.1) 反超 alpha 通道钉死的 alpha.5
        assert_eq!(cmp_versions("0.1.2-rc.1", "0.1.2-alpha.5"), Greater);
        assert_eq!(cmp_versions("0.1.2-alpha.5", "0.1.2-rc.1"), Less);
        // 正式版 > 同三元组任何预发布
        assert_eq!(cmp_versions("0.1.2", "0.1.2-rc.1"), Greater);
        assert_eq!(cmp_versions("0.1.2-rc.1", "0.1.2"), Less);
        // 三元组优先于预发布段
        assert_eq!(cmp_versions("0.1.3-alpha.1", "0.1.2-rc.9"), Greater);
        // rc 字母序 > alpha；同段数字按数值
        assert_eq!(cmp_versions("1.0.0-beta.2", "1.0.0-alpha.10"), Greater);
        assert_eq!(cmp_versions("1.0.0-alpha.10", "1.0.0-alpha.9"), Greater);
        // 相等与 build 元数据忽略
        assert_eq!(cmp_versions("1.2.3-rc.1", "1.2.3-rc.1"), Equal);
        assert_eq!(cmp_versions("1.2.3+build.7", "1.2.3"), Equal);
    }

    /* ── npm allow-scripts 拦截清单解析 ── */
    #[test]
    fn parses_allow_scripts_skipped_packages_from_real_warning_block() {
        let output = "\
npm warn deprecated node-domexception@1.0.0: Use your platform's native DOMException instead\n\
\n\
added 520 packages in 28s\n\
\n\
npm warn allow-scripts 5 packages have install scripts not yet covered by allowScripts:\n\
npm warn allow-scripts   @deepseek-ai/dsh-subprocess-local@0.1.2-rc.1 (postinstall: node scripts/ensure-spawn-helper.mjs)\n\
npm warn allow-scripts   koffi@3.2.1 (install: node ./cnoke.cjs -P . -D src/koffi --prebuild --release)\n\
npm warn allow-scripts   node-pty@1.2.0-beta.15 (install: node scripts/prebuild.js || node-gyp rebuild; postinstall: node scripts/post-install.js)\n\
npm warn allow-scripts   @google/genai@1.52.0 (preinstall: echo 'preinstall: no-op')\n\
npm warn allow-scripts   protobufjs@7.6.6 (postinstall: node scripts/postinstall)\n\
npm warn allow-scripts\n\
npm warn allow-scripts Run `npm install -g --allow-scripts=@deepseek-ai/dsh-subprocess-local,koffi,node-pty,@google/genai,protobufjs` to allow these scripts once, or `npm config set allow-scripts=… --location=user` to allow them for all global installs.\n\
";
        assert_eq!(
            parse_allow_scripts_skipped(output),
            vec![
                "@deepseek-ai/dsh-subprocess-local",
                "koffi",
                "node-pty",
                "@google/genai",
                "protobufjs"
            ]
        );
    }

    #[test]
    fn allow_scripts_parse_ignores_unrelated_output_and_dedups() {
        assert!(parse_allow_scripts_skipped("added 5 packages\nnpm warn cleanup foo").is_empty());
        // 同包出现两次只留一份（npm install 与 rebuild 输出拼接场景）
        let twice = "npm warn allow-scripts   koffi@3.2.1 (install: x)\n\
                     npm warn allow-scripts   koffi@3.2.1 (install: x)\n";
        assert_eq!(parse_allow_scripts_skipped(twice), vec!["koffi"]);
    }

    #[test]
    fn guard_blocks_next_minor_line_allows_current() {
        // 预发布段按其所属三元组参与比较：0.1.6-alpha 起视为需要壳配套适配——必须拦
        assert!(version_triple("0.1.6-alpha.1").unwrap() > DSH_MAX_ADAPTED);
        assert!(version_triple("0.1.6").unwrap() > DSH_MAX_ADAPTED);
        assert!(version_triple("0.2.0").unwrap() > DSH_MAX_ADAPTED);
        // 0.1.5 系列（含 alpha/正式）≤ 当前适配线 (0,1,5)：放行（2026-09-08 多角色
        // 评估零必须改动后放行；session v3 迁移单向是已知取舍）
        assert!(version_triple("0.1.5-alpha.1").unwrap() <= DSH_MAX_ADAPTED);
        assert!(version_triple("0.1.5").unwrap() <= DSH_MAX_ADAPTED);
        // 0.1.3/0.1.4 历史线同样放行（评估确认无破坏面）
        assert!(version_triple("0.1.3-alpha.2").unwrap() <= DSH_MAX_ADAPTED);
        assert!(version_triple("0.1.4").unwrap() <= DSH_MAX_ADAPTED);
        // 0.1.2 系列（含 alpha/rc/正式）放行
        assert!(version_triple("0.1.2-alpha.2").unwrap() <= DSH_MAX_ADAPTED);
        assert!(version_triple("0.1.2-rc.1").unwrap() <= DSH_MAX_ADAPTED);
        assert!(version_triple("0.1.2").unwrap() <= DSH_MAX_ADAPTED);
        // 旧 0.1.1.x 也放行
        assert!(version_triple("0.1.1-rc.3").unwrap() <= DSH_MAX_ADAPTED);
        assert!(version_triple("0.1.1").unwrap() <= DSH_MAX_ADAPTED);
    }
}
