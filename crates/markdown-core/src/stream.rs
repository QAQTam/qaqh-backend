//! 流式渲染管线（对应 REFERENCE §5 MarkdownBody 语义）。
//!
//! 结构对齐 Web 端：
//! ```text
//! projectBlocks(text, final):
//!   final → [{ key:"f", hash, raw, stable:true }]   // 全量渲染一次
//!   live  → [{ key:"l0", hash, raw, stable:false }] // cheap inline preview
//! ```
//!
//! 本模块提供三个性能契约（REFERENCE §8）：
//! 1. **流式追加 O(1)**：`append` 只重解析活尾（最后一个未闭合块），
//!    已封块（stable）零重渲染；
//! 2. **块哈希缓存**：`len:head…tail` 摘要，内容未变 → `RenderOp::Noop`；
//! 3. **代码块等 final**：未闭合代码块在 live 阶段不 lex（不出 Code 块）。
//!
//! 未闭合语法的字面输出由 [`crate::live::parse_live`] 保证（§3 语义 1）。

use crate::ast::{Block, Inline};
use crate::live::parse_live;
use crate::parse::parse_final;

/// 流式 markdown 文档：`finalized` 已封块 + `pending` 活尾。
#[derive(Clone, Debug, Default)]
pub struct StreamingMarkdown {
    /// 已闭合的块（stable，累积；追加不重渲染）。
    finalized: Vec<Block>,
    /// 当前活尾的原始文本（未闭合块）。
    pending_raw: String,
    /// 活尾内容的块哈希（`len:head…tail`）。
    pending_hash: u64,
    /// 是否处于未闭合代码块中（代码块等 final，live 阶段不 lex）。
    in_open_code_block: bool,
}

impl StreamingMarkdown {
    pub fn new() -> Self {
        Self::default()
    }

    /// 已封块列表（stable 块；UI 层按 key 缓存，不随追加变化）。
    pub fn finalized(&self) -> &[Block] {
        &self.finalized
    }

    /// 当前活尾原文。
    pub fn pending_raw(&self) -> &str {
        &self.pending_raw
    }

    /// 追加增量文本，返回本轮渲染计划。
    ///
    /// 语义（对齐 Web `projectBlocks`）：
    /// - 追加后重新拆分块：完整闭合的块 → `Final`（stable）；
    /// - 最后一个未闭合块 → `Live`（行内预览，仅已闭合语法）；
    /// - 内容未变化 → `Noop`（块哈希缓存命中）；
    /// - 未闭合代码块 → `WaitFinal`（不 lex，等 final 全量渲染）。
    pub fn append(&mut self, delta: &str) -> RenderPlan {
        self.pending_raw.push_str(delta);
        self.reproject()
    }

    /// 强制把当前活尾按 final 封块（producer 封块语义 `final=true`）。
    pub fn finalize(&mut self) -> RenderPlan {
        if self.pending_raw.is_empty() {
            return RenderPlan::default();
        }
        // 活尾整体解析为 final 并入 finalized
        let pending = std::mem::take(&mut self.pending_raw);
        let blocks = parse_final(&pending);
        self.in_open_code_block = false;
        self.pending_hash = 0;
        let mut plan = RenderPlan::default();
        if !blocks.is_empty() {
            self.finalized.extend(blocks.clone());
            plan.ops.push(RenderOp::Final { blocks });
        }
        plan
    }

    /// 当前完整文档（finalized + pending 合并后全量解析——仅调试/导出用，
    /// 渲染路径不调用，保持 O(1) 追加）。
    pub fn full_text(&self) -> String {
        let mut out = String::new();
        for block in &self.finalized {
            out.push_str(&crate::ast::block_plain_text(block));
            out.push('\n');
        }
        out.push_str(&self.pending_raw);
        out
    }

    /// 重新投影（append 内部）：只拆分活尾，已封块零重渲染。
    fn reproject(&mut self) -> RenderPlan {
        let mut plan = RenderPlan::default();

        // 1. 活尾按"已闭合块"重新拆分：新闭合的块解析为 final 并累积
        let pending = std::mem::take(&mut self.pending_raw);
        let (closed_text, tail) = split_pending(&pending, &mut self.in_open_code_block);
        if !closed_text.is_empty() {
            let blocks = parse_final(&closed_text);
            if !blocks.is_empty() {
                self.finalized.extend(blocks.clone());
                plan.ops.push(RenderOp::Final { blocks });
            }
        }
        self.pending_raw = tail.to_string();

        // 2. 活尾 live 预览
        if !self.pending_raw.is_empty() {
            let hash = block_hash(&self.pending_raw);
            if hash != self.pending_hash {
                self.pending_hash = hash;
                if self.in_open_code_block {
                    // 代码块等 final：live 阶段不 lex，不产出破损布局
                    plan.ops.push(RenderOp::WaitFinal);
                } else {
                    plan.ops.push(RenderOp::Live {
                        inlines: parse_live(&self.pending_raw),
                        raw: self.pending_raw.clone(),
                    });
                }
            } else {
                plan.ops.push(RenderOp::Noop);
            }
        } else {
            // 活尾为空：本轮无 live（若也没有 Final，则 Noop）
            if plan.ops.is_empty() {
                plan.ops.push(RenderOp::Noop);
            }
        }
        plan
    }
}

/// 渲染计划（一轮 append/finalize 的产物）。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RenderPlan {
    pub ops: Vec<RenderOp>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum RenderOp {
    /// 已封块：全量渲染一次（stable，替代旧 Final）。
    Final { blocks: Vec<Block> },
    /// 活尾行内预览（仅已闭合语法；未闭合字面输出）。
    Live { inlines: Vec<Inline>, raw: String },
    /// 活尾是未闭合代码块：等 final，不 lex。
    WaitFinal,
    /// 块哈希命中：内容未变化，零重渲染。
    Noop,
}

/// 块哈希（对齐 Web `blockHash`：`len:head…tail`）。
fn block_hash(text: &str) -> u64 {
    let len = text.len();
    let head: u64 = text
        .chars()
        .take(16)
        .fold(0u64, |acc, c| acc.wrapping_mul(31).wrapping_add(c as u64));
    let tail: u64 = text
        .chars()
        .rev()
        .take(16)
        .fold(0u64, |acc, c| acc.wrapping_mul(31).wrapping_add(c as u64));
    (len as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(head).wrapping_mul(31).wrapping_add(tail)
}

/// 把待定文本拆成 (已闭合块原文, 活尾)。
///
/// 拆块规则（块级语法检测，与 marked 的块级 lexer 对齐到 ChatView 所需子集）：
/// - 空行是块边界（仅 fence 外）；
/// - 围栏代码块 ``` 未闭合 → 未闭合围栏之前为已闭合块，之后整体为活尾，
///   且置 `in_open_code_block`（代码块等 final，live 不 lex）；
/// - 围栏块**整体闭合**（开 fence 行到闭 fence 行）后即视为已闭合内容，
///   围栏内部的空行不参与块切分。
///
/// 返回 `(已闭合块原文, 活尾)`；活尾可能为空串。
fn split_pending<'a>(
    pending: &'a str,
    in_open_code_block: &mut bool,
) -> (String, &'a str) {
    if pending.is_empty() {
        return (String::new(), pending);
    }

    let mut closed_end = 0usize; // 已闭合内容末尾（字节偏移，含换行）
    let mut fence_open: Option<usize> = None; // 未闭合 fence 行起始偏移
    let mut offset = 0usize;
    let mut in_fence = false;
    let mut saw_fence = false;

    for line in pending.split('\n') {
        let trimmed = line.trim_start();
        let is_fence = trimmed.starts_with("```") || trimmed.starts_with("~~~");
        if is_fence {
            saw_fence = true;
            if in_fence {
                // 闭合围栏：围栏块整体闭合
                in_fence = false;
                closed_end = offset + line.len();
            } else {
                in_fence = true;
                fence_open = Some(offset);
            }
        } else if !in_fence && line.is_empty() {
            // fence 外的空行：闭合到该行末尾（含换行；尾行 clamp 防越界）
            closed_end = (offset + line.len() + 1).min(pending.len());
        }
        offset += line.len() + 1;
    }

    if in_fence {
        // 未闭合围栏：围栏行之前是已闭合块，围栏行起是活尾
        let open_offset = fence_open.unwrap_or(0);
        *in_open_code_block = true;
        return (pending[..open_offset].to_string(), &pending[open_offset..]);
    }

    *in_open_code_block = false;
    if saw_fence {
        // 有围栏：闭合围栏行之后的内容（若有）为活尾
        let closed = pending[..closed_end].to_string();
        let tail = &pending[closed_end..];
        (closed, tail)
    } else {
        // 无围栏：纯空行切分
        let closed = pending[..closed_end].to_string();
        let tail = &pending[closed_end..];
        (closed, tail)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::Inline as I;

    #[test]
    fn first_append_all_pending() {
        let mut md = StreamingMarkdown::new();
        let plan = md.append("hello **world**");
        assert_eq!(plan.ops.len(), 1);
        let RenderOp::Live { inlines, .. } = &plan.ops[0] else {
            panic!("expect live");
        };
        assert!(inlines.contains(&I::Bold(vec![I::Text("world".into())])));
    }

    /// 关键性能契约：空行封块后追加不重建前文（O(1) 追加）
    #[test]
    fn append_after_block_break_is_o1() {
        let mut md = StreamingMarkdown::new();
        let first = md.append("para one\n\n");
        // 第一次追加：空行封块 → Final
        assert!(first.ops.iter().any(|op| matches!(op, RenderOp::Final { .. })));
        assert_eq!(md.finalized().len(), 1);

        let second = md.append("para two");
        // 第二次追加：前文已封块 → **不再产生 Final**（零重渲染），只 Live
        assert!(!second.ops.iter().any(|op| matches!(op, RenderOp::Final { .. })));
        assert!(second.ops.iter().any(|op| matches!(op, RenderOp::Live { .. })));
        assert_eq!(md.finalized().len(), 1, "已封块不随追加增长（活尾未封）");

        // 活尾内容未变化 → 哈希命中 Noop
        let plan2 = md.append("");
        assert!(plan2.ops.iter().all(|op| matches!(op, RenderOp::Noop)));
    }

    #[test]
    fn unchanged_tail_is_noop() {
        let mut md = StreamingMarkdown::new();
        md.append("abc");
        let plan = md.append("");
        assert!(plan
            .ops
            .iter()
            .all(|op| matches!(op, RenderOp::Noop)));
    }

    /// 关键语义：未闭合代码块 → WaitFinal（live 不 lex）
    #[test]
    fn open_code_block_waits_final() {
        let mut md = StreamingMarkdown::new();
        let plan = md.append("text\n```rs\nfn main() {");
        let ops = &plan.ops;
        assert!(
            ops.iter().any(|op| matches!(op, RenderOp::WaitFinal)),
            "未闭合代码块必须等 final: {ops:?}"
        );
        // 代码块内不出现 Live 预览
        assert!(!ops.iter().any(|op| matches!(op, RenderOp::Live { .. })));
    }

    #[test]
    fn code_block_finalizes_on_close() {
        let mut md = StreamingMarkdown::new();
        md.append("```rs\nfn main() {}\n```");
        // 围栏闭合即封块（不等 finalize）
        assert!(
            md.finalized()
                .iter()
                .any(|b| matches!(b, Block::Code { lang: Some(l), .. } if l == "rs")),
            "闭合围栏应立即产出 Code 块"
        );
        let plan = md.finalize();
        assert!(plan.ops.is_empty(), "已封块后 finalize 无新内容");
    }

    #[test]
    fn unclosed_inline_in_tail_is_literal() {
        let mut md = StreamingMarkdown::new();
        let plan = md.append("see **bold");
        let RenderOp::Live { inlines, .. } = &plan.ops[0] else {
            panic!("expect live");
        };
        assert_eq!(
            inlines,
            &[I::Text("see **bold".to_string())],
            "未闭合 ** 必须字面输出"
        );
    }

    #[test]
    fn finalize_merges_tail() {
        let mut md = StreamingMarkdown::new();
        md.append("para\n\n");
        md.append("tail **x**");
        let plan = md.finalize();
        assert!(plan.ops.iter().any(|op| matches!(op, RenderOp::Final { .. })));
    }
}
