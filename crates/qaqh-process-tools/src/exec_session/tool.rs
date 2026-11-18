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
    /// Maximum model output budget in tokens.
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    /// Subscribe the terminal notification at launch — the right choice for
    /// long builds, test suites, and installs:
    /// - Command still running when the initial wait window closes → the call
    ///   returns `yielded` with the session armed. End your turn; the harness
    ///   delivers the terminal result (exit code + stop reason, no output
    ///   body) as a one-shot system message when the process exits. Do not
    ///   poll afterwards; once the notice arrives, one write_stdin drains the
    ///   buffered output if needed.
    /// - Command exits within the window → the terminal result is delivered
    ///   right in this call and nothing is armed.
    /// - Not for servers/daemons meant to keep running (dev servers, vite,
    ///   watchers): they never exit, so nothing would ever notify. Omit this
    ///   flag for them (and omit timeout_ms too).
    /// - Combined with timeout_ms, a force-stop is delivered as `timed_out`.
    #[serde(default)]
    pub notify_on_exit: Option<bool>,
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
    /// Read wait window. Empty input (pure read or notify detach): 5000..=30000 ms,
    /// default 5000. Non-empty input (send keystrokes): 250..=30000 ms, default 250.
    /// The two floors are not interchangeable — e.g. 3000 ms is rejected when chars is empty.
    #[serde(default)]
    pub yield_time_ms: Option<u32>,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    /// Arm the terminal notification for an already-running session (empty
    /// chars only): the call returns immediately — end your turn, and the
    /// harness delivers the terminal result (exit code + stop reason, no
    /// output body) as a one-shot system message when the process exits.
    /// - Session already exited → the normal terminal read comes back in this
    ///   call and nothing is armed.
    /// - While armed, do not poll; after the notice, one write_stdin drains
    ///   the buffered output.
    /// - For new commands prefer exec_command(notify_on_exit=true) — one
    ///   round trip arms at launch.
    #[serde(default)]
    pub notify_on_exit: Option<bool>,
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
            "Run a shell command. `cmd` is wrapped by the selected shell dialect (`shell`); long-running commands return an execution session id — use write_stdin to read output or provide terminal input. stdout/stderr are captured as separate streams; avoid `2>&1`/`1>&2` unless merging is intentional. Pagers and git prompts/editors are disarmed via env (PAGER/GIT_PAGER/GH_PAGER=cat, GIT_TERMINAL_PROMPT=0, GIT_EDITOR=true); output ending in `:` means stuck in a pager — send `q` via write_stdin. Payload interpreters (node/bun/python/deno) are programs to run inside `cmd`, never a value for `shell`.",
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
        let launch =
            qaqh_sandbox::prepare_pty_launch(&argv, cwd.as_deref(), context.sandbox_spec())
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
                max_output_tokens,
                notify_on_exit: args.notify_on_exit.unwrap_or(false),
            },
        );
        if result.is_err() {
            if let Some(path) = launch.payload_path {
                let _ = std::fs::remove_file(path);
            }
        }
        let mut result = result?;
        if result.notify_armed {
            let id = result
                .exec_session_id
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| "<unknown>".to_owned());
            append_subscribe_notice(&mut result.output, &id);
        }
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
            "Read new output from an execution session, send terminal input, or interrupt it with exactly Ctrl-C. With notify_on_exit=true (empty chars) detach instead: the call returns immediately, end your turn, and the harness delivers the terminal result as a one-shot system message when the process exits.",
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
        // ADR-0026：订阅是纯 detach 动作，与写输入互斥；manager 侧同样只在
        // 空 chars 时生效（纵深防御），此处给出确定性报错。
        let notify_on_exit = args.notify_on_exit.unwrap_or(false);
        if notify_on_exit && !args.chars.is_empty() {
            return Err(ToolError::invalid_arguments(
                "notify_on_exit requires empty chars — subscribing is a pure detach, not an input write",
            )
            .into());
        }
        let max_output_tokens = output_budget(args.max_output_tokens)?;
        let host = context.exec_host.as_ref().ok_or_else(|| {
            ToolExecutionError::from(ToolError::new(
                ToolErrorKind::Unavailable,
                "execution session host was not injected for this conversation",
            ))
        })?;
        let mut result = host.write_stdin(
            context,
            WriteStdinRequest {
                exec_session_id: ExecSessionId::new(args.exec_session_id),
                chars: args.chars,
                yield_time_ms,
                max_output_tokens,
                notify_on_exit,
            },
        )?;
        if result.notify_armed {
            let id = result
                .exec_session_id
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| "<unknown>".to_owned());
            append_subscribe_notice(&mut result.output, &id);
        }
        Ok(result.into())
    }
}

/// ADR-0026：订阅成功时附加的行为契约文本（进模型面 output，不进 JSON 字段）。
fn append_subscribe_notice(output: &mut String, exec_session_id: &str) {
    if !output.is_empty() {
        output.push('\n');
    }
    output.push_str(&format!(
        "[exec session subscribed] Terminal notification for {exec_session_id} is now owned \
         by the harness: end your turn now — when the process exits, a one-shot system \
         message (<qaqh_exec_session_terminal>) will deliver the terminal result (exit \
         code + stop reason). Do not keep polling this session; if you need its output \
         afterwards, a single write_stdin on {exec_session_id} after the terminal notice \
         drains the buffered output."
    ));
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
