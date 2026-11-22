//! gitx 错误词表：全部可判定、可序列化进工具输出的失败形态。
//!
//! 设计纪律（对齐 qaqh-sandbox 的 fail-closed 审计口径）：错误只描述"发生了什么"，
//! 不描述"该做什么"——恢复动作（force/换会话/换分支）由调用方与审批面决定。

/// gitx 域统一错误。
#[derive(Debug, thiserror::Error)]
pub enum GitxError {
    /// git 子进程以非零码退出。`command` 为去掉了全局参数的 argv 片段。
    #[error("git command failed (`{command}`): {stderr}")]
    Git { command: String, stderr: String },
    /// PATH 上找不到 git 可执行文件。
    #[error("git executable not found on PATH")]
    GitUnavailable,
    /// 文件系统错误。
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// 注册表 JSON 读写错误。
    #[error("registry json: {0}")]
    Json(#[from] serde_json::Error),
    /// 给定目录不在 git 仓库内。
    #[error("not a git repository: {0}")]
    NotARepository(String),
    /// branch 名未通过 `git check-ref-format`。
    #[error("invalid branch name `{0}`")]
    InvalidBranch(String),
    /// 目标 worktree 有未提交变更，未给 force 时拒绝删除。
    #[error("worktree `{0}` is dirty; pass force to remove it anyway")]
    DirtyWorktree(String),
    /// id 不在本 source_root 的注册表里。
    #[error("worktree `{0}` is not managed by this registry")]
    NotManaged(String),
    /// worktree 已被其他会话占用（原子 CAS 失败）。
    #[error("worktree `{0}` is already bound to session `{1}`")]
    OwnerConflict(String, String),
    /// 释放/删除的请求者不是当前 owner。
    #[error("session `{1}` does not own worktree `{0}`")]
    NotOwner(String, String),
}

/// gitx 域统一 Result。
pub type Result<T> = std::result::Result<T, GitxError>;
