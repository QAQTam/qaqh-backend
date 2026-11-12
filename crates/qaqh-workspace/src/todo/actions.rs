//! todo::actions — **seed 参数化**的服务面直访入口。
//!
//! 文本范式（apply_patch 形态）收敛后，create/insert/positioned 等遗留路径已随
//! ID 体系退役。模型侧的变更通道是 `typed::{todo_write_for_typed,
//! todo_update_for_typed}`，它们从 runtime 线程上下文取当前会话；本文件只留
//! **显式带 session_id** 的那批——HTTP service 面与 CLI 直访没有线程上下文可取。

use serde_json::Value;

/// Seed 参数化的 todo.set 直访变体（HTTP service 面 / CLI 用；不依赖
/// runtime 线程局部上下文）。
pub fn todo_set_for(session_id: &str, args: &Value) -> Result<String, String> {
    serde_json::to_string(&todo_set_value_for(session_id, args)?)
        .map_err(|error| format!("todo: {error}"))
}

pub fn todo_set_value_for(session_id: &str, args: &Value) -> Result<Value, String> {
    super::typed::todo_update_for_typed(session_id, args)?.to_envelope_value()
}

/// Seed 参数化的 todo.list 直访变体（HTTP service 面 / CLI 用）。
pub fn todo_list_for(session_id: &str, args: &Value) -> Result<String, String> {
    super::typed::todo_list_for_typed(session_id, args)?.to_envelope_string()
}
