//! Supervised tool runner and workspace tool port for committed tool intents.
//!
//! Not connected to the production session lifecycle. The runner owns exactly
//! one execution: it never re-runs a tool to "retry" a lost result, and it
//! never reports a cancelled or dropped future as a stopped side effect.
//!
//! The executor behind [`ToolPort`] is responsible for honouring the cancel
//! flag, re-evaluating permissions and approval grants before dispatch, holding
//! quota reservations for backgrounded or unstopped work, and telling the truth
//! about whether anything started.

use qaqh_ringing_contract::fact::{ToolError, ToolMetrics};
use qaqh_ringing_contract::state::{CodeDelta, ToolCallStatus, ToolEffectInfo};
use qaqh_session::owner::{ToolLifecycle, ToolRun, ToolTerminal};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use crate::quota_ledger::{QuotaKind, QuotaLedger, QuotaReservation, ReleaseReason};

/// Everything the authorized effect pinned before dispatch.
#[derive(Debug, Clone)]
pub struct ToolRequest {
    pub execution_id: String,
    pub call_id: String,
    pub tool_name: String,
    /// 只读工具被取消时无需不确定性上报(直接 Cancelled)。
    pub read_only: bool,
    /// Effective arguments (JSON bytes).
    pub args: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub output: Vec<u8>,
    /// The executor left a process running in the background. This is not a
    /// terminal result and must not be settled as one.
    pub backgrounded: bool,
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
    TerminalError { message: String, output: Vec<u8> },
    /// Started, and may have applied effects; the result is inconclusive.
    AfterStart(String),
}

pub trait ToolPort: Send + Sync + 'static {
    /// Must return only after the external effect stopped, or must report
    /// [`ToolFailure::AfterStart`] when it cannot prove that.
    fn execute(
        &self,
        request: &ToolRequest,
        cancel: Arc<AtomicBool>,
    ) -> Result<ToolOutput, ToolFailure>;
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
    pub mode: qaqh_workspace::tool_api::AgentMode,
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
            mode: qaqh_workspace::tool_api::AgentMode::Code,
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

/// Owner record for a backgrounded process or unstopped tool whose quota
/// reservation remains held until explicit reaping/settlement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetainedToolOwner {
    pub session_id: String,
    pub call_id: String,
    pub execution_id: String,
    pub process_id: Option<u64>,
    pub backgrounded: bool,
    pub reservation: Option<QuotaReservation>,
}

/// Real [`ToolPort`] backed by a session-scoped [`qaqh_workspace::ToolManager`],
/// pre-dispatch permission/approval re-verification, and an optional durable
/// [`QuotaLedger`].
///
/// 工具生命周期(运行状态、实时输出、结构化结果与 effects)经会话 owner 的
/// [`ToolLifecycle`] 上报;这是 E09/E10 断点的接通点。
pub struct WorkspaceToolPort {
    session_id: String,
    manager: Arc<Mutex<qaqh_workspace::ToolManager>>,
    policy: RwLock<ToolPolicySnapshot>,
    approvals: Mutex<HashMap<String, ToolApprovalGrant>>,
    quota: Option<Arc<Mutex<QuotaLedger>>>,
    retained_owners: Mutex<HashMap<String, RetainedToolOwner>>,
    lifecycle: ToolLifecycle,
}

impl WorkspaceToolPort {
    pub fn new(
        session_id: impl Into<String>,
        manager: Arc<Mutex<qaqh_workspace::ToolManager>>,
        policy: ToolPolicySnapshot,
        quota: Option<Arc<Mutex<QuotaLedger>>>,
        lifecycle: ToolLifecycle,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            manager,
            policy: RwLock::new(policy),
            approvals: Mutex::new(HashMap::new()),
            quota,
            retained_owners: Mutex::new(HashMap::new()),
            lifecycle,
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

    /// Hash of the sandbox spec this port will actually execute.
    ///
    /// Tool intents must pin the real policy, not a fixed placeholder string:
    /// the ledger and the executor then agree on what the command ran under.
    pub fn sandbox_spec_hash(&self) -> String {
        let spec = self
            .policy
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .sandbox_spec
            .clone();
        let bytes = serde_json::to_vec(&spec).unwrap_or_default();
        sha256_hex(&bytes)
    }

    /// Install a single-use approval grant for one specific `execution_id`.
    pub fn grant_approval(&self, grant: ToolApprovalGrant) {
        self.approvals
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(grant.execution_id.clone(), grant);
    }

    /// Revoke an unconsumed approval grant before dispatch.
    pub fn revoke_approval(&self, execution_id: &str) -> bool {
        self.approvals
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(execution_id)
            .is_some()
    }

    /// Inspect a retained owner (backgrounded process or unstopped task).
    pub fn retained_owner(&self, execution_id: &str) -> Option<RetainedToolOwner> {
        self.retained_owners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(execution_id)
            .cloned()
    }

    pub fn retained_owners_len(&self) -> usize {
        self.retained_owners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }

    /// Explicitly settle and release the quota held by a backgrounded or
    /// previously indeterminate execution after its process has been reaped.
    pub fn settle_retained_owner(
        &self,
        execution_id: &str,
        reason: ReleaseReason,
    ) -> Result<bool, String> {
        let owner = self
            .retained_owners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(execution_id)
            .cloned();
        let Some(owner) = owner else {
            return Ok(false);
        };
        if let (Some(ledger), Some(reservation)) = (&self.quota, &owner.reservation) {
            ledger
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .release(&reservation.reservation_id, reason)?;
        }
        Ok(self
            .retained_owners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(execution_id)
            .is_some())
    }

    /// Reconcile backgrounded tool owners before reserving more quota. Process
    /// completion is authoritative in ProcessRegistry; no per-owner poller thread
    /// or duplicate lifecycle state is needed.
    fn reap_finished_process_owners(&self) {
        let pending: Vec<(String, u64)> = self
            .retained_owners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .filter_map(|owner| {
                owner
                    .process_id
                    .map(|process_id| (owner.execution_id.clone(), process_id))
            })
            .collect();

        for (execution_id, process_id) in pending {
            let Ok(process_id) = u32::try_from(process_id) else {
                continue;
            };
            let _ = qaqh_process_tools::process_registry::ProcessRegistry::try_wait(process_id);
            let info = qaqh_process_tools::process_registry::ProcessRegistry::get_info(process_id);
            let reason = match info {
                None => ReleaseReason::Completed,
                Some(info) => match info["status"].as_str() {
                    Some("running") => continue,
                    Some("killed") => ReleaseReason::Cancelled,
                    Some("exited") => ReleaseReason::Completed,
                    _ => continue,
                },
            };
            if let Err(error) = self.settle_retained_owner(&execution_id, reason) {
                log::warn!(
                    "[tool-job] could not release reaped process quota for {execution_id}: {error}"
                );
            }
        }
    }

    fn retain_owner(
        &self,
        request: &ToolRequest,
        process_id: Option<u64>,
        backgrounded: bool,
        reservation: Option<QuotaReservation>,
    ) {
        self.retained_owners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(
                request.execution_id.clone(),
                RetainedToolOwner {
                    session_id: self.session_id.clone(),
                    call_id: request.call_id.clone(),
                    execution_id: request.execution_id.clone(),
                    process_id,
                    backgrounded,
                    reservation,
                },
            );
    }

    fn uncertain_process_id(request: &ToolRequest) -> Option<u64> {
        if request.tool_name != "process" {
            return None;
        }
        let args: serde_json::Value = serde_json::from_slice(&request.args).ok()?;
        if args.get("action")?.as_str()? != "write" {
            return None;
        }
        args.get("id")?.as_u64()
    }

    fn release_reservation(&self, reservation: Option<QuotaReservation>, reason: ReleaseReason) {
        if let (Some(ledger), Some(res)) = (&self.quota, reservation) {
            let _ = ledger
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .release(&res.reservation_id, reason);
        }
    }
}

/// Pre-dispatch admission verdict. `execute` re-evaluates it; this only lets
/// the caller raise the interaction before committing a tool intent result.
#[derive(Debug, Clone)]
pub enum ToolAdmission {
    Authorized,
    ApprovalRequired { reason: String },
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

        if policy.mode == qaqh_workspace::tool_api::AgentMode::Plan
            && qaqh_workspace::PLAN_BLOCKED.contains(&request.tool_name.as_str())
        {
            return Err(ToolFailure::Denied(format!(
                "tool '{}' is blocked in PLAN mode",
                request.tool_name
            )));
        }

        let category = self
            .manager
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .category_of(&request.tool_name)
            .ok_or_else(|| {
                ToolFailure::NotStarted(format!("unknown tool: {}", request.tool_name))
            })?;

        let cancellation = qaqh_workspace::tool_api::CancellationToken::new();
        let context = qaqh_workspace::tool_api::ToolCallContext {
            call_id: request.call_id.clone(),
            session_id: self.session_id.clone(),
            workspace_root: policy.workspace_root.clone(),
            mode: policy.mode,
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
                    reason: challenge.reason().to_string(),
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

impl ToolPort for WorkspaceToolPort {
    fn execute(
        &self,
        request: &ToolRequest,
        cancel: Arc<AtomicBool>,
    ) -> Result<ToolOutput, ToolFailure> {
        let args_json = String::from_utf8(request.args.clone()).ok();
        if cancel.load(Ordering::Acquire) {
            self.lifecycle.settled(
                &request.call_id,
                &request.tool_name,
                args_json.clone(),
                failed_terminal("cancelled", "cancelled before dispatch", None, None),
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
                        failed_terminal("permission_denied", &reason, None, None),
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
                                failed_terminal("permission_denied", &reason, None, None),
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
                            failed_terminal("permission_denied", &reason, None, None),
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
                                failed_terminal("permission_denied", &reason, None, None),
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
                failed_terminal("cancelled", "cancelled before handler start", None, None),
            );
            return Err(ToolFailure::NotStarted(
                "cancelled before handler start".into(),
            ));
        }

        self.reap_finished_process_owners();

        let reservation = match &self.quota {
            Some(ledger) => match ledger
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .reserve(
                    QuotaKind::Tool,
                    1,
                    format!("tool:{}:{}", self.session_id, request.execution_id),
                    None,
                ) {
                Ok(reservation) => Some(reservation),
                Err(err) => {
                    let message = format!("tool quota reservation refused: {err}");
                    self.lifecycle.settled(
                        &request.call_id,
                        &request.tool_name,
                        args_json.clone(),
                        failed_terminal("quota_refused", &message, None, None),
                    );
                    return Err(ToolFailure::NotStarted(message));
                }
            },
            None => None,
        };

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
                self.retain_owner(request, None, false, reservation);
                let metrics = run_metrics(&run);
                self.lifecycle.finished(
                    run,
                    failed_terminal(
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
        self.reap_finished_process_owners();

        let parsed_output: Option<serde_json::Value> =
            serde_json::from_str(&exec_result.content).ok();
        let is_backgrounded = parsed_output
            .as_ref()
            .and_then(|v| v.get("status"))
            .and_then(|v| v.as_str())
            == Some("backgrounded");
        let process_id = parsed_output
            .as_ref()
            .and_then(|v| v.get("process_id"))
            .and_then(|v| v.as_u64());

        if is_backgrounded {
            self.retain_owner(request, process_id, true, reservation);
            self.reap_finished_process_owners();
            let metrics = run_metrics(&run);
            self.lifecycle.finished(
                run,
                ToolTerminal {
                    // 调用结清但进程仍在跑:状态保持运行中,由后台收割落终态。
                    status: ToolCallStatus::Running,
                    output_text: Some(exec_result.content.clone()),
                    output_truncated: false,
                    error: None,
                    metrics: Some(metrics),
                    code_delta: code_delta_of(&exec_result),
                    effects: effect_infos(&exec_result),
                },
            );
            return Ok(ToolOutput {
                output: exec_result.content.into_bytes(),
                backgrounded: true,
            });
        }

        if exec_result.success {
            self.release_reservation(reservation, ReleaseReason::Completed);
            let metrics = run_metrics(&run);
            self.lifecycle.finished(
                run,
                ToolTerminal {
                    status: ToolCallStatus::Finished,
                    output_text: Some(exec_result.content.clone()),
                    output_truncated: false,
                    error: None,
                    metrics: Some(metrics),
                    code_delta: code_delta_of(&exec_result),
                    effects: effect_infos(&exec_result),
                },
            );
            return Ok(ToolOutput {
                output: exec_result.content.into_bytes(),
                backgrounded: false,
            });
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
                self.release_reservation(reservation, ReleaseReason::Cancelled);
                let terminal = exec_error_terminal(&run, &exec_result, ToolCallStatus::Failed);
                self.lifecycle.finished(run, terminal);
                Err(ToolFailure::Denied(exec_result.content))
            }
            "manager_unavailable"
            | "runtime_not_initialized"
            | "audit_unavailable"
            | "cancelled_before_start" => {
                self.release_reservation(reservation, ReleaseReason::Cancelled);
                let terminal = exec_error_terminal(&run, &exec_result, ToolCallStatus::Failed);
                self.lifecycle.finished(run, terminal);
                Err(ToolFailure::NotStarted(exec_result.content))
            }
            "partial" | "audit_quarantined" | "tool_panicked" => {
                self.retain_owner(
                    request,
                    Self::uncertain_process_id(request),
                    false,
                    reservation,
                );
                let terminal = exec_error_terminal(&run, &exec_result, ToolCallStatus::Failed);
                self.lifecycle.finished(run, terminal);
                Err(ToolFailure::AfterStart(exec_result.content))
            }
            "cancelled" if !request.read_only => {
                self.retain_owner(
                    request,
                    Self::uncertain_process_id(request),
                    false,
                    reservation,
                );
                let terminal = exec_error_terminal(&run, &exec_result, ToolCallStatus::Cancelled);
                self.lifecycle.finished(run, terminal);
                Err(ToolFailure::AfterStart(exec_result.content))
            }
            _ => {
                self.release_reservation(reservation, ReleaseReason::Completed);
                let terminal = exec_error_terminal(&run, &exec_result, ToolCallStatus::Failed);
                self.lifecycle.finished(run, terminal);
                Err(ToolFailure::TerminalError {
                    message: exec_result.content.clone(),
                    output: exec_result.content.into_bytes(),
                })
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
            ToolFailure::Denied(reason) => failed_terminal("permission_denied", reason, None, None),
            ToolFailure::NotStarted(reason) => failed_terminal("not_started", reason, None, None),
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

/// 执行期错误 → 业务终态:code 取执行器 typed 错误,正文保留完整输出。
fn exec_error_terminal(
    run: &ToolRun,
    result: &qaqh_workspace::execution::ToolExecResult,
    status: ToolCallStatus,
) -> ToolTerminal {
    let code = result
        .result
        .error
        .as_ref()
        .map(|e| e.code.as_str().to_string())
        .unwrap_or_else(|| "tool_failed".into());
    let mut terminal = failed_terminal(
        &code,
        &result.content,
        Some(result.content.clone()),
        Some(run_metrics(run)),
    );
    terminal.status = status;
    terminal
}

fn failed_terminal(
    code: &str,
    message: &str,
    output_text: Option<String>,
    metrics: Option<ToolMetrics>,
) -> ToolTerminal {
    ToolTerminal {
        status: ToolCallStatus::Failed,
        output_text,
        output_truncated: false,
        error: Some(ToolError {
            code: code.to_string(),
            message: message.to_string(),
            retryable: false,
            details_ref: None,
        }),
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
