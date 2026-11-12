//! todo — 会话级任务清单（Session-scoped todo management）。
//!
//! 持久化 `sessions/{seed}/todo.json`。模型工具面 = `todo_write`（纯文本全量
//! 覆写，`list` 是唯一参数）/ `todo_update`（按 title 单条翻转）；只读 `todo_list`
//! 已从模型面退役，仅作为 service/CLI 直访变体存在（`actions::todo_list_for`）。

pub mod actions;
pub mod model;
pub mod parse;
pub mod split;
pub mod store;
pub(crate) mod typed;

pub use actions::{todo_list_for, todo_set_for, todo_set_value_for};
pub use model::{TodoItem, TodoMode, TodoStatus, TodoStore};
pub use split::register;
pub use store::{
    load_todo, load_todo_for, save_todo, todo_cancel_json, todo_cancel_value, todo_meta_snapshot,
    todo_status_json, todo_status_value,
};
pub use typed::{todo_list_snapshot_value, todo_list_value_for};

#[cfg(test)]
mod tests;
