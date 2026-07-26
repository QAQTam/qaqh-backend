//! Todo management: unified task/plan/goal tool.
//!
//! Persisted to `sessions/{seed}/todo.json` (session-scoped).
//! Supports CRUD, goal mode activation, and pending changes buffering.
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

use crate::{json_err, json_ok, ToolCallCtx, ToolResult};

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
    write_store(store)
}

/// Get todo items as Dashboard-compatible info structs.
pub fn get_todo_infos() -> Vec<deepx_proto::TaskInfo> {
    let store = read_store().unwrap_or_default();
    store.items.iter().map(|item| deepx_proto::TaskInfo {
        id: item.id.clone(),
        subject: item.title.clone(),
        description: item.description.clone(),
        status: match item.status { TodoStatus::Pending => "pending".into(), TodoStatus::InProgress => "in_progress".into(), TodoStatus::Completed => "completed".into(), TodoStatus::Cancelled => "cancelled".into() },
    }).collect()
}

/// Session-scoped todo status for the frontend Goal panel.
pub fn todo_status_json(seed: &str) -> Result<String, String> {
    if seed.is_empty() { return Ok("null".into()); }
    let path = deepx_types::platform::sessions_dir().join(seed).join("todo.json");
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok("null".into()),
        Err(e) => return Err(format!("read todo.json: {e}")),
    };
    let store: TodoStore = serde_json::from_str(&content).map_err(|e| format!("parse todo.json: {e}"))?;
    let current = store.current_id.as_ref().and_then(|id| store.items.iter().find(|i| &i.id == id));
    let items_summary: Vec<serde_json::Value> = store.items.iter().map(|item| serde_json::json!({
        "id": item.id, "title": item.title, "description": item.description,
        "status": match item.status { TodoStatus::Pending=>"pending", TodoStatus::InProgress=>"in_progress", TodoStatus::Completed=>"completed", TodoStatus::Cancelled=>"cancelled" },
        "complexity": item.complexity.as_ref().map(|c| format!("{:?}", c).to_lowercase()),
        "effort_min": item.effort_min,
    })).collect();
    serde_json::to_string(&serde_json::json!({
        "mode": match store.mode { TodoMode::Manual => "manual", TodoMode::Goal => "goal" },
        "current_id": store.current_id,
        "current_title": current.map(|i| i.title.clone()),
        "completed": store.items.iter().filter(|i| i.status == TodoStatus::Completed).count(),
        "total": store.items.len(),
        "items": items_summary,
        "auto_turns": store.auto_turns,
    })).map_err(|e| format!("todo: {e}"))
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
    let content =
        std::fs::read_to_string(&path).map_err(|e| format!("read todo.json: {e}"))?;
    serde_json::from_str(&content).map_err(|e| format!("parse todo.json: {e}"))
}

/// Atomic write: temporary file → rename.
fn write_store(store: &TodoStore) -> Result<(), String> {
    let path = todo_path().ok_or("no active session")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create todo directory: {e}"))?;
    }
    let tmp = path.with_extension("json.tmp");
    let data =
        serde_json::to_vec_pretty(store).map_err(|e| format!("serialize todo: {e}"))?;
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

// ═══════════════════════════════════════════════════════
// CRUD operations
// ═══════════════════════════════════════════════════════

fn exec_todo_create(args: &Value) -> Result<String, String> {
    let _guard = TODO_LOCK.lock().map_err(|_| "todo lock poisoned".to_string())?;
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
        return Err(json_err(
            "INVALID_INPUT",
            "description max 200 chars",
            "",
        ));
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

    let effort_min = args.get("effort_min").and_then(|v| v.as_u64()).map(|n| n as u32);

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
        "id": item.id,
        "content": format!("Todo {} created: {}", item.id, item.title)
    })))
}

fn exec_todo_update(args: &Value) -> Result<String, String> {
    let _guard = TODO_LOCK.lock().map_err(|_| "todo lock poisoned".to_string())?;
    let mut store = read_store()?;

    let id_str = args
        .get("id")
        .and_then(|v| v.as_str().or_else(|| v.as_u64().map(|_| "")))
        .unwrap_or("");
    let id = if id_str.starts_with('T') {
        id_str.to_string()
    } else if let Ok(n) = id_str.parse::<u32>() {
        format!("T{n}")
    } else {
        return Err(json_err(
            "INVALID_INPUT",
            "missing or invalid 'id'",
            "Provide the todo ID, e.g. T1 or 1",
        ));
    };

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
    write_store(&store)?;
    Ok(json_ok(Value::String(format!(
        "Todo {} updated: status={:?}",
        id, status
    ))))
}

fn exec_todo_delete(args: &Value) -> Result<String, String> {
    let _guard = TODO_LOCK.lock().map_err(|_| "todo lock poisoned".to_string())?;
    let mut store = read_store()?;

    let id_str = args
        .get("id")
        .and_then(|v| v.as_str().or_else(|| v.as_u64().map(|_| "")))
        .unwrap_or("");
    let id = if id_str.starts_with('T') {
        id_str.to_string()
    } else if let Ok(n) = id_str.parse::<u32>() {
        format!("T{n}")
    } else {
        return Err(json_err("INVALID_INPUT", "missing or invalid 'id'", ""));
    };

    let idx = store.items.iter().position(|item| item.id == id).ok_or_else(|| {
        json_err("NOT_FOUND", &format!("todo {id} not found"), "Use todo_list.")
    })?;

    let removed = store.items.remove(idx);
    write_store(&store)?;
    Ok(json_ok(Value::String(format!(
        "Todo {} deleted: {}",
        removed.id, removed.title
    ))))
}

fn exec_todo_list(args: &Value) -> Result<String, String> {
    let store = read_store()?;

    let filter_status = args
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("");

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
            "No todos yet. Use todo_create(title=..., description=...) to create one."
                .to_string(),
        )));
    }

    let icon = |s: &TodoStatus| -> &str {
        match s {
            TodoStatus::Pending => "○",
            TodoStatus::InProgress => "●",
            TodoStatus::Completed => "✓",
            TodoStatus::Cancelled => "✗",
        }
    };

    let mut lines: Vec<String> = Vec::new();
    lines.push(format!("Todos ({}):", items.len()));
    for item in &items {
        let complexity_str = item
            .complexity
            .as_ref()
            .map(|c| format!("[{:?}] ", c).to_lowercase())
            .unwrap_or_default();
        lines.push(format!(
            "{} {} T{}: {}{}— {}",
            icon(&item.status),
            complexity_str,
            item.id,
            item.title,
            if item.description.is_empty() {
                ""
            } else {
                " "
            },
            item.description
        ));
    }

    Ok(json_ok(Value::String(lines.join("\n"))))
}

// ═══════════════════════════════════════════════════════
// Dispatcher
// ═══════════════════════════════════════════════════════

fn handle_todo(ctx: ToolCallCtx) -> ToolResult {
    let action = ctx.get_str("action").unwrap_or("list");
    match action {
        "create" => match exec_todo_create(&ctx.args) {
            Ok(content) => ToolResult::ok(content),
            Err(content) => ToolResult::error(content),
        },
        "update" => match exec_todo_update(&ctx.args) {
            Ok(content) => ToolResult::ok(content),
            Err(content) => ToolResult::error(content),
        },
        "delete" => match exec_todo_delete(&ctx.args) {
            Ok(content) => ToolResult::ok(content),
            Err(content) => ToolResult::error(content),
        },
        "list" => match exec_todo_list(&ctx.args) {
            Ok(content) => ToolResult::ok(content),
            Err(content) => ToolResult::error(content),
        },
        "submit" => match exec_todo_submit(&ctx.args) {
            Ok(content) => ToolResult::ok(content),
            Err(content) => ToolResult::error(content),
        },
        "step_complete" => match exec_todo_step_complete(&ctx.args) {
            Ok(content) => ToolResult::ok(content),
            Err(content) => ToolResult::error(content),
        },
        "stop" => match exec_todo_stop(&ctx.args) {
            Ok(content) => ToolResult::ok(content),
            Err(content) => ToolResult::error(content),
        },
        "import_plan" => match exec_todo_import_plan(&ctx.args) {
            Ok(content) => ToolResult::ok(content),
            Err(content) => ToolResult::error(content),
        },
        _ => ToolResult::error(format!(
            "Unknown todo action: {action}. Use create, update, delete, list, step_complete, stop, or import_plan."
        )),
    }
}

// ═══════════════════════════════════════════════════════
// Submit
// ═══════════════════════════════════════════════════════

fn exec_todo_submit(_args: &Value) -> Result<String, String> {
    let store = read_store()?;
    let items: Vec<serde_json::Value> = store.items.iter().map(|item| serde_json::json!({
        "id": item.id, "title": item.title, "description": item.description,
        "status": match item.status { TodoStatus::Pending=>"pending", TodoStatus::InProgress=>"in_progress", TodoStatus::Completed=>"completed", TodoStatus::Cancelled=>"cancelled" },
        "complexity": item.complexity.as_ref().map(|c| format!("{:?}", c).to_lowercase()),
        "effort_min": item.effort_min,
    })).collect();
    serde_json::to_string(&serde_json::json!({
        "items": items,
        "total": store.items.len(),
        "completed": store.items.iter().filter(|i| i.status == TodoStatus::Completed).count(),
        "goal": false
    })).map_err(|e| format!("todo submit: {e}"))
}

// ═══════════════════════════════════════════════════════
// Goal-mode tool handlers
// ═══════════════════════════════════════════════════════

pub fn exec_todo_activate(args: &Value) -> Result<String, String> {
    let _guard = TODO_LOCK.lock().map_err(|_| "todo lock poisoned".to_string())?;
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
            .map(|s| if s.starts_with('T') { s.to_string() } else { format!("T{s}") })
            .collect()
    });

    let active_items: Vec<TodoItem> = if let Some(ref ids) = ids {
        ids.iter().filter_map(|id| store.items.iter().find(|item| &item.id == id).cloned()).collect()
    } else {
        store.items.iter().filter(|item| matches!(item.status, TodoStatus::Pending | TodoStatus::InProgress)).cloned().collect()
    };

    if active_items.is_empty() {
        return Err(json_err("EMPTY_TODO", "no items to activate",
            if ids.is_some() { "Check the IDs — none matched items in the todo list." } else { "Use todo_create first, or specify ids." }));
    }

    let total = active_items.len();
    let mut sorted = active_items;
    sorted.sort_by_key(|item| match item.complexity { Some(TodoComplexity::Small) => 0, Some(TodoComplexity::Medium) => 1, Some(TodoComplexity::Large) => 2, None => 3 });
    let first_id = sorted[0].id.clone();
    let first_title = sorted[0].title.clone();

    store.items = sorted;
    store.mode = TodoMode::Goal;
    store.current_id = Some(first_id.clone());
    store.auto_turns = 0;
    write_store(&store)?;

    Ok(json_ok(Value::String(format!(
        "Goal activated with {total} items. Starting: T{first_id} {first_title} — complete this step then call todo_step_complete(id=\"{first_id}\", summary=\"...\")."
    ))))
}

fn exec_todo_step_complete(args: &Value) -> Result<String, String> {
    let _guard = TODO_LOCK.lock().map_err(|_| "todo lock poisoned".to_string())?;
    let mut store = read_store()?;

    if store.mode != TodoMode::Goal { return Err(json_err("NO_ACTIVE_GOAL", "no goal is active", "Use todo_activate first.")); }

    let id_raw = args.get("id").and_then(|v| v.as_str()).unwrap_or("");
    let id = if id_raw.starts_with('T') { id_raw.to_string() } else if let Ok(n) = id_raw.parse::<u32>() { format!("T{n}") } else { id_raw.to_string() };
    let summary = args.get("summary").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();

    if id.is_empty() || summary.is_empty() { return Err(json_err("MISSING_PARAM", "id and summary are required", "Use the current goal item ID and a concise summary.")); }

    let current_id = store.current_id.clone().unwrap_or_default();
    if id != current_id { return Err(json_err("OUT_OF_ORDER_STEP", &format!("cannot complete {id} before {current_id}"), "Complete only the current step, or stop with todo_stop.")); }

    if let Some(item) = store.items.iter_mut().find(|item| item.id == id) { item.status = TodoStatus::Completed; item.evidence = Some(summary.clone()); }

    let next = store.items.iter().find(|item| item.id != id && matches!(item.status, TodoStatus::Pending | TodoStatus::InProgress)).cloned();
    if let Some(ref next_item) = next {
        if let Some(item) = store.items.iter_mut().find(|item| item.id == next_item.id) { item.status = TodoStatus::InProgress; }
        store.current_id = Some(next_item.id.clone());
        store.auto_turns += 1;
        let done = store.items.iter().filter(|item| item.status == TodoStatus::Completed).count();
        write_store(&store)?;
        Ok(json_ok(Value::String(format!("{id} completed: {summary}. Next: T{} {} ({done}/{}) done.", next_item.id, next_item.title, store.items.len()))))
    } else {
        store.current_id = None; store.mode = TodoMode::Manual;
        write_store(&store)?;
        Ok(json_ok(Value::String(format!("{id} completed: {summary}. All items finished. Goal mode ended."))))
    }
}

fn exec_todo_stop(args: &Value) -> Result<String, String> {
    let _guard = TODO_LOCK.lock().map_err(|_| "todo lock poisoned".to_string())?;
    let mut store = read_store()?;
    if store.mode != TodoMode::Goal { return Err(json_err("NO_ACTIVE_GOAL", "no goal is active", "Use todo_activate first.")); }
    let reason = args.get("reason").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();

    if let Some(ref current_id) = store.current_id.clone() {
        if let Some(item) = store.items.iter_mut().find(|item| &item.id == current_id) {
            if item.status == TodoStatus::InProgress { item.status = TodoStatus::Pending; }
        }
    }
    store.mode = TodoMode::Manual; store.current_id = None;
    write_store(&store)?;
    Ok(json_ok(Value::String(format!("Goal stopped{}.", if reason.is_empty() { String::new() } else { format!(": {reason}") }))))
}

// ═══════════════════════════════════════════════════════
// PLAN.md import bridge
// ═══════════════════════════════════════════════════════

fn exec_todo_import_plan(_args: &Value) -> Result<String, String> {
    let _guard = TODO_LOCK.lock().map_err(|_| "todo lock poisoned".to_string())?;
    let mut store = read_store()?;
    let plan_content = crate::plan::read_plan().map_err(|e| format!("read PLAN.md: {e}"))?;
    if plan_content.trim().is_empty() {
        return Ok(json_ok(Value::String("PLAN.md is empty. Use plan_create first.".to_string())));
    }
    let items = crate::plan::parse_plan_items(&plan_content);
    if items.is_empty() {
        return Ok(json_ok(Value::String("No non-rejected items found in PLAN.md.".to_string())));
    }

    let next = next_id(&store.items);
    let mut count = 0u32;
    for (_i, item) in items.into_iter().enumerate() {
        if item.status == "rejected" {
            continue;
        }
        let id = format!("T{}", next + count);
        store.items.push(TodoItem {
            id: id.clone(),
            title: item.title,
            description: item.description,
            status: TodoStatus::Pending,
            complexity: None,
            deps: item.deps.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
            effort_min: item.effort.parse().ok(),
            evidence: None,
        });
        count += 1;
    }
    write_store(&store)?;
    Ok(json_ok(Value::String(format!("Imported {count} items from PLAN.md to todo.json."))))
}

// ═══════════════════════════════════════════════════════
// Registration
// ═══════════════════════════════════════════════════════

use crate::{ToolHandler, ToolRisk};
use std::time::Duration;

pub fn register(mgr: &mut crate::ToolManager) {
    mgr.register(ToolHandler {
        key: "todo".to_string(),
        description: "Create, manage, and track todos via action: create, update, delete, list, step_complete, stop, or import_plan. Supports complexity labels, dependencies, effort estimates, goal step completion, and PLAN.md import. Note: goal activation is user-only, not available via this tool.",
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["create", "update", "delete", "list", "submit", "step_complete", "stop", "import_plan"], "description": "Operation to perform"},
                "id": {"type": ["string", "integer"], "description": "Todo ID for update/delete/step_complete"},
                "status": {"type": "string", "enum": ["pending", "in_progress", "completed", "cancelled"], "description": "New status for update"},
                "title": {"type": "string", "description": "Todo title, 1-100 chars (for create/update)"},
                "description": {"type": "string", "description": "Todo description, 1-200 chars (for create/update)"},
                "complexity": {"type": "string", "enum": ["small", "medium", "large"], "description": "Task complexity"},
                "deps": {"type": "array", "items": {"type": "string"}, "description": "Dependent todo IDs (for create/update)"},
                "effort_min": {"type": "integer", "description": "Estimated effort in minutes (for create/update)"},
                "evidence": {"type": "string", "description": "Completion evidence (for update when status=completed)"},
                "summary": {"type": "string", "description": "Completion summary (for step_complete)"},
                "reason": {"type": "string", "description": "Why the goal was stopped (for stop)"}
            },
            "required": ["action"], "additionalProperties": false
        }),
        handler: handle_todo, risk: ToolRisk::Write, default_timeout: Duration::from_secs(15),
    });
}
