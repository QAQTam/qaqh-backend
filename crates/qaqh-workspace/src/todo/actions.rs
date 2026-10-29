//! todo::actions — 工具与服务面入口（write/update/list）。
//!
//! 文本范式（apply_patch 形态）收敛后：create/insert/positioned 等遗留
//! 路径已随 ID 体系退役；`todo_write`（全量文本覆写）与 `todo_update`
//!（按 title 单条翻转）是唯二变更通道。

use serde_json::Value;

// ═══════════════════════════════════════════════════════
// Status parsing（服务面字符串 → typed enum）
// ═══════════════════════════════════════════════════════

// ═══════════════════════════════════════════════════════
// 工具入口（runtime ctx → typed canonical 路径）
// ═══════════════════════════════════════════════════════

pub(crate) fn exec_todo_write(args: &Value) -> Result<String, String> {
    let session_id = crate::runtime::context()
        .map(|ctx| ctx.active_session)
        .unwrap_or_default();
    super::typed::todo_write_for_typed(&session_id, args)?.to_envelope_string()
}

pub(crate) fn exec_todo_set(args: &Value) -> Result<String, String> {
    let session_id = crate::runtime::context()
        .map(|ctx| ctx.active_session)
        .unwrap_or_default();
    todo_set_for(&session_id, args)
}

/// Seed 参数化的 todo.set 直访变体（HTTP service 面 / CLI 用；不依赖
/// runtime 线程局部上下文）。
pub fn todo_set_for(session_id: &str, args: &Value) -> Result<String, String> {
    serde_json::to_string(&todo_set_value_for(session_id, args)?)
        .map_err(|error| format!("todo: {error}"))
}

pub fn todo_set_value_for(session_id: &str, args: &Value) -> Result<Value, String> {
    super::typed::todo_update_for_typed(session_id, args)?.to_envelope_value()
}

pub(crate) fn exec_todo_list(args: &Value) -> Result<String, String> {
    let session_id = crate::runtime::context()
        .map(|ctx| ctx.active_session)
        .unwrap_or_default();
    todo_list_for(&session_id, args)
}

/// Seed 参数化的 todo.list 直访变体（HTTP service 面 / CLI 用）。
pub fn todo_list_for(session_id: &str, args: &Value) -> Result<String, String> {
    super::typed::todo_list_for_typed(session_id, args)?.to_envelope_string()
}
