//! exec::handler — typed exec 工具、参数归一化与 legacy 兼容入口。

use std::collections::BTreeMap;
use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::ExecProgressSender;
use crate::ToolRisk;
use crate::file_mutate::{mutation_error, resolve_mutation_path};
#[cfg(test)]
use crate::tool_api::{
    AgentMode, CancellationToken, SandboxMode, ToolCallSource, ToolError, ToolExecutionMetrics,
    ToolModelProjection, ToolOutcome, ToolOutputValue, ToolProjection,
};
use crate::tool_api::{
    OutputBudget, ToolCallContext, ToolDescriptor, ToolExecutionError, ToolExposure, ToolName,
    ToolSource, TypedTool,
};
#[cfg(test)]
use crate::{ToolCallCtx, ToolResult};
#[cfg(test)]
use serde_json::Value;
#[cfg(test)]
use std::path::PathBuf;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExecArgs {
    #[serde(default)]
    pub argv: Option<Vec<String>>,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Option<Vec<String>>,
    #[serde(default)]
    pub shell: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub background_after_secs: Option<u64>,
    #[serde(default)]
    pub max_output_tokens: Option<u64>,
}

pub struct ExecTool;

impl TypedTool for ExecTool {
    type Args = ExecArgs;
    type Output = super::direct::ExecOutput;

    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: ToolName::new("exec").expect("valid exec tool name"),
            display_name: None,
            description: "Run a command. argv = direct exec without a shell; command = shell string (pwsh on Windows, bash elsewhere; shell= to override). Returns exit_code/output; long runs return process_id."
                .to_string(),
            input_schema: exec_schema(true),
            output_schema: serde_json::to_value(schemars::schema_for!(super::direct::ExecOutput))
                .expect("exec output schema"),
            category: crate::permission::ToolCategory::Exec,
            risk: ToolRisk::Destructive,
            default_timeout: Duration::from_secs(30),
            exposure: ToolExposure::Direct,
            source: ToolSource::Builtin,
            output_budget: OutputBudget::default(),
            capabilities: crate::tool_capabilities::builtin_capabilities("exec")
                .unwrap_or_default(),
        }
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
            .map(crate::tool_api::legacy::bridge_progress);
        run_exec(ctx, args, None, progress)
    }
}

fn exec_error(code: &str, message: impl Into<String>, hint: Option<&str>) -> ToolExecutionError {
    mutation_error(code, message, hint, json!({}))
}

#[allow(clippy::too_many_arguments, clippy::result_large_err)] // 参数面来自 exec 的既有 wire 契约；错误边界为冻结 SDK 类型。
fn run_exec(
    ctx: &ToolCallContext,
    args: ExecArgs,
    fixed: Option<super::shell::Shell>,
    progress_tx: Option<ExecProgressSender>,
) -> Result<super::direct::ExecOutput, ToolExecutionError> {
    use super::direct::direct_exec_sandboxed;
    use super::shell::Shell;

    // ── Resolve argv ──
    let shell_command = args.command.clone();
    let argv: Vec<String> = if let Some(command) = shell_command.as_deref() {
        if command.is_empty() {
            return Err(exec_error(
                "EMPTY_COMMAND",
                "command string is empty",
                Some("Provide a shell command string."),
            ));
        }
        let command = normalize_command_rg(command);
        let shell = match fixed {
            Some(shell) => {
                if !shell_available(shell) {
                    return Err(exec_error(
                        "SHELL_NOT_FOUND",
                        format!("{} not found on this machine", shell.path()),
                        Some(&format!("available shells: {}", available_shells())),
                    ));
                }
                shell
            }
            None => match args.shell.as_deref() {
                Some(name) if !name.is_empty() => match Shell::from_name(name) {
                    Some(shell) => shell,
                    None => {
                        return Err(exec_error(
                            "UNKNOWN_SHELL",
                            format!("unknown shell '{name}'"),
                            Some(
                                "Use one of: bash, zsh, sh, pwsh, powershell, cmd. The default is auto-detected (pwsh on Windows, bash elsewhere).",
                            ),
                        ));
                    }
                },
                _ => Shell::detect(),
            },
        };
        let extra_args = args.args.as_deref().filter(|args| !args.is_empty());
        if extra_args.is_some() && shell == Shell::Cmd {
            return Err(exec_error(
                "ARGS_NOT_SUPPORTED",
                "args is only supported for bash/zsh/sh ($1/$@) and pwsh -CommandWithArgs ($args)",
                Some(
                    "Use exec with shell bash/zsh/sh and args as string array (positional $1...), or shell pwsh.",
                ),
            ));
        }
        shell.derive_exec_args_with(&command, extra_args)
    } else {
        if let Some(name) = args.shell.as_deref().filter(|name| !name.is_empty()) {
            return Err(exec_error(
                "ARGV_IGNORES_SHELL",
                format!("argv mode runs direct exec without a shell; 'shell: {name}' is ignored"),
                Some("Use command (not argv) to run through a shell, or drop the shell parameter."),
            ));
        }
        if args.args.as_ref().is_some_and(|args| !args.is_empty()) {
            return Err(exec_error(
                "ARGV_IGNORES_ARGS",
                "argv mode runs direct exec without a shell; 'args' only applies to command mode",
                Some("Use command + args (positional $1... / $args), or drop args."),
            ));
        }
        let Some(mut argv) = args.argv.clone() else {
            return Err(exec_error(
                "MISSING_ARGV",
                "exec requires argv or command",
                Some(r#"Example: {"argv": ["cargo", "check"]} or {"command": "cargo check"}"#),
            ));
        };
        normalize_rg_argv(&mut argv);
        argv
    };
    if argv.is_empty() {
        return Err(exec_error(
            "EMPTY_ARGV",
            "argv array is empty",
            Some("Provide at least one element."),
        ));
    }

    // ── Execution limits / cwd / env ──
    let policy_default = crate::tool_side_fold::policy()
        .exec_max_output_tokens()
        .unwrap_or(u32::MAX);
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
        &sandbox,
    );
    // 观测线纪律（事故 2026-09-02 预防）：检测 shell 命令中的后台派生 `&`，
    // 以强提示引导走 background_after_secs + process 工具的受控路径。
    if let Some(command) = shell_command.as_deref()
        && result.status == "completed"
        && detect_background_derivation(command)
    {
        result.output.push_str(BACKGROUND_DERIVATION_HINT);
    }
    Ok(result)
}

#[cfg(test)]
fn context_from_legacy(ctx: &ToolCallCtx) -> ToolCallContext {
    let workspace = crate::current_workspace();
    let workspace_root = if workspace.is_empty() {
        PathBuf::from(".")
    } else {
        PathBuf::from(workspace)
    };
    // Legacy unit tests share process-global workspace state; a prior tempdir
    // may already be gone. Keep the test adapter executable by falling back to
    // the repository cwd instead of handing a stale root to the sandbox.
    let workspace_root = if workspace_root.exists() {
        workspace_root
    } else {
        std::env::current_dir().unwrap_or(workspace_root)
    };
    ToolCallContext {
        call_id: ctx.id.clone(),
        session_id: crate::current_session().unwrap_or_default(),
        workspace_root,
        mode: match crate::runtime::current_mode() {
            1 => AgentMode::Plan,
            _ => AgentMode::Code,
        },
        permission_level: crate::runtime::context()
            .map(|context| crate::permission::PermissionLevel::from_u8(context.permission_level))
            .unwrap_or(crate::permission::PermissionLevel::MaxLockdown),
        sandbox: if crate::authorization::is_subagent_sandbox() {
            SandboxMode::Subagent
        } else {
            SandboxMode::Main
        },
        timeout: Duration::from_secs(ctx.timeout_secs.unwrap_or(30)),
        cancellation: CancellationToken::from_shared_flag(ctx.cancel.clone()),
        progress: None,
        source: ToolCallSource::Model,
    }
}

#[cfg(test)]
pub(crate) fn handle_run_exec(ctx: ToolCallCtx) -> ToolResult {
    handle_run_with_shell(ctx, None)
}

/// Compatibility entry retained for existing in-process callers/tests.
#[cfg(test)]
pub(crate) fn handle_run_with_shell(
    ctx: ToolCallCtx,
    fixed: Option<super::shell::Shell>,
) -> ToolResult {
    let args: ExecArgs = match serde_json::from_value(ctx.args.clone()) {
        Ok(args) => args,
        Err(error) => {
            return crate::json_err(
                "INVALID_ARGUMENTS",
                format!("invalid arguments: {error}"),
                "",
            );
        }
    };
    let call_ctx = context_from_legacy(&ctx);
    match run_exec(&call_ctx, args, fixed, ctx.tx_progress.clone()) {
        Ok(output) => exec_output_to_tool_result(&output, &ctx.args),
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
pub(crate) fn normalize_rg_argv(argv: &mut [String]) {
    if !matches!(
        argv.first().map(|p| p.to_lowercase()).as_deref(),
        Some("rg") | Some("rg.exe")
    ) {
        return;
    }
    for arg in argv.iter_mut().skip(1) {
        if let Some(rest) = arg.strip_prefix("-rn") {
            let cleaned = format!("-n{rest}");
            log::info!("[exec] rg habit fix (argv): '{arg}' -> '{cleaned}'");
            *arg = cleaned;
        }
    }
}

/// command 模式版本：在 shell 命令字符串里改写 `rg -rn...` → `rg -n...`。
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
        ("bash", super::shell::Shell::Bash),
        ("pwsh", super::shell::Shell::PowerShell),
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
pub(crate) const BACKGROUND_DERIVATION_HINT: &str = "\n[!] 后台派生检测：命令包含 `&`（后台任务）。后台/孙进程可能持有输出管道，导致本结果遗漏其后继输出。长驻服务请改用 background_after_secs 参数移交后台，并以 process 工具（check/wait/kill）接管；后台命令应将 stdout/stderr 重定向到文件（证据：docs/incidents/2026-09-06-fd-hold-repro.md）。\n";

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

/// exec / bash / pwsh 共享的 input schema 模板。
/// `with_shell` 控制是否暴露 `shell` 参数（仅 exec 通用入口）。
pub(crate) fn exec_schema(with_shell: bool) -> serde_json::Value {
    let mut props = serde_json::Map::new();
    props.insert(
        "argv".into(),
        serde_json::json!({ "type": "array", "items": {"type": "string"}, "description": "Direct exec without a shell (e.g. [\"cargo\", \"check\"]); must not combine with shell/args" }),
    );
    props.insert(
        "command".into(),
        serde_json::json!({ "type": "string", "description": "Shell command string (runs via `shell`, default auto-detected)" }),
    );
    props.insert(
        "args".into(),
        serde_json::json!({ "type": "array", "items": {"type": "string"}, "description": "Extra args for command: bash/zsh/sh fills $1/$2/$@ ($0 is placeholder `_`); pwsh fills $args (-CommandWithArgs; read as $args[0]/$args.Count, $argN does not exist). cmd does not support args." }),
    );
    if with_shell {
        props.insert(
            "shell".into(),
            serde_json::json!({ "type": "string", "enum": ["bash", "zsh", "sh", "pwsh", "powershell", "cmd"], "description": "Shell for command (default auto-detected: pwsh on Windows, bash elsewhere)" }),
        );
    }
    props.insert(
        "cwd".into(),
        serde_json::json!({"type": "string", "description": "Workdir (default workspace root)"}),
    );
    props.insert(
        "env".into(),
        serde_json::json!({"type": "object", "additionalProperties": {"type": "string"}, "description": "Env overrides"}),
    );
    props.insert(
        "timeout_secs".into(),
        serde_json::json!({"type": "integer", "description": "Timeout secs (1-3600, default 30)"}),
    );
    props.insert(
        "background_after_secs".into(),
        serde_json::json!({"type": "integer", "description": "Background after secs -> backgrounded+process_id"}),
    );
    props.insert(
        "max_output_tokens".into(),
        serde_json::json!({ "type": "integer", "description": "Max output tokens (10000, 100-50000)" }),
    );
    serde_json::json!({
        "type": "object",
        "properties": props,
        "required": [],
        "additionalProperties": false,
        "oneOf": [
            {"required": ["argv"]},
            {"required": ["command"]}
        ]
    })
}
