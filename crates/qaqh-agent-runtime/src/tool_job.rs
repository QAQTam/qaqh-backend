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

use qaqh_agentloop_core::{DispatchEvidence, mailbox::FrameSize};
use qaqh_session::{
    canonical::sha256_content_hash,
    session_commit::{CommittedExecution, FenceReason},
    session_fact_v2::{FactPayload, SideEffectClass, ToolReplayCapability, ToolTerminalStatus},
};
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

/// Everything the authorized effect pinned before it was committed.
#[derive(Debug, Clone)]
pub struct ToolRequest {
    pub execution_id: String,
    pub call_id: String,
    pub tool_name: String,
    pub side_effect_class: SideEffectClass,
    pub replay: ToolReplayCapability,
    /// Effective arguments, verified against the intent's content reference.
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
        cancel: &AtomicBool,
    ) -> Result<ToolOutput, ToolFailure>;
}

#[derive(Debug, Clone)]
pub struct ToolOutcome {
    pub evidence: DispatchEvidence,
    pub terminal: Option<ToolTerminalStatus>,
    pub output: Vec<u8>,
    /// The executor kept external work alive (backgrounded process / unstopped
    /// task). The invocation is settled, the side effect is not.
    pub backgrounded: bool,
    pub error: Option<String>,
}

impl FrameSize for ToolOutcome {
    fn retained_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            .saturating_add(self.output.len())
            .saturating_add(self.error.as_ref().map_or(0, String::capacity))
    }
}

/// Policy snapshot re-evaluated immediately before dispatching a tool handler.
#[derive(Clone, Debug)]
pub struct ToolPolicySnapshot {
    pub workspace_root: PathBuf,
    pub mode: qaqh_workspace::tool_api::AgentMode,
    pub permission_level: qaqh_workspace::PermissionLevel,
    pub sandbox: qaqh_workspace::tool_api::SandboxMode,
    pub trusted_dirs: HashSet<PathBuf>,
}

impl ToolPolicySnapshot {
    pub fn workspace_write(workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            workspace_root: workspace_root.into(),
            mode: qaqh_workspace::tool_api::AgentMode::Code,
            permission_level: qaqh_workspace::PermissionLevel::WorkspaceWrite,
            sandbox: qaqh_workspace::tool_api::SandboxMode::Main,
            trusted_dirs: HashSet::new(),
        }
    }
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
pub struct WorkspaceToolPort {
    session_id: String,
    manager: Arc<Mutex<qaqh_workspace::ToolManager>>,
    policy: RwLock<ToolPolicySnapshot>,
    approvals: Mutex<HashMap<String, ToolApprovalGrant>>,
    quota: Option<Arc<Mutex<QuotaLedger>>>,
    retained_owners: Mutex<HashMap<String, RetainedToolOwner>>,
}

impl WorkspaceToolPort {
    pub fn new(
        session_id: impl Into<String>,
        manager: Arc<Mutex<qaqh_workspace::ToolManager>>,
        policy: ToolPolicySnapshot,
        quota: Option<Arc<Mutex<QuotaLedger>>>,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            manager,
            policy: RwLock::new(policy),
            approvals: Mutex::new(HashMap::new()),
            quota,
            retained_owners: Mutex::new(HashMap::new()),
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

    /// Workspace root the current policy dispatches against.
    pub fn workspace_root(&self) -> PathBuf {
        self.policy
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .workspace_root
            .clone()
    }

    /// Extend the trusted-directory set ("trust this folder" decisions).
    pub fn trust_dir(&self, dir: PathBuf) {
        self.policy
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .trusted_dirs
            .insert(dir);
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
        let removed = self
            .retained_owners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(execution_id);
        let Some(owner) = removed else {
            return Ok(false);
        };
        if let (Some(ledger), Some(reservation)) = (&self.quota, &owner.reservation) {
            ledger
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .release(&reservation.reservation_id, reason)?;
        }
        Ok(true)
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
        let args_hash = sha256_content_hash(&request.args).0;

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
            sandbox_spec: qaqh_workspace::tool_api::SandboxSpec::for_permission(
                policy.workspace_root.clone(),
                policy.permission_level,
                true,
            )
            .with_trusted_roots(policy.trusted_dirs.clone()),
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
        sha256_content_hash(args).0
    }
}

/// Sets the completion flag even when the handler panics, so the scoped
/// cancellation poller can never outlive a failed call.
struct DoneGuard(Arc<AtomicBool>);

impl Drop for DoneGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

impl ToolPort for WorkspaceToolPort {
    fn execute(
        &self,
        request: &ToolRequest,
        cancel: &AtomicBool,
    ) -> Result<ToolOutput, ToolFailure> {
        if cancel.load(Ordering::Acquire) {
            return Err(ToolFailure::NotStarted("cancelled before dispatch".into()));
        }
        let PreparedCall {
            context,
            invocation,
            policy,
            args_hash,
        } = self.prepare(request)?;

        let authorized = match qaqh_workspace::admit_with_context(
            invocation,
            &context,
            &policy.trusted_dirs,
        ) {
            qaqh_workspace::Admission::Authorized(call) => call,
            qaqh_workspace::Admission::Denied(reason) => {
                return Err(ToolFailure::Denied(reason));
            }
            qaqh_workspace::Admission::ApprovalRequired(challenge) => {
                let grant = self
                    .approvals
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&request.execution_id)
                    .ok_or_else(|| {
                        ToolFailure::Denied(format!(
                            "permission approval required or revoked for {}",
                            request.tool_name
                        ))
                    })?;
                let normalized_ws =
                    qaqh_workspace::resolve_target_path(policy.workspace_root.clone());
                let grant_ws = qaqh_workspace::resolve_target_path(grant.workspace_root);
                if grant.call_id != request.call_id
                    || grant.execution_id != request.execution_id
                    || grant.args_hash != args_hash
                    || grant_ws != normalized_ws
                {
                    return Err(ToolFailure::Denied(
                        "approval grant does not match execution identity, arguments, or workspace"
                            .into(),
                    ));
                }
                challenge.approve(true).map_err(|err| {
                    ToolFailure::Denied(format!("approval challenge rejected: {err:?}"))
                })?
            }
        };

        if cancel.load(Ordering::Acquire) {
            return Err(ToolFailure::NotStarted(
                "cancelled before handler start".into(),
            ));
        }

        let reservation = match &self.quota {
            Some(ledger) => Some(
                ledger
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .reserve(
                        QuotaKind::Tool,
                        1,
                        format!("tool:{}:{}", self.session_id, request.execution_id),
                        None,
                    )
                    .map_err(|err| {
                        ToolFailure::NotStarted(format!("tool quota reservation refused: {err}"))
                    })?,
            ),
            None => None,
        };

        let scope = qaqh_workspace::runtime::ToolExecutionScope::with_manager(
            context.clone(),
            Arc::clone(&self.manager),
        );
        let _guard = scope.install();

        let done = Arc::new(AtomicBool::new(false));
        let done_watch = Arc::clone(&done);
        let cancel_token = context.cancellation.clone();
        // Propagate cooperative cancellation into the tool context while the
        // synchronous handler is running. The guard clears `done` even if the
        // handler panics, so the scoped poller cannot outlive the scope.
        let exec_result = std::thread::scope(|s| {
            let _done = DoneGuard(Arc::clone(&done));
            s.spawn(move || {
                while !done_watch.load(Ordering::Acquire) {
                    if cancel.load(Ordering::Acquire) {
                        cancel_token.cancel();
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            });
            qaqh_workspace::execution::execute_authorized_with_context(authorized, context, None)
        });

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
                        backgrounded: true,
                        reservation,
                    },
                );
            return Ok(ToolOutput {
                output: exec_result.content.into_bytes(),
                backgrounded: true,
            });
        }

        if exec_result.success {
            self.release_reservation(reservation, ReleaseReason::Completed);
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
                Err(ToolFailure::Denied(exec_result.content))
            }
            "manager_unavailable" | "runtime_not_initialized" | "audit_unavailable" => {
                self.release_reservation(reservation, ReleaseReason::Cancelled);
                Err(ToolFailure::NotStarted(exec_result.content))
            }
            "partial" | "audit_quarantined" => {
                self.retained_owners
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(
                        request.execution_id.clone(),
                        RetainedToolOwner {
                            session_id: self.session_id.clone(),
                            call_id: request.call_id.clone(),
                            execution_id: request.execution_id.clone(),
                            process_id: None,
                            backgrounded: false,
                            reservation,
                        },
                    );
                Err(ToolFailure::AfterStart(exec_result.content))
            }
            "cancelled" if request.side_effect_class != SideEffectClass::ReadOnly => {
                self.retained_owners
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(
                        request.execution_id.clone(),
                        RetainedToolOwner {
                            session_id: self.session_id.clone(),
                            call_id: request.call_id.clone(),
                            execution_id: request.execution_id.clone(),
                            process_id: None,
                            backgrounded: false,
                            reservation,
                        },
                    );
                Err(ToolFailure::AfterStart(exec_result.content))
            }
            _ => {
                self.release_reservation(reservation, ReleaseReason::Completed);
                Err(ToolFailure::TerminalError {
                    message: exec_result.content.clone(),
                    output: exec_result.content.into_bytes(),
                })
            }
        }
    }
}

pub async fn run<P: ToolPort>(
    port: Arc<P>,
    tool_name: String,
    effect: CommittedExecution,
    mut cancel: tokio::sync::watch::Receiver<bool>,
) -> ToolOutcome {
    let parts = match effect.into_parts() {
        Ok(parts) => parts,
        Err(reason) => return fenced(reason),
    };
    let FactPayload::ToolIntent(intent) = parts.fact.payload else {
        return fenced(FenceReason::UnsupportedIntent);
    };
    let Some(args) = parts.tool_args else {
        // Nothing was launched: the intent has no verified arguments.
        return not_started("tool intent has no committed arguments");
    };
    let request = ToolRequest {
        execution_id: intent.execution_id.as_str().to_owned(),
        call_id: intent.call_id.as_str().to_owned(),
        tool_name,
        side_effect_class: intent.side_effect_class,
        replay: intent.replay_capability.clone(),
        args,
    };
    let cancelled = Arc::new(AtomicBool::new(*cancel.borrow()));
    let flag = cancelled.clone();
    let mut job = tokio::task::spawn_blocking(move || classify(port.execute(&request, &flag)));
    loop {
        tokio::select! {
            result = &mut job => return joined(result),
            changed = cancel.changed() => {
                if changed.is_err() || *cancel.borrow() {
                    cancelled.store(true, Ordering::Release);
                    break;
                }
            }
        }
    }
    // Never drop a running blocking handle and claim the tool stopped.
    joined(job.await)
}

fn joined(result: Result<ToolOutcome, tokio::task::JoinError>) -> ToolOutcome {
    match result {
        Ok(outcome) => outcome,
        Err(error) => ToolOutcome {
            evidence: DispatchEvidence::StartedUnknown,
            terminal: None,
            output: Vec::new(),
            backgrounded: false,
            error: Some(
                format!("tool worker join failed: {error}")
                    .chars()
                    .take(512)
                    .collect(),
            ),
        },
    }
}

fn classify(result: Result<ToolOutput, ToolFailure>) -> ToolOutcome {
    match result {
        Ok(output) if output.backgrounded => ToolOutcome {
            evidence: DispatchEvidence::StartedUnknown,
            terminal: Some(ToolTerminalStatus::Backgrounded),
            output: output.output,
            backgrounded: true,
            error: None,
        },
        Ok(output) => ToolOutcome {
            evidence: DispatchEvidence::StartedTerminal,
            terminal: Some(ToolTerminalStatus::Succeeded),
            output: output.output,
            backgrounded: false,
            error: None,
        },
        Err(ToolFailure::NotStarted(reason)) => not_started(&reason),
        Err(ToolFailure::Denied(reason)) => ToolOutcome {
            evidence: DispatchEvidence::NotStarted,
            terminal: Some(ToolTerminalStatus::Denied),
            output: Vec::new(),
            backgrounded: false,
            error: Some(reason.chars().take(512).collect()),
        },
        Err(ToolFailure::TerminalError { message, output }) => ToolOutcome {
            evidence: DispatchEvidence::StartedTerminal,
            terminal: Some(ToolTerminalStatus::Failed),
            output,
            backgrounded: false,
            error: Some(message.chars().take(512).collect()),
        },
        Err(ToolFailure::AfterStart(reason)) => ToolOutcome {
            evidence: DispatchEvidence::StartedUnknown,
            terminal: None,
            output: Vec::new(),
            backgrounded: false,
            error: Some(reason.chars().take(512).collect()),
        },
    }
}

fn fenced(reason: FenceReason) -> ToolOutcome {
    not_started(&format!("capability fenced before start: {reason}"))
}

fn not_started(message: &str) -> ToolOutcome {
    ToolOutcome {
        evidence: DispatchEvidence::NotStarted,
        terminal: None,
        output: Vec::new(),
        backgrounded: false,
        error: Some(message.chars().take(512).collect()),
    }
}
