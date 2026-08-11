//! 管道表格的结构检测原语（对齐 Codex TUI `table_detect`）。
//!
//! 被两个消费者共用：
//! - [`crate::gfm_live_table`]：流式表格的候选/确认判定；
//! - [`crate::markdown_unwrap`]：`` ```markdown `` 围栏含表格时的解包检测。
//!
//! 这是**结构解析**，不是渲染：转义管道 `\|` 保留反斜杠（渲染层负责
//! 展示转义）；只回答"这行能否参与表格 / 是否是分隔行 / 围栏上下文"。

/// 当前行所处的围栏上下文（决定管道行是否可作表格）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum FenceContext {
    /// 围栏外（普通 markdown 流）。
    #[default]
    Outside,
    /// `` ```md `` / `` ```markdown `` 围栏内：管道行可作表格。
    Markdown,
    /// 其他围栏（sh/rust/无 info）：管道是代码，不参与表格。
    Other,
}

/// 增量围栏跟踪器（开/闭 + 上下文）。
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct FenceState {
    pub(crate) marker: char,
    pub(crate) len: usize,
    pub(crate) ctx: FenceContext,
}

impl FenceState {
    /// 推进一行，返回该行是否为围栏行（开或闭）。
    ///
    /// 语义（对齐 Codex FenceTracker）：前导空格 >3 是缩进代码块不算围栏；
    /// 闭合 = trim 后全部为相同 marker 且长度 ≥ 开围栏长度。
    pub(crate) fn advance(state: &mut Option<FenceState>, line: &str) -> bool {
        let indent = line.len() - line.trim_start().len();
        if indent > 3 {
            return false;
        }
        let trimmed = line.trim();
        // 闭合检测：当前在围栏内
        if let Some(f) = state.as_ref() {
            if trimmed.chars().all(|c| c == f.marker) && trimmed.len() >= f.len {
                *state = None;
                return true;
            }
        }
        // 开围栏检测：≥3 个 ` 或 ~
        let marker_len = trimmed
            .chars()
            .take_while(|&c| c == '`' || c == '~')
            .count();
        if marker_len >= 3 {
            let marker = trimmed.chars().next().expect("marker_len >= 3");
            let info = trimmed[marker_len..].trim();
            let info_token = info
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_lowercase();
            let ctx = if info_token == "md" || info_token == "markdown" {
                FenceContext::Markdown
            } else {
                FenceContext::Other
            };
            *state = Some(FenceState {
                marker,
                len: marker_len,
                ctx,
            });
            return true;
        }
        false
    }
}

/// 结构解析一行管道分隔内容（对齐 Codex `parse_table_segments`）：
/// 去掉首尾 `|` 后按**未转义** `|` 切段；无首尾 `|` 时需 ≥2 段才算表格行。
pub(crate) fn parse_table_segments(line: &str) -> Option<Vec<&str>> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    let has_outer_pipe = trimmed.starts_with('|') || trimmed.ends_with('|');
    let content = trimmed.strip_prefix('|').unwrap_or(trimmed);
    let content = content.strip_suffix('|').unwrap_or(content);
    let raw_segments = split_unescaped_pipe(content);
    if !has_outer_pipe && raw_segments.len() <= 1 {
        return None;
    }
    let segments: Vec<&str> = raw_segments.into_iter().map(str::trim).collect();
    (!segments.is_empty()).then_some(segments)
}

/// 在**未转义**的 `|` 处切分（`\|` 是字面量，反斜杠保留——结构检测
/// 不负责渲染转义）。
pub(crate) fn split_unescaped_pipe(content: &str) -> Vec<&str> {
    let mut segments = Vec::with_capacity(8);
    let mut start = 0;
    let bytes = content.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            // 跳过被转义的字符。
            i += 2;
        } else if bytes[i] == b'|' {
            segments.push(&content[start..i]);
            start = i + 1;
            i += 1;
        } else {
            i += 1;
        }
    }
    segments.push(&content[start..]);
    segments
}

/// 表头行：结构解析成功且至少一个非空单元格（Codex 语义）。
pub(crate) fn is_table_header_line(line: &str) -> bool {
    parse_table_segments(line).is_some_and(|segments| segments.iter().any(|s| !s.is_empty()))
}

/// 分隔行：结构解析后每格匹配 `:?-+:?` 且 **≥3 个 `-`**（对齐 Codex）。
pub(crate) fn is_table_delimiter_line(line: &str) -> bool {
    parse_table_segments(line).is_some_and(|segments| {
        segments.iter().all(|s| {
            let t = s.trim();
            if t.is_empty() {
                return false;
            }
            let core = t.strip_prefix(':').unwrap_or(t);
            let core = core.strip_suffix(':').unwrap_or(core);
            core.len() >= 3 && core.chars().all(|c| c == '-')
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fence_open_close_tracking() {
        let mut f: Option<FenceState> = None;
        assert!(FenceState::advance(&mut f, "```markdown"));
        assert_eq!(f.as_ref().map(|f| f.ctx), Some(FenceContext::Markdown));
        assert!(!FenceState::advance(&mut f, "| a | b |"));
        assert!(FenceState::advance(&mut f, "```"));
        assert!(f.is_none());
    }

    #[test]
    fn fence_other_context() {
        let mut f: Option<FenceState> = None;
        assert!(FenceState::advance(&mut f, "```rust"));
        assert_eq!(f.as_ref().map(|f| f.ctx), Some(FenceContext::Other));
        assert!(FenceState::advance(&mut f, "```"));
        assert!(f.is_none());
    }

    #[test]
    fn indented_fence_is_not_fence() {
        let mut f: Option<FenceState> = None;
        assert!(!FenceState::advance(&mut f, "    ```md"));
        assert!(f.is_none());
    }

    #[test]
    fn header_and_delimiter_detection() {
        assert!(is_table_header_line("| A | B |"));
        assert!(is_table_header_line("a | b")); // 无首尾 | 的多段也算
        assert!(!is_table_header_line("plain"));
        assert!(is_table_delimiter_line("|---|---|"));
        assert!(is_table_delimiter_line("|:---:|:---:|"));
        assert!(!is_table_delimiter_line("| A | B |"));
        assert!(!is_table_delimiter_line("|--|")); // 不足 3 个 -
    }

    #[test]
    fn escaped_pipe_preserved() {
        let segs = parse_table_segments("| a \\| b | c |").expect("segments");
        assert_eq!(segs, vec!["a \\| b", "c"]);
    }
}
