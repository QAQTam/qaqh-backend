//! Chunk → replacements → new file contents.
//! Ported from OpenAI `codex-rs/apply-patch` (Apache-2.0); sync + std-fs.

use std::path::Path;

use crate::apply_patch_engine::EngineError;
use crate::apply_patch_engine::UpdateFileChunk;
use crate::apply_patch_engine::seek_sequence::{MatchTier, seek_sequence, seek_sequence_with_tier};
use crate::apply_patch_engine::text_file::Replacement;
use crate::apply_patch_engine::text_file::SourceFile;
use crate::match_diag::{
    MIN_DIAGNOSTIC_SIMILARITY, nearest_match_in_lines, render_diff_block,
};

pub struct AppliedPatch {
    pub original_contents: String,
    pub new_contents: String,
    pub ambiguity_warnings: Vec<String>,
}

/// Compute the new file contents after applying the chunks, reading the
/// current file contents from disk.
pub fn derive_new_contents_from_chunks(
    path: &Path,
    chunks: &[UpdateFileChunk],
) -> Result<AppliedPatch, EngineError> {
    let original_contents = std::fs::read_to_string(path).map_err(|e| EngineError::Io {
        context: format!("Failed to read file to update {}", path.to_string_lossy()),
        source: e,
    })?;

    let path_text = path.to_string_lossy().to_string();
    // 上游 685270a（#51203）：行尾无条件保持。未改动行的行尾字节原样保留，
    // 插入行用文件首选行尾；不再提供 NormalizeToLf 分支（CRLF 文件不再被
    // apply_patch 归一成 LF——物理保真归本引擎，LF 视范归 read/hash）。
    let mut source_file = SourceFile::parse(&original_contents);
    let original_lines = source_file.line_texts();
    let (replacements, ambiguity_warnings) =
        compute_replacements(&original_lines, &path_text, chunks)?;
    source_file.apply_replacements(&replacements);
    let new_contents = source_file.into_contents();
    Ok(AppliedPatch {
        original_contents,
        new_contents,
        ambiguity_warnings,
    })
}

/// Compute a list of replacements needed to transform `original_lines` into the
/// new lines, given the patch `chunks`. Each replacement is returned as
/// `(start_index, old_len, new_lines)`.
fn compute_replacements(
    original_lines: &[String],
    path: &str,
    chunks: &[UpdateFileChunk],
) -> Result<(Vec<Replacement>, Vec<String>), EngineError> {
    let mut replacements: Vec<Replacement> = Vec::new();
    let mut ambiguity_warnings: Vec<String> = Vec::new();
    let mut line_index: usize = 0;

    let total_chunks = chunks.len();
    for (chunk_idx, chunk) in chunks.iter().enumerate() {
        let chunk_num = chunk_idx + 1;
        // If a chunk has a `change_context`, we use seek_sequence to find it, then
        // adjust our `line_index` to continue from there.
        if let Some(ctx_line) = &chunk.change_context {
            if let Some(idx) = seek_sequence(
                original_lines,
                std::slice::from_ref(ctx_line),
                line_index,
                /*eof*/ false,
            ) {
                line_index = idx + 1;
            } else {
                return Err(EngineError::Compute(diagnose_missing_context(
                    original_lines,
                    path,
                    chunk_num,
                    total_chunks,
                    line_index,
                    ctx_line,
                )));
            }
        }

        if chunk.old_lines.is_empty() {
            // `SourceFile` only exposes real source lines, so the insertion
            // point is always after the final line (upstream 4883b1dd52: the
            // legacy split-representation's trailing-empty-line insertion is
            // gone; a pure addition lands at EOF regardless of blank lines).
            let insertion_idx = original_lines.len();
            replacements.push((insertion_idx, 0, chunk.new_lines.clone()));
            continue;
        }

        // Otherwise, try to match the existing lines in the file with the old lines
        // from the chunk. If found, schedule that region for replacement.
        // In many real-world diffs the last element of `old_lines` is an *empty*
        // string representing the terminating newline of the region being
        // replaced. This sentinel is not present in `original_lines` because
        // `SourceFile` stores the terminator on the preceding line. If a direct
        // search fails and the pattern ends with an empty string, retry without
        // that final element so modifications touching the end-of-file can be
        // located reliably.

        let mut pattern: &[String] = &chunk.old_lines;
        let mut found =
            seek_sequence_with_tier(original_lines, pattern, line_index, chunk.is_end_of_file);

        let mut new_slice: &[String] = &chunk.new_lines;

        if found.is_none() && pattern.last().is_some_and(String::is_empty) {
            // Retry without the trailing empty line which represents the final
            // newline in the file.
            pattern = &pattern[..pattern.len() - 1];
            if new_slice.last().is_some_and(String::is_empty) {
                new_slice = &new_slice[..new_slice.len() - 1];
            }

            found =
                seek_sequence_with_tier(original_lines, pattern, line_index, chunk.is_end_of_file);
        }

        if let Some((start_idx, tier)) = found {
            // 若非 EOF 锚定且后文存在第二处匹配，记录告警（仍按首命中应用，不静默隐瞒）。
            if !chunk.is_end_of_file
                && !pattern.is_empty()
                && let Some(second_idx) =
                    seek_sequence(original_lines, pattern, start_idx + 1, false)
            {
                let label = format_chunk_label(chunk_num, total_chunks);
                ambiguity_warnings.push(format!(
                    "{path}{label}: matched multiple locations (first at L{}, another at L{}); applied to L{} — add more context or '@@ <anchor>' if another occurrence was intended",
                    start_idx + 1,
                    second_idx + 1,
                    start_idx + 1,
                ));
            }
            // Context lines occur in both sides of a patch chunk. Keep those
            // original lines in place so their exact contents and terminators
            // survive, especially when the file has mixed line endings
            // (upstream 685270a: unconditional, no NormalizeToLf fast path).
            let orig_hunk = &original_lines[start_idx..start_idx + pattern.len()];
            let preserve_indent = matches!(
                tier,
                MatchTier::Trimmed | MatchTier::TypographicPunctuation
            );
            let mut old_start = 0;
            let mut new_start = 0;
            for &(old_context, new_context) in &chunk.context_line_indices {
                // A trailing empty context line can be removed from `pattern`
                // and `new_slice` above when it represents the final newline.
                if old_context >= pattern.len() || new_context >= new_slice.len() {
                    break;
                }
                if old_start != old_context || new_start != new_context {
                    let segment = if preserve_indent {
                        restore_trimmed_indent(
                            orig_hunk,
                            pattern,
                            &orig_hunk[old_start..old_context],
                            &pattern[old_start..old_context],
                            &new_slice[new_start..new_context],
                        )
                    } else {
                        new_slice[new_start..new_context].to_vec()
                    };
                    replacements.push((start_idx + old_start, old_context - old_start, segment));
                }
                old_start = old_context + 1;
                new_start = new_context + 1;
            }
            if old_start != pattern.len() || new_start != new_slice.len() {
                let segment = if preserve_indent {
                    restore_trimmed_indent(
                        orig_hunk,
                        pattern,
                        &orig_hunk[old_start..],
                        &pattern[old_start..],
                        &new_slice[new_start..],
                    )
                } else {
                    new_slice[new_start..].to_vec()
                };
                replacements.push((start_idx + old_start, pattern.len() - old_start, segment));
            }
            line_index = start_idx + pattern.len();
        } else {
            return Err(EngineError::Compute(diagnose_missing_lines(
                original_lines,
                path,
                chunk_num,
                total_chunks,
                line_index,
                chunk,
                pattern,
            )));
        }
    }

    replacements.sort_by_key(|(index, _, _)| *index);

    Ok((replacements, ambiguity_warnings))
}

fn leading_ws(s: &str) -> &str {
    let trimmed = s.trim_start();
    &s[..s.len() - trimmed.len()]
}

fn uniform_stripped_prefix<'a>(
    orig_hunk: &'a [String],
    pat_hunk: &[String],
) -> Option<&'a str> {
    let mut prefix: Option<&'a str> = None;
    for (orig, pat) in orig_hunk.iter().zip(pat_hunk.iter()) {
        if orig.trim().is_empty() || pat.trim().is_empty() {
            continue;
        }
        let delta = leading_ws(orig).strip_suffix(leading_ws(pat))?;
        if delta.is_empty() {
            return None;
        }
        match prefix {
            None => prefix = Some(delta),
            Some(prev) if prev == delta => {}
            Some(_) => return None,
        }
    }
    prefix
}

/// Restore leading indentation stripped by `Trimmed` / `TypographicPunctuation`
/// matching so fuzzy whitespace location does not silently drop tabs/spaces on
/// replaced lines.
fn restore_trimmed_indent(
    orig_hunk: &[String],
    pat_hunk: &[String],
    orig_seg: &[String],
    old_seg: &[String],
    new_seg: &[String],
) -> Vec<String> {
    if let Some(prefix) = uniform_stripped_prefix(orig_hunk, pat_hunk) {
        let all_start_with_prefix = new_seg
            .iter()
            .filter(|l| !l.trim().is_empty())
            .all(|l| leading_ws(l).starts_with(prefix));
        let has_old_non_empty = old_seg.iter().any(|l| !l.trim().is_empty());
        let baseline_ws = if has_old_non_empty {
            old_seg
                .iter()
                .filter(|l| !l.trim().is_empty())
                .map(|l| leading_ws(l).len())
                .min()
                .unwrap_or(0)
        } else {
            pat_hunk
                .iter()
                .filter(|l| !l.trim().is_empty())
                .map(|l| leading_ws(l).len())
                .min()
                .unwrap_or(0)
        };
        let needs_prefix = new_seg
            .iter()
            .filter(|l| !l.trim().is_empty())
            .map(|l| leading_ws(l).len())
            .min()
            .is_some_and(|m| m < prefix.len() + baseline_ws || !all_start_with_prefix);
        if needs_prefix {
            return new_seg
                .iter()
                .map(|line| {
                    if line.trim().is_empty() {
                        line.clone()
                    } else {
                        format!("{prefix}{line}")
                    }
                })
                .collect();
        }
        return new_seg.to_vec();
    }

    if orig_seg.len() == new_seg.len() && old_seg.len() == new_seg.len() {
        return (0..new_seg.len())
            .map(|k| {
                let orig_ws = leading_ws(&orig_seg[k]);
                let old_ws = leading_ws(&old_seg[k]);
                let new_ws = leading_ws(&new_seg[k]);
                if orig_ws != old_ws
                    && !new_seg[k].trim().is_empty()
                    && !new_ws.starts_with(orig_ws)
                    && let Some(delta) = orig_ws.strip_suffix(old_ws)
                    && !delta.is_empty()
                {
                    format!("{delta}{}", new_seg[k])
                } else {
                    new_seg[k].clone()
                }
            })
            .collect();
    }

    new_seg.to_vec()
}

fn format_chunk_label(chunk_num: usize, total_chunks: usize) -> String {
    if total_chunks > 1 {
        format!(" (chunk {chunk_num}/{total_chunks})")
    } else {
        String::new()
    }
}

fn diagnose_missing_context(
    original_lines: &[String],
    path: &str,
    chunk_num: usize,
    total_chunks: usize,
    line_index: usize,
    ctx_line: &str,
) -> String {
    let label = format_chunk_label(chunk_num, total_chunks);
    if line_index > 0
        && let Some(earlier) = seek_sequence(
            &original_lines[..line_index],
            &[ctx_line.to_string()],
            0,
            false,
        )
    {
        return format!(
            "Failed to find context '{ctx_line}' in {path}{label}: matched earlier at L{}, but previous chunk already advanced cursor to L{} (chunks must appear in top-to-bottom file order)",
            earlier + 1,
            line_index + 1,
        );
    }
    let file_refs: Vec<&str> = original_lines.iter().map(String::as_str).collect();
    let mut msg = format!("Failed to find context '{ctx_line}' in {path}{label}\n");
    if let Some(nearest) =
        nearest_match_in_lines(&file_refs, &[ctx_line], MIN_DIAGNOSTIC_SIMILARITY)
    {
        msg.push_str(&render_diff_block("@@ context", ctx_line, &nearest, "  "));
    } else {
        msg.push_str("  no similar line found in file; re-read the file with read.\n");
    }
    msg.trim_end().to_string()
}

fn diagnose_missing_lines(
    original_lines: &[String],
    path: &str,
    chunk_num: usize,
    total_chunks: usize,
    line_index: usize,
    chunk: &UpdateFileChunk,
    pattern: &[String],
) -> String {
    let label = format_chunk_label(chunk_num, total_chunks);
    if line_index > 0
        && let Some(earlier) = seek_sequence(&original_lines[..line_index], pattern, 0, false)
    {
        if let Some(ctx) = &chunk.change_context {
            return format!(
                "Failed to find expected lines in {path}{label}: matched earlier at L{}, before the '@@ {ctx}' anchor at L{}",
                earlier + 1,
                line_index,
            );
        }
        return format!(
            "Failed to find expected lines in {path}{label}: matched earlier at L{}, but previous chunk already advanced cursor to L{} (chunks must appear in top-to-bottom file order)",
            earlier + 1,
            line_index + 1,
        );
    }
    if chunk.is_end_of_file
        && let Some(mid) = seek_sequence(original_lines, pattern, line_index, false)
    {
        return format!(
            "Failed to find expected lines at EOF in {path}{label}: matched at L{}, not at end of file (remove '*** End of File' if editing mid-file)",
            mid + 1,
        );
    }

    let file_refs: Vec<&str> = original_lines.iter().map(String::as_str).collect();
    let pat_refs: Vec<&str> = pattern.iter().map(String::as_str).collect();
    let expected = chunk.old_lines.join("\n");
    if let Some(nearest) = nearest_match_in_lines(&file_refs, &pat_refs, MIN_DIAGNOSTIC_SIMILARITY)
    {
        let mut msg = format!("Failed to find expected lines in {path}{label}:\n");
        msg.push_str(&render_diff_block("patch expected", &expected, &nearest, "  "));
        msg.trim_end().to_string()
    } else {
        format!(
            "Failed to find expected lines in {path}{label} (no similar region found):\n{expected}",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::compute_replacements;
    use crate::apply_patch_engine::UpdateFileChunk;
    use crate::apply_patch_engine::text_file::SourceFile;

    fn apply_chunk(original: &str, chunk: UpdateFileChunk) -> String {
        let mut source = SourceFile::parse(original);
        let original_lines = source.line_texts();
        let (replacements, _) =
            compute_replacements(&original_lines, "test.txt", &[chunk]).expect("compute");
        source.apply_replacements(&replacements);
        source.into_contents()
    }

    #[test]
    fn trimmed_match_preserves_original_tab_indentation() {
        let chunk = UpdateFileChunk {
            change_context: None,
            old_lines: vec!["tab-indent:".into(), "indented value".into()],
            new_lines: vec!["tab-indent:".into(), "indented value [edited]".into()],
            is_end_of_file: false,
            context_line_indices: vec![(0, 0)],
        };
        let updated = apply_chunk("tab-indent:\n\tindented value\n", chunk);
        assert_eq!(updated, "tab-indent:\n\tindented value [edited]\n");
    }

    #[test]
    fn trimmed_match_preserves_uniform_block_indentation_across_added_lines() {
        let chunk = UpdateFileChunk {
            change_context: None,
            old_lines: vec!["if cond {".into(), "    old();".into(), "}".into()],
            new_lines: vec![
                "if cond {".into(),
                "    new_a();".into(),
                "    new_b();".into(),
                "}".into(),
            ],
            is_end_of_file: false,
            context_line_indices: vec![(0, 0), (2, 3)],
        };
        let updated = apply_chunk("\tif cond {\n\t    old();\n\t}\n", chunk);
        assert_eq!(
            updated,
            "\tif cond {\n\t    new_a();\n\t    new_b();\n\t}\n"
        );
    }

    #[test]
    fn exact_match_still_allows_intentional_reindentation() {
        let chunk = UpdateFileChunk {
            change_context: None,
            old_lines: vec!["    let x = 1;".into()],
            new_lines: vec!["        let x = 1;".into()],
            is_end_of_file: false,
            context_line_indices: Vec::new(),
        };
        let updated = apply_chunk("    let x = 1;\n", chunk);
        assert_eq!(updated, "        let x = 1;\n");
    }

    #[test]
    fn trimmed_match_does_not_double_indent_when_new_line_already_indented() {
        let chunk = UpdateFileChunk {
            change_context: None,
            old_lines: vec!["indented value".into()],
            new_lines: vec!["\tindented value [edited]".into()],
            is_end_of_file: false,
            context_line_indices: Vec::new(),
        };
        let updated = apply_chunk("\tindented value\n", chunk);
        assert_eq!(updated, "\tindented value [edited]\n");
    }
}
