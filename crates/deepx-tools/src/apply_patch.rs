//! Codex-format apply_patch transplant for DeepX.
//!
//! Accepts the custom `*** Begin Patch` / `*** End Patch` marker-based patch
//! format that GPT models naturally produce, and executes Add / Delete / Update
//! operations through DeepX's existing file primitives (`atomic_write`,
//! `file_state`).
//!
//! Core parts extracted & simplified from <codex-rs/apply-patch/>:
//!   • marker constants / batch parser  (parser.rs)
//!   • fuzzy line matching               (seek_sequence.rs)
//!   • replacement computation           (lib.rs)
use std::path::{Path, PathBuf};
use crate::{JsonArgs, ToolHandler, ToolRisk};
// ── Markers (from codex-rs/apply-patch/src/parser.rs) ──
const BEGIN_PATCH_MARKER: &str = "*** Begin Patch";
const END_PATCH_MARKER: &str = "*** End Patch";
const ADD_FILE_MARKER: &str = "*** Add File: ";
const DELETE_FILE_MARKER: &str = "*** Delete File: ";
const UPDATE_FILE_MARKER: &str = "*** Update File: ";
const MOVE_TO_MARKER: &str = "*** Move to: ";
const EOF_MARKER: &str = "*** End of File";
// ── Hunk types (from codex-rs/apply-patch/src/parser.rs) ──
#[derive(Debug, PartialEq, Clone)]
enum Hunk {
    AddFile {
        path: PathBuf,
        contents: String,
    },
    DeleteFile {
        path: PathBuf,
    },
    UpdateFile {
        path: PathBuf,
        move_path: Option<PathBuf>,
        chunks: Vec<UpdateFileChunk>,
    },
}
#[derive(Debug, PartialEq, Clone)]
struct UpdateFileChunk {
    change_context: Option<String>,
    old_lines: Vec<String>,
    new_lines: Vec<String>,
    is_end_of_file: bool,
}
// ── Batch parser ──
// Simplified from StreamingPatchParser — we don't need incremental streaming
// in DeepX, so a straightforward line-by-line pass is enough.
struct PatchParser {
    lines: Vec<String>,
    pos: usize,
    hunks: Vec<Hunk>,
}
impl PatchParser {
    fn parse(patch: &str) -> Result<Vec<Hunk>, String> {
        let lines: Vec<String> = patch.lines().map(str::to_string).collect();
        if lines.is_empty() {
            return Err("patch is empty".into());
        }
        // Strip heredoc wrapper if present (GPT-4.1 compatibility).
        let patch_lines = strip_heredoc(&lines);
        // Validate boundaries.
        let first = patch_lines.first().map(|l| l.trim());
        let last = patch_lines.last().map(|l| l.trim());
        if first != Some(BEGIN_PATCH_MARKER) {
            return Err("patch must start with '*** Begin Patch'".into());
        }
        if last != Some(END_PATCH_MARKER) {
            return Err("patch must end with '*** End Patch'".into());
        }
        let mut parser = PatchParser {
            lines: patch_lines.iter().skip(1).map(|s| s.to_string()).collect(),
            pos: 0,
            hunks: Vec::new(),
        };
        while parser.pos < parser.lines.len() {
            let line = parser.lines[parser.pos].trim().to_string();
            if line == END_PATCH_MARKER {
                break;
            }
            if line.is_empty() {
                parser.pos += 1;
                continue;
            }
            if let Some(path) = line.strip_prefix(ADD_FILE_MARKER) {
                let path = PathBuf::from(path.trim());
                parser.pos += 1;
                let contents = parser.read_add_lines();
                parser.hunks.push(Hunk::AddFile { path, contents });
            } else if let Some(path) = line.strip_prefix(DELETE_FILE_MARKER) {
                parser.hunks.push(Hunk::DeleteFile {
                    path: PathBuf::from(path.trim()),
                });
                parser.pos += 1;
            } else if let Some(path) = line.strip_prefix(UPDATE_FILE_MARKER) {
                let path = PathBuf::from(path.trim());
                parser.pos += 1;
                let (move_path, chunks) = parser.read_update_body()?;
                parser.hunks.push(Hunk::UpdateFile {
                    path,
                    move_path,
                    chunks,
                });
            } else {
                return Err(format!(
                    "unexpected line {}: '{}'",
                    parser.pos + 1,
                    line
                ));
            }
        }
        Ok(parser.hunks)
    }
    fn read_add_lines(&mut self) -> String {
        let mut content = String::new();
        while self.pos < self.lines.len() {
            let line = &self.lines[self.pos];
            if let Some(rest) = line.strip_prefix('+') {
                content.push_str(rest);
                content.push('\n');
                self.pos += 1;
            } else {
                break;
            }
        }
        content
    }
    fn read_update_body(
        &mut self,
    ) -> Result<(Option<PathBuf>, Vec<UpdateFileChunk>), String> {
        let mut move_path = None;
        let mut chunks: Vec<UpdateFileChunk> = Vec::new();
        while self.pos < self.lines.len() {
            let line = self.lines[self.pos].trim().to_string();
            if line.is_empty() {
                self.pos += 1;
                continue;
            }
            if let Some(dest) = line.strip_prefix(MOVE_TO_MARKER) {
                move_path = Some(PathBuf::from(dest.trim()));
                self.pos += 1;
                continue;
            }
            // Check for next hunk marker — exit UpdateFile body.
            if line.starts_with(ADD_FILE_MARKER)
                || line.starts_with(DELETE_FILE_MARKER)
                || line.starts_with(UPDATE_FILE_MARKER)
                || line == END_PATCH_MARKER
            {
                break;
            }
            // Read a single chunk.
            let chunk = self.read_one_update_chunk()?;
            chunks.push(chunk);
        }
        Ok((move_path, chunks))
    }
    fn read_one_update_chunk(&mut self) -> Result<UpdateFileChunk, String> {
        let line = &self.lines[self.pos];
        let trimmed = line.trim();
        // Parse context line: "@@" or "@@ some_context"
        let change_context = if trimmed == "@@" {
            self.pos += 1;
            None
        } else if let Some(ctx) = trimmed.strip_prefix("@@ ") {
            self.pos += 1;
            Some(ctx.to_string())
        } else {
            return Err(format!(
                "expected @@ context marker at line {}, got: '{}'",
                self.pos + 1,
                trimmed
            ));
        };
        let mut old_lines: Vec<String> = Vec::new();
        let mut new_lines: Vec<String> = Vec::new();
        let mut is_end_of_file = false;
        while self.pos < self.lines.len() {
            let line = &self.lines[self.pos];
            let trimmed = line.trim();
            if trimmed == EOF_MARKER {
                is_end_of_file = true;
                self.pos += 1;
                break;
            }
            if trimmed.is_empty() {
                self.pos += 1;
                continue;
            }
            // Stop at next chunk or hunk marker.
            if trimmed.starts_with("@@")
                || trimmed.starts_with(ADD_FILE_MARKER)
                || trimmed.starts_with(DELETE_FILE_MARKER)
                || trimmed.starts_with(UPDATE_FILE_MARKER)
                || trimmed == END_PATCH_MARKER
            {
                break;
            }
            // Each line starts with ' ' (context), '-' (removed), or '+' (added).
            // Context lines appear in both old_lines and new_lines.
            // Removed lines appear only in old_lines.
            // Added lines appear only in new_lines.
            if let Some(rest) = line.strip_prefix(' ') {
                old_lines.push(rest.to_string());
                new_lines.push(rest.to_string());
                self.pos += 1;
            } else if let Some(rest) = line.strip_prefix('-') {
                old_lines.push(rest.to_string());
                self.pos += 1;
            } else if let Some(rest) = line.strip_prefix('+') {
                new_lines.push(rest.to_string());
                self.pos += 1;
            } else {
                return Err(format!(
                    "unexpected update line {}: '{}'",
                    self.pos + 1,
                    line
                ));
            }
        }
        Ok(UpdateFileChunk {
            change_context,
            old_lines,
            new_lines,
            is_end_of_file,
        })
    }
}
fn strip_heredoc(lines: &[String]) -> Vec<String> {
    if lines.len() < 4 {
        return lines.to_vec();
    }
    let first = lines[0].trim();
    let last = lines[lines.len() - 1].trim();
    if (first == "<<EOF" || first == "<<'EOF'" || first == "<<\"EOF\"")
        && last.ends_with("EOF")
    {
        // Strip heredoc markers.
        lines[1..lines.len() - 1].to_vec()
    } else {
        lines.to_vec()
    }
}
// ── seek_sequence (from codex-rs/apply-patch/src/seek_sequence.rs) ──
// Four-pass fuzzy matching: exact → rstrip → trim → Unicode normalise.
fn seek_sequence(
    lines: &[String],
    pattern: &[String],
    start: usize,
    eof: bool,
) -> Option<usize> {
    if pattern.is_empty() {
        return Some(start);
    }
    if pattern.len() > lines.len() {
        return None;
    }
    let search_start = if eof && lines.len() >= pattern.len() {
        lines.len() - pattern.len()
    } else {
        start
    };
    // ① Exact match.
    for i in search_start..=lines.len().saturating_sub(pattern.len()) {
        if lines[i..i + pattern.len()] == *pattern {
            return Some(i);
        }
    }
    // ② Trim-end match.
    for i in search_start..=lines.len().saturating_sub(pattern.len()) {
        if lines[i..i + pattern.len()]
            .iter()
            .zip(pattern.iter())
            .all(|(a, b)| a.trim_end() == b.trim_end())
        {
            return Some(i);
        }
    }
    // ③ Trim match.
    for i in search_start..=lines.len().saturating_sub(pattern.len()) {
        if lines[i..i + pattern.len()]
            .iter()
            .zip(pattern.iter())
            .all(|(a, b)| a.trim() == b.trim())
        {
            return Some(i);
        }
    }
    // ④ Unicode normalise (fancy quotes → ASCII, em-dash → '-', etc.).
    for i in search_start..=lines.len().saturating_sub(pattern.len()) {
        if lines[i..i + pattern.len()]
            .iter()
            .zip(pattern.iter())
            .all(|(a, b)| normalise(a) == normalise(b))
        {
            return Some(i);
        }
    }
    None
}
fn normalise(s: &str) -> String {
    s.trim()
        .chars()
        .map(|c| match c {
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}'
            | '\u{2015}' | '\u{2212}' => '-',
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
            '\u{00A0}' | '\u{2002}' | '\u{2003}' | '\u{2004}' | '\u{2005}'
            | '\u{2006}' | '\u{2007}' | '\u{2008}' | '\u{2009}' | '\u{200A}'
            | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
            other => other,
        })
        .collect()
}
// ── Replacement engine (from codex-rs/apply-patch/src/lib.rs) ──
fn compute_replacements(
    original_lines: &[String],
    path: &str,
    chunks: &[UpdateFileChunk],
) -> Result<Vec<(usize, usize, Vec<String>)>, String> {
    let mut replacements: Vec<(usize, usize, Vec<String>)> = Vec::new();
    let mut line_index: usize = 0;
    for chunk in chunks {
        // If a chunk has a change_context, find it and advance line_index.
        if let Some(ctx_line) = &chunk.change_context {
            if let Some(idx) =
                seek_sequence(original_lines, &[ctx_line.clone()], line_index, false)
            {
                line_index = idx + 1;
            } else {
                return Err(format!(
                    "Failed to find context '{}' in {}",
                    ctx_line, path
                ));
            }
        }
        if chunk.old_lines.is_empty() {
            // Pure addition.
            let insertion_idx = if original_lines
                .last()
                .is_some_and(|s| s.is_empty())
            {
                original_lines.len() - 1
            } else {
                original_lines.len()
            };
            replacements.push((insertion_idx, 0, chunk.new_lines.clone()));
            continue;
        }
        // Try to locate old_lines via fuzzy matching.
        let mut pattern: &[String] = &chunk.old_lines;
        let mut found = seek_sequence(
            original_lines,
            pattern,
            line_index,
            chunk.is_end_of_file,
        );
        let mut new_slice: &[String] = &chunk.new_lines;
        // Retry without trailing empty line (final newline sentinel).
        if found.is_none() && pattern.last().is_some_and(|s| s.is_empty()) {
            pattern = &pattern[..pattern.len() - 1];
            if new_slice.last().is_some_and(|s| s.is_empty()) {
                new_slice = &new_slice[..new_slice.len() - 1];
            }
            found = seek_sequence(
                original_lines,
                pattern,
                line_index,
                chunk.is_end_of_file,
            );
        }
        if let Some(start_idx) = found {
            replacements.push((start_idx, pattern.len(), new_slice.to_vec()));
            line_index = start_idx + pattern.len();
        } else {
            return Err(format!(
                "Failed to find expected lines in {}:\n{}",
                path,
                chunk.old_lines.join("\n")
            ));
        }
    }
    replacements.sort_by_key(|(index, _, _)| *index);
    Ok(replacements)
}
fn apply_replacements(
    mut lines: Vec<String>,
    replacements: &[(usize, usize, Vec<String>)],
) -> Vec<String> {
    // Apply in reverse order so earlier replacements don't shift later indices.
    for (start_idx, old_len, new_segment) in replacements.iter().rev() {
        let start_idx = *start_idx;
        let old_len = *old_len;
        for _ in 0..old_len {
            if start_idx < lines.len() {
                lines.remove(start_idx);
            }
        }
        for (offset, new_line) in new_segment.iter().enumerate() {
            lines.insert(start_idx + offset, new_line.clone());
        }
    }
    lines
}
fn derive_new_contents(
    original: &str,
    path: &str,
    chunks: &[UpdateFileChunk],
) -> Result<String, String> {
    let mut lines: Vec<String> = original.split('\n').map(String::from).collect();
    // Drop trailing empty element from final newline (standard diff behavior).
    if lines.last().is_some_and(|s| s.is_empty()) {
        lines.pop();
    }
    let replacements = compute_replacements(&lines, path, chunks)?;
    let mut new_lines = apply_replacements(lines, &replacements);
    // Restore trailing newline.
    if !new_lines.last().is_some_and(|s| s.is_empty()) {
        new_lines.push(String::new());
    }
    Ok(new_lines.join("\n"))
}
// ── DeepX handler ──
pub(super) fn exec_apply_patch(args: &serde_json::Value) -> String {
    let patch_text = args.s("patch");
    if patch_text.is_empty() {
        return serde_json::json!({
            "timeis": crate::now_utc8(), "status": "error",
            "code": "EMPTY_PATCH",
            "message": "patch argument is required and must not be empty",
            "hint": "Provide a patch body starting with *** Begin Patch"
        })
        .to_string();
    }
    let expected_hash = args.s("expected_hash");
    let dry_run = args.opt_bool("dry_run").unwrap_or(false);
    // 1. Parse Codex format.
    let hunks = match PatchParser::parse(&patch_text) {
        Ok(hunks) => hunks,
        Err(e) => {
            return serde_json::json!({
                "timeis": crate::now_utc8(), "status": "error",
                "code": "PARSE_ERROR",
                "message": format!("Failed to parse patch: {}", e),
                "hint": "Check that the patch starts with *** Begin Patch and ends with *** End Patch"
            })
            .to_string();
        }
    };
    if hunks.is_empty() {
        return serde_json::json!({
            "timeis": crate::now_utc8(), "status": "error",
            "code": "EMPTY_PATCH",
            "message": "Patch contains no file changes",
            "hint": "Add at least one *** Add File: / *** Delete File: / *** Update File: section"
        })
        .to_string();
    }
    // 2. Resolve workspace (PathBuf from string — no canonicalize, which
    //    adds Windows \\?\ prefix that breaks file operations).
    let workspace = crate::CURRENT_WORKSPACE
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    if workspace.is_empty() || workspace == "." {
        return serde_json::json!({
            "timeis": crate::now_utc8(), "status": "error",
            "code": "NO_WORKSPACE",
            "message": "An existing workspace is required"
        })
        .to_string();
    }
    let workspace = std::path::PathBuf::from(&workspace);
    // 3. Verify expected_hash if provided — check every UpdateFile/DeleteFile target.
    if !expected_hash.is_empty() && !dry_run {
        for hunk in &hunks {
            match hunk {
                Hunk::UpdateFile { path, .. } | Hunk::DeleteFile { path, .. } => {
                    match verify_hash_for_path(path, &expected_hash, &workspace) {
                        Ok(()) => {}
                        Err(stale_err) => return stale_err,
                    }
                }
                Hunk::AddFile { .. } => {}
            }
        }
    }
    // 4. Dry-run: compute what would change and return a unified diff preview.
    if dry_run {
        return dry_run_preview(&hunks, &workspace);
    }
    // 5. Apply each hunk (non-dry-run).
    let mut added: Vec<String> = Vec::new();
    let mut modified: Vec<String> = Vec::new();
    let mut deleted: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    for hunk in &hunks {
        let result = match hunk {
            Hunk::AddFile { path, contents } => {
                apply_add(path, contents, &workspace)
            }
            Hunk::DeleteFile { path } => {
                apply_delete(path, &workspace)
            }
            Hunk::UpdateFile {
                path,
                move_path,
                chunks,
            } => apply_update(path, move_path.as_ref(), chunks, &workspace),
        };
        match result {
            Ok(affected) => {
                for a in affected.added {
                    added.push(a);
                }
                for m in affected.modified {
                    modified.push(m);
                }
                for d in affected.deleted {
                    deleted.push(d);
                }
            }
            Err(e) => errors.push(e),
        }
    }
    // 6. Build output.
    if !errors.is_empty() && added.is_empty() && modified.is_empty() && deleted.is_empty() {
        return serde_json::json!({
            "timeis": crate::now_utc8(), "status": "error",
            "code": "APPLY_FAILED",
            "message": errors.join("; "),
            "hint": "Read the target files and verify the patch content matches"
        })
        .to_string();
    }
    let mut summary = if errors.is_empty() {
        "Success. Updated the following files:\n".to_string()
    } else {
        format!(
            "Partial success ({} errors). Updated files:\n",
            errors.len()
        )
    };
    for p in &added {
        summary.push_str(&format!("A {}\n", p));
    }
    for p in &modified {
        summary.push_str(&format!("M {}\n", p));
    }
    for p in &deleted {
        summary.push_str(&format!("D {}\n", p));
    }
    if !errors.is_empty() {
        summary.push_str("\nErrors:\n");
        for e in &errors {
            summary.push_str(&format!("  • {}\n", e));
        }
    }
    serde_json::json!({
        "timeis": crate::now_utc8(), "status": "ok",
        "content": summary,
        "added": added, "modified": modified, "deleted": deleted
    })
    .to_string()
}
/// Verify the file at `path` matches `expected_hash`. Returns Ok on match,
/// or a JSON error string on mismatch / missing file.
fn verify_hash_for_path(
    path: &Path,
    expected_hash: &str,
    workspace: &Path,
) -> Result<(), String> {
    let full = match resolve_path(path, workspace) {
        Ok(p) => p,
        Err(e) => return Err(serde_json::json!({
            "timeis": crate::now_utc8(), "status": "error",
            "code": "STALE_FILE",
            "path": path.display().to_string(),
            "message": format!("Cannot resolve {}: {}", path.display(), e),
            "hint": "Use read to verify the file still exists."
        }).to_string()),
    };
    let content = match std::fs::read_to_string(&full) {
        Ok(c) => c,
        Err(e) => return Err(serde_json::json!({
            "timeis": crate::now_utc8(), "status": "error",
            "code": "STALE_FILE",
            "path": path.display().to_string(),
            "message": format!("Cannot read {}: {}", path.display(), e),
            "hint": "The file may have been deleted. Use list to check."
        }).to_string()),
    };
    // Normalize newlines before hashing (same as read does).
    let (normalized, _) = crate::file_shared::normalize_newlines(&content);
    let actual_hash = crate::file_shared::content_hash(&normalized);
    if actual_hash == expected_hash {
        return Ok(());
    }
    Err(serde_json::json!({
        "timeis": crate::now_utc8(), "status": "error",
        "code": "STALE_FILE",
        "path": path.display().to_string(),
        "message": "File content changed since the referenced read",
        "expected_hash": expected_hash,
        "actual_hash": actual_hash,
        "hint": "Use read to obtain current content and hash, then retry."
    }).to_string())
}
/// Dry-run: compute diffs for every UpdateFile hunk without writing anything.
fn dry_run_preview(hunks: &[Hunk], workspace: &Path) -> String {
    let mut diffs = String::from("[DRY RUN] apply_patch — preview, no changes written\n\n");
    let mut has_any = false;
    for hunk in hunks {
        match hunk {
            Hunk::AddFile { path, contents } => {
                diffs.push_str(&format!(
                    "--- /dev/null\n+++ b/{}\n@@ -0,0 +1,{} @@\n",
                    path.display(),
                    contents.lines().count().max(1),
                ));
                for line in contents.lines() {
                    diffs.push_str(&format!("+{}\n", line));
                }
                diffs.push('\n');
                has_any = true;
            }
            Hunk::DeleteFile { path } => {
                diffs.push_str(&format!("--- a/{}\n+++ /dev/null\n", path.display()));
                match resolve_path(path, workspace)
                    .and_then(|full| std::fs::read_to_string(&full).map_err(|e| e.to_string()))
                {
                    Ok(content) => {
                        let count = content.lines().count().max(1);
                        diffs.push_str(&format!(
                            "@@ -1,{} +0,0 @@\n",
                            count
                        ));
                        for line in content.lines() {
                            diffs.push_str(&format!("-{}\n", line));
                        }
                    }
                    Err(_) => {
                        diffs.push_str("@@ -1,1 +0,0 @@\n-[file not found]\n");
                    }
                }
                diffs.push('\n');
                has_any = true;
            }
            Hunk::UpdateFile {
                path,
                move_path,
                chunks,
            } => {
                let path_display = if let Some(dest) = move_path {
                    format!("{} → {}", path.display(), dest.display())
                } else {
                    path.display().to_string()
                };
                diffs.push_str(&format!("--- a/{}\n+++ b/{}\n", path_display, path_display));
                match resolve_path(path, workspace)
                    .and_then(|full| std::fs::read_to_string(&full).map_err(|e| e.to_string()))
                {
                    Ok(raw) => {
                        match derive_new_contents(&raw, &path.display().to_string(), chunks) {
                            Ok(new_content) => {
                                // Generate a minimal unified diff preview.
                                let diff = crate::file_shared::unified_diff(
                                    &raw, &new_content,
                                    &path.display().to_string(),
                                );
                                diffs.push_str(&diff);
                            }
                            Err(e) => {
                                diffs.push_str(&format!(
                                    "@@ [ERROR] {}\n",
                                    e
                                ));
                            }
                        }
                    }
                    Err(e) => {
                        diffs.push_str(&format!("@@ [ERROR] Cannot read {}: {}\n", path.display(), e));
                    }
                }
                diffs.push('\n');
                has_any = true;
            }
        }
    }
    if !has_any {
        diffs.push_str("(no changes)\n");
    }
    serde_json::json!({
        "timeis": crate::now_utc8(), "status": "ok",
        "dry_run": true,
        "content": diffs.trim_end().to_string()
    })
    .to_string()
}
struct Affected {
    added: Vec<String>,
    modified: Vec<String>,
    deleted: Vec<String>,
}
fn resolve_path(rel: &Path, workspace: &Path) -> Result<String, String> {
    // Simple join — workspace is already canonicalized by exec_apply_patch.
    let full = workspace.join(rel);
    // Quick sanity: for existing files, verify they resolve inside workspace.
    if let Ok(canon) = std::fs::canonicalize(&full) {
        let ws_lower = workspace.to_string_lossy().to_lowercase();
        let canon_lower = canon.to_string_lossy().to_lowercase();
        if !canon_lower.starts_with(&ws_lower) {
            return Err(format!("path '{}' resolves outside workspace", rel.display()));
        }
        return Ok(canon.to_string_lossy().to_string());
    }
    // File doesn't exist yet (AddFile). Just join.
    Ok(full.to_string_lossy().to_string())
}
fn ensure_parent_dir(file_path: &str) -> Result<(), String> {
    if let Some(parent) = Path::new(file_path).parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            std::fs::create_dir_all(parent).map_err(|e| {
                format!("Failed to create parent dirs for {}: {}", file_path, e)
            })?;
        }
    }
    Ok(())
}
fn apply_add(
    path: &Path,
    contents: &str,
    workspace: &Path,
) -> Result<Affected, String> {
    let full = resolve_path(path, workspace)?;
    ensure_parent_dir(&full)?;
    crate::file_shared::atomic_write(&full, contents).map_err(|e| {
        format!("Failed to write {}: {}", path.display(), e)
    })?;
    let line_count = contents.lines().count();
    crate::file_state::record_write(&full, line_count);
    Ok(Affected {
        added: vec![path.display().to_string()],
        modified: vec![],
        deleted: vec![],
    })
}
fn apply_delete(path: &Path, workspace: &Path) -> Result<Affected, String> {
    let full = resolve_path(path, workspace)?;
    let p = Path::new(&full);
    if !p.exists() {
        return Err(format!("{} does not exist", path.display()));
    }
    if p.is_dir() {
        return Err(format!("{} is a directory, refusing to delete", path.display()));
    }
    std::fs::remove_file(p).map_err(|e| {
        format!("Failed to delete {}: {}", path.display(), e)
    })?;
    crate::file_state::record_delete(&full);
    Ok(Affected {
        added: vec![],
        modified: vec![],
        deleted: vec![path.display().to_string()],
    })
}
fn apply_update(
    path: &Path,
    move_path: Option<&PathBuf>,
    chunks: &[UpdateFileChunk],
    workspace: &Path,
) -> Result<Affected, String> {
    let full = resolve_path(path, workspace)?;
    let raw = std::fs::read_to_string(&full).map_err(|e| {
        format!("Failed to read {}: {}", path.display(), e)
    })?;
    let new_content = derive_new_contents(&raw, &path.display().to_string(), chunks)?;
    if let Some(dest) = move_path {
        // Move: write to new path, remove old.
        let dest_full = resolve_path(dest, workspace)?;
        ensure_parent_dir(&dest_full)?;
        crate::file_shared::atomic_write(&dest_full, &new_content)
            .map_err(|e| format!("Failed to write {}: {}", dest.display(), e))?;
        let line_count = new_content.lines().count();
        crate::file_state::record_write(&dest_full, line_count);
        // Remove original.
        std::fs::remove_file(Path::new(&full)).map_err(|e| {
            format!("Failed to remove original {}: {}", path.display(), e)
        })?;
        crate::file_state::record_delete(&full);
        Ok(Affected {
            added: vec![],
            modified: vec![format!(
                "{} → {}",
                path.display(),
                dest.display()
            )],
            deleted: vec![],
        })
    } else {
        // In-place update.
        crate::file_shared::atomic_write(&full, &new_content)
            .map_err(|e| format!("Failed to write {}: {}", path.display(), e))?;
        let line_count = new_content.lines().count();
        crate::file_state::record_edit(&full, line_count);
        Ok(Affected {
            added: vec![],
            modified: vec![path.display().to_string()],
            deleted: vec![],
        })
    }
}
// ── Handler glue ──
fn handle_apply_patch(ctx: crate::ToolCallCtx) -> crate::ToolResult {
    let s = exec_apply_patch(&ctx.args);
    // Parse JSON status to determine success (handler_from_string! can't
    // see into JSON — it only checks for [ERROR] prefix).
    let success = !s.contains("\"status\":\"error\"");
    crate::ToolResult { success, content: s }
}
// ── Registration ──
pub fn register(mgr: &mut crate::ToolManager) {
    mgr.register(ToolHandler {
        key: "apply_patch".to_string(),
        description:
            "Apply file changes using a multi-file patch format. Supports Add, Delete, Update (with move/rename) and content-anchored fuzzy matching. Use @@ to anchor Update hunks to a function or class name; the engine locates the exact lines with Unicode-aware fuzzy search. Call read first and pass its hash as expected_hash to prevent stale writes. Use dry_run=true to preview changes without writing.",
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "patch": {
                    "type": "string",
                    "description": "Patch body in *** Begin Patch / *** End Patch format. Use *** Add File: / *** Delete File: / *** Update File: hunks. Update hunks use @@ context to anchor location, then +/- for changes."
                },
                "expected_hash": {
                    "type": "string",
                    "description": "Optional hash returned by read for the file(s) being updated or deleted. If provided, the patch is rejected when file content has changed since the read."
                },
                "dry_run": {
                    "type": "boolean",
                    "description": "Validate and show the resulting diff without writing. Default: false.",
                    "default": false
                }
            },
            "required": ["patch"],
            "additionalProperties": false
        }),
        handler: handle_apply_patch,
        risk: ToolRisk::Write,
        default_timeout: std::time::Duration::from_secs(60),
    });
}
// ── Tests ──
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parse_simple_add() {
        let patch = "\
*** Begin Patch
*** Add File: hello.txt
+hello
+world
*** End Patch";
        let hunks = PatchParser::parse(patch).unwrap();
        assert_eq!(hunks.len(), 1);
        match &hunks[0] {
            Hunk::AddFile { path, contents } => {
                assert_eq!(path, Path::new("hello.txt"));
                assert_eq!(contents, "hello\nworld\n");
            }
            _ => panic!("expected AddFile"),
        }
    }
    #[test]
    fn parse_add_and_delete() {
        let patch = "\
*** Begin Patch
*** Add File: new.rs
+fn main() {}
*** Delete File: old.rs
*** End Patch";
        let hunks = PatchParser::parse(patch).unwrap();
        assert_eq!(hunks.len(), 2);
        assert!(matches!(hunks[0], Hunk::AddFile { .. }));
        assert!(matches!(hunks[1], Hunk::DeleteFile { .. }));
    }
    #[test]
    fn parse_update_with_context() {
        let patch = "\
*** Begin Patch
*** Update File: src/lib.rs
@@ fn main():
-    old
+    new
    keep
*** End Patch";
        let hunks = PatchParser::parse(patch).unwrap();
        assert_eq!(hunks.len(), 1);
        match &hunks[0] {
            Hunk::UpdateFile { path, chunks, .. } => {
                assert_eq!(path, Path::new("src/lib.rs"));
                assert_eq!(chunks.len(), 1);
                assert_eq!(chunks[0].change_context, Some("fn main():".into()));
                assert_eq!(chunks[0].old_lines, vec!["    old", "   keep"]);
                assert_eq!(chunks[0].new_lines, vec!["    new", "   keep"]);
            }
            _ => panic!("expected UpdateFile"),
        }
    }
    #[test]
    fn parse_update_with_move() {
        let patch = "\
*** Begin Patch
*** Update File: old_name.rs
*** Move to: new_name.rs
@@ struct Foo
-    old
+    new
*** End Patch";
        let hunks = PatchParser::parse(patch).unwrap();
        match &hunks[0] {
            Hunk::UpdateFile {
                path, move_path, ..
            } => {
                assert_eq!(path, Path::new("old_name.rs"));
                assert_eq!(move_path, &Some(PathBuf::from("new_name.rs")));
            }
            _ => panic!("expected UpdateFile"),
        }
    }
    #[test]
    fn parse_heredoc_wrapped() {
        let patch = "<<'EOF'\n*** Begin Patch\n*** Add File: x.txt\n+hi\n*** End Patch\nEOF\n";
        let hunks = PatchParser::parse(patch).unwrap();
        assert_eq!(hunks.len(), 1);
    }
    #[test]
    fn seek_exact_match() {
        let lines: Vec<String> = ["foo", "bar", "baz"].iter().map(|s| s.to_string()).collect();
        let pattern: Vec<String> = ["bar", "baz"].iter().map(|s| s.to_string()).collect();
        assert_eq!(seek_sequence(&lines, &pattern, 0, false), Some(1));
    }
    #[test]
    fn seek_trim_match() {
        let lines: Vec<String> = ["  foo  ", "  bar  "]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let pattern: Vec<String> = ["foo", "bar"].iter().map(|s| s.to_string()).collect();
        assert_eq!(seek_sequence(&lines, &pattern, 0, false), Some(0));
    }
    #[test]
    fn seek_unicode_normalise() {
        let lines: Vec<String> = ["\u{2014} start", "end"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let pattern: Vec<String> = ["- start", "end"].iter().map(|s| s.to_string()).collect();
        assert_eq!(seek_sequence(&lines, &pattern, 0, false), Some(0));
    }
    #[test]
    fn compute_simple_replacement() {
        let lines: Vec<String> = ["one", "two", "three"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let chunks = vec![UpdateFileChunk {
            change_context: None,
            old_lines: vec!["two".into()],
            new_lines: vec!["TWO".into()],
            is_end_of_file: false,
        }];
        let reps = compute_replacements(&lines, "test.txt", &chunks).unwrap();
        let result = apply_replacements(lines.clone(), &reps);
        assert_eq!(result, vec!["one", "TWO", "three"]);
    }
    #[test]
    fn e2e_dry_run_does_not_write() {
        let dir = tempfile::tempdir().unwrap();
        crate::set_workspace(&dir.path().to_string_lossy());
        std::fs::write(dir.path().join("example.txt"), "before\n").unwrap();
        let args = serde_json::json!({
            "patch": "*** Begin Patch\n*** Update File: example.txt\n@@\n-before\n+after\n*** End Patch",
            "dry_run": true
        });
        let result = exec_apply_patch(&args);
        assert!(result.contains("\"dry_run\":true"), "unexpected: {result}");
        assert!(result.contains("[DRY RUN]"), "unexpected: {result}");
        assert_eq!(std::fs::read_to_string(dir.path().join("example.txt")).unwrap(), "before\n");
    }
    #[test]
    fn e2e_stale_hash_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        crate::set_workspace(&dir.path().to_string_lossy());
        std::fs::write(dir.path().join("target.txt"), "original\n").unwrap();
        let bogus_hash = "0".repeat(64);
        let args = serde_json::json!({
            "patch": "*** Begin Patch\n*** Update File: target.txt\n@@\n-original\n+modified\n*** End Patch",
            "expected_hash": bogus_hash
        });
        let result = exec_apply_patch(&args);
        assert!(result.contains("STALE_FILE"), "should reject stale hash: {result}");
        assert_eq!(std::fs::read_to_string(dir.path().join("target.txt")).unwrap(), "original\n");
    }
}
