//! # markdown-winui —— AST → windows-reactor 富文本对接原型
//!
//! 验证目标（对应 UPSTREAM-CAPABILITY-REQUEST §1.1/1.2 API 形状）：
//! 1. `markdown-core` 的解析产物**直接**映射到 fork 内 `windows-reactor`
//!    已有的 `RichTextParagraph` / `RichTextInline` / `RichTextRun` 类型
//!    —— 无需新类型，widget 层（widget.rs）与后端（set_rich_text_paragraphs）
//!    已就绪；
//! 2. 代码块**不**映射到段落：走独立 `CodeBlock` 通道，由 syntect
//!    填充带前景色的原生 token Run；
//! 3. Mermaid 由纯 Rust renderer 生成静态 SVG，再交给 WinUI 原生
//!    `SvgImageSource`；不承载 HTML/JavaScript，也不使用 WebView；
//! 4. 数学公式在 katex Rust 端口就绪前按**字面文本**回退（`throwOnError:
//!    false` 语义 + REFERENCE §9 降级阶梯：图片 / 公式 → 文本降级）。
//!
//! 协议驱动渲染（针对后端设计，`round_renderer`）：
//! - [`protocol`]：事件模型，形状对齐 `deepx-domain::ConversationEvent`
//! - [`round_renderer`]：协议事件 → Transcript 声明式状态；UI 由稳定 key
//!   的 Element 树交给 windows-reactor reconciler 提交到 XAML
//!
//! 已知 fork 缺口（本原型暴露，见 README 可行性矩阵）：
//! - `RichTextInline::Hyperlink` 后端只渲染为普通 Run（无点击事件）
//! - `RichTextRun::is_italic / is_strikethrough` 后端尚未消费

mod block_transcript;
mod diagram;
mod highlight;
mod protocol;
mod round_renderer;
mod timeline_protocol;
mod tool_content;

pub use block_transcript::{
    BLOCK_RESTORE_KEEP_TURNS, BLOCK_WINDOW_DEFAULT_LEN, BlockRestoredTurn, BlockTranscript,
    BlockTurnView, BlockView, MAX_RETAINED_BEFORE_WINDOW, RESTORE_BLOCK_BUDGET,
};
pub use diagram::{DiagramBlock, diagram_view};
pub use highlight::highlighted_code_block;
pub use protocol::{ConversationEvent, ProviderToolState, RoundDeltaKind};
pub use timeline_protocol::{
    TimelineBlock, TimelineBlockKind, TimelineBlockState, TimelineEntry, TimelineEvent,
    TimelineFailure, TimelineRound, TimelineSnapshot, TimelineTool, TimelineToolPermission,
    TimelineToolState, TimelineTurn, TimelineTurnState,
};
pub use round_renderer::{
    AnswerView, LiveSegment, PendingOutput, RESTORE_KEEP_TURNS, RestoredRound, RestoredTurn,
    RoundView, ToolCardView, Transcript, TranscriptChange, TranscriptInvalidation, TurnStatus,
    TurnView,
};
pub use tool_content::{
    ChangeStats, CodeDocument, DiffDocument, DiffFile, DiffRow, DiffRowKind, ToolBody,
    change_stats_from_result, change_stats_from_timeline, diff_file_view, parse_unified_diff,
    tool_body_from_result, tool_body_from_timeline, tool_body_view,
};

use markdown_core::ast::{Block, Inline};
use windows_reactor::{
    BackgroundExt, Element, GridChildExt, GridLength, InputExt, KeyExt, PaddingExt,
    PointerEventInfo, RenderCx, RichTextBlock, RichTextHyperlink, RichTextInline,
    RichTextParagraph, RichTextRun, TextStyleExt, TextAlignment, Updater, border, grid, text_block,
};

/// 一段 markdown 的富文本渲染产物：
/// - `paragraphs` → `RichTextBlock`（`RichTextBlock::single_paragraph` /
///   多段落构造）
/// - `code_blocks` → 独立代码块 widget（需求单 1.2，高亮器消费）
/// - `tables` → Grid 拼装的表格 widget（[`table_view`]）
/// - `blocks` → **有序块序列**（final 全量渲染按此遍历；其余字段是分通道，
///   供旧调用方/实时路径使用）
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RichTextOutput {
    pub paragraphs: Vec<RichTextParagraph>,
    pub code_blocks: Vec<CodeBlock>,
    pub diagrams: Vec<DiagramBlock>,
    pub tables: Vec<TableData>,
    /// final 渲染的有序块（保持文档块顺序：正文/表格/代码块交错）。
    pub blocks: Vec<FinalBlock>,
}

/// final 渲染的有序块（见 [`RichTextOutput::blocks`]）。
#[derive(Clone, Debug, PartialEq)]
pub enum FinalBlock {
    Paragraph(RichTextParagraph),
    Table(TableData),
    Code(CodeBlock),
    Diagram(DiagramBlock),
}

/// 表格数据（markdown GFM 表格的渲染中间表示）。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TableData {
    pub headers: Vec<Vec<Inline>>,
    pub rows: Vec<Vec<Vec<Inline>>>,
}

/// 代码块（等 final；lang 已归一；未知语言走 plain）。
#[derive(Clone, Debug, PartialEq)]
pub struct CodeBlock {
    pub lang: Option<String>,
    pub text: String,
}

/// final 渲染：AST → 富文本产物。
///
/// 块级映射表（对应 REFERENCE §9 WinUI 移植映射）：
/// | AST 块 | 映射 |
/// |---|---|
/// | Paragraph / Heading(h1-h3) | RichTextParagraph（标题字号由上层 widget 处理）|
/// | List | 每 item 一个 Paragraph，带 `• ` / `1. ` 前缀与任务标记 |
/// | Quote | 前缀 `> ` 的 Paragraph（原型简化；引用块样式由上层处理）|
/// | Table | 每行 `| a | b |` 单段（原型简化）|
/// | Code | → `code_blocks`（独立通道，不混入段落）|
/// | Rule | 空段落（分隔线样式由上层处理）|
/// | Image | → alt 文本（降级阶梯：图片降级为文本）|
pub fn render_final(blocks: &[Block]) -> RichTextOutput {
    let mut out = RichTextOutput::default();
    for block in blocks {
        match block {
            Block::Paragraph(inlines) => {
                let para = RichTextParagraph::new(inlines_to_rich(inlines));
                out.blocks.push(FinalBlock::Paragraph(para.clone()));
                out.paragraphs.push(para);
            }
            Block::Heading { level, inlines } => {
                // 标题层级（REFERENCE §7：h1=1.2em / h2=1.1em / h3=1em，500 字重）。
                // 基准 14px → h1 20 / h2 18 / h3 16 + 加粗。
                // 注意：RichTextRun.font_size 后端暂未消费（fork 半成品缺口），
                // 数据层先正确，fork 修复后即生效；is_bold 后端已消费（即时可见）。
                let size = match level {
                    1 => 20.0,
                    2 => 18.0,
                    _ => 16.0,
                };
                let mut para = RichTextParagraph::new(inlines_to_rich(inlines));
                for inline in &mut para.inlines {
                    if let RichTextInline::Run(r) = inline {
                        r.font_size = Some(size);
                        // 标题不加粗（2026-08：RichTextRun 仅有 is_bold 布尔，
                        // 700 太粗；改纯字号层次 h1 20 / h2 18 / h3 16。
                        // 若需 Medium 500 需 fork reactor 给 RichTextRun 加
                        // font_weight（run 级）+ 后端绑定）。
                    }
                }
                out.blocks.push(FinalBlock::Paragraph(para.clone()));
                out.paragraphs.push(para);
            }
            Block::List {
                ordered,
                start,
                items,
            } => {
                let mut n = *start;
                for item in items {
                    let prefix = if item.task.is_some() {
                        match item.task {
                            Some(true) => "☑ ",
                            _ => "☐ ",
                        }
                    } else if *ordered {
                        let label = n.to_string() + ". ";
                        n += 1;
                        let para = RichTextParagraph::new(vec![RichTextInline::Run(
                            RichTextRun::plain(label),
                        )]);
                        out.blocks.push(FinalBlock::Paragraph(para.clone()));
                        out.paragraphs.push(para);
                        // 前缀段已入，内容段继续
                        let para = RichTextParagraph::new(blocks_to_rich(&item.blocks));
                        out.blocks.push(FinalBlock::Paragraph(para.clone()));
                        out.paragraphs.push(para);
                        continue;
                    } else {
                        "• "
                    };
                    let mut inlines = vec![RichTextInline::Run(RichTextRun::plain(prefix))];
                    inlines.extend(blocks_to_rich(&item.blocks));
                    let para = RichTextParagraph::new(inlines);
                    out.blocks.push(FinalBlock::Paragraph(para.clone()));
                    out.paragraphs.push(para);
                }
            }
            Block::ListItem { .. } => {} // 解析层已并入 List
            Block::Quote(children) => {
                for child in children {
                    let mut para = render_final(std::slice::from_ref(child));
                    for b in &mut para.blocks {
                        if let FinalBlock::Paragraph(p) = b {
                            p.inlines
                                .insert(0, RichTextInline::Run(RichTextRun::plain("> ")));
                        }
                    }
                    out.paragraphs.extend(para.paragraphs);
                    out.code_blocks.extend(para.code_blocks);
                    out.diagrams.extend(para.diagrams);
                    out.tables.extend(para.tables);
                    out.blocks.extend(para.blocks);
                }
            }
            Block::Table { headers, rows } => {
                let table = TableData {
                    headers: headers.clone(),
                    rows: rows.clone(),
                };
                out.blocks.push(FinalBlock::Table(table.clone()));
                out.tables.push(table);
            }
            Block::Code { lang, text } => {
                if lang.as_deref() == Some("mermaid") {
                    let diagram = DiagramBlock::render(text);
                    out.blocks.push(FinalBlock::Diagram(diagram.clone()));
                    out.diagrams.push(diagram);
                    continue;
                }
                let code = CodeBlock {
                    lang: lang.clone(),
                    text: text.clone(),
                };
                out.blocks.push(FinalBlock::Code(code.clone()));
                out.code_blocks.push(code);
            }
            Block::Rule => {
                let para = RichTextParagraph::new(Vec::new());
                out.blocks.push(FinalBlock::Paragraph(para.clone()));
                out.paragraphs.push(para);
            }
        }
    }
    out
}

/// 块列表 → 行内列表（列表项 / 引用内容简化路径）。
fn blocks_to_rich(blocks: &[Block]) -> Vec<RichTextInline> {
    let mut out = Vec::new();
    for block in blocks {
        match block {
            Block::Paragraph(inlines) => out.extend(inlines_to_rich(inlines)),
            Block::Code { text, .. } => out.push(RichTextInline::Run(RichTextRun::plain(text))),
            other => out.push(RichTextInline::Run(RichTextRun::plain(
                markdown_core::ast::block_plain_text(other),
            ))),
        }
    }
    out
}

/// 行内 AST → reactor RichTextInline。
///
/// 降级路径（REFERENCE §9）：
/// - `Math` → 字面 `$source$`（katex 端口就绪前；throwOnError:false 语义）
/// - `Image` → alt 文本
/// - `SoftBreak` → 空格（RichTextBlock 内换行由段落负责）
pub fn inlines_to_rich(inlines: &[Inline]) -> Vec<RichTextInline> {
    let mut out = Vec::new();
    for inline in inlines {
        match inline {
            Inline::Text(t) => push_run(&mut out, RichTextRun::plain(t)),
            Inline::Bold(children) => {
                let mut run = RichTextRun::plain("");
                run.is_bold = true;
                // 原型：粗体段聚合为一个 run（子节点拼纯文本）
                run.text = markdown_core::ast::concat_inlines(children);
                push_run(&mut out, run);
            }
            Inline::Italic(children) => {
                let mut run = RichTextRun::plain("");
                run.is_italic = true;
                run.text = markdown_core::ast::concat_inlines(children);
                push_run(&mut out, run);
            }
            Inline::Strikethrough(children) => {
                let mut run = RichTextRun::plain("");
                run.is_strikethrough = true;
                run.text = markdown_core::ast::concat_inlines(children);
                push_run(&mut out, run);
            }
            Inline::Code(c) => {
                // 行内代码：等宽 + CJK fallback（裸 "Consolas" 无中文字形，
                // 中文会落系统默认雅黑，与正文 HarmonyOS 混排割裂——与
                // 代码块 CODE_FONT_FAMILY 同链，此处用系统字体名免资源依赖）。
                let mut run = RichTextRun::plain(c);
                run.font_family =
                    Some("Cascadia Mono, Consolas, Microsoft YaHei UI, HarmonyOS Sans SC".to_string());
                push_run(&mut out, run);
            }
            Inline::Link { text, url } => out.push(RichTextInline::Hyperlink(RichTextHyperlink {
                text: markdown_core::ast::concat_inlines(text),
                uri: url.clone(),
            })),
            Inline::Image { alt, .. } => push_run(&mut out, RichTextRun::plain(alt)),
            Inline::Math { source, display } => {
                // 降级：katex 端口就绪前回退字面（需求单 1.3 验收含此路径）
                let literal = if *display {
                    format!("$${source}$$")
                } else {
                    format!("${source}$")
                };
                push_run(&mut out, RichTextRun::plain(literal));
            }
            Inline::SoftBreak => push_run(&mut out, RichTextRun::plain(" ")),
        }
    }
    out
}

fn is_plain_run(run: &RichTextRun) -> bool {
    !run.is_bold
        && !run.is_italic
        && !run.is_strikethrough
        && run.font_family.is_none()
        && run.font_size.is_none()
}

fn push_run(out: &mut Vec<RichTextInline>, run: RichTextRun) {
    if let Some(RichTextInline::Run(last)) = out.last_mut()
        && is_plain_run(last)
        && is_plain_run(&run)
    {
        // 相邻纯文本合并（减少 Run 数量）
        last.text.push_str(&run.text);
        return;
    }
    out.push(RichTextInline::Run(run));
}

/// 便捷入口：完整 markdown → RichTextBlock widget（对应需求单 1.1
/// `markdown_block(content)` API 形状）。
pub fn markdown_block(markdown: &str) -> RichTextBlock {
    let blocks = markdown_core::parse_final(markdown);
    let out = render_final(&blocks);
    RichTextBlock::new()
        .with_paragraphs(out.paragraphs)
        .wrap()
        .selectable()
}

/// RichTextBlock 便捷扩展（原型用；fork 内可并入 widget.rs）。
pub trait RichTextBlockExt {
    fn with_paragraphs(self, paragraphs: Vec<RichTextParagraph>) -> Self;
}

impl RichTextBlockExt for RichTextBlock {
    fn with_paragraphs(mut self, paragraphs: Vec<RichTextParagraph>) -> Self {
        self.paragraphs = paragraphs;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_reactor::{Element, KeyExt};

    /// 端到端：markdown → RichTextBlock（widget 层），验证零胶水对接
    #[test]
    fn end_to_end_markdown_block() {
        let rt = markdown_block("# Title\n\nsome **bold** text\n\n- [x] done\n- todo");
        assert_eq!(rt.paragraphs.len(), 4, "标题 + 段落 + 2 列表项");
        // 标题
        let title = &rt.paragraphs[0].inlines[0];
        let RichTextInline::Run(run) = title else {
            panic!("expect run")
        };
        assert_eq!(run.text, "Title");
        // 加粗
        let bold = &rt.paragraphs[1].inlines[1];
        let RichTextInline::Run(run) = bold else {
            panic!("expect run")
        };
        assert!(run.is_bold);
        assert_eq!(run.text, "bold");
        // 任务列表前缀
        let task = &rt.paragraphs[2].inlines[0];
        let RichTextInline::Run(run) = task else {
            panic!("expect run")
        };
        assert!(run.text.contains('☑'));
    }

    /// 可挂载性：RichTextBlock 是合法 Element（ElementExt 已实现）
    #[test]
    fn rich_text_block_is_element() {
        let rt = markdown_block("hello");
        let el: Element = rt.into();
        assert!(matches!(el, Element::RichTextBlock(_)));
        let keyed = el.with_key("chat-answer");
        assert_eq!(keyed.key(), Some("chat-answer"));
    }

    /// 代码块走独立通道（不混入段落）
    #[test]
    fn code_block_separate_channel() {
        let out = render_final(&markdown_core::parse_final(
            "text\n```rs\nfn main() {}\n```",
        ));
        assert!(!out.paragraphs.iter().any(|p| {
            p.inlines
                .iter()
                .any(|i| matches!(i, RichTextInline::Run(r) if r.text.contains("fn main")))
        }));
        assert_eq!(out.code_blocks.len(), 1);
        assert_eq!(out.code_blocks[0].lang.as_deref(), Some("rs"));
    }

    #[test]
    fn mermaid_fence_becomes_native_diagram_channel() {
        let out = render_final(&markdown_core::parse_final(
            "before\n\n```mermaid\nflowchart LR; A-->B\n```\n\nafter",
        ));
        assert_eq!(out.diagrams.len(), 1);
        assert!(out.code_blocks.is_empty());
        assert!(matches!(out.blocks[1], FinalBlock::Diagram(_)));
        assert!(
            out.diagrams[0]
                .light_svg
                .as_deref()
                .is_some_and(|svg| svg.contains("<svg"))
        );
    }

    /// 有序块：正文/表格/代码块按文档顺序交错（修复「表格/代码块堆到
    /// 文字之下」：final 渲染必须按 blocks 顺序，而非通道分组）。
    #[test]
    fn final_blocks_preserve_document_order() {
        let md = "前言\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n中间\n\n```rs\nfn x() {}\n```\n\n结尾";
        let out = render_final(&markdown_core::parse_final(md));
        // 三通道各自计数正确。
        assert_eq!(out.paragraphs.len(), 3);
        assert_eq!(out.tables.len(), 1);
        assert_eq!(out.code_blocks.len(), 1);
        // 有序序列：Paragraph → Table → Paragraph → Code → Paragraph。
        assert_eq!(out.blocks.len(), 5);
        assert!(matches!(out.blocks[0], FinalBlock::Paragraph(_)));
        assert!(matches!(out.blocks[1], FinalBlock::Table(_)));
        assert!(matches!(out.blocks[2], FinalBlock::Paragraph(_)));
        assert!(matches!(out.blocks[3], FinalBlock::Code(_)));
        assert!(matches!(out.blocks[4], FinalBlock::Paragraph(_)));
        // 通道内容与 blocks 一致（同序提取可还原文档）。
        let texts: Vec<String> = out
            .blocks
            .iter()
            .filter_map(|b| match b {
                FinalBlock::Paragraph(p) => Some(inline_text(p)),
                _ => None,
            })
            .collect();
        assert!(texts[0].starts_with("前言"));
        assert!(texts[1].starts_with("中间"));
        assert!(texts[2].starts_with("结尾"));
    }

    /// 数学降级：katex 端口前按字面输出
    #[test]
    fn math_falls_back_to_literal() {
        let out = render_final(&markdown_core::parse_final("solve $x^2=4$"));
        let joined: String = out.paragraphs[0]
            .inlines
            .iter()
            .map(|i| match i {
                RichTextInline::Run(r) => r.text.clone(),
                RichTextInline::Hyperlink(h) => h.text.clone(),
                RichTextInline::LineBreak => "\n".to_string(),
            })
            .collect();
        assert!(joined.contains("$x^2=4$"), "必须回退字面: {joined}");
    }

    fn inline_text(p: &RichTextParagraph) -> String {
        p.inlines
            .iter()
            .map(|i| match i {
                RichTextInline::Run(r) => r.text.clone(),
                RichTextInline::Hyperlink(h) => h.text.clone(),
                RichTextInline::LineBreak => "\n".to_string(),
            })
            .collect()
    }

    /// 标题层级：h1 > h2 > h3 字号递减、不加粗（700 太粗，纯字号层次）；
    /// h4+ 降级为加粗段落。
    #[test]
    fn heading_levels_apply_sizes() {
        let out = render_final(&markdown_core::parse_final(
            "# 一级\n\n## 二级\n\n### 三级\n\n#### 四级",
        ));
        assert_eq!(out.paragraphs.len(), 4);
        let size_of = |p: &RichTextParagraph| match &p.inlines[0] {
            RichTextInline::Run(r) => (r.font_size, r.is_bold),
            _ => (None, false),
        };
        assert_eq!(size_of(&out.paragraphs[0]), (Some(20.0), false));
        assert_eq!(size_of(&out.paragraphs[1]), (Some(18.0), false));
        assert_eq!(size_of(&out.paragraphs[2]), (Some(16.0), false));
        // h4 降级：Bold 包裹、无字号（与正文同尺寸，仅加粗）
        assert!(matches!(
            &out.paragraphs[3].inlines[0],
            RichTextInline::Run(r) if r.is_bold && r.font_size.is_none()
        ));
    }

    /// 表格走独立通道（不再降级为文本行）
    #[test]
    fn table_goes_to_separate_channel() {
        let md = "| A | B |\n|---|---|\n| 1 | 2 |\n| 3 | **4** |";
        let out = render_final(&markdown_core::parse_final(md));
        assert_eq!(out.tables.len(), 1);
        assert_eq!(out.tables[0].headers.len(), 2);
        assert_eq!(out.tables[0].rows.len(), 2);
        // 单元格内行内语法保留（Bold 不丢）
        assert!(matches!(
            out.tables[0].rows[1][1].as_slice(),
            [Inline::Bold(_)]
        ));
        // 不产生降级文本段落
        assert!(out.paragraphs.is_empty(), "表格不应进段落通道");
    }

    /// table_view 产出可横向滚动的 Grid 元素树。
    #[test]
    fn table_view_builds_grid() {
        let md = "| A | B |\n|---|---|\n| 1 | 2 |";
        let out = render_final(&markdown_core::parse_final(md));
        let el = table_view(&out.tables[0], "tbl", None);
        let Element::ScrollViewer(scroll) = &el else {
            panic!("expect horizontal scroll viewer");
        };
        let Element::Border(b) = &*scroll.child else {
            panic!("expect border card");
        };
        let Element::Grid(g) = &*b.child else {
            panic!("expect grid inside border");
        };
        assert_eq!(g.columns.len(), 2);
        // 全 Star 列（内容比例铺满，无表格右侧空白；见
        // table_columns_are_content_weighted_star）
        assert!(
            g.columns.iter().all(|c| matches!(c, GridLength::Star(_))),
            "expect star columns, got {:?}",
            g.columns
        );
        // 卡片背景：半透明次级填充（mica 友好），而非纯白 CardBackground
        let bg = b
            .modifiers
            .theme_bindings
            .as_ref()
            .and_then(|m| m.get(&windows_reactor::Prop::Background))
            .map(|t| t.resource_key().to_string());
        assert_eq!(
            bg.as_deref(),
            Some("CardBackgroundFillColorSecondaryBrush"),
            "表格卡片必须用半透明次级填充"
        );
        // 表头行 + 1 数据行 = 2 个跨列行 Border（不再是逐单元格网格）
        assert_eq!(g.children.len(), 2);
        // 表头行：grid_row=0、跨全部列、无背景填充
        let Element::Border(hdr) = &g.children[0] else {
            panic!("expect header row border");
        };
        let gp = hdr.modifiers.grid.as_ref().expect("header grid placement");
        assert_eq!(gp.row, 0);
        assert_eq!(gp.column_span, 2);
        assert!(
            hdr.modifiers
                .theme_bindings
                .as_ref()
                .and_then(|m| m.get(&windows_reactor::Prop::Background))
                .is_none(),
            "方向 A 表头无填充背景"
        );
        // 表头单元格：semibold + 次级色前景
        let Element::Grid(hg) = &*hdr.child else {
            panic!("expect inner grid in header row");
        };
        assert_eq!(hg.columns.len(), 2);
        let Element::TextBlock(h0) = &hg.children[0] else {
            panic!("expect header textblock");
        };
        assert_eq!(h0.text, "A");
        assert_eq!(h0.font_weight, Some(600));
        let fg = h0
            .modifiers
            .theme_bindings
            .as_ref()
            .and_then(|m| m.get(&windows_reactor::Prop::Foreground))
            .map(|t| t.resource_key().to_string());
        assert_eq!(fg.as_deref(), Some("TextFillColorSecondaryBrush"));
        // 数据行：grid_row=1、跨列、行间分隔线；数字单元格右对齐
        let Element::Border(r0) = &g.children[1] else {
            panic!("expect data row border");
        };
        let gp = r0.modifiers.grid.as_ref().expect("row grid placement");
        assert_eq!(gp.row, 1);
        assert_eq!(gp.column_span, 2);
        let Element::Grid(rg) = &*r0.child else {
            panic!("expect inner grid in data row");
        };
        let Element::TextBlock(c0) = &rg.children[0] else {
            panic!("expect cell textblock");
        };
        assert_eq!(c0.text, "1");
        assert_eq!(
            c0.text_alignment,
            Some(windows_reactor::TextAlignment::Center),
            "表格单元格全居中（非 Excel 语义）"
        );
        // hover=None：数据行不注册指针处理器（静态渲染）
        assert!(
            r0.modifiers.pointer_handlers.is_none(),
            "无 hover 状态时不得注册指针处理器"
        );
    }

    /// 列宽 = 内容估算的 Star 权重：长内容列权重 > 短内容列。
    /// 内容少 → 剩余空间按内容比例吸收（无表格右侧空白）；
    /// 内容多 → 列压缩、单元格换行（不横向爆长）。
    #[test]
    fn table_columns_are_content_weighted_star() {
        let md = "| 状态 | 一个比较长的中文单元格内容说明 |\n|---|---|\n| OK | 短 |";
        let out = render_final(&markdown_core::parse_final(md));
        let el = table_view(&out.tables[0], "t", None);
        let Element::ScrollViewer(scroll) = &el else {
            panic!("expect scroll viewer");
        };
        let Element::Border(b) = &*scroll.child else {
            panic!("expect border card");
        };
        let Element::Grid(g) = &*b.child else {
            panic!("expect grid inside border");
        };
        let [GridLength::Star(w0), GridLength::Star(w1)] = g.columns.as_slice() else {
            panic!("expect star columns, got {:?}", g.columns);
        };
        // 长内容列（col1）权重 > 短内容列（col0）
        assert!(
            *w1 > *w0,
            "长内容列权重应更大: col0={w0} vs col1={w1}"
        );
        // 权重不小于 1（空内容列保底）
        assert!(*w0 >= 1.0 && *w1 >= 1.0);
    }
}

/// 表格行悬停状态：由组件层持有（`use_reducer`），经渲染链透传给
/// [`table_view`] 只读消费。行 key 由调用方生成（表格 key + 行号）。
///
/// 每次渲染时 `rows` 是当前快照；进入/离开回调由行元素注册，内部走
/// `Updater` 函数式更新（不需要读旧值）。
#[derive(Clone)]
pub struct TableHover {
    rows: std::collections::HashMap<String, bool>,
    updater: Updater<std::collections::HashMap<String, bool>>,
}

impl TableHover {
    /// 在组件渲染函数中创建（hook 调用，顺序必须稳定）。
    pub fn new(cx: &mut RenderCx) -> Self {
        let (rows, updater) = cx.use_reducer::<std::collections::HashMap<String, bool>>(
            std::collections::HashMap::new(),
        );
        Self { rows, updater }
    }

    /// 某行当前是否悬停。
    pub fn row_hovered(&self, key: &str) -> bool {
        self.rows.get(key).copied().unwrap_or(false)
    }

    /// 行进入悬停的回调（`on_pointer_entered`）。
    pub fn row_enter(&self, key: String) -> impl Fn(PointerEventInfo) + Clone + 'static {
        let updater = self.updater.clone();
        move |_info: PointerEventInfo| {
            let key = key.clone();
            updater.call(move |mut rows: std::collections::HashMap<String, bool>| {
                rows.insert(key, true);
                rows
            });
        }
    }

    /// 行离开悬停的回调（`on_pointer_exited`）。
    pub fn row_leave(&self, key: String) -> impl Fn() + Clone + 'static {
        let updater = self.updater.clone();
        move || {
            let key = key.clone();
            updater.call(move |mut rows: std::collections::HashMap<String, bool>| {
                rows.insert(key, false);
                rows
            });
        }
    }
}

/// 数字/百分比单元格判断 → 右对齐（大小、进度等数据列）。
/// 支持 `%` 后缀、千分位逗号、小数（至多一个 `.`）、负数。
/// 表格 → reactor Grid 元素树（现代行分隔线风格）。
///
/// 布局：表头行（row 0，无填充、semibold + 次级色，底部 1px 分隔线）+
/// 数据行（row 1..，行间 1px 分隔线、悬停 `SubtleFill` 整行高亮）。
/// 无竖线（去 Excel 网格感）；数字列右对齐；主键列（第一列）semibold。
///
/// `hover` 为 `Some` 时启用行悬停（由组件层持有状态）；`None` 则静态渲染
/// （单元测试 / 无需交互的场景）。
///
/// 列宽策略（修复「字少右侧空白 / 字多横向爆长」）：
/// - 每列按内容估算宽度生成 Star 权重（全 Star，无 Auto 列）；
/// - 内容少：剩余空间按内容比例吸收 → 表格铺满视口，右侧不留白；
/// - 内容多：列被压缩、单元格换行（wrap）→ 表格保持视口宽，不横向拉长；
/// - 超长内容（长 URL 等）在 Star 列内换行，不会撑爆表格。
pub fn table_view(table: &TableData, key: &str, hover: Option<&TableHover>) -> Element {
    let n_cols = table.headers.len().max(1);
    let n_rows = 1 + table.rows.len();

    // 列宽：内容分类 + 精确显示宽（Codex 移植，markdown-core::table_layout）。
    // Star 按内容宽度比例铺满可用宽（方向 A：无表格右侧空白）；字符单元
    // 权重（ASCII=1/CJK=2）即真实渲染 px 的比例，长内容列份额大属正常。
    // 压缩能力已就位（compute_column_widths 的 available_width），后续
    // 接入视口宽度时启用优先级压缩（TokenHeavy 先让 / Compact 保底）。
    let metrics = markdown_core::table_layout::collect_column_metrics(
        &table.headers,
        &table.rows,
        n_cols,
    );
    let widths = markdown_core::table_layout::compute_column_widths(&metrics, None)
        .unwrap_or_else(|| vec![3; n_cols]);
    // 列宽 = 内容比例（Star 铺满，无右侧空白）。数字列不做权重上浮
    // （曾尝试 ×1.5/×2.0，带单位识别生效后数字列被撑到右侧列，回退）。
    let cols: Vec<GridLength> = widths
        .iter()
        .map(|w| GridLength::Star(*w as f64))
        .collect();

    let mut children: Vec<Element> = Vec::new();

    // 表头行（row 0）：跨列 Border，无背景，底部 1px 分隔线。
    let mut header_cells: Vec<Element> = Vec::new();
    for (ci, cell) in table.headers.iter().enumerate() {
        let mut tb = text_block(markdown_core::ast::concat_inlines(cell))
            .wrap()
            .semibold()
            .foreground(windows_reactor::ThemeRef::SecondaryText);
        tb.text_alignment = Some(TextAlignment::Center);
        header_cells.push(
            tb.grid_column(ci as i32)
                .with_key(format!("{key}-h{ci}"))
                .into(),
        );
    }
    children.push(
        border(grid(header_cells).columns(cols.clone()))
            .border_brush(windows_reactor::ThemeRef::DividerStroke)
            .border_thickness(windows_reactor::Thickness {
                left: 0.0,
                top: 0.0,
                right: 0.0,
                bottom: 1.0,
            })
            .padding(windows_reactor::Thickness::xy(12.0, 10.0))
            .grid_row(0)
            .grid_column_span(n_cols as i32)
            .with_key(format!("{key}-header"))
            .into(),
    );

    // 数据行（row 1..）：跨列 Border，行间 1px 分隔线（末行无），悬停高亮。
    for (ri, row) in table.rows.iter().enumerate() {
        let row_key = format!("{key}-r{ri}");
        let hovered = hover.is_some_and(|h| h.row_hovered(&row_key));
        let mut cells: Vec<Element> = Vec::new();
        for (ci, cell) in row.iter().enumerate() {
            let text = markdown_core::ast::concat_inlines(cell);
            let mut tb = text_block(text.clone()).wrap();
            // 全居中（对话工具语义，非 Excel 网格）；主键列 semibold。
            tb.text_alignment = Some(TextAlignment::Center);
            if ci == 0 {
                tb = tb.semibold();
            }
            cells.push(
                tb.grid_column(ci as i32)
                    .with_key(format!("{row_key}c{ci}"))
                    .into(),
            );
        }
        let mut row_el = border(grid(cells).columns(cols.clone()))
            .border_brush(windows_reactor::ThemeRef::DividerStroke)
            .border_thickness(windows_reactor::Thickness {
                left: 0.0,
                top: 0.0,
                right: 0.0,
                bottom: if ri + 1 < table.rows.len() { 1.0 } else { 0.0 },
            })
            .padding(windows_reactor::Thickness::xy(12.0, 10.0))
            .grid_row(ri as i32 + 1)
            .grid_column_span(n_cols as i32)
            .with_key(row_key.clone());
        if let Some(h) = hover {
            row_el = row_el
                .on_pointer_entered(h.row_enter(row_key.clone()))
                .on_pointer_exited(h.row_leave(row_key.clone()));
            if hovered {
                row_el = row_el.background(windows_reactor::ThemeRef::SubtleFill);
            }
        }
        children.push(row_el.into());
    }

    let rows_def = std::iter::repeat_n(GridLength::Auto, n_rows);
    let card = border(grid(children).columns(cols).rows(rows_def))
        // 半透明次级卡片填充：与代码卡片一致，mica 背景下透出质感而非纯白块。
        .background(windows_reactor::ThemeRef::custom(
            "CardBackgroundFillColorSecondaryBrush",
        ))
        .corner_radius(8.0)
        .border_brush(windows_reactor::ThemeRef::CardStroke)
        .border_thickness(windows_reactor::Thickness::uniform(1.0))
        .with_key(format!("{key}-card"));
    windows_reactor::scroll_viewer(card)
        .horizontal_scroll_bar_visibility(windows_reactor::ScrollBarVisibility::Auto)
        .vertical_scroll_bar_visibility(windows_reactor::ScrollBarVisibility::Disabled)
        .with_key(key)
        .into()
}
