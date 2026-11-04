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
    /// Shell name; defaults to the host-selected shell.
    #[serde(default)]
    pub shell: Option<String>,
    /// Start a login shell. Supported for bash, zsh, and sh.
    #[serde(default)]
    pub login: bool,
    /// Allocate a PTY for interactive terminal semantics.
    #[serde(default)]
    pub tty: bool,
    /// Initial wait window, 250..=30000 ms. Defaults to 10000.
    #[serde(default)]
    pub yield_time_ms: Option<u32>,
    /// Hard process-tree deadline. Null leaves the execution unbounded.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Maximum model output budget in tokens.
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WriteStdinArgs {
    pub exec_session_id: String,
    #[serde(default)]
    pub chars: String,
    /// Empty input defaults to 5000 ms; non-empty input defaults to 250 ms.
    #[serde(default)]
    pub yield_time_ms: Option<u32>,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
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
            "Run a shell command. Long-running commands return an execution session id; use write_stdin to read output or provide terminal input.",
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
        let max_output_tokens = output_budget(args.max_output_tokens)?;
        let shell = crate::exec::handler::resolve_shell(context, args.shell.as_deref(), None)?;
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
        let launch =
            qaqh_sandbox::prepare_pty_launch(&argv, cwd.as_deref(), context.sandbox_spec())
                .map_err(|message| {
                    ToolExecutionError::from(ToolError::new(ToolErrorKind::Execution, message))
                })?;
        let mut env = crate::exec::direct::minimal_child_env();
        if args.tty && !env.iter().any(|(key, _)| key == "TERM") {
            env.push(("TERM".into(), "xterm-256color".into()));
        }
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
                max_output_tokens,
            },
        );
        if result.is_err() {
            if let Some(path) = launch.payload_path {
                let _ = std::fs::remove_file(path);
            }
        }
        result.map(Into::into).map_err(Into::into)
    }
}

pub struct WriteStdinTool;

impl TypedTool for WriteStdinTool {
    type Args = WriteStdinArgs;
    type Output = ExecSessionToolOutput;

    fn meta(&self) -> ToolMeta {
        ToolMeta::new(
            "write_stdin",
            "Read new output from an execution session, send terminal input, or interrupt it with exactly Ctrl-C.",
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
        let min_yield = if args.chars.is_empty() { 5_000 } else { 250 };
        if !(min_yield..=30_000).contains(&yield_time_ms) {
            return Err(ToolError::invalid_arguments(format!(
                "yield_time_ms must be between {min_yield} and 30000 for this input"
            ))
            .into());
        }
        let max_output_tokens = output_budget(args.max_output_tokens)?;
        let host = context.exec_host.as_ref().ok_or_else(|| {
            ToolExecutionError::from(ToolError::new(
                ToolErrorKind::Unavailable,
                "execution session host was not injected for this conversation",
            ))
        })?;
        host.write_stdin(
            context,
            WriteStdinRequest {
                exec_session_id: ExecSessionId::new(args.exec_session_id),
                chars: args.chars,
                yield_time_ms,
                max_output_tokens,
            },
        )
        .map(Into::into)
        .map_err(Into::into)
    }
}

fn output_budget(requested: Option<u32>) -> Result<u32, ToolExecutionError> {
    let default = crate::hooks::exec_max_output_tokens().unwrap_or(10_000);
    let budget = requested.unwrap_or(default);
    if !(1..=50_000).contains(&budget) {
        return Err(
            ToolError::invalid_arguments("max_output_tokens must be between 1 and 50000").into(),
        );
    }
    Ok(budget)
}

pub fn register(manager: &mut impl qaqh_tool_core::tool_api::RegistersTyped) {
    manager.register_typed_tool(ExecCommandTool);
    manager.register_typed_tool(WriteStdinTool);
}
