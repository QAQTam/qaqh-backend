//! worktree 注册表：`{source_root}/.qaqh/worktree/registry.json`。
//!
//! 管理事实源（ADR-0033 决策 6）：注册表只登记**本 crate 创建**的 worktree；
//! 绕过工具手工创建的由 [`crate::git::worktree_list`] 发现式展示为 unmanaged。
//! 落盘原子：tempfile 同目录写 + persist 整体替换。owner 绑定是进程内
//! compare-and-set（v1 单 daemon 进程强一致；跨进程粒度留 v2 的 per-worktree
//! gitdir 文件形态，即 codex `metadata.rs` 的 `persist_noclobber` 手法）。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{GitxError, Result};

/// 注册表文件路径：`{base_dir}/registry.json`。
#[must_use]
pub fn registry_path(base_dir: &Path) -> PathBuf {
    base_dir.join("registry.json")
}

/// 当前毫秒时间戳；时钟异常时取 0（不 panic，注册时间戳不是安全关键路径）。
#[must_use]
pub fn now_epoch_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// 一条 worktree 登记。
///
/// 序列化形态是内部文件格式（非 wire 契约），不参与 ts/C# 导出。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeRecord {
    pub id: String,
    pub branch: String,
    pub slug: String,
    pub path: PathBuf,
    pub head_sha: String,
    pub owner_session_id: Option<String>,
    pub created_at_ms: i64,
}

/// 注册表：加载-变更-原子保存的最小包装。
pub struct Registry {
    file_path: PathBuf,
    records: Vec<WorktreeRecord>,
}

impl Registry {
    /// 文件缺失 = 空注册表（首次使用即空，不视为错误）。
    pub fn load(base_dir: &Path) -> Result<Self> {
        let file_path = registry_path(base_dir);
        let records = match std::fs::read_to_string(&file_path) {
            Ok(content) => serde_json::from_str(&content)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error.into()),
        };
        Ok(Self { file_path, records })
    }

    /// 原子落盘：tempfile 同目录 + persist 整体替换。
    fn save(&self) -> Result<()> {
        let parent = self
            .file_path
            .parent()
            .ok_or_else(|| GitxError::Io(std::io::Error::other("registry path has no parent")))?;
        std::fs::create_dir_all(parent)?;
        let mut temp = tempfile::NamedTempFile::new_in(parent)?;
        serde_json::to_writer_pretty(&mut temp, &self.records)?;
        use std::io::Write as _;
        temp.flush()?;
        temp.persist(&self.file_path)
            .map_err(|persist_error| GitxError::Io(persist_error.error))?;
        Ok(())
    }

    #[must_use]
    pub fn records(&self) -> &[WorktreeRecord] {
        &self.records
    }

    /// 按 id 查找。
    #[must_use]
    pub fn find(&self, id: &str) -> Option<&WorktreeRecord> {
        self.records.iter().find(|record| record.id == id)
    }

    /// 追加并落盘。
    pub fn insert(&mut self, record: WorktreeRecord) -> Result<()> {
        self.records.push(record);
        self.save()
    }

    /// 原子绑定 owner：已占用且非同一会话 → [`GitxError::OwnerConflict`]；
    /// 同一会话重复绑定幂等成功。成功即落盘。
    pub fn bind_owner(&mut self, id: &str, session_id: &str) -> Result<WorktreeRecord> {
        let current = self
            .find(id)
            .cloned()
            .ok_or_else(|| GitxError::NotManaged(id.to_string()))?;
        if let Some(existing) = current.owner_session_id.as_deref() {
            if existing == session_id {
                return Ok(current);
            }
            return Err(GitxError::OwnerConflict(id.to_string(), existing.to_string()));
        }
        if let Some(record) = self.records.iter_mut().find(|record| record.id == id) {
            record.owner_session_id = Some(session_id.to_string());
        }
        self.save()?;
        let bound = self
            .find(id)
            .cloned()
            .ok_or_else(|| GitxError::NotManaged(id.to_string()))?;
        Ok(bound)
    }

    /// 释放 owner：无 owner 幂等成功；他人持有 → [`GitxError::NotOwner`]。
    pub fn unbind_owner(&mut self, id: &str, session_id: &str) -> Result<()> {
        let current = self
            .find(id)
            .cloned()
            .ok_or_else(|| GitxError::NotManaged(id.to_string()))?;
        match current.owner_session_id.as_deref() {
            None => Ok(()),
            Some(owner) if owner == session_id => {
                if let Some(record) = self.records.iter_mut().find(|record| record.id == id) {
                    record.owner_session_id = None;
                }
                self.save()
            }
            Some(owner) => Err(GitxError::NotOwner(id.to_string(), owner.to_string())),
        }
    }

    /// 摘除一条登记并落盘，返回被摘除的记录。
    pub fn remove(&mut self, id: &str) -> Result<WorktreeRecord> {
        let index = self
            .records
            .iter()
            .position(|record| record.id == id)
            .ok_or_else(|| GitxError::NotManaged(id.to_string()))?;
        let record = self.records.remove(index);
        self.save()?;
        Ok(record)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn sample_record(path: &Path) -> WorktreeRecord {
        WorktreeRecord {
            id: "0199wt-0001".to_string(),
            branch: "feature/auth".to_string(),
            slug: "feature-auth-abcd".to_string(),
            path: path.to_path_buf(),
            head_sha: "a".repeat(40),
            owner_session_id: None,
            created_at_ms: 1_760_000_000_000,
        }
    }

    #[test]
    fn load_missing_file_is_empty_registry() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Registry::load(dir.path()).unwrap();
        assert!(registry.records().is_empty());
    }

    #[test]
    fn insert_persists_across_reload() {
        let dir = tempfile::tempdir().unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        registry.insert(sample_record(dir.path())).unwrap();
        let reloaded = Registry::load(dir.path()).unwrap();
        assert_eq!(reloaded.records().len(), 1);
        assert_eq!(reloaded.records()[0].branch, "feature/auth");
    }

    #[test]
    fn bind_owner_is_cas() {
        let dir = tempfile::tempdir().unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        registry.insert(sample_record(dir.path())).unwrap();
        assert!(registry.bind_owner("0199wt-0001", "ses-1").is_ok());
        let conflict = registry.bind_owner("0199wt-0001", "ses-2");
        assert!(matches!(conflict, Err(GitxError::OwnerConflict(_, _))));
        // 同会话幂等。
        assert!(registry.bind_owner("0199wt-0001", "ses-1").is_ok());
        // 释放后可被他人绑定。
        registry.unbind_owner("0199wt-0001", "ses-1").unwrap();
        assert!(registry.bind_owner("0199wt-0001", "ses-2").is_ok());
    }

    #[test]
    fn unbind_by_non_owner_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        registry.insert(sample_record(dir.path())).unwrap();
        registry.bind_owner("0199wt-0001", "ses-1").unwrap();
        let wrong = registry.unbind_owner("0199wt-0001", "ses-2");
        assert!(matches!(wrong, Err(GitxError::NotOwner(_, _))));
    }

    #[test]
    fn unknown_id_reports_not_managed() {
        let dir = tempfile::tempdir().unwrap();
        let mut registry = Registry::load(dir.path()).unwrap();
        let missing = registry.bind_owner("nope", "ses-1");
        assert!(matches!(missing, Err(GitxError::NotManaged(_))));
    }
}
