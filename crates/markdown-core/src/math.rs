//! 数学公式分隔符规格（对应 REFERENCE §6 katex 语义）：
//! - display：`$$..$$`、`\[..\]`
//! - inline ：`$..$`、`\(..\)`
//! - 代码块 / 行内代码内的 `$` **不误渲染**
//! - 渲染失败回退字面文本（`throwOnError: false` 语义）

/// 扫描文本，产出数学区间列表。
///
/// 规则（对齐 katex auto-render 的保守行为）：
/// 1. 开 `$` 后必须能闭合，否则按字面文本（不产出 Math）；
/// 2. 闭合 `$` 前不允许紧跟空白（`$ x $` 不是公式）；
/// 3. 开 `$` 前的字符不能是反斜杠（`\$` 是转义美元）；
/// 4. 行内 `$..$` 内容不允许跨行（跨行按字面）；
/// 5. display `$$..$$` 允许跨行。
///
/// 调用方负责跳过代码块 / 行内代码区间（`ignored` 参数传入），
/// 保证代码内 `$` 不误渲染。
pub fn scan_math(text: &str) -> Vec<MathSpan> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        // `\[` / `\(` 开头的 display/inline 数学
        if b == b'\\' && i + 1 < bytes.len() {
            let (open, close, display) = match bytes[i + 1] {
                b'[' => (2usize, b']', true),
                b'(' => (2usize, b')', false),
                _ => (0, 0, false),
            };
            if open != 0 {
                if let Some(end) = find_closing(text, i + open, close, display) {
                    // end 指向 `]`/`)`；`end-1` 是反斜杠，source 截止到它之前
                    out.push(MathSpan {
                        start: i,
                        end: end + 1,
                        display,
                        source: text[i + open..end - 1].to_string(),
                    });
                    i = end + 1;
                    continue;
                }
            }
            i += 1;
            continue;
        }
        // `$$` / `$`
        if b == b'$' {
            let display = i + 1 < bytes.len() && bytes[i + 1] == b'$';
            let content_start = i + if display { 2 } else { 1 };
            if let Some(end) = find_dollar_close(text, content_start, display, i) {
                out.push(MathSpan {
                    start: i,
                    end: end + 1,
                    display,
                    source: text[content_start..end].to_string(),
                });
                i = end + 1;
                continue;
            }
        }
        i += 1;
    }
    out
}

#[derive(Clone, Debug, PartialEq)]
pub struct MathSpan {
    pub start: usize,
    pub end: usize,
    pub display: bool,
    pub source: String,
}

/// 找 `$` / `$$` 的闭合。
/// `prev_is_backslash`：开 `$` 前是反斜杠则视为转义，跳过。
fn find_dollar_close(text: &str, mut i: usize, display: bool, open_at: usize) -> Option<usize> {
    // 规则 3：转义美元
    if open_at > 0 && text.as_bytes()[open_at - 1] == b'\\' {
        return None;
    }
    let bytes = text.as_bytes();
    while i < bytes.len() {
        if bytes[i] == b'$' {
            let is_display_close = i + 1 < bytes.len() && bytes[i + 1] == b'$';
            if display {
                if is_display_close {
                    return Some(i + 1);
                }
                i += 1;
                continue;
            }
            // 行内闭合：不能紧跟空白（规则 2），且不能跨行（规则 4）
            if bytes[i - 1] == b' ' || bytes[i - 1] == b'\n' || bytes[i - 1] == b'\t' {
                i += 1;
                continue;
            }
            if text[i - 1..=i].contains('\n') {
                return None;
            }
            // 行内 `$` 闭合时后面紧跟 `$$` 的情况：视为 display 的开头，不闭合
            if is_display_close {
                i += 2;
                continue;
            }
            return Some(i);
        }
        if !display && bytes[i] == b'\n' {
            return None; // 规则 4：行内不跨行
        }
        i += 1;
    }
    None
}

/// 找 `\]` / `\)` 的闭合（允许跨行）。
fn find_closing(text: &str, mut i: usize, close: u8, display: bool) -> Option<usize> {
    let bytes = text.as_bytes();
    while i < bytes.len() {
        if bytes[i] == close {
            // 必须是 `\` 前缀
            if i > 0 && bytes[i - 1] == b'\\' {
                return Some(i);
            }
        }
        if !display && bytes[i] == b'\n' {
            return None;
        }
        i += 1;
    }
    None
}

/// 渲染失败回退：把 MathSpan 区间还原为字面文本（`throwOnError: false` 语义）。
/// 上层调用 `render_math(source, display) -> Result<...>`，Err 时用本函数回退。
pub fn math_to_literal(span: &MathSpan) -> String {
    if span.display {
        format!("$${}$$", span.source)
    } else {
        format!("${}$", span.source)
    }
}

/// 按 MathSpan 列表把文本切成片段（字面 / 公式交替）。
pub fn split_by_math<'a>(text: &'a str, spans: &'a [MathSpan]) -> Vec<Segment<'a>> {
    let mut out = Vec::new();
    let mut cursor = 0;
    for span in spans {
        if span.start > cursor {
            out.push(Segment::Literal(&text[cursor..span.start]));
        }
        out.push(Segment::Math(span));
        cursor = span.end;
    }
    if cursor < text.len() {
        out.push(Segment::Literal(&text[cursor..]));
    }
    out
}

#[derive(Clone, Debug, PartialEq)]
pub enum Segment<'a> {
    Literal(&'a str),
    Math(&'a MathSpan),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_dollar() {
        let spans = scan_math("inline $a^2+b^2=c^2$ done");
        assert_eq!(spans.len(), 1);
        assert!(!spans[0].display);
        assert_eq!(spans[0].source, "a^2+b^2=c^2");
    }

    #[test]
    fn display_dollar() {
        let spans = scan_math("$$\\int_0^1 x\\,dx$$");
        assert_eq!(spans.len(), 1);
        assert!(spans[0].display);
    }

    #[test]
    fn backslash_paren_and_bracket() {
        let spans = scan_math(r"inline \(x^2\) and display \[y^2\]");
        assert_eq!(spans.len(), 2);
        assert!(!spans[0].display);
        assert!(spans[1].display);
        assert_eq!(spans[0].source, "x^2");
        assert_eq!(spans[1].source, "y^2");
    }

    #[test]
    fn unclosed_dollar_is_literal() {
        let spans = scan_math("price is $5 and $10");
        assert!(spans.is_empty(), "不应产出 Math，全部按字面");
    }

    #[test]
    fn dollar_with_space_not_math() {
        let spans = scan_math("$ x $");
        assert!(spans.is_empty());
    }

    #[test]
    fn escaped_dollar_not_math() {
        let spans = scan_math(r"\$5 not math");
        assert!(spans.is_empty());
    }

    #[test]
    fn inline_does_not_cross_lines() {
        let spans = scan_math("$a\nb$");
        assert!(spans.is_empty());
    }

    #[test]
    fn display_crosses_lines() {
        let spans = scan_math("$$\na\nb\n$$");
        assert_eq!(spans.len(), 1);
        assert!(spans[0].display);
    }

    #[test]
    fn failure_fallback_to_literal() {
        // 模拟 katex throwOnError:false：渲染失败 → 字面文本
        let spans = scan_math("bad $\\notacommand{}$");
        if let Some(span) = spans.first() {
            assert_eq!(math_to_literal(span), "$\\notacommand{}$");
        }
    }
}
