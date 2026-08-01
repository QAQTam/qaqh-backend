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
        "hint": "Use read to obtain current content and hash, then retry the edit."
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
}

/// Normalize CRLF → LF in content. Returns (normalized, was_crlf).
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
pub(super) fn closest_line(content: &str, search: &str) -> Option<(usize, String)> {
    let needle = search.lines().next().unwrap_or(search).trim();
    if needle.is_empty() {
        return None;
    }
    content
        .lines()
        .enumerate()
        .map(|(i, l)| (i, l, l.trim().len() as i64 - needle.len() as i64))
        .filter(|(_, l, _)| l.contains(needle) || needle.contains(l.trim()))
        .min_by_key(|(_, _, diff)| diff.unsigned_abs())
        .map(|(i, l, _)| (i + 1, l.to_string()))
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

/// Count added/removed lines and find first changed line from a unified diff.
/// Returns (added_lines, removed_lines, first_changed_line).
pub(crate) fn diff_stats(diff: &str) -> (u32, u32, u32) {
    let mut added = 0u32;
    let mut removed = 0u32;
    let mut first_line = 1u32;
    let mut got_hunk = false;
    for line in diff.lines() {
        if line.starts_with("@@") {
            if let Some(rest) = line.strip_prefix("@@ -") {
                if let Some(comma) = rest.find(',') {
                    if let Ok(start) = rest[..comma].parse::<u32>() {
                        if !got_hunk {
                            first_line = start;
                            got_hunk = true;
                        }
                    }
                }
            }
        } else if line.starts_with('+') && !line.starts_with("+++") {
            added += 1;
        } else if line.starts_with('-') && !line.starts_with("---") {
            removed += 1;
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

/// Apply the diff (remove old_lines, insert new_lines) and format the result.
pub(super) fn apply_diff_and_format(
    path: &str,
    file_lines: &[&str],
    match_idx: usize,
    win: usize,
    new_lines: &[String],
    description: &str,
    was_fuzzy: bool,
    dry_run: bool,
    was_crlf: bool,
    had_final_newline: bool,
) -> String {
    let mut out_lines: Vec<&str> = file_lines.to_vec();
    out_lines.splice(match_idx..match_idx + win, std::iter::empty());
    for (j, line) in new_lines.iter().enumerate() {
        out_lines.insert(match_idx + j, line);
    }
    let mut new_content = out_lines.join("\n");
    // `str::lines()` omits the terminal empty item. Preserve the final newline so
    // line-oriented edits do not introduce formatting-only churn.
    if had_final_newline && !new_content.ends_with('\n') {
        new_content.push('\n');
    }

    if dry_run {
        let line = match_idx + 1;
        let added = new_lines.len() as u32;
        let removed = win as u32;
        let mut result = String::new();
        if was_fuzzy {
            result.push_str("\u{26a0} fuzzy match (indentation normalized)\n");
        }
        result.push_str(&format!(
            "[DRY RUN] {path} — preview, no changes written\n\n"
        ));
        result.push_str(&format!("--- a/{}\n+++ b/{}\n", path, path));
        let ctx_line = file_lines.get(match_idx.saturating_sub(1)).unwrap_or(&"");
        result.push_str(&format!(
            "@@ -{},{} +{},{} @@ {}\n",
            line,
            removed.max(1),
            line,
            added.max(1),
            ctx_line
        ));
        let ctx_start = match_idx.saturating_sub(2);
        for i in ctx_start..match_idx {
            result.push_str(&format!(" {}\n", file_lines[i]));
        }
        for i in match_idx..match_idx + win {
            result.push_str(&format!("-{}\n", file_lines[i]));
        }
        for l in new_lines {
            result.push_str(&format!("+{}\n", l));
        }
        let ctx_end = (match_idx + win + 2).min(out_lines.len());
        for i in (match_idx + win)..ctx_end {
            result.push_str(&format!(" {}\n", out_lines[i]));
        }
        let desc = if description.is_empty() {
            "edit_block"
        } else {
            description
        };
        result.push_str(&format!(
            "\n[DRY RUN] {path}:{line} +{added} -{removed} | {desc} (dry run)"
        ));
        return result;
    }

    // Restore CRLF if original file had Windows line endings
    let write_content = if was_crlf {
        new_content.replace('\n', "\r\n")
    } else {
        new_content
    };
    match std::fs::write(path, &write_content) {
        Ok(_) => {
            let line_count = write_content.lines().count();
            crate::file_state::record_edit(path, line_count);
            let line = match_idx + 1;
            let added = new_lines.len() as u32;
            let removed = win as u32;
            let mut result = String::new();
            if was_fuzzy {
                result.push_str("\u{26a0} fuzzy match (indentation normalized)\n");
            }
            let desc = if description.is_empty() {
                "edit_block"
            } else {
                description
            };
            let disp = crate::display_path(&path);
            // On success, omit the full diff body — the LLM already knows what it changed.
            // Saves ~80-90% of tool result tokens per edit.
            use std::fmt::Write;
            let _ = write!(result, "[OK] {disp}:{line} +{added} -{removed} | {desc}");
            result
        }
        Err(e) => format!(
            "[ERROR] Cannot write {}: {}\n[HINT] Verify parent directory exists and is writable.",
            crate::display_path(&path),
            e
        ),
    }
}

pub(super) fn is_binary_read_error(err: &str) -> bool {
    err.contains("valid UTF-8")
        || err.contains("utf8")
        || err.contains("utf-8")
        || err.contains("UTF-8")
}
