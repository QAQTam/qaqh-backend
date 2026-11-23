use std::time::Duration;

use qaqh_tool_core::tool_api::{
    ExecCommandRequest, ExecCommandResult, ToolCallContext, ToolError, ToolErrorKind,
    ToolExecutionError, ToolMeta, ToolProjection, ToolStatus, TypedTool, WriteStdinRequest,
};
use qaqh_types::{ExecSessionId, ToolCallOutcome};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExecCommandArgs {
    /// Shell command to execute.
    pub cmd: String,
    /// Working directory. Defaults to the conversation workspace.
    #[serde(default)]
    pub workdir: Option<String>,
    /// Shell dialect used to wrap `cmd` — write `cmd` in that dialect. One of
    /// pwsh | bash | bash4windows | zsh | sh. Omit for auto-detect. NEVER put
    /// a payload interpreter (node/bun/python/deno) here — run it inside
    /// `cmd`, e.g. cmd="node build.js".
    #[serde(default)]
    pub shell: Option<ShellName>,
    /// Start a login shell. Supported for bash, zsh, and sh.
    #[serde(default)]
    pub login: bool,
    /// Allocate a PTY for terminal semantics. Defaults to true; set false to use pipes.
    #[serde(default = "default_tty")]
    pub tty: bool,
    /// Initial wait window, 250..=30000 ms. Defaults to 10000.
    #[serde(default)]
    pub yield_time_ms: Option<u32>,
    /// Hard process-tree deadline. Null leaves the execution unbounded.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Maximum model output budget in Unicode characters.
    #[serde(default)]
    pub max_output_chars: Option<u32>,
    /// Request escalated (unsandboxed) execution with explicit user approval.
    /// Do NOT set this on your first attempt unless you already know sandbox
    /// isolation blocks the required operation; run normally in the sandbox first.
    #[serde(default)]
    pub with_escalated_permissions: Option<bool>,
    /// Concise human-readable summary explaining why escalated permissions outside
    /// the sandbox are required (displayed on the user's approval card). Required
    /// when `with_escalated_permissions` is true.
    #[serde(default)]
    pub justification: Option<String>,
}

/// exec_command `shell` 字段的白名单——这个 enum **就是**模型面 schema：
/// `powershell`（Windows PowerShell 5.1，无 `&&` 语句分隔符）与 `cmd` 只保留
/// 给 config `exec.default_shell` 与 bare-machine 兜底降级链，不再出现在
/// 模型面。任何 payload 解释器（node/bun/python）是 `cmd` 里运行的程序，
/// 不是包装 shell（历史上模型把 shell 字段当「payload 解释器选择器」填
/// node/python → unknown_shell）。
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ShellName {
    /// POSIX bash. On Windows resolves a native bash-compatible engine when
    /// present, else Git-for-Windows/MSYS2 bash; never the WSL wrapper.
    Bash,
    /// Explicit Windows alias of `bash` (same resolution; disambiguates when platform guessing is risky).
    #[serde(rename = "bash4windows")]
    Bash4Windows,
    /// zsh (macOS default).
    Zsh,
    /// POSIX sh.
    Sh,
    /// PowerShell 7+ (`pwsh.exe`).
    Pwsh,
}

impl ShellName {
    /// 映射回 [`crate::exec::shell::Shell::from_name`] 接受的显式壳名
    /// （resolve_shell 的 requested 路径按名解析）。
    pub(crate) fn config_name(&self) -> &'static str {
        match self {
            ShellName::Bash | ShellName::Bash4Windows => "bash",
            ShellName::Zsh => "zsh",
            ShellName::Sh => "sh",
            ShellName::Pwsh => "pwsh",
        }
    }
}

fn default_tty() -> bool {
    true
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WriteStdinArgs {
    pub exec_session_id: String,
    #[serde(default)]
    pub chars: String,
    /// Read wait window. Empty input (pure read): 5000..=300000 ms, default
    /// 5000 — a single long poll waits for the process and returns as soon as
    /// it exits; prefer one large yield_time_ms over repeated short polls.
    /// Non-empty input (send keystrokes): 250..=30000 ms, default 250.
    /// The two floors are not interchangeable — e.g. 3000 ms is rejected when chars is empty.
    #[serde(default)]
    pub yield_time_ms: Option<u32>,
    #[serde(default)]
    pub max_output_chars: Option<u32>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ExecSessionToolOutput {
    pub outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exec_session_id: Option<String>,
    pub output: String,
    pub output_truncated: bool,
    pub omitted_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub cancelled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_bytes_written: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    #[serde(skip)]
    #[schemars(skip)]
    call_outcome: ToolCallOutcome,
}

impl From<ExecCommandResult> for ExecSessionToolOutput {
    fn from(result: ExecCommandResult) -> Self {
        let (outcome, error_code, error_message) = match &result.outcome {
            ToolCallOutcome::Completed => ("completed", None, None),
            ToolCallOutcome::Yielded { .. } => ("yielded", None, None),
            ToolCallOutcome::Failed { error } => (
                "failed",
                Some(error.code.clone()),
                Some(error.message.clone()),
            ),
            ToolCallOutcome::Cancelled { reason } => (
                "cancelled",
                Some("cancelled".into()),
                Some(format!("execution call cancelled: {reason:?}")),
            ),
            ToolCallOutcome::Interrupted { reason } => (
                "interrupted",
                Some("interrupted".into()),
                Some(format!("execution call interrupted: {reason:?}")),
            ),
        };
        Self {
            outcome: outcome.into(),
            exec_session_id: result.exec_session_id.map(|id| id.to_string()),
            output: result.output,
            output_truncated: result.output_truncated,
            omitted_bytes: result.omitted_bytes,
            exit_code: result.exit_code,
            timed_out: result.timed_out,
            cancelled: result.cancelled,
            input_bytes_written: result.input_bytes_written,
            error_code,
            error_message,
            call_outcome: result.outcome,
        }
    }
}

impl ToolProjection for ExecSessionToolOutput {
    fn call_outcome(&self) -> Option<ToolCallOutcome> {
        Some(self.call_outcome.clone())
    }

    fn status(&self) -> ToolStatus {
        if self.call_outcome.is_success() {
            ToolStatus::Ok
        } else {
            ToolStatus::Error
        }
    }

    fn error(&self) -> Option<ToolError> {
        let message = self.error_message.as_deref()?;
        let kind = match self.call_outcome {
            ToolCallOutcome::Cancelled { .. } => ToolErrorKind::Cancelled,
            ToolCallOutcome::Failed { .. } | ToolCallOutcome::Interrupted { .. } => {
                ToolErrorKind::Execution
            }
            ToolCallOutcome::Completed | ToolCallOutcome::Yielded { .. } => return None,
        };
        let mut error = ToolError::new(kind, message);
        error.details = self
            .error_code
            .as_ref()
            .map(|code| serde_json::json!({"code": code}));
        Some(error)
    }
}

pub struct ExecCommandTool;

impl TypedTool for ExecCommandTool {
    type Args = ExecCommandArgs;
    type Output = ExecSessionToolOutput;

    fn meta(&self) -> ToolMeta {
        ToolMeta::new(
            "exec_command",
            "Run a shell command inside the OS sandbox by default. `cmd` is wrapped by the selected shell dialect (`shell`); long-running commands return an execution session id — use write_stdin to read output or provide terminal input. To wait for a long-running command, issue a single empty write_stdin with a large yield_time_ms (up to 300000): it returns as soon as the process exits. If an operation is blocked by the sandbox and genuinely requires unsandboxed access, retry with `with_escalated_permissions: true` and a concise `justification` for the user's approval card. stdout/stderr are captured as separate streams; avoid `2>&1`/`1>&2` unless merging is intentional. Pagers and git prompts/editors are disarmed via env (PAGER/GIT_PAGER/GH_PAGER=cat, GIT_TERMINAL_PROMPT=0, GIT_EDITOR=true); output ending in `:` means stuck in a pager — send `q` via write_stdin. Payload interpreters (node/bun/python/deno) are programs to run inside `cmd`, never a value for `shell`.",
            crate::permission::ToolCategory::Exec,
            crate::ToolRisk::Destructive,
            Duration::from_secs(30),
        )
    }

    #[allow(clippy::result_large_err)]
    fn run(
        &self,
        context: &ToolCallContext,
        args: Self::Args,
    ) -> Result<Self::Output, ToolExecutionError> {
        if args.cmd.trim().is_empty() {
            return Err(ToolError::invalid_arguments("cmd must not be empty").into());
        }
        let yield_time_ms = args.yield_time_ms.unwrap_or(10_000);
        if !(250..=30_000).contains(&yield_time_ms) {
            return Err(ToolError::invalid_arguments(
                "yield_time_ms must be between 250 and 30000",
            )
            .into());
        }
        let max_output_chars = output_budget(context.exec_max_output_chars, args.max_output_chars)?;
        let shell = crate::exec::handler::resolve_shell(
            context,
            args.shell.as_ref().map(|shell| shell.config_name()),
            None,
        )?;
        crate::exec::handler::ensure_shell_available(shell)?;
        if args.login
            && !matches!(
                shell,
                crate::exec::shell::Shell::Bash
                    | crate::exec::shell::Shell::Zsh
                    | crate::exec::shell::Shell::Sh
            )
        {
            return Err(ToolError::invalid_arguments(
                "login=true is supported only with bash, zsh, or sh",
            )
            .into());
        }

        let command = crate::exec::handler::normalize_command_rg(&args.cmd);
        let mut argv = shell.derive_exec_args_with(&command, None);
        if args.login {
            argv[1] = "-lc".into();
        }
        let cwd = args
            .workdir
            .as_deref()
            .map(|workdir| crate::file_mutate::resolve_mutation_path(context, workdir))
            .or_else(|| {
                let workspace = context.workspace_root.to_string_lossy();
                (!workspace.is_empty() && workspace != ".").then(|| workspace.into_owned())
            });
        let _ = &args.justification;
        let effective_sandbox = if args.with_escalated_permissions.unwrap_or(false) {
            qaqh_policy::SandboxSpec::disabled()
        } else {
            context.sandbox_spec().clone()
        };
        let launch =
            qaqh_sandbox::prepare_pty_launch(&argv, cwd.as_deref(), &effective_sandbox)
                .map_err(|message| {
                    ToolExecutionError::from(ToolError::new(ToolErrorKind::Execution, message))
                })?;
        let mut env = crate::exec::direct::minimal_child_env();
        if args.tty && !env.iter().any(|(key, _)| key == "TERM") {
            env.push(("TERM".into(), "xterm-256color".into()));
        }
        // config [exec.env] 用户授权注入面（daemon 热重载实时更新；层序见
        // exec::env_overlay 模块注释）。
        env.extend(crate::exec::env_overlay::snapshot());
        // pager/编辑器/凭据提示解除（harness 最终权威）：tty 路径 git 内置
        // less 分页必挂死，见 forced_child_env 文档。
        env.extend(crate::exec::direct::forced_child_env());
        let host = context.exec_host.as_ref().ok_or_else(|| {
            ToolExecutionError::from(ToolError::new(
                ToolErrorKind::Unavailable,
                "execution session host was not injected for this conversation",
            ))
        })?;
        let result = host.exec_command(
            context,
            ExecCommandRequest {
                command: args.cmd,
                argv: launch.argv,
                cwd,
                env,
                tty: args.tty,
                yield_time_ms,
                timeout_ms: args.timeout_ms,
                max_output_chars,
                sandbox_spec: Some(effective_sandbox),
            },
        );
        if result.is_err() {
            if let Some(path) = launch.payload_path {
                let _ = std::fs::remove_file(path);
            }
        }
        let result = result?;
        Ok(result.into())
    }
}

pub struct WriteStdinTool;

impl TypedTool for WriteStdinTool {
    type Args = WriteStdinArgs;
    type Output = ExecSessionToolOutput;

    fn meta(&self) -> ToolMeta {
        ToolMeta::new(
            "write_stdin",
            "Read new output from an execution session, send terminal input, or interrupt it with exactly Ctrl-C. Empty chars polls for output: yield_time_ms 5000..=300000 (default 5000) — to wait for a long-running command to finish, issue one empty poll with a large yield_time_ms; it returns immediately when the process exits, delivering the terminal result and remaining output. Non-empty chars writes keystrokes: yield_time_ms 250..=30000 (default 250).",
            crate::permission::ToolCategory::Exec,
            crate::ToolRisk::Destructive,
            Duration::from_secs(310),
        )
    }

    #[allow(clippy::result_large_err)]
    fn run(
        &self,
        context: &ToolCallContext,
        args: Self::Args,
    ) -> Result<Self::Output, ToolExecutionError> {
        if !ExecSessionId::is_valid(&args.exec_session_id) {
            return Err(
                ToolError::invalid_arguments("exec_session_id must be an execs_ ULID").into(),
            );
        }
        if args.chars.len() > 64 * 1024 {
            return Err(
                ToolError::invalid_arguments("chars exceeds the 64 KiB UTF-8 input limit").into(),
            );
        }
        let default_yield = if args.chars.is_empty() { 5_000 } else { 250 };
        let yield_time_ms = args.yield_time_ms.unwrap_or(default_yield);
        // codex 同款双轨：空读是长轮询通道（上限 300s，进程退出即返回），
        // 非空写入保持短窗上限让交互 stdin 响应快。
        let (min_yield, max_yield) = if args.chars.is_empty() {
            (5_000, 300_000)
        } else {
            (250, 30_000)
        };
        if !(min_yield..=max_yield).contains(&yield_time_ms) {
            return Err(ToolError::invalid_arguments(format!(
                "yield_time_ms must be between {min_yield} and {max_yield} for this input"
            ))
            .into());
        }
        let max_output_chars = output_budget(context.exec_max_output_chars, args.max_output_chars)?;
        let host = context.exec_host.as_ref().ok_or_else(|| {
            ToolExecutionError::from(ToolError::new(
                ToolErrorKind::Unavailable,
                "execution session host was not injected for this conversation",
            ))
        })?;
        let result = host.write_stdin(
            context,
            WriteStdinRequest {
                exec_session_id: ExecSessionId::new(args.exec_session_id),
                chars: args.chars,
                yield_time_ms,
                max_output_chars,
            },
        )?;
        Ok(result.into())
    }
}

fn output_budget(
    policy_default: Option<u32>,
    requested: Option<u32>,
) -> Result<u32, ToolExecutionError> {
    // P0-2：上限随 ToolCallContext 显式到达（fold policy 在执行入口注入）。
    let budget = requested.unwrap_or(policy_default.unwrap_or(32_000));
    if !(1..=200_000).contains(&budget) {
        return Err(
            ToolError::invalid_arguments("max_output_chars must be between 1 and 200000").into(),
        );
    }
    Ok(budget)
}

pub fn register(manager: &mut impl qaqh_tool_core::tool_api::RegistersTyped) {
    manager.register_typed_tool(ExecCommandTool);
    manager.register_typed_tool(WriteStdinTool);
}
