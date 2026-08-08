//! Session-scoped todo management.
//!
//! Persisted to `sessions/{seed}/todo.json` (session-scoped).
//! The public model contract supports create, status update, cancel, and list.
//!
//! Data model:
//! ```json
//! {
//!   "items": [
//!     {"id":"T1","title":"...","description":"...","status":"pending","evidence":null}
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
    if store.current_id.as_deref() == Some(id) {
        store.current_id = None;
    }

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

    let id = format!("T{}", next_id(&store.items));
    let item = TodoItem {
        id: id.clone(),
        title,
        description,
        status: TodoStatus::Pending,
        evidence: None,
    };
    store.items.push(item.clone());
    write_store(&store)?;
    Ok(json_ok(serde_json::json!({
        "item": todo_item_json(&item),
        "message": format!("Todo {} created.", item.id)
    })))
}

/// 单次 create_batch 的 items 上限（防滥用；模型单轮计划任务通常 ≤ 10）。
const BATCH_CREATE_MAX_ITEMS: usize = 20;

/// 批量创建：一条命令串行创建一群 todo。
///
/// 与多次并行 `create` 的关键差异（解决编号排序错误）：
/// - 单次 `TODO_LOCK` 内完成 read → 分配 → write，无 read-modify-write 交错窗口；
/// - 基于**一次快照**连续分配编号 T{n}, T{n+1}, …，编号必然连续、无重复、无跳号；
/// - 全量校验先行：任一 title/description 非法 → 整体失败、零写入（原子性），
///   模型修正后重试不会产生半批残留。
fn exec_todo_create_batch(args: &Value) -> Result<String, String> {
    let _guard = TODO_LOCK
        .lock()
        .map_err(|_| "todo lock poisoned".to_string())?;

    let items_arg = args
        .get("items")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            json_err(
                "INVALID_INPUT",
                "items array is required for create_batch",
                "Provide items: [{\"title\": \"...\", \"description\": \"...\"}, ...]",
            )
        })?;
    if items_arg.is_empty() {
        return Err(json_err(
            "INVALID_INPUT",
            "items must not be empty",
            "Provide at least one {title, description?} entry.",
        ));
    }
    if items_arg.len() > BATCH_CREATE_MAX_ITEMS {
        return Err(json_err(
            "INVALID_INPUT",
            &format!("items max {BATCH_CREATE_MAX_ITEMS} entries per call"),
            "Split the list into multiple create_batch calls.",
        ));
    }

    // 全量校验先行：任一非法 → 整体失败，零写入。
    let mut pending: Vec<(String, String)> = Vec::with_capacity(items_arg.len());
    for (index, entry) in items_arg.iter().enumerate() {
        let title = entry
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if title.is_empty() || title.chars().count() > 100 {
            return Err(json_err(
                "INVALID_INPUT",
                &format!("items[{index}].title must be 1-100 chars"),
                "Keep titles short and imperative, e.g. 'Add login API'",
            ));
        }
        let description = entry
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if description.chars().count() > 200 {
            return Err(json_err(
                "INVALID_INPUT",
                &format!("items[{index}].description max 200 chars"),
                "",
            ));
        }
        pending.push((title, description));
    }

    let mut store = read_store()?;
    // 一次快照内连续分配：base 之后逐个 +1，保证无重复、无跳号。
    let mut next = next_id(&store.items);
    let mut created = Vec::with_capacity(pending.len());
    for (title, description) in pending {
        let item = TodoItem {
            id: format!("T{next}"),
            title,
            description,
            status: TodoStatus::Pending,
            evidence: None,
        };
        next += 1;
        created.push(item);
    }
    store.items.extend(created.iter().cloned());
    write_store(&store)?;

    Ok(json_ok(serde_json::json!({
        "created": created.iter().map(todo_item_json).collect::<Vec<_>>(),
        "count": created.len(),
        "message": format!(
            "Created {} todos: {}",
            created.len(),
            created.iter().map(|item| item.id.as_str()).collect::<Vec<_>>().join(", ")
        ),
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
                "Use task(action=\"list\") to see all IDs.",
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
            "No tasks yet. Use task(action=\"create\", title=..., description=...) to create one."
                .to_string(),
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

fn handle_task(ctx: ToolCallCtx) -> ToolResult {
    let action = ctx
        .args
        .get("action")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let result = match action {
        "create" => exec_todo_create(&ctx.args),
        "create_batch" => exec_todo_create_batch(&ctx.args),
        "update" => exec_todo_update(&ctx.args),
        "cancel" => exec_todo_cancel(&ctx.args),
        "list" => exec_todo_list(&ctx.args),
        _ => {
            return ToolResult::error(
                "task.action must be create, create_batch, update, cancel, or list",
            );
        }
    };
    tool_result(result)
}

// ═══════════════════════════════════════════════════════
// Registration
// ═══════════════════════════════════════════════════════

use crate::{ToolHandler, ToolRisk};
use std::time::Duration;

pub fn register(mgr: &mut crate::ToolManager) {
    // 主工具：todo（prompt/文档统一用 todo 命名）。
    mgr.register_with_placement(
        ToolHandler {
            key: "todo".to_string(),
            description: "Manage the session task list through one typed interface. USE create_batch (not multiple parallel create calls) when creating a GROUP of tasks: one command creates them atomically with consecutive IDs T{n}, T{n+1}, ... inside a single locked read-modify-write — parallel-safe, never duplicates or reorders numbering. Example: todo(action=\"create_batch\", items=[{\"title\": \"实现登录\", \"description\": \"...\"}, {\"title\": \"写测试\"}]). Use create for a single task (todo(action=\"create\", title=\"...\")), update/cancel by id, list to view. task state is internal session state and does not grant file permissions.",
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["create", "create_batch", "update", "cancel", "list"]},
                    "title": {
                        "type": "string",
                        "description": "Short imperative title, 1-100 characters. Format: '[动作] [对象]'. Examples: '实现 JWT 刷新', '修复搜索框卡顿', '编写 API 文档'. Required for action=create."
                    },
                    "description": {
                        "type": "string",
                        "description": "Optional context or acceptance criteria, at most 200 characters."
                    },
                    "items": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "title": {"type": "string", "description": "Short imperative title, 1-100 characters."},
                                "description": {"type": "string", "description": "Optional context or acceptance criteria, at most 200 characters."}
                            },
                            "required": ["title"]
                        },
                        "description": "Required for action=create_batch. The group of tasks to create atomically in ONE command — do NOT fire parallel create calls. IDs are assigned consecutively (T{n}, T{n+1}, ...) inside one locked read-modify-write. All-or-nothing validation (any invalid entry rejects the whole batch with zero writes). Max 20 entries. Example: [{\"title\": \"实现登录 API\"}, {\"title\": \"修复搜索卡顿\", \"description\": \"...\"}]."
                    },
                    "id": {"type": ["string", "integer"], "description": "Task id (e.g. \"T1\" or 1). Omit for action=create/create_batch (new ids are generated); pass the id returned by create/list for action=update or cancel."},
                    "status": {"type": "string", "enum": ["pending", "in_progress", "completed", "cancelled"]},
                    "evidence": {"type": "string"}
                },
                "required": ["action"],
                "additionalProperties": false
            }),
            handler: handle_task,
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
    fn batch_create_assigns_consecutive_ids() {
        with_isolated_todo(|_seed| {
            // 已有 T1（单 create）→ batch 3 个必须为 T2,T3,T4
            exec_todo_create(&serde_json::json!({"title": "single"})).unwrap();
            let result = exec_todo_create_batch(&serde_json::json!({
                "items": [
                    {"title": "a"},
                    {"title": "b", "description": "desc b"},
                    {"title": "c"},
                ]
            }));
            let value = parse(&result);
            let created = value["created"].as_array().unwrap();
            assert_eq!(created.len(), 3);
            let got: Vec<&str> = created
                .iter()
                .map(|item| item["id"].as_str().unwrap())
                .collect();
            assert_eq!(got, ["T2", "T3", "T4"]);
            assert_eq!(value["count"], 3);
            let store = read_store().unwrap();
            assert_eq!(ids(&store), ["T1", "T2", "T3", "T4"]);
        });
    }

    #[test]
    fn batch_create_is_atomic_on_validation_failure() {
        with_isolated_todo(|_seed| {
            let result = exec_todo_create_batch(&serde_json::json!({
                "items": [
                    {"title": "good"},
                    {"title": "   "}, // 空 title → 全批失败
                ]
            }));
            assert!(result.is_err());
            let err: serde_json::Value =
                serde_json::from_str(result.unwrap_err().as_str()).unwrap();
            assert_eq!(err["code"], "INVALID_INPUT");
            // 零写入：store 文件不存在或为空
            let store = read_store().unwrap();
            assert!(store.items.is_empty());
        });
    }

    #[test]
    fn batch_create_rejects_empty_and_oversized() {
        with_isolated_todo(|_seed| {
            let empty = exec_todo_create_batch(&serde_json::json!({"items": []}));
            assert!(empty.is_err());
            let items: Vec<serde_json::Value> = (0..21)
                .map(|index| serde_json::json!({"title": format!("t{index}")}))
                .collect();
            let oversized = exec_todo_create_batch(&serde_json::json!({"items": items}));
            assert!(oversized.is_err());
            let store = read_store().unwrap();
            assert!(store.items.is_empty());
        });
    }

    #[test]
    fn batch_create_keeps_numbering_consistent_after_cancel() {
        with_isolated_todo(|_seed| {
            exec_todo_create(&serde_json::json!({"title": "t1"})).unwrap();
            exec_todo_cancel(&serde_json::json!({"id": "T1"})).unwrap();
            let result = exec_todo_create_batch(&serde_json::json!({
                "items": [{"title": "x"}, {"title": "y"}]
            }));
            let value = parse(&result);
            let got: Vec<&str> = value["created"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| item["id"].as_str().unwrap())
                .collect();
            // max+1 语义：即使 T1 已取消，新编号从 T2 继续，无重复
            assert_eq!(got, ["T2", "T3"]);
        });
    }

    #[test]
    fn batch_and_single_create_interleave_without_duplicates() {
        with_isolated_todo(|_seed| {
            let r1 = exec_todo_create_batch(&serde_json::json!({
                "items": [{"title": "a"}, {"title": "b"}]
            }))
            .unwrap();
            let v1: serde_json::Value = serde_json::from_str(&r1).unwrap();
            let ids1: Vec<&str> = v1["created"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| item["id"].as_str().unwrap())
                .collect();
            assert_eq!(ids1, ["T1", "T2"]);
            exec_todo_create(&serde_json::json!({"title": "c"})).unwrap();
            let r2 = exec_todo_create_batch(&serde_json::json!({
                "items": [{"title": "d"}]
            }))
            .unwrap();
            let v2: serde_json::Value = serde_json::from_str(&r2).unwrap();
            assert_eq!(v2["created"][0]["id"], "T4");
            let store = read_store().unwrap();
            assert_eq!(ids(&store), ["T1", "T2", "T3", "T4"]);
        });
    }
}
