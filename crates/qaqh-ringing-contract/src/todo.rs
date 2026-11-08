//! Canonical todo service read model, shared by daemon and native clients.
use serde::Serialize;

/// `todo.list` 结果条目(status 为 todo 词表字符串)。
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct TodoItemSnapshot {
    pub id: String,
    pub title: String,
    pub description: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
}

/// `todo.list` 结果的计数投影。
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct TodoCountsSnapshot {
    pub pending: usize,
    pub in_progress: usize,
    pub completed: usize,
    pub cancelled: usize,
    pub total: usize,
}

/// `todo.list` 结果(items + 当前项 + 计数;service 面不再注入旧信封的
/// `timeis`/`status` 键——那是工具信封的遗留,不属于查询结果契约)。
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct TodoListSnapshot {
    pub items: Vec<TodoItemSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_id: Option<String>,
    pub counts: TodoCountsSnapshot,
}
