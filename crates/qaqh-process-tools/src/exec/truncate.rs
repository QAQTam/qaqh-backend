//! exec::truncate — token 截断（CJK 估计 + token_truncate/find_token_boundary）。

/// CJK character ranges used for token-count estimation.
/// Token estimate: ~1 token per 3.3 ASCII characters or per 1.67 CJK characters
/// (CJK is denser, so each character costs more tokens than an ASCII one).
pub(crate) const fn is_cjk(c: char) -> bool {
    matches!(c,
        '\u{4e00}'..='\u{9fff}' | '\u{3400}'..='\u{4dbf}'
        | '\u{3000}'..='\u{303f}' | '\u{ff00}'..='\u{ffef}'
        | '\u{3040}'..='\u{30ff}'
    )
}

/// Find byte index for `target` tokens walking forward.
pub(crate) fn find_token_boundary(text: &str, target_tokens: u32) -> usize {
    let target_f64 = target_tokens as f64;
    let mut char_count = 0usize;
    let mut cjk_count = 0usize;
    for (i, c) in text.char_indices() {
        if is_cjk(c) {
            cjk_count += 1;
        } else {
            char_count += 1;
        }
        let est = char_count as f64 / 3.3 + cjk_count as f64 / 1.67;
        if est >= target_f64 {
            return i;
        }
    }
    text.len()
}

/// Find byte index for `target` tokens walking backward from end.
pub(crate) fn find_token_boundary_reverse(text: &str, target_tokens: u32) -> usize {
    let target_f64 = target_tokens as f64;
    let mut char_count = 0usize;
    let mut cjk_count = 0usize;
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    for (i, c) in chars.iter().rev() {
        if is_cjk(*c) {
            cjk_count += 1;
        } else {
            char_count += 1;
        }
        let est = char_count as f64 / 3.3 + cjk_count as f64 / 1.67;
        if est >= target_f64 {
            return *i;
        }
    }
    0
}

/// Token-aware smart truncation: keeps head (70%) + tail (30%).
pub(crate) fn token_truncate(text: &str, max_tokens: u32) -> String {
    let total = qaqh_types::token::count_tokens(text);
    if total <= max_tokens {
        return text.to_string();
    }
    let head_tokens = (max_tokens as f64 * 0.7).max(1.0) as u32;
    let tail_tokens = (max_tokens as f64 * 0.3).max(1.0) as u32;
    let head_end = find_token_boundary(text, head_tokens);
    let tail_start = find_token_boundary_reverse(text, tail_tokens);
    if head_end >= tail_start {
        let end = find_token_boundary(text, max_tokens);
        format!(
            "{}\n...[TRUNCATED: {}/{} tokens. Call exec again with a narrower command or a filtering pipeline.]",
            text.get(..end).expect("token boundary is a char boundary"),
            max_tokens,
            total
        )
    } else {
        let tail = text
            .get(tail_start..)
            .expect("token boundary is a char boundary");
        format!(
            "{}\n\n...[TRUNCATED: {}/{} tokens, {} lines dropped. Call exec again with a narrower command or a filtering pipeline.]\n\n{}",
            text.get(..head_end)
                .expect("token boundary is a char boundary"),
            max_tokens,
            total,
            text.get(head_end..tail_start)
                .expect("token boundaries are char boundaries")
                .lines()
                .count(),
            tail.trim_start(),
        )
    }
}

// ── Output helpers ──

pub(crate) fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('[') => {
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            }
            Some(']' | 'P' | '_' | '^') => {
                while let Some(next) = chars.next() {
                    if next == '\x07' {
                        break;
                    }
                    if next == '\x1b' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

// ── ConPTY 行重排重建（tty 模型侧出口专用）──

/// ConPTY（Windows）对超过终端列数的逻辑行按重绘流发出
/// `行片段 \r\n CSI row;cols H 边界字符(重发) 其余片段`：对真实终端渲染幂等
/// （重发字符覆写边界格），对纯文本消费者 = 边界字符重复 + 逻辑行被 CRLF
/// 拦腰截断——exec 会话 ID / repo 名恰好在 80 列边界断裂变形，即模型侧
/// 「字符串随机损坏」的根源（取证：复现行 54/60 各 +3 字符，first diff 恒在
/// 第 80 列）。
///
/// 重建判定（全部成立才动刀，其余字节一律原样透传）：
/// 1. `\r\n` 紧跟 `CSI r;{cols}H`（列参数 == 终端列数）；
/// 2. CSI 后第一个字符 == CRLF 前一个字符（重发 = 覆写同一边界格）。
const REFLOW_HOLD_CAP: usize = 32;

/// oneshot tty 出口：整段重建 + 剥 ANSI + 换行规范化（`\r\n`→`\n`）。
/// 末尾未决签名尾巴按字面输出——EOF 后签名不可能再续。
pub(crate) fn clean_pty_model_text(raw: &str, cols: u16) -> String {
    let (mut joined, tail) = rejoin_conpty_rows(raw, cols, None);
    joined.push_str(&tail);
    strip_ansi(&joined).replace("\r\n", "\n")
}

/// 流式重建器：增量模型读出口跨读持有未决签名尾巴（write_stdin/poll 的
/// 读边界会切在 `\r\n\x1b[r;{cols}H` 签名中间）。
pub(crate) struct ConptyReflower {
    cols: u16,
    carry: String,
    /// 最后一个已产出字符：跨 feed 边界的边界字符比对源。
    last_char: Option<char>,
}

impl ConptyReflower {
    pub(crate) fn new(cols: u16) -> Self {
        Self {
            cols,
            carry: String::new(),
            last_char: None,
        }
    }

    /// 喂入本读窗口的原始流文本，返回重建后文本。
    pub(crate) fn feed(&mut self, raw: &str) -> String {
        if raw.is_empty() && self.carry.is_empty() {
            return String::new();
        }
        let combined = std::mem::take(&mut self.carry) + raw;
        let (mut out, hold) = rejoin_conpty_rows(&combined, self.cols, self.last_char);
        if hold.chars().count() > REFLOW_HOLD_CAP {
            // 病态流（CUP 参数远超 u16 位宽）不再持有，按字面交还。
            out.push_str(&hold);
        } else {
            self.carry = hold;
        }
        if let Some(c) = out.chars().next_back() {
            self.last_char = Some(c);
        }
        out
    }

    /// EOF：未决尾巴按字面交还，调用方拼回输出走同一条 strip_ansi 线。
    pub(crate) fn finish(&mut self) -> String {
        std::mem::take(&mut self.carry)
    }
}

enum ReflowVerdict {
    /// 单个 `\r`（后随非 `\n`）：字面输出。
    Plain,
    /// `\r\n` 但不构成重排签名：输出 `\r\n`，从 `after` 续扫（CSI 原样
    /// 留在输出里，由 strip_ansi 统一剥除）。
    Crlf { after: usize },
    /// 重排签名完整命中：CRLF + CSI + 重发字符整体删除。
    WrapContinuation { after: usize },
    /// 输入在签名中途结束：从签名起点留待下一次输入续判。
    Hold,
}

/// 扫描 `input`，返回 (可判定的重建文本, 未决签名尾巴)。
/// `margin_before` 是输入起点之前最后一个已产出字符（跨读比对用）。
fn rejoin_conpty_rows(input: &str, cols: u16, margin_before: Option<char>) -> (String, String) {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != b'\r' {
            match bytes[i..].iter().position(|&b| b == b'\r') {
                Some(k) => {
                    out.push_str(input.get(i..i + k).expect("`i` 恒落在字符边界"));
                    i += k;
                }
                None => {
                    out.push_str(input.get(i..).expect("`i` 恒落在字符边界"));
                    i = bytes.len();
                }
            }
            continue;
        }
        match classify_reflow(input, i, cols, margin_before) {
            ReflowVerdict::Plain => {
                out.push('\r');
                i += 1;
            }
            ReflowVerdict::Crlf { after } => {
                out.push_str("\r\n");
                i = after;
            }
            ReflowVerdict::WrapContinuation { after } => i = after,
            ReflowVerdict::Hold => break,
        }
    }
    let tail = input.get(i..).expect("`i` 恒落在字符边界").to_owned();
    (out, tail)
}

fn classify_reflow(input: &str, p: usize, cols: u16, margin_before: Option<char>) -> ReflowVerdict {
    let bytes = input.as_bytes();
    if p + 1 >= bytes.len() {
        // 裸 `\r` 结尾：可能是 `\r\n` 或签名前半，留待续判。
        return ReflowVerdict::Hold;
    }
    if bytes[p + 1] != b'\n' {
        return ReflowVerdict::Plain;
    }
    let q = p + 2;
    if q >= bytes.len() {
        return ReflowVerdict::Hold;
    }
    if bytes[q] != 0x1b {
        return ReflowVerdict::Crlf { after: q };
    }
    if q + 1 >= bytes.len() {
        return ReflowVerdict::Hold;
    }
    if bytes[q + 1] != b'[' {
        return ReflowVerdict::Crlf { after: q };
    }
    // CUP 参数：数字与 `;`，终止于 `H`。
    let mut i = q + 2;
    let mut last_semi = None;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b';' {
            last_semi = Some(i);
            i += 1;
        } else if b.is_ascii_digit() {
            i += 1;
        } else {
            break;
        }
    }
    if i >= bytes.len() {
        return ReflowVerdict::Hold;
    }
    if bytes[i] != b'H' {
        return ReflowVerdict::Crlf { after: q };
    }
    let col = match last_semi.and_then(|s| input.get(s + 1..i)) {
        Some(digits) => match digits.parse::<u16>() {
            Ok(v) => v,
            Err(_) => return ReflowVerdict::Crlf { after: q },
        },
        None => return ReflowVerdict::Crlf { after: q },
    };
    if col != cols {
        return ReflowVerdict::Crlf { after: q };
    }
    let h_end = i + 1;
    // 重发字符：CSI 后第一个字符；缺失则留待续判。
    let dup = match input.get(h_end..).and_then(|s| s.chars().next()) {
        Some(c) => c,
        None => return ReflowVerdict::Hold,
    };
    // 边界字符：CRLF 前一个字符；输入起点之前则用跨读状态。
    let margin = if p == 0 {
        margin_before
    } else {
        // 前一字符的末尾恰在 `p`：从 `p-1` 起步回溯到它的起点。
        let mut start = p - 1;
        while start > 0 && !input.is_char_boundary(start) {
            start -= 1;
        }
        input.get(start..p).and_then(|s| s.chars().next_back())
    };
    if margin == Some(dup) {
        ReflowVerdict::WrapContinuation {
            after: h_end + dup.len_utf8(),
        }
    } else {
        ReflowVerdict::Crlf { after: q }
    }
}

#[cfg(test)]
mod tests {
    use super::{clean_pty_model_text, strip_ansi, ConptyReflower};

    /// 回归钉：pwsh/ConPTY 实测泄露进模型侧 tool result 的全部形态——
    /// DEC 私有模式（?9001/?1004/?25）、SGR 彩色、光标定位、OSC 标题——
    /// exec_session 读出口（result_from_core）与 pty_exec 共用这条净化线。
    #[test]
    fn strip_ansi_removes_conpty_control_frames() {
        let leaked = "\u{1b}[?9001h\u{1b}[?1004h\u{1b}[m\u{1b}]0;C:\\pwsh.exe\u{7}\u{1b}[?25h\
                      \u{1b}[32m\u{1b}[1mMode\u{1b}[m \u{1b}[4;1Htext\u{1b}[7mHILITE\u{1b}[0m\u{1b}[?25l\u{1b}[?9001l\u{1b}[?1004l";
        assert_eq!(strip_ansi(leaked), "Mode textHILITE");
    }

    /// 取证回归钉：ConPTY 80×24 重排签名——strip_ansi 只剥 CSI，重复字符与
    /// 截断 CRLF 会漏进模型侧。重建必须还原逻辑行。
    #[test]
    fn clean_pty_model_text_rejoins_conpty_wrap_signature() {
        let logical = format!("{}M{}", "x".repeat(79), "y".repeat(21));
        let conpty = format!("{}M\r\n\x1b[23;80HM{}", "x".repeat(79), "y".repeat(21));
        let cleaned = clean_pty_model_text(&conpty, 80);
        assert_eq!(cleaned, logical, "逻辑行必须还原: {cleaned:?}");
    }

    /// 增量读边界切在签名中间时，跨读状态必须接住（write_stdin 场景）。
    #[test]
    fn conpty_reflower_rejoins_across_read_boundaries() {
        let mut rf = ConptyReflower::new(80);
        let a = rf.feed("abcM\r");
        let b = rf.feed("\n\x1b[23;80H");
        let c = rf.feed("Mdef");
        let d = rf.finish();
        assert_eq!(format!("{a}{b}{c}{d}"), "abcMdef");
        assert!(rf.finish().is_empty(), "finish 必须幂等");
    }

    /// 非签名一律原样：真实 CRLF、非本终端列数的 CUP、重发字符≠边界字符；
    /// 换行规范化只压 `\r\n`→`\n`。
    #[test]
    fn clean_pty_model_text_passthrough_and_newline_normalize() {
        assert_eq!(clean_pty_model_text("a\r\nb", 80), "a\nb");
        // 列数不符：CUP 随后被 strip_ansi 剥除，CRLF 照发。
        assert_eq!(clean_pty_model_text("a\r\n\x1b[10;5Hb", 80), "a\nb");
        // 重发字符 ≠ 边界字符：不是重排签名。
        assert_eq!(clean_pty_model_text("abc\r\n\x1b[23;80HX", 80), "abc\nX");
    }
}
