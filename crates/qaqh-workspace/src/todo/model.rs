//! todo::model — 数据模型（TodoItem/TodoStatus/TodoMode/TodoStore + TODO_LOCK）。

use serde::{Deserialize, Serialize};

use std::sync::Mutex;

// ═══════════════════════════════════════════════════════
// Data model
// ═══════════════════════════════════════════════════════

pub(crate) static TODO_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TodoItem {
    /// 身份 = 标题（纯文本范式）。保留字段以兼容 GoalEngine / 前端
    /// dashboard 的 id 匹配；赋值恒等于 `title`。
    pub id: String,
    pub title: String,
    pub description: String,
    #[serde(default = "default_status")]
    pub status: TodoStatus,
    /// Completion evidence (filled when status=completed).
    #[serde(default)]
    pub evidence: Option<String>,
}

fn default_status() -> TodoStatus {
    TodoStatus::Pending
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    #[serde(rename = "in_progress")]
    InProgress,
    Completed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum TodoMode {
    #[default]
    Manual,
    Goal,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TodoStore {
    pub items: Vec<TodoItem>,
    #[serde(default)]
    pub mode: TodoMode,
    #[serde(default)]
    pub current_id: Option<String>,
    #[serde(default)]
    pub auto_turns: u32,
    #[serde(default = "default_max_auto")]
    pub max_auto_turns: u32,
    /// 列表归档位：全部条目进入终态（completed/cancelled）后置位。
    /// 归档后下一次非 todo 工具调用回**一次** cleared 提示（由
    /// `cleared_notified` 保证只发一次），随后注入静默直至重建。
    /// 旧 todo.json 的 `next_id` 字段：serde 忽略未知字段，无需保留。
    #[serde(default)]
    pub archived: bool,
    /// cleared 提示已随某个 tool_result 发出（append-only 契约：提示只发
    /// 一次，之后的快照返回 None）。
    #[serde(default)]
    pub cleared_notified: bool,
}

fn default_max_auto() -> u32 {
    24
}
