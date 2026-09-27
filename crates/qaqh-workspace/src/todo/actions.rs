//! todo::actions — V2 操作（create/set/list + todo_set_for/todo_list_for 直访变体）。

use serde_json::Value;

use crate::{json_err_string, json_ok};

use super::model::{TODO_LOCK, TodoItem, TodoStatus};
use super::parse::{NewTodo, alloc_id, insertion_index, parse_new_todo};
use super::store::{normalize_current_id, read_store, todo_item_json, write_store};

// ═══════════════════════════════════════════════════════
// Todo V2 operations
// ═══════════════════════════════════════════════════════

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn exec_todo_create(args: &Value, positioned: bool) -> Result<String, String> {
    let _guard = TODO_LOCK
        .lock()
        .map_err(|_| "todo lock poisoned".to_string())?;
    let mut store = read_store()?;
    if !positioned && (args.get("after_id").is_some() || args.get("before_id").is_some()) {
        return Err(json_err_string(
            "INVALID_INPUT",
            "create does not accept before_id or after_id",
            "Use action=insert for positioned tasks.",
        ));
    }
    let insertion = if positioned {
        insertion_index(&store, args)?
    } else {
        store.items.len()
    };
    let pending: Vec<NewTodo> = if let Some(items) = args.get("items").and_then(Value::as_array) {
        items
            .iter()
            .enumerate()
            .map(|(index, item)| parse_new_todo(item, &format!("items[{index}]")))
            .collect::<Result<Vec<_>, String>>()?
    } else {
        vec![parse_new_todo(args, "todo")?]
    };

    // IDs are permanent monotonically assigned identities. Inserting a subtask
    // changes display order only; existing IDs are never renumbered.
    let mut created = Vec::with_capacity(pending.len());
    for todo in pending {
        created.push(TodoItem {
            id: alloc_id(&mut store),
            title: todo.title,
            description: todo.description,
            status: TodoStatus::Pending,
            evidence: None,
        });
    }
    store
        .items
        .splice(insertion..insertion, created.iter().cloned());
    normalize_current_id(&mut store);
    write_store(&store)?;

    Ok(json_ok(serde_json::json!({
        "created": created.iter().map(todo_item_json).collect::<Vec<_>>(),
        "count": created.len(),
        "message": format!(
            "Created {} todo(s): {}",
            created.len(),
            created.iter().map(|item| item.id.as_str()).collect::<Vec<_>>().join(", ")
        ),
    })))
}

pub(crate) fn parse_status(value: &str) -> Option<TodoStatus> {
    match value {
        // V2 public vocabulary.
        "idle" => Some(TodoStatus::Pending),
        "in_progress" => Some(TodoStatus::InProgress),
        "completed" | "complete" => Some(TodoStatus::Completed),
        "cancelled" | "canceled" => Some(TodoStatus::Cancelled),
        // V1 compatibility for persisted/model calls during rollout.
        "pending" => Some(TodoStatus::Pending),
        _ => None,
    }
}

/// 可选文本编辑字段：absent → None（不修改）；提供则 trim 后校验长度。
/// title 不允许空（1-100）；description 允许空（显式清空）。
pub(crate) fn parse_edit_field(
    value: Option<&Value>,
    label: &str,
    max_chars: usize,
    allow_empty: bool,
) -> Result<Option<String>, String> {
    let Some(value) = value else {
        return Ok(None);
    };
    let text = value.as_str().unwrap_or_default().trim().to_string();
    if text.is_empty() {
        if allow_empty {
            return Ok(Some(text));
        }
        return Err(json_err_string(
            "INVALID_INPUT",
            format!("{label} must be 1-{max_chars} chars when provided"),
            "Omit the field to leave it unchanged.",
        ));
    }
    if text.chars().count() > max_chars {
        return Err(json_err_string(
            "INVALID_INPUT",
            format!("{label} max {max_chars} chars"),
            "",
        ));
    }
    Ok(Some(text))
}

/// todo_write：全量覆写（v4，Codex update_plan 语义 + QAQ id/evidence 增强）。
/// `items` 即完整清单：整体替换 store，status 为每条必填字段（写即状态——
/// 模型对 in_progress 的显式告知发生在这里）。条目可携带既有 `id` 原样
/// 引用（前端 dashboard / 审计不破），缺省则按 next_id 高水位新分配。
/// 结构变化（增删改排序）与状态变化（翻转 in_progress/completed）共用本
/// 工具；单条纯状态流转的轻量通道是 todo_update。
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
/// runtime 线程局部上下文）。锁与工具路径一致：本函数内获取，持久化
/// 直调 write_store_for（不得再走 save_todo 二次加锁）。
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
