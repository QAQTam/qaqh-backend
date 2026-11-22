//! Hook 执行器:command 子进程与 http 端点两条通道。
//!
//! 决策语义(三家共同口径):
//! - exit 0 + stdout JSON = 结构化决策(JSON 缺席 = allow/放行);
//! - 门事件 exit 2 = 无条件 deny/block,**stdout 忽略**;
//! - 其余失败 = fail-open,首行 stderr 进记录;
//! - JSON 决策与 exit 2 冲突时 exit 2 赢(JSON allow 也不放行)。
//!
//! env:保留变量(`QQH_HOOK_*` + Claude 兼容 `CLAUDE_PROJECT_DIR`)恒为运行时
//! 注入值,handler `env` 不可覆盖。

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;

use tokio::io::AsyncWriteExt;

use crate::config::HandlerKind;
use crate::event::{HookEventEnvelope, GATE_EXIT_CODE, MAX_OUTPUT_SIZE};

/// 运行时上下文:runner 注入保留 env 用。
#[derive(Debug, Clone)]
pub struct RunContext {
    pub session_id: String,
    pub workspace_root: String,
    pub cwd: String,
    pub permission_mode: Option<String>,
    pub turn_id: Option<String>,
    pub call_id: Option<String>,
}

/// 保留 env 键;handler env 里出现即丢弃并警告。
pub const RESERVED_ENV_KEYS: &[&str] = &[
    "QQH_HOOK_EVENT",
    "QQH_HOOK_NAME",
    "QQH_SESSION_ID",
    "QQH_WORKSPACE_ROOT",
    "QQH_HOOK_TURN_ID",
    "QQH_HOOK_CALL_ID",
    "CLAUDE_PROJECT_DIR",
];

#[derive(Debug)]
pub enum RunnerOutcome {
    /// 正常退出(含 exit 2)。
    Completed {
        exit_code: i32,
        stdout: String,
        stderr_first_line: Option<String>,
    },
    TimedOut,
    SpawnFailed(String),
    /// http 非 2xx。
    HttpRejected(u16, String),
}

impl RunnerOutcome {
    /// 门事件语义:exit 2(或 http 非 2xx 等价)视为 block/deny 信号。
    pub fn is_gate_exit(&self) -> bool {
        matches!(self, Self::Completed { exit_code, .. } if *exit_code == GATE_EXIT_CODE)
    }

    pub fn succeeded(&self) -> bool {
        matches!(self, Self::Completed { exit_code, .. } if *exit_code == 0)
    }

    pub fn summary(&self) -> Option<String> {
        match self {
            Self::Completed {
                stdout,
                stderr_first_line,
                ..
            } => stderr_first_line
                .clone()
                .or_else(|| trimmed_non_empty(stdout)),
            Self::TimedOut => Some("hook timed out".to_string()),
            Self::SpawnFailed(error) => Some(error.clone()),
            Self::HttpRejected(status, body) => Some(format!("http {status}: {body}")),
        }
    }
}

fn trimmed_non_empty(text: &str) -> Option<String> {
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// 执行一个 handler(command/http)。返回原始结果;决策解析在 dispatcher。
pub async fn run_handler(
    entry: &crate::discovery::HookEntry,
    envelope: &HookEventEnvelope,
    ctx: &RunContext,
    timeout_secs: u64,
) -> RunnerOutcome {
    match entry.handler_kind {
        HandlerKind::Command => run_command(entry, envelope, ctx, timeout_secs).await,
        HandlerKind::Http => run_http(entry, envelope, timeout_secs).await,
    }
}

async fn run_command(
    entry: &crate::discovery::HookEntry,
    envelope: &HookEventEnvelope,
    ctx: &RunContext,
    timeout_secs: u64,
) -> RunnerOutcome {
    let command = entry.handler.command.as_deref().unwrap_or_default();
    let mut cmd = build_command(command, &entry.source_dir);
    cmd.envs(user_env(&entry.handler.env))
        .envs(reserved_env(entry, envelope, ctx))
        .current_dir(&entry.source_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let payload = envelope.to_hook_json().to_string();
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(error) => return RunnerOutcome::SpawnFailed(format!("spawn `{command}`: {error}")),
    };

    // stdin 先写完再关:多数 hook 脚本按 EOF 读。
    let mut stdin = child.stdin.take().expect("stdin piped");
    let write_result = stdin.write_all(payload.as_bytes()).await;
    drop(stdin);
    if let Err(error) = write_result {
        log::warn!("[hooks] {} stdin write: {error}", entry.name);
    }

    let wait = async {
        let output = child.wait_with_output().await;
        output
    };

    match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), wait).await {
        Ok(Ok(output)) => {
            let stdout = clip_output(&String::from_utf8_lossy(&output.stdout));
            let stderr = String::from_utf8_lossy(&output.stderr);
            RunnerOutcome::Completed {
                exit_code: output.status.code().unwrap_or(-1),
                stdout,
                stderr_first_line: stderr
                    .lines()
                    .find(|line| !line.trim().is_empty())
                    .map(|line| line.chars().take(200).collect()),
            }
        }
        Ok(Err(error)) => RunnerOutcome::SpawnFailed(format!("wait `{command}`: {error}")),
        Err(_) => RunnerOutcome::TimedOut,
    }
}

async fn run_http(
    entry: &crate::discovery::HookEntry,
    envelope: &HookEventEnvelope,
    timeout_secs: u64,
) -> RunnerOutcome {
    let url = entry.handler.url.clone().unwrap_or_default();
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .build()
    {
        Ok(client) => client,
        Err(error) => return RunnerOutcome::SpawnFailed(format!("http client: {error}")),
    };
    let response = match client
        .post(&url)
        .json(&envelope.to_hook_json())
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => return RunnerOutcome::SpawnFailed(format!("http post {url}: {error}")),
    };
    let status = response.status().as_u16();
    let body = response
        .text()
        .await
        .map(|text| clip_output(&text))
        .unwrap_or_default();
    if (200..300).contains(&status) {
        RunnerOutcome::Completed {
            exit_code: 0,
            stdout: body,
            stderr_first_line: None,
        }
    } else {
        RunnerOutcome::HttpRejected(status, body)
    }
}

/// shell 元字符判定:含其一则经 shell 解释,否则直接 spawn(相对 source_dir)。
const SHELL_METACHARS: &[char] = &[
    '|', '&', ';', '<', '>', '$', '`', '\\', '"', '\'', '*', '?', '[', ']', '#', '~', '(', ')',
    '{', '}', '!', '\n', ' ',
];

fn build_command(command: &str, source_dir: &Path) -> tokio::process::Command {
    let needs_shell = command.chars().any(|c| SHELL_METACHARS.contains(&c));
    if needs_shell {
        #[cfg(windows)]
        {
            let mut cmd = tokio::process::Command::new("cmd.exe");
            cmd.arg("/S").arg("/C").arg(command);
            cmd
        }
        #[cfg(not(windows))]
        {
            let mut cmd = tokio::process::Command::new("sh");
            cmd.arg("-c").arg(command);
            cmd
        }
    } else {
        let resolved = source_dir.join(command);
        tokio::process::Command::new(resolved)
    }
}

fn user_env(env: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    env.iter()
        .filter(|(key, _)| {
            let reserved = RESERVED_ENV_KEYS.contains(&key.as_str());
            if reserved {
                log::warn!("[hooks] env `{key}` is reserved; value ignored");
            }
            !reserved
        })
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

fn reserved_env(
    entry: &crate::discovery::HookEntry,
    envelope: &HookEventEnvelope,
    ctx: &RunContext,
) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert("QQH_HOOK_EVENT".to_string(), envelope.hook_event_name.key().to_string());
    env.insert("QQH_HOOK_NAME".to_string(), entry.name.clone());
    env.insert("QQH_SESSION_ID".to_string(), ctx.session_id.clone());
    env.insert("QQH_WORKSPACE_ROOT".to_string(), ctx.workspace_root.clone());
    // Claude 兼容别名。
    env.insert("CLAUDE_PROJECT_DIR".to_string(), ctx.workspace_root.clone());
    if let Some(turn_id) = &ctx.turn_id {
        env.insert("QQH_HOOK_TURN_ID".to_string(), turn_id.clone());
    }
    if let Some(call_id) = &ctx.call_id {
        env.insert("QQH_HOOK_CALL_ID".to_string(), call_id.clone());
    }
    env
}

fn clip_output(text: &str) -> String {
    if text.len() <= MAX_OUTPUT_SIZE {
        return text.to_string();
    }
    let mut clipped: String = text.chars().take(MAX_OUTPUT_SIZE).collect();
    clipped.push_str("\n[qaqh-hooks: output truncated]");
    clipped
}

/// stdout JSON 决策(`{"decision": ...}` 形状);解析失败返回 None。
#[derive(Debug, Clone, Default)]
pub struct DecisionJson {
    pub decision: Option<String>,
    pub reason: Option<String>,
    pub updated_input: Option<serde_json::Value>,
    pub additional_context: Option<String>,
    pub updated_tool_output: Option<String>,
}

impl DecisionJson {
    pub fn parse(stdout: &str) -> Option<Self> {
        let trimmed = stdout.trim();
        if trimmed.is_empty() || !trimmed.starts_with('{') {
            return None;
        }
        let value: serde_json::Value = serde_json::from_str(trimmed).ok()?;
        Some(Self {
            decision: value.get("decision").and_then(|v| v.as_str()).map(str::to_string),
            reason: value.get("reason").and_then(|v| v.as_str()).map(str::to_string),
            updated_input: value.get("updatedInput").cloned().or_else(|| {
                value.get("updated_input").cloned()
            }),
            additional_context: value
                .get("additionalContext")
                .or_else(|| value.get("additional_context"))
                .and_then(|v| v.as_str())
                .map(str::to_string),
            updated_tool_output: value
                .get("updatedToolOutput")
                .or_else(|| value.get("updated_tool_output"))
                .and_then(|v| v.as_str())
                .map(str::to_string),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn decision_json_parses_both_casings() {
        let parsed = DecisionJson::parse(&json!({
            "decision": "deny", "reason": "no rm", "updatedInput": {"a": 1}
        }).to_string()).unwrap();
        assert_eq!(parsed.decision.as_deref(), Some("deny"));
        assert_eq!(parsed.reason.as_deref(), Some("no rm"));
        assert!(parsed.updated_input.is_some());

        let snake = DecisionJson::parse(&json!({
            "decision": "allow", "updated_input": {"b": 2}, "additional_context": "hi"
        }).to_string()).unwrap();
        assert!(snake.updated_input.is_some());
        assert_eq!(snake.additional_context.as_deref(), Some("hi"));
    }

    #[test]
    fn decision_json_ignores_non_json() {
        assert!(DecisionJson::parse("").is_none());
        assert!(DecisionJson::parse("plain text").is_none());
        assert!(DecisionJson::parse("[1,2]").is_none());
    }

    #[test]
    fn user_env_drops_reserved_keys() {
        let mut env = BTreeMap::new();
        env.insert("QQH_SESSION_ID".to_string(), "spoof".to_string());
        env.insert("MY_VAR".to_string(), "ok".to_string());
        let filtered = user_env(&env);
        assert!(!filtered.contains_key("QQH_SESSION_ID"));
        assert_eq!(filtered.get("MY_VAR").map(String::as_str), Some("ok"));
    }
}
