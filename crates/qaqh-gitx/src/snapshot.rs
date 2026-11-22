//! `GitSnapshot` 读模型构造（ADR-0033 决策 7 的 wire 面）。
//!
//! 类型来自 `qaqh-ringing-contract::gitx`——前端与 daemon 的唯一契约来源；
//! 本模块只负责把 git CLI 的结果翻译成契约形状。

use std::path::Path;

use qaqh_ringing_contract::{GitSnapshot, GITX_SCHEMA};

use crate::error::Result;
use crate::git;

/// 采集 source_root 的 git 快照（branch/head/dirty）。
pub fn build(source_root: &Path) -> Result<GitSnapshot> {
    git::verify_repository(source_root)?;
    let head_sha = git::rev_parse_commit(source_root, "HEAD").ok();
    let branch = git::current_branch(source_root);
    // 有提交但无分支名 = detached HEAD。
    let detached = branch.is_none() && head_sha.is_some();
    let dirty = git::is_dirty(source_root)?;
    Ok(GitSnapshot {
        schema: GITX_SCHEMA.to_string(),
        source_root: source_root.to_string_lossy().into_owned(),
        branch,
        head_sha,
        detached,
        dirty,
    })
}
