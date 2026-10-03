//! 参数仍在流式输出时的行数估算——渲染层旁路，不参与执行。
//!
//! 为什么需要它：写工具必须等参数 JSON 完整才谈得上落盘（执行是原子的），
//! 但渲染层希望模型吐出第一行时就能看到「目前 +N −M」。因此这里只做只读
//! 扫描，且任何异常都退化成"沿用上一次的值"——估算通道永远不许影响回合。
//!
//! 口径是**参数行数**，与 `code_delta::compute` 的终态数字同源（有测试锁住
//! "整段参数喂完后的估算 == 终态 CodeChanged"）：
//!
//! - `write` → `content` 的行数；
//! - `edit` → `new_str` 行数 / `old_str` 行数；
//! - `apply_patch` → 补丁里的 `+` / `-` 行（上下文行不算），复用
//!   [`StreamingPatchParser`]，所以计数规则与引擎解析器天然一致。
//!
//! 它因此**不等于**"实际改动行数"：`replace_all` 会成倍、命中失败会归零、
//! 展示层 diff 又是另一个口径。终态一到，权威值立刻接管。

use crate::apply_patch_engine::StreamingPatchParser;

/// 估算出来的参数行数。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ArgLineEstimate {
    pub lines_added: u32,
    pub lines_removed: u32,
}

/// 一个参数键对行数的作用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    /// 该键的字符串值按"新增行"计数。
    Added,
    /// 按"删除行"计数。
    Removed,
    /// 值是 codex 补丁文本，交给 [`StreamingPatchParser`] 数 `+` / `-`。
    Patch,
}

/// 按片段消费 JSON 参数文本的行数估算器。
///
/// 只投喂**新到达的片段**（不是累计串），所以每片段成本恒定。
#[derive(Debug, Default)]
pub struct ArgLineEstimator {
    /// 该工具的参数键 → 计数方向。三个写工具都是扁平 schema，无需处理嵌套。
    targets: &'static [(&'static str, Target)],
    scan: Scan,
    added: LineTally,
    removed: LineTally,
    patch: StreamingPatchParser,
    /// 已解码、尚未投喂给 `patch` 的补丁文本。
    patch_pending: String,
    patch_added: u32,
    patch_removed: u32,
    /// 补丁文本解析失败：冻结读数，等终态接管。
    patch_frozen: bool,
}

#[derive(Debug, Default)]
struct Scan {
    /// `{` / `[` 层数；只有 depth == 1 的字符串才是工具的参数键/值。
    depth: usize,
    in_string: bool,
    string_is_key: bool,
    /// depth 1 上刚读完 `:`，下一个 token 是值。
    want_value: bool,
    last_key: String,
    /// 当前值字符串归属的目标；`None` = 不关心（path / dry_run 等）。
    active: Option<Target>,
    /// 刚吃到 `\`，下一个字符是转义体。
    escaped: bool,
    /// `\uXXXX` 收码中。
    in_hex: bool,
    hex_digits: [u8; 4],
    hex_len: u8,
}

/// 与 `str::lines()` 同口径的行计数：换行符数 + 是否有未收尾的一行。
#[derive(Debug, Default)]
struct LineTally {
    newlines: u32,
    pending: bool,
}

impl LineTally {
    fn feed(&mut self, ch: char) {
        if ch == '\n' {
            self.newlines = self.newlines.saturating_add(1);
            self.pending = false;
        } else {
            self.pending = true;
        }
    }

    fn lines(&self) -> u32 {
        self.newlines.saturating_add(u32::from(self.pending))
    }
}

/// 该工具值得估算吗；以及哪些参数键算数。
fn targets_for(tool_name: &str) -> Option<&'static [(&'static str, Target)]> {
    match tool_name {
        "write" => Some(&[("content", Target::Added)]),
        "edit" => Some(&[("new_str", Target::Added), ("old_str", Target::Removed)]),
        "apply_patch" => Some(&[("patch", Target::Patch)]),
        // 其余工具（read / grep / exec / …）没有行数概念：不估算，
        // 免得给只读调用画出 +/-。
        _ => None,
    }
}

impl ArgLineEstimator {
    /// 写工具之外返回 `None`（没有估算器就没有估算事件）。
    pub fn for_tool(tool_name: &str) -> Option<Self> {
        Some(Self {
            targets: targets_for(tool_name)?,
            ..Self::default()
        })
    }

    /// 投喂一段新到达的参数文本。调用方从累计串里按字节偏移切出余量即可
    /// （前缀切前缀必然是 UTF-8 边界）。
    pub fn push_fragment(&mut self, fragment: &str) {
        for ch in fragment.chars() {
            self.step(ch);
        }
        self.flush_patch();
    }

    /// 当前估算值。逐片段读取的代价恒定。
    pub fn estimate(&self) -> ArgLineEstimate {
        ArgLineEstimate {
            lines_added: self.added.lines().saturating_add(self.patch_added),
            lines_removed: self.removed.lines().saturating_add(self.patch_removed),
        }
    }

    fn step(&mut self, ch: char) {
        if self.scan.in_hex {
            self.consume_hex(ch);
            return;
        }
        if self.scan.escaped {
            self.scan.escaped = false;
            self.consume_escape(ch);
            return;
        }
        if self.scan.in_string {
            match ch {
                '\\' => self.scan.escaped = true,
                '"' => self.close_string(),
                other => self.deliver(other),
            }
            return;
        }
        match ch {
            '{' | '[' => self.scan.depth += 1,
            '}' | ']' => {
                self.scan.depth = self.scan.depth.saturating_sub(1);
                if self.scan.depth <= 1 {
                    self.scan.want_value = false;
                }
            }
            ':' => self.scan.want_value = self.scan.depth == 1,
            // 值可以是 bool / number / 嵌套对象——它们不会把 want_value 清掉。
            // 不在这一个逗号上收尾，后面每个键都会被当成前一个值的尾巴读错
            // （`{"dry_run":true,"content":…}` 实测数出 0 行）。
            ',' if self.scan.depth == 1 => self.scan.want_value = false,
            '"' => self.open_string(),
            _ => {}
        }
    }

    fn open_string(&mut self) {
        self.scan.in_string = true;
        if self.scan.depth != 1 {
            // 嵌套结构里的字符串与行数无关。
            self.scan.string_is_key = false;
            self.scan.active = None;
            return;
        }
        self.scan.string_is_key = !self.scan.want_value;
        if self.scan.string_is_key {
            self.scan.last_key.clear();
            return;
        }
        self.scan.want_value = false;
        self.scan.active = self
            .targets
            .iter()
            .find(|(key, _)| *key == self.scan.last_key.as_str())
            .map(|(_, target)| *target);
    }

    fn close_string(&mut self) {
        self.scan.in_string = false;
        self.scan.active = None;
    }

    /// 把解码后的字符送到它该去的地方。
    fn deliver(&mut self, ch: char) {
        if self.scan.string_is_key {
            self.scan.last_key.push(ch);
            return;
        }
        match self.scan.active {
            Some(Target::Added) => self.added.feed(ch),
            Some(Target::Removed) => self.removed.feed(ch),
            Some(Target::Patch) if !self.patch_frozen => self.patch_pending.push(ch),
            _ => {}
        }
    }

    fn consume_escape(&mut self, ch: char) {
        let decoded = match ch {
            'n' => '\n',
            'r' => '\r',
            't' => '\t',
            'b' => '\u{8}',
            'f' => '\u{c}',
            '"' => '"',
            '\\' => '\\',
            '/' => '/',
            'u' => {
                self.scan.in_hex = true;
                self.scan.hex_len = 0;
                return;
            }
            // 非法转义：连反斜杠一起当内容交付，估算不因噎废食。
            other => {
                self.deliver('\\');
                other
            }
        };
        self.deliver(decoded);
    }

    fn consume_hex(&mut self, ch: char) {
        if !(ch.is_ascii_hexdigit() && self.scan.hex_len < 4) {
            // 序列被打断：丢掉这次转义，当前字符照常交付。
            self.scan.in_hex = false;
            self.deliver(ch);
            return;
        }
        self.scan.hex_digits[self.scan.hex_len as usize] = ch as u8;
        self.scan.hex_len += 1;
        if self.scan.hex_len < 4 {
            return;
        }
        self.scan.in_hex = false;
        let digits = std::str::from_utf8(&self.scan.hex_digits)
            .unwrap_or("000a")
            .to_owned();
        if let Some(decoded) = u32::from_str_radix(&digits, 16)
            .ok()
            .and_then(char::from_u32)
        {
            self.deliver(decoded);
        }
    }

    fn flush_patch(&mut self) {
        if self.patch_pending.is_empty() || self.patch_frozen {
            return;
        }
        let text = std::mem::take(&mut self.patch_pending);
        if self.patch.push_delta(&text).is_err() {
            // 半截补丁里出现非法行：读数停在最后一次好值，终态负责真相。
            self.patch_frozen = true;
            return;
        }
        let stats = self.patch.stats();
        self.patch_added = stats.lines_added as u32;
        self.patch_removed = stats.lines_removed as u32;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::code_delta::compute;

    /// 整段喂 vs 逐字符喂，结果必须一致——这是"边输出边数"的正确性前提。
    fn estimate_of(tool: &str, args_json: &str, chunk: usize) -> ArgLineEstimate {
        let mut est = ArgLineEstimator::for_tool(tool).expect("tool is estimable");
        let chars: Vec<char> = args_json.chars().collect();
        for piece in chars.chunks(chunk) {
            est.push_fragment(&piece.iter().collect::<String>());
        }
        est.estimate()
    }

    fn estimate(tool: &str, args_json: &str) -> ArgLineEstimate {
        estimate_of(tool, args_json, usize::MAX)
    }

    const WRITE_ARGS: &str = r#"{"path":"src/a.rs","content":"one\ntwo\nthree\n"}"#;
    const EDIT_ARGS: &str = r#"{"path":"src/a.rs","old_str":"x\ny","new_str":"X\nY\nZ\n"}"#;
    const PATCH_ARGS: &str = r#"{"patch":"*** Begin Patch\n*** Update File: a.txt\n@@\n-keep\n+KEEP\n context\n+plus\n*** End Patch\n"}"#;

    #[test]
    fn write_counts_content_lines() {
        assert_eq!(
            estimate("write", WRITE_ARGS),
            ArgLineEstimate {
                lines_added: 3,
                lines_removed: 0
            }
        );
    }

    #[test]
    fn edit_counts_both_sides() {
        assert_eq!(
            estimate("edit", EDIT_ARGS),
            ArgLineEstimate {
                lines_added: 3,
                lines_removed: 2
            }
        );
    }

    #[test]
    fn apply_patch_counts_plus_minus_but_not_context() {
        assert_eq!(
            estimate("apply_patch", PATCH_ARGS),
            ArgLineEstimate {
                lines_added: 2,
                lines_removed: 1
            }
        );
    }

    /// 换行以 `\n` 两字符出现在 JSON 文本里：切在反斜杠和 `n` 中间也不能丢行。
    #[test]
    fn splitting_mid_escape_loses_nothing() {
        for chunk in [1, 2, 3, 5, 7, 11, 40] {
            assert_eq!(
                estimate_of("write", WRITE_ARGS, chunk),
                estimate("write", WRITE_ARGS),
                "chunk={chunk}"
            );
            assert_eq!(
                estimate_of("edit", EDIT_ARGS, chunk),
                estimate("edit", EDIT_ARGS),
                "chunk={chunk}"
            );
            assert_eq!(
                estimate_of("apply_patch", PATCH_ARGS, chunk),
                estimate("apply_patch", PATCH_ARGS),
                "chunk={chunk}"
            );
        }
    }

    /// `\uXXXX` 形式的换行同样算一行。
    #[test]
    fn unicode_escape_newline_counts_as_a_line() {
        let args = r#"{"path":"a","content":"one\u000atwo"}"#;
        assert_eq!(estimate("write", args).lines_added, 2);
        assert_eq!(estimate_of("write", args, 1).lines_added, 2);
    }

    /// 非行数字段（path / dry_run / 键名本身）不得被计成行。
    #[test]
    fn unrelated_keys_are_not_counted() {
        let args = r#"{"path":"dir/a\nb\nc","dry_run":true,"content":"single"}"#;
        assert_eq!(
            estimate("write", args),
            ArgLineEstimate {
                lines_added: 1,
                lines_removed: 0
            }
        );
    }

    /// 非字符串值（bool / number / 嵌套对象 / 数组）之后的键必须仍被认作键。
    #[test]
    fn non_string_values_do_not_shift_the_key_pairing() {
        for args in [
            r#"{"dry_run":true,"content":"one\ntwo"}"#,
            r#"{"timeout":30,"content":"one\ntwo"}"#,
            r#"{"extra":{"a":1,"b":[1,2]},"content":"one\ntwo"}"#,
            r#"{"content":"one\ntwo","append":false}"#,
        ] {
            assert_eq!(estimate("write", args).lines_added, 2, "args: {args}");
        }
    }

    /// 值里出现 `{` `}` `:` 不得扰动结构判定。
    #[test]
    fn braces_inside_the_value_are_just_content() {
        let args = r#"{"content":"fn a() {\n}\nsecond"}"#;
        assert_eq!(estimate("write", args).lines_added, 3);
    }

    /// 估算的终点必须等于终态 CodeChanged：数字不能"出现后又变一套"。
    #[test]
    fn matches_the_terminal_code_delta() {
        for (tool, args_json) in [
            ("write", WRITE_ARGS),
            ("edit", EDIT_ARGS),
            ("apply_patch", PATCH_ARGS),
        ] {
            let args: serde_json::Value = serde_json::from_str(args_json).unwrap();
            let terminal = compute(tool, &args).unwrap_or_else(|| panic!("{tool} 终态没出数"));
            assert_eq!(
                estimate(tool, args_json),
                ArgLineEstimate {
                    lines_added: terminal.lines_added as u32,
                    lines_removed: terminal.lines_removed as u32,
                },
                "{tool} 的流式估算与终态口径不一致"
            );
        }
    }

    #[test]
    fn read_only_tools_get_no_estimator() {
        for tool in ["read", "grep", "exec", "confirm_apply", "journal"] {
            assert!(ArgLineEstimator::for_tool(tool).is_none(), "{tool}");
        }
    }

    /// 半截补丁已经非法时：停在最后一次好值，不 panic、不报错。
    #[test]
    fn broken_patch_freezes_the_last_good_number() {
        let mut est = ArgLineEstimator::for_tool("apply_patch").unwrap();
        est.push_fragment(r#"{"patch":"*** Begin Patch\n*** Add File: a.txt\n+one\n"#);
        let good = est.estimate();
        assert_eq!((good.lines_added, good.lines_removed), (1, 0));
        // 一条既不是 `+` 也不是 marker 的行 → 解析器报错。
        est.push_fragment("garbage line\n");
        assert_eq!(est.estimate(), good, "非法后续不得改写已冻结的读数");
        est.push_fragment("*** End Patch\n\"}");
        assert_eq!(est.estimate(), good);
    }

    /// 空参数 / 还没吐出目标键时是 0/0，而不是 None——UI 据此不画。
    #[test]
    fn nothing_arrived_yet_is_zero() {
        assert_eq!(
            estimate("apply_patch", r#"{"pat"#),
            ArgLineEstimate::default()
        );
        assert_eq!(estimate("edit", "{}"), ArgLineEstimate::default());
    }
}
