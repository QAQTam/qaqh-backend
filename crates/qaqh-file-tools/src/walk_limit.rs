//! Walk 停走护栏：把 `ToolCallContext::timeout` / `cancellation` 变成目录
//! 遍历循环里真正被消费的 deadline 检查。
//!
//! 背景（2026-11-17）：`glob`/`grep` 曾在 15 万文件、无 ignore 规则的工作区
//! 上单次跑 236s 无任何出口——`descriptor.default_timeout` 注入 ctx 后无人
//! 读取，等于装饰品。护栏必须在 walk 的每个条目上生效（Windows 下逐条目
//! 本就付 µs 级 syscall，`Instant::now()` 的开销可忽略）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::tool_api::ToolCallContext;

#[derive(Clone)]
pub(crate) struct WalkLimit {
    deadline: Option<Instant>,
    cancel: Arc<AtomicBool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WalkStop {
    Deadline,
    Cancelled,
}

impl WalkLimit {
    pub(crate) fn from_ctx(ctx: &ToolCallContext) -> Self {
        // timeout == 0 只出现在直接构造 ctx 的路径（descriptor 校验拒绝
        // 非交互工具零超时）；语义按"不设 deadline"处理。
        let deadline = if ctx.timeout == Duration::ZERO {
            None
        } else {
            Some(Instant::now() + ctx.timeout)
        };
        Self {
            deadline,
            cancel: ctx.cancellation.shared_flag(),
        }
    }

    /// deadline 优先返回（取消检查在前：取消是显式意图，压过一切）。
    pub(crate) fn stopped(&self) -> Option<WalkStop> {
        if self.cancel.load(Ordering::SeqCst) {
            return Some(WalkStop::Cancelled);
        }
        if self.deadline.is_some_and(|d| Instant::now() >= d) {
            return Some(WalkStop::Deadline);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expired_deadline_reports_stop() {
        let ctx_cancel = crate::tool_api::CancellationToken::new();
        let limit = WalkLimit {
            deadline: Some(Instant::now() - Duration::from_secs(1)),
            cancel: ctx_cancel.shared_flag(),
        };
        assert_eq!(limit.stopped(), Some(WalkStop::Deadline));
        // 取消优先于 deadline。
        ctx_cancel.cancel();
        assert_eq!(limit.stopped(), Some(WalkStop::Cancelled));
    }

    #[test]
    fn zero_timeout_has_no_deadline() {
        let ctx_cancel = crate::tool_api::CancellationToken::new();
        let limit = WalkLimit {
            deadline: None,
            cancel: ctx_cancel.shared_flag(),
        };
        assert_eq!(limit.stopped(), None);
    }
}
