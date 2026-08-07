//! 流式 live 行内解析（对应 REFERENCE §5 / 需求单 1.1"流式语义"）。
//!
//! 这是整个规格里**最不可丢**的语义：
//! - 只解析**已闭合**的内联语法（`**bold**`、`` `code` ``、`[link](url)`、`$math$`）；
//! - **未闭合**语法（`**` / `` ` `` / `[`）按字面文本输出 → 流式期间不会出现
//!   破损布局；
//! - 块级内容（代码块 / 图表 / 表格）一律不参与 live —— 等 final（§3 语义 2）。
//!
//! Web 端对应 `marked.parseInline`（rAF 节流后的低开销路径）。这里用
//! 单遍线性扫描实现：`O(n)`，无回溯，可安全用于每帧追加。

use crate::ast::Inline;

/// 解析一段行内文本为 live 预览节点。
///
/// 与 final 路径（pulldown-cmark）不同，本函数**只识别已闭合语法**：
/// - `**x**` → `Bold([Text("x")])`；`**x` → `Text("**x")`（字面）
/// - `*x*` → `Italic`（内容首尾非空白，marked 规则）
/// - `` `x` `` → `Code("x")`；`` `x `` → `Text("`x")`（字面）
/// - `[t](u)` → `Link`；`[t](` → `Text("[t](")`（字面）
/// - `$x$` / `$$x$$` → `Math`；未闭合 `$` → 字面
pub fn parse_live(input: &str) -> Vec<Inline> {
    let mut out = Vec::new();
    let mut chars = input.char_indices().peekable();
    let mut literal = String::new();

    while let Some((_, c)) = chars.next() {
        match c {
            '*' => {
                // `**` 开闭判断
                let double = matches!(chars.peek(), Some(&(_, '*')));
                if double {
                    chars.next(); // 消费第二个 `*`
                    match scan_closed_delim(&mut chars, "**", false) {
                        Ok(consumed) => {
                            flush(&mut literal, &mut out);
                            out.push(Inline::Bold(parse_live(&consumed)));
                        }
                        Err(consumed) => {
                            literal.push_str("**");
                            literal.push_str(&consumed);
                        }
                    }
                } else {
                    // 单星：尝试 Italic（内容首尾非空白）
                    match scan_closed_delim(&mut chars, "*", false) {
                        Ok(consumed) => {
                            let trimmed = consumed.trim();
                            if trimmed.len() == consumed.len() && !trimmed.is_empty() {
                                flush(&mut literal, &mut out);
                                out.push(Inline::Italic(parse_live(&consumed)));
                            } else {
                                literal.push('*');
                                literal.push_str(&consumed);
                            }
                        }
                        Err(consumed) => {
                            literal.push('*');
                            literal.push_str(&consumed);
                        }
                    }
                }
            }
            '_' => {
                if matches!(chars.peek(), Some(&(_, '_'))) {
                    chars.next();
                    match scan_closed_delim(&mut chars, "__", false) {
                        Ok(consumed) => {
                            flush(&mut literal, &mut out);
                            out.push(Inline::Bold(parse_live(&consumed)));
                        }
                        Err(consumed) => {
                            literal.push_str("__");
                            literal.push_str(&consumed);
                        }
                    }
                } else {
                    literal.push('_');
                }
            }
            '~' => {
                if matches!(chars.peek(), Some(&(_, '~'))) {
                    chars.next();
                    match scan_closed_delim(&mut chars, "~~", false) {
                        Ok(consumed) => {
                            flush(&mut literal, &mut out);
                            out.push(Inline::Strikethrough(parse_live(&consumed)));
                        }
                        Err(consumed) => {
                            literal.push_str("~~");
                            literal.push_str(&consumed);
                        }
                    }
                } else {
                    literal.push('~');
                }
            }
            '`' => match scan_closed_delim(&mut chars, "`", false) {
                Ok(consumed) => {
                    flush(&mut literal, &mut out);
                    out.push(Inline::Code(consumed));
                }
                Err(consumed) => {
                    literal.push('`');
                    literal.push_str(&consumed);
                }
            },
            '[' => {
                // 尝试 `[text](url)`；失败则字面（已消费内容归还）
                match scan_link(&mut chars) {
                    Ok((text, url)) => {
                        flush(&mut literal, &mut out);
                        out.push(Inline::Link {
                            text: parse_live(&text),
                            url,
                        });
                    }
                    Err(consumed) => {
                        literal.push('[');
                        literal.push_str(&consumed);
                    }
                }
            }
            '$' => {
                // `$$` display / `$` inline
                let display = matches!(chars.peek(), Some(&(_, '$')));
                if display {
                    chars.next();
                }
                let closer = if display { "$$" } else { "$" };
                match scan_closed_delim(&mut chars, closer, display) {
                    Ok(src) => {
                        flush(&mut literal, &mut out);
                        out.push(Inline::Math {
                            source: src,
                            display,
                        });
                    }
                    Err(consumed) => {
                        literal.push_str(if display { "$$" } else { "$" });
                        literal.push_str(&consumed);
                    }
                }
            }
            '\n' => {
                flush(&mut literal, &mut out);
                out.push(Inline::SoftBreak);
            }
            '\\' => {
                // 转义：下一字符按字面（防 `\*` 误判为强调）
                if let Some((_, next)) = chars.next() {
                    literal.push(next);
                } else {
                    literal.push('\\');
                }
            }
            _ => literal.push(c),
        }
    }
    flush(&mut literal, &mut out);
    out
}

/// 从当前迭代器位置扫描到闭合分隔符。
/// `allow_newline=false`（行内语法不跨行）。
/// 返回 `Ok(内容)`（不含分隔符）；失败返回 `Err(已消费文本)`，
/// 调用方必须把已消费文本按字面输出（未闭合语法字面语义）。
fn scan_closed_delim<I>(
    chars: &mut std::iter::Peekable<I>,
    closer: &str,
    allow_newline: bool,
) -> Result<String, String>
where
    I: Iterator<Item = (usize, char)>,
{
    let mut buf = String::new();
    let closer_chars: Vec<char> = closer.chars().collect();
    loop {
        match chars.next() {
            None => return Err(buf),
            Some((_, c)) => {
                if c == closer_chars[0] {
                    // 检查是否完整闭合
                    let mut matched = true;
                    let mut lookahead = Vec::new();
                    for &cc in &closer_chars[1..] {
                        match chars.next() {
                            Some((_, n)) => {
                                lookahead.push(n);
                                if n != cc {
                                    matched = false;
                                    break;
                                }
                            }
                            None => {
                                matched = false;
                                break;
                            }
                        }
                    }
                    if matched {
                        return Ok(buf);
                    }
                    // 不完整闭合：把消费的字符并入 buf（继续扫描）
                    buf.push(c);
                    buf.extend(lookahead);
                } else if c == '\n' && !allow_newline {
                    // 行内语法不跨行：失败，已消费内容归还调用方
                    buf.push(c);
                    return Err(buf);
                } else {
                    buf.push(c);
                }
            }
        }
    }
}

/// 尝试扫描 `[text](url)`。成功返回 `Ok((text, url))`；
/// 失败返回 `Err(已消费文本)`（调用方按字面输出）。
fn scan_link<I>(chars: &mut std::iter::Peekable<I>) -> Result<(String, String), String>
where
    I: Iterator<Item = (usize, char)>,
{
    // 扫到 `]`
    let mut text = String::new();
    loop {
        match chars.next() {
            None => return Err(text),
            Some((_, ']')) => break,
            Some((_, c)) if c == '\n' => {
                text.push(c);
                return Err(text);
            }
            Some((_, c)) => text.push(c),
        }
    }
    // 期望 `(`
    match chars.next() {
        Some((_, '(')) => {}
        Some((_, c)) => {
            let mut consumed = text;
            consumed.push(']');
            consumed.push(c);
            return Err(consumed);
        }
        None => {
            let mut consumed = text;
            consumed.push(']');
            return Err(consumed);
        }
    }
    // 扫到 `)`
    let mut url = String::new();
    loop {
        match chars.next() {
            None => {
                let mut consumed = text;
                consumed.push_str("](");
                consumed.push_str(&url);
                return Err(consumed);
            }
            Some((_, ')')) => break,
            Some((_, c)) if c == '\n' => {
                let mut consumed = text;
                consumed.push_str("](");
                consumed.push_str(&url);
                consumed.push(c);
                return Err(consumed);
            }
            Some((_, c)) => url.push(c),
        }
    }
    if url.is_empty() {
        let mut consumed = text;
        consumed.push_str("]()");
        return Err(consumed);
    }
    Ok((text, url))
}

fn flush(literal: &mut String, out: &mut Vec<Inline>) {
    if !literal.is_empty() {
        out.push(Inline::Text(std::mem::take(literal)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> Inline {
        Inline::Text(s.to_string())
    }

    #[test]
    fn plain_text_passthrough() {
        assert_eq!(parse_live("hello world"), vec![text("hello world")]);
    }

    #[test]
    fn closed_bold_parses() {
        assert_eq!(
            parse_live("a **b** c"),
            vec![
                text("a "),
                Inline::Bold(vec![text("b")]),
                text(" c"),
            ]
        );
    }

    /// 关键语义：未闭合 `**` 按字面输出，不产生破损结构
    #[test]
    fn unclosed_bold_is_literal() {
        assert_eq!(parse_live("a **b"), vec![text("a **b")]);
        assert_eq!(parse_live("a **b** c **d"), vec![
            text("a "),
            Inline::Bold(vec![text("b")]),
            text(" c **d"),
        ]);
    }

    #[test]
    fn closed_inline_code() {
        assert_eq!(
            parse_live("use `let x = 1;` ok"),
            vec![text("use "), Inline::Code("let x = 1;".into()), text(" ok")]
        );
    }

    /// 关键语义：未闭合 `` ` `` 字面输出（流式期间代码不会"跳变"）
    #[test]
    fn unclosed_code_is_literal() {
        assert_eq!(parse_live("a `b"), vec![text("a `b")]);
    }

    #[test]
    fn closed_link() {
        assert_eq!(
            parse_live("[docs](https://example.com)"),
            vec![Inline::Link {
                text: vec![text("docs")],
                url: "https://example.com".into(),
            }]
        );
    }

    /// 关键语义：未闭合 `[` 字面输出
    #[test]
    fn unclosed_link_is_literal() {
        assert_eq!(parse_live("see [docs"), vec![text("see [docs")]);
        assert_eq!(parse_live("see [docs]("), vec![text("see [docs](")]);
    }

    #[test]
    fn strikethrough_closed() {
        assert_eq!(
            parse_live("~~gone~~ here"),
            vec![
                Inline::Strikethrough(vec![text("gone")]),
                text(" here"),
            ]
        );
    }

    #[test]
    fn math_closed_and_unclosed() {
        assert_eq!(
            parse_live("$x^2$ and $y"),
            vec![
                Inline::Math {
                    source: "x^2".into(),
                    display: false,
                },
                text(" and $y"),
            ]
        );
    }

    #[test]
    fn nested_bold_inside_bold() {
        assert_eq!(
            parse_live("**a *b* c**"),
            vec![Inline::Bold(vec![
                text("a "),
                Inline::Italic(vec![text("b")]),
                text(" c"),
            ])]
        );
    }

    #[test]
    fn escaped_star_is_literal() {
        assert_eq!(parse_live(r"\*not bold\*"), vec![text("*not bold*")]);
    }

    #[test]
    fn soft_break_emitted() {
        assert_eq!(parse_live("a\nb"), vec![text("a"), Inline::SoftBreak, text("b")]);
    }

    /// 流式追加的不变量：`parse_live(prefix + delta)` 在 delta 未闭合语法时
    /// 与 `parse_live(prefix)` 的结果在已闭合前缀上一致（追加不重排前文）。
    #[test]
    fn append_invariant() {
        let p1 = parse_live("hello **world**");
        let p2 = parse_live("hello **world** and `code");
        assert_eq!(p1, p2[..2]);
        assert_eq!(p2[2], text(" and `code"));
    }
}
