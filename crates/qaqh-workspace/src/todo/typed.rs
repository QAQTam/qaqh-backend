//! todo typed output 试点。
//!
//! 本模块是 `todo_write` / `todo_update` / `todo_list` 的 canonical 输出结构。
//! legacy JSON wire、HTTP service 与 timeline display 都从这里派生；生产
//! 执行经 [`TypedToolAdapter`](crate::tool_api::TypedToolAdapter) 进入
//! [`ErasedTool`](crate::tool_api::ErasedTool)。

use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ToolRisk;
use crate::permission::ToolCategory;
use crate::tool_api::{
    ToolBody, ToolContentBlock, ToolDisplay, ToolError, ToolErrorKind, ToolExecutionError,
    ToolHeader, ToolMeta, ToolProjection, TypedTool,
};

use super::model::{TODO_LOCK, TodoItem, TodoStatus};
use super::parse::{parse_todo_text, render_todo_text};
use super::store::{
    count_status, normalize_current_id, read_store_for, status_name, write_store_for,
};

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
/// List session tasks.
pub struct TodoListArgs {
    /// pending | in_progress | completed | cancelled.
    #[serde(default)]
    pub status: Option<TodoStatusView>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct TodoCounts {
    pub pending: usize,
    pub in_progress: usize,
    pub completed: usize,
    pub cancelled: usize,
    pub total: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(rename_all = "snake_case")]
pub enum TodoStatusView {
    // 变体名必须与 wire 契约一致（pending|in_progress|completed|cancelled）：
    // typed 桥经 to_args_value 把 Args **再序列化**回 Value 交给解析层，
    // 变体名若叫 Idle 会被 rename_all 写成 "idle"，parse_status 不认——
    // 实测 todo_write "pending" 进 → "idle" 出 → INVALID_INPUT（G5 补跑
    // todo_contract 红）。"pending" 别名只救反序列化，救不了序列化。
    Pending,
    InProgress,
    #[serde(alias = "complete")]
    Completed,
    #[serde(alias = "canceled")]
    Cancelled,
}

impl From<&TodoStatus> for TodoStatusView {
    fn from(status: &TodoStatus) -> Self {
        match status {
            TodoStatus::Pending => Self::Pending,
            TodoStatus::InProgress => Self::InProgress,
            TodoStatus::Completed => Self::Completed,
            TodoStatus::Cancelled => Self::Cancelled,
        }
    }
}

impl From<TodoStatusView> for TodoStatus {
    fn from(view: TodoStatusView) -> Self {
        match view {
            TodoStatusView::Pending => Self::Pending,
            TodoStatusView::InProgress => Self::InProgress,
            TodoStatusView::Completed => Self::Completed,
            TodoStatusView::Cancelled => Self::Cancelled,
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
#[serde(deny_unknown_fields)]
/// Replace the whole task list with one plain-text block (one line per todo).
/// Line format: `- [ ]|~|x|- title [| description [| evidence]]` —
/// markers: `[ ]` pending, `[~]` in_progress (parallel allowed), `[x]` done,
/// `[-]` cancelled. Titles are identities: duplicates rejected; renames are
/// delete + create.
pub struct TodoWriteArgs {
    /// The full todo list, plain text. Empty string clears the list.
    pub list: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
/// Set one task's status by its exact title (loop for batches).
pub struct TodoUpdateArgs {
    /// Exact title of the target task.
    pub title: String,
    /// pending | in_progress | completed | cancelled.
    pub status: TodoStatusView,
    /// Required when completed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TodoListOutput {
    pub items: Vec<TodoItemView>,
    pub current_id: Option<String>,
    pub counts: TodoCounts,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TodoWriteOutput {
    pub total: usize,
    pub in_progress: usize,
    pub message: String,
    /// 解析后的清单视图（模型据此校验 roundtrip；文本范式下模型刚写过
    /// 全量，receipt 只需紧凑核对面）。
    #[serde(default)]
    pub items: Vec<TodoItemView>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TodoUpdateOutput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<TodoItemView>,
    pub message: String,
    /// 全量快照：update 后回传完整列表状态（模型据此取下一步）。
    #[serde(default)]
    pub items: Vec<TodoItemView>,
}

impl TodoListOutput {
    pub fn summary_text(&self) -> String {
        format!(
            "{} task(s) · {} in progress",
            self.counts.total, self.counts.in_progress
        )
    }

    /// service/wire 共用的成功信封（保留 timeis/status 字段）。
    pub fn to_envelope_value(&self) -> Result<Value, String> {
        envelope_value(self, "todo.list")
    }

    pub fn to_envelope_string(&self) -> Result<String, String> {
        serde_json::to_string(&self.to_envelope_value()?)
            .map_err(|error| format!("todo.list: {error}"))
    }
}

impl TodoWriteOutput {
    pub fn to_envelope_value(&self) -> Result<Value, String> {
        envelope_value(self, "todo.write")
    }

    pub fn to_envelope_string(&self) -> Result<String, String> {
        serde_json::to_string(&self.to_envelope_value()?)
            .map_err(|error| format!("todo.write: {error}"))
    }
}

impl TodoUpdateOutput {
    pub fn to_envelope_value(&self) -> Result<Value, String> {
        envelope_value(self, "todo.update")
    }

    pub fn to_envelope_string(&self) -> Result<String, String> {
        serde_json::to_string(&self.to_envelope_value()?)
            .map_err(|error| format!("todo.update: {error}"))
    }
}

fn envelope_value<T: Serialize>(output: &T, label: &str) -> Result<Value, String> {
    let mut value = serde_json::to_value(output).map_err(|error| format!("{label}: {error}"))?;
    if let Some(object) = value.as_object_mut() {
        object.insert("timeis".into(), Value::String(crate::now_utc8()));
        object.insert("status".into(), Value::String("ok".into()));
    }
    Ok(value)
}

fn todo_display(summary: impl Into<String>) -> ToolDisplay {
    ToolDisplay::new(
        ToolHeader::Other {
            label: "todo".to_string(),
        },
        ToolBody::None,
    )
    .with_summary(summary)
}

impl ToolProjection for TodoListOutput {
    fn model_blocks(&self) -> Vec<ToolContentBlock> {
        vec![ToolContentBlock::Text {
            text: self.to_envelope_string().unwrap_or_default(),
        }]
    }

    fn display(&self, _args: &Value) -> ToolDisplay {
        todo_display(self.summary_text())
    }
}

impl ToolProjection for TodoWriteOutput {
    fn model_blocks(&self) -> Vec<ToolContentBlock> {
        vec![ToolContentBlock::Text {
            text: self.to_envelope_string().unwrap_or_default(),
        }]
    }

    fn display(&self, _args: &Value) -> ToolDisplay {
        todo_display(self.message.clone())
    }
}

impl ToolProjection for TodoUpdateOutput {
    fn model_blocks(&self) -> Vec<ToolContentBlock> {
        vec![ToolContentBlock::Text {
            text: self.to_envelope_string().unwrap_or_default(),
        }]
    }

    fn display(&self, _args: &Value) -> ToolDisplay {
        todo_display(self.message.clone())
    }
}

pub fn todo_list_for_typed(session_id: &str, args: &Value) -> Result<TodoListOutput, String> {
    let args: TodoListArgs = serde_json::from_value(args.clone()).map_err(|error| {
        crate::json_err_string(
            "invalid_input",
            format!("invalid todo_list args: {error}"),
            "Use {\"status\": \"pending|in_progress|completed|cancelled\"} or omit it.",
        )
    })?;
    let store = read_store_for(session_id)?;
    // v2 类型生成后 filter 已是 typed enum（schema enum 由类型给出），
    // 不再走字符串 parse_status。
    let filter = args.status;
    let items: Vec<TodoItemView> = store
        .items
        .iter()
        .filter(|item| {
            filter
                .as_ref()
                .is_none_or(|status| TodoStatusView::from(&item.status) == *status)
        })
        .map(TodoItemView::from)
        .collect();
    let counts = TodoCounts {
        pending: count_status(&store, TodoStatus::Pending),
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

/// `todo_write` 的 canonical mutation 路径。
pub fn todo_write_for_typed(session_id: &str, args: &Value) -> Result<TodoWriteOutput, String> {
    let _guard = TODO_LOCK
        .lock()
        .map_err(|_| "todo lock poisoned".to_string())?;
    let args: TodoWriteArgs = serde_json::from_value(args.clone()).map_err(|error| {
        crate::json_err_string(
            "invalid_input",
            format!("invalid todo_write args: {error}"),
            "Provide the full list as plain text in `list`.",
        )
    })?;
    let mut store = read_store_for(session_id)?;
    let replaced = store.items.len();
    store.items = parse_todo_text(&args.list)?;
    // 重建即解除归档与 cleared 通知位；随后按新列表状态重新判定归档位。
    store.archived = false;
    store.cleared_notified = false;
    super::store::refresh_archive_flag(&mut store);
    normalize_current_id(&mut store);
    write_store_for(session_id, &store)?;

    let in_progress = count_status(&store, TodoStatus::InProgress);
    let rebuild_note = if replaced > 0 {
        format!(" Replaced {replaced} previous item(s).")
    } else {
        String::new()
    };
    Ok(TodoWriteOutput {
        total: store.items.len(),
        in_progress,
        message: format!(
            "Plan updated: {} item(s), {} in progress.{rebuild_note}",
            store.items.len(),
            in_progress
        ),
        items: store.items.iter().map(TodoItemView::from).collect(),
    })
}

/// `todo_update` / service `todo.set` 的 canonical mutation 路径。
///
/// 保留既有三种输入形态：单条 `{id,status,evidence?}`、`ids` 批量状态、
/// `updates` 并行条目；工具面仍由 `TodoUpdateArgs` 限制为单条。
pub fn todo_update_for_typed(session_id: &str, args: &Value) -> Result<TodoUpdateOutput, String> {
    let _guard = TODO_LOCK
        .lock()
        .map_err(|_| "todo lock poisoned".to_string())?;
    let args: TodoUpdateArgs = serde_json::from_value(args.clone()).map_err(|error| {
        crate::json_err_string(
            "invalid_input",
            format!("invalid todo_update args: {error}"),
            "Use {\"title\", \"status\", \"evidence?\"}.",
        )
    })?;
    let mut store = read_store_for(session_id)?;
    let title = args.title.trim();
    if title.is_empty() {
        return Err(crate::json_err_string(
            "invalid_input",
            "title is required",
            "Titles are identities — use the exact title from the list.",
        ));
    }
    let Some(index) = store.items.iter().position(|item| item.title == title) else {
        let available = render_todo_text(&store);
        return Err(crate::json_err_string(
            "not_found",
            format!("no todo titled `{title}`"),
            &format!("Exact titles only. Current list:\n{available}"),
        ));
    };
    store.items[index].status = TodoStatus::from(args.status);
    if let Some(evidence) = args
        .evidence
        .as_deref()
        .map(str::trim)
        .filter(|evidence| !evidence.is_empty())
    {
        store.items[index].evidence = Some(evidence.to_string());
    }
    normalize_current_id(&mut store);
    super::store::refresh_archive_flag(&mut store);
    write_store_for(session_id, &store)?;

    let status_name = status_name(&store.items[index].status);
    Ok(TodoUpdateOutput {
        item: Some(TodoItemView::from(&store.items[index])),
        message: format!("Todo `{title}` is now {status_name}."),
        items: store.items.iter().map(TodoItemView::from).collect(),
    })
}

/// service 面直接返回 canonical Value，避免 `parse_json_string`。
pub fn todo_list_value_for(session_id: &str, args: &Value) -> Result<Value, String> {
    // service 信封把会话键与业务参数放在同一对象；typed args 只接受 status，
    // 且 `TodoListArgs` 开了 `deny_unknown_fields`——会话键必须剥掉，
    // 否则 `todo.list` 会以 `invalid todo_list args` 失败。
    let mut tool_args = args.clone();
    if let Some(object) = tool_args.as_object_mut() {
        object.remove("session_id");
    }
    todo_list_for_typed(session_id, &tool_args)?.to_envelope_value()
}

fn recoverable(error: String) -> ToolExecutionError {
    ToolExecutionError::Recoverable(legacy_error_to_tool_error(error))
}

fn legacy_error_to_tool_error(raw: String) -> ToolError {
    let parsed = serde_json::from_str::<Value>(&raw).unwrap_or(Value::Null);
    let code = parsed
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or("tool_error");
    let message = parsed
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or(&raw)
        .to_string();
    let hint = parsed
        .get("hint")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let kind = match code {
        "invalid_input" | "invalid_args" | "invalid_arguments" => ToolErrorKind::InvalidArguments,
        "not_found" => ToolErrorKind::NotFound,
        "permission_denied" | "permission_required" | "blocked_by_mode" => {
            ToolErrorKind::PermissionDenied
        }
        "cancelled" => ToolErrorKind::Cancelled,
        "timeout" => ToolErrorKind::Timeout,
        _ => ToolErrorKind::Execution,
    };
    let mut error = ToolError::new(kind, message);
    if let Some(hint) = hint {
        error = error.with_hint(hint);
    }
    error
}

#[allow(clippy::result_large_err)] // TypedTool's frozen public error boundary.
fn to_args_value<T: Serialize>(args: &T) -> Result<Value, ToolExecutionError> {
    serde_json::to_value(args).map_err(|error| {
        ToolExecutionError::Recoverable(ToolError::invalid_arguments(format!(
            "serialize typed args: {error}"
        )))
    })
}

pub struct TodoWriteTool;

impl TypedTool for TodoWriteTool {
    type Args = TodoWriteArgs;
    type Output = TodoWriteOutput;

    fn meta(&self) -> ToolMeta {
        ToolMeta::new(
            "todo_write",
            "Replace the whole task list (full-replace) with one plain-text block. One line per todo: `- [ ]|~|x|- title [| description [| evidence]]` (markers: [ ] pending, [~] in_progress — parallel allowed, [x] completed, [-] cancelled). Titles are identities: duplicates rejected, renames are delete+create. Empty list clears the plan.",
            ToolCategory::Write,
            ToolRisk::Write,
            Duration::from_secs(15),
        )
    }

    fn run(
        &self,
        ctx: &crate::tool_api::ToolCallContext,
        args: Self::Args,
    ) -> Result<Self::Output, ToolExecutionError> {
        let args = to_args_value(&args)?;
        todo_write_for_typed(&ctx.session_id, &args).map_err(recoverable)
    }
}

pub struct TodoUpdateTool;

impl TypedTool for TodoUpdateTool {
    type Args = TodoUpdateArgs;
    type Output = TodoUpdateOutput;

    fn meta(&self) -> ToolMeta {
        ToolMeta::new(
            "todo_update",
            "Set one task's status: {id, status, evidence?}. One task per call — loop for batches.",
            ToolCategory::Write,
            ToolRisk::Write,
            Duration::from_secs(15),
        )
    }

    fn run(
        &self,
        ctx: &crate::tool_api::ToolCallContext,
        args: Self::Args,
    ) -> Result<Self::Output, ToolExecutionError> {
        let args = to_args_value(&args)?;
        todo_update_for_typed(&ctx.session_id, &args).map_err(recoverable)
    }
}

// `TodoListTool` 已从模型工具面退役（双工具收敛）：只读列表由
// `todo_list_for` / `todo_list_value_for` 继续服务 HTTP service 与 CLI。
