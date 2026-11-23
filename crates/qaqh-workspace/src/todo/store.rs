//! todo::store — 查询与服务面 JSON（status/cancel/meta）。
//!
//! v4.4 P1-3：持久化全部经 [`super::host::TodoHost`]——`todo.json` 的
//! 读写函数与 `TODO_LOCK` 已物理删除，运行期不存在第二条读写路径。

use crate::json_err_string;

use super::host::{empty_store, require_host};
use super::model::{TodoItem, TodoStatus, TodoStore};

// ═══════════════════════════════════════════════════════
// Persistence via the TodoHost port
// ═══════════════════════════════════════════════════════

/// Session-aware variant: load TodoStore for an explicit seed (no RUNTIME_CTX).
pub fn load_todo_for(session_id: &str) -> Result<TodoStore, String> {
    if session_id.is_empty() {
        return Err("no active session".into());
    }
    Ok(require_host()?
        .load_todo(session_id)?
        .unwrap_or_else(empty_store))
}

/// Session-scoped todo status for the frontend Todo panel.
pub fn todo_status_value(session_id: &str) -> Result<serde_json::Value, String> {
    if session_id.is_empty() {
        return Ok(serde_json::Value::Null);
    }
    // 无记录 = 旧「文件不存在」：对前端面板保持 Null。
    let Some(store) = require_host()?.load_todo(session_id)? else {
        return Ok(serde_json::Value::Null);
    };
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
    Ok(serde_json::json!({
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
}

pub fn todo_status_json(session_id: &str) -> Result<String, String> {
    serde_json::to_string(&todo_status_value(session_id)?).map_err(|e| format!("todo: {e}"))
}

/// Direct cancel by session seed — no runtime context needed.
pub fn todo_cancel_value(session_id: &str, id: &str) -> Result<serde_json::Value, String> {
    if session_id.is_empty() {
        return Err(json_err_string("invalid_input", "no active session", ""));
    }
    let host = require_host()?;
    if host.load_todo(session_id)?.is_none() {
        return Err(json_err_string(
            "not_found",
            "no todo list for this session",
            "",
        ));
    }
    // 读-改-写落在宿主的同一个写事务里（取代旧 TODO_LOCK 串行）。
    let id = id.to_string();
    host.mutate_todo(
        session_id,
        Box::new(move |store| {
            let idx = store
                .items
                .iter()
                .position(|item| item.id == id)
                .ok_or_else(|| {
                    json_err_string(
                        "not_found",
                        format!("todo {id} not found"),
                        "Use the todo.list service method to see all IDs.",
                    )
                })?;
            store.items[idx].status = TodoStatus::Cancelled;
            normalize_current_id(store);
            let item_json = todo_item_json(&store.items[idx]);
            Ok(serde_json::json!({
                "timeis": crate::now_utc8(),
                "status": "ok",
                "item": item_json,
                "message": format!("Todo {id} cancelled.")
            }))
        }),
    )
}

pub fn todo_cancel_json(session_id: &str, id: &str) -> Result<String, String> {
    serde_json::to_string(&todo_cancel_value(session_id, id)?).map_err(|e| format!("todo: {e}"))
}

pub(crate) fn status_name(status: &TodoStatus) -> &'static str {
    match status {
        TodoStatus::Pending => "pending",
        TodoStatus::InProgress => "in_progress",
        TodoStatus::Completed => "completed",
        TodoStatus::Cancelled => "cancelled",
    }
}

pub(crate) fn count_status(store: &TodoStore, status: TodoStatus) -> usize {
    store
        .items
        .iter()
        .filter(|item| item.status == status)
        .count()
}

pub(crate) fn todo_item_json(item: &TodoItem) -> serde_json::Value {
    serde_json::json!({
        "id": item.id,
        "title": item.title,
        "description": item.description,
        "status": status_name(&item.status),
        "evidence": item.evidence,
    })
}

pub(crate) fn normalize_current_id(store: &mut TodoStore) {
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

/// 维护归档位：全部条目进入终态（completed/cancelled）→ `archived=true`；
/// 存在活跃条目（或空列表）→ `false`。write 重建 / update 翻转后调用。
/// 允许并行 in_progress：归档判定只看"是否还有非终态条目"，不限制数量。
pub(crate) fn refresh_archive_flag(store: &mut TodoStore) {
    store.archived = !store.items.is_empty()
        && store
            .items
            .iter()
            .all(|item| matches!(item.status, TodoStatus::Completed | TodoStatus::Cancelled));
}

/// 工具结果 meta 快照（状态机收口，runtime 每次非 todo 工具调用时求值）。
///
/// - 无列表 / 已归档 → `None`（零 token，不注入）；
/// - 全部终态 → 置归档位并回一次 `[todos] all done`（"下一次工具请求时
///   置空"），之后归档位挡住后续注入；
/// - 有活跃条目 → 单行紧凑式 `{done}/{total} done · now: <title>`，只有
///   正在工作的条目；完整列表由 todo_write/todo_update 自身的 output 携带。
///
/// **快照即冻结**：调用方把返回值写进 tool_result 后不得再改——历史
/// append-only 是 KV cache 前缀命中的前提。
pub fn todo_meta_snapshot(session_id: &str) -> Option<String> {
    if session_id.is_empty() {
        return None;
    }
    // 快照注入是尽力而为的旁路：宿主缺失/读失败都归入「不注入」，
    // 与原 read_store_for 失败路径同律（不伪装成功也不拖崩回合）。
    let host = require_host().ok()?;
    let mut store = host.load_todo(session_id).ok()??;
    if store.items.is_empty() {
        return None;
    }
    // 归档后的 cleared 提示：只发一次（"下一次工具请求时置空"），随后静默。
    if store.archived {
        if store.cleared_notified {
            return None;
        }
        store.cleared_notified = true;
        let _ = host.save_todo(session_id, &store);
        return Some("[todos] all tasks done — list cleared.".to_string());
    }
    // 兜底：write/update 都会维护 archived，这里覆盖直接写路径未触达的情形。
    let total = store.items.len();
    let done = store
        .items
        .iter()
        .filter(|item| matches!(item.status, TodoStatus::Completed | TodoStatus::Cancelled))
        .count();
    if done == total {
        store.archived = true;
        let _ = host.save_todo(session_id, &store);
        return Some("[todos] all tasks done — list cleared.".to_string());
    }
    // 并行工作：取第一个 in_progress；无则取第一个 pending（提示下一步）。
    let current = store
        .items
        .iter()
        .find(|item| item.status == TodoStatus::InProgress)
        .or_else(|| {
            store
                .items
                .iter()
                .find(|item| item.status == TodoStatus::Pending)
        })?;
    Some(format!(
        "[todos] {done}/{total} done · now: {} ({})",
        current.title,
        status_name(&current.status)
    ))
}
