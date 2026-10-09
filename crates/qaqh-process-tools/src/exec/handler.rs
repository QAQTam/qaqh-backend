//! exec::handler — typed exec 工具与参数归一化。

use std::collections::BTreeMap;
use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::ExecProgressSender;
#[cfg(test)]
use crate::ToolResult;
use crate::ToolRisk;
use crate::file_mutate::{mutation_error, resolve_mutation_path};
use crate::tool_api::{ToolCallContext, ToolExecutionError, ToolMeta, TypedTool};
#[cfg(test)]
use crate::tool_api::{
    ToolError, ToolExecutionMetrics, ToolModelProjection, ToolOutcome, ToolOutputValue,
    ToolProjection,
};
#[cfg(test)]
use serde_json::Value;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
/// Run a shell command.
pub struct ExecArgs {
    /// Shell command string (runs via `shell`, default auto-detected).
    #[serde(default)]
    pub command: Option<String>,
    /// Extra args for command (see the harness system prompt for per-shell rules).
    #[serde(default)]
    pub args: Option<Vec<String>>,
    /// Shell for command: bash | zsh | sh | pwsh | powershell | cmd (default auto-detected: pwsh on Windows, bash elsewhere).
    #[serde(default)]
    pub shell: Option<String>,
    /// Workdir (default workspace root).
    #[serde(default)]
    pub cwd: Option<String>,
    /// Env overrides.
    #[serde(default)]
    pub env: Option<BTreeMap<String, String>>,
    /// Timeout secs (1-3600, default 30).
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// Background after secs -> backgrounded+process_id.
    #[serde(default)]
    pub background_after_secs: Option<u64>,
    /// Max output tokens (default 10000, range 100-50000).
    #[serde(default)]
    pub max_output_tokens: Option<u64>,
}

pub struct ExecTool;

impl TypedTool for ExecTool {
    type Args = ExecArgs;
    type Output = super::direct::ExecOutput;

    fn meta(&self) -> ToolMeta {
        ToolMeta::new(
            "exec",
            "Run a shell command. The command is wrapped by the selected shell (pwsh on Windows, bash elsewhere; shell= to override). Returns exit_code/output; long runs return process_id.",
            crate::permission::ToolCategory::Exec,
            ToolRisk::Destructive,
            Duration::from_secs(30),
        )
    }

    #[allow(clippy::result_large_err)] // ToolExecutionError is the frozen typed boundary.
    fn run(
        &self,
        ctx: &ToolCallContext,
        args: Self::Args,
    ) -> Result<Self::Output, ToolExecutionError> {
        let progress = ctx
            .progress
            .as_ref()
            .map(crate::tool_api::result::bridge_progress);
        run_exec(ctx, args, None, progress)
    }
}

fn exec_error(code: &str, message: impl Into<String>, hint: Option<&str>) -> ToolExecutionError {
    mutation_error(code, message, hint, json!({}))
}

#[allow(clippy::result_large_err)] // ToolExecutionError is the frozen typed boundary.
fn ensure_shell_available(shell: super::shell::Shell) -> Result<(), ToolExecutionError> {
    if shell_available(shell) {
        return Ok(());
    }
    Err(exec_error(
        "shell_not_found",
        format!("{} not found on this machine", shell.path()),
        Some(&format!("available shells: {}", available_shells())),
    ))
}

#[allow(clippy::result_large_err)] // ToolExecutionError is the frozen typed boundary.
fn resolve_shell(
    ctx: &ToolCallContext,
    requested: Option<&str>,
    fixed: Option<super::shell::Shell>,
) -> Result<super::shell::Shell, ToolExecutionError> {
    use super::shell::Shell;

    let resolve_named = |name: &str, source: &str| {
        Shell::from_name(name).ok_or_else(|| {
            exec_error(
                "unknown_shell",
                format!("unknown shell '{name}' from {source}"),
                Some(
                    "Use one of: pwsh, powershell, bash, bash4windows, zsh, sh, cmd. The default is auto-detected.",
                ),
            )
        })
    };

    if let Some(shell) = fixed {
        return Ok(shell);
    }

    if let Some(name) = requested.filter(|name| !name.trim().is_empty()) {
        return resolve_named(name.trim(), "exec shell");
    }

    if let Some(name) = ctx
        .exec_default_shell
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty() && !name.eq_ignore_ascii_case("auto"))
    {
        return resolve_named(name, "exec.default_shell");
    }

    if let Some(shell) = Shell::auto_candidates()
        .iter()
        .copied()
        .find(|shell| shell_available(*shell))
    {
        return Ok(shell);
    }

    Err(exec_error(
        "shell_not_found",
        "no supported shell found on this machine",
        Some("Install one of: pwsh, powershell, bash, zsh, sh, cmd."),
    ))
}

#[allow(clippy::too_many_arguments, clippy::result_large_err)] // 参数面来自 exec 的既有 wire 契约；错误边界为冻结 SDK 类型。
pub(crate) fn run_exec(
    ctx: &ToolCallContext,
    args: ExecArgs,
    fixed: Option<super::shell::Shell>,
    progress_tx: Option<ExecProgressSender>,
) -> Result<super::direct::ExecOutput, ToolExecutionError> {
    use super::direct::direct_exec_sandboxed;
    use super::shell::Shell;

    // ── Resolve shell command ──
    let Some(command) = args.command.as_deref() else {
        return Err(exec_error(
            "missing_command",
            "exec requires a command string",
            Some(r#"Example: {"command": "cargo check"}"#),
        ));
    };
    if command.trim().is_empty() {
        return Err(exec_error(
            "empty_command",
            "command string is empty",
            Some("Provide a shell command string."),
        ));
    }
    let shell_command = normalize_command_rg(command);
    let shell = resolve_shell(ctx, args.shell.as_deref(), fixed)?;
    let extra_args = args.args.as_deref().filter(|args| !args.is_empty());
    if extra_args.is_some() && shell == Shell::Cmd {
        return Err(exec_error(
            "args_not_supported",
            "args is only supported for bash/zsh/sh ($1/$@) and pwsh -CommandWithArgs ($args)",
            Some(
                "Use exec with shell bash/zsh/sh and args as string array (positional $1...), or shell pwsh.",
            ),
        ));
    }
    ensure_shell_available(shell)?;
    let argv = shell.derive_exec_args_with(&shell_command, extra_args);

    // ── Execution limits / cwd / env ──
    let policy_default = crate::hooks::exec_max_output_tokens().unwrap_or(u32::MAX);
    let max_output_tokens = args
        .max_output_tokens
        .filter(|&n| (100..=50000).contains(&n))
        .map(|n| n as u32)
        .unwrap_or(policy_default);
    let timeout_secs = args
        .timeout_secs
        .filter(|&n| n > 0 && n <= 3600)
        .unwrap_or_else(|| ctx.timeout.as_secs().clamp(1, 3600));
    let background_after_secs = args.background_after_secs.filter(|&n| n > 0 && n <= 3600);
    let cwd: Option<String> = args
        .cwd
        .as_deref()
        .map(|cwd| {
            let resolved = resolve_mutation_path(ctx, cwd);
            if resolved.is_empty() {
                cwd.to_string()
            } else {
                resolved
            }
        })
        .or_else(|| {
            let workspace = ctx.workspace_root.to_string_lossy();
            if workspace.is_empty() || workspace == "." {
                None
            } else {
                Some(workspace.to_string())
            }
        });
    let env: Option<Vec<(String, String)>> = args
        .env
        .map(|env| env.into_iter().collect())
        .filter(|pairs: &Vec<(String, String)>| !pairs.is_empty());
    let cancel = ctx.cancellation.shared_flag();
    let sandbox = ctx.sandbox_spec();

    let mut result = direct_exec_sandboxed(
        &argv,
        env.as_deref(),
        cwd.as_deref(),
        max_output_tokens,
        timeout_secs,
        background_after_secs,
        Some(cancel.as_ref()),
        progress_tx,
        &ctx.call_id,
        sandbox,
        &shell_command,
    );
    // 观测线纪律（事故 2026-09-02 预防）：检测 shell 命令中的后台派生 `&`，
    // 以强提示引导走 background_after_secs + process 工具的受控路径。
    if result.status == "completed" && detect_background_derivation(&shell_command) {
        result.output.push_str(BACKGROUND_DERIVATION_HINT);
    }
    Ok(result)
}

/// exec 单测入口：显式上下文 + args → v1 `ToolResult` 信封。
#[cfg(test)]
pub(crate) fn run_exec_for_test(
    ctx: &ToolCallContext,
    args: Value,
    fixed: Option<super::shell::Shell>,
) -> ToolResult {
    let parsed: ExecArgs = match serde_json::from_value(args.clone()) {
        Ok(parsed) => parsed,
        Err(error) => {
            return crate::json_err(
                "invalid_arguments",
                format!("invalid arguments: {error}"),
                "",
            );
        }
    };
    let progress = ctx
        .progress
        .as_ref()
        .map(crate::tool_api::result::bridge_progress);
    match run_exec(ctx, parsed, fixed, progress) {
        Ok(output) => exec_output_to_tool_result(&output, &args),
        Err(ToolExecutionError::Recoverable(error)) => tool_result_from_error(error),
        Err(ToolExecutionError::Fatal(fatal)) => {
            panic!("exec tool fatal: {}", fatal.message)
        }
    }
}

#[cfg(test)]
fn exec_output_to_tool_result(output: &super::direct::ExecOutput, args: &Value) -> ToolResult {
    let model_text = output.to_json();
    let outcome = ToolOutcome {
        status: output.status(),
        output: ToolOutputValue::Json(serde_json::to_value(output).unwrap_or_else(|_| json!({}))),
        error: output.error(),
        model: ToolModelProjection {
            text: model_text.clone(),
            truncated: false,
        },
        display: output.display(args),
        images: Vec::new(),
        metrics: ToolExecutionMetrics {
            elapsed: Duration::ZERO,
            output_bytes: model_text.len() as u64,
            retry_count: 0,
            effective_tool_name: None,
            user_initiated: false,
        },
        effects: Vec::new(),
    };
    outcome.to_tool_result()
}

#[cfg(test)]
fn tool_result_from_error(error: ToolError) -> ToolResult {
    let mut text = error.detail.clone();
    if let Some(hint) = error.hint.as_ref()
        && !hint.is_empty()
    {
        text.push_str("\nHint: ");
        text.push_str(hint);
    }
    let mut result = ToolResult::error_with(
        error.code.as_str(),
        text,
        error.retryable,
        error.hint.clone(),
    );
    result.data = error.details.unwrap_or_else(|| json!({}));
    result
}

/// ripgrep `-rn` 习惯陷阱防御（grep 迁移）。
pub(crate) fn normalize_command_rg(command: &str) -> String {
    use std::sync::OnceLock;
    static RG_HABIT_RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RG_HABIT_RE.get_or_init(|| {
        regex::Regex::new(r"(?i)\b(rg(\.exe)?)\s+-rn([a-z]*)").expect("rg habit regex")
    });
    re.replace_all(command, |caps: &regex::Captures| {
        let prog = &caps[1];
        let rest = caps.get(3).map(|m| m.as_str()).unwrap_or("");
        let cleaned = format!("{prog} -n{rest}");
        log::info!("[exec] rg habit fix (command): 'rg -rn{rest}' -> '{cleaned}'");
        cleaned
    })
    .into_owned()
}

pub(crate) fn shell_available(shell: super::shell::Shell) -> bool {
    shell.available()
}

pub(crate) fn available_shells() -> String {
    let mut list = Vec::new();
    for (name, shell) in [
        ("pwsh", super::shell::Shell::PowerShell),
        ("powershell", super::shell::Shell::WindowsPowerShell),
        ("bash", super::shell::Shell::Bash),
        ("zsh", super::shell::Shell::Zsh),
        ("sh", super::shell::Shell::Sh),
        ("cmd", super::shell::Shell::Cmd),
    ] {
        if shell_available(shell) {
            list.push(name);
        }
    }
    if list.is_empty() {
        "none detected".to_string()
    } else {
        list.join(", ")
    }
}

/// 后台派生强提示（观测线纪律，fd 持有复现实验 2026-09-06）：裸 `cmd &` 的
/// 孙进程持 fd1/fd2 = 管道写端，孤儿化 reparent 后长期滞留——阶段 2 的
/// settle 机制保证读线程有界退出，但输出完整性仍以重定向到文件 + 受控移交
/// （background_after_secs + process 工具）为正道。
pub(crate) const BACKGROUND_DERIVATION_HINT: &str = "\n[!] 后台派生检测：命令包含 `&`（后台任务）。后台/孙进程可能持有输出管道，导致本结果遗漏其后继输出。长驻服务请改用 background_after_secs 参数移交后台，并以 process 工具（check/wait/kill）接管；后台命令应将 stdout/stderr 重定向到文件（证据：docs/current/debug-backlog.md）。\n";

/// 检测 shell 命令中的后台派生操作符。
/// 剥除逻辑与（`&&`）与重定向组合（`>&`/`&>`，覆盖 `2>&1`）后残留的
/// `&` 才是后台派生；引号内的 `&`（sed/awk 等）会误报——提示为建议性
/// 输出，宁滥勿缺。
pub(crate) fn detect_background_derivation(command: &str) -> bool {
    command
        .replace("&&", "")
        .replace(">&", "")
        .replace("&>", "")
        .contains('&')
}

// ── Registration ──
