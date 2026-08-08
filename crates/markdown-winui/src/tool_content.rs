use windows_reactor::{
    ColorScheme, Element, ElementExt, GridLength, HorizontalAlignment, ScrollBarVisibility,
    SelectionMode, TextWrapping, ThemeRef, Thickness, VerticalAlignment, border, grid, list_view,
    scroll_viewer, text_block, vstack,
};

use crate::{CodeBlock, highlighted_code_block};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CodeDocument {
    pub path: Option<String>,
    pub language: Option<String>,
    pub text: String,
    pub start_line: usize,
    pub total_lines: Option<usize>,
    pub truncated: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChangeStats {
    pub lines_added: usize,
    pub lines_removed: usize,
    pub files_created: usize,
    pub files_deleted: usize,
    pub file: Option<String>,
}

impl ChangeStats {
    pub fn is_empty(&self) -> bool {
        self.lines_added == 0
            && self.lines_removed == 0
            && self.files_created == 0
            && self.files_deleted == 0
            && self.file.is_none()
    }

    pub fn label(&self) -> String {
        let mut parts = vec![
            format!("+{}", self.lines_added),
            format!("−{}", self.lines_removed),
        ];
        if self.files_created > 0 {
            parts.push(format!("新建 {}", self.files_created));
        }
        if self.files_deleted > 0 {
            parts.push(format!("删除 {}", self.files_deleted));
        }
        parts.join("  ")
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ToolBody {
    #[default]
    Empty,
    Text(String),
    Code(Vec<CodeDocument>),
    Diff(DiffDocument),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiffDocument {
    pub files: Vec<DiffFile>,
    pub lines_added: usize,
    pub lines_removed: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiffFile {
    pub old_path: String,
    pub new_path: String,
    pub rows: Vec<DiffRow>,
    pub lines_added: usize,
    pub lines_removed: usize,
}

impl DiffFile {
    pub fn display_path(&self) -> &str {
        let path = if self.new_path.is_empty() || self.new_path == "/dev/null" {
            &self.old_path
        } else {
            &self.new_path
        };
        path.strip_prefix("a/")
            .or_else(|| path.strip_prefix("b/"))
            .unwrap_or(path)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DiffRowKind {
    #[default]
    Context,
    Added,
    Removed,
    Hunk,
    Meta,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiffRow {
    pub kind: DiffRowKind,
    pub old_line: Option<usize>,
    pub new_line: Option<usize>,
    pub text: String,
}

/// Build a typed presentation body from the canonical serialized ToolResult.
pub fn tool_body_from_result(
    name: &str,
    args_json: Option<&str>,
    result: &serde_json::Value,
) -> ToolBody {
    let model_text = result
        .pointer("/model/text")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();

    let mut diffs = Vec::new();
    collect_diff_strings(result.get("data"), &mut diffs);
    if diffs.is_empty() && looks_like_diff(model_text) {
        diffs.push(model_text.to_string());
    }
    if diffs.is_empty()
        && is_patch_tool(name)
        && let Some(patch) = json_string_field(args_json, "patch")
        && looks_like_diff(&patch)
    {
        diffs.push(patch);
    }
    if !diffs.is_empty() {
        let document = parse_unified_diff(&diffs.join("\n"));
        if !document.files.is_empty() {
            return ToolBody::Diff(document);
        }
    }

    if is_read_tool(name) && !model_text.trim().is_empty() {
        return ToolBody::Code(code_documents_from_read(args_json, result, model_text));
    }

    if model_text.trim().is_empty() {
        ToolBody::Empty
    } else {
        ToolBody::Text(model_text.to_string())
    }
}

pub fn change_stats_from_result(
    result: &serde_json::Value,
    body: &ToolBody,
) -> Option<ChangeStats> {
    if let ToolBody::Diff(document) = body {
        return Some(ChangeStats {
            lines_added: document.lines_added,
            lines_removed: document.lines_removed,
            files_created: document
                .files
                .iter()
                .filter(|file| file.old_path == "/dev/null")
                .count(),
            files_deleted: document
                .files
                .iter()
                .filter(|file| file.new_path == "/dev/null")
                .count(),
            file: (document.files.len() == 1).then(|| document.files[0].display_path().to_string()),
        });
    }
    let data = result.get("data")?;
    let stats = ChangeStats {
        lines_added: data
            .get("lines_added")
            .or_else(|| data.get("insertions"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as usize,
        lines_removed: data
            .get("lines_removed")
            .or_else(|| data.get("deletions"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as usize,
        files_created: data
            .get("files_created")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as usize,
        files_deleted: data
            .get("files_deleted")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as usize,
        file: data
            .get("file")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
    };
    (!stats.is_empty()).then_some(stats)
}

/// Build the same presentation from a retained timeline tool.
pub fn tool_body_from_timeline(
    name: &str,
    args_json: Option<&str>,
    output: Option<&str>,
) -> ToolBody {
    let output = output.unwrap_or_default();
    if looks_like_diff(output) {
        let document = parse_unified_diff(output);
        if !document.files.is_empty() {
            return ToolBody::Diff(document);
        }
    }
    if is_patch_tool(name)
        && let Some(patch) = json_string_field(args_json, "patch")
        && looks_like_diff(&patch)
    {
        let document = parse_unified_diff(&patch);
        if !document.files.is_empty() {
            return ToolBody::Diff(document);
        }
    }
    if is_read_tool(name) && !output.trim().is_empty() {
        return ToolBody::Code(code_documents_from_read(
            args_json,
            &serde_json::Value::Null,
            output,
        ));
    }
    if output.trim().is_empty() {
        ToolBody::Empty
    } else {
        ToolBody::Text(output.to_string())
    }
}

/// Recover change counts for restored timeline cards. Exact diff bodies take
/// precedence; compact receipts such as `+12 -3` are used as a fallback.
pub fn change_stats_from_timeline(body: &ToolBody, summary: Option<&str>) -> Option<ChangeStats> {
    if let ToolBody::Diff(document) = body {
        return Some(ChangeStats {
            lines_added: document.lines_added,
            lines_removed: document.lines_removed,
            files_created: document
                .files
                .iter()
                .filter(|file| file.old_path == "/dev/null")
                .count(),
            files_deleted: document
                .files
                .iter()
                .filter(|file| file.new_path == "/dev/null")
                .count(),
            file: (document.files.len() == 1).then(|| document.files[0].display_path().to_string()),
        });
    }
    let summary = summary?;
    let added = signed_count(summary, '+');
    let removed = signed_count(summary, '-');
    (added.is_some() || removed.is_some()).then_some(ChangeStats {
        lines_added: added.unwrap_or(0),
        lines_removed: removed.unwrap_or(0),
        ..ChangeStats::default()
    })
}

fn signed_count(text: &str, sign: char) -> Option<usize> {
    text.split(|character: char| character.is_whitespace() || matches!(character, ',' | ';' | '|'))
        .find_map(|token| {
            let digits = token.strip_prefix(sign)?;
            let digits = digits.trim_end_matches(|character: char| !character.is_ascii_digit());
            (!digits.is_empty() && digits.chars().all(|character| character.is_ascii_digit()))
                .then(|| digits.parse().ok())
                .flatten()
        })
}

fn is_read_tool(name: &str) -> bool {
    matches!(name, "read_file" | "file.read" | "read")
}

fn is_patch_tool(name: &str) -> bool {
    matches!(name, "apply_patch" | "git.apply_patch" | "patch")
}

fn json_string_field(args_json: Option<&str>, key: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(args_json?)
        .ok()?
        .get(key)?
        .as_str()
        .map(str::to_string)
}

fn collect_diff_strings(value: Option<&serde_json::Value>, out: &mut Vec<String>) {
    let Some(value) = value else { return };
    match value {
        serde_json::Value::Object(object) => {
            for (key, value) in object {
                if (key == "diff" || key == "patch" || key == "content")
                    && value.as_str().is_some_and(looks_like_diff)
                {
                    out.push(value.as_str().unwrap_or_default().to_string());
                } else {
                    collect_diff_strings(Some(value), out);
                }
            }
        }
        serde_json::Value::Array(array) => {
            for value in array {
                collect_diff_strings(Some(value), out);
            }
        }
        _ => {}
    }
}

fn looks_like_diff(text: &str) -> bool {
    (text.contains("--- ") && text.contains("+++ ") && text.contains("@@ "))
        || text.starts_with("diff --git ")
}

fn code_documents_from_read(
    args_json: Option<&str>,
    result: &serde_json::Value,
    model_text: &str,
) -> Vec<CodeDocument> {
    let args = args_json
        .and_then(|json| serde_json::from_str::<serde_json::Value>(json).ok())
        .unwrap_or_default();
    let requests: Vec<&serde_json::Value> = args
        .get("requests")
        .and_then(serde_json::Value::as_array)
        .map(|requests| requests.iter().collect())
        .unwrap_or_else(|| vec![&args]);
    let metadata: Vec<&serde_json::Value> = result
        .pointer("/data/files")
        .and_then(serde_json::Value::as_array)
        .map(|files| files.iter().collect())
        .unwrap_or_default();
    let chunks: Vec<&str> = model_text.split("\n\n---\n\n").collect();
    let count = chunks.len().max(metadata.len()).max(requests.len());
    (0..count)
        .map(|index| {
            let meta = metadata
                .get(index)
                .copied()
                .unwrap_or(&serde_json::Value::Null);
            let request = requests
                .get(index)
                .copied()
                .unwrap_or(&serde_json::Value::Null);
            let path = meta
                .get("path")
                .and_then(serde_json::Value::as_str)
                .or_else(|| request.get("path").and_then(serde_json::Value::as_str))
                .map(str::to_string);
            let start_line = meta
                .get("start_line")
                .and_then(serde_json::Value::as_u64)
                .or_else(|| {
                    request
                        .get("start_line")
                        .and_then(serde_json::Value::as_u64)
                })
                .unwrap_or(1) as usize;
            let total_lines = meta
                .get("total_lines")
                .and_then(serde_json::Value::as_u64)
                .map(|value| value as usize);
            let truncated = meta
                .get("truncated")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let text = strip_read_line_prefixes(chunks.get(index).copied().unwrap_or_default());
            let language = path.as_deref().and_then(language_from_path);
            CodeDocument {
                path,
                language,
                text,
                start_line,
                total_lines,
                truncated,
            }
        })
        .collect()
}

fn strip_read_line_prefixes(text: &str) -> String {
    text.lines()
        .map(|line| {
            let Some(rest) = line.strip_prefix('L') else {
                return line;
            };
            let Some((number, content)) = rest.split_once(": ") else {
                return line;
            };
            number
                .chars()
                .all(|character| character.is_ascii_digit())
                .then_some(content)
                .unwrap_or(line)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn language_from_path(path: &str) -> Option<String> {
    std::path::Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
}

pub fn parse_unified_diff(text: &str) -> DiffDocument {
    let mut document = DiffDocument::default();
    let mut current: Option<DiffFile> = None;
    let mut old_line = 0usize;
    let mut new_line = 0usize;
    let mut in_hunk = false;

    let flush = |document: &mut DiffDocument, current: &mut Option<DiffFile>| {
        if let Some(file) = current.take()
            && (!file.rows.is_empty() || !file.old_path.is_empty() || !file.new_path.is_empty())
        {
            document.lines_added += file.lines_added;
            document.lines_removed += file.lines_removed;
            document.files.push(file);
        }
    };

    for line in text.lines() {
        if let Some(paths) = line.strip_prefix("diff --git ") {
            flush(&mut document, &mut current);
            let mut parts = paths.split_whitespace();
            current = Some(DiffFile {
                old_path: parts.next().unwrap_or_default().to_string(),
                new_path: parts.next().unwrap_or_default().to_string(),
                ..DiffFile::default()
            });
            in_hunk = false;
            continue;
        }
        if let Some(path) = line.strip_prefix("--- ") {
            if current.is_none() {
                current = Some(DiffFile::default());
            }
            if let Some(file) = current.as_mut() {
                file.old_path = path.split('\t').next().unwrap_or(path).to_string();
            }
            continue;
        }
        if let Some(path) = line.strip_prefix("+++ ") {
            if current.is_none() {
                current = Some(DiffFile::default());
            }
            if let Some(file) = current.as_mut() {
                file.new_path = path.split('\t').next().unwrap_or(path).to_string();
            }
            continue;
        }
        if line.starts_with("@@") {
            if current.is_none() {
                current = Some(DiffFile::default());
            }
            if let Some((old, new)) = parse_hunk_starts(line) {
                old_line = old;
                new_line = new;
            }
            current.as_mut().unwrap().rows.push(DiffRow {
                kind: DiffRowKind::Hunk,
                text: line.to_string(),
                ..DiffRow::default()
            });
            in_hunk = true;
            continue;
        }
        if !in_hunk {
            continue;
        }
        let Some(file) = current.as_mut() else {
            continue;
        };
        if let Some(text) = line.strip_prefix('+') {
            file.rows.push(DiffRow {
                kind: DiffRowKind::Added,
                new_line: Some(new_line),
                text: text.to_string(),
                ..DiffRow::default()
            });
            file.lines_added += 1;
            new_line += 1;
        } else if let Some(text) = line.strip_prefix('-') {
            file.rows.push(DiffRow {
                kind: DiffRowKind::Removed,
                old_line: Some(old_line),
                text: text.to_string(),
                ..DiffRow::default()
            });
            file.lines_removed += 1;
            old_line += 1;
        } else if let Some(text) = line.strip_prefix(' ') {
            file.rows.push(DiffRow {
                kind: DiffRowKind::Context,
                old_line: Some(old_line),
                new_line: Some(new_line),
                text: text.to_string(),
            });
            old_line += 1;
            new_line += 1;
        } else {
            file.rows.push(DiffRow {
                kind: DiffRowKind::Meta,
                text: line.to_string(),
                ..DiffRow::default()
            });
        }
    }
    flush(&mut document, &mut current);
    document
}

fn parse_hunk_starts(header: &str) -> Option<(usize, usize)> {
    let mut parts = header.split_whitespace();
    (parts.next()? == "@@").then_some(())?;
    let old = parts.next()?.strip_prefix('-')?;
    let new = parts.next()?.strip_prefix('+')?;
    Some((range_start(old)?, range_start(new)?))
}

fn range_start(range: &str) -> Option<usize> {
    range.split(',').next()?.parse().ok()
}

pub fn tool_body_view(
    body: &ToolBody,
    scheme: ColorScheme,
    font_family: &str,
    key: &str,
) -> Element {
    match body {
        ToolBody::Empty => text_block("").into(),
        ToolBody::Text(text) => scroll_viewer(
            text_block(text)
                .font_size(13.0)
                .font_family(font_family)
                .wrap()
                .selectable(),
        )
        .max_height(520.0)
        .vertical_scroll_bar_visibility(ScrollBarVisibility::Auto)
        .with_key(key)
        .into(),
        ToolBody::Code(documents) => vstack(
            documents
                .iter()
                .enumerate()
                .map(|(index, document)| {
                    code_document_view(document, scheme, font_family, &format!("{key}-{index}"))
                })
                .collect::<Vec<_>>(),
        )
        .spacing(8.0)
        .with_key(key)
        .into(),
        ToolBody::Diff(diff) => diff_document_view(diff, font_family, key),
    }
}

fn code_document_view(
    document: &CodeDocument,
    scheme: ColorScheme,
    font_family: &str,
    key: &str,
) -> Element {
    let code = CodeBlock {
        lang: document.language.clone(),
        text: document.text.clone(),
    };
    let mut highlighted = highlighted_code_block(&code, scheme, font_family);
    highlighted.text_wrapping = TextWrapping::NoWrap;
    let line_count = document.text.lines().count().max(1);
    let line_numbers = (0..line_count)
        .map(|offset| (document.start_line + offset).to_string())
        .collect::<Vec<_>>()
        .join("\n");
    let title = document.path.as_deref().unwrap_or("文件内容").to_string();
    let range = document.total_lines.map(|total| {
        let end = document.start_line + line_count.saturating_sub(1);
        format!("L{}–{} / {}", document.start_line, end, total)
    });
    let header = grid((
        text_block(title)
            .font_size(12.0)
            .semibold()
            .text_trimming(windows_reactor::TextTrimming::CharacterEllipsis)
            .grid_column(0),
        text_block(range.unwrap_or_default())
            .font_size(12.0)
            .foreground(ThemeRef::SecondaryText)
            .horizontal_alignment(HorizontalAlignment::Right)
            .grid_column(1),
    ))
    .columns([GridLength::STAR, GridLength::Auto]);
    let code_grid = grid((
        border(
            text_block(line_numbers)
                .font_family(font_family)
                .font_size(13.0)
                .line_height(20.0)
                .foreground(ThemeRef::SecondaryText),
        )
        .background(ThemeRef::ControlFillSecondary)
        .padding(Thickness::xy(8.0, 0.0))
        .grid_column(0),
        Element::from(highlighted).grid_column(1),
    ))
    .columns([GridLength::Auto, GridLength::STAR])
    .column_spacing(10.0);
    border(
        vstack((
            header,
            scroll_viewer(code_grid)
                .horizontal_scroll_bar_visibility(ScrollBarVisibility::Auto)
                .vertical_scroll_bar_visibility(ScrollBarVisibility::Auto)
                .max_height(520.0),
        ))
        .spacing(8.0),
    )
    .background(ThemeRef::CardBackground)
    .border_brush(ThemeRef::CardStroke)
    .border_thickness(Thickness::uniform(1.0))
    .corner_radius(6.0)
    .padding(8.0)
    .automation_name(if document.truncated {
        "文件代码预览，内容已截断"
    } else {
        "文件代码预览"
    })
    .with_key(key)
    .into()
}

fn diff_document_view(document: &DiffDocument, font_family: &str, key: &str) -> Element {
    let mut files = Vec::new();
    for (index, file) in document.files.iter().enumerate() {
        files.push(diff_file_view(
            file,
            font_family,
            &format!("{key}-file-{index}"),
        ));
    }
    vstack((
        text_block(format!(
            "{} 个文件  +{}  −{}",
            document.files.len(),
            document.lines_added,
            document.lines_removed
        ))
        .font_size(12.0)
        .foreground(ThemeRef::SecondaryText),
        vstack(files).spacing(8.0),
    ))
    .spacing(8.0)
    .automation_name(format!(
        "代码差异，{} 个文件，新增 {} 行，删除 {} 行",
        document.files.len(),
        document.lines_added,
        document.lines_removed
    ))
    .with_key(key)
    .into()
}

fn diff_file_view(file: &DiffFile, font_family: &str, key: &str) -> Element {
    let row_count = file.rows.len();
    let height = ((row_count as f64 * 24.0) + 4.0).clamp(48.0, 520.0);
    let font = font_family.to_string();
    let rows = list_view(file.rows.clone(), move |row: &DiffRow, index| {
        diff_row_view(row, &font, index)
    })
    .selection_mode(SelectionMode::None)
    .height(height)
    .with_key(format!("{key}-rows"));
    border(vstack((
        grid((
            text_block(file.display_path())
                .font_size(12.0)
                .semibold()
                .text_trimming(windows_reactor::TextTrimming::CharacterEllipsis)
                .grid_column(0),
            text_block(format!("+{}  −{}", file.lines_added, file.lines_removed))
                .font_size(12.0)
                .foreground(ThemeRef::SecondaryText)
                .horizontal_alignment(HorizontalAlignment::Right)
                .grid_column(1),
        ))
        .columns([GridLength::STAR, GridLength::Auto])
        .padding(Thickness::xy(8.0, 6.0)),
        rows,
    )))
    .background(ThemeRef::CardBackground)
    .border_brush(ThemeRef::CardStroke)
    .border_thickness(Thickness::uniform(1.0))
    .corner_radius(6.0)
    .with_key(key)
    .into()
}

fn diff_row_view(row: &DiffRow, font_family: &str, index: usize) -> Element {
    if row.kind == DiffRowKind::Hunk {
        return border(
            text_block(&row.text)
                .font_family(font_family)
                .font_size(12.0)
                .foreground(ThemeRef::AccentText)
                .selectable(),
        )
        .background(ThemeRef::ControlFillSecondary)
        .padding(Thickness::xy(8.0, 3.0))
        .automation_name(format!("差异区段 {}", row.text))
        .with_key(format!("hunk-{index}"))
        .into();
    }
    let marker = match row.kind {
        DiffRowKind::Added => "+",
        DiffRowKind::Removed => "−",
        DiffRowKind::Meta => "·",
        _ => " ",
    };
    let old = row
        .old_line
        .map(|line| line.to_string())
        .unwrap_or_default();
    let new = row
        .new_line
        .map(|line| line.to_string())
        .unwrap_or_default();
    let mut surface = border(
        grid((
            text_block(old)
                .font_family(font_family)
                .font_size(12.0)
                .foreground(ThemeRef::SecondaryText)
                .horizontal_alignment(HorizontalAlignment::Right)
                .vertical_alignment(VerticalAlignment::Top)
                .grid_column(0),
            text_block(new)
                .font_family(font_family)
                .font_size(12.0)
                .foreground(ThemeRef::SecondaryText)
                .horizontal_alignment(HorizontalAlignment::Right)
                .vertical_alignment(VerticalAlignment::Top)
                .grid_column(1),
            text_block(marker)
                .font_family(font_family)
                .font_size(13.0)
                .semibold()
                .grid_column(2),
            text_block(&row.text)
                .font_family(font_family)
                .font_size(13.0)
                .wrap()
                .selectable()
                .grid_column(3),
        ))
        .columns([
            GridLength::Pixel(42.0),
            GridLength::Pixel(42.0),
            GridLength::Pixel(18.0),
            GridLength::STAR,
        ])
        .column_spacing(4.0)
        .padding(Thickness::xy(4.0, 2.0)),
    );
    surface = match row.kind {
        DiffRowKind::Added => surface.background(ThemeRef::SystemSuccessBackground),
        DiffRowKind::Removed => surface.background(ThemeRef::SystemCriticalBackground),
        DiffRowKind::Meta => surface.background(ThemeRef::ControlFillSecondary),
        _ => surface,
    };
    surface
        .automation_name(match row.kind {
            DiffRowKind::Added => format!("新增第 {} 行：{}", row.new_line.unwrap_or(0), row.text),
            DiffRowKind::Removed => {
                format!("删除原第 {} 行：{}", row.old_line.unwrap_or(0), row.text)
            }
            _ => row.text.clone(),
        })
        .with_key(format!("row-{index}"))
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multi_file_unified_diff_with_line_numbers() {
        let diff = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -2,2 +2,3 @@\n keep\n-old\n+new\n+extra\ndiff --git a/b.txt b/b.txt\n--- a/b.txt\n+++ b/b.txt\n@@ -1 +1 @@\n-x\n+y\n";
        let parsed = parse_unified_diff(diff);
        assert_eq!(parsed.files.len(), 2);
        assert_eq!((parsed.lines_added, parsed.lines_removed), (3, 2));
        assert_eq!(parsed.files[0].display_path(), "a.rs");
        assert!(matches!(
            parsed.files[0].rows[2],
            DiffRow {
                kind: DiffRowKind::Removed,
                old_line: Some(3),
                ..
            }
        ));
        assert!(matches!(
            parsed.files[0].rows[3],
            DiffRow {
                kind: DiffRowKind::Added,
                new_line: Some(3),
                ..
            }
        ));
    }

    #[test]
    fn edit_result_prefers_structured_diff() {
        let result = serde_json::json!({
            "summary": "ok",
            "data": {"files": [{"ops": [{"diff": "--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-old\n+new\n"}]}]},
            "model": {"text": "compact receipt"}
        });
        assert!(matches!(
            tool_body_from_result("edit_file", None, &result),
            ToolBody::Diff(DiffDocument { files, .. }) if files.len() == 1
        ));
    }

    #[test]
    fn read_result_becomes_code_and_strips_transport_line_prefix() {
        let result = serde_json::json!({
            "data": {"files": [{"path": "src/lib.rs", "start_line": 10, "total_lines": 20}]},
            "model": {"text": "L10: fn main() {\nL11: }"}
        });
        let ToolBody::Code(documents) = tool_body_from_result(
            "read_file",
            Some(r#"{"path":"src/lib.rs","start_line":10}"#),
            &result,
        ) else {
            panic!("expected code body");
        };
        let code = &documents[0];
        assert_eq!(code.text, "fn main() {\n}");
        assert_eq!(code.start_line, 10);
        assert_eq!(code.language.as_deref(), Some("rs"));
    }

    #[test]
    fn restored_patch_uses_diff_counts_before_receipt() {
        let body = tool_body_from_timeline(
            "apply_patch",
            Some(r#"{"patch":"--- a/a.rs\n+++ b/a.rs\n@@ -1 +1,2 @@\n-old\n+new\n+extra\n"}"#),
            Some("[OK] 1 file(s), +99 -88"),
        );
        let stats = change_stats_from_timeline(&body, Some("+99 -88")).unwrap();
        assert_eq!((stats.lines_added, stats.lines_removed), (2, 1));
        assert_eq!(stats.file.as_deref(), Some("a.rs"));
    }

    #[test]
    fn restored_compact_receipt_recovers_change_counts() {
        let stats = change_stats_from_timeline(&ToolBody::Empty, Some("edited file +12 -3"))
            .expect("compact counts");
        assert_eq!((stats.lines_added, stats.lines_removed), (12, 3));
    }
}
