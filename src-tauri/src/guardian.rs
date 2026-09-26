//! 壳内置守护 Agent（guardian）：独立于 dsh 运行时的稳定保障、问题收集与自愈处置。
//!
//! 三层架构（设计文档：docs/plans/2026-09-26-guardian-agent-design.md）：
//! 1. 采集层：周期探活服务 origin（本地/远程一视同仁——远程模式的 origin 是本地反代）
//!    + 增量 tail 壳日志（规则表逐行分类）+ 接管 watch_child 终态（自动重启放弃后分析）。
//! 2. 决策层：规则表命中 → 白名单修复动作；规则未命中且配置了 LLM → 证据脱敏后交
//!    OpenAI 兼容接口出结构化诊断，**动作必须命中白名单才可执行**（LLM 永远不能
//!    创造新动作）。
//! 3. 执行层：动作全部经 FlowGate 与用户操作/启动流程串行；全自动模式带防风暴
//!    安全阀（同类修复最小间隔、连续失败冷却）；修复后探活验证才记 resolved。
//!
//! 反馈：问题台账（runtime_root/guardian/issues.json，上限 200 条）+ 通知中心摘要
//! + 托盘角标 + 守护报告窗（ui/guardian.html）。日志前缀 `[守护]`。
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;
use tauri::Manager;

use crate::notifications;
use crate::readiness;
use crate::runtime;

/* ── 常量 ── */

/// 巡检节拍（秒）。
const TICK_SECS: u64 = 10;
/// 首次巡检延迟：避开启动流程（FlowGate 也会拦，双保险）。
const FIRST_TICK_SECS: u64 = 30;
/// 探活连续失败多少次才立案（3 次 ≈ 30s，容忍服务短暂卡顿）。
const PROBE_FAIL_THRESHOLD: u32 = 3;
/// 同类问题立案去抖（秒）：防止一条刷屏错误立一堆案。
const DEBOUNCE_SECS: u64 = 600;
/// 同类修复最小间隔（秒）：全自动模式的防风暴底线。
const FIX_MIN_INTERVAL_SECS: u64 = 1800;
/// 高风险动作（官方源重装）最小间隔：比普通修复严一个量级。
const HIGH_RISK_INTERVAL_SECS: u64 = 6 * 3600;
/// 同类连续失败多少次后进入冷却（冷却期只记录建议不再动手）。
const FAIL_STREAK_BLOCK: u32 = 3;
/// 冷却时长（秒）。
const BLOCK_COOLDOWN_SECS: u64 = 1800;
/// 修复后验证超时（秒）：重启流程自身有界（URL 等待 180s），验证给足余量。
const VERIFY_TIMEOUT_SECS: u64 = 240;
/// 台账容量：超出丢最旧。
const LEDGER_CAP: usize = 200;
/// 证据/单条文本截断（字符）。
const EVIDENCE_MAX_CHARS: usize = 2048;
/// LLM 请求超时（秒，透传 curl --max-time）。
const LLM_TIMEOUT_SECS: u64 = 25;

/* ── 配置（runtime_root/guardian.json，与 launcher.json 分离便于 API key 隔离） ── */

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct LlmCfg {
    #[serde(default)]
    pub enabled: bool,
    /// OpenAI 兼容根地址（不含 /chat/completions），如 https://open.bigmodel.cn/api/paas/v4
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub api_key: String,
}

impl Default for LlmCfg {
    fn default() -> Self {
        Self { enabled: false, base_url: String::new(), model: String::new(), api_key: String::new() }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct GuardianCfg {
    /// 守护总开关（关闭 = 纯观察都不做，回到没有 Agent 的壳）。
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 全自动修复开关（默认关 = 半自动：检测到异常仅提醒，用户在守护报告里
    /// 决定是否修复——良性问题不影响使用，不该自动折腾）。
    #[serde(default)]
    pub auto_fix: bool,
    #[serde(default)]
    pub llm: LlmCfg,
}

fn default_true() -> bool {
    true
}

impl Default for GuardianCfg {
    fn default() -> Self {
        Self { enabled: true, auto_fix: false, llm: LlmCfg::default() }
    }
}

pub fn config_path() -> PathBuf {
    runtime::runtime_root().join("guardian.json")
}

/// 读取配置；缺失/损坏一律回退默认（Agent 绝不因配置问题阻断启动）。
pub fn config() -> GuardianCfg {
    std::fs::read_to_string(config_path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn save_config(cfg: &GuardianCfg) -> Result<(), String> {
    atomic_write_json(&config_path(), cfg)
}

/* ── 修复动作白名单 ── */

/// 白名单动作：LLM 与规则表只能引用，不能创造。执行方式见 perform()。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// 重启服务（远程模式 = 重连远程实例）——最常用的万能动作。
    RestartService,
    /// 清掉固定端口配置改回随机端口后重启（EADDRINUSE 专用）。
    ClearFixedPort,
    /// 补装 profile 插件（FlowGate 内执行）。
    RefreshProfilePlugins,
    /// 修复/重装便携运行时并续跑启动序列（自管闸锁）。
    RepairRuntime,
    /// 官方源重装 DSH（高风险：耗时最长，冷却间隔也最长）。
    ReinstallOfficial,
}

impl Action {
    pub fn as_str(&self) -> &'static str {
        match self {
            Action::RestartService => "RestartService",
            Action::ClearFixedPort => "ClearFixedPort",
            Action::RefreshProfilePlugins => "RefreshProfilePlugins",
            Action::RepairRuntime => "RepairRuntime",
            Action::ReinstallOfficial => "ReinstallOfficial",
        }
    }

    pub fn parse(s: &str) -> Option<Action> {
        Some(match s {
            "RestartService" => Action::RestartService,
            "ClearFixedPort" => Action::ClearFixedPort,
            "RefreshProfilePlugins" => Action::RefreshProfilePlugins,
            "RepairRuntime" => Action::RepairRuntime,
            "ReinstallOfficial" => Action::ReinstallOfficial,
            _ => return None,
        })
    }

    fn high_risk(&self) -> bool {
        matches!(self, Action::ReinstallOfficial)
    }
}

/* ── 问题台账 ── */

/// 台账条目 outcome 取值：pending（待处置）→ fixing（修复中）→ resolved / failed；
/// 无动作或仅建议时停在 pending（advice 文本带建议）。
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Issue {
    pub id: u64,
    /// unix 秒。
    pub at: u64,
    pub category: String,
    /// "warn" | "error"
    pub severity: String,
    /// "probe" | "log" | "supervisor"
    pub source: String,
    pub evidence: String,
    pub diagnosis: String,
    /// 白名单动作名（None = 仅诊断建议）。
    pub action: Option<String>,
    pub outcome: String,
    /// LLM/规则给出的补充建议（无需动作时用户可读）。
    #[serde(default)]
    pub advice: String,
    pub resolved_at: Option<u64>,
}

fn ledger_dir() -> PathBuf {
    runtime::runtime_root().join("guardian")
}

fn ledger_path() -> PathBuf {
    ledger_dir().join("issues.json")
}

static LEDGER: Mutex<Vec<Issue>> = Mutex::new(Vec::new());

/// 纯函数便于单测：插入新案（最新在前）并截断到容量。
fn ledger_insert(ledger: &mut Vec<Issue>, issue: Issue) {
    ledger.insert(0, issue);
    if ledger.len() > LEDGER_CAP {
        ledger.truncate(LEDGER_CAP);
    }
}

/// 纯函数便于单测：下一可用 id（历史最大 + 1，台账被清空后从 1 重新计）。
fn ledger_next_id(ledger: &[Issue]) -> u64 {
    ledger.iter().map(|i| i.id).max().unwrap_or(0) + 1
}

/// 纯函数便于单测：是否已有同类别未决（pending/fixing）条目——有则不重复立案。
fn ledger_has_open(ledger: &[Issue], category: &str) -> bool {
    ledger
        .iter()
        .any(|i| i.category == category && matches!(i.outcome.as_str(), "pending" | "fixing"))
}

/// 纯函数便于单测：同类别 warn 级问题近期（24h）已解决过 → 不再重复立案。
/// 良性永久性告警（如 skipping bundle）每次服务启动都会打一行，半自动模式下
/// 每次都提醒就是骚扰；error 级不受此限（真故障复发必须再报）。
fn ledger_recently_resolved(ledger: &[Issue], category: &str, now: u64) -> bool {
    const RESOLVED_SUPPRESS_SECS: u64 = 24 * 3600;
    ledger.iter().any(|i| {
        i.category == category
            && i.outcome == "resolved"
            && i.resolved_at.is_some_and(|t| now.saturating_sub(t) < RESOLVED_SUPPRESS_SECS)
    })
}

fn load_ledger() -> Vec<Issue> {
    std::fs::read_to_string(ledger_path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_ledger(ledger: &[Issue]) -> Result<(), String> {
    let path = ledger_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(ledger).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
}

/// 进程内台账与磁盘同步初始化（setup 时调用一次）。
fn init_ledger() {
    let mut l = LEDGER.lock().unwrap();
    *l = load_ledger();
}

/* ── 运行态（内存） ── */

#[derive(Default)]
struct FixStat {
    last_at: u64,
    fail_streak: u32,
    blocked_until: u64,
}

#[derive(Default)]
struct Rt {
    probe_fail_streak: u32,
    /// service_unreachable 的未决案 id：恢复可达时自动销案。
    open_unreachable: Option<u64>,
    /// category -> 最近立案时刻（去抖）。
    debounce: HashMap<String, u64>,
    /// category -> 修复频率/冷却状态。
    fix_stat: HashMap<String, FixStat>,
    /// 日志 tail 偏移与半行缓冲。
    log_offset: u64,
    log_partial: String,
    last_tick: Option<u64>,
}

static RT: std::sync::OnceLock<Mutex<Rt>> = std::sync::OnceLock::new();

fn rt() -> &'static Mutex<Rt> {
    RT.get_or_init(|| Mutex::new(Rt::default()))
}

/* ── 安全阀（纯函数便于单测） ── */

/// 同类修复是否放行：冷却期拒绝、间隔未到拒绝。返回 Err(原因) 时动作降级为建议。
fn fix_allowed(stat: Option<&FixStat>, action: Action, now: u64) -> Result<(), &'static str> {
    if let Some(s) = stat {
        if s.blocked_until > now {
            return Err("连续失败冷却中");
        }
        let min = if action.high_risk() { HIGH_RISK_INTERVAL_SECS } else { FIX_MIN_INTERVAL_SECS };
        if now.saturating_sub(s.last_at) < min {
            return Err("同类修复间隔未到");
        }
    }
    Ok(())
}

/// 修复失败后更新统计；返回 true 表示本次失败触发了冷却升级。
fn fix_record_failure(stat: &mut FixStat, now: u64) -> bool {
    stat.last_at = now;
    stat.fail_streak = stat.fail_streak.saturating_add(1);
    if stat.fail_streak >= FAIL_STREAK_BLOCK {
        stat.blocked_until = now + BLOCK_COOLDOWN_SECS;
        stat.fail_streak = 0;
        return true;
    }
    false
}

/* ── 规则表（纯函数便于单测） ── */

pub struct RuleHit {
    pub category: &'static str,
    pub severity: &'static str,
    pub diagnosis: String,
    pub action: Option<Action>,
    pub advice: String,
}

/// 单行日志分类。命中即返回；顺序即优先级（越具体的越靠前）。
pub fn classify_line(line: &str) -> Option<RuleHit> {
    // Node.js 致命横幅（复用 supervisor 启动期同款判定）
    if let Some(banner) = crate::supervisor::crash_banner(line) {
        return Some(RuleHit {
            category: "node_crash",
            severity: "error",
            diagnosis: format!("服务进程崩溃（{banner}）"),
            action: Some(Action::RestartService),
            advice: String::new(),
        });
    }
    // profile bundle 失联：核心更新清空/迁移丢失 → 补装 profile 插件。
    // 「skipping」前缀 = dsh 主动跳过继续跑（非致命），降级 warn 避免错误级噪音
    if line.contains("cannot resolve profile bundle") {
        let skipping = line.contains("skipping profile bundle");
        return Some(RuleHit {
            category: "profile_bundle",
            severity: if skipping { "warn" } else { "error" },
            diagnosis: if skipping {
                "profile 插件 bundle 失联，已跳过加载（服务可启动，已自动补装）".into()
            } else {
                "profile 插件 bundle 无法解析（目录失联或安装被打断）".into()
            },
            action: Some(Action::RefreshProfilePlugins),
            advice: String::new(),
        });
    }
    // 本地链接插件缺依赖：壳修不了（要去插件源码仓库装依赖），只给可操作提示
    if let Some(hint) = crate::supervisor::local_plugin_hint(line) {
        return Some(RuleHit {
            category: "plugin_dep_missing",
            severity: "error",
            diagnosis: "本地链接插件缺依赖".into(),
            action: None,
            advice: hint,
        });
    }
    if line.contains("MODULE_NOT_FOUND") || line.contains("Cannot find module") {
        return Some(RuleHit {
            category: "module_missing",
            severity: "error",
            diagnosis: "Node 模块缺失（运行时或插件安装不完整）".into(),
            action: Some(Action::RefreshProfilePlugins),
            advice: String::new(),
        });
    }
    if line.contains("EADDRINUSE") {
        return Some(RuleHit {
            category: "port_in_use",
            severity: "error",
            diagnosis: "固定端口被占用，服务绑定失败".into(),
            action: Some(Action::ClearFixedPort),
            advice: "已回退随机端口（设置里的固定端口配置被清除）".into(),
        });
    }
    // 事件流鉴权失败：401 必须独立成词（前后不能是数字）——端口号 54014 含 "401"
    // 子串的误报教训（2026-09-26 实机首日）
    if line.contains("[events]") && contains_standalone_401(line) {
        return Some(RuleHit {
            category: "auth_401",
            severity: "warn",
            diagnosis: "事件流鉴权失败（凭证失效），重启服务重新换证".into(),
            action: Some(Action::RestartService),
            advice: String::new(),
        });
    }
    if line.contains("npm ERR!") {
        return Some(RuleHit {
            category: "npm_error",
            severity: "warn",
            diagnosis: "npm 安装/修复流程报错（网络或包源异常）".into(),
            action: Some(Action::RepairRuntime),
            advice: String::new(),
        });
    }
    None
}

/// 多行文本分类（watch_child 终态的错误串是拼了尾环的多行文本）。
pub fn classify_text(text: &str) -> Option<RuleHit> {
    text.lines().find_map(classify_line)
}

/// 纯函数便于单测：是否存在独立成词的 "401"（前后相邻字符都不是 ASCII 数字）。
/// 防止端口号 54014 / 计数值 14012 这类子串触发鉴权失败误报。
fn contains_standalone_401(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut from = 0;
    while let Some(rel) = s[from..].find("401") {
        let i = from + rel;
        let before_ok = i == 0 || !bytes[i - 1].is_ascii_digit();
        let after = i + 3;
        let after_ok = after >= bytes.len() || !bytes[after].is_ascii_digit();
        if before_ok && after_ok {
            return true;
        }
        from = i + 1;
    }
    false
}

/* ── 立案 / 销案 ── */

fn log_line(msg: &str) {
    if let Some(mut f) = runtime::open_log_append() {
        let _ = writeln!(f, "[守护] {msg}");
    }
}

/// 立案（去抖与同类别未决检查由调用方完成）：写台账 + 通知中心 + 托盘角标。
fn open_issue(
    app: &tauri::AppHandle,
    category: &str,
    severity: &str,
    source: &str,
    evidence: &str,
    diagnosis: &str,
    action: Option<Action>,
    advice: &str,
) -> u64 {
    let now = runtime::unix_now();
    let mut ledger = LEDGER.lock().unwrap();
    let id = ledger_next_id(&ledger);
    ledger_insert(
        &mut ledger,
        Issue {
            id,
            at: now,
            category: category.to_string(),
            severity: severity.to_string(),
            source: source.to_string(),
            evidence: truncate_chars(evidence, EVIDENCE_MAX_CHARS),
            diagnosis: diagnosis.to_string(),
            action: action.map(|a| a.as_str().to_string()),
            outcome: "pending".into(),
            advice: advice.to_string(),
            resolved_at: None,
        },
    );
    let snapshot = ledger.clone();
    drop(ledger);
    if let Err(e) = save_ledger(&snapshot) {
        log_line(&format!("台账落盘失败: {e}"));
    }
    // 完整提醒链：历史 + OS 弹泡 + 任务栏闪烁 + 角标（半自动模式下立案提醒即主反馈）
    notifications::present(
        app,
        "守护 Agent",
        &format!("[{}] {}", severity_zh(severity), diagnosis),
    );
    log_line(&format!("立案 #{id} [{category}] {diagnosis}"));
    id
}

/// 更新 outcome 并落盘（案不存在时静默——台账可能已被新案挤出）。
fn set_outcome(id: u64, outcome: &str, note: &str) {
    let snapshot = {
        let mut ledger = LEDGER.lock().unwrap();
        let now = runtime::unix_now();
        if let Some(issue) = ledger.iter_mut().find(|i| i.id == id) {
            issue.outcome = outcome.to_string();
            if outcome == "resolved" {
                issue.resolved_at = Some(now);
            }
            if !note.is_empty() {
                issue.advice = if issue.advice.is_empty() {
                    note.to_string()
                } else {
                    format!("{}；{note}", issue.advice)
                };
            }
            Some(ledger.clone())
        } else {
            None
        }
    };
    if let Some(snapshot) = snapshot {
        let _ = save_ledger(&snapshot);
    }
    if !note.is_empty() {
        log_line(&format!("#{id} → {outcome}: {note}"));
    }
}

fn resolve_issue(app: &tauri::AppHandle, id: u64, note: &str) {
    set_outcome(id, "resolved", note);
    notifications::record(app, "守护 Agent", &format!("问题 #{id} 已解决：{note}"));
}

fn severity_zh(severity: &str) -> &'static str {
    match severity {
        "error" => "错误",
        "warn" => "警告",
        _ => "提示",
    }
}

/// 纯函数便于单测：按字符数截断（UTF-8 不切半）。
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

fn atomic_write_json<T: serde::Serialize>(path: &PathBuf, value: &T) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

/* ── 采集层 ── */

/// 服务探活：origin 本地是 dsh web、远程是本地反代——统一语义「链路通即活」，
/// 401 登录墙恰恰证明代理链是通的（对齐 readiness::http_reachable 的用途注释）。
fn probe_service(app: &tauri::AppHandle, now: u64) {
    let state = app.state::<crate::AppState>();
    let origin = state.origin.lock().unwrap().clone();
    let Some(url) = origin else {
        rt().lock().unwrap().probe_fail_streak = 0;
        return;
    };
    let ok = readiness::http_reachable(&url);
    let mut g = rt().lock().unwrap();
    if ok {
        g.probe_fail_streak = 0;
        if let Some(id) = g.open_unreachable.take() {
            drop(g);
            resolve_issue(app, id, "服务恢复可达");
        }
        return;
    }
    g.probe_fail_streak += 1;
    let streak = g.probe_fail_streak;
    let already_open = g.open_unreachable.is_some();
    let debounced = debounced(&g, "service_unreachable", now);
    if streak >= PROBE_FAIL_THRESHOLD && !already_open && !debounced {
        g.debounce.insert("service_unreachable".to_string(), now);
        drop(g);
        let evidence = format!("连续 {streak} 次探测 {url} 无响应");
        let id = open_issue(
            app,
            "service_unreachable",
            "error",
            "probe",
            &evidence,
            "服务无响应（进程可能退出或卡死）",
            Some(Action::RestartService),
            "",
        );
        rt().lock().unwrap().open_unreachable = Some(id);
    }
}

/// 增量 tail 壳日志：轮转（文件变小）即重置；半行缓冲跨 tick 拼接。
fn tail_log(app: &tauri::AppHandle, now: u64) {
    let path = runtime::log_file();
    let Ok(meta) = std::fs::metadata(&path) else { return };
    let len = meta.len();
    {
        let mut rt = rt().lock().unwrap();
        if len < rt.log_offset {
            rt.log_offset = 0;
            rt.log_partial.clear();
        }
        if len == rt.log_offset {
            return;
        }
    }
    let Ok(mut file) = std::fs::File::open(&path) else { return };
    let chunk = {
        let mut rt = rt().lock().unwrap();
        let _ = file.seek(SeekFrom::Start(rt.log_offset));
        let mut buf = Vec::new();
        let _ = (&mut file).take(len - rt.log_offset).read_to_end(&mut buf);
        rt.log_offset = len;
        let mut full = std::mem::take(&mut rt.log_partial);
        full.push_str(&String::from_utf8_lossy(&buf));
        // 最后一个 \n 之后是半行，留到下个 tick
        match full.rfind('\n') {
            Some(pos) => {
                rt.log_partial = full[pos + 1..].to_string();
                full[..=pos].to_string()
            }
            None => {
                rt.log_partial = full;
                String::new()
            }
        }
    };
    for line in chunk.lines() {
        if let Some(hit) = classify_line(line) {
            maybe_open_from_rule(app, &hit, line, now);
        }
    }
}

/// 规则命中 → 去抖 + 同类别未决检查后立案。
fn maybe_open_from_rule(app: &tauri::AppHandle, hit: &RuleHit, line: &str, now: u64) {
    let (skip_debounce, has_open, recently_resolved) = {
        let rt = rt().lock().unwrap();
        let ledger = LEDGER.lock().unwrap();
        (
            debounced(&rt, hit.category, now),
            ledger_has_open(&ledger, hit.category),
            hit.severity == "warn" && ledger_recently_resolved(&ledger, hit.category, now),
        )
    };
    if skip_debounce || has_open || recently_resolved {
        return;
    }
    mark_debounced(hit.category, now);
    open_issue(
        app,
        hit.category,
        hit.severity,
        "log",
        line,
        &hit.diagnosis,
        hit.action,
        &hit.advice,
    );
}

/// watch_child 终态接管：自动重启放弃（status.error + 无子进程 + 无流程在途）时，
/// 对错误文本分类并立案——修复动作让「多次异常退出后躺平」重新有系统性出路。
fn check_supervisor_terminal(app: &tauri::AppHandle, now: u64) {
    let state = app.state::<crate::AppState>();
    if state.restarting.is_held() {
        return;
    }
    let (error, child_none, text) = {
        let s = state.status.lock().unwrap();
        let child_none = state.child.lock().unwrap().is_none();
        (s.error, child_none, s.text.clone())
    };
    if !error || !child_none {
        return;
    }
    let (debounced, has_open) = {
        let rt = rt().lock().unwrap();
        let debounced = debounced(&rt, "restart_exhausted", now);
        let has_open = {
            let ledger = LEDGER.lock().unwrap();
            ledger_has_open(&ledger, "restart_exhausted")
        };
        (debounced, has_open)
    };
    if debounced || has_open {
        return;
    }
    mark_debounced("restart_exhausted", now);
    let hit = classify_text(&text);
    let (category, diagnosis, action, advice) = match &hit {
        Some(h) => (
            h.category,
            format!("自动重启已放弃：{}", h.diagnosis),
            h.action,
            h.advice.clone(),
        ),
        None => (
            "restart_exhausted",
            "服务多次异常退出，自动重启已放弃（原因未知，建议 AI 诊断）".into(),
            Some(Action::RestartService),
            String::new(),
        ),
    };
    open_issue(app, category, "error", "supervisor", &text, &diagnosis, action, &advice);
}

/// 纯函数便于单测：去抖判断。
fn debounced(rt: &Rt, category: &str, now: u64) -> bool {
    rt.debounce
        .get(category)
        .is_some_and(|last| now.saturating_sub(*last) < DEBOUNCE_SECS)
}

fn mark_debounced(category: &str, now: u64) {
    rt().lock().unwrap().debounce.insert(category.to_string(), now);
}

/* ── 执行层 ── */

/// 闸锁内执行（仅用于内部不带闸锁的自愈函数；restart_by_mode / install_and_start
/// 自管闸锁，不得包进来——FlowGate 不可重入）。
fn with_gate<T>(app: &tauri::AppHandle, f: impl FnOnce() -> T) -> T {
    let state = app.state::<crate::AppState>();
    state.restarting.acquire();
    let out = f();
    state.restarting.release();
    out
}

fn perform(app: &tauri::AppHandle, action: Action) -> Result<(), String> {
    match action {
        Action::RestartService => {
            crate::supervisor::restart_by_mode(app);
            Ok(())
        }
        Action::ClearFixedPort => {
            let mut cfg = crate::settings::load();
            if cfg.fixed_port.is_some() {
                cfg.fixed_port = None;
                crate::settings::save(&cfg)?;
                log_line("已清除固定端口配置（EADDRINUSE 修复）");
            }
            crate::supervisor::restart_by_mode(app);
            Ok(())
        }
        Action::RefreshProfilePlugins => {
            let r = with_gate(app, || {
                let profiles = crate::install::profile_names();
                crate::install::install_profile_plugins(&profiles, "守护 Agent")
            });
            if let Err(e) = r {
                return Err(e);
            }
            // 补装只落文件，必须重启才生效（服务存活但损坏的场景自己不会好）
            crate::supervisor::restart_by_mode(app);
            Ok(())
        }
        // install_and_start → install_runtime（自取闸锁）→ start_service，无需外包
        Action::RepairRuntime => {
            crate::install::install_and_start(app);
            Ok(())
        }
        Action::ReinstallOfficial => {
            let r = with_gate(app, || crate::install::force_reinstall_official());
            if let Err(e) = r {
                return Err(e);
            }
            crate::supervisor::restart_by_mode(app);
            Ok(())
        }
    }
}

/// 修复后验证：等 origin 恢复可达（重启流程自身有界，验证只兜长尾）。
fn wait_service_up(app: &tauri::AppHandle, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        {
            let state = app.state::<crate::AppState>();
            if state.restarting.is_held() {
                drop(state);
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
            let origin = state.origin.lock().unwrap().clone();
            if let Some(url) = origin {
                if readiness::http_reachable(&url) {
                    return true;
                }
            }
        }
        std::thread::sleep(Duration::from_secs(3));
    }
    false
}

/// 全流程执行一个修复动作：fixing → 动作 → 验证 → resolved/failed → 统计/通知。
fn execute_fix(app: &tauri::AppHandle, id: u64, action: Action) {
    let now = runtime::unix_now();
    set_outcome(id, "fixing", "");
    log_line(&format!("执行修复 #{id}: {}", action.as_str()));
    let result = perform(app, action);
    let verified = result.is_ok() && wait_service_up(app, Duration::from_secs(VERIFY_TIMEOUT_SECS));
    let mut blocked = false;
    {
        let mut rt = rt().lock().unwrap();
        let stat = rt.fix_stat.entry(action.as_str().to_string()).or_default();
        if verified {
            *stat = FixStat { last_at: now, fail_streak: 0, blocked_until: 0 };
        } else {
            blocked = fix_record_failure(stat, now);
        }
    }
    if verified {
        resolve_issue(app, id, &format!("修复动作 {} 执行成功且服务恢复", action.as_str()));
    } else {
        let err = result.err().unwrap_or_else(|| "验证超时".into());
        let note = if blocked {
            format!("修复失败（{err}）；连续失败已达上限，同类修复冷却 {} 分钟", BLOCK_COOLDOWN_SECS / 60)
        } else {
            format!("修复失败：{err}")
        };
        set_outcome(id, "failed", &note);
        notifications::record(app, "守护 Agent", &format!("修复 #{id} 未成功：{err}"));
    }
}

/// 纯函数便于单测：该类别的问题症状是否是「服务起不来/无响应」。
/// 这类案在执行修复前先复核当前健康度——watch_child 可能已经把服务救活，
/// 此时再执行重启就是无意义的自愈后补刀。
fn symptom_is_down(category: &str) -> bool {
    matches!(
        category,
        "service_unreachable"
            | "node_crash"
            | "restart_exhausted"
            | "port_in_use"
            | "npm_error"
            | "module_missing"
    )
}

/// 处置待决案：每个 tick 至多执行一个动作（阻塞型，别把节拍全吃掉）。
/// 处置待决案。两阶段：
/// 1) 自愈复核（半自动/全自动都做）——「起不来」类 pending 案若服务已恢复健康，
///    自动销案，避免堆积与无意义补刀；
/// 2) 修复执行仅全自动模式：半自动只提醒，动作由用户在报告页触发
///    （guardian_fix 走同一套安全阀冷却）。每个 tick 至多执行一个（阻塞型）。
fn process_pending(app: &tauri::AppHandle, cfg: &GuardianCfg, now: u64) {
    let mut candidates: Vec<(u64, Action, String)> = {
        let ledger = LEDGER.lock().unwrap();
        let mut v: Vec<(u64, Action, String)> = ledger
            .iter()
            .filter(|i| i.outcome == "pending")
            .filter_map(|i| {
                i.action.as_deref().and_then(Action::parse).map(|a| (i.id, a, i.category.clone()))
            })
            .collect();
        v.reverse(); // 最旧优先（台账最新在前）
        v
    };
    let mut fix_queue: Vec<(u64, Action, String)> = Vec::new();
    for (id, action, category) in candidates.drain(..) {
        // 症状是「服务不可达」的案子先复核：服务当前健康 = 已自愈，销案不折腾
        if symptom_is_down(&category) {
            // 先落局部变量再比较：块尾表达式会让 MutexGuard 临时值活过 state 的析构（E0597，
            // 与 install_and_start 同款教训）
            let healthy = {
                let state = app.state::<crate::AppState>();
                let origin = state.origin.lock().unwrap().clone();
                origin.is_some_and(|url| readiness::http_reachable(&url))
            };
            if healthy {
                resolve_issue(app, id, "复核时服务已恢复健康，视为自愈");
                continue;
            }
        }
        fix_queue.push((id, action, category));
    }
    if !cfg.auto_fix {
        return; // 半自动：到此为止，修复由用户决定
    }
    for (id, action, category) in fix_queue {
        let allowed = {
            let rt = rt().lock().unwrap();
            fix_allowed(rt.fix_stat.get(&category), action, now)
        };
        if let Err(reason) = allowed {
            // 冷却/间隔未到：原因写进案子的建议里（outcome 仍为 pending，下轮再查）
            set_outcome(id, "pending", &format!("自动修复暂缓：{reason}，可手动执行"));
            continue;
        }
        execute_fix(app, id, action);
        return; // 一个 tick 只做一个（阻塞型）
    }
}

/* ── LLM 增强（OpenAI 兼容，curl 外部进程，零新依赖） ── */

/// 纯函数便于单测：脱敏——token= 参数值打码 + 按字符截断。
fn sanitize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find("token=") {
        out.push_str(&rest[..pos + "token=".len()]);
        rest = &rest[pos + "token=".len()..];
        let end = rest.find(['&', ' ', '"', '\'', ')']).unwrap_or(rest.len());
        out.push_str("«masked»");
        rest = &rest[end..];
    }
    out.push_str(rest);
    truncate_chars(&out, EVIDENCE_MAX_CHARS)
}

#[derive(Debug, Clone)]
pub struct LlmVerdict {
    pub diagnosis: String,
    pub action: Option<Action>,
    pub advice: String,
}

/// 证据 → OpenAI 兼容 /chat/completions → 结构化诊断。curl 不可用/未配置/解析失败
/// 一律 Err，调用方降级为纯规则（Agent 核心功能不依赖 LLM）。
fn llm_diagnose(evidence: &str) -> Result<LlmVerdict, String> {
    let cfg = config();
    let llm = cfg.llm;
    if !llm.enabled || llm.base_url.is_empty() || llm.model.is_empty() || llm.api_key.is_empty() {
        return Err("LLM 未配置".into());
    }
    let url = format!("{}/chat/completions", llm.base_url.trim_end_matches('/'));
    let system = "你是 Windows 桌面应用 dsh-desktop 的守护诊断引擎。壳监督一个本地 dsh web 服务(Node)。\
根据日志证据输出严格 JSON：{\"diagnosis\":\"一句话根因\",\"action\":\"RestartService|ClearFixedPort|RefreshProfilePlugins|RepairRuntime|ReinstallOfficial 之一或 null\",\"advice\":\"给用户的建议，可空\"}。\
action 只能从白名单选，不确定就给 null；除 JSON 外不要输出任何内容。";
    let body = serde_json::json!({
        "model": llm.model,
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": sanitize(evidence)}
        ],
        "temperature": 0,
        "stream": false
    });
    let dir = ledger_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let body_path = dir.join("llm-req.json");
    let hdr_path = dir.join("llm-hdr.txt");
    std::fs::write(&body_path, serde_json::to_string(&body).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    // key 经 @file 传头，不进进程命令行（进程列表可见性问题）
    std::fs::write(&hdr_path, format!("Authorization: Bearer {}\r\nContent-Type: application/json\r\n", llm.api_key))
        .map_err(|e| e.to_string())?;
    let mut cmd = if cfg!(windows) {
        std::process::Command::new("curl.exe")
    } else {
        std::process::Command::new("curl")
    };
    cmd.args([
        "-sS",
        "--max-time",
        &LLM_TIMEOUT_SECS.to_string(),
        "-X",
        "POST",
        &url,
        "-H",
        &format!("@{}", hdr_path.display()),
        "--data",
        &format!("@{}", body_path.display()),
    ]);
    let _ = runtime::no_window(&mut cmd);
    let out = cmd.output().map_err(|e| format!("curl 启动失败: {e}"));
    let _ = std::fs::remove_file(&body_path);
    let _ = std::fs::remove_file(&hdr_path);
    let out = out?;
    if !out.status.success() {
        return Err(format!("curl 退出码非零: {}", String::from_utf8_lossy(&out.stderr)));
    }
    let resp: serde_json::Value = serde_json::from_slice(&out.stdout).map_err(|e| format!("响应非 JSON: {e}"))?;
    if let Some(msg) = resp.get("error").and_then(|e| e.get("message")).and_then(|m| m.as_str()) {
        return Err(format!("接口报错: {msg}"));
    }
    let content = resp
        .pointer("/choices/0/message/content")
        .and_then(|c| c.as_str())
        .ok_or("响应缺少 choices[0].message.content")?;
    let verdict = parse_verdict(content)?;
    Ok(verdict)
}

/// 纯函数便于单测：剥代码围栏 → 解析 verdict → action 白名单校验（非法动作降为 None）。
fn parse_verdict(content: &str) -> Result<LlmVerdict, String> {
    let t = content.trim();
    let t = t.strip_prefix("```json").or_else(|| t.strip_prefix("```")).unwrap_or(t);
    let t = t.strip_suffix("```").unwrap_or(t).trim();
    let v: serde_json::Value = serde_json::from_str(t).map_err(|e| format!("verdict 非 JSON: {e}"))?;
    let action = v
        .get("action")
        .and_then(|a| a.as_str())
        .and_then(Action::parse);
    Ok(LlmVerdict {
        diagnosis: v.get("diagnosis").and_then(|d| d.as_str()).unwrap_or("").to_string(),
        action,
        advice: v.get("advice").and_then(|a| a.as_str()).unwrap_or("").to_string(),
    })
}

/// 对某个案执做 AI 诊断（规则未命中时的增强路径 + 报告页手动按钮）。
/// 诊断结论写回台账 advice/diagnosis；LLM 给出白名单动作且案子还没有动作时补挂。
pub fn diagnose_issue(app: &tauri::AppHandle, id: u64) -> Result<String, String> {
    let evidence = {
        let ledger = LEDGER.lock().unwrap();
        let issue = ledger.iter().find(|i| i.id == id).ok_or("问题不存在")?;
        format!("类别: {}\n诊断: {}\n证据:\n{}", issue.category, issue.diagnosis, issue.evidence)
    };
    let verdict = llm_diagnose(&evidence)?;
    let mut note = format!("AI 诊断：{}", verdict.diagnosis);
    if !verdict.advice.is_empty() {
        note.push_str(&format!("（建议：{}）", verdict.advice));
    }
    {
        let snapshot = {
            let mut ledger = LEDGER.lock().unwrap();
            if let Some(issue) = ledger.iter_mut().find(|i| i.id == id) {
                if !verdict.diagnosis.is_empty() {
                    issue.diagnosis = format!("{}（AI：{}）", issue.diagnosis, verdict.diagnosis);
                }
                if issue.action.is_none() {
                    issue.action = verdict.action.map(|a| a.as_str().to_string());
                }
                if !verdict.advice.is_empty() {
                    issue.advice = if issue.advice.is_empty() {
                        verdict.advice.clone()
                    } else {
                        format!("{}；{}", issue.advice, verdict.advice)
                    };
                }
                Some(ledger.clone())
            } else {
                None
            }
        };
        if let Some(s) = snapshot {
            let _ = save_ledger(&s);
        }
    }
    log_line(&format!("AI 诊断 #{id}: {}", verdict.diagnosis));
    let _ = app;
    Ok(note)
}

/* ── 守护循环 ── */

fn inspect(app: &tauri::AppHandle, cfg: &GuardianCfg, now: u64) {
    let _ = cfg;
    probe_service(app, now);
    tail_log(app, now);
    check_supervisor_terminal(app, now);
}

fn tick(app: &tauri::AppHandle) {
    let cfg = config();
    if !cfg.enabled {
        return;
    }
    {
        let state = app.state::<crate::AppState>();
        if state.restarting.is_held() {
            return;
        }
    }
    let now = runtime::unix_now();
    inspect(app, &cfg, now);
    process_pending(app, &cfg, now);
    rt().lock().unwrap().last_tick = Some(runtime::unix_now());
}

/// 守护线程入口（对齐 watch_child 模式：专用线程 + 固定节拍 + 闸锁探测跳过）。
fn run(app: tauri::AppHandle) {
    std::thread::sleep(Duration::from_secs(FIRST_TICK_SECS));
    loop {
        // 守护自身绝不 panic 带崩线程：单轮异常下一轮再来
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| tick(&app)));
        if let Err(e) = result {
            log_line(&format!("巡检轮异常（忽略，下轮继续）: {:?}", e.downcast_ref::<String>()));
        }
        std::thread::sleep(Duration::from_secs(TICK_SECS));
    }
}

/// setup 时调用：加载台账 + 起守护线程。
pub fn spawn(app: tauri::AppHandle) {
    init_ledger();
    std::thread::spawn(move || run(app));
}

/* ── 命令层 API（main.rs 的 #[tauri::command] 包装，全部 caller_is_local 守卫） ── */

#[derive(Serialize, Clone)]
pub struct Snapshot {
    pub enabled: bool,
    pub auto_fix: bool,
    pub llm_enabled: bool,
    pub last_tick: Option<u64>,
    pub issues: Vec<Issue>,
}

pub fn snapshot() -> Snapshot {
    let cfg = config();
    Snapshot {
        enabled: cfg.enabled,
        auto_fix: cfg.auto_fix,
        llm_enabled: cfg.llm.enabled && !cfg.llm.base_url.is_empty() && !cfg.llm.api_key.is_empty(),
        last_tick: rt().lock().unwrap().last_tick,
        issues: LEDGER.lock().unwrap().clone(),
    }
}

/// 开关（持久化到 guardian.json）。
pub fn set_enabled(enabled: bool) -> GuardianCfg {
    let mut cfg = config();
    cfg.enabled = enabled;
    let _ = save_config(&cfg);
    cfg
}

/// 立即巡检一次（命令触发：专用线程，绝不卡 UI）。
pub fn run_once(app: tauri::AppHandle) {
    std::thread::spawn(move || {
        let cfg = config();
        if !cfg.enabled {
            return;
        }
        let now = runtime::unix_now();
        inspect(&app, &cfg, now);
        process_pending(&app, &cfg, now);
        rt().lock().unwrap().last_tick = Some(runtime::unix_now());
        let _ = app.emit_guardian_refresh();
    });
}

/// 手动执行某案的建议动作（绕过 auto_fix 开关，不绕过安全阀冷却）。
pub fn fix_issue(app: tauri::AppHandle, id: u64) -> Result<(), String> {
    let (action, category) = {
        let ledger = LEDGER.lock().unwrap();
        let issue = ledger.iter().find(|i| i.id == id).ok_or("问题不存在")?;
        let action = issue.action.as_deref().and_then(Action::parse).ok_or("该问题没有可执行动作")?;
        (action, issue.category.clone())
    };
    {
        let rt = rt().lock().unwrap();
        if let Err(reason) = fix_allowed(rt.fix_stat.get(&category), action, runtime::unix_now()) {
            return Err(format!("安全阀拦截：{reason}（可稍后再试）"));
        }
    }
    std::thread::spawn(move || {
        execute_fix(&app, id, action);
        let _ = app.emit_guardian_refresh();
    });
    Ok(())
}

/// 手动 AI 诊断某案（后台执行，完成后刷新）。
pub fn diagnose_async(app: tauri::AppHandle, id: u64) {
    std::thread::spawn(move || {
        let result = diagnose_issue(&app, id);
        if let Err(e) = &result {
            log_line(&format!("AI 诊断 #{id} 失败: {e}"));
            notifications::record(&app, "守护 Agent", &format!("AI 诊断失败：{e}"));
        }
        let _ = app.emit_guardian_refresh();
    });
}

/// 守护报告窗刷新事件（guardian.html 监听；未开窗则静默，与通知中心同款）。
trait EmitGuardianRefresh {
    fn emit_guardian_refresh(&self) -> Result<(), tauri::Error>;
}
impl EmitGuardianRefresh for tauri::AppHandle {
    fn emit_guardian_refresh(&self) -> Result<(), tauri::Error> {
        use tauri::Emitter;
        self.emit("guardian-updated", ())
    }
}

/// 打开/聚焦守护报告窗（模式与通知中心一致：按需创建、再次点击仅聚焦）。
pub fn open_window(app: &tauri::AppHandle) -> tauri::Result<()> {
    if let Some(w) = app.get_webview_window("guardian") {
        let _ = w.show();
        let _ = w.set_focus();
        return Ok(());
    }
    let handle = app.clone();
    app.run_on_main_thread(move || {
        let w = tauri::WebviewWindowBuilder::new(
            &handle,
            "guardian",
            tauri::WebviewUrl::App("guardian.html".into()),
        )
        .title("DSH 守护 Agent")
        .inner_size(440.0, 580.0)
        .min_inner_size(340.0, 420.0)
        .center()
        .build();
        if let Err(e) = w {
            log_line(&format!("创建守护报告窗失败: {e}"));
        }
    })
}

/* ── 测试 ── */

#[cfg(test)]
mod tests {
    use super::*;

    /* ── 规则表 ── */
    #[test]
    fn classify_hits_known_failure_modes() {
        let hit = classify_line("[err] Node.js v22.14.0").expect("崩溃横幅应命中");
        assert_eq!(hit.category, "node_crash");
        assert_eq!(hit.action, Some(Action::RestartService));

        let hit = classify_line(
            "[err] Error: cannot resolve profile bundle for dsh-better-sidebar (run dsh plugin --profile web install)",
        )
        .expect("bundle 失联应命中");
        assert_eq!(hit.category, "profile_bundle");
        assert_eq!(hit.action, Some(Action::RefreshProfilePlugins));

        let hit = classify_line(
            "[err] Error: Cannot find package '@deepseek-ai/schemastery' imported from E:/x/dsh-twin",
        )
        .expect("本地链接插件缺依赖应命中");
        assert_eq!(hit.category, "plugin_dep_missing");
        assert_eq!(hit.action, None, "壳修不了源码仓依赖，只给建议");
        assert!(hit.advice.contains("pnpm install"), "建议要可操作");

        let hit = classify_line("[err] Error: listen EADDRINUSE: address already in use 127.0.0.1:4418").unwrap();
        assert_eq!(hit.action, Some(Action::ClearFixedPort));

        let hit = classify_line("[npm] npm ERR! code ECONNRESET").unwrap();
        assert_eq!(hit.action, Some(Action::RepairRuntime));

        let hit = classify_line("[events] 订阅失败: 401 Unauthorized").unwrap();
        assert_eq!(hit.category, "auth_401");

        // 端口号含 "401" 子串不得误报（实机首日真实误报：端口 54014）
        assert!(classify_line("[events] 事件流启动（gen=1）：base=http://127.0.0.1:54014").is_none());
        assert!(classify_line("[out] count=14012 requests").is_none());

        // skipping 前缀的 bundle 失联是非致命跳过：降级 warn，动作保留
        let hit = classify_line(
            "[err] dsh: skipping profile bundle \"@dsh-extra/dsh-actors\": Error: cannot resolve profile bundle \"@dsh-extra/dsh-actors\"",
        )
        .unwrap();
        assert_eq!(hit.severity, "warn", "skipping = 非致命，应为 warn");
        assert_eq!(hit.action, Some(Action::RefreshProfilePlugins));

        // 普通行不命中
        assert!(classify_line("[out] dsh web: http://127.0.0.1:4418/?token=x").is_none());
        assert!(classify_line("[守护] 立案 #3").is_none());
        // 「Node.js」出现在错误文案中段不算横幅（对齐 supervisor 语义）
        assert!(classify_line("[err] Error: Node.js require failed").is_none());
    }

    #[test]
    fn classify_text_finds_first_hit_across_lines() {
        let text = "服务未就绪\n最近输出：\n[err] Error: listen EADDRINUSE: address already in use 127.0.0.1:4418";
        let hit = classify_text(text).expect("多行文本应命中");
        assert_eq!(hit.category, "port_in_use");
        assert!(classify_text("一切正常").is_none());
    }

    /* ── 安全阀 ── */
    #[test]
    fn fix_allowed_enforces_interval_block_and_high_risk() {
        let now = 1_000_000u64;
        assert!(fix_allowed(None, Action::RestartService, now).is_ok(), "无历史放行");
        let fresh = FixStat { last_at: now - 60, fail_streak: 0, blocked_until: 0 };
        assert!(fix_allowed(Some(&fresh), Action::RestartService, now).is_err(), "间隔未到拒绝");
        let old = FixStat { last_at: now - FIX_MIN_INTERVAL_SECS - 1, fail_streak: 0, blocked_until: 0 };
        assert!(fix_allowed(Some(&old), Action::RestartService, now).is_ok());
        // 高风险动作间隔更长
        assert!(fix_allowed(Some(&old), Action::ReinstallOfficial, now).is_err());
        let blocked = FixStat { last_at: 0, fail_streak: 0, blocked_until: now + 100 };
        assert_eq!(fix_allowed(Some(&blocked), Action::RestartService, now), Err("连续失败冷却中"));
    }

    #[test]
    fn fix_failures_escalate_to_cooldown_then_reset() {
        let now = 5_000_000u64;
        let mut stat = FixStat::default();
        assert!(!fix_record_failure(&mut stat, now));
        assert!(!fix_record_failure(&mut stat, now + 1));
        assert!(fix_record_failure(&mut stat, now + 2), "第 3 次失败触发冷却");
        assert!(stat.blocked_until > now + 2, "冷却截止在未来");
        assert_eq!(stat.fail_streak, 0, "冷却期计数清零");
    }

    /* ── 台账纯函数 ── */
    #[test]
    fn ledger_insert_keeps_newest_first_and_caps() {
        let mut ledger: Vec<Issue> = Vec::new();
        for i in 1..=(LEDGER_CAP + 10) as u64 {
            ledger_insert(
                &mut ledger,
                Issue {
                    id: i,
                    at: i,
                    category: "x".into(),
                    severity: "warn".into(),
                    source: "log".into(),
                    evidence: String::new(),
                    diagnosis: String::new(),
                    action: None,
                    outcome: "pending".into(),
                    advice: String::new(),
                    resolved_at: None,
                },
            );
        }
        assert_eq!(ledger.len(), LEDGER_CAP);
        assert_eq!(ledger[0].id, LEDGER_CAP as u64 + 10, "最新在前");
        assert_eq!(ledger_next_id(&ledger), LEDGER_CAP as u64 + 11);
        assert!(ledger_has_open(&ledger, "x"));
        assert!(!ledger_has_open(&ledger, "y"));
    }

    /* ── 良性告警销案抑制 ── */
    #[test]
    fn warn_suppressed_after_recent_resolve_but_errors_never() {
        let now = 9_000_000u64;
        let resolved = |resolved_at: Option<u64>| Issue {
            id: 1,
            at: now - 3600,
            category: "profile_bundle".into(),
            severity: "warn".into(),
            source: "log".into(),
            evidence: String::new(),
            diagnosis: String::new(),
            action: None,
            outcome: "resolved".into(),
            advice: String::new(),
            resolved_at,
        };
        // 1 小时前解决过：warn 不再重复立案
        assert!(ledger_recently_resolved(&[resolved(Some(now - 3600))], "profile_bundle", now));
        // 超过 24h：重新立案
        assert!(!ledger_recently_resolved(
            &[resolved(Some(now - 25 * 3600))],
            "profile_bundle",
            now
        ));
        // 未解决/别的类别：不抑制
        assert!(!ledger_recently_resolved(&[resolved(None)], "profile_bundle", now));
        assert!(!ledger_recently_resolved(&[resolved(Some(now - 60))], "node_crash", now));
    }

    /* ── 脱敏 / verdict 解析 ── */
    #[test]
    fn sanitize_masks_token_values_and_truncates() {
        let s = sanitize("http://127.0.0.1:4418/?token=AbC-123_xYz0 &more");
        assert!(s.contains("token=«masked»"), "{s}");
        assert!(!s.contains("AbC-123"), "token 值必须被打码");
        let long = "x".repeat(EVIDENCE_MAX_CHARS + 50);
        assert_eq!(sanitize(&long).chars().count(), EVIDENCE_MAX_CHARS + 1);
    }

    #[test]
    fn parse_verdict_strips_fence_and_validates_action_whitelist() {
        let v = parse_verdict("```json\n{\"diagnosis\":\"端口占用\",\"action\":\"ClearFixedPort\",\"advice\":\"已换随机端口\"}\n```").unwrap();
        assert_eq!(v.action, Some(Action::ClearFixedPort));
        assert_eq!(v.diagnosis, "端口占用");
        let v = parse_verdict(r#"{"diagnosis":"未知","action":"FormatDisk","advice":""}"#).unwrap();
        assert_eq!(v.action, None, "白名单外动作必须降为 None");
        assert!(parse_verdict("not json").is_err());
    }

    #[test]
    fn truncate_chars_never_splits_utf8() {
        let s = "错".repeat(3000);
        let t = truncate_chars(&s, 2048);
        assert_eq!(t.chars().count(), 2049);
    }

    /* ── 自愈复核 ── */
    #[test]
    fn symptom_is_down_covers_unreachable_class_only() {
        for c in ["service_unreachable", "node_crash", "restart_exhausted", "port_in_use", "npm_error", "module_missing"] {
            assert!(symptom_is_down(c), "{c} 应视为「起不来」类");
        }
        // 凭证/插件类：服务可达但行为异常，不能靠「健康就销案」跳过修复
        for c in ["auth_401", "profile_bundle", "plugin_dep_missing"] {
            assert!(!symptom_is_down(c), "{c} 不应视为「起不来」类");
        }
    }
}
