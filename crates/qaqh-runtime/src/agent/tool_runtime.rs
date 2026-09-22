//! ToolRuntime：生产工具 worker 的唯一执行边界。
//!
//! P3-1 只收敛执行机制，不改变工具契约或事件语义：
//! - 授权工具在 runtime 统一 spawn worker；
//! - 进度通道统一在 runtime 排空；
//! - outbox 记录与 join 归一统一在 runtime 完成；
//! - UI、LLM batch、approved-resume batch 不再各自复制这段逻辑。
//!
//! sandbox 不在本层实现。P4 只需在这个边界外包/注入 sandbox 执行器。

use std::sync::mpsc::Receiver;
use std::thread::JoinHandle;

use qaqh_workspace::AuthorizedToolCall;
use qaqh_workspace::ExecProgressEvent;
use qaqh_workspace::runtime::ToolExecutionScope;

use crate::agent::engine_tool::ToolEngine;
use crate::agent::types::{AdmittedTool, RingContext};

/// 并行工具 worker 上限（保持 v1 行为）。
pub(crate) const MAX_PARALLEL_TOOL_WORKERS: usize = 4;

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
