//! `turn_lap` 的测试可见面（BUG-2026-09-13-08 回归用）。
//!
//! `turn_lap::admit` 是 `pub(crate)`：生产路径只从 loop 内部调用。集成测试
//! （`tests/`）无法触及 crate 私有模块，但取消收割语义必须在**真实**的
//! `execute_admitted_batch` 上验证（不能靠复制实现冒充）。这里只包装测试入口，
//! 不扩大其它内部面。

use std::collections::HashSet;

use crate::agent::engine_tool::ToolEngine;
use crate::agent::engine_turn::TurnEngine;
use crate::agent::state::agent::AgentState;
use crate::agent::tool_runtime::ToolBatchOrigin;
use crate::agent::turn_actor::TurnActor;
use crate::agent::types::{AdmittedTool, Outcome, RingContext, TurnState, YieldReason};

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

/// Exercise the production yield observer and durable interaction-request
/// write path from integration tests.
pub fn observe_yield_for_test(
    engine: &mut TurnEngine,
    agent: &mut AgentState,
    turn_id: &str,
    input_id: &str,
    pending_call_id: &str,
) -> Result<(), String> {
    engine
        .begin_input(turn_id, input_id)
        .map_err(|error| error.to_string())?;
    engine.suspended = Some(TurnState {
        session_id: agent.session.session_id.clone(),
        turn_id: turn_id.to_string(),
        round_num: 0,
        pending_permission_ids: vec![pending_call_id.to_string()],
        pending_permission_bodies: Vec::new(),
        deferred_authorized: Vec::new(),
        tool_call_order: vec![pending_call_id.to_string()],
        serial_call_ids: HashSet::new(),
        pending_asks: std::collections::VecDeque::new(),
        pending_plans: std::collections::VecDeque::new(),
        pending_todo_activation: None,
        usage: None,
        reason: YieldReason::PermissionPending,
    });
    engine
        .observe_outcome(
            agent,
            &Outcome::YieldToUser {
                turn_id: turn_id.to_string(),
                reason: YieldReason::PermissionPending,
            },
        )
        .map_err(|error| error.to_string())
}

/// Exercise the production yield observer and durable interaction-request
/// write path from integration tests（ask 变体，覆盖 #345 的正文 ref）。
pub fn observe_ask_yield_for_test(
    engine: &mut TurnEngine,
    agent: &mut AgentState,
    turn_id: &str,
    input_id: &str,
    pending_call_id: &str,
    mode: qaqh_domain::AskMode,
    questions: Vec<qaqh_domain::AskQuestion>,
) -> Result<(), String> {
    engine
        .begin_input(turn_id, input_id)
        .map_err(|error| error.to_string())?;
    let mut pending_asks = std::collections::VecDeque::new();
    pending_asks.push_back(crate::agent::types::PendingAsk {
        call_id: pending_call_id.to_string(),
        mode,
        questions,
    });
    engine.suspended = Some(TurnState {
        session_id: agent.session.session_id.clone(),
        turn_id: turn_id.to_string(),
        round_num: 0,
        pending_permission_ids: Vec::new(),
        pending_permission_bodies: Vec::new(),
        deferred_authorized: Vec::new(),
        tool_call_order: vec![pending_call_id.to_string()],
        serial_call_ids: HashSet::new(),
        pending_asks,
        pending_plans: std::collections::VecDeque::new(),
        pending_todo_activation: None,
        usage: None,
        reason: YieldReason::AskUser,
    });
    engine
        .observe_outcome(
            agent,
            &Outcome::YieldToUser {
                turn_id: turn_id.to_string(),
                reason: YieldReason::AskUser,
            },
        )
        .map_err(|error| error.to_string())
}

/// Exercise the production yield observer and durable interaction-request
/// write path from integration tests（permission 变体，覆盖授权详情正文 ref）。
pub fn observe_permission_yield_for_test(
    engine: &mut TurnEngine,
    agent: &mut AgentState,
    turn_id: &str,
    input_id: &str,
    pending_call_id: &str,
    body: Vec<u8>,
) -> Result<(), String> {
    engine
        .begin_input(turn_id, input_id)
        .map_err(|error| error.to_string())?;
    engine.suspended = Some(TurnState {
        session_id: agent.session.session_id.clone(),
        turn_id: turn_id.to_string(),
        round_num: 0,
        pending_permission_ids: vec![pending_call_id.to_string()],
        pending_permission_bodies: vec![(pending_call_id.to_string(), body)],
        deferred_authorized: Vec::new(),
        tool_call_order: vec![pending_call_id.to_string()],
        serial_call_ids: HashSet::new(),
        pending_asks: std::collections::VecDeque::new(),
        pending_plans: std::collections::VecDeque::new(),
        pending_todo_activation: None,
        usage: None,
        reason: YieldReason::PermissionPending,
    });
    engine
        .observe_outcome(
            agent,
            &Outcome::YieldToUser {
                turn_id: turn_id.to_string(),
                reason: YieldReason::PermissionPending,
            },
        )
        .map_err(|error| error.to_string())
}

/// Exercise the production interaction-resolution write path from integration
/// tests.
pub fn record_interaction_resolution_for_test(
    agent: &mut AgentState,
    interaction_id: &str,
    decision: &str,
    command_id: Option<&str>,
) -> Result<(), String> {
    TurnEngine::record_interaction_resolution(agent, interaction_id, decision, command_id)
        .map_err(|error| error.to_string())
}
