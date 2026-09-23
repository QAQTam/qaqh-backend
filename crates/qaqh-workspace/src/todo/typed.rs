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
    OutputBudget, ToolBody, ToolContentBlock, ToolDescriptor, ToolDisplay, ToolError,
    ToolErrorKind, ToolExecutionError, ToolExposure, ToolHeader, ToolName, ToolProjection,
    ToolSource, TypedTool,
};

use super::actions::{parse_edit_field, parse_status};
use super::model::{TODO_LOCK, TodoItem, TodoStatus};
use super::parse::{alloc_id, expand_todo_ids, parse_todo_id, parse_write_items};
use super::store::{
    count_status, normalize_current_id, read_store_for, status_name, write_store_for,
};

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
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
    #[serde(alias = "pending")]
    Idle,
    InProgress,
    #[serde(alias = "complete")]
    Completed,
    #[serde(alias = "canceled")]
    Cancelled,
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
pub struct TodoWriteItemArgs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    pub title: String,
    pub status: TodoStatusView,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TodoWriteArgs {
    pub items: Vec<TodoWriteItemArgs>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explanation: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TodoUpdateArgs {
    pub id: Value,
    pub status: TodoStatusView,
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
    pub replaced: usize,
    pub total: usize,
    pub assigned: Vec<String>,
    pub current_id: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TodoUpdated {
    pub id: String,
    pub status: TodoStatusView,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TodoUpdateOutput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<TodoItemView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated: Option<Vec<TodoUpdated>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_found: Option<Vec<String>>,
    pub message: String,
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

    fn summary(&self) -> Option<String> {
        Some(self.summary_text())
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

    fn summary(&self) -> Option<String> {
        Some(self.message.clone())
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

    fn summary(&self) -> Option<String> {
        Some(self.message.clone())
    }

    fn display(&self, _args: &Value) -> ToolDisplay {
        todo_display(self.message.clone())
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

/// `todo_write` 的 canonical mutation 路径。
pub fn todo_write_for_typed(seed: &str, args: &Value) -> Result<TodoWriteOutput, String> {
    let _guard = TODO_LOCK
        .lock()
        .map_err(|_| "todo lock poisoned".to_string())?;
    let mut store = read_store_for(seed)?;
    let incoming = parse_write_items(args)?;

    // ID 解析三态：显式引用（必须已存在）→ 原样保留；缺省 → next_id 高水位
    // 新分配。显式引用未知 ID 是硬错误（覆写语义下引用不存在的 ID 只能是
    // 模型幻觉，放行会静默产生永久孤儿号）；同一次覆写内重复引用也拒绝。
    let mut next_items: Vec<TodoItem> = Vec::with_capacity(incoming.len());
    let mut assigned: Vec<String> = Vec::new();
    let mut seen_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (index, parsed) in incoming.into_iter().enumerate() {
        let id = match parsed.id {
            Some(id) => {
                if !seen_ids.insert(id.clone()) {
                    return Err(crate::json_err_string(
                        "INVALID_INPUT",
                        format!("items[{index}] duplicates id {id}"),
                        "Each id may appear at most once per write.",
                    ));
                }
                if store.items.iter().any(|item| item.id == id) {
                    id
                } else {
                    return Err(crate::json_err_string(
                        "NOT_FOUND",
                        format!("items[{index}] references unknown id {id}"),
                        "Omit \"id\" to assign a new one, or use todo_list to inspect existing IDs.",
                    ));
                }
            }
            None => {
                let id = alloc_id(&mut store);
                assigned.push(id.clone());
                id
            }
        };
        next_items.push(TodoItem {
            id,
            title: parsed.title,
            description: parsed.description,
            status: parsed.status,
            evidence: parsed.evidence,
        });
    }

    let replaced = store.items.len();
    store.items = next_items;
    normalize_current_id(&mut store);
    write_store_for(seed, &store)?;

    let current_id = store
        .items
        .iter()
        .find(|item| item.status == TodoStatus::InProgress)
        .map(|item| item.id.clone());
    Ok(TodoWriteOutput {
        replaced,
        total: store.items.len(),
        assigned: assigned.clone(),
        current_id,
        message: format!(
            "Plan updated: {} item(s) ({} new).",
            store.items.len(),
            assigned.len()
        ),
    })
}

/// `todo_update` / service `todo.set` 的 canonical mutation 路径。
///
/// 保留既有三种输入形态：单条 `{id,status,evidence?}`、`ids` 批量状态、
/// `updates` 并行条目；工具面仍由 `TodoUpdateArgs` 限制为单条。
pub fn todo_update_for_typed(seed: &str, args: &Value) -> Result<TodoUpdateOutput, String> {
    let _guard = TODO_LOCK
        .lock()
        .map_err(|_| "todo lock poisoned".to_string())?;
    let mut store = read_store_for(seed)?;

    /// 一次变更（支持单条 / ids 批量 / updates 并行三种来源）。
    /// status 为 None 表示纯编辑（title/description/evidence）。
    struct PendingSet {
        id: String,
        status: Option<TodoStatus>,
        evidence: Option<String>,
        title: Option<String>,
        description: Option<String>,
    }

    let pending: Vec<PendingSet> = if let Some(updates) =
        args.get("updates").and_then(Value::as_array)
    {
        if updates.is_empty() {
            return Err(crate::json_err_string(
                "INVALID_INPUT",
                "updates must not be empty",
                "Provide at least one {id, status} entry.",
            ));
        }
        let mut list = Vec::new();
        for update in updates {
            let id = parse_todo_id(update.get("id")).ok_or_else(|| {
                crate::json_err_string(
                    "INVALID_INPUT",
                    "updates[].id missing or invalid",
                    "Provide the assigned ID, e.g. T1.",
                )
            })?;
            let requested = update
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or_default();
            // 纯编辑（title/description）时 status 可省略；提供则必须合法。
            let status = if requested.is_empty() {
                None
            } else {
                Some(parse_status(requested).ok_or_else(|| {
                    crate::json_err_string(
                        "INVALID_INPUT",
                        format!("unknown status: {requested}"),
                        "Use idle, in_progress, completed, or cancelled.",
                    )
                })?)
            };
            let evidence = update
                .get("evidence")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string);
            let title = parse_edit_field(update.get("title"), "updates[].title", 100, false)?;
            let description = parse_edit_field(
                update.get("description"),
                "updates[].description",
                200,
                true,
            )?;
            if status.is_none() && title.is_none() && description.is_none() && evidence.is_none() {
                return Err(crate::json_err_string(
                    "INVALID_INPUT",
                    format!("updates[] entry for {id} changes nothing"),
                    "Provide status, evidence, title, or description.",
                ));
            }
            list.push(PendingSet {
                id,
                status,
                evidence,
                title,
                description,
            });
        }
        list
    } else if let Some(ids) = args.get("ids") {
        let requested = args
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let status = parse_status(requested).ok_or_else(|| {
            crate::json_err_string(
                "INVALID_INPUT",
                format!("unknown status: {requested}"),
                "Use idle, in_progress, completed, or cancelled.",
            )
        })?;
        let mut list = Vec::new();
        for expr in ids.as_array().ok_or_else(|| {
            crate::json_err_string(
                "INVALID_INPUT",
                "ids must be an array of strings",
                "Use ids: [\"T1\", \"T1-T3\"]",
            )
        })? {
            let expr = expr.as_str().ok_or_else(|| {
                crate::json_err_string(
                    "INVALID_INPUT",
                    "ids entries must be strings",
                    "Use ids: [\"T1\", \"T1-T3\"]",
                )
            })?;
            for id in expand_todo_ids(expr)? {
                list.push(PendingSet {
                    id,
                    status: Some(status.clone()),
                    evidence: None,
                    title: None,
                    description: None,
                });
            }
        }
        list
    } else {
        let id = parse_todo_id(args.get("id")).ok_or_else(|| {
            crate::json_err_string(
                "INVALID_INPUT",
                "missing or invalid id",
                "Provide the assigned ID, e.g. T1.",
            )
        })?;
        let requested = args
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let status = parse_status(requested).ok_or_else(|| {
            crate::json_err_string(
                "INVALID_INPUT",
                format!("unknown status: {requested}"),
                "Use idle, in_progress, completed, or cancelled.",
            )
        })?;
        let evidence = args
            .get("evidence")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        if args.get("evidence").is_some() && evidence.is_none() {
            return Err(crate::json_err_string(
                "INVALID_INPUT",
                "evidence must be a non-empty string when provided",
                "Omit evidence unless there is a concrete result summary.",
            ));
        }
        vec![PendingSet {
            id,
            status: Some(status),
            evidence,
            title: None,
            description: None,
        }]
    };

    // 宽松应用：未知 ID 记入 not_found，不中断其余更新。
    let mut updated: Vec<TodoUpdated> = Vec::new();
    let mut not_found: Vec<String> = Vec::new();
    let mut last_updated_idx: Option<usize> = None;
    for pending in &pending {
        if let Some(idx) = store.items.iter().position(|item| item.id == pending.id) {
            if let Some(status) = &pending.status {
                store.items[idx].status = status.clone();
            }
            if pending.evidence.is_some() {
                store.items[idx].evidence = pending.evidence.clone();
            }
            if let Some(title) = &pending.title {
                store.items[idx].title = title.clone();
            }
            if let Some(description) = &pending.description {
                store.items[idx].description = description.clone();
            }
            updated.push(TodoUpdated {
                id: pending.id.clone(),
                status: TodoStatusView::from(&store.items[idx].status),
            });
            last_updated_idx = Some(idx);
        } else {
            not_found.push(pending.id.clone());
        }
    }

    if updated.is_empty() {
        return Err(crate::json_err_string(
            "NOT_FOUND",
            format!("no matching todos: {}", not_found.join(", ")),
            "Use todo(action=\"list\") to inspect IDs.",
        ));
    }

    normalize_current_id(&mut store);
    write_store_for(seed, &store)?;

    if pending.len() == 1 && updated.len() == 1 {
        // 单条路径保持兼容返回（V1 客户端/前端依赖 item + message）。
        let item = TodoItemView::from(&store.items[last_updated_idx.expect("single update")]);
        let id = &pending[0].id;
        let message = match &pending[0].status {
            Some(status) => format!("Todo {id} is now {}.", status_name(status)),
            None => format!("Todo {id} updated."),
        };
        Ok(TodoUpdateOutput {
            item: Some(item),
            updated: None,
            not_found: None,
            message,
        })
    } else {
        let updated_count = updated.len();
        Ok(TodoUpdateOutput {
            item: None,
            updated: Some(updated),
            not_found: Some(not_found),
            message: format!("Updated {updated_count} todo(s)."),
        })
    }
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

fn recoverable(error: String) -> ToolExecutionError {
    ToolExecutionError::Recoverable(legacy_error_to_tool_error(error))
}

fn legacy_error_to_tool_error(raw: String) -> ToolError {
    let parsed = serde_json::from_str::<Value>(&raw).unwrap_or(Value::Null);
    let code = parsed
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or("TOOL_ERROR");
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
        "INVALID_INPUT" | "INVALID_ARGS" | "INVALID_ARGUMENTS" => ToolErrorKind::InvalidArguments,
        "NOT_FOUND" => ToolErrorKind::NotFound,
        "PERMISSION_DENIED" | "PERMISSION_REQUIRED" | "BLOCKED_BY_MODE" => {
            ToolErrorKind::PermissionDenied
        }
        "CANCELLED" => ToolErrorKind::Cancelled,
        "TIMEOUT" => ToolErrorKind::Timeout,
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

fn descriptor(
    name: &str,
    description: &str,
    input_schema: Value,
    output_schema: Value,
    category: ToolCategory,
    risk: ToolRisk,
) -> ToolDescriptor {
    ToolDescriptor {
        name: ToolName::new(name).expect("valid todo tool name"),
        display_name: None,
        description: description.to_string(),
        input_schema,
        output_schema,
        category,
        risk,
        default_timeout: Duration::from_secs(15),
        exposure: ToolExposure::Direct,
        source: ToolSource::Builtin,
        output_budget: OutputBudget::default(),
        capabilities: crate::tool_capabilities::builtin_capabilities(name).unwrap_or_default(),
    }
}

pub struct TodoWriteTool;

impl TypedTool for TodoWriteTool {
    type Args = TodoWriteArgs;
    type Output = TodoWriteOutput;

    fn descriptor(&self) -> ToolDescriptor {
        descriptor(
            "todo_write",
            "Replace the whole task list (full-replace). Each item needs title+status; keep ids to preserve items; exactly one in_progress.",
            super::split::todo_write_schema(),
            serde_json::to_value(schemars::schema_for!(TodoWriteOutput))
                .expect("todo_write output schema"),
            ToolCategory::Write,
            ToolRisk::Write,
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

    fn descriptor(&self) -> ToolDescriptor {
        descriptor(
            "todo_update",
            "Set one task's status: {id, status, evidence?}. One task per call — loop for batches.",
            super::split::todo_update_schema(),
            serde_json::to_value(schemars::schema_for!(TodoUpdateOutput))
                .expect("todo_update output schema"),
            ToolCategory::Write,
            ToolRisk::Write,
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

pub struct TodoListTool;

impl TypedTool for TodoListTool {
    type Args = TodoListArgs;
    type Output = TodoListOutput;

    fn descriptor(&self) -> ToolDescriptor {
        descriptor(
            "todo_list",
            "List session tasks; optional status filter. Read-only (allowed in plan mode).",
            super::split::todo_list_schema(),
            serde_json::to_value(schemars::schema_for!(TodoListOutput))
                .expect("todo_list output schema"),
            ToolCategory::Read,
            ToolRisk::ReadOnly,
        )
    }

    fn run(
        &self,
        ctx: &crate::tool_api::ToolCallContext,
        args: Self::Args,
    ) -> Result<Self::Output, ToolExecutionError> {
        let args = to_args_value(&args)?;
        todo_list_for_typed(&ctx.session_id, &args).map_err(recoverable)
    }
}
