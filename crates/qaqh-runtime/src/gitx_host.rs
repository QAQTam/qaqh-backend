//! `GitxHost` 的进程内实现（ADR-0033 切片 2）。
//!
//! 与 `goal_host` / `host_impl` 同款先例：模型工具 handler 经宿主句柄直达
//! 进程内状态，不建立 HTTP 回环。worktree 工具只需要一件事——把会话映射到
//! 其**源工作区**根（注册表所在地），复用 registry 的 `session_cwd` 单一路径，
//! 与服务面写面作用域解析（`handlers/gitx.rs::source_workspace`）同源：
//! 空 cwd / 目录已消失一律 `None`，由工具层报 `workspace_unbound`。

use crate::registry::spawn::session_cwd;

impl qaqh_gitx::GitxHost for crate::service::QaqhService {
    fn source_workspace(&self, session_id: &str) -> Option<std::path::PathBuf> {
        session_cwd(&self.owners, session_id)
            .filter(|cwd| !cwd.is_empty() && std::path::Path::new(cwd).is_dir())
            .map(std::path::PathBuf::from)
    }
}
