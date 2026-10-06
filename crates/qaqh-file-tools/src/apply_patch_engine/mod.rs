//! Codex-style apply-patch engine (ported from OpenAI `codex-rs/apply-patch`,
//! Apache-2.0). Self-contained parser + content matcher for the
//! `*** Begin Patch` patch format — NO line numbers, NO unified-diff headers.
//!
//! Ported surface (sync, std-fs, PathBuf instead of PathUri/ExecutorFileSystem):
//! - `parser.rs`     — boundary checks + lenient heredoc stripping
//! - `streaming_parser.rs` — hunk state machine (lines → Hunk list)
//! - `seek_sequence.rs`    — 4-tier content matching (exact → rstrip → trim → Unicode-normalised)
//! - `file_update.rs`      — chunk → replacements → new contents
//! - `text_file.rs`        — line-ending-preserving source file abstraction
//!
//! Update semantics: all hunks located on the CURRENT file state, applied in
//! order; hunks are applied one at a time, so any failure leaves the already
//! written files in place (non-atomic — see [`EngineError::Partial`]). Paths
//! resolve relative to the caller-supplied cwd.

mod file_update;
mod parser;
mod seek_sequence;
mod streaming_parser;
mod text_file;

use std::fmt;
use std::path::{Path, PathBuf};

pub use file_update::{AppliedPatch, derive_new_contents_from_chunks};
pub use parser::{Hunk, ParseError, UpdateFileChunk, parse_patch, patch_stats};
pub use streaming_parser::{PatchStats, StreamingPatchParser};

/// Controls how updates reconstruct the target file after matching a patch.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum UpdateMode {
    /// Preserve the historical behavior of normalizing updated files to LF
    /// (matches the workspace LF-canonical-view contract of read/hash).
    #[default]
    NormalizeToLf,
    /// Preserve existing line endings and use the file's preferred ending for new lines.
    PreserveLineEndings,
}

#[derive(Debug)]
pub enum EngineError {
    Parse(ParseError),
    Io {
        context: String,
        source: std::io::Error,
    },
    Compute(String),
    EmptyPatch,
    /// 词法上在工作区内、canonicalize 后逃逸出边界的 patch 路径（workspace 内
    /// 符号链接指向外部）。这类路径 admit 层看到的是工作区内路径、不会提权，
    /// 属不可审批的隐蔽逃逸，引擎保持硬拒。词法上就在工作区外的路径（绝对
    /// 路径 / `..` 逃逸）走 admit 层提权后照常落盘，不产生此错误。
    PathOutsideWorkspace {
        path: String,
    },
    /// 目标路径的最终组件是符号链接：按策略拒绝（不替换链接、不穿透写）。
    SymlinkTarget {
        path: String,
        target: String,
    },
    /// `*** Add File:` targeted a path that already exists. Dry-run reports this
    /// instead of a plain `[DRY RUN] … ok`; a real apply keeps the upstream
    /// overwrite semantics (fixture `011_add_overwrites_existing_file`) but the
    /// replaced contents are captured in [`FileDelta::old`].
    WouldOverwrite {
        path: String,
    },
    /// A hunk failed after earlier hunks were already written to disk (hunks are
    /// applied one at a time — non-atomic, inherited from upstream codex). Carries
    /// the already-applied and not-yet-applied patch paths so the tool layer can
    /// state the facts instead of claiming "no partial application happened".
    Partial {
        error: Box<EngineError>,
        applied: Vec<String>,
        not_applied: Vec<String>,
    },
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EngineError::Parse(e) => write!(f, "invalid patch: {e}"),
            EngineError::Io { context, source } => write!(f, "{context}: {source}"),
            EngineError::Compute(msg) => write!(f, "{msg}"),
            EngineError::EmptyPatch => write!(f, "no changes to apply"),
            EngineError::PathOutsideWorkspace { path } => {
                write!(f, "patch path resolves outside the workspace: {path}")
            }
            EngineError::SymlinkTarget { path, target } => write!(
                f,
                "refusing to patch '{path}': it is a symbolic link to '{target}'; use the target path directly"
            ),
            EngineError::WouldOverwrite { path } => {
                write!(f, "Add File target already exists: {path}")
            }
            EngineError::Partial {
                error,
                applied,
                not_applied,
            } => write!(
                f,
                "{error}\nNOTE: hunks are applied one at a time and are NOT atomic — already applied: {}; not applied: {}. Fix and re-send ONLY the failed part, or verify the already-applied files with `git diff`/read before re-sending.",
                applied.join(", "),
                not_applied.join(", ")
            ),
        }
    }
}

impl EngineError {
    /// Attach "already applied / not applied" patch paths to a mid-patch
    /// failure. Returns the error unchanged when nothing was written yet (the
    /// first hunk failed), so the existing single-hunk failure surface — and its
    /// error variants — stay exactly as before.
    fn with_partial(self, applied: Vec<String>, not_applied: Vec<String>) -> Self {
        if applied.is_empty() {
            self
        } else {
            EngineError::Partial {
                error: Box::new(self),
                applied,
                not_applied,
            }
        }
    }
}

impl From<ParseError> for EngineError {
    fn from(e: ParseError) -> Self {
        EngineError::Parse(e)
    }
}

impl From<std::io::Error> for EngineError {
    fn from(err: std::io::Error) -> Self {
        EngineError::Io {
            context: "I/O error".to_string(),
            source: err,
        }
    }
}

/// Per-file outcome of an applied patch (paths keep the patch's spelling).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct AffectedPaths {
    pub added: Vec<String>,
    pub modified: Vec<String>,
    pub deleted: Vec<String>,
}

impl AffectedPaths {
    pub fn all(&self) -> impl Iterator<Item = &String> {
        self.added.iter().chain(&self.modified).chain(&self.deleted)
    }
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.modified.is_empty() && self.deleted.is_empty()
    }
}

/// Per-file delta produced while applying (paths keep the patch's spelling;
/// contents are the actual bytes read/written — used for stats and ledger sync).
#[derive(Debug, Clone, PartialEq)]
pub struct FileDelta {
    pub path: String,
    /// 解析后的绝对路径（`resolve_workspace_path` 结果）：工具侧账本
    /// （file_state）的唯一键。`path` 保留补丁书写形态仅用于展示。
    pub resolved_path: String,
    pub old: Option<String>,
    pub new: Option<String>,
}

/// Outcome of applying (or dry-running) a patch.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ApplyOutcome {
    pub affected: AffectedPaths,
    pub deltas: Vec<FileDelta>,
}

/// Resolve a hunk path against the workspace root. Relative paths are joined to
/// `cwd`; absolute paths are used as-is. Anything escaping the workspace root is
/// rejected (the tool's permission model is workspace-bounded).
pub(crate) fn resolve_workspace_path(cwd: &Path, path: &Path) -> Result<PathBuf, EngineError> {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    // BUG-2026-09-13-01：入口先词法消解 `..`（对齐 Codex PathUri：词法消解且
    // clamp 在锚点内）。此前依赖 exists()/canonicalize 的折叠巧合拦截逃逸；
    // 当父链无法解析时 `_ => joined.clone()` 兜底会放行含未消解 `..` 的路径，
    // 落盘时 OS 再解析就可能越过 workspace 边界。归一化后兜底分支结构上
    // 不可能携带 `..`，前缀比较（组件感知）也不会被字面 `..` 干扰。
    let joined = {
        use crate::permission::normalize_lexically;
        normalize_lexically(&joined)
    };
    // 策略：最终组件是符号链接时拒绝（不替换链接、不穿透写），
    // 必须在 canonicalize 之前检查——canonicalize 会把链接解析成目标。
    if let Ok(meta) = std::fs::symlink_metadata(&joined)
        && meta.file_type().is_symlink()
    {
        let target = std::fs::read_link(&joined)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| "<unresolved>".to_string());
        return Err(EngineError::SymlinkTarget {
            path: joined.to_string_lossy().to_string(),
            target,
        });
    }
    // Canonicalize the parent so `..` escapes are caught; the file itself may
    // not exist yet (Add), so canonicalize the deepest existing ancestor.
    let abs = if joined.exists() {
        joined.canonicalize().unwrap_or(joined.clone())
    } else {
        match joined.parent() {
            Some(parent) if parent.exists() => {
                let canon_parent = parent
                    .canonicalize()
                    .unwrap_or_else(|_| parent.to_path_buf());
                canon_parent.join(joined.file_name().unwrap_or_default())
            }
            _ => joined.clone(),
        }
    };
    let cwd_abs = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    // Windows canonicalize returns a `\\?\`-prefixed verbatim path; strip it on
    // both sides so the prefix comparison is meaningful.
    let strip_verbatim = |p: &Path| -> PathBuf {
        let s = p.to_string_lossy();
        let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
        PathBuf::from(s)
    };
    // 2026-10-05 权限规则：跨工作区写由 admit 层提权（patch 头路径已全部提取进
    // 授权资源），引擎不再按 workspace 边界硬拒——词法上就在工作区外的路径
    // （绝对路径 / `..` 逃逸，`joined` 已词法归一）在审批后照常落盘。
    //
    // 唯一保留的拒绝：**词法在工作区内、canonicalize 后逃逸**的路径（workspace
    // 内符号链接指向外部）。这类路径 admit 看到的是工作区内路径、不会触发提权，
    // 引擎若放行即 fail-open 写穿边界——与 symlink 最终组件拒绝同族，保持硬拒。
    let lexically_inside = strip_verbatim(&joined).starts_with(strip_verbatim(&cwd_abs));
    if lexically_inside && !strip_verbatim(&abs).starts_with(strip_verbatim(&cwd_abs)) {
        return Err(EngineError::PathOutsideWorkspace {
            path: joined.to_string_lossy().to_string(),
        });
    }
    Ok(abs)
}

/// Apply a `*** Begin Patch` patch to the workspace rooted at `cwd`.
pub fn apply_patch_engine(
    patch: &str,
    cwd: &Path,
    mode: UpdateMode,
) -> Result<ApplyOutcome, EngineError> {
    let hunks = parse_patch(patch)?.hunks;
    if hunks.is_empty() {
        return Err(EngineError::EmptyPatch);
    }
    let mut outcome = ApplyOutcome::default();
    for (idx, hunk) in hunks.iter().enumerate() {
        if let Err(err) = apply_hunk(hunk, cwd, mode, &mut outcome) {
            // Hunks are written one at a time (non-atomic, inherited from
            // upstream): everything already in `outcome` is on disk. Report it
            // with the error instead of letting the caller claim otherwise.
            let applied = outcome.deltas.iter().map(|d| d.path.clone()).collect();
            let not_applied = hunks[idx..]
                .iter()
                .map(|h| h.path().to_string_lossy().to_string())
                .collect();
            return Err(err.with_partial(applied, not_applied));
        }
    }
    Ok(outcome)
}

/// Apply a single hunk to disk, appending its delta to `outcome`.
fn apply_hunk(
    hunk: &Hunk,
    cwd: &Path,
    mode: UpdateMode,
    outcome: &mut ApplyOutcome,
) -> Result<(), EngineError> {
    let affected_path = hunk.path().to_string_lossy().to_string();
    let resolved = resolve_workspace_path(cwd, hunk.path())?;
    match hunk {
        Hunk::AddFile { contents, .. } => {
            // `*** Add File:` keeps upstream's overwrite semantics (fixture
            // 011_add_overwrites_existing_file), but the replaced contents must
            // be captured — upstream reads them into `overwritten_content`
            // (codex-rs/apply-patch/src/lib.rs:508-533) so the delta carries
            // rollback material. This port dropped that into `old: None`, which
            // made the overwrite silent (BUG-2026-09-16-10).
            let old = existing_file_contents(&resolved);
            write_file_with_missing_parent_retry(&resolved, contents.as_bytes())?;
            outcome.affected.added.push(affected_path.clone());
            outcome.deltas.push(FileDelta {
                path: affected_path,
                resolved_path: resolved.to_string_lossy().to_string(),
                old,
                new: Some(contents.clone()),
            });
        }
        Hunk::DeleteFile { .. } => {
            let old = std::fs::read_to_string(&resolved).ok();
            let meta = std::fs::metadata(&resolved).map_err(|e| EngineError::Io {
                context: format!("Failed to delete file {}", resolved.to_string_lossy()),
                source: e,
            })?;
            if meta.is_dir() {
                return Err(EngineError::Io {
                    context: format!(
                        "Failed to delete file {}: path is a directory",
                        resolved.to_string_lossy()
                    ),
                    source: std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "path is a directory",
                    ),
                });
            }
            std::fs::remove_file(&resolved).map_err(|e| EngineError::Io {
                context: format!("Failed to delete file {}", resolved.to_string_lossy()),
                source: e,
            })?;
            outcome.affected.deleted.push(affected_path.clone());
            outcome.deltas.push(FileDelta {
                path: affected_path,
                resolved_path: resolved.to_string_lossy().to_string(),
                old,
                new: None,
            });
        }
        Hunk::UpdateFile {
            path: src_path,
            move_path,
            chunks,
        } => {
            let resolved = resolve_workspace_path(cwd, src_path)?;
            let applied =
                derive_new_contents_from_chunks(&resolved, chunks, mode).map_err(|e| match e {
                    EngineError::Compute(msg) => EngineError::Compute(format!(
                        "Failed to update file {}: {msg}",
                        resolved.to_string_lossy()
                    )),
                    other => other,
                })?;
            if let Some(dest) = move_path {
                let dest_resolved = resolve_workspace_path(cwd, dest)?;
                write_file_with_missing_parent_retry(
                    &dest_resolved,
                    applied.new_contents.as_bytes(),
                )?;
                let meta = std::fs::metadata(&resolved).map_err(|e| EngineError::Io {
                    context: format!("Failed to remove original {}", resolved.to_string_lossy()),
                    source: e,
                })?;
                if meta.is_dir() {
                    return Err(EngineError::Io {
                        context: format!(
                            "Failed to remove original {}: path is a directory",
                            resolved.to_string_lossy()
                        ),
                        source: std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "path is a directory",
                        ),
                    });
                }
                std::fs::remove_file(&resolved).map_err(|e| EngineError::Io {
                    context: format!("Failed to remove original {}", resolved.to_string_lossy()),
                    source: e,
                })?;
            } else {
                std::fs::write(&resolved, applied.new_contents.as_bytes()).map_err(|e| {
                    EngineError::Io {
                        context: format!("Failed to write file {}", resolved.to_string_lossy()),
                        source: e,
                    }
                })?;
            }
            outcome.affected.modified.push(affected_path.clone());
            outcome.deltas.push(FileDelta {
                path: affected_path,
                resolved_path: resolved.to_string_lossy().to_string(),
                old: Some(applied.original_contents),
                new: Some(applied.new_contents),
            });
        }
    }
    Ok(())
}

/// Dry-run: parse and fully compute every hunk against the current file state,
/// but write nothing. It surfaces the failures of a read-only pre-check, but
/// cannot foresee failures that depend on earlier hunks having been written
/// (e.g. delete then update the same file), so a real apply may still differ.
pub fn dry_run_patch_engine(patch: &str, cwd: &Path) -> Result<ApplyOutcome, EngineError> {
    let hunks = parse_patch(patch)?.hunks;
    if hunks.is_empty() {
        return Err(EngineError::EmptyPatch);
    }
    let mut outcome = ApplyOutcome::default();
    for hunk in &hunks {
        let affected_path = hunk.path().to_string_lossy().to_string();
        let resolved = resolve_workspace_path(cwd, hunk.path())?;
        match hunk {
            Hunk::AddFile { contents, .. } => {
                if std::fs::metadata(&resolved).is_ok_and(|m| m.is_dir()) {
                    return Err(EngineError::Io {
                        context: format!(
                            "Cannot add file {}: path is a directory",
                            resolved.to_string_lossy()
                        ),
                        source: std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "path is a directory",
                        ),
                    });
                }
                // Dry-run must not report a plain `[DRY RUN] … ok` for a patch
                // that would replace an existing file wholesale
                // (BUG-2026-09-16-10).
                if std::fs::metadata(&resolved).is_ok() {
                    return Err(EngineError::WouldOverwrite {
                        path: resolved.to_string_lossy().to_string(),
                    });
                }
                outcome.affected.added.push(affected_path.clone());
                outcome.deltas.push(FileDelta {
                    path: affected_path,
                    resolved_path: resolved.to_string_lossy().to_string(),
                    old: None,
                    new: Some(contents.clone()),
                });
            }
            Hunk::DeleteFile { .. } => {
                let old = std::fs::read_to_string(&resolved).ok();
                let meta = std::fs::metadata(&resolved).map_err(|e| EngineError::Io {
                    context: format!("Failed to delete file {}", resolved.to_string_lossy()),
                    source: e,
                })?;
                if meta.is_dir() {
                    return Err(EngineError::Io {
                        context: format!(
                            "Failed to delete file {}: path is a directory",
                            resolved.to_string_lossy()
                        ),
                        source: std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "path is a directory",
                        ),
                    });
                }
                outcome.affected.deleted.push(affected_path.clone());
                outcome.deltas.push(FileDelta {
                    path: affected_path,
                    resolved_path: resolved.to_string_lossy().to_string(),
                    old,
                    new: None,
                });
            }
            Hunk::UpdateFile { chunks, .. } => {
                // Full compute (read + match + rebuild) — same failure surface
                // as a real apply, minus the write.
                let applied =
                    derive_new_contents_from_chunks(&resolved, chunks, UpdateMode::default())
                        .map_err(|e| match e {
                            EngineError::Compute(msg) => EngineError::Compute(format!(
                                "Failed to update file {}: {msg}",
                                resolved.to_string_lossy()
                            )),
                            other => other,
                        })?;
                outcome.affected.modified.push(affected_path.clone());
                outcome.deltas.push(FileDelta {
                    path: affected_path,
                    resolved_path: resolved.to_string_lossy().to_string(),
                    old: Some(applied.original_contents),
                    new: Some(applied.new_contents),
                });
            }
        }
    }
    Ok(outcome)
}

/// Contents of `path` when it is an existing regular file, `None` otherwise
/// (missing, a directory, or not valid UTF-8). Used by `*** Add File:` to keep
/// the overwritten contents as rollback material.
fn existing_file_contents(path: &Path) -> Option<String> {
    std::fs::metadata(path).ok().filter(|m| m.is_file())?;
    std::fs::read_to_string(path).ok()
}

fn write_file_with_missing_parent_retry(path: &Path, contents: &[u8]) -> Result<(), EngineError> {
    match std::fs::write(path, contents) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| EngineError::Io {
                    context: format!(
                        "Failed to create parent directories for {}",
                        path.to_string_lossy()
                    ),
                    source: e,
                })?;
            }
            std::fs::write(path, contents).map_err(|e| EngineError::Io {
                context: format!("Failed to write file {}", path.to_string_lossy()),
                source: e,
            })
        }
        Err(err) => Err(EngineError::Io {
            context: format!("Failed to write file {}", path.to_string_lossy()),
            source: err,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn add_file_patch(path: &str) -> String {
        format!("*** Begin Patch\n*** Add File: {path}\n+new\n*** End Patch\n")
    }

    /// T-3-1 / BUG-2026-09-16-10：覆盖保持上游语义，但旧内容必须进
    /// `FileDelta.old`（回滚素材），不再丢成 `None`。
    #[test]
    fn add_file_overwrite_records_previous_contents() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "old one\nold two\n").unwrap();
        let outcome =
            apply_patch_engine(&add_file_patch("a.txt"), dir.path(), UpdateMode::default())
                .expect("overwrite keeps upstream semantics");
        assert_eq!(outcome.deltas.len(), 1);
        assert_eq!(outcome.deltas[0].old.as_deref(), Some("old one\nold two\n"));
        assert_eq!(outcome.deltas[0].new.as_deref(), Some("new\n"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "new\n"
        );
    }

    /// T-3-1：dry_run 对已存在路径必须显式 `WOULD_OVERWRITE`，且不落盘。
    #[test]
    fn dry_run_reports_would_overwrite_for_existing_add_target() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "old\n").unwrap();
        let err = dry_run_patch_engine(&add_file_patch("a.txt"), dir.path()).unwrap_err();
        assert!(
            matches!(err, EngineError::WouldOverwrite { .. }),
            "got: {err:?}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "old\n"
        );
    }

    // ── N-4①（PR #87 评审附带观察）：hunk 头里的路径不得是另一条 marker 行 ──
    //
    // 修前实测：`*** Add File: *** End Patch` 会把 marker 行当作文件名，引擎返回
    // `Ok` 并**真的建出一个叫 `*** End Patch` 的文件**（探针跑在 2026-09-17，
    // 目录列表出现 `["*** End Patch: bar.txt", "*** End Patch"]`）。

    /// Add/Delete/Update/Move to 四种头都必须拒绝「路径 = 另一条 `***` marker」。
    #[test]
    fn marker_line_is_rejected_as_a_hunk_path() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
        let cases = [
            "*** Begin Patch\n*** Add File: *** End Patch\n+x\n*** End Patch\n",
            "*** Begin Patch\n*** Delete File: *** End Patch\n*** End Patch\n",
            "*** Begin Patch\n*** Update File: *** End Patch\n@@\n-alpha\n+ALPHA\n*** End Patch\n",
            "*** Begin Patch\n*** Update File: a.txt\n*** Move to: *** End Patch\n@@\n-alpha\n+ALPHA\n*** End Patch\n",
        ];
        for patch in cases {
            let err = apply_patch_engine(patch, dir.path(), UpdateMode::default())
                .expect_err(&format!("marker path must be rejected: {patch}"));
            assert!(
                matches!(err, EngineError::Parse(_)),
                "expected a parse error, got: {err:?}"
            );
        }
        // 没有任何「marker 文件名」落到盘上，源文件也没被动过。
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "alpha\n"
        );
        let names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["a.txt".to_string()], "got: {names:?}");
    }

    /// 空路径本来就被 marker 的尾随空格卡住（`*** Add File: ` 去掉尾空格后
    /// 不再匹配 `"*** Add File: "`），这里把该行为锁住——评审怀疑它会把
    /// workspace 根 `pop()` 掉，实测不会（N-4②）。
    #[test]
    fn empty_hunk_path_is_a_parse_error() {
        let dir = tempdir().unwrap();
        for patch in [
            "*** Begin Patch\n*** Add File: \n+x\n*** End Patch\n",
            "*** Begin Patch\n*** Delete File: \n*** End Patch\n",
        ] {
            let err = apply_patch_engine(patch, dir.path(), UpdateMode::default())
                .expect_err(&format!("empty path must be rejected: {patch}"));
            assert!(
                matches!(err, EngineError::Parse(_)),
                "expected a parse error, got: {err:?}"
            );
        }
        // workspace 根还在（没有被 `pop()` 掉）。
        assert!(dir.path().exists());
    }

    /// 控制用例：只有 `*** End Patch` 的「补丁」既不是合法开头也不是合法结尾，
    /// 必须是 `PARSE_ERROR`（评审声称它现在返回 Ok，实测不成立）。
    #[test]
    fn end_marker_alone_is_a_parse_error() {
        let dir = tempdir().unwrap();
        for patch in [
            "*** End Patch\n+x\n*** End Patch\n",
            "*** Begin Patch\n*** Add File: c.txt\n+x\n*** End Patch\n*** Add File: d.txt\n+y\n*** End Patch\n",
        ] {
            let err = apply_patch_engine(patch, dir.path(), UpdateMode::default())
                .expect_err(&format!("must not parse: {patch}"));
            assert!(
                matches!(err, EngineError::Parse(_)),
                "expected a parse error, got: {err:?}"
            );
        }
        assert!(!dir.path().join("c.txt").exists());
        assert!(!dir.path().join("d.txt").exists());
    }

    /// 符号链接 workspace + 绝对路径：合法请求不得被拒（N-4③，评审怀疑
    /// `joined.exists()` 的 canonicalize 与原 cwd 比对会误拒）。
    // std::os::unix::fs::symlink 只在 Unix 存在；Windows 下本测试整体跳过，
    // 链接语义（canonicalize 解析链接、逃逸拒绝）由 CI 的 Linux leg 覆盖。
    #[cfg(unix)]
    #[test]
    fn symlinked_workspace_accepts_absolute_paths() {
        let dir = tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("a.txt"), "alpha\n").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        // cwd 走链接，patch 用绝对真路径：改得动。
        let patch = format!(
            "*** Begin Patch\n*** Update File: {}\n@@\n-alpha\n+ALPHA\n*** End Patch\n",
            real.join("a.txt").display()
        );
        apply_patch_engine(&patch, &link, UpdateMode::default()).expect("link cwd + real abs path");
        assert_eq!(
            std::fs::read_to_string(real.join("a.txt")).unwrap(),
            "ALPHA\n"
        );

        // cwd 走真路径，patch 用绝对链接路径：同样改得动。
        let patch = format!(
            "*** Begin Patch\n*** Update File: {}\n@@\n-ALPHA\n+ALPHA2\n*** End Patch\n",
            link.join("a.txt").display()
        );
        apply_patch_engine(&patch, &real, UpdateMode::default()).expect("real cwd + link abs path");
        assert_eq!(
            std::fs::read_to_string(real.join("a.txt")).unwrap(),
            "ALPHA2\n"
        );

        // 但逃逸仍然被拒：workspace 内的链接指向外部时写不进去。
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let escape = real.join("escape");
        std::os::unix::fs::symlink(&outside, &escape).unwrap();
        let patch = "*** Begin Patch\n*** Add File: escape/leak.txt\n+x\n*** End Patch\n";
        let err = apply_patch_engine(patch, &real, UpdateMode::default()).unwrap_err();
        assert!(
            matches!(err, EngineError::PathOutsideWorkspace { .. }),
            "got: {err:?}"
        );
        assert!(!outside.join("leak.txt").exists());
    }

    /// T-3-1：不存在的路径照常 dry-run 通过。
    #[test]
    fn dry_run_allows_add_of_missing_path() {
        let dir = tempdir().unwrap();
        let outcome = dry_run_patch_engine(&add_file_patch("fresh.txt"), dir.path()).unwrap();
        assert_eq!(outcome.affected.added, vec!["fresh.txt".to_string()]);
        assert!(!dir.path().join("fresh.txt").exists());
    }

    /// 2026-10-05 权限规则：词法上就在工作区外的路径（绝对路径 / `..` 逃逸）
    /// 由 admit 层提权后照常落盘——引擎不再按 workspace 边界硬拒。
    #[test]
    fn cross_workspace_target_applies_after_admission_approval() {
        let dir = tempdir().unwrap();
        let ws = dir.path().join("ws");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(&outside).unwrap();

        // 绝对路径：admit 从 patch 头提取到外部路径 → 提权 → 引擎放行。
        let patch = format!(
            "*** Begin Patch\n*** Add File: {}\n+cross\n*** End Patch\n",
            outside.join("new.txt").display()
        );
        apply_patch_engine(&patch, &ws, UpdateMode::default())
            .expect("lexically-outside absolute path must apply (admission owns the boundary)");
        assert_eq!(
            std::fs::read_to_string(outside.join("new.txt")).unwrap(),
            "cross\n"
        );

        // `..` 逃逸：词法归一后落在工作区外 → 同样放行。
        let escape = "*** Begin Patch\n*** Update File: ../outside/new.txt\n@@\n-cross\n+cross2\n*** End Patch\n";
        apply_patch_engine(escape, &ws, UpdateMode::default())
            .expect("'..' escape is lexically outside → admission-approved write applies");
        assert_eq!(
            std::fs::read_to_string(outside.join("new.txt")).unwrap(),
            "cross2\n"
        );
    }

    /// `patch_stats`（统计通道）与 `parse_patch`（执行通道）的接受/拒绝面必须
    /// 完全一致：执行成功的补丁不能数不出行数，执行拒掉的补丁也不能报出数字。
    #[test]
    fn patch_stats_accepts_exactly_what_parse_patch_accepts() {
        let cases = [
            add_file_patch("a.txt"),
            // heredoc 包裹靠 lenient 分支放行，两边都得认。
            "<<'EOF'\n*** Begin Patch\n*** Add File: a.txt\n+x\n*** End Patch\nEOF\n".to_string(),
            "*** Begin Patch\n*** Add File: a.txt\n+x\n".to_string(), // 缺 End Patch
            "not a patch at all".to_string(),
            String::new(),
        ];
        for patch in &cases {
            assert_eq!(
                patch_stats(patch).is_ok(),
                parse_patch(patch).is_ok(),
                "stats/parse disagree on: {patch:?}"
            );
        }
        let stats = patch_stats(&add_file_patch("a.txt")).unwrap();
        assert_eq!(
            (
                stats.lines_added,
                stats.files_created,
                stats.single_path.as_deref()
            ),
            (1, 1, Some(Path::new("a.txt")))
        );
    }

    /// T-3-2：失败发生在第 2 个 hunk 时，错误必须携带「已生效 / 未生效」。
    #[test]
    fn partial_failure_carries_applied_paths() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "beta\n").unwrap();
        let patch = "\
*** Begin Patch
*** Update File: a.txt
@@
-alpha
+ALPHA
*** Update File: b.txt
@@
-ABSENT
+whatever
*** End Patch
";
        let err = apply_patch_engine(patch, dir.path(), UpdateMode::default()).unwrap_err();
        match err {
            EngineError::Partial {
                applied,
                not_applied,
                ..
            } => {
                assert_eq!(applied, vec!["a.txt".to_string()]);
                assert_eq!(not_applied, vec!["b.txt".to_string()]);
            }
            other => panic!("expected Partial, got {other:?}"),
        }
        // 1 号 hunk 确实已落盘（非原子，与上游一致）
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "ALPHA\n"
        );
    }

    /// T-3-2：首个 hunk 就失败时不加 `Partial` 包装（单 hunk 失败面保持不变）。
    #[test]
    fn first_hunk_failure_is_not_wrapped() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
        let patch =
            "*** Begin Patch\n*** Update File: a.txt\n@@\n-ABSENT\n+whatever\n*** End Patch\n";
        let err = apply_patch_engine(patch, dir.path(), UpdateMode::default()).unwrap_err();
        assert!(
            matches!(err, EngineError::Compute(_)),
            "expected plain Compute, got {err:?}"
        );
    }
}
