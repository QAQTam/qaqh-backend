//! # markdown-winui —— AST → windows-reactor 富文本对接原型
//!
//! 验证目标（对应 UPSTREAM-CAPABILITY-REQUEST §1.1/1.2 API 形状）：
//! 1. `markdown-core` 的解析产物**直接**映射到 fork 内 `windows-reactor`
//!    已有的 `RichTextParagraph` / `RichTextInline` / `RichTextRun` 类型
//!    —— 无需新类型，widget 层（widget.rs）与后端（set_rich_text_paragraphs）
//!    已就绪；
//! 2. 代码块**不**映射到段落：走独立 `CodeBlock` 通道（需求单 1.2 的
//!    `code_block(text, lang, theme)`，由高亮器填充 token Run）；
//! 3. 数学公式在 katex Rust 端口就绪前按**字面文本**回退（`throwOnError:
//!    false` 语义 + REFERENCE §9 降级阶梯：图片 / 公式 → 文本降级）。
//!
//! 协议驱动渲染（针对后端设计，`round_renderer`）：
//! - [`protocol`]：事件模型，形状对齐 `deepx-domain::ConversationEvent`
//! - [`round_renderer`]：Transcript 状态机 → 渲染命令序列（live 尾巴 /
//!   final 重建 / 工具卡 upsert / 内容局域化），UI 层按命令执行
//!
//! 已知 fork 缺口（本原型暴露，见 README 可行性矩阵）：
//! - `RichTextRun` 缺前景色字段（高亮 token 着色需要 fork 扩展）
//! - `RichTextInline::Hyperlink` 后端只渲染为普通 Run（无点击事件）
//! - `RichTextRun::is_italic / is_strikethrough / font_family / font_size`
//!   后端 `set_rich_text_paragraphs` 尚未消费（半成品）

mod protocol;
mod round_renderer;

pub use protocol::{ConversationEvent, RoundDeltaKind};
pub use round_renderer::{
    AnswerView, LiveSegment, RenderCommand, RestoredRound, RestoredTurn, RoundView, ToolCardView,
    Transcript, TurnStatus, TurnView,
};

use markdown_core::ast::{Block, Inline};
use markdown_core::{RenderOp, RenderPlan, StreamingMarkdown};
use windows_reactor::{
    Element, ElementExt, GridLength, RichTextBlock, RichTextHyperlink, RichTextInline,
    RichTextParagraph, RichTextRun, border, grid, text_block,
};

/// 一段 markdown 的富文本渲染产物：
/// - `paragraphs` → `RichTextBlock`（`RichTextBlock::single_paragraph` /
///   多段落构造）
/// - `code_blocks` → 独立代码块 widget（需求单 1.2，高亮器消费）
/// - `tables` → Grid 拼装的表格 widget（[`table_view`]）
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RichTextOutput {
    pub paragraphs: Vec<RichTextParagraph>,
    pub code_blocks: Vec<CodeBlock>,
    pub tables: Vec<TableData>,
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

/// 流式渲染器：把 `StreamingMarkdown` 的渲染计划映射为
/// `RichTextOutput` 增量（live 只更新段落，final 全量替换）。
#[derive(Clone, Debug, Default)]
pub struct StreamingRichText {
    pub output: RichTextOutput,
}

impl StreamingRichText {
    pub fn new() -> Self {
        Self::default()
    }

    /// 追加增量并应用渲染计划。
    pub fn append(&mut self, md: &mut StreamingMarkdown, delta: &str) {
        let plan = md.append(delta);
        self.apply(&plan);
    }

    pub fn finalize(&mut self, md: &mut StreamingMarkdown) {
        let plan = md.finalize();
        self.apply(&plan);
    }

    fn apply(&mut self, plan: &RenderPlan) {
        for op in &plan.ops {
            match op {
                RenderOp::Final { blocks } => {
                    self.output = render_final(blocks);
                }
                RenderOp::Live { inlines, .. } => {
                    // live 阶段：仅替换"活尾段落"（最后一段）
                    let tail_para = RichTextParagraph::new(inlines_to_rich(inlines));
                    if self.output.paragraphs.is_empty() {
                        self.output.paragraphs.push(tail_para);
                    } else {
                        let last = self.output.paragraphs.len() - 1;
                        self.output.paragraphs[last] = tail_para;
                    }
                }
                RenderOp::WaitFinal | RenderOp::Noop => {}
            }
        }
    }
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
                out.paragraphs
                    .push(RichTextParagraph::new(inlines_to_rich(inlines)));
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
                        r.is_bold = true;
                    }
                }
                out.paragraphs.push(para);
            }
            Block::List { ordered, start, items } => {
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
                        out.paragraphs.push(RichTextParagraph::new(vec![
                            RichTextInline::Run(RichTextRun::plain(label)),
                        ]));
                        // 前缀段已入，内容段继续
                        out.paragraphs.push(RichTextParagraph::new(
                            blocks_to_rich(&item.blocks),
                        ));
                        continue;
                    } else {
                        "• "
                    };
                    let mut inlines = vec![RichTextInline::Run(RichTextRun::plain(prefix))];
                    inlines.extend(blocks_to_rich(&item.blocks));
                    out.paragraphs.push(RichTextParagraph::new(inlines));
                }
            }
            Block::ListItem { .. } => {} // 解析层已并入 List
            Block::Quote(children) => {
                for child in children {
                    let mut para = render_final(std::slice::from_ref(child));
                    for p in &mut para.paragraphs {
                        p.inlines.insert(
                            0,
                            RichTextInline::Run(RichTextRun::plain("> ")),
                        );
                    }
                    out.paragraphs.extend(para.paragraphs);
                    out.code_blocks.extend(para.code_blocks);
                }
            }
            Block::Table { headers, rows } => out.tables.push(TableData {
                headers: headers.clone(),
                rows: rows.clone(),
            }),
            Block::Code { lang, text } => out.code_blocks.push(CodeBlock {
                lang: lang.clone(),
                text: text.clone(),
            }),
            Block::Rule => out.paragraphs.push(RichTextParagraph::new(Vec::new())),
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
                // 行内代码：mono 字体（fork 的 font_family 字段已预留）
                let mut run = RichTextRun::plain(c);
                run.font_family = Some("Consolas".to_string());
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
    RichTextBlock::new().with_paragraphs(out.paragraphs).wrap().selectable()
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
    use windows_reactor::{Element, ElementExt};

    /// 端到端：markdown → RichTextBlock（widget 层），验证零胶水对接
    #[test]
    fn end_to_end_markdown_block() {
        let rt = markdown_block("# Title\n\nsome **bold** text\n\n- [x] done\n- todo");
        assert_eq!(rt.paragraphs.len(), 4, "标题 + 段落 + 2 列表项");
        // 标题
        let title = &rt.paragraphs[0].inlines[0];
        let RichTextInline::Run(run) = title else { panic!("expect run") };
        assert_eq!(run.text, "Title");
        // 加粗
        let bold = &rt.paragraphs[1].inlines[1];
        let RichTextInline::Run(run) = bold else { panic!("expect run") };
        assert!(run.is_bold);
        assert_eq!(run.text, "bold");
        // 任务列表前缀
        let task = &rt.paragraphs[2].inlines[0];
        let RichTextInline::Run(run) = task else { panic!("expect run") };
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
        let out = render_final(&markdown_core::parse_final("text\n```rs\nfn main() {}\n```"));
        assert!(!out.paragraphs.iter().any(|p| p.inlines.iter().any(|i| {
            matches!(i, RichTextInline::Run(r) if r.text.contains("fn main"))
        })));
        assert_eq!(out.code_blocks.len(), 1);
        assert_eq!(out.code_blocks[0].lang.as_deref(), Some("rs"));
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

    /// 流式：live 期间段落增量更新，final 全量替换
    #[test]
    fn streaming_live_then_final() {
        let mut md = StreamingMarkdown::new();
        let mut view = StreamingRichText::new();
        view.append(&mut md, "hello **wor");
        assert_eq!(view.output.paragraphs.len(), 1);
        // 未闭合 **：字面
        let joined = inline_text(&view.output.paragraphs[0]);
        assert_eq!(joined, "hello **wor");

        view.append(&mut md, "ld**");
        let joined = inline_text(&view.output.paragraphs[0]);
        assert!(joined.contains("bold") || joined.contains("world"), "{joined}");
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

    /// 标题层级：h1 > h2 > h3 字号递减 + 加粗；h4+ 降级为加粗段落
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
        assert_eq!(size_of(&out.paragraphs[0]), (Some(20.0), true));
        assert_eq!(size_of(&out.paragraphs[1]), (Some(18.0), true));
        assert_eq!(size_of(&out.paragraphs[2]), (Some(16.0), true));
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

    /// table_view 产出 Grid 元素树（表头加粗 + grid_row/column 定位）
    #[test]
    fn table_view_builds_grid() {
        let md = "| A | B |\n|---|---|\n| 1 | 2 |";
        let out = render_final(&markdown_core::parse_final(md));
        let el = table_view(&out.tables[0], "tbl");
        // 外层是描边卡片（Border），内层是 Grid
        let Element::Border(b) = &el else {
            panic!("expect border card");
        };
        let Element::Grid(g) = &*b.child else {
            panic!("expect grid inside border");
        };
        assert_eq!(g.columns.len(), 2);
        // 表头 2 个 + 数据行 2 个 = 4 个子元素
        assert_eq!(g.children.len(), 4);
        // 表头单元格携带 grid_row=0（单元格是 Border 卡片）
        let grid = g.children[0].modifiers().and_then(|m| m.grid.as_ref());
        assert_eq!(grid.map(|g| g.row), Some(0));
        assert_eq!(grid.map(|g| g.column), Some(0));
        // 表头加粗 + 背景（视觉区分）
        let Element::Border(h0) = &g.children[0] else {
            panic!("expect border cell");
        };
        let Element::TextBlock(tb) = &*h0.child else {
            panic!("expect textblock in cell");
        };
        assert_eq!(tb.font_weight, Some(600));
        // 数据行单元格 grid_row=1
        let grid = g.children[2].modifiers().and_then(|m| m.grid.as_ref());
        assert_eq!(grid.map(|g| g.row), Some(1));
        assert_eq!(grid.map(|g| g.column), Some(0));
    }
}

/// 表格 → reactor Grid 元素树。
///
/// 布局：表头行（row 0，加粗 + 半透明背景 + 底部粗线）+ 数据行（row 1..，
/// 单元格右/下 1px 细线构成网格）；列宽等分 Star；整表描边卡片。
/// 行/列必须显式定义（WinUI Grid 无定义时 SetRow 会重叠）。
pub fn table_view(table: &TableData, key: &str) -> Element {
    let n_cols = table.headers.len().max(1);
    let n_rows = 1 + table.rows.len();
    let mut children: Vec<Element> = Vec::new();

    // 表头行（row 0）：加粗 + 背景 + 底部粗线
    for (ci, cell) in table.headers.iter().enumerate() {
        children.push(
            table_cell(
                markdown_core::ast::concat_inlines(cell),
                format!("{key}-h{ci}"),
                0,
                ci as i32,
                n_cols as i32,
                n_rows as i32,
                true,
            ),
        );
    }
    // 数据行（row 1..）：单元格右/下 1px 细线
    for (ri, row) in table.rows.iter().enumerate() {
        for (ci, cell) in row.iter().enumerate() {
            children.push(table_cell(
                markdown_core::ast::concat_inlines(cell),
                format!("{key}-r{ri}c{ci}"),
                ri as i32 + 1,
                ci as i32,
                n_cols as i32,
                n_rows as i32,
                false,
            ));
        }
    }

    let cols = std::iter::repeat_n(GridLength::Star(1.0), n_cols);
    let rows = std::iter::repeat_n(GridLength::Auto, n_rows);
    border(grid(children).columns(cols).rows(rows))
        .corner_radius(6.0)
        .border_brush(windows_reactor::Color {
            a: 255,
            r: 190,
            g: 190,
            b: 190,
        })
        .border_thickness(windows_reactor::Thickness {
            left: 1.0,
            top: 1.0,
            right: 1.0,
            bottom: 1.0,
        })
        .with_key(key)
        .into()
}

/// 单个表格单元格：Border 描边（右/下 1px；表头底部 2px + 半透明背景）。
/// 最右列去掉右线、最后一行去掉底线，避免与外框双线。
/// 线色 150 灰（深/浅主题均可见；205 灰在浅色 Mica 上几乎隐形）。
fn table_cell(
    text: String,
    key: String,
    row: i32,
    col: i32,
    n_cols: i32,
    n_rows: i32,
    is_header: bool,
) -> Element {
    let line = windows_reactor::Color {
        a: 255,
        r: 150,
        g: 150,
        b: 150,
    };
    let (bottom, top) = if is_header {
        (2.0, 1.0)
    } else {
        (1.0, 0.0)
    };
    let mut tb = text_block(text).wrap().center_aligned();
    if is_header {
        tb = tb.semibold();
    }
    let mut cell = border(tb)
        .border_brush(line)
        .border_thickness(windows_reactor::Thickness {
            left: 0.0,
            top,
            right: if col + 1 < n_cols { 1.0 } else { 0.0 },
            bottom: if row + 1 < n_rows { bottom } else { 0.0 },
        });
    if is_header {
        // 半透明灰背景（表头区视觉区分）
        cell = cell.background(windows_reactor::Color {
            a: 52,
            r: 128,
            g: 128,
            b: 128,
        });
    }
    cell.grid_row(row)
        .grid_column(col)
        .with_key(key)
        .into()
}
