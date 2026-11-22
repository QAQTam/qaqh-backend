//! `GitStatusSnapshot` 读模型构造（ADR-0041 决策 7 的前端消费面）。
//!
//! 与 [`super::snapshot`]（模型侧一眼快照）分层：这里多出 porcelain v2 逐文件、
//! upstream/ahead-behind 与未推送清单——只进桌面壳,不进模型上下文。

use std::path::Path;

use qaqh_ringing_contract::{GitFileStatus, GitStatusSnapshot, GitUnpushedCommit, GITX_SCHEMA};

use crate::error::Result;
use crate::git;

/// 未推送提交清单上限（对齐桌面壳旧 `MAX_UNPUSHED`）。
const MAX_UNPUSHED: u32 = 50;

struct ParsedStatus {
    upstream: Option<String>,
    ahead: Option<u32>,
    behind: Option<u32>,
    files: Vec<GitFileStatus>,
}

/// 采集 source_root 的完整 git 状态。`@{u}..HEAD` 失败（标签丢失等）按空清单
/// 降级,而不是让整个读面失败。
pub fn build(source_root: &Path) -> Result<GitStatusSnapshot> {
    git::verify_repository(source_root)?;
    let head_sha = git::rev_parse_commit(source_root, "HEAD").ok();
    let branch = git::current_branch(source_root);
    let detached = branch.is_none() && head_sha.is_some();
    let parsed = parse(&git::status_porcelain_v2(source_root)?);
    let unpushed = if parsed.upstream.is_some() && parsed.ahead.unwrap_or(0) > 0 {
        git::unpushed_log(source_root, MAX_UNPUSHED)
            .unwrap_or_default()
            .into_iter()
            .map(|(hash, subject)| GitUnpushedCommit { hash, subject })
            .collect()
    } else {
        Vec::new()
    };
    Ok(GitStatusSnapshot {
        schema: GITX_SCHEMA.to_string(),
        source_root: source_root.to_string_lossy().into_owned(),
        branch,
        head_sha,
        detached,
        upstream: parsed.upstream,
        ahead: parsed.ahead,
        behind: parsed.behind,
        files: parsed.files,
        unpushed,
    })
}

fn parse(stdout: &str) -> ParsedStatus {
    let mut out = ParsedStatus {
        upstream: None,
        ahead: None,
        behind: None,
        files: Vec::new(),
    };
    for line in stdout.lines() {
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("# branch.upstream ") {
            out.upstream = Some(rest.trim().to_string());
            continue;
        }
        if let Some(rest) = line.strip_prefix("# branch.ab ") {
            let mut parts = rest.trim().split_whitespace();
            out.ahead = parts
                .next()
                .and_then(|token| token.strip_prefix('+'))
                .and_then(|token| token.parse().ok());
            out.behind = parts
                .next()
                .and_then(|token| token.strip_prefix('-'))
                .and_then(|token| token.parse().ok());
            continue;
        }
        if let Some(path) = line.strip_prefix("? ") {
            out.files.push(GitFileStatus {
                path: path.to_string(),
                index: "?".to_string(),
                worktree: "?".to_string(),
            });
            continue;
        }
        if !line.starts_with("1 ") && !line.starts_with("2 ") {
            continue;
        }
        let fields: Vec<&str> = line.split(' ').collect();
        let xy = fields.get(1).copied().unwrap_or("..");
        // 普通项 8 个前置字段,rename 项多一个 X<score> → 9;路径可含空格取剩余整段。
        let skip = if line.starts_with("2 ") { 9 } else { 8 };
        let path_field = fields.get(skip..).unwrap_or(&[]).join(" ");
        let Some(path) = path_field.split('\t').next().filter(|p| !p.is_empty()) else {
            continue;
        };
        out.files.push(GitFileStatus {
            path: path.to_string(),
            index: xy.chars().next().unwrap_or('.').to_string(),
            worktree: xy.chars().nth(1).unwrap_or('.').to_string(),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// porcelain v2 头段 + 普通项 + rename 项 + 未跟踪的逐行解析。
    #[test]
    fn parses_porcelain_v2_headers_and_entries() {
        let stdout = "\
# branch.oid 3f2a000000000000000000000000000000000000
# branch.head feature/auth
# branch.upstream origin/feature/auth
# branch.ab +2 -0
1 .M N... 100644 100644 100644 abc def src/lib.rs
2 R. N... 100644 100644 100644 abc def R100 new dir/new.rs\told.rs
? docs/new.md
";
        let parsed = parse(stdout);
        assert_eq!(parsed.upstream.as_deref(), Some("origin/feature/auth"));
        assert_eq!(parsed.ahead, Some(2));
        assert_eq!(parsed.behind, Some(0));
        assert_eq!(parsed.files.len(), 3);
        assert_eq!(parsed.files[0].path, "src/lib.rs");
        assert_eq!(parsed.files[0].index, ".");
        assert_eq!(parsed.files[0].worktree, "M");
        assert_eq!(parsed.files[1].path, "new dir/new.rs");
        assert_eq!(parsed.files[1].index, "R");
        assert_eq!(parsed.files[2].index, "?");
    }
}
