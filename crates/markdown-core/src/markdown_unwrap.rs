//! 解包 `` ```markdown `` / `` ```md `` 围栏中含表格的内容（Codex 移植）。
//!
//! 背景：LLM 常把表格包进 `` ```markdown `` 围栏（当作"markdown 输出"
//! 的代码块）。不处理时 pulldown-cmark 会把整个围栏渲染成代码块（显示
//! 表格源码）。Codex 的做法：渲染前检测这类围栏，**若内容含表格**
//! （表头行 + 分隔行紧邻）则剥掉围栏行让表格走原生解析；不含表格的
//! markdown 围栏保持代码块（原样）。
//!
//! 在 [`crate::parse::parse_final`] 入口调用（纯文本变换）。

use super::table_detect::{FenceContext, FenceState, is_table_delimiter_line, is_table_header_line};

/// 解包含表格的 `` ```markdown `` 围栏，返回变换后的文本。
///
/// 实现：逐行写入 `out`；遇到 markdown 围栏时记录开行区间与内容行，
/// 闭合时若内容含表格则删除开行、跳过闭行（内容保留）；否则原样保留。
pub fn unwrap_markdown_fence_tables(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut fence: Option<FenceState> = None;
    // 当前 markdown 围栏的开行在 out 中的字节区间。
    let mut md_open_span: Option<(usize, usize)> = None;
    let mut md_content: Vec<String> = Vec::new();

    for line in input.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if FenceState::advance(&mut fence, trimmed) {
            if fence.is_some() {
                // 开围栏
                if fence
                    .as_ref()
                    .is_some_and(|f| f.ctx == FenceContext::Markdown)
                {
                    md_open_span = Some((out.len(), out.len() + line.len()));
                    md_content.clear();
                } else {
                    md_open_span = None;
                    md_content.clear();
                }
                out.push_str(line);
            } else if let Some((start, end)) = md_open_span.take() {
                // 闭围栏：markdown 围栏含表格 → 剥开行、跳过闭行
                if contains_table(&md_content) {
                    out.replace_range(start..end, "");
                } else {
                    out.push_str(line);
                }
                md_content.clear();
            } else {
                out.push_str(line);
            }
            continue;
        }
        if md_open_span.is_some() {
            md_content.push(trimmed.to_string());
        }
        out.push_str(line);
    }
    out
}

/// 内容行中是否存在"表头行 + 分隔行"紧邻（含表格的判定）。
fn contains_table(lines: &[String]) -> bool {
    lines
        .windows(2)
        .any(|w| is_table_header_line(&w[0]) && is_table_delimiter_line(&w[1]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ```markdown 围栏含表格 → 剥围栏，内容保留
    #[test]
    fn unwraps_markdown_fence_with_table() {
        let src = "前文\n\n```markdown\n| A | B |\n|---|---|\n| 1 | 2 |\n```\n\n后文\n";
        let out = unwrap_markdown_fence_tables(src);
        assert!(!out.contains("```"), "围栏行被剥掉: {out}");
        assert!(out.contains("| A | B |\n|---|---|\n| 1 | 2 |\n"), "表格内容保留");
        assert!(out.contains("前文") && out.contains("后文"));
    }

    /// ```md 变体同样解包
    #[test]
    fn unwraps_md_fence() {
        let src = "```md\n| a | b |\n|---|---|\n```\n";
        let out = unwrap_markdown_fence_tables(src);
        assert!(!out.contains("```"));
        assert!(out.contains("| a | b |"));
    }

    /// 不含表格的 markdown 围栏：保持代码块（原样）
    #[test]
    fn keeps_markdown_fence_without_table() {
        let src = "```markdown\n# 标题\n\n一段说明\n```\n";
        assert_eq!(unwrap_markdown_fence_tables(src), src);
    }

    /// 其他围栏（rust）不受影响
    #[test]
    fn keeps_other_fences() {
        let src = "```rust\nlet x = |a| a;\n```\n";
        assert_eq!(unwrap_markdown_fence_tables(src), src);
    }

    /// 未闭合 markdown 围栏：不剥（内容原样）
    #[test]
    fn keeps_unclosed_fence() {
        let src = "```markdown\n| a | b |\n|---|---|\n";
        assert_eq!(unwrap_markdown_fence_tables(src), src);
    }

    /// 多围栏：只剥含表格的
    #[test]
    fn mixed_fences() {
        let src = "```markdown\n| a |\n|---|\n```\n\n```rust\nfn main() {}\n```\n\n```markdown\n# 无表格\n```\n";
        let out = unwrap_markdown_fence_tables(src);
        assert!(out.contains("| a |\n|---|"), "表格围栏已解包");
        assert!(out.contains("```rust"), "rust 围栏保留");
        assert!(out.contains("```markdown\n# 无表格\n```"), "无表格 markdown 围栏保留");
    }
}
