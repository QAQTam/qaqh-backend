//! qaqh-gitx —— 工作区 git 域：worktree 会话隔离 + git 快照消费（ADR-0041）。
//!
//! 分层：[`git`] 是 git CLI 适配层；[`registry`] 是管理事实源；[`paths`] 负责目录
//! 名派生；[`snapshot`] 产出 wire 读模型（类型来自 `qaqh-ringing-contract::gitx`）。
//!
//! 绑定语义（ADR-0041 决策 3）：绑定是**会话级**的——actor 上下文 workspace 指向
//! worktree（代码路径），`.qaqh` 工件继续锚定 source_root（双锚点）；一个 worktree
//! 同时至多一个会话（registry CAS）。v1 管理面单 daemon 进程内强一致；跨进程
//! owner 文件形态与 gix 0.90 status 引擎是 v2。

#![cfg_attr(not(test), forbid(unsafe_code))]

pub mod error;
pub mod git;
pub mod paths;
pub mod registry;
pub mod snapshot;
pub mod status;
pub mod tools;

pub use error::{GitxError, Result};
pub use registry::WorktreeRecord;
pub use tools::{GitxHost, install_host, host, register};

use std::io::Write as _;
use std::path::{Path, PathBuf};

use qaqh_ringing_contract::{
    GitSnapshot, GitStatusSnapshot, WorktreeCreateParams, WorktreeView, GITX_SCHEMA,
};

/// 会话绑定结果：双锚点（worktree 代码路径 ↔ source_root 工件锚点）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeBinding {
    pub worktree_root: PathBuf,
    pub source_root: PathBuf,
    pub branch: String,
    pub head_sha: String,
    pub owner_session_id: String,
}

/// per-source_root 的 worktree 管理器。
///
/// 全部方法无内部共享状态（注册表每次从盘上加载），多 manager 实例并发安全由
/// 原子落盘保证；单进程内的占用竞争在 `Registry::bind_owner` 收敛。
pub struct WorktreeManager {
    source_root: PathBuf,
}

impl WorktreeManager {
    /// 以源工作区根创建管理器（不做 git 校验——延迟到首次操作）。
    #[must_use]
    pub fn new(source_root: impl Into<PathBuf>) -> Self {
        Self {
            source_root: source_root.into(),
        }
    }

    /// 源工作区根（`.qaqh` 工件的锚点）。
    #[must_use]
    pub fn source_root(&self) -> &Path {
        &self.source_root
    }

    /// 注册表内的 managed worktree → 契约读模型。
    pub fn list(&self) -> Result<Vec<WorktreeView>> {
        let registry = self.load_registry()?;
        Ok(registry
            .records()
            .iter()
            .map(|record| self.record_to_view(record))
            .collect())
    }

    /// 发现式枚举：不在注册表里的 linked worktree（手工 `git worktree add` 等），
    /// 按 ADR-0041 决策 6 标注 unmanaged，不享受自动放行。
    ///
    /// 主工作区自身（`worktree list` 的第一条）与 managed 条目都排除；比较经
    /// canonicalize，规避 Windows 短路径/大小写的文本差异。
    pub fn list_unmanaged(&self) -> Result<Vec<PathBuf>> {
        let registry = self.load_registry()?;
        let canonical = |path: &Path| {
            std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
        };
        let source_canonical = canonical(&self.source_root);
        let managed = registry
            .records()
            .iter()
            .map(|record| canonical(&record.path))
            .collect::<Vec<_>>();
        Ok(git::worktree_list(&self.source_root)?
            .into_iter()
            .map(|raw| raw.path)
            .filter(|path| {
                let path = canonical(path);
                path != source_canonical && !managed.contains(&path)
            })
            .collect())
    }

    /// 创建 worktree（`git worktree add -b <branch> <path> <base>`）并登记。
    ///
    /// branch 名必须**不存在**（git 会拒绝重复建支）；失败即回滚，不留半成品。
    pub fn create(
        &self,
        params: &WorktreeCreateParams,
        owner_session_id: Option<&str>,
    ) -> Result<WorktreeView> {
        git::verify_repository(&self.source_root)?;
        git::validate_branch(&params.branch_name)?;
        let base = params.base_ref.clone().unwrap_or_else(|| "HEAD".to_string());
        let head_sha = git::rev_parse_commit(&self.source_root, &base)?;

        let base_dir = paths::worktree_base_dir(&self.source_root);
        std::fs::create_dir_all(&base_dir)?;
        ensure_gitignored(&base_dir)?;

        let dir_name = format!(
            "{}-{}",
            paths::slugify_branch(&params.branch_name),
            paths::new_collision_suffix()
        );
        let path = base_dir.join(dir_name);
        git::worktree_add(&self.source_root, &path, &params.branch_name, &base)?;

        // 登记失败 → 回滚 git 侧，绝不留“有目录无登记”的孤儿。
        let record = WorktreeRecord {
            id: uuid::Uuid::now_v7().to_string(),
            branch: params.branch_name.clone(),
            slug: paths::slugify_branch(&params.branch_name),
            path: path.clone(),
            head_sha: head_sha.clone(),
            owner_session_id: owner_session_id.map(str::to_string),
            created_at_ms: registry::now_epoch_ms(),
        };
        let mut registry = self.load_registry()?;
        if let Err(error) = registry.insert(record.clone()) {
            let _ = git::worktree_remove(&self.source_root, &path, true);
            let _ = std::fs::remove_dir(&path);
            return Err(error);
        }
        Ok(self.record_to_view(&record))
    }

    /// 会话绑定（原子 CAS owner）。重复绑定同一会话幂等成功。
    pub fn bind(&self, worktree_id: &str, session_id: &str) -> Result<WorktreeBinding> {
        let mut registry = self.load_registry()?;
        let record = registry.bind_owner(worktree_id, session_id)?;
        Ok(WorktreeBinding {
            worktree_root: record.path,
            source_root: self.source_root.clone(),
            branch: record.branch,
            head_sha: record.head_sha,
            owner_session_id: session_id.to_string(),
        })
    }

    /// 释放绑定；无绑定时幂等成功。
    pub fn unbind(&self, worktree_id: &str, session_id: &str) -> Result<()> {
        let mut registry = self.load_registry()?;
        registry.unbind_owner(worktree_id, session_id)
    }

    /// 删除 worktree：dirty 未给 force 拒绝；会话发起时校验 owner
    /// （用户侧/前端删除不传 session_id，不受 owner 约束）。
    pub fn remove(&self, worktree_id: &str, force: bool, session_id: Option<&str>) -> Result<()> {
        let registry = self.load_registry()?;
        let record = registry
            .find(worktree_id)
            .cloned()
            .ok_or_else(|| GitxError::NotManaged(worktree_id.to_string()))?;
        if let (Some(requestor), Some(owner)) = (session_id, record.owner_session_id.as_deref())
            && requestor != owner
        {
            return Err(GitxError::NotOwner(
                worktree_id.to_string(),
                requestor.to_string(),
            ));
        }
        if !force && record.path.is_dir() && git::is_dirty(&record.path)? {
            return Err(GitxError::DirtyWorktree(worktree_id.to_string()));
        }
        git::worktree_remove(&self.source_root, &record.path, force)?;
        let mut registry = self.load_registry()?;
        registry.remove(worktree_id)?;
        Ok(())
    }

    /// source_root 的 git 快照（契约读模型）。
    pub fn snapshot(&self) -> Result<GitSnapshot> {
        snapshot::build(&self.source_root)
    }

    /// 完整 git 状态（前端消费面,与模型侧 [`WorktreeManager::snapshot`] 分层）。
    pub fn status(&self) -> Result<GitStatusSnapshot> {
        status::build(&self.source_root)
    }

    /// 该会话当前绑定的 worktree 登记（registry owner 反查；无绑定返回 `None`）。
    ///
    /// runtime 的 workspace 派生（`ensure_session_workspace`）用它把绑定会话的
    /// 工具执行动态落到 worktree 根（ADR-0041 决策 3/4 的派生式同步落点）。
    pub fn record_for_session(&self, session_id: &str) -> Result<Option<WorktreeRecord>> {
        let registry = self.load_registry()?;
        Ok(registry
            .records()
            .iter()
            .find(|record| record.owner_session_id.as_deref() == Some(session_id))
            .cloned())
    }

    fn load_registry(&self) -> Result<registry::Registry> {
        registry::Registry::load(&paths::worktree_base_dir(&self.source_root))
    }

    fn record_to_view(&self, record: &WorktreeRecord) -> WorktreeView {
        WorktreeView {
            schema: GITX_SCHEMA.to_string(),
            id: record.id.clone(),
            branch: record.branch.clone(),
            slug: record.slug.clone(),
            path: record.path.to_string_lossy().into_owned(),
            source_root: self.source_root.to_string_lossy().into_owned(),
            head_sha: record.head_sha.clone(),
            owner_session_id: record.owner_session_id.clone(),
            created_at_ms: record.created_at_ms,
            missing_dir: !record.path.is_dir(),
        }
    }
}

/// 确保 `{source_root}/.qaqh/.gitignore` 覆盖 `worktree/`（幂等追加）。
///
/// 嵌套 worktree 树必须对 git 状态/dirwalk/journal 全链路不可见——这是 ADR-0041
/// 决策 2 的硬前提，创建函数强制维护，不依赖仓库已有 ignore 规则。
fn ensure_gitignored(base_dir: &Path) -> Result<()> {
    let qaqh_dir = base_dir.parent().ok_or_else(|| {
        GitxError::Io(std::io::Error::other("worktree base dir has no parent"))
    })?;
    let ignore_path = qaqh_dir.join(".gitignore");
    let existing = std::fs::read_to_string(&ignore_path).unwrap_or_default();
    if existing
        .lines()
        .any(|line| line.trim() == paths::WORKTREE_IGNORE_ENTRY)
    {
        return Ok(());
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&ignore_path)?;
    if !existing.is_empty() && !existing.ends_with('\n') {
        writeln!(file)?;
    }
    writeln!(file, "{}", paths::WORKTREE_IGNORE_ENTRY)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    /// 建一个带单次提交的真实 git 仓库（用户信息局部配置，不依赖全局）。
    ///
    /// 预置根 `.gitignore` 含 `.qaqh/`——qaqh 托管工作区的既定前提；没有这条，
    /// harness 自己创建的 `.qaqh/.gitignore` 会把主区染成 untracked（dirty 语义
    /// 是诚实的，但部署形态下不该发生）。
    fn init_repo() -> TempDir {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        std::fs::write(root.join(".gitignore"), ".qaqh/\n").unwrap();
        for args in [
            ["init", "-b", "main"],
            ["config", "user.name", "qaqh-test"],
            ["config", "user.email", "qaqh-test@example.com"],
        ] {
            let status = Command::new("git").current_dir(root).args(args).status().unwrap();
            assert!(status.success());
        }
        std::fs::write(root.join("README.md"), "hello\n").unwrap();
        let status = Command::new("git")
            .current_dir(root)
            .args(["add", "."])
            .status()
            .unwrap();
        assert!(status.success());
        let status = Command::new("git")
            .current_dir(root)
            .args(["commit", "-m", "init"])
            .status()
            .unwrap();
        assert!(status.success());
        dir
    }

    #[test]
    fn create_bind_conflict_remove_round_trip() {
        let repo = init_repo();
        let manager = WorktreeManager::new(repo.path());
        let params = WorktreeCreateParams {
            branch_name: "feature/auth".to_string(),
            base_ref: None,
            attach: false,
        };
        let view = manager.create(&params, None).unwrap();
        assert!(view.path.contains("feature-auth-"));
        assert!(!view.missing_dir);
        // .gitignore 纪律：.qaqh/.gitignore 必须覆盖 worktree/。
        let ignore = std::fs::read_to_string(repo.path().join(".qaqh").join(".gitignore")).unwrap();
        assert!(ignore.lines().any(|line| line.trim() == "worktree/"));
        // 新分支已检出。
        assert_eq!(
            git::current_branch(Path::new(&view.path)).as_deref(),
            Some("feature/auth")
        );

        let binding = manager.bind(&view.id, "ses-1").unwrap();
        assert_eq!(binding.branch, "feature/auth");
        assert_eq!(binding.source_root, repo.path());
        assert!(matches!(
            manager.bind(&view.id, "ses-2"),
            Err(GitxError::OwnerConflict(_, _))
        ));
        assert!(manager.bind(&view.id, "ses-1").is_ok());

        // dirty 未 force 拒绝。
        std::fs::write(binding.worktree_root.join("dirty.txt"), "wip\n").unwrap();
        assert!(matches!(
            manager.remove(&view.id, false, Some("ses-1")),
            Err(GitxError::DirtyWorktree(_))
        ));
        // force + owner 校验通过。
        manager.remove(&view.id, true, Some("ses-1")).unwrap();
        assert!(!binding.worktree_root.exists());
        assert!(manager.list().unwrap().is_empty());
    }

    #[test]
    fn snapshot_reports_branch_dirty_and_unmanaged() {
        let repo = init_repo();
        let manager = WorktreeManager::new(repo.path());
        let snap = manager.snapshot().unwrap();
        assert_eq!(snap.schema, GITX_SCHEMA);
        assert_eq!(snap.branch.as_deref(), Some("main"));
        assert!(!snap.dirty);
        assert!(!snap.detached);

        let params = WorktreeCreateParams {
            branch_name: "side".to_string(),
            base_ref: None,
            attach: false,
        };
        let view = manager.create(&params, Some("ses-9")).unwrap();
        let unmanaged = manager.list_unmanaged().unwrap();
        assert!(unmanaged.is_empty());
        let listed = manager.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].owner_session_id.as_deref(), Some("ses-9"));

        // 手工 worktree → unmanaged 发现（排除主工作区自身与 managed 条目）。
        // 建在池目录里（忽略区），不污染主区 status——与手工场景的真实形态一致。
        let manual = paths::worktree_base_dir(repo.path()).join("manual-wt");
        let status = Command::new("git")
            .current_dir(repo.path())
            .args(["worktree", "add", "-b", "manual-branch"])
            .arg(&manual)
            .status()
            .unwrap();
        assert!(status.success());
        let unmanaged = manager.list_unmanaged().unwrap();
        assert_eq!(unmanaged.len(), 1);
        assert_eq!(unmanaged[0], manual);

        // dirty 属于 worktree 自身；source_root 的快照不受影响。
        std::fs::write(Path::new(&view.path).join("untracked.txt"), "x\n").unwrap();
        assert!(git::is_dirty(Path::new(&view.path)).unwrap());
        let snap = manager.snapshot().unwrap();
        assert!(!snap.dirty);
    }

    #[test]
    fn invalid_branch_is_rejected_before_any_side_effect() {
        let repo = init_repo();
        let manager = WorktreeManager::new(repo.path());
        let params = WorktreeCreateParams {
            branch_name: "bad branch..name".to_string(),
            base_ref: None,
            attach: false,
        };
        assert!(matches!(
            manager.create(&params, None),
            Err(GitxError::InvalidBranch(_))
        ));
        // 校验发生在任何目录副作用之前：worktree 池目录根本不该被建出来。
        assert!(!paths::worktree_base_dir(repo.path()).exists());
    }
}
