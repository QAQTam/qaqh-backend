//! gitx 契约：worktree 读模型与命令参数（ADR-0041）。
//!
//! wire schema `qaqh.ringing.gitx/v1`（判别惯例对齐 `TEAM_SCHEMA`）。字段风格
//! 与 ringing 契约一致（snake_case、无 rename_all）——与 `qaqh-profile` wire DTO 层的
//! camelCase K2 域是不同域，不混用。

use serde::{Deserialize, Serialize};

/// `/gitx` 面 schema 判别值（契约锁定：客户端按字面量校验）。
pub const GITX_SCHEMA: &str = "qaqh.ringing.gitx/v1";

/// 一条 managed worktree 的读模型（`worktree.list` 条目）。
///
/// `missing_dir` 对齐 `WorkspaceEntry` 惯例：注册表里登记了但盘上已不在
/// （手工删除/清理工具扫掉），前端应展示为失效态并引导 prune。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct WorktreeView {
    pub schema: String,
    pub id: String,
    /// 完整 branch 名（不含 `refs/heads/` 前缀）。
    pub branch: String,
    /// 目录派生名（不含 hash 后缀）；目录名 = `{slug}-{hash4}`。
    pub slug: String,
    /// worktree 绝对路径。
    pub path: String,
    /// 源工作区根（`.qaqh` 工件锚点，ADR-0041 决策 3 双锚点）。
    pub source_root: String,
    pub head_sha: String,
    /// 占用会话；`None` = 空闲可绑定。
    pub owner_session_id: Option<String>,
    pub created_at_ms: i64,
    pub missing_dir: bool,
}

/// `git.snapshot` 读模型：源工作区的 branch/head/dirty。
///
/// dirty 语义 = `git status --porcelain` 非空（含未跟踪）；worktree 自身的
/// dirty 不在这里——worktree 视图以其 `path` 为根另行采集。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct GitSnapshot {
    pub schema: String,
    pub source_root: String,
    /// `None` = detached HEAD 或无提交。
    pub branch: Option<String>,
    pub head_sha: Option<String>,
    pub detached: bool,
    pub dirty: bool,
}

/// `gitx.status` 读模型：源工作区的完整 git 状态（桌面壳消费面）。
///
/// 与模型侧 [`GitSnapshot`] 分层：逐文件/同步态/未推送只进前端,不进模型
/// 上下文。dirty 语义 = `files` 非空（porcelain 含未跟踪）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct GitStatusSnapshot {
    pub schema: String,
    pub source_root: String,
    /// `None` = detached HEAD 或无提交。
    pub branch: Option<String>,
    pub head_sha: Option<String>,
    pub detached: bool,
    /// 上游分支名;`None` = 没有跟踪,无从比较。
    pub upstream: Option<String>,
    pub ahead: Option<u32>,
    pub behind: Option<u32>,
    /// porcelain v2 逐文件 XY 双码(index=暂存态,worktree=工作区态,"?"=未跟踪)。
    pub files: Vec<GitFileStatus>,
    /// 上游存在且 ahead>0 时的未推送提交(最新在前,上限 50)。
    pub unpushed: Vec<GitUnpushedCommit>,
}

/// porcelain v2 的 XY 双码：index = 暂存区态,worktree = 工作区态;未跟踪为 "?"。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct GitFileStatus {
    pub path: String,
    pub index: String,
    pub worktree: String,
}

/// 一条未推送提交(`%h%x09%s`)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct GitUnpushedCommit {
    pub hash: String,
    pub subject: String,
}

/// `worktree.create` 参数。`branch_name` 必须是不存在的新分支（服务端经
/// `git check-ref-format --branch` 校验并以 `-b` 创建）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct WorktreeCreateParams {
    pub branch_name: String,
    /// 新分支的基线 commit-ish；`None` = HEAD。
    pub base_ref: Option<String>,
    /// true = 创建后当前会话立即绑定（模型自用场景）；false = 只建不绑，
    /// 留给用户/编排方把 worktree 分配给其他会话。
    pub attach: bool,
}

/// `worktree.remove` 参数。dirty 且未 `force` 时服务端拒绝。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct WorktreeRemoveParams {
    pub worktree_id: String,
    pub force: bool,
}

/// 绑定结果读模型：双锚点（worktree 代码路径 ↔ source_root 工件锚点）。
/// 绑定后模型环境块的 workspace 行 = `worktree_root`；`.qaqh` 工件继续落在
/// `source_root`（ADR-0041 决策 3）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct WorktreeBindingView {
    pub worktree_root: String,
    pub source_root: String,
    pub branch: String,
    pub head_sha: String,
    pub owner_session_id: String,
}

/// `gitx.worktree_list` 结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct GitxWorktreeListResult {
    /// 注册表登记的 managed worktree（含 `missing_dir` 失效态）。
    pub worktrees: Vec<WorktreeView>,
    /// 发现式枚举出的 unmanaged linked worktree 路径（手工 `git worktree add`
    /// 等绕过工具的产物，按 ADR-0041 决策 6 不享受自动放行）。
    pub unmanaged_paths: Vec<String>,
}

/// `gitx.worktree_create` 结果。`attach = true` 时 `binding` 必有。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct GitxWorktreeCreateResult {
    pub worktree: WorktreeView,
    pub binding: Option<WorktreeBindingView>,
}

/// `gitx.worktree_remove` 结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct GitxWorktreeRemoveResult {
    pub removed: bool,
}

/// `gitx.worktree_unbind` 结果。`unbound = false` = 会话本无绑定（幂等成功）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct GitxWorktreeUnbindResult {
    pub unbound: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// wire 形状锁：与客户端侧共享同一份样本。少一个字段/改一个键名，前端在
    /// 编译期或运行期直接断（惯例对齐 lib.rs 的 team_response 测试）。
    #[test]
    fn worktree_view_round_trips_the_daemon_wire_shape() {
        let payload = serde_json::json!({
            "schema": GITX_SCHEMA,
            "id": "0199a0f0-0000-7000-8000-00000000ab12",
            "branch": "feature/auth",
            "slug": "feature-auth",
            "path": "D:/Projects/demo/.qaqh/worktree/feature-auth-ab12",
            "source_root": "D:/Projects/demo",
            "head_sha": "3f2a000000000000000000000000000000000000",
            "owner_session_id": null,
            "created_at_ms": 1_760_000_000_000i64,
            "missing_dir": false
        });
        let view: WorktreeView = serde_json::from_value(payload.clone()).unwrap();
        assert_eq!(view.schema, GITX_SCHEMA);
        assert_eq!(view.branch, "feature/auth");
        assert_eq!(view.owner_session_id, None);
        let round: serde_json::Value = serde_json::to_value(&view).unwrap();
        assert_eq!(round, payload);
    }

    #[test]
    fn binding_view_round_trips() {
        let payload = serde_json::json!({
            "worktree_root": "D:/Projects/demo/.qaqh/worktree/feature-auth-ab12",
            "source_root": "D:/Projects/demo",
            "branch": "feature/auth",
            "head_sha": "3f2a000000000000000000000000000000000000",
            "owner_session_id": "0199a0f0-0000-7000-8000-000000000001"
        });
        let binding: WorktreeBindingView = serde_json::from_value(payload.clone()).unwrap();
        let round: serde_json::Value = serde_json::to_value(&binding).unwrap();
        assert_eq!(round, payload);
    }

    #[test]
    fn git_status_snapshot_round_trips_the_daemon_wire_shape() {
        let payload = serde_json::json!({
            "schema": GITX_SCHEMA,
            "source_root": "D:/Projects/demo",
            "branch": "feature/auth",
            "head_sha": "3f2a000000000000000000000000000000000000",
            "detached": false,
            "upstream": "origin/feature/auth",
            "ahead": 2,
            "behind": 0,
            "files": [
                { "path": "src/lib.rs", "index": "M", "worktree": " " },
                { "path": "docs/new.md", "index": "?", "worktree": "?" }
            ],
            "unpushed": [
                { "hash": "abc1234", "subject": "feat: x" }
            ]
        });
        let status: GitStatusSnapshot = serde_json::from_value(payload.clone()).unwrap();
        assert_eq!(status.files.len(), 2);
        assert_eq!(status.unpushed.len(), 1);
        let round: serde_json::Value = serde_json::to_value(&status).unwrap();
        assert_eq!(round, payload);
    }
}
