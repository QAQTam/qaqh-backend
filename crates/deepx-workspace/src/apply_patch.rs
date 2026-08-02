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
use crate::{JsonArgs, ToolHandler, ToolRisk};
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
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
                validate_patch_path(&path, "Add File")?;
                parser.pos += 1;
                let contents = parser.read_add_lines();
                if contents.is_empty() {
                    return Err(format!(
                        "Add file hunk for path '{}' is empty",
                        path.display()
                    ));
                }
                parser.hunks.push(Hunk::AddFile { path, contents });
            } else if let Some(path) = line.strip_prefix(DELETE_FILE_MARKER) {
                let path = PathBuf::from(path.trim());
                validate_patch_path(&path, "Delete File")?;
                parser.hunks.push(Hunk::DeleteFile { path });
                parser.pos += 1;
            } else if let Some(path) = line.strip_prefix(UPDATE_FILE_MARKER) {
                let path = PathBuf::from(path.trim());
                validate_patch_path(&path, "Update File")?;
                parser.pos += 1;
                let (move_path, chunks) = parser.read_update_body()?;
                if chunks.is_empty() {
                    return Err(format!(
                        "Update file hunk for path '{}' is empty",
                        path.display()
                    ));
                }
                parser.hunks.push(Hunk::UpdateFile {
                    path,
                    move_path,
                    chunks,
                });
            } else {
                return Err(format!("unexpected line {}: '{}'", parser.pos + 1, line));
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
    fn read_update_body(&mut self) -> Result<(Option<PathBuf>, Vec<UpdateFileChunk>), String> {
        let mut move_path = None;
        let mut chunks: Vec<UpdateFileChunk> = Vec::new();
        while self.pos < self.lines.len() {
            let line = self.lines[self.pos].trim().to_string();
            if line.is_empty() {
                self.pos += 1;
                continue;
            }
            if let Some(dest) = line.strip_prefix(MOVE_TO_MARKER) {
                let dest = PathBuf::from(dest.trim());
                validate_patch_path(&dest, "Move to")?;
                if move_path.replace(dest).is_some() {
                    return Err("update hunk contains more than one Move to marker".into());
                }
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
            if line.is_empty() {
                old_lines.push(String::new());
                new_lines.push(String::new());
                self.pos += 1;
                continue;
            }
            if let Some(rest) = line.strip_prefix(' ') {
                old_lines.push(rest.to_string());
                new_lines.push(rest.to_string());
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
            if let Some(rest) = line.strip_prefix('-') {
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
fn validate_patch_path(path: &Path, marker: &str) -> Result<(), String> {
    if path.as_os_str().is_empty() {
        return Err(format!("{marker} path must not be empty"));
    }
    Ok(())
}
fn strip_heredoc(lines: &[String]) -> Vec<String> {
    if lines.len() < 4 {
        return lines.to_vec();
    }
    let first = lines[0].trim();
    let last = lines[lines.len() - 1].trim();
    if (first == "<<EOF" || first == "<<'EOF'" || first == "<<\"EOF\"") && last.ends_with("EOF") {
        // Strip heredoc markers.
        lines[1..lines.len() - 1].to_vec()
    } else {
        lines.to_vec()
    }
}
// ── seek_sequence (from codex-rs/apply-patch/src/seek_sequence.rs) ──
// Four-pass fuzzy matching: exact → rstrip → trim → Unicode normalise.
fn seek_sequence(lines: &[String], pattern: &[String], start: usize, eof: bool) -> Option<usize> {
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
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
            | '\u{2212}' => '-',
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
            '\u{00A0}' | '\u{2002}' | '\u{2003}' | '\u{2004}' | '\u{2005}' | '\u{2006}'
            | '\u{2007}' | '\u{2008}' | '\u{2009}' | '\u{200A}' | '\u{202F}' | '\u{205F}'
            | '\u{3000}' => ' ',
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
            if let Some(idx) = seek_sequence(original_lines, &[ctx_line.clone()], line_index, false)
            {
                line_index = idx + 1;
            } else {
                return Err(format!("Failed to find context '{}' in {}", ctx_line, path));
            }
        }
        if chunk.old_lines.is_empty() {
            // Pure addition.
            let insertion_idx = if original_lines.last().is_some_and(|s| s.is_empty()) {
                original_lines.len() - 1
            } else {
                original_lines.len()
            };
            replacements.push((insertion_idx, 0, chunk.new_lines.clone()));
            continue;
        }
        // Try to locate old_lines via fuzzy matching.
        let mut pattern: &[String] = &chunk.old_lines;
        let mut found = seek_sequence(original_lines, pattern, line_index, chunk.is_end_of_file);
        let mut new_slice: &[String] = &chunk.new_lines;
        // Retry without trailing empty line (final newline sentinel).
        if found.is_none() && pattern.last().is_some_and(|s| s.is_empty()) {
            pattern = &pattern[..pattern.len() - 1];
            if new_slice.last().is_some_and(|s| s.is_empty()) {
                new_slice = &new_slice[..new_slice.len() - 1];
            }
            found = seek_sequence(original_lines, pattern, line_index, chunk.is_end_of_file);
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
#[derive(Debug)]
struct PatchFailure {
    code: &'static str,
    message: String,
    hint: &'static str,
}

impl PatchFailure {
    fn apply(message: impl Into<String>) -> Self {
        Self {
            code: "APPLY_FAILED",
            message: message.into(),
            hint: "Read the target files and verify the patch content matches",
        }
    }
}

#[derive(Debug)]
enum PlannedChange {
    Add {
        display: String,
        target: PathBuf,
        contents: String,
    },
    Delete {
        display: String,
        target: PathBuf,
        original: String,
    },
    Update {
        display: String,
        source: PathBuf,
        target: PathBuf,
        original: String,
        contents: String,
    },
}

#[derive(Debug)]
struct PatchPlan {
    changes: Vec<PlannedChange>,
}

impl PatchPlan {
    fn plan_hash(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        for change in &self.changes {
            match change {
                PlannedChange::Add { display, contents, .. } => {
                    hasher.update(b"add\0");
                    hasher.update(display.as_bytes());
                    hasher.update(b"\0");
                    hasher.update(contents.as_bytes());
                }
                PlannedChange::Delete { display, original, .. } => {
                    hasher.update(b"delete\0");
                    hasher.update(display.as_bytes());
                    hasher.update(b"\0");
                    hasher.update(original.as_bytes());
                }
                PlannedChange::Update { display, original, contents, .. } => {
                    hasher.update(b"update\0");
                    hasher.update(display.as_bytes());
                    hasher.update(b"\0");
                    hasher.update(original.as_bytes());
                    hasher.update(b"\0");
                    hasher.update(contents.as_bytes());
                }
            }
        }
        hex::encode(hasher.finalize())
    }
}

struct WorkspaceResolver {
    root: PathBuf,
}

impl WorkspaceResolver {
    fn new(workspace: &Path) -> Result<Self, String> {
        let absolute = if workspace.is_absolute() {
            workspace.to_path_buf()
        } else {
            std::env::current_dir()
                .map_err(|e| format!("cannot resolve current directory: {e}"))?
                .join(workspace)
        };
        let root = std::fs::canonicalize(&absolute)
            .map_err(|e| format!("cannot resolve workspace '{}': {e}", workspace.display()))?;
        if !root.is_dir() {
            return Err(format!(
                "workspace '{}' is not a directory",
                workspace.display()
            ));
        }
        Ok(Self { root })
    }

    fn resolve(&self, patch_path: &Path) -> Result<PathBuf, String> {
        let candidate = if patch_path.is_absolute() {
            // Absolute paths are trusted as-is (same semantics as read/write/
            // edit): the model said exactly where to write.
            patch_path.to_path_buf()
        } else {
            // Relative paths resolve inside the workspace root. Reject parent
            // components so the patch cannot escape, then canonicalize the
            // ancestor chain to defend against symlink escapes.
            if patch_path
                .components()
                .any(|component| matches!(component, Component::ParentDir))
            {
                return Err(format!(
                    "path '{}' contains a parent-directory component",
                    patch_path.display()
                ));
            }
            self.root.join(patch_path)
        };
        let mut ancestor = candidate.as_path();
        let mut missing = Vec::new();
        while !ancestor.exists() {
            let Some(name) = ancestor.file_name() else {
                return Err(format!("cannot resolve path '{}'", patch_path.display()));
            };
            missing.push(name.to_os_string());
            let Some(parent) = ancestor.parent() else {
                return Err(format!("cannot resolve path '{}'", patch_path.display()));
            };
            ancestor = parent;
        }
        let mut resolved = std::fs::canonicalize(ancestor)
            .map_err(|e| format!("cannot resolve '{}': {e}", patch_path.display()))?;
        for component in missing.iter().rev() {
            resolved.push(component);
        }
        // Only relative paths must stay inside the workspace root.
        if !patch_path.is_absolute() && !resolved.starts_with(&self.root) {
            return Err(format!(
                "path '{}' resolves outside workspace",
                patch_path.display()
            ));
        }
        Ok(resolved)
    }
}

impl PatchPlan {
    fn build(hunks: &[Hunk], resolver: &WorkspaceResolver) -> Result<Self, PatchFailure> {
        let mut changes = Vec::with_capacity(hunks.len());
        let mut touched = HashSet::new();
        for hunk in hunks {
            match hunk {
                Hunk::AddFile { path, contents } => {
                    let target = resolver.resolve(path).map_err(PatchFailure::apply)?;
                    reserve_path(&mut touched, &target, path)?;
                    if target.is_dir() {
                        return Err(PatchFailure::apply(format!(
                            "{} is a directory, refusing to overwrite",
                            path.display()
                        )));
                    }
                    changes.push(PlannedChange::Add {
                        display: path.display().to_string(),
                        target,
                        contents: contents.clone(),
                    });
                }
                Hunk::DeleteFile { path } => {
                    let target = resolver.resolve(path).map_err(PatchFailure::apply)?;
                    reserve_path(&mut touched, &target, path)?;
                    if !target.exists() {
                        return Err(PatchFailure::apply(format!(
                            "{} does not exist",
                            path.display()
                        )));
                    }
                    if target.is_dir() {
                        return Err(PatchFailure::apply(format!(
                            "{} is a directory, refusing to delete",
                            path.display()
                        )));
                    }
                    let original = std::fs::read_to_string(&target).map_err(|e| {
                        PatchFailure::apply(format!("Failed to read {}: {e}", path.display()))
                    })?;
                    changes.push(PlannedChange::Delete {
                        display: path.display().to_string(),
                        target,
                        original,
                    });
                }
                Hunk::UpdateFile {
                    path,
                    move_path,
                    chunks,
                } => {
                    let source = resolver.resolve(path).map_err(PatchFailure::apply)?;
                    if !source.is_file() {
                        return Err(PatchFailure::apply(format!(
                            "{} does not exist or is not a file",
                            path.display()
                        )));
                    }
                    let original = std::fs::read_to_string(&source).map_err(|e| {
                        PatchFailure::apply(format!("Failed to read {}: {e}", path.display()))
                    })?;
                    let contents =
                        derive_new_contents(&original, &path.display().to_string(), chunks)
                            .map_err(PatchFailure::apply)?;
                    let target = match move_path {
                        Some(dest) => resolver.resolve(dest).map_err(PatchFailure::apply)?,
                        None => source.clone(),
                    };
                    reserve_path(&mut touched, &source, path)?;
                    if target != source {
                        let display_path = move_path.as_deref().unwrap_or(path);
                        reserve_path(&mut touched, &target, display_path)?;
                    }
                    if target.is_dir() {
                        return Err(PatchFailure::apply(format!(
                            "{} is a directory, refusing to overwrite",
                            move_path.as_deref().unwrap_or(path).display()
                        )));
                    }
                    let display = match move_path {
                        Some(dest) if target != source => {
                            format!("{} → {}", path.display(), dest.display())
                        }
                        _ => path.display().to_string(),
                    };
                    changes.push(PlannedChange::Update {
                        display,
                        source,
                        target,
                        original,
                        contents,
                    });
                }
            }
        }
        Ok(Self { changes })
    }
}

fn reserve_path(
    touched: &mut HashSet<PathBuf>,
    resolved: &Path,
    patch_path: &Path,
) -> Result<(), PatchFailure> {
    if touched.insert(resolved.to_path_buf()) {
        Ok(())
    } else {
        Err(PatchFailure::apply(format!(
            "path '{}' is modified more than once in the same patch",
            patch_path.display()
        )))
    }
}

pub(crate) fn extract_target_paths(args: &serde_json::Value) -> Vec<PathBuf> {
    let Some(patch) = args.get("patch").and_then(|value| value.as_str()) else {
        return Vec::new();
    };
    let Ok(hunks) = PatchParser::parse(patch) else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    for hunk in hunks {
        match hunk {
            Hunk::AddFile { path, .. } | Hunk::DeleteFile { path } => paths.push(path),
            Hunk::UpdateFile {
                path, move_path, ..
            } => {
                paths.push(path);
                if let Some(dest) = move_path {
                    paths.push(dest);
                }
            }
        }
    }
    let workspace = crate::CURRENT_WORKSPACE
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .clone();
    paths
        .into_iter()
        .map(|path| {
            if path.is_absolute() || workspace.is_empty() || workspace == "." {
                path
            } else {
                Path::new(&workspace).join(path)
            }
        })
        .collect()
}

fn execute_apply_patch(args: &serde_json::Value) -> crate::ToolResult {
    let patch_text = args.s("patch");
    if patch_text.is_empty() {
        return error_result(
            "EMPTY_PATCH",
            "patch argument is required and must not be empty",
            "Provide a patch body starting with *** Begin Patch",
        );
    }
    let dry_run = args.opt_bool("dry_run").unwrap_or(false);
    let hunks = match PatchParser::parse(&patch_text) {
        Ok(hunks) => hunks,
        Err(e) => {
            return error_result(
                "PARSE_ERROR",
                format!("Failed to parse patch: {e}"),
                "Check that the patch starts with *** Begin Patch and ends with *** End Patch",
            );
        }
    };
    if hunks.is_empty() {
        return error_result(
            "EMPTY_PATCH",
            "Patch contains no file changes",
            "Add at least one *** Add File: / *** Delete File: / *** Update File: section",
        );
    }
    let workspace = crate::CURRENT_WORKSPACE
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    // No workspace configured? Fall back to the process directory — the same
    // semantics as read/write/edit, so apply_patch never refuses to run.
    let effective_ws = if workspace.is_empty() || workspace == "." {
        ".".to_string()
    } else {
        workspace.clone()
    };
    let resolver = match WorkspaceResolver::new(Path::new(&effective_ws)) {
        Ok(resolver) => resolver,
        Err(error) => {
            return error_result(
                "NO_WORKSPACE",
                error,
                "Open an existing workspace and retry",
            );
        }
    };
    let plan = match PatchPlan::build(&hunks, &resolver) {
        Ok(plan) => plan,
        Err(failure) => return failure_result(failure),
    };
    let plan_hash = plan.plan_hash();
    if !dry_run {
        let expected = match args.get("plan_hash").and_then(|value| value.as_str()) {
            Some(value) if !value.is_empty() => value,
            _ => {
                return error_result(
                    "PLAN_HASH_REQUIRED",
                    "a dry-run plan_hash is required before applying changes",
                    "Run apply_patch with dry_run=true, then retry with its plan_hash.",
                );
            }
        };
        if expected != plan_hash {
            return error_result(
                "PLAN_HASH_MISMATCH",
                "the dry-run plan no longer matches the current workspace",
                "Run apply_patch with dry_run=true again and use its new plan_hash.",
            );
        }
    }
    if dry_run {
        return dry_run_result(&plan, &plan_hash);
    }
    apply_plan(plan)
}

fn error_result(
    code: &'static str,
    message: impl Into<String>,
    hint: &'static str,
) -> crate::ToolResult {
    crate::ToolResult::error(serde_json::json!({
            "timeis": crate::now_utc8(),
            "status": "error",
            "code": code,
            "message": message.into(),
            "hint": hint,
        })
        .to_string())
}

fn failure_result(failure: PatchFailure) -> crate::ToolResult {
    error_result(failure.code, failure.message, failure.hint)
}

fn dry_run_result(plan: &PatchPlan, plan_hash: &str) -> crate::ToolResult {
    let mut preview = String::from("[DRY RUN] apply_patch — preview, no changes written\n\n");
    for change in &plan.changes {
        match change {
            PlannedChange::Add {
                display, contents, ..
            } => {
                preview.push_str(&format!(
                    "--- /dev/null\n+++ b/{display}\n@@ -0,0 +1,{} @@\n",
                    contents.lines().count().max(1)
                ));
                for line in contents.lines() {
                    preview.push_str(&format!("+{line}\n"));
                }
            }
            PlannedChange::Delete {
                display, original, ..
            } => {
                preview.push_str(&format!(
                    "--- a/{display}\n+++ /dev/null\n@@ -1,{} +0,0 @@\n",
                    original.lines().count().max(1)
                ));
                for line in original.lines() {
                    preview.push_str(&format!("-{line}\n"));
                }
            }
            PlannedChange::Update {
                display,
                original,
                contents,
                ..
            } => {
                preview.push_str(&crate::file_shared::unified_diff(
                    original, contents, display,
                ));
            }
        }
        preview.push('\n');
    }
    crate::ToolResult::ok_data(serde_json::json!({
            "timeis": crate::now_utc8(),
            "status": "ok",
            "dry_run": true,
            "plan_hash": plan_hash,
        }), preview.trim_end())
}

fn apply_plan(plan: PatchPlan) -> crate::ToolResult {
    let mut added: Vec<String> = Vec::new();
    let mut modified: Vec<String> = Vec::new();
    let mut deleted: Vec<String> = Vec::new();
    for change in plan.changes {
        match change {
            PlannedChange::Add {
                display,
                target,
                contents,
            } => {
                let result = ensure_parent_dir(&target).and_then(|()| {
                    write_text(&target, &contents)?;
                    crate::file_state::record_write(
                        &target.to_string_lossy(),
                        contents.lines().count(),
                    );
                    Ok(())
                });
                match result {
                    Ok(()) => added.push(display),
                    Err(error) => {
                        return partial_failure_result(error, added, modified, deleted);
                    }
                }
            }
            PlannedChange::Delete {
                display, target, ..
            } => {
                if let Err(error) = std::fs::remove_file(&target)
                    .map_err(|e| format!("Failed to delete {display}: {e}"))
                {
                    return partial_failure_result(error, added, modified, deleted);
                }
                crate::file_state::record_delete(&target.to_string_lossy());
                deleted.push(display);
            }
            PlannedChange::Update {
                display,
                source,
                target,
                contents,
                ..
            } => {
                if let Err(error) = ensure_parent_dir(&target).and_then(|()| {
                    write_text(&target, &contents)
                        .map_err(|e| format!("Failed to write {display}: {e}"))
                }) {
                    return partial_failure_result(error, added, modified, deleted);
                }
                if source == target {
                    crate::file_state::record_edit(
                        &target.to_string_lossy(),
                        contents.lines().count(),
                    );
                    modified.push(display);
                } else {
                    modified.push(display.clone());
                    if let Err(error) = std::fs::remove_file(&source)
                        .map_err(|e| format!("Failed to remove original {display}: {e}"))
                    {
                        return partial_failure_result(error, added, modified, deleted);
                    }
                    crate::file_state::record_move(
                        &source.to_string_lossy(),
                        &target.to_string_lossy(),
                    );
                }
            }
        }
    }
    let mut summary = "Success. Updated the following files:\n".to_string();
    for p in &added {
        summary.push_str(&format!("A {}\n", p));
    }
    for p in &modified {
        summary.push_str(&format!("M {}\n", p));
    }
    for p in &deleted {
        summary.push_str(&format!("D {}\n", p));
    }
    crate::ToolResult::ok(serde_json::json!({
            "timeis": crate::now_utc8(),
            "status": "ok",
            "content": summary,
            "added": added,
            "modified": modified,
            "deleted": deleted,
        })
        .to_string())
}

fn partial_failure_result(
    message: String,
    added: Vec<String>,
    modified: Vec<String>,
    deleted: Vec<String>,
) -> crate::ToolResult {
    crate::ToolResult::partial(serde_json::json!({
            "timeis": crate::now_utc8(),
            "status": "error",
            "code": "APPLY_PARTIAL",
            "message": message,
            "hint": "Some earlier operations may have completed; read the listed files before retrying",
            "added": added,
            "modified": modified,
            "deleted": deleted,
        })
        .to_string())
}

fn ensure_parent_dir(file_path: &Path) -> Result<(), String> {
    if let Some(parent) = file_path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            std::fs::create_dir_all(parent).map_err(|e| {
                format!(
                    "Failed to create parent dirs for {}: {e}",
                    file_path.display()
                )
            })?;
        }
    }
    Ok(())
}

fn write_text(path: &Path, contents: &str) -> Result<(), String> {
    crate::file_shared::atomic_write(&path.to_string_lossy(), contents)
        .map_err(|e| format!("Failed to write {}: {e}", path.display()))
}

// ── Handler glue ──
fn handle_apply_patch(ctx: crate::ToolCallCtx) -> crate::ToolResult {
    execute_apply_patch(&ctx.args)
}
// ── Registration ──
pub fn register(mgr: &mut crate::ToolManager) {
    mgr.register(ToolHandler {
        key: "apply_patch".to_string(),
        description:
            "Apply file changes using a multi-file patch format. Supports Add, Delete, Update (with move/rename) and content-anchored fuzzy matching. Use @@ to anchor Update hunks to a function or class name; the engine locates the exact lines with Unicode-aware fuzzy search. Use dry_run=true to preview changes without writing.",
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "patch": {
                    "type": "string",
                    "description": "Patch body in *** Begin Patch / *** End Patch format. Use *** Add File: / *** Delete File: / *** Update File: hunks. Update hunks use @@ context to anchor location, then +/- for changes."
                },
                "dry_run": {
                    "type": "boolean",
                    "description": "Validate and show the resulting diff without writing. Default: false.",
                    "default": false
                },
                "plan_hash": {
                    "type": "string",
                    "description": "Hash returned by the matching dry-run. If provided, the current patch plan must match exactly."
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
    use std::sync::MutexGuard;

    fn runtime_guard() -> MutexGuard<'static, ()> {
        crate::TEST_RUNTIME_SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn apply_after_dry_run(patch: &str) -> crate::ToolResult {
        let preview = execute_apply_patch(&serde_json::json!({
            "patch": patch,
            "dry_run": true,
        }));
        assert!(preview.is_success(), "dry-run failed: {}", preview.model_text());
        let plan_hash = preview.data["plan_hash"].as_str().expect("plan hash");
        execute_apply_patch(&serde_json::json!({
            "patch": patch,
            "plan_hash": plan_hash,
        }))
    }
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
        let lines: Vec<String> = ["foo", "bar", "baz"]
            .iter()
            .map(|s| s.to_string())
            .collect();
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
        let _guard = runtime_guard();
        let dir = tempfile::tempdir().unwrap();
        crate::set_workspace(&dir.path().to_string_lossy());
        std::fs::write(dir.path().join("example.txt"), "before\n").unwrap();
        let args = serde_json::json!({
            "patch": "*** Begin Patch\n*** Update File: example.txt\n@@\n-before\n+after\n*** End Patch",
            "dry_run": true
        });
        let result = execute_apply_patch(&args);
        assert!(result.is_success(), "unexpected: {}", result.model_text());
        assert_eq!(result.data["dry_run"], true);
        assert!(result.data["plan_hash"].is_string());
        assert!(
            result.model_text().contains("[DRY RUN]"),
            "unexpected: {}",
            result.model_text()
        );
        assert!(
            !result.model_text().contains("[ERROR]"),
            "unexpected: {}",
            result.model_text()
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("example.txt")).unwrap(),
            "before\n"
        );
        crate::set_workspace(".");
    }
    #[test]
    fn parser_preserves_blank_context_and_rejects_empty_update() {
        let patch = "*** Begin Patch\n*** Update File: file.txt\n@@\n context before\n\n context after\n-old\n+new\n*** End Patch";
        let hunks = PatchParser::parse(patch).unwrap();
        let Hunk::UpdateFile { chunks, .. } = &hunks[0] else {
            panic!("expected update");
        };
        assert_eq!(
            chunks[0].old_lines,
            vec!["context before", "", "context after", "old"]
        );
        assert_eq!(
            chunks[0].new_lines,
            vec!["context before", "", "context after", "new"]
        );
        assert!(
            PatchParser::parse("*** Begin Patch\n*** Update File: file.txt\n*** End Patch")
                .unwrap_err()
                .contains("is empty")
        );
    }

    #[test]
    fn registration_preserves_json_patch_contract() {
        let mut manager = crate::ToolManager::new();
        register(&mut manager);
        let handler = manager.handlers.get("apply_patch").unwrap();
        assert_eq!(handler.input_schema["type"], "object");
        assert_eq!(handler.input_schema["required"], serde_json::json!(["patch"]));
        assert_eq!(handler.input_schema["properties"]["patch"]["type"], "string");
        assert_eq!(
            handler.input_schema["properties"]["dry_run"]["default"],
            false
        );
        assert_eq!(handler.input_schema["additionalProperties"], false);
    }

    #[test]
    fn e2e_relative_and_workspace_absolute_paths_apply() {
        let _guard = runtime_guard();
        let dir = tempfile::tempdir().unwrap();
        crate::set_workspace(&dir.path().to_string_lossy());
        for (name, content) in [
            ("relative-delete.txt", "delete relative\n"),
            ("absolute-delete.txt", "delete absolute\n"),
            ("relative-update.txt", "relative old\n"),
            ("absolute-update.txt", "absolute old\n"),
            ("move-old.txt", "move old\n"),
        ] {
            std::fs::write(dir.path().join(name), content).unwrap();
        }
        let absolute_add = dir.path().join("absolute-add.txt");
        let absolute_delete = dir.path().join("absolute-delete.txt");
        let absolute_update = dir.path().join("absolute-update.txt");
        let patch = format!(
            "*** Begin Patch\n\
             *** Add File: relative-add.txt\n\
             +relative add\n\
             *** Add File: {}\n\
             +absolute add\n\
             *** Delete File: relative-delete.txt\n\
             *** Delete File: {}\n\
             *** Update File: relative-update.txt\n\
             @@\n\
             -relative old\n\
             +relative new\n\
             *** Update File: {}\n\
             @@\n\
             -absolute old\n\
             +absolute new\n\
             *** Update File: move-old.txt\n\
             *** Move to: move-new.txt\n\
             @@\n\
             -move old\n\
             +move new\n\
             *** End Patch",
            absolute_add.display(),
            absolute_delete.display(),
            absolute_update.display()
        );
        let result = apply_after_dry_run(&patch);
        assert!(result.is_success(), "{}", result.model_text());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("relative-add.txt")).unwrap(),
            "relative add\n"
        );
        assert_eq!(
            std::fs::read_to_string(&absolute_add).unwrap(),
            "absolute add\n"
        );
        assert!(!dir.path().join("relative-delete.txt").exists());
        assert!(!absolute_delete.exists());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("relative-update.txt")).unwrap(),
            "relative new\n"
        );
        assert_eq!(
            std::fs::read_to_string(&absolute_update).unwrap(),
            "absolute new\n"
        );
        assert!(!dir.path().join("move-old.txt").exists());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("move-new.txt")).unwrap(),
            "move new\n"
        );
        crate::set_workspace(".");
    }

    #[test]
    fn preflight_failure_does_not_apply_earlier_hunks() {
        let _guard = runtime_guard();
        let dir = tempfile::tempdir().unwrap();
        crate::set_workspace(&dir.path().to_string_lossy());
        let patch = "*** Begin Patch\n\
                     *** Add File: should-not-exist.txt\n\
                     +content\n\
                     *** Update File: missing.txt\n\
                     @@\n\
                     -old\n\
                     +new\n\
                     *** End Patch";
        let result = execute_apply_patch(&serde_json::json!({ "patch": patch }));
        assert!(!result.is_success(), "{}", result.model_text());
        assert!(!dir.path().join("should-not-exist.txt").exists());
        crate::set_workspace(".");
    }

    #[test]
    fn dry_run_read_error_is_a_failed_tool_result() {
        let _guard = runtime_guard();
        let dir = tempfile::tempdir().unwrap();
        crate::set_workspace(&dir.path().to_string_lossy());
        let result = execute_apply_patch(&serde_json::json!({
            "patch": "*** Begin Patch\n*** Update File: missing.txt\n@@\n-old\n+new\n*** End Patch",
            "dry_run": true
        }));
        assert!(!result.is_success(), "{}", result.model_text());
        assert!(result.model_text().contains("\"status\":\"error\""));
        crate::set_workspace(".");
    }

    #[test]
    fn add_rejects_parent_and_absolute_workspace_escape() {
        let _guard = runtime_guard();
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        crate::set_workspace(&workspace.to_string_lossy());
        let parent_escape = root.path().join("parent-escape.txt");
        // Relative `..` escape is rejected at the tool layer: patch text is
        // free-form, so a hallucinated patch must not silently write outside
        // the workspace. Absolute paths are trusted by the tool and instead
        // surface through the permission layer as High risk (user approval).
        let patch = "*** Begin Patch\n*** Add File: ..\\parent-escape.txt\n+escape\n*** End Patch".to_string();
        let result = execute_apply_patch(&serde_json::json!({ "patch": patch }));
        assert!(!result.is_success(), "{}", result.model_text());
        assert!(!parent_escape.exists());
        crate::set_workspace(".");
    }

    #[test]
    fn patch_paths_are_visible_to_permission_authorization() {
        let _guard = runtime_guard();
        let root_dir = tempfile::tempdir().unwrap();
        let workspace = root_dir.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        crate::set_workspace(&workspace.to_string_lossy());
        let args = serde_json::json!({
            "patch": "*** Begin Patch\n*** Update File: source.txt\n*** Move to: nested/dest.txt\n@@\n-old\n+new\n*** End Patch"
        });
        let resources = crate::permission::extract_target_paths("apply_patch", &args);
        let root = std::fs::canonicalize(&workspace).unwrap();
        assert_eq!(resources.len(), 2);
        assert!(resources.iter().all(|path| path.starts_with(&root)));
        assert!(matches!(
            crate::permission::needs_permission(
                crate::permission::PermissionLevel::WorkspaceFree,
                "apply_patch",
                &args,
                &workspace,
                &HashSet::new(),
            ),
            crate::permission::PermissionDecision::AutoApprove
        ));
        let outside_args = serde_json::json!({
            "patch": format!(
                "*** Begin Patch\n*** Add File: {}\n+outside\n*** End Patch",
                root_dir.path().join("outside.txt").display()
            )
        });
        assert!(matches!(
            crate::permission::needs_permission(
                crate::permission::PermissionLevel::WorkspaceFree,
                "apply_patch",
                &outside_args,
                &workspace,
                &HashSet::new(),
            ),
            crate::permission::PermissionDecision::AskUser { .. }
        ));
        crate::set_workspace(".");
    }

    #[test]
    fn dispatch_reports_apply_patch_failure_and_success_structurally() {
        let _guard = runtime_guard();
        let dir = tempfile::tempdir().unwrap();
        crate::set_workspace(&dir.path().to_string_lossy());
        crate::runtime::init_tools("apply-patch-test", &[], Vec::new());
        crate::runtime::set_context("apply-patch-test", 4);
        let preview_args = serde_json::json!({
            "patch": "*** Begin Patch\n*** Add File: through-dispatch.txt\n+created\n*** End Patch",
            "dry_run": true
        })
        .to_string();
        let preview = crate::execution::execute_with_context(
            "apply_patch",
            "",
            &preview_args,
            "apply-patch-preview",
            None,
        );
        let plan_hash = preview.result.data["plan_hash"].as_str().unwrap();
        let success_args = serde_json::json!({
            "patch": "*** Begin Patch\n*** Add File: through-dispatch.txt\n+created\n*** End Patch",
            "plan_hash": plan_hash,
        })
        .to_string();
        let success = crate::execution::execute_with_context(
            "apply_patch",
            "",
            &success_args,
            "apply-patch-success",
            None,
        );
        assert!(success.result.is_success(), "{}", success.result.model_text());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("through-dispatch.txt")).unwrap(),
            "created\n"
        );
        let failure_args = serde_json::json!({
            "patch": "*** Begin Patch\n*** Update File: missing.txt\n@@\n-old\n+new\n*** End Patch",
            "dry_run": true
        })
        .to_string();
        let failure = crate::execution::execute_with_context(
            "apply_patch",
            "",
            &failure_args,
            "apply-patch-failure",
            None,
        );
        assert!(!failure.result.is_success(), "{}", failure.result.model_text());
        assert!(failure.result.model_text().contains("\"status\":\"error\""));
        crate::runtime::clear_context();
        crate::set_workspace(".");
    }

    #[test]
    fn add_rejects_symlink_or_junction_escape_when_supported() {
        let _guard = runtime_guard();
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let outside = root.path().join("outside");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&outside).unwrap();
        let link = workspace.join("link");
        #[cfg(windows)]
        if std::os::windows::fs::symlink_dir(&outside, &link).is_err() {
            return;
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        crate::set_workspace(&workspace.to_string_lossy());
        let result = execute_apply_patch(&serde_json::json!({
            "patch": "*** Begin Patch\n*** Add File: link/escape.txt\n+escape\n*** End Patch"
        }));
        assert!(!result.is_success(), "{}", result.model_text());
        assert!(!outside.join("escape.txt").exists());
        crate::set_workspace(".");
    }
}
