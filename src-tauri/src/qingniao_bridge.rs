//! 青鸟接入层：把壳内已经结构化的现场失败交给通用 `qingniao` SDK。
//!
//! 通用 SDK 不知道 dsh / pnpm / 插件；本模块负责三件 dsh 特有的事：
//! 1. 决定一条现场信号归属哪个**可修仓库**（壳、元仓或具体插件仓）；
//! 2. 把现场语境（dsh 版本、模式、类别、动作、错误码）放进 `attributes`；
//! 3. 为安装/加载类失败补上日志里的**根因行**（guardian 的 evidence 只有
//!    一行“已回滚”，真正的 `ERR_PNPM_*` 在它前面几行）。
//!
//! 未配置 `QINGNIAO_ENDPOINT` / `NUWA_INGEST_URL` 时整层为空操作，避免把信号
//! 静默堆进兜底文件。

use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use qingniao::{Event, HttpSink, Meta, Options, Reporter};

const APP_REPO: &str = "github.com/lomehong/dsh-desktop";
const SUITE_REPO: &str = "github.com/lomehong/digital-twin";

/// Installed plugin package → source repo (the repair target).
const PLUGIN_REPOS: &[(&str, &str)] = &[
    ("@dsh-extra/dsh-twin", "github.com/lomehong/dsh-twin"),
    ("@dsh-extra/dsh-mind", "github.com/lomehong/dsh-mind"),
    ("@dsh-extra/dsh-memory", "github.com/lomehong/dsh-memory"),
    ("@dsh-extra/dsh-ledger", "github.com/lomehong/dsh-ledger"),
    ("@dsh-extra/dsh-regression", "github.com/lomehong/dsh-regression"),
    ("@dsh-extra/dsh-actors", "github.com/lomehong/dsh-actors"),
    ("@dsh-extra/dsh-redact", "github.com/lomehong/dsh-redact"),
    ("@dsh-extra/dsh-task-board", "github.com/lomehong/dsh-task-board"),
    ("@dsh-extra/im-channel", "github.com/lomehong/dsh-im-bot"),
    (
        "@dsh-extra/dsh-client-ui-settings-im",
        "github.com/lomehong/dsh-im-bot",
    ),
    ("dsh-yuyi", "github.com/lomehong/dsh-yuyi"),
];

static REPORTER: OnceLock<Option<Arc<Reporter>>> = OnceLock::new();

pub fn init() {
    let endpoint = std::env::var("QINGNIAO_ENDPOINT")
        .ok()
        .or_else(|| std::env::var("NUWA_INGEST_URL").ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let Some(endpoint) = endpoint else {
        let _ = REPORTER.set(None);
        return;
    };
    let token = std::env::var("QINGNIAO_TOKEN")
        .ok()
        .or_else(|| std::env::var("NUWA_INGEST_TOKEN").ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let mut sink = HttpSink::new(endpoint);
    sink.token = token;
    // 兜底跟应用数据走（便携模式下随 U 盘），而不是用户全局缓存目录。
    sink.fallback_file = Some(
        crate::runtime::runtime_root()
            .join("qingniao")
            .join("fallback.jsonl"),
    );
    let mut options = Options::new(Meta {
        repo: APP_REPO.to_string(),
        service: Some("desktop".to_string()),
        severity: None,
        instance: None,
    });
    options.sink = Some(Arc::new(sink));
    let reporter = Reporter::new(options);
    reporter.install_panic_hook();
    let _ = REPORTER.set(Some(reporter));
}

pub fn close() {
    if let Some(reporter) = reporter() {
        reporter.close(Duration::from_secs(3));
    }
}

fn reporter() -> Option<&'static Arc<Reporter>> {
    REPORTER.get().and_then(Option::as_ref)
}

/// One guardian case → one signal. Local-only environment failures (probe,
/// port, auth) are deliberately not shipped: the guardian repairs them and a
/// code PR is the wrong response.
pub fn report_issue(
    category: &str,
    severity: &str,
    source: &str,
    evidence: &str,
    diagnosis: &str,
    action: Option<&str>,
) {
    let Some(reporter) = reporter() else {
        return;
    };
    let Some(repo) = repo_for(category, evidence, diagnosis) else {
        return;
    };
    let root_cause = root_cause_line(evidence);
    let message = if root_cause.is_empty() {
        format!("{evidence}\n{diagnosis}")
    } else {
        format!("{evidence}\n--- root cause ---\n{root_cause}")
    };
    let attrs = attributes(category, source, action, &message);
    // The repair agent reads the issue's stack sample. Put the environment
    // there as a stable header so the context survives even though the current
    // ingest `Signal` does not yet persist `attributes`.
    let environment = attrs
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(" ");
    let stack = format!("ENVIRONMENT: {environment}\n{evidence}\n--- root cause ---\n{root_cause}");
    let mut event = Event::new(format!("[{category}] {diagnosis}"))
        .message(message.clone())
        .stack(stack)
        .severity(match severity {
            "error" => "high",
            _ => "medium",
        })
        .repo(repo)
        .fingerprint(qingniao::fingerprint(
            repo,
            "rust",
            &qingniao::log_template(&format!("{category}:{message}")),
        ));
    for (key, value) in attrs {
        event = event.attribute(key, value);
    }
    reporter.report(event);
}

/// Report an installer failure that never became a guardian case (spawn error,
/// non-zero exit with no classified line).
pub fn report_install_failure(phase: &str, error: &str) {
    let Some(reporter) = reporter() else {
        return;
    };
    let message = format!("{error}\n--- root cause ---\n{}", root_cause_line(error));
    let attrs = attributes("suite_install_failed", "installer", None, &message);
    let environment = attrs
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(" ");
    let mut event = Event::new(format!("[install] {phase}"))
        .message(message.clone())
        .stack(format!("ENVIRONMENT: {environment}\n{error}"))
        .severity("high")
        .repo(SUITE_REPO)
        .fingerprint(qingniao::fingerprint(
            SUITE_REPO,
            "rust",
            &qingniao::log_template(&format!("install:{phase}:{message}")),
        ));
    for (key, value) in attrs {
        event = event.attribute(key, value);
    }
    reporter.report(event);
}

fn repo_for(category: &str, evidence: &str, diagnosis: &str) -> Option<&'static str> {
    match category {
        "service_unreachable" | "auth_401" | "port_in_use" => return None,
        _ => {}
    }
    let text = format!("{evidence}\n{diagnosis}");
    for (package, repo) in PLUGIN_REPOS {
        if text.contains(package) {
            return Some(repo);
        }
    }
    match category {
        "node_crash" | "module_missing" | "plugin_dep_missing" => Some(APP_REPO),
        _ => Some(SUITE_REPO),
    }
}

fn attributes(
    category: &str,
    source: &str,
    action: Option<&str>,
    message: &str,
) -> BTreeMap<String, String> {
    let mut attributes = BTreeMap::new();
    attributes.insert("phase".to_string(), source.to_string());
    attributes.insert("category".to_string(), category.to_string());
    if let Some(action) = action {
        attributes.insert("guardian_action".to_string(), action.to_string());
    }
    if let Some(version) = crate::install::installed_dsh_version() {
        attributes.insert("dsh_version".to_string(), version);
    }
    attributes.insert(
        "mode".to_string(),
        if crate::runtime::portable_root().is_some() {
            "portable".to_string()
        } else {
            "installed".to_string()
        },
    );
    if let Some(code) = error_code(message) {
        attributes.insert("error_code".to_string(), code);
    }
    attributes
}

/// First `ERR_*`/`E*` error code in the text (e.g. `ERR_PNPM_MISSING_TARBALL_INTEGRITY`).
fn error_code(text: &str) -> Option<String> {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .find(|token| token.starts_with("ERR_") && token.len() <= 64)
        .map(str::to_string)
}

/// Guardian evidence is the matched line; installation failures print the real
/// cause a few lines earlier. Return the last `ERR_*` / `Error:` line from the
/// log tail, or empty when the evidence already carries one.
fn root_cause_line(evidence: &str) -> String {
    if evidence.contains("ERR_") || evidence.contains("Error:") {
        return String::new();
    }
    let path = crate::runtime::log_file();
    let Ok(mut file) = std::fs::File::open(&path) else {
        return String::new();
    };
    let len = file.metadata().map(|meta| meta.len()).unwrap_or(0);
    let window = 256 * 1024u64;
    let start = len.saturating_sub(window);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return String::new();
    }
    let mut buffer = Vec::new();
    if file.take(window).read_to_end(&mut buffer).is_err() {
        return String::new();
    }
    String::from_utf8_lossy(&buffer)
        .lines()
        .rev()
        .find(|line| {
            line.contains("ERR_")
                || line.contains("Error:")
                || line.contains("Install FAILED")
                || line.contains("missing peer")
        })
        .map(|line| line.trim_start_matches('\u{feff}').trim().chars().take(800).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_environment_failures_are_not_shipped() {
        for category in ["service_unreachable", "auth_401", "port_in_use"] {
            assert_eq!(repo_for(category, "", ""), None, "{category}");
        }
    }

    #[test]
    fn a_named_plugin_is_attributed_to_its_own_repo() {
        assert_eq!(
            repo_for(
                "profile_bundle",
                "cannot resolve profile bundle \"@dsh-extra/dsh-actors\"",
                "",
            ),
            Some("github.com/lomehong/dsh-actors")
        );
        assert_eq!(
            repo_for("profile_bundle", "missing peer @dsh-extra/im-channel@0.1.9", ""),
            Some("github.com/lomehong/dsh-im-bot")
        );
    }

    #[test]
    fn installer_failures_default_to_the_suite_meta_repo() {
        assert_eq!(
            repo_for("suite_install_failed", "[数字分身] 已回滚 package.json 至装前快照", ""),
            Some(SUITE_REPO)
        );
        assert_eq!(
            repo_for("install", "install-all.bat exited 1", ""),
            Some(SUITE_REPO)
        );
    }

    #[test]
    fn error_code_is_extracted_from_log_lines() {
        assert_eq!(
            error_code("ERR_PNPM_MISSING_TARBALL_INTEGRITY Cannot install package"),
            Some("ERR_PNPM_MISSING_TARBALL_INTEGRITY".to_string())
        );
        assert_eq!(error_code("plain failure with no code"), None);
    }
}
