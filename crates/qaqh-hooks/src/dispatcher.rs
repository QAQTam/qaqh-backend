//! 五门 dispatcher:PreToolUse / PermissionRequest / UserPromptSubmit /
//! PostToolUse / Stop + 观察事件。
//!
//! 运行顺序 = 声明序(Config → Global → Project);第一个 deny/block 终止
//! 后续 handler(grok 同律)。fail-open 为缺省,`on_failure = block` 的
//! handler 失败按 block 处理。异步 handler 发后不管,不参与任何门决策。

use std::time::Instant;

use crate::config::FailurePolicy;
use crate::discovery::{HookEntry, HookRegistry, HookSource};
use crate::event::{HookEventEnvelope, HookEventName, HookPayload};
use crate::result::{
    HookRefusal, ObserveOutcome, PermissionOutcome, PostToolUseOutcome, PreToolUseOutcome,
    PromptOutcome, RunRecord, RunStatus, StopOutcome,
};
use crate::runner::{run_handler, DecisionJson, RunnerOutcome, RunContext};

/// 单事件派发上下文;trust 不可信的项目 hook 记为 SkippedUntrusted。
pub struct Dispatch<'a> {
    pub registry: &'a HookRegistry,
    pub ctx: RunContext,
}

impl<'a> Dispatch<'a> {
    fn eligible(
        &self,
        entry: &HookEntry,
        envelope: &HookEventEnvelope,
    ) -> (bool, Option<RunRecord>) {
        if entry.source.trusted_by_default() {
            return (true, None);
        }
        // 项目作用域:dispatch 前由调用方刷新 project_trusted;这里按标记跳过。
        (self.project_trusted, None)
    }

    /// 项目 hooks 当前是否放行(dispatch 构造后可更新)。
    pub fn project_trusted: bool = true,
}
