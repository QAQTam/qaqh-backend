//! ToolRuntime：生产工具 worker 的唯一执行边界。
//!
//! P3-1 只收敛执行机制，不改变工具契约或事件语义：
//! - 授权工具在 runtime 统一 spawn worker；
//! - 进度通道统一在 runtime 排空；
//! - outbox 记录与 join 归一统一在 runtime 完成；
//! - UI、LLM batch、approved-resume batch 不再各自复制这段逻辑。
//!
//! sandbox 不在本层实现。P4 只需在这个边界外包/注入 sandbox 执行器。

use std::collections::HashSet;
use std::sync::mpsc::Receiver;
use std::thread::JoinHandle;

use qaqh_message::PendingTool;
use qaqh_workspace::AuthorizedToolCall;
use qaqh_workspace::ExecProgressEvent;
use qaqh_workspace::runtime::ToolExecutionScope;

use crate::agent::dashboard;
use crate::agent::engine_tool::ToolEngine;
use crate::agent::types::{AdmittedTool, RingContext};

/// 并行工具 worker 上限（保持 v1 行为）。
pub(crate) const MAX_PARALLEL_TOOL_WORKERS: usize = 4;

/// 取消收尾的取消原因文案（进入模型上下文的 tool_result）。
const CANCELLED_TOOL_RESULT: &str = "[CANCELLED] Tool was not executed (user interrupted).";

/// 一个已启动的工具 worker。
pub(crate) struct ToolRun {
    pub call_id: String,
    pub tool_name: String,
    handle: JoinHandle<qaqh_workspace::execution::ToolExecResult>,
}

/// worker 的归一结果；panic 保留给调用方决定回填文案/路径。
pub(crate) enum ToolRunOutcome {
    Completed(Box<qaqh_workspace::execution::ToolExecResult>),
    Panicked,
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
    /// parallel batching, serial tail, progress, outbox, cancellation sealing
    /// and ordered skill effects.
    pub(crate) fn execute_batch(
        ctx: &mut RingContext,
        tool: &ToolEngine,
        mut admitted: Vec<AdmittedTool>,
        tool_call_order: &[String],
        serial_call_ids: &HashSet<String>,
        turn_id: &str,
        round_num: u32,
    ) -> bool {
        // L3 outbox: execution facts are recorded inside the tool worker thread,
        // right after the tool returns, so a kill between "tool ran" and "result
        // persisted" is still distinguishable from "tool never ran".
        let outbox_seed = ctx.agent.session.seed.clone();
        admitted.sort_by_key(|item| {
            tool_call_order
                .iter()
                .position(|id| id == &item.call_id)
                .unwrap_or(usize::MAX)
        });
        let mut ordered_skill_effects = Vec::new();
        let (mut parallel, serial): (Vec<_>, Vec<_>) = admitted
            .into_iter()
            .partition(|item| !serial_call_ids.contains(&item.call_id));

        while !parallel.is_empty() {
            // 批间取消检查：取消已到达就不再 spawn 新工具线程、不再发
            // Running 事件；剩余批由下方补取消终态。
            if ctx.cancel.is_set() {
                let remaining = parallel
                    .iter()
                    .map(|item| item.call_id.clone())
                    .chain(serial.iter().map(|item| item.call_id.clone()));
                seal_unexecuted_as_cancelled(ctx, remaining, CANCELLED_TOOL_RESULT);
                apply_ordered_skill_effects(ctx, ordered_skill_effects, tool_call_order);
                return false;
            }
            let batch_len = parallel.len().min(MAX_PARALLEL_TOOL_WORKERS);
            let batch: Vec<_> = parallel.drain(..batch_len).collect();
            for admitted in &batch {
                let call_id = admitted.call_id.clone();
                let tool_name = admitted.auth.tool_name().to_string();
                let tool_args = admitted.auth.args().clone();
                ToolEngine::emit_timeline_tool_running(
                    ctx, turn_id, round_num, &call_id, &tool_name, &tool_args,
                );
            }
            let (progress_rx, runs) = Self::spawn_batch(batch, outbox_seed.clone());
            let cancelled = ctx.cancel.is_set();
            let results = Self::collect(ctx, tool, progress_rx, runs, turn_id, round_num);

            // BUG-2026-09-13-08：取消不再丢弃已执行结果。
            //
            // 工具线程一旦 spawn 就真实执行（副作用已发生、outbox 已记录），
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
                }
            }
            if cancelled {
                // 剩余并行批根本没起，或上面的项被回收时仍未落结果——统一补终态，
                // 保证「每个 tool_use 恰有一条 tool_result」。
                let remaining = parallel.iter().map(|item| item.call_id.clone());
                seal_unexecuted_as_cancelled(ctx, remaining, CANCELLED_TOOL_RESULT);
                parallel.clear();
                break;
            }
        }

        let mut serial = serial.into_iter();
        while let Some(admitted) = serial.next() {
            if ctx.cancel.is_set() {
                // 串行路径取消：已收集的 skill_effects 照常应用，未执行的项（当前
                // 这一项及其后全部）补取消终态。旧实现 `return false` 会丢弃已
                // 收集的 skill_effects，并把未执行的 tool_use 留成 open。
                let remaining = std::iter::once(admitted.call_id)
                    .chain(serial.by_ref().map(|admitted| admitted.call_id));
                seal_unexecuted_as_cancelled(ctx, remaining, CANCELLED_TOOL_RESULT);
                apply_ordered_skill_effects(ctx, ordered_skill_effects, tool_call_order);
                return false;
            }
            let call_id = admitted.call_id.clone();
            let tool_name = admitted.auth.tool_name().to_string();
            let tool_args = admitted.auth.args().clone();
            ToolEngine::emit_timeline_tool_running(
                ctx, turn_id, round_num, &call_id, &tool_name, &tool_args,
            );
            let (progress_rx, runs) = Self::spawn_batch(vec![admitted], outbox_seed.clone());
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

    /// 启动一批已经授权的工具 worker。
    ///
    /// 调用方负责把 `admitted` 限制在 [`MAX_PARALLEL_TOOL_WORKERS`] 内；本函数
    /// 不做调度决策，只负责执行机制。进度 sender 共享给本批所有 worker。
    pub(crate) fn spawn_batch(
        admitted: Vec<AdmittedTool>,
        outbox_seed: String,
    ) -> (Receiver<ExecProgressEvent>, Vec<ToolRun>) {
        let (progress_tx, progress_rx) = qaqh_workspace::bounded_exec_progress_channel();
        let mut runs = Vec::with_capacity(admitted.len());
        for admitted in admitted {
            let tx = progress_tx.clone();
            runs.push(Self::spawn(
                admitted.call_id,
                admitted.auth.tool_name().to_string(),
                admitted.auth,
                admitted.scope,
                tx,
                outbox_seed.clone(),
            ));
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
        tool_name: String,
        auth: Box<AuthorizedToolCall>,
        context: qaqh_workspace::tool_api::ToolCallContext,
        turn_id: &str,
        round_num: u32,
    ) -> ToolRunResult {
        let (progress_tx, progress_rx) = qaqh_workspace::bounded_exec_progress_channel();
        let run = Self::spawn(
            call_id,
            tool_name,
            auth,
            ToolExecutionScope::capture(context),
            progress_tx,
            ctx.agent.session.seed.clone(),
        );
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
                let outcome = match run.handle.join() {
                    Ok(result) => ToolRunOutcome::Completed(Box::new(result)),
                    Err(_) => ToolRunOutcome::Panicked,
                };
                ToolRunResult {
                    call_id: run.call_id,
                    tool_name: run.tool_name,
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
        outbox_seed: String,
    ) -> ToolRun {
        let worker_call_id = call_id.clone();
        let worker_tool_name = tool_name.clone();
        let handle = std::thread::Builder::new()
            .stack_size(4 * 1024 * 1024)
            .spawn(move || {
                let context = scope.context().clone();
                let _scope = scope.install();
                let result = qaqh_workspace::execution::execute_authorized_with_context(
                    *auth,
                    context,
                    Some(progress_tx),
                );
                crate::agent::tool_outbox::record(
                    &outbox_seed,
                    &worker_call_id,
                    &worker_tool_name,
                    result.success,
                );
                result
            })
            .expect("tool thread spawn");

        ToolRun {
            call_id,
            tool_name,
            handle,
        }
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
/// - outbox 里有执行记录的（工具真的跑过，只是结果没等到）→ 跳过，
///   由回填路径负责；
/// - 其余（从未执行 / 取消点在 spawn 之前）→ 补 `Cancelled` 终态。
///
/// 返回补了终态的项数。
fn seal_unexecuted_as_cancelled(
    ctx: &mut RingContext,
    call_ids: impl IntoIterator<Item = String>,
    reason: &str,
) -> usize {
    let executed = crate::agent::tool_outbox::executed_call_ids(&ctx.agent.session.seed);
    let mut sealed = 0;
    for call_id in call_ids {
        if ctx.agent.msg.step_has_tool_result(&call_id) || executed.contains(&call_id) {
            continue;
        }
        ctx.agent.msg.push_tool_result_canonical(
            &call_id,
            &qaqh_types::ToolResult::cancelled(reason),
            &[],
        );
        sealed += 1;
    }
    sealed
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
