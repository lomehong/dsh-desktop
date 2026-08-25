//! 便携版分身信息向导：移植自 dsh-persona scripts/setup.ps1 的向导与预设生成逻辑。
//! 仅便携模式生效——安装版的人设由 dsh-persona 的 setup 脚本负责。
//!
//! 保存产物（全部落在包内 Data/home，即 DSH_HOME）：
//! - .agent-presets/digital-twin/{agent.cordis.yml,preset.yml}：基于内置 standard 预设
//!   现场替换人设行 + 追加御驿/记忆工具行（与 setup.ps1 同一算法）
//! - profiles/web/cordis.patch.yml：system-prompt 人设 / skill-filesystem / 默认预设
//! - im-channel/credentials/wecom.json（填了 BotID/Secret 才写）
//! - persona-configured.json：完成标记（重跑向导可覆盖全部产物）
use std::path::PathBuf;

use tauri::Manager;

use crate::runtime;

/// 前端提交的向导字段（与 setup.ps1 的 8 项向导一致 + 可选企业微信凭证）。
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WizardFields {
    pub owner: String,
    pub owner_title: String,
    pub owner_stance: String,
    pub owner_scope: String,
    pub owner_style: String,
    pub owner_address: String,
    pub twin_name: String,
    pub twin_aliases: String,
    pub bot_id: Option<String>,
    pub secret: Option<String>,
}

fn marker_path(home: &PathBuf) -> PathBuf {
    home.join("persona-configured.json")
}

/// 便携模式且尚未完成分身配置。
pub fn needed() -> bool {
    match runtime::portable_home() {
        Some(home) => !marker_path(&home).exists(),
        None => false,
    }
}

/// 别名子句：支持中英文逗号、顿号分隔，列举成「a」、「b」形式。
fn alias_clause(aliases: &str, twin_name: &str) -> String {
    let list: Vec<String> = aliases
        .split([',', '，', '、'])
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| format!("「{s}」"))
        .collect();
    if list.is_empty() {
        return format!("当有人称呼「{twin_name}」时，指的就是你；");
    }
    let joined = list.join("、");
    format!("当有人称呼{joined}或「{twin_name}」时，指的就是你；")
}

/// 人设正文（setup.ps1 的 $personaText 模板；`{{model}}` 为 dsh 的模型占位符，原样保留）。
fn persona_text(f: &WizardFields) -> String {
    let alias = alias_clause(&f.twin_aliases, &f.twin_name);
    [
        format!("你是{}的数字分身，由 {{{{model}}}} 模型驱动。", f.owner),
        String::new(),
        format!(
            "你的身份：{}（{}）的{}。你了解{}的工作背景、管理风格和个人偏好，以{}的视角思考问题，用{}的风格沟通表达。",
            f.owner, f.owner_title, f.owner_stance, f.owner, f.owner, f.owner
        ),
        String::new(),
        format!(
            "你的角色：一个务实、高效的 AI 搭档。你不是在「服务」{}，而是在「协作」——你提供专业分析和建议，{}做最终决策。你们是有商有量的伙伴关系。",
            f.owner, f.owner
        ),
        String::new(),
        format!(
            "你的称呼习惯与身份区分：每条消息开头的方括号标注了发送者身份——「主人…」表示你的主人{}，「访客…」表示其他使用者。只有消息以「主人」标注时，你才称呼对方为「{}」；消息以「访客」标注时，以「您」或对方的姓名、职位礼貌称呼，绝不称呼访客为「{}」。",
            f.owner, f.owner_address, f.owner_address
        ),
        String::new(),
        format!(
            "你的名字与自我认知：你的名字是「{}」。{}回答时以「{}」自称。你不是{}本人——你是{}的数字分身：{}指的是你服务的人，而你（「{}」）是协助{}的 AI 搭档。",
            f.twin_name, alias, f.twin_name, f.owner, f.owner, f.owner, f.twin_name, f.owner
        ),
        String::new(),
        format!("你的工作范围：涵盖{}的文档处理、方案分析、决策支持、跨部门协调等事务。", f.owner_scope),
        String::new(),
        format!("沟通风格：{}", f.owner_style),
    ]
    .join("\n")
}

/// 人设块：非空行加 6 空格缩进（可直接嵌入 YAML 的块标量 >-），空行保持空。
fn persona_block(f: &WizardFields) -> String {
    persona_text(f)
        .lines()
        .map(|line| if line.is_empty() { String::new() } else { format!("      {line}") })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 内置 standard 预设路径（运行时内，随包携带、离线可读）。
fn standard_preset_path() -> Option<PathBuf> {
    let root = runtime::portable_root()?;
    let mut p = root.join("node");
    if !cfg!(windows) {
        p = p.join("lib");
    }
    Some(
        p.join("node_modules")
            .join("@deepseek-ai")
            .join("dsh")
            .join("config")
            .join("agent-presets")
            .join("standard")
            .join("agent.cordis.yml"),
    )
}

/// compose 的纯函数核心（便于单测）：替换人设行、加头注释与工具行。
fn compose_agent_yaml(std_text: &str, block: &str) -> Result<String, String> {
    let marker = "      You are a coding agent";
    let mut did_replace = false;
    let lines: Vec<String> = std_text
        .lines()
        .map(|line| {
            if !did_replace && line.starts_with(marker) {
                did_replace = true;
                block.to_string()
            } else {
                line.to_string()
            }
        })
        .collect();
    if !did_replace {
        return Err(
            "未能替换 standard 预设的人设行（DSH 版本变化？）。请改用安装版 setup 脚本配置人设".into(),
        );
    }
    let mut replaced = lines.join("\n");
    if std_text.ends_with('\n') {
        replaced.push('\n');
    }
    let header = "# 数字分身预设：结构与 DSH 内置 standard 预设一致，仅替换 persona 人设。\n";
    let tools = "\n# 御驿通信工具（dsh-yuyi）\n- id: tool-yuyi\n  name: dsh-yuyi/tools\n\n# 共享记忆工具（dsh-memory）\n- id: tool-memory\n  name: '@dsh-extra/dsh-memory/tools'\n";
    let mut out = String::new();
    out.push_str(header);
    out.push_str(replaced.trim_end_matches(['\n', '\r']));
    out.push_str(tools);
    Ok(out)
}

/// 生成 profile 的 cordis.patch.yml（人设 / 技能目录 / 默认预设）。
fn build_cordis_patch(f: &WizardFields) -> String {
    format!(
        "# ── 数字分身 ──────────────────────────────────────────────────────\n- id: system-prompt\n  config:\n    persona: >-\n{}\n\n- id: skill-filesystem\n  config:\n    directories:\n      - ~/.dsh/skills\n\n- id: agent-presets\n  config:\n    default: digital-twin\n",
        persona_block(f)
    )
}

fn write_utf8(path: &PathBuf, content: &str) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("创建目录失败 {}: {e}", dir.display()))?;
    }
    std::fs::write(path, content).map_err(|e| format!("写入 {} 失败: {e}", path.display()))
}

/// 保存向导结果并落完成标记。
pub fn save(f: &WizardFields) -> Result<(), String> {
    let home = runtime::portable_home().ok_or("仅便携模式支持分身向导")?;
    apply_files(&home, f)
}

/// 写全部产物到指定 home（save 的核心；standard 预设文本由调用方提供，便于单测注入）。
fn apply_files(home: &PathBuf, f: &WizardFields) -> Result<(), String> {
    let std_text = standard_preset_text()?;
    apply_files_with(home, f, &std_text)
}

fn standard_preset_text() -> Result<String, String> {
    let std_path = standard_preset_path()
        .ok_or_else(|| "便携运行时缺失，无法读取内置 standard 预设".to_string())?;
    std::fs::read_to_string(&std_path).map_err(|e| format!("读取 standard 预设失败: {e}"))
}

fn apply_files_with(home: &PathBuf, f: &WizardFields, std_text: &str) -> Result<(), String> {
    let composition = compose_agent_yaml(std_text, &persona_block(f))?;
    write_utf8(
        &home.join(".agent-presets").join("digital-twin").join("agent.cordis.yml"),
        &composition,
    )?;
    write_utf8(
        &home.join(".agent-presets").join("digital-twin").join("preset.yml"),
        &format!("name: 数字分身\ndescription: {}（{}）的专属数字分身。\n", f.owner, f.owner_title),
    )?;
    write_utf8(&home.join("profiles").join("web").join("cordis.patch.yml"), &build_cordis_patch(f))?;

    if let (Some(bot), Some(secret)) = (f.bot_id.as_deref(), f.secret.as_deref()) {
        if !bot.trim().is_empty() && !secret.trim().is_empty() {
            let json = serde_json::json!({ "botId": bot.trim(), "secret": secret.trim() });
            write_utf8(
                &home.join("im-channel").join("credentials").join("wecom.json"),
                &json.to_string(),
            )?;
        }
    }

    let marker = serde_json::json!({
        "configured": true,
        "owner": f.owner,
        "twinName": f.twin_name,
        "configuredAt": chrono_like_now(),
    });
    write_utf8(&marker_path(home), &marker.to_string())
}

/// 无 chrono 依赖的本地时间戳（精度到分钟即可，仅作记录）。
fn chrono_like_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("unix:{secs}")
}

/// 托盘「重新运行分身向导」：停服务 → 回加载页向导态（保存后自动重启服务）。
pub fn reopen(app: &tauri::AppHandle) {
    let state: tauri::State<crate::AppState> = app.state();
    if let Some(mut child) = state.child.lock().unwrap().take() {
        crate::supervisor::kill_tree(child.id());
        let _ = child.wait();
    }
    *state.origin.lock().unwrap() = None;
    crate::webview::navigate_to_loader(app);
    crate::status::wizard(app, "正在配置分身信息…");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields() -> WizardFields {
        WizardFields {
            owner: "罗拉".into(),
            owner_title: "公司副总裁".into(),
            owner_stance: "专属 AI 协作伙伴".into(),
            owner_scope: "人力资源、审计".into(),
            owner_style: "直接、务实。".into(),
            owner_address: "老板".into(),
            twin_name: "罗拉的数字分身".into(),
            twin_aliases: "分身, 小罗、阿分".into(),
            bot_id: Some("bot-1".into()),
            secret: Some("sec-1".into()),
        }
    }

    /// 模拟内置 standard 预设的关键片段（人设行 + 前后文）。
    const STD_SNIPPET: &str = "instructions:\n  - id: persona\n    template: >-\n      You are a coding agent running on dsh.\n      Follow the workspace conventions.\n";

    #[test]
    fn alias_clause_lists_all_forms() {
        let c = alias_clause("分身, 小罗、阿分", "罗拉的数字分身");
        assert_eq!(c, "当有人称呼「分身」、「小罗」、「阿分」或「罗拉的数字分身」时，指的就是你；");
        assert_eq!(alias_clause("", "小七"), "当有人称呼「小七」时，指的就是你；");
    }

    #[test]
    fn persona_text_keeps_model_placeholder() {
        let t = persona_text(&fields());
        assert!(t.contains("{{model}}"), "模型占位符必须原样保留");
        assert!(t.contains("罗拉的数字分身"));
        assert!(!t.contains("${"), "不应残留 PowerShell 插值语法");
    }

    #[test]
    fn persona_block_indents_nonempty_lines_six_spaces() {
        let b = persona_block(&fields());
        for line in b.lines() {
            assert!(
                line.is_empty() || line.starts_with("      "),
                "非空行必须 6 空格缩进: {line}"
            );
        }
        assert!(b.lines().any(|l| l.trim_start().starts_with("你是罗拉的数字分身")));
    }

    #[test]
    fn compose_replaces_persona_line_and_appends_tools() {
        let out = compose_agent_yaml(STD_SNIPPET, "      你是罗拉的数字分身。").unwrap();
        assert!(out.starts_with("# 数字分身预设"), "须有头注释");
        assert!(!out.contains("You are a coding agent"), "原人设行应被替换");
        assert!(out.contains("      你是罗拉的数字分身。"));
        assert!(out.contains("- id: tool-yuyi\n  name: dsh-yuyi/tools"));
        assert!(out.contains("- id: tool-memory\n  name: '@dsh-extra/dsh-memory/tools'"));
        // 未包含人设行的模板应报错而不是静默产出错误预设
        assert!(compose_agent_yaml("no marker here", "x").is_err());
    }

    #[test]
    fn cordis_patch_shape() {
        let p = build_cordis_patch(&fields());
        assert!(p.contains("- id: system-prompt"));
        assert!(p.contains("    persona: >-"));
        assert!(p.contains("- id: skill-filesystem"));
        assert!(p.contains("      - ~/.dsh/skills"));
        assert!(p.contains("    default: digital-twin"));
    }

    #[test]
    fn apply_files_writes_all_outputs() {
        let home = std::env::temp_dir().join(format!("dsh-wizard-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        apply_files_with(&home, &fields(), STD_SNIPPET).unwrap();

        let composition =
            std::fs::read_to_string(home.join(".agent-presets").join("digital-twin").join("agent.cordis.yml")).unwrap();
        assert!(composition.contains("你是罗拉的数字分身"));

        let preset =
            std::fs::read_to_string(home.join(".agent-presets").join("digital-twin").join("preset.yml")).unwrap();
        assert!(preset.contains("name: 数字分身"));
        assert!(preset.contains("罗拉（公司副总裁）"));

        assert!(home.join("profiles").join("web").join("cordis.patch.yml").exists());

        let cred = std::fs::read_to_string(home.join("im-channel").join("credentials").join("wecom.json")).unwrap();
        assert!(cred.contains("\"botId\":\"bot-1\""));

        let marker: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(marker_path(&home)).unwrap()).unwrap();
        assert_eq!(marker["configured"], true);
        assert_eq!(marker["owner"], "罗拉");

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn apply_files_skips_empty_credentials() {
        let mut f = fields();
        f.bot_id = Some("  ".into());
        f.secret = None;
        let home = std::env::temp_dir().join(format!("dsh-wizard-test-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        apply_files_with(&home, &f, STD_SNIPPET).unwrap();
        assert!(!home.join("im-channel").join("credentials").join("wecom.json").exists());
        let _ = std::fs::remove_dir_all(&home);
    }
}
