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

use super::actions::{parse_edit_field, parse_status};
use super::model::{TODO_LOCK, TodoItem, TodoStatus};
use super::parse::{alloc_id, expand_todo_ids, parse_todo_id, parse_write_items};
use super::store::{
    count_status, normalize_current_id, read_store_for, status_name, write_store_for,
};

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
/// List session tasks.
pub struct TodoListArgs {
    /// Optional status filter (pending | in_progress | completed | cancelled).
    #[serde(default)]
    pub status: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct TodoCounts {
    pub pending: usize,
    pub in_progress: usize,
    pub completed: usize,
    pub cancelled: usize,
    pub total: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
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
/// One task entry of the full-replace list.
pub struct TodoWriteItemArgs {
    /// Existing T<n> to keep/update this task; omit to assign a new one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    /// Task title (1-100 chars). Required for new items; optional when `id` references an existing task (the previous title is kept).
    /// 省略 = 沿用同 `id` 既有条目的标题（仅既有 id 成立；新条目必填）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Exactly one item should be in_progress while working (pending | in_progress | completed | cancelled).
    pub status: TodoStatusView,
    /// Optional context (<=200 chars).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Completion evidence (for completed items).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
/// Replace the whole task list (full-replace). Each item needs status; keep every
/// prior item you want to keep; exactly one in_progress.
pub struct TodoWriteArgs {
    /// The FULL task list — replaces the previous list entirely (max 20 items).
    pub items: Vec<TodoWriteItemArgs>,
    /// Optional one-liner on why the plan changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explanation: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
/// Set one task's status (one task per call — loop for batches).
pub struct TodoUpdateArgs {
    /// Target ID (e.g. T1).
    pub id: Value,
    /// Target status (pending | in_progress | completed | cancelled).
    pub status: TodoStatusView,
    /// Completion summary (required when completed).
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
    let filter = args
        .status
        .as_deref()
        .filter(|value| !value.is_empty())
        .map(|value| {
            parse_status(value).ok_or_else(|| {
                crate::json_err_string(
                    "invalid_input",
                    format!("unknown status: {value}"),
                    "Use pending, in_progress, completed, or cancelled.",
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
    let mut store = read_store_for(session_id)?;
    let incoming = parse_write_items(args)?;

    // ID 解析三态：显式引用**已存在**的 id → 原样保留；缺省 → next_id 高水位
    // 新分配；显式引用**未知** id → 按新建处理（分配新号）并把 remap 写进回执。
    //
    // 未知 id 早先是硬错误（"防模型幻觉产生孤儿号"），但实测这是最高频的失败：
    // 模型把「想新建」写成「引用一个不存在的号」，整次覆写被拒、计划原地踏步。
    // 现在降级为可恢复的 remap——孤儿号的风险由「一律新分配」消掉，模型从回执
    // 的 assigned 列表（及 message 里列出的 remap 记录）就能拿到真实 ID。同一次覆写内重复引用仍然拒绝
    // （那是模型自己前后矛盾，静默去重只会掩盖问题）。
    let mut next_items: Vec<TodoItem> = Vec::with_capacity(incoming.len());
    let mut assigned: Vec<String> = Vec::new();
    let mut remapped: Vec<String> = Vec::new();
    let mut seen_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (index, parsed) in incoming.into_iter().enumerate() {
        let known = parsed
            .id
            .as_ref()
            .is_some_and(|id| store.items.iter().any(|item| item.id == *id));
        let id = match parsed.id {
            Some(id) => {
                if !seen_ids.insert(id.clone()) {
                    return Err(crate::json_err_string(
                        "invalid_input",
                        format!("items[{index}] duplicates id {id}"),
                        "Each id may appear at most once per write.",
                    ));
                }
                if known {
                    id
                } else {
                    let fresh = alloc_id(&mut store);
                    remapped.push(format!("{id}->{fresh}"));
                    assigned.push(fresh.clone());
                    fresh
                }
            }
            None => {
                let id = alloc_id(&mut store);
                assigned.push(id.clone());
                id
            }
        };
        // 标题继承只对**既有**条目成立：新条目没标题就是没标题，不能凭空捏造。
        let title = match parsed.title {
            Some(title) => title,
            None => store
                .items
                .iter()
                .find(|item| item.id == id)
                .map(|item| item.title.clone())
                .ok_or_else(|| {
                    crate::json_err_string(
                        "invalid_input",
                        format!("items[{index}].title is required for new items"),
                        "Send a title for new tasks; omit it only when re-referencing an existing id.",
                    )
                })?,
        };
        next_items.push(TodoItem {
            id,
            title,
            description: parsed.description,
            status: parsed.status,
            evidence: parsed.evidence,
        });
    }

    let replaced = store.items.len();
    store.items = next_items;
    normalize_current_id(&mut store);
    write_store_for(session_id, &store)?;

    let current_id = store
        .items
        .iter()
        .find(|item| item.status == TodoStatus::InProgress)
        .map(|item| item.id.clone());
    let remap_note = if remapped.is_empty() {
        String::new()
    } else {
        format!(" Unknown ids reassigned: {}.", remapped.join(", "))
    };
    Ok(TodoWriteOutput {
        replaced,
        total: store.items.len(),
        assigned: assigned.clone(),
        current_id,
        message: format!(
            "Plan updated: {} item(s) ({} new).{remap_note}",
            store.items.len(),
            assigned.len()
        ),
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
    let mut store = read_store_for(session_id)?;

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
                "invalid_input",
                "updates must not be empty",
                "Provide at least one {id, status} entry.",
            ));
        }
        let mut list = Vec::new();
        for update in updates {
            let id = parse_todo_id(update.get("id")).ok_or_else(|| {
                crate::json_err_string(
                    "invalid_input",
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
                        "invalid_input",
                        format!("unknown status: {requested}"),
                        "Use pending, in_progress, completed, or cancelled.",
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
                    "invalid_input",
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
                "invalid_input",
                format!("unknown status: {requested}"),
                "Use pending, in_progress, completed, or cancelled.",
            )
        })?;
        let mut list = Vec::new();
        for expr in ids.as_array().ok_or_else(|| {
            crate::json_err_string(
                "invalid_input",
                "ids must be an array of strings",
                "Use ids: [\"T1\", \"T1-T3\"]",
            )
        })? {
            let expr = expr.as_str().ok_or_else(|| {
                crate::json_err_string(
                    "invalid_input",
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
                "invalid_input",
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
                "invalid_input",
                format!("unknown status: {requested}"),
                "Use pending, in_progress, completed, or cancelled.",
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
                "invalid_input",
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
            "not_found",
            format!("no matching todos: {}", not_found.join(", ")),
            "Use todo(action=\"list\") to inspect IDs.",
        ));
    }

    normalize_current_id(&mut store);
    write_store_for(session_id, &store)?;

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
            "Replace the whole task list (full-replace). Each item needs status; keep ids to preserve items; exactly one in_progress. `title` may be omitted when `id` references an existing task.",
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

pub struct TodoListTool;

impl TypedTool for TodoListTool {
    type Args = TodoListArgs;
    type Output = TodoListOutput;

    fn meta(&self) -> ToolMeta {
        ToolMeta::new(
            "todo_list",
            "List session tasks; optional status filter. Read-only (allowed in plan mode).",
            ToolCategory::Read,
            ToolRisk::ReadOnly,
            Duration::from_secs(15),
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
