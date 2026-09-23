//! `turn_lap` 的测试可见面（BUG-2026-09-13-08 回归用）。
//!
//! `turn_lap::admit` 是 `pub(crate)`：生产路径只从 loop 内部调用。集成测试
//! （`tests/`）无法触及 crate 私有模块，但取消收割语义必须在**真实**的
//! `execute_admitted_batch` 上验证（不能靠复制实现冒充）。这里只包装测试入口，
//! 不扩大其它内部面。

use std::collections::HashSet;

use crate::agent::engine_tool::ToolEngine;
use crate::agent::tool_runtime::ToolBatchOrigin;
use crate::agent::turn_actor::TurnActor;
use crate::agent::types::{AdmittedTool, RingContext};

/// Execute an admitted batch through the production runtime with a fresh
/// active actor, matching the normal (non-resume) production call shape.
pub fn execute_admitted_batch(
    ctx: &mut RingContext,
    tool: &ToolEngine,
    admitted: Vec<AdmittedTool>,
    tool_call_order: &[String],
    serial_call_ids: &HashSet<String>,
    turn_id: &str,
    round_num: u32,
) -> bool {
    let mut actor = TurnActor::new();
    actor
        .begin_input(turn_id, &format!("test-input:{turn_id}"))
        .expect("test actor start");
    crate::agent::turn_lap::admit::execute_admitted_batch(
        ctx,
        tool,
        Some(&mut actor),
        ToolBatchOrigin::Normal,
        admitted,
        tool_call_order,
        serial_call_ids,
        turn_id,
        round_num,
    )
}
