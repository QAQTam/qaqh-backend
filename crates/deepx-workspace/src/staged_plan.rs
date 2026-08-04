//! 两阶段编辑工具（preview → commit）的暂存计划存储。
//!
//! `dry_run`（预览写入）生成的计划持久化到 `<workspace>/.deepx/staged-plans/{preview_id}.json`，
//! 后续提交（`commit`）仅凭 `preview_id` 即可应用，无需重复传输完整修改内容。
//!
//! `preview_id` 即计划内容哈希（64-hex）：
//! - 内容寻址：加载后重算哈希与文件名比对，文件被篡改即失效；
//! - 不可猜测：防止跨会话/跨进程盗用（配合 workspace 归属校验）。
//!
//! 生命周期：
//! - TTL 24h：过期计划在 load / store 时惰性清理，sweep 兜底；
//! - 配额 500 个 / 50MB：超限按 mtime LRU 淘汰最旧。

use crate::apply_patch::PatchPlan;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// 暂存计划有效期（秒）。
pub const STAGED_TTL_SECS: u64 = 24 * 60 * 60;
/// 暂存计划数量上限。
pub const STAGED_MAX_PLANS: usize = 500;
/// 暂存计划总字节上限。
pub const STAGED_MAX_TOTAL_BYTES: u64 = 50 * 1024 * 1024;

/// 落盘的暂存计划。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StagedPlan {
    pub preview_id: String,
    pub tool: String,
    pub workspace: String,
    pub created_at: u64,
    pub expires_at: u64,
    pub changes: Vec<crate::apply_patch::PlannedChange>,
}

/// `load` 的失败分类。
#[derive(Debug)]
pub enum LoadError {
    /// 计划不存在 / 已被提交或放弃 / 文件名非法。
    NotFound,
    /// 已过 TTL。
    Stale,
    /// 内容损坏或哈希校验失败（被篡改）。
    Invalid(String),
    /// 归属校验失败：当前 workspace 与计划创建时不一致。
    WorkspaceMismatch,
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 暂存计划目录：`<workspace>/.deepx/staged-plans`。
fn staged_dir(workspace: &str) -> PathBuf {
    PathBuf::from(workspace).join(".deepx").join("staged-plans")
}

/// `preview_id` 必须为 64 位小写 hex（sha256 输出形态），防路径注入与猜测。
pub fn is_valid_preview_id(preview_id: &str) -> bool {
    preview_id.len() == 64
        && preview_id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// 保存计划并返回 `preview_id`（= 内容哈希）。写盘后顺带 sweep 清理。
pub fn store(plan: &PatchPlan, tool: &str, workspace: &str) -> Result<String, String> {
    let preview_id = plan.plan_hash();
    let dir = staged_dir(workspace);
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("cannot create staged plan dir {}: {e}", dir.display()))?;
    let now = unix_now();
    let staged = StagedPlan {
        preview_id: preview_id.clone(),
        tool: tool.to_string(),
        workspace: workspace.to_string(),
        created_at: now,
        expires_at: now + STAGED_TTL_SECS,
        changes: plan.changes.clone(),
    };
    let json = serde_json::to_string_pretty(&staged)
        .map_err(|e| format!("cannot serialize staged plan: {e}"))?;
    // 原子写：同目录临时文件 + rename，避免半写文件被 load 读到。
    let path = dir.join(format!("{preview_id}.json"));
    let tmp = dir.join(format!("{preview_id}.json.tmp"));
    std::fs::write(&tmp, json).map_err(|e| format!("cannot write staged plan: {e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("cannot finalize staged plan: {e}"))?;
    sweep(workspace);
    Ok(preview_id)
}

/// 加载计划。校验：格式 → 内容哈希（防篡改）→ workspace 归属 → TTL。
pub fn load(preview_id: &str, workspace: &str) -> Result<StagedPlan, LoadError> {
    if !is_valid_preview_id(preview_id) {
        return Err(LoadError::NotFound);
    }
    let path = staged_dir(workspace).join(format!("{preview_id}.json"));
    let text = std::fs::read_to_string(&path).map_err(|_| LoadError::NotFound)?;
    let staged: StagedPlan = serde_json::from_str(&text)
        .map_err(|e| LoadError::Invalid(format!("corrupt staged plan: {e}")))?;
    // 内容寻址：preview_id 必须等于计划内容的哈希，篡改即拒绝。
    let recomputed = PatchPlan {
        changes: staged.changes.clone(),
    }
    .plan_hash();
    if recomputed != preview_id {
        return Err(LoadError::Invalid("staged plan content hash mismatch".into()));
    }
    if staged.workspace != workspace {
        return Err(LoadError::WorkspaceMismatch);
    }
    let now = unix_now();
    if now > staged.expires_at {
        let _ = std::fs::remove_file(&path);
        return Err(LoadError::Stale);
    }
    Ok(staged)
}

/// 删除计划。返回是否存在。
pub fn remove(preview_id: &str, workspace: &str) -> bool {
    if !is_valid_preview_id(preview_id) {
        return false;
    }
    let path = staged_dir(workspace).join(format!("{preview_id}.json"));
    match std::fs::remove_file(&path) {
        Ok(()) => true,
        Err(_) => false,
    }
}

/// 清理：删除过期计划；数量/总字节超配额时按 mtime LRU 淘汰最旧。
/// 惰性调用（store 之后），无需常驻任务。
pub fn sweep(workspace: &str) {
    let dir = staged_dir(workspace);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let now = unix_now();
    let mut plans: Vec<(PathBuf, u64, u64)> = Vec::new(); // (path, mtime_secs, size)
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            // 清理残留 .tmp 与无关文件
            let _ = std::fs::remove_file(&path);
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let modified = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if now.saturating_sub(modified) > STAGED_TTL_SECS {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        plans.push((path, modified, meta.len()));
    }
    if plans.is_empty() {
        return;
    }
    // 数量配额
    while plans.len() > STAGED_MAX_PLANS {
        plans.sort_by_key(|(_, modified, _)| *modified);
        let (path, ..) = plans.remove(0);
        let _ = std::fs::remove_file(path);
    }
    // 总字节配额
    let mut total: u64 = plans.iter().map(|(_, _, size)| *size).sum();
    plans.sort_by_key(|(_, modified, _)| *modified);
    while total > STAGED_MAX_TOTAL_BYTES && !plans.is_empty() {
        let (path, _, size) = plans.remove(0);
        total = total.saturating_sub(size);
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apply_patch::PlannedChange;
    use std::path::Path;

    fn sample_plan() -> PatchPlan {
        PatchPlan {
            changes: vec![PlannedChange::Add {
                display: "a.txt".into(),
                target: Path::new("a.txt").to_path_buf(),
                contents: "hello".into(),
            }],
        }
    }

    fn tmp_workspace(label: &str) -> String {
        let dir = std::env::temp_dir().join(format!(
            "deepx-staged-{label}-{}-{}",
            std::process::id(),
            unix_now()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.to_string_lossy().into_owned()
    }

    #[test]
    fn store_load_roundtrip() {
        let ws = tmp_workspace("roundtrip");
        let id = store(&sample_plan(), "apply_patch", &ws).unwrap();
        assert_eq!(id.len(), 64);
        let loaded = load(&id, &ws).unwrap();
        assert_eq!(loaded.tool, "apply_patch");
        assert_eq!(loaded.changes.len(), 1);
        remove(&id, &ws);
        std::fs::remove_dir_all(&ws).unwrap();
    }

    #[test]
    fn tampered_plan_is_rejected() {
        let ws = tmp_workspace("tamper");
        let id = store(&sample_plan(), "apply_patch", &ws).unwrap();
        let path = staged_dir(&ws).join(format!("{id}.json"));
        // 篡改内容：把 contents 改掉
        let mut text = std::fs::read_to_string(&path).unwrap();
        text = text.replace("hello", "evil");
        std::fs::write(&path, text).unwrap();
        assert!(matches!(
            load(&id, &ws),
            Err(LoadError::Invalid(_))
        ));
        std::fs::remove_dir_all(&ws).unwrap();
    }

    #[test]
    fn workspace_mismatch_rejected() {
        let ws1 = tmp_workspace("ws1");
        let ws2 = tmp_workspace("ws2");
        let id = store(&sample_plan(), "apply_patch", &ws1).unwrap();
        // 文件被复制/迁移到另一个 workspace 的暂存目录：内容哈希相同但归属不符。
        let from = staged_dir(&ws1).join(format!("{id}.json"));
        let to = staged_dir(&ws2).join(format!("{id}.json"));
        std::fs::create_dir_all(staged_dir(&ws2)).unwrap();
        std::fs::copy(&from, &to).unwrap();
        assert!(matches!(
            load(&id, &ws2),
            Err(LoadError::WorkspaceMismatch)
        ));
        std::fs::remove_dir_all(&ws1).unwrap();
        std::fs::remove_dir_all(&ws2).unwrap();
    }

    #[test]
    fn invalid_preview_id_rejected() {
        let ws = tmp_workspace("invalid-id");
        assert!(matches!(
            load("../../etc/passwd", &ws),
            Err(LoadError::NotFound)
        ));
        assert!(matches!(
            load("not-a-hash", &ws),
            Err(LoadError::NotFound)
        ));
        assert!(!remove("../../etc/passwd", &ws));
        std::fs::remove_dir_all(&ws).unwrap();
    }

    #[test]
    fn remove_and_missing_are_not_found() {
        let ws = tmp_workspace("missing");
        let id = store(&sample_plan(), "apply_patch", &ws).unwrap();
        assert!(remove(&id, &ws));
        assert!(!remove(&id, &ws));
        assert!(matches!(load(&id, &ws), Err(LoadError::NotFound)));
        std::fs::remove_dir_all(&ws).unwrap();
    }
}
