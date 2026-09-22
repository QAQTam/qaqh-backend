//! todo_list 的 typed output 试点。
//!
//! 本模块是 `todo_list` 的 canonical 输出结构；legacy JSON wire、HTTP
//! service 与 timeline display 都从这里派生。`todo_write` / `todo_update`
//! 仍走既有 legacy 路径，待后续切片迁移。

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::tool_api::{ToolBody, ToolContentBlock, ToolDisplay, ToolHeader, ToolProjection};

use super::actions::parse_status;
use super::model::{TodoItem, TodoStatus};
use super::store::{count_status, read_store_for};

#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TodoListArgs {
    #[serde(default)]
    pub status: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct TodoCounts {
    pub idle: usize,
    pub in_progress: usize,
    pub completed: usize,
    pub cancelled: usize,
    pub total: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(rename_all = "snake_case")]
pub enum TodoStatusView {
    Idle,
    InProgress,
    Completed,
    Cancelled,
}

impl TodoStatusView {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
        }
    }
}

impl From<&TodoStatus> for TodoStatusView {
    fn from(status: &TodoStatus) -> Self {
        match status {
            TodoStatus::Pending => Self::Idle,
            TodoStatus::InProgress => Self::InProgress,
            TodoStatus::Completed => Self::Completed,
            TodoStatus::Cancelled => Self::Cancelled,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TodoItemView {
    pub id: String,
    pub title: String,
    pub description: String,
    pub status: TodoStatusView,
    pub evidence: Option<String>,
}

impl From<&TodoItem> for TodoItemView {
    fn from(item: &TodoItem) -> Self {
        Self {
            id: item.id.clone(),
            title: item.title.clone(),
            description: item.description.clone(),
            status: TodoStatusView::from(&item.status),
            evidence: item.evidence.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TodoListOutput {
    pub items: Vec<TodoItemView>,
    pub current_id: Option<String>,
    pub counts: TodoCounts,
}

impl TodoListOutput {
    /// 模型面的人类可读文本；不把 JSON 信封当 summary。
    pub fn model_text(&self) -> String {
        let mut text = format!(
            "{} task(s): {} idle, {} in progress, {} completed, {} cancelled.",
            self.counts.total,
            self.counts.idle,
            self.counts.in_progress,
            self.counts.completed,
            self.counts.cancelled
        );
        for item in &self.items {
            text.push_str(&format!(
                "\n- {} [{}] {}",
                item.id,
                item.status.as_str(),
                item.title
            ));
            if !item.description.is_empty() {
                text.push_str(&format!(" — {}", item.description));
            }
        }
        text
    }

    pub fn summary_text(&self) -> String {
        format!(
            "{} task(s) · {} in progress",
            self.counts.total, self.counts.in_progress
        )
    }

    /// service/wire 共用的成功信封（保留 timeis/status 字段）。
    pub fn to_envelope_value(&self) -> Result<Value, String> {
        let mut value =
            serde_json::to_value(self).map_err(|error| format!("todo.list: {error}"))?;
        if let Some(object) = value.as_object_mut() {
            object.insert("timeis".into(), Value::String(crate::now_utc8()));
            object.insert("status".into(), Value::String("ok".into()));
        }
        Ok(value)
    }

    pub fn to_envelope_string(&self) -> Result<String, String> {
        serde_json::to_string(&self.to_envelope_value()?)
            .map_err(|error| format!("todo.list: {error}"))
    }
}

impl ToolProjection for TodoListOutput {
    fn model_blocks(&self) -> Vec<ToolContentBlock> {
        vec![ToolContentBlock::Text {
            text: self.model_text(),
        }]
    }

    fn summary(&self) -> Option<String> {
        Some(self.summary_text())
    }

    fn display(&self, _args: &Value) -> ToolDisplay {
        ToolDisplay::new(
            ToolHeader::Other {
                label: "todo".to_string(),
            },
            ToolBody::None,
        )
        .with_summary(self.summary_text())
    }
}

pub fn todo_list_for_typed(seed: &str, args: &Value) -> Result<TodoListOutput, String> {
    let args: TodoListArgs = serde_json::from_value(args.clone()).map_err(|error| {
        crate::json_err_string(
            "INVALID_INPUT",
            format!("invalid todo_list args: {error}"),
            "Use {\"status\": \"idle|in_progress|completed|cancelled\"} or omit it.",
        )
    })?;
    let store = read_store_for(seed)?;
    let filter = args
        .status
        .as_deref()
        .filter(|value| !value.is_empty())
        .map(|value| {
            parse_status(value).ok_or_else(|| {
                crate::json_err_string(
                    "INVALID_INPUT",
                    format!("unknown status: {value}"),
                    "Use idle, in_progress, completed, or cancelled.",
                )
            })
        })
        .transpose()?;
    let items: Vec<TodoItemView> = store
        .items
        .iter()
        .filter(|item| filter.as_ref().is_none_or(|status| item.status == *status))
        .map(TodoItemView::from)
        .collect();
    let counts = TodoCounts {
        idle: count_status(&store, TodoStatus::Pending),
        in_progress: count_status(&store, TodoStatus::InProgress),
        completed: count_status(&store, TodoStatus::Completed),
        cancelled: count_status(&store, TodoStatus::Cancelled),
        total: store.items.len(),
    };
    Ok(TodoListOutput {
        items,
        current_id: store.current_id,
        counts,
    })
}

/// service 面直接返回 canonical Value，避免 `parse_json_string`。
pub fn todo_list_value_for(seed: &str, args: &Value) -> Result<Value, String> {
    // service 信封把 seed 与业务参数放在同一对象；typed args 只接受 status。
    let mut tool_args = args.clone();
    if let Some(object) = tool_args.as_object_mut() {
        object.remove("seed");
    }
    todo_list_for_typed(seed, &tool_args)?.to_envelope_value()
}
