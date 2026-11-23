//! `TodoHost` 端口的进程内实现(v4.4 P1-3,MUST 铁闸 2 收编后的唯一运行路径)。
//!
//! GoalHost/Subagent/Gitx 同款先例:daemon 装配时安装,工具与 service 面经
//! 端口直达 `qaqh.db`;未安装即报错,不回退文件路径。原子性判据:所有
//! 读-改-写落在 `qaqh_session::db::todos::mutate_todo` 的单个写事务内
//! (常驻用例 `qaqh-session/tests/todos_atomicity.rs`)。

use serde_json::Value;

use qaqh_session::db::todos::{self, TodoRow, TodoStateRow};
use qaqh_session::db::DbError;
use qaqh_workspace::todo::{TodoHost, TodoItem, TodoMode, TodoStatus, TodoStore};

use crate::service::QaqhService;

fn status_to_str(status: &TodoStatus) -> &'static str {
    match status {
        TodoStatus::Pending => "pending",
        TodoStatus::InProgress => "in_progress",
        TodoStatus::Completed => "completed",
        TodoStatus::Cancelled => "cancelled",
    }
}

fn status_from_str(raw: &str) -> Result<TodoStatus, String> {
    match raw {
        "pending" => Ok(TodoStatus::Pending),
        "in_progress" => Ok(TodoStatus::InProgress),
        "completed" => Ok(TodoStatus::Completed),
        "cancelled" => Ok(TodoStatus::Cancelled),
        other => Err(format!("corrupt todo status {other:?}")),
    }
}

fn mode_to_str(mode: &TodoMode) -> &'static str {
    match mode {
        TodoMode::Manual => "manual",
        TodoMode::Goal => "goal",
    }
}

fn mode_from_str(raw: &str) -> Result<TodoMode, String> {
    match raw {
        "manual" => Ok(TodoMode::Manual),
        "goal" => Ok(TodoMode::Goal),
        other => Err(format!("corrupt todo mode {other:?}")),
    }
}

fn to_store(items: &[TodoRow], state: &TodoStateRow) -> Result<TodoStore, String> {
    Ok(TodoStore {
        items: items
            .iter()
            .map(|row| {
                Ok(TodoItem {
                    id: row.todo_id.clone(),
                    title: row.title.clone(),
                    description: row.description.clone(),
                    status: status_from_str(&row.status)?,
                    evidence: row.evidence.clone(),
                })
            })
            .collect::<Result<Vec<_>, String>>()?,
        mode: mode_from_str(&state.mode)?,
        current_id: state.current_id.clone(),
        auto_turns: u32::try_from(state.auto_turns).map_err(|_| "corrupt auto_turns")?,
        max_auto_turns: u32::try_from(state.max_auto_turns).map_err(|_| "corrupt max_auto_turns")?,
        archived: state.archived,
        cleared_notified: state.cleared_notified,
    })
}

fn from_store(store: &TodoStore) -> (Vec<TodoRow>, TodoStateRow) {
    (
        store
            .items
            .iter()
            .map(|item| TodoRow {
                todo_id: item.id.clone(),
                title: item.title.clone(),
                description: item.description.clone(),
                status: status_to_str(&item.status).to_string(),
                evidence: item.evidence.clone(),
            })
            .collect(),
        TodoStateRow {
            mode: mode_to_str(&store.mode).to_string(),
            current_id: store.current_id.clone(),
            auto_turns: store.auto_turns as i64,
            max_auto_turns: store.max_auto_turns as i64,
            archived: store.archived,
            cleared_notified: store.cleared_notified,
        },
    )
}

fn unwrap_workspace_error(error: DbError) -> String {
    match error {
        // 闭包内业务错误(cancel/update 的 json_err 信封)原样透传,
        // 不得套一层数据库文案。
        DbError::Workspace(message) => message,
        other => other.to_string(),
    }
}

impl TodoHost for QaqhService {
    fn load_todo(&self, session_id: &str) -> Result<Option<TodoStore>, String> {
        let loaded = todos::load_todo(self.owners().db(), session_id)
            .map_err(|error| error.to_string())?;
        match loaded {
            None => Ok(None),
            Some((items, state)) => to_store(&items, &state).map(Some),
        }
    }

    fn mutate_todo(
        &self,
        session_id: &str,
        f: Box<dyn FnOnce(&mut TodoStore) -> Result<Value, String> + Send>,
    ) -> Result<Value, String> {
        todos::mutate_todo(self.owners().db(), session_id, move |rows, state| {
            let mut store = to_store(rows, state)?;
            let outcome = f(&mut store)?;
            let (next_rows, next_state) = from_store(&store);
            *rows = next_rows;
            *state = next_state;
            Ok(outcome)
        })
        .map_err(unwrap_workspace_error)
    }

    fn save_todo(&self, session_id: &str, store: &TodoStore) -> Result<(), String> {
        let (rows, state) = from_store(store);
        todos::mutate_todo(
            self.owners().db(),
            session_id,
            move |cur_rows, cur_state| {
                *cur_rows = rows;
                *cur_state = state;
                Ok(())
            },
        )
        .map_err(unwrap_workspace_error)
    }
}
