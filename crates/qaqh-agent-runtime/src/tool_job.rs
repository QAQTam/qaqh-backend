//! Session-scoped tool execution and lifecycle reporting.
//!
//! The port owns exactly
//! one execution: it never re-runs a tool to "retry" a lost result, and it
//! never reports a cancelled or dropped future as a stopped side effect.
//!
//! The executor behind [`WorkspaceToolPort`] is responsible for honouring the cancel
//! flag, re-evaluating permissions and approval grants before dispatch, and telling the truth
//! about whether anything started.

use qaqh_process_tools::exec_session::{
    ExecManagerError, ExecManagerErrorKind, ExecSessionManager, ExecSessionPoll,
    ExecSessionPublisher, ExecSessionResult, ExecSessionStart,
};
use qaqh_ringing_contract::fact::{
    ToolCallOutcome, ToolCancellationReason, ToolError, ToolInterruptionReason, ToolMetrics,
};
use qaqh_ringing_contract::state::{CodeDelta, PermissionRequestDetails, ToolEffectInfo};
use qaqh_session::owner::{ToolLifecycle, ToolRun, ToolSettlement};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

#[derive(Clone)]
struct OwnerExecPublisher {
    lifecycle: ToolLifecycle,
}

impl std::fmt::Debug for OwnerExecPublisher {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("OwnerExecPublisher")
    }
}

impl ExecSessionPublisher for OwnerExecPublisher {
    fn upsert(&self, view: qaqh_ringing_contract::state::ExecSessionView) {
        self.lifecycle.exec_session_upsert(view);
    }

    fn output(
        &self,
        exec_session_id: &qaqh_types::ExecSessionId,
        start_offset: u64,
        end_offset: u64,
        source: qaqh_ringing_contract::events::ExecOutputSource,
        text: String,
        snapshot: qaqh_process_tools::exec_session::capture::OutputSnapshot,
        stream_dropped_bytes: u64,
    ) {
        self.lifecycle.exec_session_output(
            exec_session_id.as_str(),
            start_offset,
            end_offset,
            source,
            text,
            snapshot.next_offset,
            snapshot.output_preview,
            snapshot.preview_start_offset,
            snapshot.capture_omitted_bytes,
            stream_dropped_bytes,
        );
    }

    fn finish(
        &self,
        view: qaqh_ringing_contract::state::ExecSessionView,
        transcript: String,
        truncated: bool,
    ) {
        let state_json =
            serde_json::to_string(&view.state).expect("execution session state is serializable");
        self.lifecycle
            .exec_session_finished(qaqh_session::owner::ExecSessionSettlement {
                exec_session_id: view.id.as_str().to_string(),
                state_json,
                finished_at_ms: view
                    .finished_at_ms
                    .unwrap_or_else(qaqh_session::db::store::now_ms),
                output_complete: view.output_complete,
                output_content_id: None,
                output_text: (!transcript.is_empty()).then_some(transcript),
                output_truncated: truncated,
                output_preview: view.output_preview,
                preview_start_offset: view.preview_start_offset,
            });
    }
}

#[derive(Debug)]
struct SessionExecHost {
    manager: Arc<ExecSessionManager>,
    publisher: Arc<OwnerExecPublisher>,
    origin_turn_id: String,
}

impl qaqh_workspace::tool_api::ExecSessionHost for SessionExecHost {
    fn exec_command(
        &self,
        context: &qaqh_workspace::tool_api::ToolCallContext,
        request: qaqh_workspace::tool_api::ExecCommandRequest,
    ) -> Result<qaqh_workspace::tool_api::ExecCommandResult, qaqh_workspace::tool_api::ToolError>
    {
        let result = self
            .manager
            .start(
                ExecSessionStart {
                    owner_session_id: context.session_id.clone(),
                    origin_turn_id: self.origin_turn_id.clone(),
                    origin_call_id: context.call_id.clone(),
                    command: request.command,
                    cwd: request.cwd,
                    argv: request.argv,
                    env: request.env,
                    tty: request.tty,
                    timeout_ms: request.timeout_ms,
                    yield_time_ms: request.yield_time_ms,
                    max_output_bytes: request.max_output_tokens as usize * 4,
                    cancellation: context.cancellation.shared_flag(),
                },
                self.publisher.clone(),
            )
            .map_err(exec_manager_error)?;
        Ok(exec_result_to_tool_core(result))
    }

    fn write_stdin(
        &self,
        context: &qaqh_workspace::tool_api::ToolCallContext,
        request: qaqh_workspace::tool_api::WriteStdinRequest,
    ) -> Result<qaqh_workspace::tool_api::ExecCommandResult, qaqh_workspace::tool_api::ToolError>
    {
        let result = self
            .manager
            .poll(ExecSessionPoll {
                exec_session_id: request.exec_session_id,
                owner_session_id: context.session_id.clone(),
                chars: request.chars,
                yield_time_ms: request.yield_time_ms,
                max_output_bytes: request.max_output_tokens as usize * 4,
                cancellation: context.cancellation.shared_flag(),
            })
            .map_err(exec_manager_error)?;
        Ok(exec_result_to_tool_core(result))
    }
}

fn exec_result_to_tool_core(
    result: ExecSessionResult,
) -> qaqh_workspace::tool_api::ExecCommandResult {
    qaqh_workspace::tool_api::ExecCommandResult {
        outcome: result.outcome,
        exec_session_id: result.exec_session_id,
        output: result.output,
        output_truncated: result.output_truncated,
        omitted_bytes: result.omitted_bytes,
        exit_code: result.exit_code,
        timed_out: result.timed_out,
        cancelled: result.cancelled,
        input_bytes_written: result.input_bytes_written,
    }
}

fn exec_manager_error(error: ExecManagerError) -> qaqh_workspace::tool_api::ToolError {
    use qaqh_workspace::tool_api::ToolErrorKind;
    let kind = match error.kind {
        ExecManagerErrorKind::QuotaExceeded | ExecManagerErrorKind::Shutdown => {
            ToolErrorKind::Unavailable
        }
        ExecManagerErrorKind::NotFound => ToolErrorKind::NotFound,
        ExecManagerErrorKind::WrongOwner => ToolErrorKind::PermissionDenied,
        ExecManagerErrorKind::InputUnsupported | ExecManagerErrorKind::InputTooLarge => {
            ToolErrorKind::InvalidArguments
        }
        ExecManagerErrorKind::InputWrite | ExecManagerErrorKind::Spawn => ToolErrorKind::Execution,
    };
    let mut tool_error = qaqh_workspace::tool_api::ToolError::new(kind, error.message);
    tool_error.retryable = matches!(
        error.kind,
        ExecManagerErrorKind::QuotaExceeded | ExecManagerErrorKind::Shutdown
    );
    if error.bytes_written > 0 {
        tool_error.details = Some(serde_json::json!({
            "input_bytes_written": error.bytes_written,
            "input_may_have_been_applied": true,
        }));
    }
    tool_error
}

/// Everything the authorized effect pinned before dispatch.
#[derive(Debug, Clone)]
pub struct ToolRequest {
    pub execution_id: String,
    pub call_id: String,
    pub turn_id: String,
    pub tool_name: String,
    /// 只读工具被取消时无需不确定性上报(直接 Cancelled)。
    pub read_only: bool,
    /// Effective arguments (JSON bytes).
    pub args: Vec<u8>,
}

/// The executor's own account of how far it got. Any error that could have
/// applied a side effect must be reported as `AfterStart`.
#[derive(Debug, Clone)]
pub enum ToolFailure {
    /// Refused before anything observable happened.
    NotStarted(String),
    /// Refused by permission policy, mode, or revoked approval before dispatch.
    Denied(String),
    /// Executed and returned a deterministic tool-level failure (e.g. file not
    /// found, stale hash) with structured output.
    TerminalError(String),
    /// Started, and may have applied effects; the result is inconclusive.
    AfterStart(String),
}

/// Policy snapshot re-evaluated immediately before dispatching a tool handler.
///
/// The OS sandbox spec is part of the snapshot, not re-derived at dispatch:
/// the port must execute exactly the policy the session was assembled with,
/// including an explicitly disabled sandbox. Recomputing it at dispatch is how
/// the config switch silently lost its consumer (report 2026-10-31 §11.2 F10).
#[derive(Clone, Debug)]
pub struct ToolPolicySnapshot {
    pub workspace_root: PathBuf,
    pub permission_level: qaqh_workspace::PermissionLevel,
    pub sandbox: qaqh_workspace::tool_api::SandboxMode,
    pub trusted_dirs: HashSet<PathBuf>,
    /// Canonical sandbox policy for this session's tool calls.
    pub sandbox_spec: qaqh_workspace::tool_api::SandboxSpec,
}

impl ToolPolicySnapshot {
    pub fn workspace_write(workspace_root: impl Into<PathBuf>) -> Self {
        let workspace_root = workspace_root.into();
        let sandbox_spec = qaqh_workspace::tool_api::SandboxSpec::for_permission(
            workspace_root.clone(),
            qaqh_workspace::PermissionLevel::WorkspaceWrite,
            true,
        );
        Self {
            workspace_root,
            permission_level: qaqh_workspace::PermissionLevel::WorkspaceWrite,
            sandbox: qaqh_workspace::tool_api::SandboxMode::Main,
            trusted_dirs: HashSet::new(),
            sandbox_spec,
        }
    }

    /// Extend the trusted-directory set. Only an enabled sandbox gains writable
    /// roots; a disabled spec must never be re-enabled by granting access.
    pub fn trust_dir(&mut self, dir: PathBuf) {
        self.sandbox_spec = self
            .sandbox_spec
            .clone()
            .with_trusted_roots(std::iter::once(dir.clone()));
        self.trusted_dirs.insert(dir);
    }
}

/// 小写 hex sha256(工具参数/请求身份摘要的公共原语)。
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    let hash = sha2::Sha256::digest(bytes);
    hash.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Single-use user approval bound to one exact `(call_id, execution_id, args_hash, workspace)`.
///
/// An approval can never be reused for another execution generation, another
/// call, modified arguments, or a changed workspace root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolApprovalGrant {
    pub interaction_id: String,
    pub call_id: String,
    pub execution_id: String,
    pub args_hash: String,
    pub workspace_root: PathBuf,
}

/// Real [`WorkspaceToolPort`] backed by a session-scoped [`qaqh_workspace::ToolManager`],
/// pre-dispatch permission/approval re-verification.
///
/// 工具生命周期(运行状态、实时输出、结构化结果与 effects)经会话 owner 的
/// [`ToolLifecycle`] 上报;这是 E09/E10 断点的接通点。
pub struct WorkspaceToolPort {
    session_id: String,
    manager: Arc<Mutex<qaqh_workspace::ToolManager>>,
    policy: RwLock<ToolPolicySnapshot>,
    approvals: Mutex<HashMap<String, ToolApprovalGrant>>,
    lifecycle: ToolLifecycle,
    exec_sessions: Arc<ExecSessionManager>,
}

impl WorkspaceToolPort {
    pub fn new(
        session_id: impl Into<String>,
        manager: Arc<Mutex<qaqh_workspace::ToolManager>>,
        policy: ToolPolicySnapshot,
        lifecycle: ToolLifecycle,
        exec_sessions: Arc<ExecSessionManager>,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            manager,
            policy: RwLock::new(policy),
            approvals: Mutex::new(HashMap::new()),
            lifecycle,
            exec_sessions,
        }
    }

    /// Update the active policy snapshot. Any subsequent dispatch re-evaluates
    /// permissions against this updated snapshot before invoking the handler.
    pub fn update_policy(&self, policy: ToolPolicySnapshot) {
        *self
            .policy
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = policy;
    }

    /// Replace the per-session and global tool filters, returning the catalog
    /// that the model may use after both filters are applied.
    pub fn update_tool_allowlists(
        &self,
        allowed_tools: Vec<String>,
        global_allowed_tools: Option<Vec<String>>,
    ) -> Vec<qaqh_types::ToolDef> {
        let mut manager = self
            .manager
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        manager.set_global_allowed(global_allowed_tools);
        manager.apply_init(allowed_tools);
        manager.filtered_defs()
    }

    /// Workspace root the current policy dispatches against.
    pub fn workspace_root(&self) -> PathBuf {
        self.policy
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .workspace_root
            .clone()
    }

    /// Extend the trusted-directory set ("trust this folder" decisions).
    ///
    /// The writable roots move with it, but only while the sandbox is enabled;
    /// see [`ToolPolicySnapshot::trust_dir`].
    pub fn trust_dir(&self, dir: PathBuf) {
        self.policy
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .trust_dir(dir);
    }

    /// Install a single-use approval grant for one specific `execution_id`.
    pub fn grant_approval(&self, grant: ToolApprovalGrant) {
        self.approvals
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(grant.execution_id.clone(), grant);
    }
}

/// Pre-dispatch admission verdict. `execute` re-evaluates it; this only lets
/// the caller raise the interaction before committing a tool intent result.
#[derive(Debug, Clone)]
pub enum ToolAdmission {
    Authorized,
    ApprovalRequired { details: PermissionRequestDetails },
    Denied(String),
}

/// Everything derived from one request before admission.
struct PreparedCall {
    context: qaqh_workspace::tool_api::ToolCallContext,
    invocation: qaqh_workspace::ToolInvocation,
    policy: ToolPolicySnapshot,
    args_hash: String,
}

impl WorkspaceToolPort {
    fn prepare(&self, request: &ToolRequest) -> Result<PreparedCall, ToolFailure> {
        if request.args.len() > 256 * 1024 {
            return Err(ToolFailure::NotStarted(
                "tool arguments exceed limit".into(),
            ));
        }
        let args_value: serde_json::Value = serde_json::from_slice(&request.args)
            .map_err(|e| ToolFailure::NotStarted(format!("invalid tool JSON arguments: {e}")))?;
        let args_hash = sha256_hex(&request.args);

        let policy = self
            .policy
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();

        let category = self
            .manager
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .category_of(&request.tool_name)
            .ok_or_else(|| {
                ToolFailure::NotStarted(format!("unknown tool: {}", request.tool_name))
            })?;

        let cancellation = qaqh_workspace::tool_api::CancellationToken::new();
        let exec_host = Arc::new(SessionExecHost {
            manager: Arc::clone(&self.exec_sessions),
            publisher: Arc::new(OwnerExecPublisher {
                lifecycle: self.lifecycle.clone(),
            }),
            origin_turn_id: request.turn_id.clone(),
        });
        let context = qaqh_workspace::tool_api::ToolCallContext {
            call_id: request.call_id.clone(),
            session_id: self.session_id.clone(),
            workspace_root: policy.workspace_root.clone(),
            permission_level: policy.permission_level,
            sandbox: policy.sandbox,
            // The snapshot already carries the resolved spec (config switch
            // included). Rebuild it here and the OS sandbox silently ignores
            // sandbox.enabled=false.
            sandbox_spec: policy.sandbox_spec.clone(),
            exec_default_shell: None,
            timeout: Duration::ZERO,
            cancellation,
            progress: None,
            exec_progress: None,
            exec_host: Some(exec_host),
            official_web: None,
            source: qaqh_workspace::tool_api::ToolCallSource::Model,
        };
        let invocation = qaqh_workspace::ToolInvocation {
            session_id: self.session_id.clone(),
            call_id: request.call_id.clone(),
            tool_name: request.tool_name.clone(),
            args: args_value,
            category,
        };
        Ok(PreparedCall {
            context,
            invocation,
            policy,
            args_hash,
        })
    }

    /// Evaluate permission policy without executing anything.
    pub fn precheck(&self, request: &ToolRequest) -> ToolAdmission {
        let prepared = match self.prepare(request) {
            Ok(prepared) => prepared,
            Err(ToolFailure::Denied(reason)) => return ToolAdmission::Denied(reason),
            Err(error) => return ToolAdmission::Denied(format!("{error:?}")),
        };
        match qaqh_workspace::admit_with_context(
            prepared.invocation,
            &prepared.context,
            &prepared.policy.trusted_dirs,
        ) {
            qaqh_workspace::Admission::Authorized(_) => ToolAdmission::Authorized,
            qaqh_workspace::Admission::ApprovalRequired(challenge) => {
                ToolAdmission::ApprovalRequired {
                    details: PermissionRequestDetails {
                        tool_name: challenge.tool_name().to_string(),
                        action_summary: challenge.action_summary(),
                        reason: challenge.reason().to_string(),
                        paths: challenge
                            .resources()
                            .iter()
                            .map(|path| path.to_string_lossy().into_owned())
                            .collect(),
                        category: challenge.category().as_str().to_string(),
                        level: challenge.context().permission_level.to_u8(),
                        risk: match challenge.risk() {
                            qaqh_workspace::PermissionRisk::Low => "low",
                            qaqh_workspace::PermissionRisk::Medium => "medium",
                            qaqh_workspace::PermissionRisk::High => "high",
                        }
                        .to_string(),
                        consequence: challenge.consequence().to_string(),
                    },
                }
            }
            qaqh_workspace::Admission::Denied(reason) => ToolAdmission::Denied(reason),
        }
    }

    /// Content hash of the exact bytes a grant is bound to.
    pub fn args_hash(args: &[u8]) -> String {
        sha256_hex(args)
    }
}

impl WorkspaceToolPort {
    pub fn execute(
        &self,
        request: &ToolRequest,
        cancel: Arc<AtomicBool>,
    ) -> Result<String, ToolFailure> {
        let args_json = String::from_utf8(request.args.clone()).ok();
        if cancel.load(Ordering::Acquire) {
            self.lifecycle.settled(
                &request.call_id,
                &request.tool_name,
                args_json.clone(),
                cancelled_settlement("cancelled before dispatch"),
            );
            return Err(ToolFailure::NotStarted("cancelled before dispatch".into()));
        }
        let PreparedCall {
            context,
            invocation,
            policy,
            args_hash,
        } = match self.prepare(request) {
            Ok(prepared) => prepared,
            Err(failure) => {
                self.settle_failure(request, args_json.as_deref(), &failure);
                return Err(failure);
            }
        };

        let authorized =
            match qaqh_workspace::admit_with_context(invocation, &context, &policy.trusted_dirs) {
                qaqh_workspace::Admission::Authorized(call) => call,
                qaqh_workspace::Admission::Denied(reason) => {
                    self.lifecycle.settled(
                        &request.call_id,
                        &request.tool_name,
                        args_json.clone(),
                        failed_settlement("permission_denied", &reason, None, None),
                    );
                    return Err(ToolFailure::Denied(reason));
                }
                qaqh_workspace::Admission::ApprovalRequired(challenge) => {
                    let grant = match self
                        .approvals
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .remove(&request.execution_id)
                    {
                        Some(grant) => grant,
                        None => {
                            let reason = format!(
                                "permission approval required or revoked for {}",
                                request.tool_name
                            );
                            self.lifecycle.settled(
                                &request.call_id,
                                &request.tool_name,
                                args_json.clone(),
                                failed_settlement("permission_denied", &reason, None, None),
                            );
                            return Err(ToolFailure::Denied(reason));
                        }
                    };
                    let normalized_ws =
                        qaqh_workspace::resolve_target_path(policy.workspace_root.clone());
                    let grant_ws = qaqh_workspace::resolve_target_path(grant.workspace_root);
                    if grant.call_id != request.call_id
                        || grant.execution_id != request.execution_id
                        || grant.args_hash != args_hash
                        || grant_ws != normalized_ws
                    {
                        let reason =
                        "approval grant does not match execution identity, arguments, or workspace"
                            .to_string();
                        self.lifecycle.settled(
                            &request.call_id,
                            &request.tool_name,
                            args_json.clone(),
                            failed_settlement("permission_denied", &reason, None, None),
                        );
                        return Err(ToolFailure::Denied(reason));
                    }
                    match challenge.approve(true) {
                        Ok(call) => call,
                        Err(err) => {
                            let reason = format!("approval challenge rejected: {err:?}");
                            self.lifecycle.settled(
                                &request.call_id,
                                &request.tool_name,
                                args_json.clone(),
                                failed_settlement("permission_denied", &reason, None, None),
                            );
                            return Err(ToolFailure::Denied(reason));
                        }
                    }
                }
            };

        if cancel.load(Ordering::Acquire) {
            self.lifecycle.settled(
                &request.call_id,
                &request.tool_name,
                args_json.clone(),
                cancelled_settlement("cancelled before handler start"),
            );
            return Err(ToolFailure::NotStarted(
                "cancelled before handler start".into(),
            ));
        }

        let mut context = context;
        context.cancellation =
            qaqh_workspace::tool_api::CancellationToken::from_shared_flag(Arc::clone(&cancel));
        let scope = qaqh_workspace::runtime::ToolExecutionScope::with_manager(
            context.clone(),
            Arc::clone(&self.manager),
        );
        let _guard = scope.install();

        // 进入实际执行:E09/E10 断点接通——直通 sender 注入 exec 处理器,
        // 实时输出经 owner 泵转成 tool.output.delta;终态带结构化结果与 effects。
        let run = self
            .lifecycle
            .started(&request.call_id, &request.tool_name, args_json);
        let progress_tx = run.exec_sender();
        let exec_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            qaqh_workspace::execution::execute_authorized_with_context(
                authorized,
                context,
                Some(progress_tx),
            )
        }));
        let exec_result = match exec_result {
            Ok(result) => result,
            Err(_) => {
                let inflight_cleared = self
                    .manager
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .fail_inflight_after_panic(&request.call_id);
                if inflight_cleared {
                    log::warn!(
                        "[tool-job] cleared in-flight registration after panic for {}",
                        request.call_id
                    );
                }
                let metrics = run_metrics(&run);
                self.lifecycle.finished(
                    run,
                    failed_settlement(
                        "tool_panicked",
                        "tool handler panicked; side-effect status is unknown",
                        None,
                        Some(metrics),
                    ),
                );
                return Err(ToolFailure::AfterStart(
                    "tool handler panicked; side-effect status is unknown".into(),
                ));
            }
        };

        // 契约 outcome 是唯一终态权威(spec §4.2):typed 执行器直接给出
        // completed/yielded/failed 等事实;不再从模型正文 JSON 推断
        // backgrounded/process_id(E03 已删)。
        let contract_outcome = exec_result.outcome.clone();
        if let Some(outcome) = &contract_outcome {
            let metrics = run_metrics(&run);
            self.lifecycle.finished(
                run,
                ToolSettlement {
                    call_id: request.call_id.clone(),
                    tool_name: request.tool_name.clone(),
                    outcome: outcome.clone(),
                    output_text: Some(exec_result.content.clone()),
                    output_truncated: false,
                    metrics: Some(metrics),
                    code_delta: code_delta_of(&exec_result),
                    effects: effect_infos(&exec_result),
                },
            );
            return Ok(exec_result.content);
        }

        if exec_result.success {
            let metrics = run_metrics(&run);
            self.lifecycle.finished(
                run,
                ToolSettlement {
                    call_id: request.call_id.clone(),
                    tool_name: request.tool_name.clone(),
                    outcome: ToolCallOutcome::Completed,
                    output_text: Some(exec_result.content.clone()),
                    output_truncated: false,
                    metrics: Some(metrics),
                    code_delta: code_delta_of(&exec_result),
                    effects: effect_infos(&exec_result),
                },
            );
            return Ok(exec_result.content);
        }

        let err_code = exec_result
            .result
            .error
            .as_ref()
            .map(|e| e.code.as_str())
            .unwrap_or("");
        match err_code {
            "permission_denied" | "blocked_by_mode" | "session_mismatch" | "workspace_mismatch"
            | "resource_mismatch" => {
                let terminal = exec_error_terminal(&run, &exec_result);
                self.lifecycle.finished(run, terminal);
                Err(ToolFailure::Denied(exec_result.content))
            }
            "manager_unavailable"
            | "runtime_not_initialized"
            | "audit_unavailable"
            | "cancelled_before_start" => {
                let terminal = exec_error_terminal(&run, &exec_result);
                self.lifecycle.finished(run, terminal);
                Err(ToolFailure::NotStarted(exec_result.content))
            }
            "partial" | "audit_quarantined" | "tool_panicked" => {
                let terminal = exec_error_terminal(&run, &exec_result);
                self.lifecycle.finished(run, terminal);
                Err(ToolFailure::AfterStart(exec_result.content))
            }
            "cancelled" if !request.read_only => {
                // 取消但效果不可确认:interrupted/execution_unconfirmed,
                // 不把发出取消信号当已取消(spec §8.1)。
                let terminal = ToolSettlement {
                    call_id: request.call_id.clone(),
                    tool_name: request.tool_name.clone(),
                    outcome: ToolCallOutcome::Interrupted {
                        reason: ToolInterruptionReason::ExecutionUnconfirmed,
                    },
                    output_text: Some(exec_result.content.clone()),
                    output_truncated: false,
                    metrics: Some(run_metrics(&run)),
                    code_delta: code_delta_of(&exec_result),
                    effects: effect_infos(&exec_result),
                };
                self.lifecycle.finished(run, terminal);
                Err(ToolFailure::AfterStart(exec_result.content))
            }
            "cancelled" => {
                let terminal = cancelled_settlement(&exec_result.content);
                self.lifecycle.finished(run, terminal);
                Err(ToolFailure::Denied(exec_result.content))
            }
            _ => {
                let terminal = exec_error_terminal(&run, &exec_result);
                self.lifecycle.finished(run, terminal);
                Err(ToolFailure::TerminalError(exec_result.content))
            }
        }
    }
}

impl WorkspaceToolPort {
    /// 调度前的拒绝:没有 Started 行,终态直落 owner。
    fn settle_failure(
        &self,
        request: &ToolRequest,
        args_json: Option<&str>,
        failure: &ToolFailure,
    ) {
        let terminal = match failure {
            ToolFailure::Denied(reason) => {
                failed_settlement("permission_denied", reason, None, None)
            }
            ToolFailure::NotStarted(reason) => failed_settlement("not_started", reason, None, None),
            // 其余变体只产生于 started 之后,走 finished。
            _ => return,
        };
        self.lifecycle.settled(
            &request.call_id,
            &request.tool_name,
            args_json.map(str::to_string),
            terminal,
        );
    }
}

/// 执行期错误 → 业务终态:错误进规范错误对象(outcome.failed),正文保留完整输出。
fn exec_error_terminal(
    run: &ToolRun,
    result: &qaqh_workspace::execution::ToolExecResult,
) -> ToolSettlement {
    let code = result
        .result
        .error
        .as_ref()
        .map(|e| e.code.as_str().to_string())
        .unwrap_or_else(|| "tool_failed".into());
    failed_settlement(
        &code,
        &result.content,
        Some(result.content.clone()),
        Some(run_metrics(run)),
    )
}

/// 已确认取消(只读工具/调度前):取消原因按回合取消记录。
fn cancelled_settlement(message: &str) -> ToolSettlement {
    ToolSettlement {
        call_id: String::new(),
        tool_name: String::new(),
        outcome: ToolCallOutcome::Cancelled {
            reason: ToolCancellationReason::TurnCancel,
        },
        output_text: Some(message.to_string()),
        output_truncated: false,
        metrics: None,
        code_delta: None,
        effects: Vec::new(),
    }
}

fn failed_settlement(
    code: &str,
    message: &str,
    output_text: Option<String>,
    metrics: Option<ToolMetrics>,
) -> ToolSettlement {
    ToolSettlement {
        call_id: String::new(),
        tool_name: String::new(),
        outcome: ToolCallOutcome::Failed {
            error: ToolError {
                code: code.to_string(),
                message: message.to_string(),
                retryable: false,
                details_ref: None,
            },
        },
        output_text,
        output_truncated: false,
        metrics,
        code_delta: None,
        effects: Vec::new(),
    }
}

/// 终态指标:retry_count 恒 0(此 runner 从不重跑),progress 总量来自直通通道。
fn run_metrics(run: &ToolRun) -> ToolMetrics {
    let finished_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or_default();
    ToolMetrics {
        started_at_ms: run.started_at_ms(),
        finished_at_ms,
        retry_count: 0,
        output_bytes: 0,
        progress_bytes_total: run.progress_totals().total_bytes(),
    }
}

/// workspace 结构化效果 → 契约词表;SubagentSpawned 的执行面字段留在 runtime。
fn effect_infos(result: &qaqh_workspace::execution::ToolExecResult) -> Vec<ToolEffectInfo> {
    result
        .skill_effects
        .iter()
        .map(|effect| match effect {
            qaqh_workspace::ToolEffect::Skill(qaqh_skills::SkillEffect::Activate(activation)) => {
                ToolEffectInfo::SkillActivated {
                    name: activation.metadata.name.clone(),
                }
            }
            qaqh_workspace::ToolEffect::Skill(qaqh_skills::SkillEffect::Release { name }) => {
                ToolEffectInfo::SkillReleased { name: name.clone() }
            }
            qaqh_workspace::ToolEffect::SubagentSpawned {
                child_session_id,
                name,
                task_text,
                ..
            } => ToolEffectInfo::SubagentSpawned {
                child_session_id: child_session_id.clone(),
                name: name.clone(),
                task_text: task_text.clone(),
            },
        })
        .collect()
}

fn code_delta_of(result: &qaqh_workspace::execution::ToolExecResult) -> Option<CodeDelta> {
    result.code_delta.as_ref().map(|delta| CodeDelta {
        lines_added: delta.lines_added,
        lines_removed: delta.lines_removed,
        files_created: delta.files_created,
        files_deleted: delta.files_deleted,
        file: delta.file.clone(),
    })
}
