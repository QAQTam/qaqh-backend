//! Session-scoped todo management.
//!
//! Persisted to `sessions/{seed}/todo.json` (session-scoped).
//! The public model contract supports create, ordered insert, ID-only state changes, and list.
//!
//! Data model:
//! ```json
//! {
//!   "items": [
//!     {"id":"T1","title":"...","description":"...","status":"idle","evidence":null}
//!   ]
//! }
//! ```

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Mutex;

use crate::{ToolCallCtx, ToolResult, json_err, json_ok};

static TODO_LOCK: Mutex<()> = Mutex::new(());

// ═══════════════════════════════════════════════════════
// Data model
// ═══════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TodoItem {
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
}

fn default_max_auto() -> u32 {
    24
}

// ═══════════════════════════════════════════════════════
// Persistence
// ═══════════════════════════════════════════════════════

fn todo_path() -> Option<std::path::PathBuf> {
    let session = crate::runtime::context()
        .map(|ctx| ctx.active_session)
        .unwrap_or_default();
    if session.is_empty() {
        None
    } else {
        Some(
            deepx_types::platform::sessions_dir()
                .join(&session)
                .join("todo.json"),
        )
    }
}

/// Public API: load the TodoStore from disk (used by GoalEngine).
pub fn load_todo() -> Result<TodoStore, String> {
    read_store()
}

/// Public API: save the TodoStore to disk atomically (used by GoalEngine).
pub fn save_todo(store: &TodoStore) -> Result<(), String> {
    write_store(store)
}

/// Get todo items as Dashboard-compatible info structs.
pub fn get_todo_infos() -> Vec<deepx_proto::TaskInfo> {
    let store = read_store().unwrap_or_default();
    store
        .items
        .iter()
        .map(|item| deepx_proto::TaskInfo {
            id: item.id.clone(),
            subject: item.title.clone(),
            description: item.description.clone(),
            status: match item.status {
                TodoStatus::Pending => "idle".into(),
                TodoStatus::InProgress => "in_progress".into(),
                TodoStatus::Completed => "completed".into(),
                TodoStatus::Cancelled => "cancelled".into(),
            },
            evidence: item.evidence.clone(),
        })
        .collect()
}

/// Session-scoped todo status for the frontend Todo panel.
pub fn todo_status_json(seed: &str) -> Result<String, String> {
    if seed.is_empty() {
        return Ok("null".into());
    }
    let path = deepx_types::platform::sessions_dir()
        .join(seed)
        .join("todo.json");
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok("null".into()),
        Err(e) => return Err(format!("read todo.json: {e}")),
    };
    let store: TodoStore =
        serde_json::from_str(&content).map_err(|e| format!("parse todo.json: {e}"))?;
    let current = store
        .current_id
        .as_ref()
        .and_then(|id| {
            store
                .items
                .iter()
                .find(|item| &item.id == id && item.status == TodoStatus::InProgress)
        })
        .or_else(|| {
            store
                .items
                .iter()
                .find(|item| item.status == TodoStatus::InProgress)
        });
    let pending = count_status(&store, TodoStatus::Pending);
    let in_progress = count_status(&store, TodoStatus::InProgress);
    let completed = count_status(&store, TodoStatus::Completed);
    let cancelled = count_status(&store, TodoStatus::Cancelled);
    let items_summary: Vec<serde_json::Value> = store.items.iter().map(todo_item_json).collect();
    serde_json::to_string(&serde_json::json!({
        "mode": "manual",
        "current_id": current.map(|item| item.id.clone()),
        "current_title": current.map(|i| i.title.clone()),
        "idle": pending,
        "pending": pending,
        "in_progress": in_progress,
        "completed": completed,
        "cancelled": cancelled,
        "total": store.items.len(),
        "items": items_summary,
    }))
    .map_err(|e| format!("todo: {e}"))
}

/// Direct cancel by session seed — no runtime context needed.
pub fn todo_cancel_json(seed: &str, id: &str) -> Result<String, String> {
    if seed.is_empty() {
        return Err(json_err("INVALID_INPUT", "no active session", ""));
    }
    let _guard = TODO_LOCK
        .lock()
        .map_err(|_| "todo lock poisoned".to_string())?;
    let path = deepx_types::platform::sessions_dir()
        .join(seed)
        .join("todo.json");
    if !path.exists() {
        return Err(json_err("NOT_FOUND", "no todo list for this session", ""));
    }
    let content = std::fs::read_to_string(&path).map_err(|e| format!("read todo.json: {e}"))?;
    let mut store: TodoStore =
        serde_json::from_str(&content).map_err(|e| format!("parse todo.json: {e}"))?;
    let idx = store
        .items
        .iter()
        .position(|item| item.id == id)
        .ok_or_else(|| {
            json_err(
                "NOT_FOUND",
                &format!("todo {id} not found"),
                "Use todo(action=\"list\") to see all IDs.",
            )
        })?;

    store.items[idx].status = TodoStatus::Cancelled;
    normalize_current_id(&mut store);

    let item_json = todo_item_json(&store.items[idx]);
    let tmp = path.with_extension("json.tmp");
    let data = serde_json::to_vec_pretty(&store).map_err(|e| format!("serialize todo: {e}"))?;
    std::fs::write(&tmp, data).map_err(|e| format!("write todo.tmp: {e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("rename todo: {e}"))?;

    Ok(json_ok(serde_json::json!({
        "item": item_json,
        "message": format!("Todo {id} cancelled.")
    })))
}

fn read_store() -> Result<TodoStore, String> {
    let path = todo_path().ok_or("no active session")?;
    if !path.exists() {
        return Ok(TodoStore {
            items: Vec::new(),
            mode: TodoMode::Manual,
            current_id: None,
            auto_turns: 0,
            max_auto_turns: 24,
        });
    }
    let content = std::fs::read_to_string(&path).map_err(|e| format!("read todo.json: {e}"))?;
    let store: TodoStore =
        serde_json::from_str(&content).map_err(|e| format!("parse todo.json: {e}"))?;
    Ok(store)
}

fn status_name(status: &TodoStatus) -> &'static str {
    match status {
        TodoStatus::Pending => "idle",
        TodoStatus::InProgress => "in_progress",
        TodoStatus::Completed => "completed",
        TodoStatus::Cancelled => "cancelled",
    }
}

fn count_status(store: &TodoStore, status: TodoStatus) -> usize {
    store
        .items
        .iter()
        .filter(|item| item.status == status)
        .count()
}

fn todo_item_json(item: &TodoItem) -> serde_json::Value {
    serde_json::json!({
        "id": item.id,
        "title": item.title,
        "description": item.description,
        "status": status_name(&item.status),
        "evidence": item.evidence,
    })
}

/// Atomic write: temporary file → rename.
fn write_store(store: &TodoStore) -> Result<(), String> {
    let path = todo_path().ok_or("no active session")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create todo directory: {e}"))?;
    }
    let tmp = path.with_extension("json.tmp");
    let data = serde_json::to_vec_pretty(store).map_err(|e| format!("serialize todo: {e}"))?;
    std::fs::write(&tmp, data).map_err(|e| format!("write todo.tmp: {e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("rename todo: {e}"))
}

// ═══════════════════════════════════════════════════════
// ID generation
// ═══════════════════════════════════════════════════════

fn next_id(items: &[TodoItem]) -> u32 {
    items
        .iter()
        .filter_map(|item| item.id.strip_prefix('T')?.parse::<u32>().ok())
        .max()
        .unwrap_or(0)
        + 1
}

fn parse_todo_id(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(id) => {
            let id = id.trim();
            if id.starts_with('T') && id[1..].parse::<u32>().is_ok() {
                Some(id.to_string())
            } else {
                id.parse::<u32>().ok().map(|number| format!("T{number}"))
            }
        }
        Value::Number(number) => number.as_u64().map(|number| format!("T{number}")),
        _ => None,
    }
}

// ═══════════════════════════════════════════════════════
// Todo V2 operations
// ═══════════════════════════════════════════════════════

/// A single model-authored task description before its permanent ID is assigned.
#[derive(Debug, Clone)]
struct NewTodo {
    title: String,
    description: String,
}

/// One mutation can create at most this many tasks. Keeping creation in one
/// transaction prevents parallel tool calls from racing the T{n} allocator.
const MAX_CREATE_ITEMS: usize = 20;

fn parse_new_todo(value: &Value, label: &str) -> Result<NewTodo, String> {
    let title = value
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if title.is_empty() || title.chars().count() > 100 {
        return Err(json_err(
            "INVALID_INPUT",
            &format!("{label}.title must be 1-100 chars"),
            "Use a short imperative title, e.g. 'Add login API'.",
        ));
    }
    let description = value
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if description.chars().count() > 200 {
        return Err(json_err(
            "INVALID_INPUT",
            &format!("{label}.description max 200 chars"),
            "",
        ));
    }
    Ok(NewTodo { title, description })
}

fn parse_create_items(args: &Value) -> Result<Vec<NewTodo>, String> {
    if let Some(items) = args.get("items").and_then(Value::as_array) {
        if items.is_empty() {
            return Err(json_err(
                "INVALID_INPUT",
                "items must not be empty",
                "Provide at least one {title, description?} item.",
            ));
        }
        if items.len() > MAX_CREATE_ITEMS {
            return Err(json_err(
                "INVALID_INPUT",
                &format!("items max {MAX_CREATE_ITEMS} entries per call"),
                "Split the plan into multiple create calls.",
            ));
        }
        return items
            .iter()
            .enumerate()
            .map(|(index, item)| parse_new_todo(item, &format!("items[{index}]")))
            .collect();
    }
    Ok(vec![parse_new_todo(args, "todo")?])
}

fn insertion_index(store: &TodoStore, args: &Value) -> Result<usize, String> {
    let before_raw = args.get("before_id");
    let after_raw = args.get("after_id");
    let before = parse_todo_id(before_raw);
    let after = parse_todo_id(after_raw);
    if before_raw.is_some() && before.is_none() {
        return Err(json_err(
            "INVALID_INPUT",
            "invalid before_id",
            "Use an assigned ID such as T1.",
        ));
    }
    if after_raw.is_some() && after.is_none() {
        return Err(json_err(
            "INVALID_INPUT",
            "invalid after_id",
            "Use an assigned ID such as T1.",
        ));
    }
    if before.is_some() && after.is_some() {
        return Err(json_err(
            "INVALID_INPUT",
            "use only one of before_id or after_id",
            "",
        ));
    }
    if before.is_none() && after.is_none() {
        return Err(json_err(
            "INVALID_INPUT",
            "insert requires before_id or after_id",
            "Use action=create to append tasks at the end.",
        ));
    }
    if let Some(id) = before {
        return store
            .items
            .iter()
            .position(|item| item.id == id)
            .ok_or_else(|| {
                json_err(
                    "NOT_FOUND",
                    &format!("todo {id} not found"),
                    "Use todo(action=\"list\") to inspect IDs.",
                )
            });
    }
    if let Some(id) = after {
        return store
            .items
            .iter()
            .position(|item| item.id == id)
            .map(|index| index + 1)
            .ok_or_else(|| {
                json_err(
                    "NOT_FOUND",
                    &format!("todo {id} not found"),
                    "Use todo(action=\"list\") to inspect IDs.",
                )
            });
    }
    unreachable!("insert anchor presence was validated above")
}

fn exec_todo_create(args: &Value, positioned: bool) -> Result<String, String> {
    let _guard = TODO_LOCK
        .lock()
        .map_err(|_| "todo lock poisoned".to_string())?;
    let mut store = read_store()?;
    if !positioned && (args.get("after_id").is_some() || args.get("before_id").is_some()) {
        return Err(json_err(
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
    let pending = parse_create_items(args)?;

    // IDs are permanent monotonically assigned identities. Inserting a subtask
    // changes display order only; existing IDs are never renumbered.
    let mut next = next_id(&store.items);
    let mut created = Vec::with_capacity(pending.len());
    for todo in pending {
        created.push(TodoItem {
            id: format!("T{next}"),
            title: todo.title,
            description: todo.description,
            status: TodoStatus::Pending,
            evidence: None,
        });
        next += 1;
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

fn parse_status(value: &str) -> Option<TodoStatus> {
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

fn exec_todo_set(args: &Value) -> Result<String, String> {
    let _guard = TODO_LOCK
        .lock()
        .map_err(|_| "todo lock poisoned".to_string())?;
    let mut store = read_store()?;
    let id = parse_todo_id(args.get("id")).ok_or_else(|| {
        json_err(
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
        json_err(
            "INVALID_INPUT",
            &format!("unknown status: {requested}"),
            "Use idle, in_progress, completed, or cancelled.",
        )
    })?;
    let idx = store
        .items
        .iter()
        .position(|item| item.id == id)
        .ok_or_else(|| {
            json_err(
                "NOT_FOUND",
                &format!("todo {id} not found"),
                "Use todo(action=\"list\") to inspect IDs.",
            )
        })?;

    let evidence = args
        .get("evidence")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    if args.get("evidence").is_some() && evidence.is_none() {
        return Err(json_err(
            "INVALID_INPUT",
            "evidence must be a non-empty string when provided",
            "Omit evidence unless there is a concrete result summary.",
        ));
    }
    {
        let item = &mut store.items[idx];
        item.status = status.clone();
        if evidence.is_some() {
            item.evidence = evidence;
        }
        // V1 clients may still send schema-filled empty strings. Status changes
        // are deliberately ID-only and must never erase task metadata.
    }

    normalize_current_id(&mut store);
    let item_json = todo_item_json(&store.items[idx]);
    write_store(&store)?;
    Ok(json_ok(serde_json::json!({
        "item": item_json,
        "message": format!("Todo {id} is now {}.", status_name(&status))
    })))
}

fn normalize_current_id(store: &mut TodoStore) {
    if let Some(active) = store
        .items
        .iter()
        .find(|item| item.status == TodoStatus::InProgress)
    {
        store.current_id = Some(active.id.clone());
    } else {
        store.current_id = None;
    }
}

fn exec_todo_list(args: &Value) -> Result<String, String> {
    let store = read_store()?;
    let filter = args
        .get("status")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(|value| {
            parse_status(value).ok_or_else(|| {
                json_err(
                    "INVALID_INPUT",
                    &format!("unknown status: {value}"),
                    "Use idle, in_progress, completed, or cancelled.",
                )
            })
        })
        .transpose()?;
    let items: Vec<&TodoItem> = store
        .items
        .iter()
        .filter(|item| filter.as_ref().is_none_or(|status| item.status == *status))
        .collect();

    Ok(json_ok(serde_json::json!({
        "items": items.into_iter().map(todo_item_json).collect::<Vec<_>>(),
        "current_id": store.current_id,
        "counts": {
            "idle": count_status(&store, TodoStatus::Pending),
            "in_progress": count_status(&store, TodoStatus::InProgress),
            "completed": count_status(&store, TodoStatus::Completed),
            "cancelled": count_status(&store, TodoStatus::Cancelled),
            "total": store.items.len(),
        }
    })))
}

// ═══════════════════════════════════════════════════════
// Dispatcher and registration
// ═══════════════════════════════════════════════════════

fn tool_result(result: Result<String, String>) -> ToolResult {
    match result {
        Ok(content) => ToolResult::ok(content),
        Err(content) => ToolResult::error(content),
    }
}

fn reject_fields(args: &Value, fields: &[&str], action: &str) -> Result<(), String> {
    let present: Vec<&str> = fields
        .iter()
        .copied()
        .filter(|field| args.get(*field).is_some())
        .collect();
    if present.is_empty() {
        Ok(())
    } else {
        Err(json_err(
            "INVALID_INPUT",
            &format!("action={action} does not accept: {}", present.join(", ")),
            "Follow the action-specific Todo V2 schema.",
        ))
    }
}

fn handle_todo(ctx: ToolCallCtx) -> ToolResult {
    let action = ctx
        .args
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let result = match action {
        "create" => reject_fields(
            &ctx.args,
            &["id", "status", "evidence", "after_id", "before_id"],
            action,
        )
        .and_then(|_| exec_todo_create(&ctx.args, false)),
        "insert" => reject_fields(&ctx.args, &["id", "status", "evidence"], action)
            .and_then(|_| exec_todo_create(&ctx.args, true)),
        "set" => reject_fields(
            &ctx.args,
            &["title", "description", "items", "after_id", "before_id"],
            action,
        )
        .and_then(|_| exec_todo_set(&ctx.args)),
        "list" => reject_fields(
            &ctx.args,
            &[
                "title",
                "description",
                "items",
                "id",
                "evidence",
                "after_id",
                "before_id",
            ],
            action,
        )
        .and_then(|_| exec_todo_list(&ctx.args)),
        // V1 compatibility aliases. They are accepted but no longer advertised.
        "create_batch" => exec_todo_create(&ctx.args, false),
        "update" => exec_todo_set(&ctx.args),
        "cancel" => {
            let mut args = ctx.args.clone();
            if let Some(object) = args.as_object_mut() {
                object.insert("status".into(), Value::String("cancelled".into()));
            }
            exec_todo_set(&args)
        }
        _ => Err(json_err(
            "INVALID_INPUT",
            "todo.action must be create, insert, set, or list",
            "",
        )),
    };
    tool_result(result)
}

use crate::{ToolHandler, ToolRisk};
use std::time::Duration;

pub fn register(mgr: &mut crate::ToolManager) {
    mgr.register_with_placement(
        ToolHandler {
            key: "todo".to_string(),
            description: "Manage the ordered session task list. Create one or many tasks with action=create and title/description (or items); IDs T1, T2, ... are assigned atomically. After creation, change state using only action=set, id, and status (idle, in_progress, completed, cancelled); evidence is optional when completed. If work reveals a subtask, action=insert creates it after_id or before_id without renumbering existing IDs. action=list returns the ordered list. Do not send title/description during state changes.",
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["create", "insert", "set", "list"],
                        "description": "create tasks; insert a discovered subtask; set one task state; or list tasks."
                    },
                    "title": {
                        "type": "string",
                        "description": "For single create/insert: short imperative title, 1-100 chars."
                    },
                    "description": {
                        "type": "string",
                        "description": "For single create/insert: optional context or acceptance criteria, max 200 chars."
                    },
                    "items": {
                        "type": "array",
                        "maxItems": 20,
                        "description": "For creating a group atomically. IDs are assigned in array order.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "title": {"type": "string"},
                                "description": {"type": "string"}
                            },
                            "required": ["title"],
                            "additionalProperties": false
                        }
                    },
                    "id": {
                        "type": ["string", "integer"],
                        "description": "For action=set: assigned task ID, e.g. T1."
                    },
                    "status": {
                        "type": "string",
                        "enum": ["idle", "in_progress", "completed", "cancelled"],
                        "description": "For action=set."
                    },
                    "evidence": {
                        "type": "string",
                        "description": "Optional completion/result summary for action=set."
                    },
                    "after_id": {
                        "type": ["string", "integer"],
                        "description": "For action=insert: place new task(s) after this stable ID."
                    },
                    "before_id": {
                        "type": ["string", "integer"],
                        "description": "For action=insert: place new task(s) before this stable ID."
                    }
                },
                "required": ["action"],
                "additionalProperties": false,
                "oneOf": [
                    {
                        "title": "Create one task",
                        "properties": {"action": {"const": "create"}},
                        "required": ["action", "title"],
                        "not": {"anyOf": [
                            {"required": ["items"]},
                            {"required": ["id"]},
                            {"required": ["status"]},
                            {"required": ["evidence"]},
                            {"required": ["after_id"]},
                            {"required": ["before_id"]}
                        ]}
                    },
                    {
                        "title": "Create a task group",
                        "properties": {"action": {"const": "create"}},
                        "required": ["action", "items"],
                        "not": {"anyOf": [
                            {"required": ["title"]},
                            {"required": ["description"]},
                            {"required": ["id"]},
                            {"required": ["status"]},
                            {"required": ["evidence"]},
                            {"required": ["after_id"]},
                            {"required": ["before_id"]}
                        ]}
                    },
                    {
                        "title": "Insert one task after an ID",
                        "properties": {"action": {"const": "insert"}},
                        "required": ["action", "title", "after_id"],
                        "not": {"anyOf": [
                            {"required": ["items"]},
                            {"required": ["id"]},
                            {"required": ["status"]},
                            {"required": ["evidence"]},
                            {"required": ["before_id"]}
                        ]}
                    },
                    {
                        "title": "Insert one task before an ID",
                        "properties": {"action": {"const": "insert"}},
                        "required": ["action", "title", "before_id"],
                        "not": {"anyOf": [
                            {"required": ["items"]},
                            {"required": ["id"]},
                            {"required": ["status"]},
                            {"required": ["evidence"]},
                            {"required": ["after_id"]}
                        ]}
                    },
                    {
                        "title": "Insert a task group after an ID",
                        "properties": {"action": {"const": "insert"}},
                        "required": ["action", "items", "after_id"],
                        "not": {"anyOf": [
                            {"required": ["title"]},
                            {"required": ["description"]},
                            {"required": ["id"]},
                            {"required": ["status"]},
                            {"required": ["evidence"]},
                            {"required": ["before_id"]}
                        ]}
                    },
                    {
                        "title": "Insert a task group before an ID",
                        "properties": {"action": {"const": "insert"}},
                        "required": ["action", "items", "before_id"],
                        "not": {"anyOf": [
                            {"required": ["title"]},
                            {"required": ["description"]},
                            {"required": ["id"]},
                            {"required": ["status"]},
                            {"required": ["evidence"]},
                            {"required": ["after_id"]}
                        ]}
                    },

                    {
                        "title": "Set task state",
                        "properties": {"action": {"const": "set"}},
                        "required": ["action", "id", "status"],
                        "not": {"anyOf": [
                            {"required": ["title"]},
                            {"required": ["description"]},
                            {"required": ["items"]},
                            {"required": ["after_id"]},
                            {"required": ["before_id"]}
                        ]}
                    },
                    {
                        "title": "List tasks",
                        "properties": {"action": {"const": "list"}},
                        "required": ["action"],
                        "not": {"anyOf": [
                            {"required": ["title"]},
                            {"required": ["description"]},
                            {"required": ["items"]},
                            {"required": ["id"]},
                            {"required": ["evidence"]},
                            {"required": ["after_id"]},
                            {"required": ["before_id"]}
                        ]}
                    }
                ]
            }),
            handler: handle_todo,
            risk: ToolRisk::Write,
            default_timeout: Duration::from_secs(15),
        },
        crate::ToolPlacement::Workspace,
    );
}

// ═══════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    /// 隔离数据目录（USERPROFILE/HOME → 临时目录）并设置会话上下文；
    /// 结束恢复环境，避免污染真实 ~/.deepx/sessions。
    fn with_isolated_todo<F: FnOnce(&str)>(f: F) {
        let _guard = crate::TEST_RUNTIME_SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let home_var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        let old_home: Option<OsString> = std::env::var_os(home_var);
        // Rust 2024: set_var/remove_var are unsafe (test-only, single-threaded via TEST_RUNTIME_SERIAL).
        unsafe { std::env::set_var(home_var, &dir.path()) };
        let seed = format!("test-seed-{}", std::process::id());
        crate::runtime::set_context(&seed, 4);
        f(&seed);
        unsafe {
            match old_home {
                Some(value) => std::env::set_var(home_var, value),
                None => std::env::remove_var(home_var),
            }
        }
    }

    fn parse(result: &Result<String, String>) -> serde_json::Value {
        serde_json::from_str(result.as_ref().unwrap()).unwrap()
    }

    fn ids(store: &TodoStore) -> Vec<String> {
        store.items.iter().map(|item| item.id.clone()).collect()
    }

    #[test]
    fn create_group_assigns_consecutive_ids_atomically() {
        with_isolated_todo(|_seed| {
            exec_todo_create(&serde_json::json!({"title": "single"}), false).unwrap();
            let result = exec_todo_create(
                &serde_json::json!({
                    "items": [
                        {"title": "a"},
                        {"title": "b", "description": "desc b"},
                        {"title": "c"}
                    ]
                }),
                false,
            );
            let value = parse(&result);
            let got: Vec<&str> = value["created"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| item["id"].as_str().unwrap())
                .collect();
            assert_eq!(got, ["T2", "T3", "T4"]);
            assert_eq!(ids(&read_store().unwrap()), ["T1", "T2", "T3", "T4"]);
        });
    }

    #[test]
    fn create_group_is_atomic_on_validation_failure() {
        with_isolated_todo(|_seed| {
            let result = exec_todo_create(
                &serde_json::json!({
                    "items": [{"title": "good"}, {"title": "   "}]
                }),
                false,
            );
            assert!(result.is_err());
            assert!(read_store().unwrap().items.is_empty());
        });
    }

    #[test]
    fn create_rejects_empty_and_oversized_groups() {
        with_isolated_todo(|_seed| {
            assert!(exec_todo_create(&serde_json::json!({"items": []}), false).is_err());
            let items: Vec<Value> = (0..21)
                .map(|index| serde_json::json!({"title": format!("t{index}")}))
                .collect();
            assert!(exec_todo_create(&serde_json::json!({"items": items}), false).is_err());
            assert!(read_store().unwrap().items.is_empty());
        });
    }

    #[test]
    fn insert_preserves_ids_and_changes_display_order() {
        with_isolated_todo(|_seed| {
            exec_todo_create(
                &serde_json::json!({
                    "items": [{"title": "a"}, {"title": "b"}]
                }),
                false,
            )
            .unwrap();
            let result = exec_todo_create(
                &serde_json::json!({"title": "child", "after_id": "T1"}),
                true,
            );
            assert_eq!(parse(&result)["created"][0]["id"], "T3");
            assert_eq!(ids(&read_store().unwrap()), ["T1", "T3", "T2"]);
        });
    }

    #[test]
    fn set_status_is_id_only_and_never_erases_metadata() {
        with_isolated_todo(|_seed| {
            exec_todo_create(
                &serde_json::json!({"title": "Keep me", "description": "Keep this too"}),
                false,
            )
            .unwrap();
            exec_todo_set(&serde_json::json!({
                "id": "T1",
                "status": "in_progress",
                "title": "",
                "description": ""
            }))
            .unwrap();
            exec_todo_set(&serde_json::json!({
                "id": "T1",
                "status": "completed",
                "evidence": "verified",
                "title": "",
                "description": ""
            }))
            .unwrap();
            let store = read_store().unwrap();
            assert_eq!(store.items[0].title, "Keep me");
            assert_eq!(store.items[0].description, "Keep this too");
            assert_eq!(store.items[0].evidence.as_deref(), Some("verified"));
            assert_eq!(store.items[0].status, TodoStatus::Completed);
            assert!(store.current_id.is_none());
        });
    }

    #[test]
    fn idle_is_public_alias_for_pending() {
        with_isolated_todo(|_seed| {
            exec_todo_create(&serde_json::json!({"title": "a"}), false).unwrap();
            exec_todo_set(&serde_json::json!({"id": "T1", "status": "in_progress"})).unwrap();
            exec_todo_set(&serde_json::json!({"id": "T1", "status": "idle"})).unwrap();
            assert_eq!(read_store().unwrap().items[0].status, TodoStatus::Pending);
        });
    }

    #[test]
    fn current_id_falls_back_to_another_in_progress_task() {
        with_isolated_todo(|_seed| {
            exec_todo_create(
                &serde_json::json!({"items": [{"title": "a"}, {"title": "b"}]}),
                false,
            )
            .unwrap();
            exec_todo_set(&serde_json::json!({"id": "T1", "status": "in_progress"})).unwrap();
            exec_todo_set(&serde_json::json!({"id": "T2", "status": "in_progress"})).unwrap();
            exec_todo_set(&serde_json::json!({"id": "T1", "status": "completed"})).unwrap();
            assert_eq!(read_store().unwrap().current_id.as_deref(), Some("T2"));
        });
    }

    #[test]
    fn insert_requires_exactly_one_existing_anchor() {
        with_isolated_todo(|_seed| {
            exec_todo_create(&serde_json::json!({"title": "parent"}), false).unwrap();

            let missing = exec_todo_create(&serde_json::json!({"title": "child"}), true);
            assert!(missing.is_err());

            let invalid = exec_todo_create(
                &serde_json::json!({"title": "child", "after_id": "not-an-id"}),
                true,
            );
            assert!(invalid.is_err());

            let absent = exec_todo_create(
                &serde_json::json!({"title": "child", "before_id": "T99"}),
                true,
            );
            assert!(absent.is_err());

            let conflicting = exec_todo_create(
                &serde_json::json!({
                    "title": "child",
                    "after_id": "T1",
                    "before_id": "T1"
                }),
                true,
            );
            assert!(conflicting.is_err());
            assert_eq!(ids(&read_store().unwrap()), ["T1"]);
        });
    }

    #[test]
    fn v2_field_guard_rejects_metadata_on_set() {
        let args = serde_json::json!({
            "action": "set",
            "id": "T1",
            "status": "completed",
            "title": ""
        });
        assert!(
            reject_fields(
                &args,
                &["title", "description", "items", "after_id", "before_id"],
                "set"
            )
            .is_err()
        );
    }
}
