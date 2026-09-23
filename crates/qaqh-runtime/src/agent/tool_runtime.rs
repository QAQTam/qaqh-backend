//! ToolRuntime：生产工具 worker 的唯一执行边界。
//!
//! P3-1 只收敛执行机制，不改变工具契约或事件语义：
//! - 授权工具在 runtime 统一 spawn worker；
//! - 进度通道统一在 runtime 排空；
//! - canonical ToolIntent/ToolFinished 提交与 join 归一统一在 runtime 完成；
//! - UI、LLM batch、approved-resume batch 不再各自复制这段逻辑。
//!
//! sandbox 不在本层实现。P4 只需在这个边界外包/注入 sandbox 执行器。

use std::collections::HashSet;
use std::sync::mpsc::Receiver;
use std::thread::JoinHandle;

use qaqh_message::PendingTool;
use qaqh_session::actor::ToolAdmission;
use qaqh_session::canonical::{
    ToolLedgerError, generate_ulid, sha256_content_hash, ulid_from_text,
};
use qaqh_session::session_fact_v2::{
    ContentRef, EventId, ExecutionId, PolicyDecisionRef, SideEffectClass, ToolCallId, ToolError,
    ToolFinished, ToolIntent, ToolIntentPolicyOutcome, ToolMetrics, ToolReplayCapability,
    ToolTerminalStatus, TurnId,
};
use qaqh_workspace::AuthorizedToolCall;
use qaqh_workspace::ExecProgressEvent;
use qaqh_workspace::runtime::ToolExecutionScope;

use crate::agent::dashboard;
use crate::agent::engine_tool::ToolEngine;
use crate::agent::state::agent::{tool_ledger_lease_ms, unix_ms};
use crate::agent::turn_actor::TurnActor;
use crate::agent::types::{AdmittedTool, RingContext};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolBatchOrigin {
    Normal,
    Resume,
}

/// 并行工具 worker 上限（保持 v1 行为）。
pub(crate) const MAX_PARALLEL_TOOL_WORKERS: usize = 4;

/// 取消收尾的取消原因文案（进入模型上下文的 tool_result）。
const CANCELLED_TOOL_RESULT: &str = "[CANCELLED] Tool was not executed (user interrupted).";

#[derive(Clone)]
struct LedgerRun {
    execution_id: ExecutionId,
    intent_at_ms: i64,
}

/// A prepared call either enters the handler or is rejected by the durable
/// ledger before any side effect can happen.
enum ToolRunMode {
    /// Append the durable intent immediately before this worker is spawned.
    Prepare,
    Execute {
        ledger: Option<LedgerRun>,
    },
    Blocked {
        message: String,
    },
}

/// 一个已启动的工具 worker。
pub(crate) struct ToolRun {
    pub call_id: String,
    pub tool_name: String,
    handle: JoinHandle<qaqh_workspace::execution::ToolExecResult>,
    ledger: Option<LedgerRun>,
}

/// worker 的归一结果；panic 保留给调用方决定回填文案/路径。
pub(crate) enum ToolRunOutcome {
    Completed(Box<qaqh_workspace::execution::ToolExecResult>),
    Panicked,
    /// Handler completed but the canonical terminal could not be committed.
    LedgerFailed(String),
}

pub(crate) struct ToolRunResult {
    pub call_id: String,
    pub tool_name: String,
    pub outcome: ToolRunOutcome,
}

pub(crate) struct ToolRuntime;

impl ToolRuntime {
    /// 计算本批后续写者集合。迁移期继续使用既有 conflict 算法，
    /// capabilities 切换留到显式行为变更切片。
    pub(crate) fn serial_call_ids(pending: &[PendingTool]) -> HashSet<String> {
        let write_pairs: Vec<(String, serde_json::Value)> = pending
            .iter()
            .map(|tool| (tool.name.clone(), tool.args.clone()))
            .collect();
        let (_serial_groups, serial_after) =
            qaqh_workspace::conflict::resolve_write_conflicts(&write_pairs);
        serial_after
            .iter()
            .map(|index| pending[*index].id.clone())
            .collect()
    }

    /// Execute a batch of already-authorized tools, partitioned by write conflicts.
    ///
    /// Owns the existing v1 scheduling/cancellation/backfill behavior. Callers
    /// provide the admitted batch and original model order; this method handles
    /// parallel batching, serial tail, progress, ledger commit, cancellation sealing
    /// and ordered skill effects.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn execute_batch(
        ctx: &mut RingContext,
        tool: &ToolEngine,
        actor: Option<&mut TurnActor>,
        origin: ToolBatchOrigin,
        admitted: Vec<AdmittedTool>,
        tool_call_order: &[String],
        serial_call_ids: &HashSet<String>,
        turn_id: &str,
        round_num: u32,
    ) -> bool {
        let mut actor = actor;
        let mut admitted = Self::prepare_admitted(ctx, admitted, turn_id);
        admitted.sort_by_key(|(item, _)| {
            tool_call_order
                .iter()
                .position(|id| id == &item.call_id)
                .unwrap_or(usize::MAX)
        });
        let mut ordered_skill_effects = Vec::new();
        let (mut parallel, serial): (Vec<_>, Vec<_>) = admitted
            .into_iter()
            .partition(|(item, _)| !serial_call_ids.contains(&item.call_id));

        while !parallel.is_empty() {
            // 批间取消检查：取消已到达就不再 spawn 新工具线程、不再发
            // Running 事件；剩余批由下方补取消终态。
            if ctx.cancel.is_set() {
                let remaining = parallel
                    .iter()
                    .map(|(item, _)| item.call_id.clone())
                    .chain(serial.iter().map(|(item, _)| item.call_id.clone()));
                let actor = actor.as_deref_mut();
                seal_unexecuted_as_cancelled(ctx, actor, remaining, CANCELLED_TOOL_RESULT, turn_id);
                apply_ordered_skill_effects(ctx, ordered_skill_effects, tool_call_order);
                return false;
            }
            let batch_len = parallel.len().min(MAX_PARALLEL_TOOL_WORKERS);
            let batch: Vec<_> = parallel.drain(..batch_len).collect();
            for (admitted, _) in &batch {
                let call_id = admitted.call_id.clone();
                let tool_name = admitted.auth.tool_name().to_string();
                let tool_args = admitted.auth.args().clone();
                ToolEngine::emit_timeline_tool_running(
                    ctx, turn_id, round_num, &call_id, &tool_name, &tool_args,
                );
            }
            let actor_ref = actor.as_deref_mut();
            let (progress_rx, runs) = Self::spawn_batch(ctx, actor_ref, origin, batch, turn_id);
            let cancelled = ctx.cancel.is_set();
            let results = Self::collect(ctx, tool, progress_rx, runs, turn_id, round_num);

            // BUG-2026-09-13-08：取消不再丢弃已执行结果。
            //
            // 工具线程一旦 spawn 就真实执行（副作用可能已发生，且 ToolIntent 已 durable），
            // 「取消」只意味着不再等剩余项——已 join 出来的 canonical 结果必须
            // 照常回填，否则 store 留下 open tool_use，下轮模型重发 → 重复执行。
            // 剩余批（含仍在执行的项）在下方统一补取消终态。
            for result in results {
                let call_id = result.call_id;
                let tool_name = result.tool_name;
                match result.outcome {
                    ToolRunOutcome::Completed(result) => {
                        let result = *result;
                        backfill_executed_result(
                            ctx,
                            &call_id,
                            &tool_name,
                            turn_id,
                            round_num,
                            result.result,
                            result.code_delta,
                        );
                        ordered_skill_effects.push((call_id.clone(), result.skill_effects));
                    }
                    ToolRunOutcome::Panicked => ctx.agent.msg.push_tool_result_direct(
                        &call_id,
                        "[ERROR] tool thread panicked",
                        false,
                    ),
                    ToolRunOutcome::LedgerFailed(message) => {
                        ctx.agent.msg.push_tool_result_canonical(
                            &call_id,
                            &qaqh_types::ToolResult::error_with(
                                "LEDGER_WRITE_FAILED",
                                message,
                                false,
                                None,
                            ),
                            &[],
                        );
                    }
                }
            }
            if cancelled {
                // 剩余并行批根本没起，或上面的项被回收时仍未落结果——统一补终态，
                // 保证「每个 tool_use 恰有一条 tool_result」。
                let remaining = parallel.iter().map(|(item, _)| item.call_id.clone());
                let actor_ref = actor.as_deref_mut();
                seal_unexecuted_as_cancelled(
                    ctx,
                    actor_ref,
                    remaining,
                    CANCELLED_TOOL_RESULT,
                    turn_id,
                );
                parallel.clear();
                break;
            }
        }

        let mut serial = serial.into_iter();
        while let Some((admitted, mode)) = serial.next() {
            if ctx.cancel.is_set() {
                // 串行路径取消：已收集的 skill_effects 照常应用，未执行的项（当前
                // 这一项及其后全部）补取消终态。旧实现 `return false` 会丢弃已
                // 收集的 skill_effects，并把未执行的 tool_use 留成 open。
                let remaining = std::iter::once(admitted.call_id)
                    .chain(serial.by_ref().map(|(admitted, _)| admitted.call_id));
                let actor = actor.as_deref_mut();
                seal_unexecuted_as_cancelled(ctx, actor, remaining, CANCELLED_TOOL_RESULT, turn_id);
                apply_ordered_skill_effects(ctx, ordered_skill_effects, tool_call_order);
                return false;
            }
            let call_id = admitted.call_id.clone();
            let tool_name = admitted.auth.tool_name().to_string();
            let tool_args = admitted.auth.args().clone();
            ToolEngine::emit_timeline_tool_running(
                ctx, turn_id, round_num, &call_id, &tool_name, &tool_args,
            );
            let actor = actor.as_deref_mut();
            let (progress_rx, runs) =
                Self::spawn_batch(ctx, actor, origin, vec![(admitted, mode)], turn_id);
            let results = Self::collect(ctx, tool, progress_rx, runs, turn_id, round_num);
            for result in results {
                match result.outcome {
                    ToolRunOutcome::Completed(result) => {
                        let result = *result;
                        backfill_executed_result(
                            ctx,
                            &call_id,
                            &tool_name,
                            turn_id,
                            round_num,
                            result.result,
                            result.code_delta,
                        );
                        ordered_skill_effects.push((call_id.clone(), result.skill_effects));
                    }
                    ToolRunOutcome::Panicked => ctx.agent.msg.push_tool_result_direct(
                        &call_id,
                        "[ERROR] tool thread panicked",
                        false,
                    ),
                    ToolRunOutcome::LedgerFailed(message) => {
                        ctx.agent.msg.push_tool_result_canonical(
                            &call_id,
                            &qaqh_types::ToolResult::error_with(
                                "LEDGER_WRITE_FAILED",
                                message,
                                false,
                                None,
                            ),
                            &[],
                        );
                    }
                }
            }
        }

        if ctx.cancel.is_set() {
            // 串行循环收尾后仍可能落在取消态（最后一项 join 期间收到取消）：
            // 结果已回填，这里只做终态兜底，绝不丢弃。
            apply_ordered_skill_effects(ctx, ordered_skill_effects, tool_call_order);
            return false;
        }
        apply_ordered_skill_effects(ctx, ordered_skill_effects, tool_call_order);
        true
    }

    /// Classify every admitted call against the durable ledger before
    /// scheduling. New calls are marked `Prepare`; their intent is appended
    /// immediately before the worker spawn so a cancelled serial tail does not
    /// leave an orphan intent.
    fn prepare_admitted(
        ctx: &mut RingContext,
        admitted: Vec<AdmittedTool>,
        wire_turn_id: &str,
    ) -> Vec<(AdmittedTool, ToolRunMode)> {
        let ledger = match ctx.agent.tool_ledger_mut() {
            Ok(Some(ledger)) => ledger,
            Ok(None) => {
                return admitted
                    .into_iter()
                    .map(|item| (item, ToolRunMode::Execute { ledger: None }))
                    .collect();
            }
            Err(error) => {
                let message = format!("tool ledger unavailable: {error}");
                return admitted
                    .into_iter()
                    .map(|item| {
                        (
                            item,
                            ToolRunMode::Blocked {
                                message: message.clone(),
                            },
                        )
                    })
                    .collect();
            }
        };

        let mut prepared = Vec::with_capacity(admitted.len());
        for item in admitted {
            let call_id = canonical_call_id(&item.call_id);
            let existing = ledger
                .get(&call_id)
                .map(|entry| (entry.finished().cloned(), entry.intent().cloned()));

            if let Some((Some(finished), _)) = existing {
                prepared.push((
                    item,
                    ToolRunMode::Blocked {
                        message: format!(
                            "tool call {} already has terminal {:?}; refusing to execute again",
                            call_id, finished.terminal_status
                        ),
                    },
                ));
                continue;
            }

            if let Some((_, Some(intent))) = existing {
                if matches!(
                    &intent.replay_capability,
                    ToolReplayCapability::IdempotentReplay
                ) {
                    let now = unix_ms();
                    if let Err(error) = ledger.ensure_lease(now, tool_ledger_lease_ms()) {
                        prepared.push((
                            item,
                            ToolRunMode::Blocked {
                                message: format!(
                                    "tool ledger lease renewal failed before replay: {error}"
                                ),
                            },
                        ));
                        continue;
                    }
                    prepared.push((
                        item,
                        ToolRunMode::Execute {
                            ledger: Some(LedgerRun {
                                execution_id: intent.execution_id.clone(),
                                intent_at_ms: intent.intent_at_ms,
                            }),
                        },
                    ));
                    continue;
                }

                let now = unix_ms();
                let finished = Self::indeterminate_finished(&call_id, &intent, now);
                let message = match ledger.append_finished(
                    EventId::new(generate_ulid()),
                    Some(canonical_turn_id(wire_turn_id)),
                    finished,
                    now,
                ) {
                    Ok(_) => format!(
                        "tool call {} is indeterminate after an open intent; refusing replay",
                        call_id
                    ),
                    Err(error) => format!(
                        "tool call {} has an open intent and could not be sealed: {error}",
                        call_id
                    ),
                };
                prepared.push((item, ToolRunMode::Blocked { message }));
                continue;
            }

            // New durable calls append their intent immediately before the
            // worker spawn in `spawn_batch`; appending here would create
            // orphan intents for a serial tail cancelled before it runs.
            prepared.push((item, ToolRunMode::Prepare));
        }
        prepared
    }

    fn prepare_one(
        ctx: &mut RingContext,
        actor: Option<&mut TurnActor>,
        origin: ToolBatchOrigin,
        item: &AdmittedTool,
        wire_turn_id: &str,
    ) -> Result<Option<LedgerRun>, String> {
        let Some(ledger) = ctx
            .agent
            .tool_ledger_mut()
            .map_err(|error| format!("tool ledger unavailable: {error}"))?
        else {
            return Ok(None);
        };
        let now = unix_ms();
        ledger
            .ensure_lease(now, tool_ledger_lease_ms())
            .map_err(|error| format!("tool ledger lease renewal failed: {error}"))?;
        let execution_id = ExecutionId::new(format!("exec_{}", generate_ulid()));
        let intent = Self::build_intent(item, &execution_id, now);
        let actor_turn = TurnId::new(wire_turn_id);
        let canonical_turn = canonical_turn_id(wire_turn_id);
        let event_id = EventId::new(generate_ulid());

        if let Some(actor) = actor {
            let resume_interaction =
                (origin == ToolBatchOrigin::Resume).then_some(item.call_id.as_str());
            let admission = actor
                .admit_tool_intent(
                    ledger,
                    &actor_turn,
                    &canonical_turn,
                    event_id,
                    intent,
                    now,
                    resume_interaction,
                )
                .map_err(|error| format!("tool intent admission failed: {error}"))?;
            return match admission {
                ToolAdmission::Admitted {
                    execution_id,
                    intent_at_ms,
                } => Ok(Some(LedgerRun {
                    execution_id,
                    intent_at_ms,
                })),
                ToolAdmission::ExistingIntent { intent } => {
                    if matches!(
                        intent.replay_capability,
                        ToolReplayCapability::IdempotentReplay
                    ) {
                        Ok(Some(LedgerRun {
                            execution_id: intent.execution_id,
                            intent_at_ms: intent.intent_at_ms,
                        }))
                    } else {
                        Err(format!(
                            "tool call {} already has an open intent; refusing replay",
                            intent.call_id
                        ))
                    }
                }
                ToolAdmission::ExistingFinished { finished } => Err(format!(
                    "tool call {} already has terminal {:?}; refusing to execute again",
                    finished.call_id, finished.terminal_status
                )),
                ToolAdmission::Cancelled => Err(format!(
                    "tool call {} was cancelled before admission",
                    canonical_call_id(&item.call_id)
                )),
                ToolAdmission::TurnTerminal { turn_id, terminal } => Err(format!(
                    "turn {turn_id} is terminal ({terminal:?}); refusing tool admission"
                )),
            };
        }

        ledger
            .append_intent(event_id, Some(canonical_turn), intent, now)
            .map_err(|error| format!("tool intent append failed: {error}"))?;
        Ok(Some(LedgerRun {
            execution_id,
            intent_at_ms: now,
        }))
    }

    fn build_intent(item: &AdmittedTool, execution_id: &ExecutionId, now: i64) -> ToolIntent {
        let capabilities =
            qaqh_workspace::tool_capabilities::builtin_capabilities(item.auth.tool_name())
                .unwrap_or_default();
        let args_bytes = serde_json::to_vec(item.auth.args()).unwrap_or_default();
        let args_hash = sha256_content_hash(&args_bytes);
        let context = item.scope.context();
        let sandbox_bytes = serde_json::to_vec(&serde_json::json!({
            "workspace_root": context.workspace_root.to_string_lossy(),
            "mode": format!("{:?}", context.mode),
            "permission_level": context.permission_level as u8,
            "sandbox": format!("{:?}", context.sandbox),
        }))
        .unwrap_or_default();
        let sandbox_spec_hash = sha256_content_hash(&sandbox_bytes);

        ToolIntent {
            call_id: canonical_call_id(&item.call_id),
            execution_id: execution_id.clone(),
            idempotency_key: capabilities
                .idempotent
                .then(|| format!("{}:{}", item.auth.tool_name(), item.call_id)),
            replay_capability: if capabilities.idempotent {
                ToolReplayCapability::IdempotentReplay
            } else {
                ToolReplayCapability::NoReplay
            },
            policy_decision: PolicyDecisionRef {
                outcome: ToolIntentPolicyOutcome::Allow,
                rule_id: format!("authorized:{}", item.auth.grant().as_str()),
                decided_at_ms: now,
                reason_ref: None,
            },
            effective_args_ref: Some(ContentRef::new(args_hash.clone())),
            effective_args_hash: Some(args_hash),
            sandbox_spec_hash,
            side_effect_class: side_effect_class(item.auth.tool_name()),
            intent_at_ms: now,
        }
    }

    fn indeterminate_finished(call_id: &ToolCallId, intent: &ToolIntent, now: i64) -> ToolFinished {
        ToolFinished {
            call_id: call_id.clone(),
            execution_id: Some(intent.execution_id.clone()),
            terminal_status: ToolTerminalStatus::Indeterminate,
            output_ref: None,
            error: Some(ToolError {
                code: "indeterminate_after_crash".into(),
                message: "non-idempotent execution was not replayed".into(),
                retryable: false,
                details_ref: None,
            }),
            metrics: ToolMetrics {
                started_at_ms: intent.intent_at_ms,
                finished_at_ms: now,
                retry_count: 0,
                output_bytes: 0,
                progress_bytes_total: 0,
            },
            reconciled: false,
            evidence_ref: None,
            evidence_fact_seq: None,
            evidence_event_id: None,
            recovery_ref: None,
            finished_at_ms: now,
        }
    }

    fn append_finished(
        ctx: &mut RingContext,
        call_id: &str,
        ledger_run: &LedgerRun,
        outcome: &ToolRunOutcome,
        wire_turn_id: &str,
    ) -> Result<(), ToolLedgerError> {
        let Some(ledger) = ctx.agent.tool_ledger_mut()? else {
            return Ok(());
        };
        let now = unix_ms();
        ledger.ensure_lease(now, tool_ledger_lease_ms())?;
        let (terminal_status, error, output_bytes) = match outcome {
            ToolRunOutcome::Completed(result) => (
                terminal_status(&result.result),
                canonical_tool_error(result.result.error.as_ref()),
                result.content.len() as u64,
            ),
            ToolRunOutcome::Panicked => (
                ToolTerminalStatus::Indeterminate,
                Some(ToolError {
                    code: "tool_panicked".into(),
                    message: "tool thread panicked; side-effect outcome is unknown".into(),
                    retryable: false,
                    details_ref: None,
                }),
                0,
            ),
            ToolRunOutcome::LedgerFailed(message) => (
                ToolTerminalStatus::Indeterminate,
                Some(ToolError {
                    code: "ledger_write_failed".into(),
                    message: message.clone(),
                    retryable: false,
                    details_ref: None,
                }),
                0,
            ),
        };
        let finished = ToolFinished {
            call_id: canonical_call_id(call_id),
            execution_id: Some(ledger_run.execution_id.clone()),
            terminal_status,
            output_ref: None,
            error,
            metrics: ToolMetrics {
                started_at_ms: ledger_run.intent_at_ms,
                finished_at_ms: now,
                retry_count: 0,
                output_bytes,
                progress_bytes_total: 0,
            },
            reconciled: false,
            evidence_ref: None,
            evidence_fact_seq: None,
            evidence_event_id: None,
            recovery_ref: None,
            finished_at_ms: now,
        };
        ledger.append_finished(
            EventId::new(generate_ulid()),
            Some(canonical_turn_id(wire_turn_id)),
            finished,
            now,
        )?;
        Ok(())
    }

    /// 启动一批已经授权的工具 worker。
    ///
    /// 调用方负责把 `admitted` 限制在 [`MAX_PARALLEL_TOOL_WORKERS`] 内；本函数
    /// 不做调度决策，只负责执行机制。进度 sender 共享给本批所有 worker。
    fn spawn_batch(
        ctx: &mut RingContext,
        mut actor: Option<&mut TurnActor>,
        origin: ToolBatchOrigin,
        admitted: Vec<(AdmittedTool, ToolRunMode)>,
        wire_turn_id: &str,
    ) -> (Receiver<ExecProgressEvent>, Vec<ToolRun>) {
        let (progress_tx, progress_rx) = qaqh_workspace::bounded_exec_progress_channel();
        let mut runs = Vec::with_capacity(admitted.len());
        for (admitted, mode) in admitted {
            let tx = progress_tx.clone();
            let call_id = admitted.call_id.clone();
            let tool_name = admitted.auth.tool_name().to_string();
            match mode {
                ToolRunMode::Prepare => {
                    let actor = actor.as_deref_mut();
                    match Self::prepare_one(ctx, actor, origin, &admitted, wire_turn_id) {
                        Ok(ledger) => runs.push(Self::spawn(
                            call_id,
                            tool_name,
                            admitted.auth,
                            admitted.scope,
                            tx,
                            ledger,
                        )),
                        Err(message) => {
                            let blocked_name = tool_name.clone();
                            let handle = std::thread::Builder::new()
                                .stack_size(4 * 1024 * 1024)
                                .spawn(move || blocked_tool_exec_result(&blocked_name, &message))
                                .expect("tool thread spawn");
                            runs.push(ToolRun {
                                call_id,
                                tool_name,
                                handle,
                                ledger: None,
                            });
                        }
                    }
                }
                ToolRunMode::Execute { ledger } => runs.push(Self::spawn(
                    call_id,
                    tool_name,
                    admitted.auth,
                    admitted.scope,
                    tx,
                    ledger,
                )),
                ToolRunMode::Blocked { message } => {
                    let blocked_name = tool_name.clone();
                    let handle = std::thread::Builder::new()
                        .stack_size(4 * 1024 * 1024)
                        .spawn(move || blocked_tool_exec_result(&blocked_name, &message))
                        .expect("tool thread spawn");
                    runs.push(ToolRun {
                        call_id,
                        tool_name,
                        handle,
                        ledger: None,
                    });
                }
            }
        }
        drop(progress_tx);
        (progress_rx, runs)
    }

    /// 执行一个 UI 直调工具。上下文在此处捕获为显式 worker scope。
    #[allow(clippy::too_many_arguments)] // 参数面与 UI 调用边界一一对应，后续并入 ToolCallContext 时再收口。
    pub(crate) fn run_authorized(
        ctx: &mut RingContext,
        tool: &ToolEngine,
        call_id: String,
        _tool_name: String,
        auth: Box<AuthorizedToolCall>,
        context: qaqh_workspace::tool_api::ToolCallContext,
        turn_id: &str,
        round_num: u32,
    ) -> ToolRunResult {
        let admitted = AdmittedTool {
            call_id,
            auth,
            scope: ToolExecutionScope::capture(context),
        };
        let prepared = Self::prepare_admitted(ctx, vec![admitted], turn_id);
        let (admitted, mode) = prepared
            .into_iter()
            .next()
            .expect("single admitted tool must produce one prepared call");
        let (progress_rx, mut runs) = Self::spawn_batch(
            ctx,
            None,
            ToolBatchOrigin::Normal,
            vec![(admitted, mode)],
            turn_id,
        );
        let run = runs
            .pop()
            .expect("single tool runtime run must produce one worker");
        Self::collect(ctx, tool, progress_rx, vec![run], turn_id, round_num)
            .into_iter()
            .next()
            .expect("single tool runtime run must produce one result")
    }

    /// 排空本批进度并 join 所有 worker，保持调用方传入的顺序。
    pub(crate) fn collect(
        ctx: &mut RingContext,
        tool: &ToolEngine,
        progress_rx: Receiver<ExecProgressEvent>,
        runs: Vec<ToolRun>,
        turn_id: &str,
        round_num: u32,
    ) -> Vec<ToolRunResult> {
        tool.drain_progress_external(ctx, progress_rx, turn_id, round_num, || {
            runs.iter().all(|run| run.handle.is_finished())
        });

        runs.into_iter()
            .map(|run| {
                let ToolRun {
                    call_id,
                    tool_name,
                    handle,
                    ledger,
                } = run;
                let outcome = match handle.join() {
                    Ok(result) => ToolRunOutcome::Completed(Box::new(result)),
                    Err(_) => ToolRunOutcome::Panicked,
                };
                if let Some(ledger_run) = ledger
                    && let Err(error) =
                        Self::append_finished(ctx, &call_id, &ledger_run, &outcome, turn_id)
                {
                    return ToolRunResult {
                        call_id,
                        tool_name,
                        outcome: ToolRunOutcome::LedgerFailed(error.to_string()),
                    };
                }
                ToolRunResult {
                    call_id,
                    tool_name,
                    outcome,
                }
            })
            .collect()
    }

    fn spawn(
        call_id: String,
        tool_name: String,
        auth: Box<AuthorizedToolCall>,
        scope: ToolExecutionScope,
        progress_tx: qaqh_workspace::ExecProgressSender,
        ledger: Option<LedgerRun>,
    ) -> ToolRun {
        let handle = std::thread::Builder::new()
            .stack_size(4 * 1024 * 1024)
            .spawn(move || {
                let context = scope.context().clone();
                let _scope = scope.install();
                qaqh_workspace::execution::execute_authorized_with_context(
                    *auth,
                    context,
                    Some(progress_tx),
                )
            })
            .expect("tool thread spawn");

        ToolRun {
            call_id,
            tool_name,
            handle,
            ledger,
        }
    }
}

pub(crate) fn canonical_call_id(wire_call_id: &str) -> ToolCallId {
    ToolCallId::new(format!("call_{}", ulid_from_text(wire_call_id)))
}

pub(crate) fn canonical_turn_id(wire_turn_id: &str) -> TurnId {
    TurnId::new(format!("turn_{}", ulid_from_text(wire_turn_id)))
}

pub(crate) fn canonical_interaction_id(
    wire_interaction_id: &str,
) -> qaqh_session::session_fact_v2::InteractionId {
    qaqh_session::session_fact_v2::InteractionId::new(format!(
        "int_{}",
        ulid_from_text(wire_interaction_id)
    ))
}

fn terminal_status(result: &qaqh_types::ToolResult) -> ToolTerminalStatus {
    if result
        .error
        .as_ref()
        .is_some_and(|error| error.code == "AUDIT_QUARANTINED")
    {
        return ToolTerminalStatus::Indeterminate;
    }
    if result
        .error
        .as_ref()
        .is_some_and(|error| error.code == "TIMEOUT")
    {
        return ToolTerminalStatus::TimedOut;
    }
    match result.status {
        qaqh_types::ToolStatus::Ok => ToolTerminalStatus::Succeeded,
        qaqh_types::ToolStatus::Error => ToolTerminalStatus::Failed,
        qaqh_types::ToolStatus::Partial => ToolTerminalStatus::Partial,
        qaqh_types::ToolStatus::Backgrounded => ToolTerminalStatus::Backgrounded,
        qaqh_types::ToolStatus::Cancelled => ToolTerminalStatus::Cancelled,
    }
}

fn canonical_tool_error(error: Option<&qaqh_types::ToolError>) -> Option<ToolError> {
    error.map(|error| ToolError {
        code: error.code.clone(),
        message: error.message.clone(),
        retryable: error.retryable,
        details_ref: None,
    })
}

fn side_effect_class(tool_name: &str) -> SideEffectClass {
    match tool_name {
        "read" | "glob" | "grep" | "read_image" | "todo_list" => SideEffectClass::ReadOnly,
        "exec" | "process" => SideEffectClass::Process,
        "web_fetch" => SideEffectClass::Network,
        _ => SideEffectClass::WorkspaceWrite,
    }
}

fn blocked_tool_exec_result(
    tool_name: &str,
    message: &str,
) -> qaqh_workspace::execution::ToolExecResult {
    qaqh_workspace::execution::ToolExecResult {
        content: message.to_string(),
        success: false,
        result: qaqh_types::ToolResult::error_with(
            "LEDGER_BLOCKED",
            message.to_string(),
            false,
            None,
        ),
        meta: qaqh_workspace::ToolExecMeta {
            name: tool_name.to_string(),
            elapsed_ms: 0,
            output_size: 0,
            success: false,
            args_summary: String::new(),
        },
        code_delta: None,
        skill_effects: Vec::new(),
    }
}

/// 取消收尾：保证每个 tool_use 恰有一条 tool_result。
///
/// BUG-2026-09-13-08：`abort_running_turn` 会调用
/// `remove_last_step_if_incomplete()`，而该方法要求 step 全部 tool_use 都有
/// 结果。取消时若把「已执行但未回填」的项和「根本没跑」的项一视同仁，整个
/// step（含已执行结果）会被一起丢弃 → 下轮模型重发同一 tool_use，工具重复
/// 执行。
///
/// 因此本函数只处理确实不会再有结果的 call_id：
/// - 已在本轮回填过结果的（`msg.step_has_tool_result`）→ 跳过；
/// - canonical ledger 已有 intent 的（工具已准入，结果可能未等到）→ 跳过，
///   由回填或 recovery 路径负责；
/// - 其余（从未执行 / 取消点在 spawn 之前）→ 补 `Cancelled` 终态。
///
/// 返回补了终态的项数。
fn seal_unexecuted_as_cancelled(
    ctx: &mut RingContext,
    actor: Option<&mut TurnActor>,
    call_ids: impl IntoIterator<Item = String>,
    reason: &str,
    wire_turn_id: &str,
) -> usize {
    let call_ids: Vec<String> = call_ids.into_iter().collect();
    let started = match ctx.agent.tool_ledger_mut() {
        Ok(Some(ledger)) => call_ids
            .iter()
            .filter(|call_id| ledger.get(&canonical_call_id(call_id)).is_some())
            .cloned()
            .collect::<HashSet<_>>(),
        Ok(None) => HashSet::new(),
        Err(error) => {
            log::error!("[TOOL] canonical ledger unavailable during cancel sealing: {error}");
            HashSet::new()
        }
    };
    let unexecuted: Vec<String> = call_ids
        .into_iter()
        .filter(|call_id| {
            !ctx.agent.msg.step_has_tool_result(call_id) && !started.contains(call_id)
        })
        .collect();
    for call_id in &unexecuted {
        ctx.agent.msg.push_tool_result_canonical(
            call_id,
            &qaqh_types::ToolResult::cancelled(reason),
            &[],
        );
    }

    if let Some(actor) = actor {
        let now = unix_ms();
        match ctx.agent.tool_ledger_mut() {
            Ok(Some(ledger)) => {
                let actor_turn = TurnId::new(wire_turn_id);
                let canonical_turn = canonical_turn_id(wire_turn_id);
                let canonical_calls = unexecuted
                    .iter()
                    .map(|call_id| canonical_call_id(call_id))
                    .collect();
                if let Err(error) = actor.cancel_tool_batch(
                    ledger,
                    &actor_turn,
                    &canonical_turn,
                    canonical_calls,
                    now,
                ) {
                    log::warn!(
                        "[tool-ledger] actor cancellation failed for turn {wire_turn_id}: {error}"
                    );
                }
            }
            Ok(None) => {
                if let Err(error) = actor.cancel(wire_turn_id) {
                    log::warn!(
                        "[tool-ledger] actor cancellation failed for turn {wire_turn_id}: {error}"
                    );
                }
            }
            Err(error) => {
                log::warn!(
                    "[tool-ledger] ledger unavailable during cancellation for turn {wire_turn_id}: {error}"
                );
            }
        }
    } else {
        for call_id in &unexecuted {
            append_executionless_cancelled(ctx, call_id, wire_turn_id);
        }
    }

    unexecuted.len()
}

/// Seal a call that was cancelled before any durable ToolIntent was appended.
fn append_executionless_cancelled(ctx: &mut RingContext, call_id: &str, wire_turn_id: &str) {
    let now = unix_ms();
    let finished = ToolFinished {
        call_id: canonical_call_id(call_id),
        execution_id: None,
        terminal_status: ToolTerminalStatus::Cancelled,
        output_ref: None,
        error: None,
        metrics: ToolMetrics {
            started_at_ms: now,
            finished_at_ms: now,
            retry_count: 0,
            output_bytes: 0,
            progress_bytes_total: 0,
        },
        reconciled: false,
        evidence_ref: None,
        evidence_fact_seq: None,
        evidence_event_id: None,
        recovery_ref: None,
        finished_at_ms: now,
    };
    match ctx.agent.tool_ledger_mut() {
        Ok(Some(ledger)) => {
            if let Err(error) = ledger.ensure_lease(now, tool_ledger_lease_ms()) {
                log::warn!("[tool-ledger] cancelled call {call_id} lease renewal failed: {error}");
                return;
            }
            if let Err(error) = ledger.append_finished(
                EventId::new(generate_ulid()),
                Some(canonical_turn_id(wire_turn_id)),
                finished,
                now,
            ) {
                log::warn!(
                    "[tool-ledger] cancelled call {call_id} terminal append failed: {error}"
                );
            }
        }
        Ok(None) => {}
        Err(error) => {
            log::warn!("[tool-ledger] cancelled call {call_id} ledger unavailable: {error}");
        }
    }
}

/// 回填一个已执行工具的结果，并发射其副作用（code delta / dashboard）。
fn backfill_executed_result(
    ctx: &mut RingContext,
    call_id: &str,
    tool_name: &str,
    turn_id: &str,
    round_num: u32,
    canonical_result: qaqh_types::ToolResult,
    code_delta: Option<qaqh_domain::CodeDeltaRecord>,
) {
    ctx.agent
        .msg
        .push_tool_result_canonical(call_id, &canonical_result, &canonical_result.images);
    if let Some(ref delta) = code_delta {
        ctx.stats.push_delta(delta.clone());
        // Ringing 双发：CodeChanged（与 engine_tool 同载荷）
        ctx.emitter.emit_domain(qaqh_domain::DomainEvent::Tool(
            qaqh_domain::ToolEvent::CodeChanged {
                tool_call_id: call_id.to_string(),
                turn_id: turn_id.to_string(),
                round_num,
                lines_added: delta.lines_added,
                lines_removed: delta.lines_removed,
                files_created: delta.files_created,
                files_deleted: delta.files_deleted,
                file: delta.file.clone(),
            },
        ));
    }
    // Instant refresh for todo tools
    if matches!(tool_name, "todo") {
        // Ringing 双发：DashboardUpdated（replaceable 覆盖）
        ctx.emitter.emit_domain(qaqh_domain::DomainEvent::Control(
            qaqh_domain::ControlEvent::DashboardUpdated {
                hp_connected: true,
                session_seed: ctx.agent.session.seed.clone(),
                tool_calls_total: 0,
                tool_failures: 0,
                current_phase: "single".into(),
                streaming: false,
            },
        ));
        ctx.emitter.emit_domain(qaqh_domain::DomainEvent::Control(
            qaqh_domain::ControlEvent::DashboardSnapshot {
                snapshot: dashboard::build_snapshot(ctx.agent.session.seed.clone()),
            },
        ));
    }
}

/// 按模型原始 tool_call_order 应用已收集的 skill effects。
fn apply_ordered_skill_effects(
    ctx: &mut RingContext,
    mut ordered: Vec<(String, Vec<qaqh_workspace::ToolEffect>)>,
    tool_call_order: &[String],
) {
    ordered.sort_by_key(|(call_id, _)| {
        tool_call_order
            .iter()
            .position(|id| id == call_id)
            .unwrap_or(usize::MAX)
    });
    for (_, effects) in ordered {
        ctx.agent.apply_tool_effects(effects, ctx.flow);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_error_maps_to_canonical_timed_out() {
        let result =
            qaqh_types::ToolResult::error_with("TIMEOUT", "tool timed out".to_string(), true, None);
        assert_eq!(
            terminal_status(&result),
            ToolTerminalStatus::TimedOut,
            "timeout must not collapse into generic failed"
        );
    }

    #[test]
    fn audit_quarantine_maps_to_canonical_indeterminate() {
        let result = qaqh_types::ToolResult::error_with(
            "AUDIT_QUARANTINED",
            "result audit failed after side effects".to_string(),
            false,
            None,
        );
        assert_eq!(
            terminal_status(&result),
            ToolTerminalStatus::Indeterminate,
            "quarantined side effects must not be reported as a definite failure"
        );
    }
}
