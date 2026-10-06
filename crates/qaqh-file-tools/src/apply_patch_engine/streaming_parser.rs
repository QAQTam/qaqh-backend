//! Streaming line-by-line state machine for the `*** Begin Patch` format.
//! Ported from OpenAI `codex-rs/apply-patch` (Apache-2.0).

use std::path::{Path, PathBuf};

use crate::apply_patch_engine::parser::ADD_FILE_MARKER;
use crate::apply_patch_engine::parser::BEGIN_PATCH_MARKER;
use crate::apply_patch_engine::parser::CHANGE_CONTEXT_MARKER;
use crate::apply_patch_engine::parser::DELETE_FILE_MARKER;
use crate::apply_patch_engine::parser::EMPTY_CHANGE_CONTEXT_MARKER;
use crate::apply_patch_engine::parser::END_PATCH_MARKER;
use crate::apply_patch_engine::parser::EOF_MARKER;
use crate::apply_patch_engine::parser::Hunk;
use crate::apply_patch_engine::parser::MOVE_TO_MARKER;
use crate::apply_patch_engine::parser::ParseError;
use crate::apply_patch_engine::parser::UPDATE_FILE_MARKER;
use crate::apply_patch_engine::parser::UpdateFileChunk;

use Hunk::*;
use ParseError::*;

const ENVIRONMENT_ID_MARKER: &str = "*** Environment ID:";

/// 补丁规模计数（± 行数、涉及文件数），由逐行状态机在解析的同时累加。
///
/// 刻意在 `process_line` 的分支点上计数，而不是事后从 `Hunk` 反推：
/// `UpdateFileChunk` 把上下文行同时塞进 `old_lines` 和 `new_lines`
/// （`push_context_line`），从 chunk 长度算会把上下文行计成改动行。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PatchStats {
    pub lines_added: usize,
    pub lines_removed: usize,
    pub files_created: usize,
    pub files_deleted: usize,
    /// 全部 hunk 指向同一文件时给出该路径；出现第二个不同路径后置 `None`
    /// （多文件补丁没有「这一个文件」可报）。
    pub single_path: Option<PathBuf>,
}

#[derive(Debug, Default, Clone)]
pub struct StreamingPatchParser {
    line_buffer: String,
    state: StreamingParserState,
    line_number: usize,
}

#[derive(Debug, Default, Clone)]
struct StreamingParserState {
    mode: StreamingParserMode,
    hunks: Vec<Hunk>,
    environment_id: Option<String>,
    stats: PatchStats,
    /// 已出现两个不同 hunk 路径：此后 `single_path` 永久为 `None`
    /// （否则第三个 hunk 会把它重新填回去）。
    multi_path_seen: bool,
}

#[derive(Debug, Default, Clone, Copy)]
enum StreamingParserMode {
    #[default]
    NotStarted,
    StartedPatch,
    AddFile,
    DeleteFile,
    UpdateFile {
        hunk_line_number: usize,
    },
    EndedPatch,
}

/// 校验 hunk 头里解析出的路径：空、或本身就是另一条 `*** …` marker 行时拒绝。
///
/// 没有这道守卫时 `*** Add File: *** End Patch` 会把 marker 行当成文件名，
/// 引擎返回 `Ok` 并**真的建出一个叫 `*** End Patch` 的文件**（N-4①，2026-09-17
/// PR #87 评审附带观察；同族的 Delete/Move to 一并覆盖）。
fn validate_hunk_path(path: &str, line_number: usize) -> Result<(), ParseError> {
    if path.is_empty() || path.starts_with("***") {
        return Err(InvalidHunkError {
            message: format!(
                "'{path}' is not a valid hunk path: a hunk path must not be empty or another '***' marker line"
            ),
            line_number,
        });
    }
    Ok(())
}

impl StreamingPatchParser {
    pub fn environment_id(&self) -> Option<&str> {
        self.state.environment_id.as_deref()
    }

    /// 已解析部分的规模计数。逐 chunk 喂入时可随时读取，供渲染层在补丁
    /// 还没输出完时显示「目前 +N −M」。
    pub fn stats(&self) -> PatchStats {
        self.state.stats.clone()
    }

    /// 记录一个 hunk 头的路径归属（`single_path` 只在全程同一文件时保留）。
    fn note_hunk_path(&mut self, path: &Path) {
        if self.state.multi_path_seen {
            return;
        }
        match &self.state.stats.single_path {
            None => self.state.stats.single_path = Some(path.to_path_buf()),
            Some(first) if first == path => {}
            Some(_) => {
                self.state.stats.single_path = None;
                self.state.multi_path_seen = true;
            }
        }
    }

    fn ensure_update_hunk_is_not_empty(&self, line: &str) -> Result<(), ParseError> {
        if let Some(UpdateFile { path, chunks, .. }) = self.state.hunks.last() {
            if chunks.is_empty()
                && let StreamingParserMode::UpdateFile { hunk_line_number } = self.state.mode
            {
                return Err(InvalidHunkError {
                    message: format!("Update file hunk for path '{}' is empty", path.display()),
                    line_number: hunk_line_number,
                });
            }
            if chunks
                .last()
                .is_some_and(|chunk| chunk.old_lines.is_empty() && chunk.new_lines.is_empty())
            {
                if line == END_PATCH_MARKER {
                    return Err(InvalidHunkError {
                        message: "Update hunk does not contain any lines".to_string(),
                        line_number: self.line_number,
                    });
                }
                return Err(InvalidHunkError {
                    message: format!(
                        "Unexpected line found in update hunk: '{line}'. Every line should start with ' ' (context line), '+' (added line), or '-' (removed line)"
                    ),
                    line_number: self.line_number,
                });
            }
        }
        Ok(())
    }

    fn handle_hunk_headers_and_end_patch(&mut self, trimmed: &str) -> Result<bool, ParseError> {
        if matches!(self.state.mode, StreamingParserMode::StartedPatch)
            && let Some(environment_id) = trimmed.strip_prefix(ENVIRONMENT_ID_MARKER)
        {
            if self.state.environment_id.is_some() {
                return Err(InvalidPatchError(
                    "apply_patch environment_id cannot be specified more than once".to_string(),
                ));
            }
            let environment_id = environment_id.trim();
            if environment_id.is_empty() {
                return Err(InvalidPatchError(
                    "apply_patch environment_id cannot be empty".to_string(),
                ));
            }
            self.state.environment_id = Some(environment_id.to_string());
            return Ok(true);
        }
        if trimmed == END_PATCH_MARKER {
            self.ensure_update_hunk_is_not_empty(trimmed)?;
            self.state.mode = StreamingParserMode::EndedPatch;
            return Ok(true);
        }
        if let Some(path) = trimmed.strip_prefix(ADD_FILE_MARKER) {
            self.ensure_update_hunk_is_not_empty(trimmed)?;
            validate_hunk_path(path, self.line_number)?;
            let path = PathBuf::from(path);
            self.note_hunk_path(&path);
            self.state.stats.files_created += 1;
            self.state.hunks.push(AddFile {
                path,
                contents: String::new(),
            });
            self.state.mode = StreamingParserMode::AddFile;
            return Ok(true);
        }
        if let Some(path) = trimmed.strip_prefix(DELETE_FILE_MARKER) {
            self.ensure_update_hunk_is_not_empty(trimmed)?;
            validate_hunk_path(path, self.line_number)?;
            let path = PathBuf::from(path);
            self.note_hunk_path(&path);
            self.state.stats.files_deleted += 1;
            self.state.hunks.push(DeleteFile { path });
            self.state.mode = StreamingParserMode::DeleteFile;
            return Ok(true);
        }
        if let Some(path) = trimmed.strip_prefix(UPDATE_FILE_MARKER) {
            self.ensure_update_hunk_is_not_empty(trimmed)?;
            validate_hunk_path(path, self.line_number)?;
            let path = PathBuf::from(path);
            self.note_hunk_path(&path);
            self.state.hunks.push(UpdateFile {
                path,
                move_path: None,
                chunks: Vec::new(),
            });
            self.state.mode = StreamingParserMode::UpdateFile {
                hunk_line_number: self.line_number,
            };
            return Ok(true);
        }
        Ok(false)
    }

    /// 喂入一段补丁文本（可以是一个字符，也可以是整份）。刻意**不**回吐
    /// hunks：逐 chunk 调用时返回整表克隆会把线性解析变成 O(n²)，
    /// 中途读数走 [`stats`](Self::stats)，完整结果在 [`finish`](Self::finish) 取。
    pub fn push_delta(&mut self, delta: &str) -> Result<(), ParseError> {
        for ch in delta.chars() {
            if ch == '\n' {
                let mut line = std::mem::take(&mut self.line_buffer);
                line.truncate(line.strip_suffix('\r').map_or(line.len(), str::len));
                self.line_number += 1;
                self.process_line(&line)?;
            } else {
                self.line_buffer.push(ch);
            }
        }

        Ok(())
    }

    pub fn finish(&mut self) -> Result<Vec<Hunk>, ParseError> {
        if !self.line_buffer.is_empty() {
            let line = std::mem::take(&mut self.line_buffer);
            self.line_number += 1;
            if line.trim() == END_PATCH_MARKER {
                self.ensure_update_hunk_is_not_empty(line.trim())?;
                self.state.mode = StreamingParserMode::EndedPatch;
            } else {
                self.process_line(&line)?;
            }
        }

        if !matches!(self.state.mode, StreamingParserMode::EndedPatch) {
            return Err(InvalidPatchError(
                "The last line of the patch must be '*** End Patch'".to_string(),
            ));
        }

        Ok(self.state.hunks.clone())
    }

    fn process_line(&mut self, line: &str) -> Result<(), ParseError> {
        let trimmed = line.trim();
        match self.state.mode {
            StreamingParserMode::NotStarted => {
                if trimmed == BEGIN_PATCH_MARKER {
                    self.state.mode = StreamingParserMode::StartedPatch;
                    return Ok(());
                }
                Err(InvalidPatchError(
                    "The first line of the patch must be '*** Begin Patch'".to_string(),
                ))
            }
            StreamingParserMode::StartedPatch => {
                if self.handle_hunk_headers_and_end_patch(trimmed)? {
                    return Ok(());
                }
                Err(InvalidHunkError {
                    message: format!(
                        "'{trimmed}' is not a valid hunk header. Valid hunk headers: '*** Add File: {{path}}', '*** Delete File: {{path}}', '*** Update File: {{path}}'"
                    ),
                    line_number: self.line_number,
                })
            }
            StreamingParserMode::AddFile => {
                if self.handle_hunk_headers_and_end_patch(trimmed)? {
                    return Ok(());
                }
                if let Some(line_to_add) = line.strip_prefix('+')
                    && let Some(AddFile { contents, .. }) = self.state.hunks.last_mut()
                {
                    self.state.stats.lines_added += 1;
                    contents.push_str(line_to_add);
                    contents.push('\n');
                    return Ok(());
                }
                Err(InvalidHunkError {
                    message: format!(
                        "'{trimmed}' is not a valid hunk header. Valid hunk headers: '*** Add File: {{path}}', '*** Delete File: {{path}}', '*** Update File: {{path}}'"
                    ),
                    line_number: self.line_number,
                })
            }
            StreamingParserMode::DeleteFile => {
                if self.handle_hunk_headers_and_end_patch(trimmed)? {
                    return Ok(());
                }
                Err(InvalidHunkError {
                    message: format!(
                        "'{trimmed}' is not a valid hunk header. Valid hunk headers: '*** Add File: {{path}}', '*** Delete File: {{path}}', '*** Update File: {{path}}'"
                    ),
                    line_number: self.line_number,
                })
            }
            StreamingParserMode::UpdateFile { hunk_line_number } => {
                let update_line = line.trim_end();
                if self.handle_hunk_headers_and_end_patch(update_line)? {
                    return Ok(());
                }

                if let Some(UpdateFile {
                    move_path, chunks, ..
                }) = self.state.hunks.last_mut()
                {
                    if chunks.last().is_some_and(|chunk| chunk.is_end_of_file) {
                        if update_line.is_empty() {
                            return Ok(());
                        }
                        if update_line != EMPTY_CHANGE_CONTEXT_MARKER
                            && !update_line.starts_with(CHANGE_CONTEXT_MARKER)
                        {
                            return Err(InvalidHunkError {
                                message: format!(
                                    "Expected update hunk to start with a @@ context marker, got: '{line}'"
                                ),
                                line_number: self.line_number,
                            });
                        }
                    }

                    if chunks.is_empty()
                        && move_path.is_none()
                        && let Some(move_to_path) = update_line.strip_prefix(MOVE_TO_MARKER)
                    {
                        validate_hunk_path(move_to_path, self.line_number)?;
                        *move_path = Some(PathBuf::from(move_to_path));
                        self.state.mode = StreamingParserMode::UpdateFile { hunk_line_number };
                        return Ok(());
                    }

                    if (update_line == EMPTY_CHANGE_CONTEXT_MARKER
                        || update_line.starts_with(CHANGE_CONTEXT_MARKER))
                        && chunks.last().is_some_and(|chunk| {
                            chunk.old_lines.is_empty() && chunk.new_lines.is_empty()
                        })
                    {
                        return Err(InvalidHunkError {
                            message: format!(
                                "Unexpected line found in update hunk: '{line}'. Every line should start with ' ' (context line), '+' (added line), or '-' (removed line)"
                            ),
                            line_number: self.line_number,
                        });
                    }

                    if update_line == EMPTY_CHANGE_CONTEXT_MARKER {
                        chunks.push(UpdateFileChunk::default());
                        self.state.mode = StreamingParserMode::UpdateFile { hunk_line_number };
                        return Ok(());
                    }

                    if let Some(change_context) = update_line.strip_prefix(CHANGE_CONTEXT_MARKER) {
                        chunks.push(UpdateFileChunk {
                            change_context: Some(change_context.to_string()),
                            ..UpdateFileChunk::default()
                        });
                        self.state.mode = StreamingParserMode::UpdateFile { hunk_line_number };
                        return Ok(());
                    }

                    if update_line == EOF_MARKER {
                        if chunks.last().is_some_and(|chunk| {
                            chunk.old_lines.is_empty() && chunk.new_lines.is_empty()
                        }) {
                            return Err(InvalidHunkError {
                                message: "Update hunk does not contain any lines".to_string(),
                                line_number: self.line_number,
                            });
                        }
                        if let Some(chunk) = chunks.last_mut() {
                            chunk.is_end_of_file = true;
                        }
                        self.state.mode = StreamingParserMode::UpdateFile { hunk_line_number };
                        return Ok(());
                    }

                    if line.is_empty() {
                        if chunks.is_empty() {
                            chunks.push(UpdateFileChunk::default());
                        }
                        if let Some(chunk) = chunks.last_mut() {
                            chunk.push_context_line(String::new());
                        }
                        self.state.mode = StreamingParserMode::UpdateFile { hunk_line_number };
                        return Ok(());
                    }

                    if let Some(line_to_add) = line.strip_prefix(' ') {
                        if chunks.is_empty() {
                            chunks.push(UpdateFileChunk::default());
                        }
                        if let Some(chunk) = chunks.last_mut() {
                            chunk.push_context_line(line_to_add.to_string());
                        }
                        self.state.mode = StreamingParserMode::UpdateFile { hunk_line_number };
                        return Ok(());
                    }

                    if let Some(line_to_add) = line.strip_prefix('+') {
                        if chunks.is_empty() {
                            chunks.push(UpdateFileChunk::default());
                        }
                        if let Some(chunk) = chunks.last_mut() {
                            self.state.stats.lines_added += 1;
                            chunk.new_lines.push(line_to_add.to_string());
                        }
                        self.state.mode = StreamingParserMode::UpdateFile { hunk_line_number };
                        return Ok(());
                    }

                    if let Some(line_to_remove) = line.strip_prefix('-') {
                        if chunks.is_empty() {
                            chunks.push(UpdateFileChunk::default());
                        }
                        if let Some(chunk) = chunks.last_mut() {
                            self.state.stats.lines_removed += 1;
                            chunk.old_lines.push(line_to_remove.to_string());
                        }
                        self.state.mode = StreamingParserMode::UpdateFile { hunk_line_number };
                        return Ok(());
                    }

                    if chunks.last().is_some_and(|chunk| {
                        !chunk.old_lines.is_empty() || !chunk.new_lines.is_empty()
                    }) {
                        return Err(InvalidHunkError {
                            message: format!(
                                "Expected update hunk to start with a @@ context marker, got: '{line}'"
                            ),
                            line_number: self.line_number,
                        });
                    }
                }
                Err(InvalidHunkError {
                    message: format!(
                        "Unexpected line found in update hunk: '{line}'. Every line should start with ' ' (context line), '+' (added line), or '-' (removed line)"
                    ),
                    line_number: self.line_number,
                })
            }
            StreamingParserMode::EndedPatch => {
                if trimmed.is_empty() {
                    Ok(())
                } else {
                    Err(InvalidPatchError(
                        "The last line of the patch must be '*** End Patch'".to_string(),
                    ))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 三种 hunk 各一：Add 2 行；Update 删 1 增 2 带 1 上下文；Delete 0 行。
    /// （字面量必须顶格写：` \ 换行` 续行会把上下文行的前导空格一起吃掉。）
    const MIXED: &str = "\
*** Begin Patch
*** Add File: new.txt
+a
+b
*** Update File: a.txt
@@
-gone
+here
+there
 keep me
*** Delete File: gone.txt
*** End Patch
";

    /// 按 `chunk_size` 个字符切片喂入——渲染层将来就是这么流的。
    fn stats_for(chunk_size: usize, patch: &str) -> PatchStats {
        let mut parser = StreamingPatchParser::default();
        let chars: Vec<char> = patch.chars().collect();
        for piece in chars.chunks(chunk_size) {
            parser
                .push_delta(&piece.iter().collect::<String>())
                .expect("valid patch prefix");
        }
        parser.finish().expect("patch must end with End Patch");
        parser.stats()
    }

    /// 计数与喂入粒度无关：这是「LLM 还在输出时就报行数」的正确性前提。
    #[test]
    fn stats_are_identical_whatever_the_chunk_size() {
        let expected = PatchStats {
            lines_added: 4,
            lines_removed: 1,
            files_created: 1,
            files_deleted: 1,
            single_path: None,
        };
        for chunk_size in [1, 2, 3, 7, 13, 64, 4096] {
            assert_eq!(
                stats_for(chunk_size, MIXED),
                expected,
                "chunk_size={chunk_size}"
            );
        }
    }

    /// `push_delta` 不再回吐 hunks（去 O(n²) 克隆），解析结果必须逐字节不变。
    #[test]
    fn hunk_output_is_chunk_invariant() {
        let mut once = StreamingPatchParser::default();
        once.push_delta(MIXED).unwrap();
        let whole = once.finish().unwrap();

        let mut split = StreamingPatchParser::default();
        for line in MIXED.lines() {
            split.push_delta(&format!("{line}\n")).unwrap();
        }
        assert_eq!(split.finish().unwrap(), whole);
    }

    /// 上下文行不算改动，即使它的内容本身以 `-` / `+` 开头。
    #[test]
    fn context_lines_are_never_counted_as_changes() {
        let patch = "\
*** Begin Patch
*** Update File: a.txt
@@
-alpha
+ALPHA
 -dashed content
 +plussed content
*** End Patch
";
        let stats = stats_for(1, patch);
        assert_eq!(
            (stats.lines_added, stats.lines_removed),
            (1, 1),
            "got: {stats:?}"
        );
    }

    /// `single_path`：全程同一文件才报，出现第二个路径后不再复活。
    #[test]
    fn single_path_reports_only_when_the_patch_touches_one_file() {
        let one_file = "\
*** Begin Patch
*** Update File: a.txt
@@
-x
+X
*** Update File: a.txt
@@
-y
+Y
*** End Patch
";
        let stats = stats_for(3, one_file);
        assert_eq!(stats.single_path.as_deref(), Some(Path::new("a.txt")));
        assert_eq!((stats.lines_added, stats.lines_removed), (2, 2));

        let two_files = "\
*** Begin Patch
*** Update File: a.txt
@@
-x
+X
*** Update File: b.txt
@@
-y
+Y
*** Update File: a.txt
@@
-z
+Z
*** End Patch
";
        assert_eq!(stats_for(3, two_files).single_path, None);
    }

    /// 补丁没输完就得能读数：`push_delta` 中途不校验 `*** End Patch`。
    #[test]
    fn stats_are_readable_before_the_patch_is_complete() {
        let mut parser = StreamingPatchParser::default();
        for line in MIXED.lines().take(4) {
            parser.push_delta(&format!("{line}\n")).unwrap();
        }
        assert_eq!(
            parser.stats(),
            PatchStats {
                lines_added: 2,
                lines_removed: 0,
                files_created: 1,
                files_deleted: 0,
                single_path: Some(PathBuf::from("new.txt")),
            }
        );
    }
}
