//! Shared helpers for file edit tools.

use std::io::Write;
use std::path::Path;

/// Stable content fingerprint exposed by `read` and accepted as a write precondition.
pub(crate) fn content_hash(content: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(content.as_bytes()))
}

/// Refuse an edit based on a stale read without changing the file.
pub(super) fn verify_expected_hash(
    path: &str,
    content: &str,
    expected: Option<&str>,
) -> Result<(), String> {
    let Some(expected) = expected.filter(|value| !value.is_empty()) else {
        return Ok(());
    };
    let actual = content_hash(content);
    if actual == expected {
        return Ok(());
    }
    Err(serde_json::json!({
        "timeis": crate::now_utc8(), "status": "error", "code": "STALE_FILE", "path": path,
        "message": "File content changed since the referenced read",
        "expected_hash": expected, "actual_hash": actual,
        "hint": "Use read_file to obtain current content and hash, then retry the edit."
    })
    .to_string())
}

/// Write through a sibling temporary file, so a failed write never leaves a partially
/// truncated destination. Rename is atomic on supported filesystems.
pub(super) fn atomic_write(path: &str, content: &str) -> std::io::Result<()> {
    let target = Path::new(path);
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    let name = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("deepx-file");
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = parent.join(format!(".{name}.deepx-{}-{nonce}.tmp", std::process::id()));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        replace_file(&temporary, target)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

#[cfg(not(windows))]
fn replace_file(source: &Path, target: &Path) -> std::io::Result<()> {
    std::fs::rename(source, target)
}

#[cfg(windows)]
fn replace_file(source: &Path, target: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    use windows::core::PCWSTR;

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let target = target
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    unsafe {
        MoveFileExW(
            PCWSTR::from_raw(source.as_ptr()),
            PCWSTR::from_raw(target.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    }
    .map_err(std::io::Error::other)
}

#[cfg(test)]
mod atomic_write_tests {
    use super::*;

    #[test]
    fn atomic_write_replaces_an_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.txt");
        std::fs::write(&target, "before").unwrap();

        atomic_write(&target.to_string_lossy(), "after").unwrap();

        assert_eq!(std::fs::read_to_string(target).unwrap(), "after");
    }

    #[test]
    fn diff_stats_between_counts_changes_and_first_line() {
        let before = "a\nb\nc\nd\ne\n";
        let after = "a\nb\nX\nY\ne\n";
        // 第 3 行起：替换 2 行
        let (added, removed, first_line) = diff_stats_between(before, after);
        assert_eq!((added, removed, first_line), (2, 2, 3));
    }

    #[test]
    fn diff_stats_between_handles_insert_and_delete() {
        let before = "a\nb\nc\n";
        let after = "a\nb\nB2\nc\nd\n";
        let (added, removed, first_line) = diff_stats_between(before, after);
        assert_eq!((added, removed, first_line), (2, 0, 3));
    }

    #[test]
    fn diff_stats_between_identical_content_is_zero() {
        let (added, removed, first_line) = diff_stats_between("x\ny\n", "x\ny\n");
        assert_eq!((added, removed, first_line), (0, 0, 1));
    }

    #[test]
    fn closest_line_prefers_content_similarity_over_length() {
        // 行长接近但内容无关的行不应胜过内容相似的行
        let content = "fn unrelated_but_long_function_signature() -> Result<(), String> {\nfn helper() -> i32 {\n";
        let found = closest_line(content, "fn helper() -> i32").unwrap();
        assert_eq!(found.0, 2);
        assert!(found.1.contains("helper"));
    }

    #[test]
    fn closest_line_skips_empty_lines() {
        let content = "\n\nfn target() {}\n";
        let found = closest_line(content, "fn target() {}").unwrap();
        assert_eq!(found.0, 3);
    }
}

/// Normalize CRLF → LF in content. Returns (normalized, was_crlf).
///
/// # 换行统一契约（LF canonical view）
///
/// 所有文件工具共享同一"规范视图"（LF）：
/// - `read` 的展示、行号、`hash` 基于 LF 视图（file_query.rs 同款归一化）；
/// - `edit`/`edit_block`/`edit_file` 的**匹配**与 **`expected_hash` 校验**必须
///   在 LF 视图上进行（read 返回的 hash 即 LF 视图 hash，跨视图校验会失配）；
/// - **写回时按 `was_crlf` 还原原始换行**（CRLF 文件保持 CRLF，最小 diff）；
/// - 新文件/`write` 按模型给定内容原样落盘（不归一化）。
///
/// 行号语义：`\r\n` 与 `\n` 在 LF 视图中同构（孤立 `\r` 也归一化为 `\n`），
/// 因此行号在两种换行下完全一致，`\r` 不产生额外行。
pub(crate) fn normalize_newlines(content: &str) -> (String, bool) {
    if content.contains("\r\n") {
        (content.replace("\r\n", "\n"), true)
    } else if content.contains('\r') {
        (content.replace('\r', "\n"), true)
    } else {
        (content.to_string(), false)
    }
}

/// Find the closest line in content to the given search string.
/// Returns (line_number, line_content).
///
/// Scoring: exact containment (either direction) wins and prefers a smaller
/// length gap (original behavior); otherwise Jaro-Winkler similarity decides,
/// so content-related lines beat mere length lookalikes. Empty lines are
/// skipped — a blank line is never the best diagnostic anchor.
pub(super) fn closest_line(content: &str, search: &str) -> Option<(usize, String)> {
    let needle = search.lines().next().unwrap_or(search).trim();
    if needle.is_empty() {
        return None;
    }
    let mut best: Option<(usize, String, f64)> = None;
    for (i, l) in content.lines().enumerate() {
        let line = l.trim();
        if line.is_empty() {
            continue;
        }
        let score = if line.contains(needle) || needle.contains(line) {
            // containment: prefer smaller length gap (original behavior)
            1.0 + 1.0 / (1.0 + (line.len() as f64 - needle.len() as f64).abs())
        } else {
            strsim::jaro_winkler(line, needle)
        };
        if best.as_ref().map_or(true, |(_, _, s)| score > *s) {
            best = Some((i + 1, l.to_string(), score));
        }
    }
    best.map(|(n, s, _)| (n, s))
}

/// Produce a unified diff between two file contents.
/// Shows the first diff region with context.
pub(crate) fn unified_diff(before: &str, after: &str, path: &str) -> String {
    use similar::TextDiff;

    if before == after {
        return String::new();
    }
    let diff = TextDiff::from_lines(before, after);
    diff.unified_diff()
        .context_radius(3)
        .header(&format!("a/{path}"), &format!("b/{path}"))
        .to_string()
}

/// Count added/removed lines and find the first changed line between two
/// contents, using `similar`'s structured diff ops — no diff-text parsing.
///
/// `first_line` is the 1-based line of the first actual change in `before`
/// (more precise than the unified-diff hunk header, which includes context).
pub(crate) fn diff_stats_between(before: &str, after: &str) -> (u32, u32, u32) {
    use similar::DiffTag;
    let diff = similar::TextDiff::from_lines(before, after);
    let mut added = 0u32;
    let mut removed = 0u32;
    let mut first_line = 1u32;
    let mut got_change = false;
    for op in diff.ops() {
        if op.tag() == DiffTag::Equal {
            continue;
        }
        if !got_change {
            first_line = op.old_range().start as u32 + 1;
            got_change = true;
        }
        match op.tag() {
            DiffTag::Insert => added += op.new_range().len() as u32,
            DiffTag::Delete => removed += op.old_range().len() as u32,
            DiffTag::Replace => {
                added += op.new_range().len() as u32;
                removed += op.old_range().len() as u32;
            }
            DiffTag::Equal => {}
        }
    }
    (added, removed, first_line)
}

/// Score candidates by context-before/context-after proximity and pick the best match.
/// Returns Ok(index) on success, or Err(partial-message) when context is missing.
pub(super) fn disambiguate_match(
    candidates: &[usize],
    context_before: &[String],
    context_after: &[String],
    file_lines: &[&str],
    path: &str,
    win: usize,
) -> Result<usize, String> {
    if candidates.len() == 1 {
        return Ok(candidates[0]);
    }
    let norm_before: Vec<String> = context_before
        .iter()
        .map(|l| l.trim_end().to_string())
        .collect();
    let norm_after: Vec<String> = context_after
        .iter()
        .map(|l| l.trim_end().to_string())
        .collect();
    if norm_before.is_empty() && norm_after.is_empty() {
        let locs: Vec<String> = candidates
            .iter()
            .take(5)
            .map(|&i| format!("L{}", i + 1))
            .collect();
        return Err(serde_json::json!({
            "timeis": crate::now_utc8(),
            "status": "error",
            "code": "AMBIGUOUS_MATCH",
            "path": path,
            "message": format!("old_lines matches at {} locations: {}", candidates.len(), locs.join(", ")),
            "candidates": candidates.iter().take(5).map(|&i| i+1).collect::<Vec<usize>>(),
            "hint": "Add context_before/context_after to disambiguate."
        }).to_string());
    }
    let mut best = candidates[0];
    let mut best_score: i32 = -1000;
    for &pos in candidates {
        let mut score = 0i32;
        for (j, cl) in norm_before.iter().enumerate() {
            let fi = pos as i32 - norm_before.len() as i32 + j as i32;
            if fi >= 0 && (fi as usize) < file_lines.len() {
                let fl = file_lines[fi as usize].trim_end().to_string();
                if fl == *cl {
                    score += 3;
                } else if fl.trim() == cl.trim() {
                    score += 1;
                } else {
                    score -= 1;
                }
            } else {
                score -= 2;
            }
        }
        for (j, cl) in norm_after.iter().enumerate() {
            let fi = pos + win + j;
            if fi < file_lines.len() {
                let fl = file_lines[fi].trim_end().to_string();
                if fl == *cl {
                    score += 3;
                } else if fl.trim() == cl.trim() {
                    score += 1;
                } else {
                    score -= 1;
                }
            } else {
                score -= 2;
            }
        }
        if score > best_score {
            best = pos;
            best_score = score;
        }
    }
    Ok(best)
}

pub(super) fn is_binary_read_error(err: &str) -> bool {
    err.contains("valid UTF-8")
        || err.contains("utf8")
        || err.contains("utf-8")
        || err.contains("UTF-8")
}
