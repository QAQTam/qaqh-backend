//! Session-scoped todo management.
//!
//! Persisted to `sessions/{seed}/todo.json` (session-scoped).
//! The public model contract supports create, status update, cancel, and list.
//! Legacy Goal fields remain readable while Goal automation is frozen.
//!
//! Data model:
//! ```json
//! {
//!   "items": [
//!     {"id":"T1","title":"...","description":"...","status":"pending",
//!      "complexity":"small","deps":[],"effort_min":30,"evidence":null}
//!   ],
//!   "mode": "manual",
//!   "current_id": null,
//!   "auto_turns": 0,
//!   "max_auto_turns": 24
//! }
//! ```

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Mutex;

use crate::{ToolCallCtx, ToolResult, json_err, json_ok};

static TODO_LOCK: Mutex<()> = Mutex::new(());

/// Goal automation is intentionally frozen while the manual todo workflow is
/// the supported contract. Keep the persisted variants readable so existing
/// sessions can be opened without a migration.
pub const GOAL_MODE_ENABLED: bool = false;

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
    #[serde(default)]
    pub complexity: Option<TodoComplexity>,
    #[serde(default)]
    pub deps: Vec<String>,
    #[serde(default)]
    pub effort_min: Option<u32>,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TodoComplexity {
    Small,
    Medium,
    Large,
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
    let mut store = store.clone();
    normalize_frozen_goal(&mut store);
    write_store(&store)
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
                TodoStatus::Pending => "pending".into(),
                TodoStatus::InProgress => "in_progress".into(),
                TodoStatus::Completed => "completed".into(),
                TodoStatus::Cancelled => "cancelled".into(),
            },
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
    let mut store: TodoStore =
        serde_json::from_str(&content).map_err(|e| format!("parse todo.json: {e}"))?;
    normalize_frozen_goal(&mut store);
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
        "pending": pending,
        "in_progress": in_progress,
        "completed": completed,
        "cancelled": cancelled,
        "total": store.items.len(),
        "items": items_summary,
        "goal_enabled": GOAL_MODE_ENABLED,
    }))
    .map_err(|e| format!("todo: {e}"))
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
    let mut store: TodoStore =
        serde_json::from_str(&content).map_err(|e| format!("parse todo.json: {e}"))?;
    normalize_frozen_goal(&mut store);
    Ok(store)
}

fn normalize_frozen_goal(store: &mut TodoStore) {
    if !GOAL_MODE_ENABLED {
        store.mode = TodoMode::Manual;
        store.current_id = store.current_id.take().filter(|id| {
            store
                .items
                .iter()
                .any(|item| &item.id == id && item.status == TodoStatus::InProgress)
        });
        store.auto_turns = 0;
    }
}

fn status_name(status: &TodoStatus) -> &'static str {
    match status {
        TodoStatus::Pending => "pending",
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
        "complexity": item.complexity.as_ref().map(|c| format!("{c:?}").to_lowercase()),
        "effort_min": item.effort_min,
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
// CRUD operations
// ═══════════════════════════════════════════════════════

fn exec_todo_create(args: &Value) -> Result<String, String> {
    let _guard = TODO_LOCK
        .lock()
        .map_err(|_| "todo lock poisoned".to_string())?;
    let mut store = read_store()?;

    let title = args
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let description = args
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();

    if title.is_empty() || title.chars().count() > 100 {
        return Err(json_err(
            "INVALID_INPUT",
            "title must be 1-100 chars",
            "Keep the title short and imperative, e.g. 'Add login API'",
        ));
    }
    if description.chars().count() > 200 {
        return Err(json_err("INVALID_INPUT", "description max 200 chars", ""));
    }

    let complexity = args
        .get("complexity")
        .and_then(|v| v.as_str())
        .and_then(|s| match s {
            "small" => Some(TodoComplexity::Small),
            "medium" => Some(TodoComplexity::Medium),
            "large" => Some(TodoComplexity::Large),
            _ => None,
        });

    let deps: Vec<String> = args
        .get("deps")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let effort_min = args
        .get("effort_min")
        .and_then(|v| v.as_u64())
        .map(|n| n as u32);

    let id = format!("T{}", next_id(&store.items));
    let item = TodoItem {
        id: id.clone(),
        title,
        description,
        status: TodoStatus::Pending,
        complexity,
        deps,
        effort_min,
        evidence: None,
    };
    store.items.push(item.clone());
    write_store(&store)?;
    Ok(json_ok(serde_json::json!({
        "item": todo_item_json(&item),
        "message": format!("Todo {} created.", item.id)
    })))
}

fn exec_todo_update(args: &Value) -> Result<String, String> {
    let _guard = TODO_LOCK
        .lock()
        .map_err(|_| "todo lock poisoned".to_string())?;
    let mut store = read_store()?;

    let id = parse_todo_id(args.get("id")).ok_or_else(|| {
        json_err(
            "INVALID_INPUT",
            "missing or invalid 'id'",
            "Provide the todo ID, e.g. T1 or 1",
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
                "Use todo_list to see all IDs.",
            )
        })?;

    let item = &mut store.items[idx];

    if let Some(s) = args.get("status").and_then(|v| v.as_str()) {
        item.status = match s {
            "pending" => TodoStatus::Pending,
            "in_progress" => TodoStatus::InProgress,
            "completed" => TodoStatus::Completed,
            "cancelled" => TodoStatus::Cancelled,
            _ => {
                return Err(json_err(
                    "INVALID_INPUT",
                    &format!("unknown status: {s}"),
                    "Use: pending, in_progress, completed, or cancelled.",
                ));
            }
        };
    }
    if let Some(t) = args.get("title").and_then(|v| v.as_str()) {
        let t = t.trim().to_string();
        if t.len() > 100 {
            return Err(json_err("INVALID_INPUT", "title max 100 chars", ""));
        }
        item.title = t;
    }
    if let Some(d) = args.get("description").and_then(|v| v.as_str()) {
        let d = d.trim().to_string();
        if d.len() > 200 {
            return Err(json_err("INVALID_INPUT", "description max 200 chars", ""));
        }
        item.description = d;
    }
    if let Some(c) = args.get("complexity").and_then(|v| v.as_str()) {
        item.complexity = match c {
            "small" => Some(TodoComplexity::Small),
            "medium" => Some(TodoComplexity::Medium),
            "large" => Some(TodoComplexity::Large),
            _ => None,
        };
    }
    if let Some(deps) = args.get("deps").and_then(|v| v.as_array()) {
        item.deps = deps
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
    }
    if let Some(e) = args.get("effort_min").and_then(|v| v.as_u64()) {
        item.effort_min = Some(e as u32);
    }
    if let Some(ev) = args.get("evidence").and_then(|v| v.as_str()) {
        item.evidence = Some(ev.trim().to_string());
    }

    let status = item.status.clone();
    let item_json = todo_item_json(item);
    match status {
        TodoStatus::InProgress => store.current_id = Some(id.clone()),
        TodoStatus::Pending | TodoStatus::Completed | TodoStatus::Cancelled
            if store.current_id.as_deref() == Some(id.as_str()) =>
        {
            store.current_id = None;
        }
        _ => {}
    }
    write_store(&store)?;
    Ok(json_ok(serde_json::json!({
        "item": item_json,
        "message": format!("Todo {id} status is now {}.", status_name(&status))
    })))
}

fn exec_todo_cancel(args: &Value) -> Result<String, String> {
    let mut args = args.clone();
    let object = args
        .as_object_mut()
        .ok_or_else(|| json_err("INVALID_INPUT", "arguments must be an object", ""))?;
    object.insert("status".to_string(), Value::String("cancelled".to_string()));
    exec_todo_update(&args)
}

fn exec_todo_list(args: &Value) -> Result<String, String> {
    let store = read_store()?;

    let filter_status = args.get("status").and_then(|v| v.as_str()).unwrap_or("");

    let items: Vec<&TodoItem> = if filter_status.is_empty() {
        store.items.iter().collect()
    } else {
        store
            .items
            .iter()
            .filter(|item| {
                let s = match item.status {
                    TodoStatus::Pending => "pending",
                    TodoStatus::InProgress => "in_progress",
                    TodoStatus::Completed => "completed",
                    TodoStatus::Cancelled => "cancelled",
                };
                s == filter_status
            })
            .collect()
    };

    if items.is_empty() {
        return Ok(json_ok(Value::String(
            "No todos yet. Use todo_create(title=..., description=...) to create one.".to_string(),
        )));
    }

    Ok(json_ok(serde_json::json!({
        "items": items.into_iter().map(todo_item_json).collect::<Vec<_>>(),
        "counts": {
            "pending": count_status(&store, TodoStatus::Pending),
            "in_progress": count_status(&store, TodoStatus::InProgress),
            "completed": count_status(&store, TodoStatus::Completed),
            "cancelled": count_status(&store, TodoStatus::Cancelled),
            "total": store.items.len(),
        }
    })))
}

// ═══════════════════════════════════════════════════════
// Dispatcher
// ═══════════════════════════════════════════════════════

fn tool_result(result: Result<String, String>) -> ToolResult {
    match result {
        Ok(content) => ToolResult::ok(content),
        Err(content) => ToolResult::error(content),
    }
}

fn handle_todo_create(ctx: ToolCallCtx) -> ToolResult {
    tool_result(exec_todo_create(&ctx.args))
}

fn handle_todo_update(ctx: ToolCallCtx) -> ToolResult {
    tool_result(exec_todo_update(&ctx.args))
}

fn handle_todo_cancel(ctx: ToolCallCtx) -> ToolResult {
    tool_result(exec_todo_cancel(&ctx.args))
}

fn handle_todo_list(ctx: ToolCallCtx) -> ToolResult {
    tool_result(exec_todo_list(&ctx.args))
}

// ═══════════════════════════════════════════════════════
// Goal-mode tool handlers
// ═══════════════════════════════════════════════════════

pub fn exec_todo_activate(args: &Value) -> Result<String, String> {
    if !GOAL_MODE_ENABLED {
        return Err(json_err(
            "GOAL_FEATURE_FROZEN",
            "Goal automation is temporarily unavailable",
            "Use the manual todo tools instead.",
        ));
    }
    let _guard = TODO_LOCK
        .lock()
        .map_err(|_| "todo lock poisoned".to_string())?;
    let mut store = read_store()?;

    if store.mode == TodoMode::Goal {
        return Err(json_err(
            "GOAL_ALREADY_ACTIVE",
            "a goal is already active",
            "Stop it first with todo_stop.",
        ));
    }

    let ids: Option<Vec<String>> = args.get("ids").and_then(|v| v.as_array()).map(|arr| {
        arr.iter()
            .filter_map(|v| v.as_str())
            .map(|s| {
                if s.starts_with('T') {
                    s.to_string()
                } else {
                    format!("T{s}")
                }
            })
            .collect()
    });

    let active_items: Vec<TodoItem> = if let Some(ref ids) = ids {
        ids.iter()
            .filter_map(|id| store.items.iter().find(|item| &item.id == id).cloned())
            .collect()
    } else {
        store
            .items
            .iter()
            .filter(|item| matches!(item.status, TodoStatus::Pending | TodoStatus::InProgress))
            .cloned()
            .collect()
    };

    if active_items.is_empty() {
        return Err(json_err(
            "EMPTY_TODO",
            "no items to activate",
            if ids.is_some() {
                "Check the IDs — none matched items in the todo list."
            } else {
                "Use todo_create first, or specify ids."
            },
        ));
    }

    let total = active_items.len();
    let mut sorted = active_items;
    sorted.sort_by_key(|item| match item.complexity {
        Some(TodoComplexity::Small) => 0,
        Some(TodoComplexity::Medium) => 1,
        Some(TodoComplexity::Large) => 2,
        None => 3,
    });
    for item in &mut sorted {
        item.status = TodoStatus::Pending;
    }
    sorted[0].status = TodoStatus::InProgress;
    let first_id = sorted[0].id.clone();
    let first_title = sorted[0].title.clone();

    store.items = sorted;
    store.mode = TodoMode::Goal;
    store.current_id = Some(first_id.clone());
    store.auto_turns = 0;
    write_store(&store)?;

    Ok(json_ok(Value::String(format!(
        "Goal activated with {total} items. Starting: {first_id} {first_title} — complete this step then call todo_step_complete(id=\"{first_id}\", summary=\"...\")."
    ))))
}

// ═══════════════════════════════════════════════════════
// Registration
// ═══════════════════════════════════════════════════════

use crate::{ToolHandler, ToolRisk};
use std::time::Duration;

pub fn register(mgr: &mut crate::ToolManager) {
    // Compatibility decision: tool definitions are regenerated for every
    // model session, so there is no persisted caller that needs the old
    // multiplexed `todo(action=...)` schema. The TodoStore format remains
    // dual-readable, and the runtime keeps its old todo.action endpoint as an
    // explicit frozen response for mixed desktop/daemon versions.
    mgr.register(ToolHandler {
        key: "todo_create".to_string(),
        description: "Create one todo. The new todo starts in pending status.",
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "title": {"type": "string", "description": "Short imperative title, 1-100 characters"},
                "description": {"type": "string", "description": "Optional context, at most 200 characters"}
            },
            "required": ["title"],
            "additionalProperties": false
        }),
        handler: handle_todo_create,
        risk: ToolRisk::Write,
        default_timeout: Duration::from_secs(15),
    });
    mgr.register(ToolHandler {
        key: "todo_update".to_string(),
        description: "Set a todo status to pending, in_progress, completed, or cancelled.",
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "id": {"type": ["string", "integer"], "description": "Todo ID, for example T1 or 1"},
                "status": {"type": "string", "enum": ["pending", "in_progress", "completed", "cancelled"]},
                "evidence": {"type": "string", "description": "Optional concise completion evidence"}
            },
            "required": ["id", "status"],
            "additionalProperties": false
        }),
        handler: handle_todo_update,
        risk: ToolRisk::Write,
        default_timeout: Duration::from_secs(15),
    });
    mgr.register(ToolHandler {
        key: "todo_cancel".to_string(),
        description: "Cancel one todo. This is the simplest way to set its status to cancelled.",
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "id": {"type": ["string", "integer"], "description": "Todo ID, for example T1 or 1"}
            },
            "required": ["id"],
            "additionalProperties": false
        }),
        handler: handle_todo_cancel,
        risk: ToolRisk::Write,
        default_timeout: Duration::from_secs(15),
    });
    mgr.register(ToolHandler {
        key: "todo_list".to_string(),
        description: "List todos and exact status counts. Optionally filter by one status.",
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "status": {"type": "string", "enum": ["pending", "in_progress", "completed", "cancelled"]}
            },
            "additionalProperties": false
        }),
        handler: handle_todo_list,
        risk: ToolRisk::ReadOnly,
        default_timeout: Duration::from_secs(15),
    });
}
