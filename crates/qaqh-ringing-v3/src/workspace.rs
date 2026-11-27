//! Workspace 域强类型 RPC（会话工作区分组、创建、重命名、删除与归属迁移）。

use qaqh_core_vocab::{SessionId, WorkspaceId};
use serde::{Deserialize, Serialize};

/// 工作区分组条目。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceEntry {
    /// 工作区 id。
    pub id: WorkspaceId,
    /// 工作区磁盘路径。
    pub path: String,
    /// 展示标题。
    pub title: String,
    /// 侧栏排序权值。
    pub order: u32,
    /// 归属于该工作区的会话 id 列表。
    pub session_ids: Vec<SessionId>,
    /// 磁盘目录当前是否缺失。
    pub missing_dir: bool,
}

/// Workspace 域请求。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkspaceRequest {
    /// 列出全部工作区。
    List,
    /// 注册/新建工作区。
    Create {
        /// 目标路径。
        path: String,
    },
    /// 重命名工作区。
    Rename {
        /// 工作区 id。
        workspace_id: WorkspaceId,
        /// 新标题。
        title: String,
    },
    /// 删除工作区（会话迁出行为由服务端控制）。
    Delete {
        /// 工作区 id。
        workspace_id: WorkspaceId,
    },
    /// 把指定会话移入目标工作区。
    MoveSession {
        /// 会话 id。
        session_id: SessionId,
        /// 目标工作区 id。
        workspace_id: WorkspaceId,
    },
}

/// Workspace 域回执。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkspaceResponse {
    /// 工作区列表。
    List(Vec<WorkspaceEntry>),
    /// 已新建的工作区条目。
    Created(WorkspaceEntry),
    /// 变更受理。
    Ack,
}
