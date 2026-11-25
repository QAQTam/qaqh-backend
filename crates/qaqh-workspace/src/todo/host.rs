//! `TodoHost` 端口——注册期显式注入（v4.5 P2-1b）。
//!
//! todo 的状态权威在 `qaqh.db` 的 `session_todos`/`session_todo_state` 两张表;
//! 表访问不能落在 workspace(rusqlite 只在 `qaqh-session` 声明,且 workspace
//! 下游编译图拖不起)。因此本模块只定义端口:实现由 `qaqh-runtime` 为
//! `QaqhService` 提供,在装配 `ToolManager` / service 直访路径上**显式传入**
//! `Arc<dyn TodoHost>`——进程级全局槽已物理删除,**未注入即无法构造,不
//! 回退文件路径**(宪法 #5)。**同一进程可并存多套宿主实例,互不串台**
//! (反证:`crates/qaqh-agent-runtime/tests/todo_host_isolation.rs`)。
//!
//! 原子性判据:所有读-改-写经 [`TodoHost::mutate_todo`] 在宿主侧**单个写
//! 事务**内完成(写执行器串行化)——这是旧 `TODO_LOCK` 语义的替代者,
//! 常驻用例见 `qaqh-session/tests/todos_atomicity.rs`。

use std::collections::HashMap;
use std::sync::Mutex;

use super::model::{TodoMode, TodoStore};

/// [`TodoHost::mutate_todo`] 的闭包类型（宿主侧在单写事务内调用）。
pub type TodoMutation =
    Box<dyn FnOnce(&mut TodoStore) -> Result<serde_json::Value, String> + Send>;

pub trait TodoHost: Send + Sync {
    /// 只读加载。`None` = 从未建过清单(旧「文件不存在」语义,
    /// `todo.status` 查询面对前端回 Null 的判据)。
    fn load_todo(&self, session_id: &str) -> Result<Option<TodoStore>, String>;

    /// 单事务读-改-写:宿主加载当前清单(无记录 = 空 + 默认状态),跑 `f`,
    /// `Ok` 落库并透传返回值,`Err` 整体回滚不落库。
    fn mutate_todo(
        &self,
        session_id: &str,
        f: TodoMutation,
    ) -> Result<serde_json::Value, String>;

    /// 条件整写(仅 `todo_meta_snapshot` 的归档/cleared 置位分支用)。
    fn save_todo(&self, session_id: &str, store: &TodoStore) -> Result<(), String>;
}

/// 与旧 `read_store_for` 的「文件缺失 = 空清单」分支逐字段一致
/// (注意 `max_auto_turns` 的 serde 默认是 24,不是 `Default` 的 0)。
pub(crate) fn empty_store() -> TodoStore {
    TodoStore {
        items: Vec::new(),
        mode: TodoMode::Manual,
        current_id: None,
        auto_turns: 0,
        max_auto_turns: 24,
        archived: false,
        cleared_notified: false,
    }
}

// ═══════════════════════════════════════════════════════
// 测试装置(公共,供跨 crate 夹具构造;生产装配只走真实 DB 宿主)
// ═══════════════════════════════════════════════════════

/// 内存宿主:单锁 RMW(原子性语义与 SQLite 串行写等价,可按会话并发)。
#[derive(Default)]
pub struct MemoryTodoHost {
    stores: Mutex<HashMap<String, TodoStore>>,
}

impl TodoHost for MemoryTodoHost {
    fn load_todo(&self, session_id: &str) -> Result<Option<TodoStore>, String> {
        Ok(self
            .stores
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(session_id)
            .cloned())
    }

    fn mutate_todo(
        &self,
        session_id: &str,
        f: Box<dyn FnOnce(&mut TodoStore) -> Result<serde_json::Value, String> + Send>,
    ) -> Result<serde_json::Value, String> {
        let mut stores = self.stores.lock().unwrap_or_else(|e| e.into_inner());
        let mut store = stores
            .get(session_id)
            .cloned()
            .unwrap_or_else(empty_store);
        let outcome = f(&mut store)?;
        stores.insert(session_id.to_string(), store);
        Ok(outcome)
    }

    fn save_todo(&self, session_id: &str, store: &TodoStore) -> Result<(), String> {
        self.stores
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session_id.to_string(), store.clone());
        Ok(())
    }
}
