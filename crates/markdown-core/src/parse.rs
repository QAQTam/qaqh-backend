//! final 解析：pulldown-cmark（GFM）→ 自研 AST。
//!
//! 对应 Web 端 `marked(GFM)` + `cleanMarkedHTML` 的解析层。选型依据
//! （需求单 1.1 API 形状建议）：pulldown-cmark 是 Rust 生态事实标准，
//! GFM 表格 / 任务列表 / 围栏代码天然支持，且无 DOM 依赖（可进 worker /
//! 后台线程，性能契约 REFERENCE §8.2）。
//!
//! 语义对齐点：
//! - h4+ 标题降级为加粗段落（REFERENCE §3）
//! - 代码块语言别名归一（§4）
//! - 数学分隔符独立扫描（§6）：代码块 / 行内代码内 `$` 不误渲染
//! - 表格 / 任务列表走 GFM
//! - 原始 HTML 有限透传：按字面丢弃（安全面收紧，REFERENCE §10 缺口 1）

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

use crate::ast::{Block, Inline, ListItem};
use crate::code::normalize_lang;
use crate::math::{self, MathSpan};

/// GFM 全开（对应 marked 的 gfm: true 默认）。
fn options() -> Options {
    Options::ENABLE_TABLES
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
}

/// 全量解析一段 markdown 为块级 AST（final 渲染用）。
pub fn parse_final(input: &str) -> Vec<Block> {
    let mut frames: Vec<Frame> = Vec::new();
    let mut blocks: Vec<Block> = Vec::new();

    for event in Parser::new_ext(input, options()) {
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => frames.push(Frame::paragraph()),
                Tag::Heading { level, .. } => frames.push(Frame::heading(level as u8)),
                Tag::BlockQuote(_) => frames.push(Frame::quote()),
                Tag::List(start) => frames.push(Frame::list(start)),
                Tag::Item => frames.push(Frame::item()),
                Tag::CodeBlock(kind) => {
                    let lang = match kind {
                        CodeBlockKind::Fenced(info) => info
                            .trim()
                            .split_whitespace()
                            .next()
                            .map(normalize_lang),
                        CodeBlockKind::Indented => None,
                    };
                    frames.push(Frame::code(lang));
                }
                Tag::Table(_) => frames.push(Frame::table()),
                Tag::TableHead => table_cursor(&mut frames, Cursor::Head),
                Tag::TableRow => table_cursor(&mut frames, Cursor::Row),
                Tag::TableCell => table_cursor(&mut frames, Cursor::Cell),
                Tag::Emphasis => frames.push(Frame::inline(InlineKind::Emphasis)),
                Tag::Strong => frames.push(Frame::inline(InlineKind::Strong)),
                Tag::Strikethrough => frames.push(Frame::inline(InlineKind::Strikethrough)),
                Tag::Link {
                    dest_url, ..
                } => frames.push(Frame::inline(InlineKind::Link {
                    url: dest_url.to_string(),
                })),
                Tag::Image {
                    dest_url, ..
                } => frames.push(Frame::inline(InlineKind::Image {
                    url: dest_url.to_string(),
                    alt: String::new(), // alt 由 children 文本收集（End 时填充）
                })),
                _ => frames.push(Frame::ignore()),
            },
            Event::End(tag_end) => match tag_end {
                TagEnd::TableCell => {
                    // 表格单元格结束：把游标内容收进 headers / 当前行
                    if let Some(Node::TableCellCell(cell)) =
                        take_table_cell(&mut frames).map(Node::TableCellCell)
                    {
                        push_cell(cell, &mut frames);
                    }
                }
                TagEnd::TableRow => finalize_row(&mut frames),
                TagEnd::TableHead => {
                    // headers 已在各 Cell 结束时收集
                }
                _ => {
                    let frame = frames.pop().expect("tag stack underflow");
                    match frame.into_node() {
                        Node::Block(b) => push_block(b, &mut frames, &mut blocks),
                        Node::Inline(i) => push_inline(i, &mut frames),
                        Node::Item { task, blocks: item_blocks } => {
                            push_block(
                                Block::ListItem {
                                    task,
                                    blocks: item_blocks,
                                },
                                &mut frames,
                                &mut blocks,
                            )
                        }
                        Node::TableCellCell(_) | Node::None => {}
                    }
                }
            },
            Event::Text(t) => {
                let text = t.to_string();
                let spans = math::scan_math(&text);
                if spans.is_empty() {
                    push_inline(Inline::Text(text), &mut frames);
                } else {
                    for seg in math::split_by_math(&text, &spans) {
                        match seg {
                            math::Segment::Literal(s) => {
                                push_inline(Inline::Text(s.to_string()), &mut frames)
                            }
                            math::Segment::Math(span) => {
                                push_inline(math_inline(span), &mut frames)
                            }
                        }
                    }
                }
            }
            Event::Code(c) => push_inline(Inline::Code(c.to_string()), &mut frames),
            Event::SoftBreak | Event::HardBreak => {
                push_inline(Inline::SoftBreak, &mut frames)
            }
            Event::TaskListMarker(checked) => {
                if let Some(frame) = frames.iter_mut().rev().find(|f| f.is_item()) {
                    frame.task = Some(checked);
                }
            }
            Event::Rule => blocks.push(Block::Rule),
            // 原始 HTML / 脚注引用：字面丢弃（安全面收紧）
            Event::Html(_) | Event::FootnoteReference(_) => {}
            _ => {}
        }
    }

    while let Some(frame) = frames.pop() {
        push_block(frame.into_node().into_block_or_empty(), &mut frames, &mut blocks);
    }
    blocks
}

fn math_inline(span: &MathSpan) -> Inline {
    Inline::Math {
        source: span.source.clone(),
        display: span.display,
    }
}

// ---------------------------------------------------------------------------
// Frame 栈
// ---------------------------------------------------------------------------

/// 每个打开的标签对应一个 Frame；子节点统一收进 `children`，
/// 出栈时一次性构建节点——避免跨帧借用。
struct Frame {
    kind: FrameKind,
    children: Vec<Node>,
    /// Item 的任务标记（TaskListMarker 事件写入）
    task: Option<bool>,
}

enum FrameKind {
    Paragraph,
    Heading { level: u8 },
    Quote,
    List { ordered: bool, start: u64 },
    Item,
    Code { lang: Option<String>, text: String },
    Table(TableAcc),
    Inline(InlineKind),
    Ignore,
}

/// 表格累加器：游标式收集。
#[derive(Default)]
struct TableAcc {
    headers: Vec<Vec<Inline>>,
    rows: Vec<Vec<Vec<Inline>>>,
    current_cell: Option<Vec<Inline>>,
    current_row: Vec<Vec<Inline>>,
    in_head: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum Cursor {
    Head,
    Row,
    Cell,
}

fn table_cursor(frames: &mut [Frame], cursor: Cursor) {
    let Some(frame) = frames.iter_mut().rev().find(|f| f.is_table()) else {
        return;
    };
    let FrameKind::Table(acc) = &mut frame.kind else {
        return;
    };
    match cursor {
        Cursor::Head => {
            acc.in_head = true;
            acc.current_cell = Some(Vec::new());
        }
        Cursor::Row => {
            acc.in_head = false;
            acc.current_row = Vec::new();
            acc.current_cell = Some(Vec::new());
        }
        Cursor::Cell => {
            acc.current_cell = Some(Vec::new());
        }
    }
}

/// take 当前单元格内容（TableCell 结束时调用）。
fn take_table_cell(frames: &mut [Frame]) -> Option<Vec<Inline>> {
    let frame = frames.iter_mut().rev().find(|f| f.is_table())?;
    let FrameKind::Table(acc) = &mut frame.kind else {
        return None;
    };
    acc.current_cell.take()
}

fn push_cell(inlines: Vec<Inline>, frames: &mut [Frame]) {
    let Some(frame) = frames.iter_mut().rev().find(|f| f.is_table()) else {
        return;
    };
    let FrameKind::Table(acc) = &mut frame.kind else {
        return;
    };
    if acc.in_head {
        acc.headers.push(inlines);
    } else {
        acc.current_row.push(inlines);
    }
}

fn finalize_row(frames: &mut [Frame]) {
    let Some(frame) = frames.iter_mut().rev().find(|f| f.is_table()) else {
        return;
    };
    let FrameKind::Table(acc) = &mut frame.kind else {
        return;
    };
    if !acc.in_head && !acc.current_row.is_empty() {
        acc.rows.push(std::mem::take(&mut acc.current_row));
    }
}

impl Frame {
    fn paragraph() -> Self {
        Self {
            kind: FrameKind::Paragraph,
            children: Vec::new(),
            task: None,
        }
    }

    fn heading(level: u8) -> Self {
        Self {
            kind: FrameKind::Heading { level },
            children: Vec::new(),
            task: None,
        }
    }

    fn quote() -> Self {
        Self {
            kind: FrameKind::Quote,
            children: Vec::new(),
            task: None,
        }
    }

    fn list(start: Option<u64>) -> Self {
        Self {
            kind: FrameKind::List {
                ordered: start.is_some(),
                start: start.unwrap_or(1),
            },
            children: Vec::new(),
            task: None,
        }
    }

    fn item() -> Self {
        Self {
            kind: FrameKind::Item,
            children: Vec::new(),
            task: None,
        }
    }

    fn code(lang: Option<String>) -> Self {
        Self {
            kind: FrameKind::Code {
                lang,
                text: String::new(),
            },
            children: Vec::new(),
            task: None,
        }
    }

    fn table() -> Self {
        Self {
            kind: FrameKind::Table(TableAcc::default()),
            children: Vec::new(),
            task: None,
        }
    }

    fn inline(kind: InlineKind) -> Self {
        Self {
            kind: FrameKind::Inline(kind),
            children: Vec::new(),
            task: None,
        }
    }

    fn ignore() -> Self {
        Self {
            kind: FrameKind::Ignore,
            children: Vec::new(),
            task: None,
        }
    }

    fn is_table(&self) -> bool {
        matches!(self.kind, FrameKind::Table(_))
    }

    fn is_item(&self) -> bool {
        matches!(self.kind, FrameKind::Item)
    }

    fn into_node(self) -> Node {
        match self.kind {
            FrameKind::Paragraph => {
                Node::Block(Block::Paragraph(self.children.into_inlines()))
            }
            FrameKind::Heading { level } => {
                let inlines = self.children.into_inlines();
                if level >= 4 {
                    // h4+ 降级为加粗段落（REFERENCE §3 显式化）
                    Node::Block(Block::Paragraph(vec![Inline::Bold(inlines)]))
                } else {
                    Node::Block(Block::Heading { level, inlines })
                }
            }
            FrameKind::Quote => Node::Block(Block::Quote(self.children.into_blocks())),
            FrameKind::List {
                ordered,
                start,
            } => {
                let items = self
                    .children
                    .into_iter()
                    .filter_map(|n| match n {
                        Node::Block(Block::ListItem { task, blocks }) => {
                            Some(ListItem { task, blocks })
                        }
                        _ => None,
                    })
                    .collect();
                Node::Block(Block::List {
                    ordered,
                    start,
                    items,
                })
            }
            FrameKind::Item => Node::Item {
                task: self.task,
                blocks: self.children.into_blocks(),
            },
            FrameKind::Code { lang, text } => {
                // 协议化表格：```table 围栏 + JSON（见 docs/table-protocol.md）。
                // LLM 按 prompt 输出结构化表格；解析失败回退字面代码块
                // （内容永不丢失）。
                if lang.as_deref() == Some("table")
                    && let Some((headers, rows)) = parse_table_protocol(&text)
                {
                    Node::Block(Block::Table { headers, rows })
                } else {
                    Node::Block(Block::Code { lang, text })
                }
            }
            FrameKind::Table(acc) => Node::Block(Block::Table {
                headers: acc.headers,
                rows: acc.rows,
            }),
            FrameKind::Inline(kind) => {
                let inlines = self.children.into_inlines();
                Node::Inline(kind.wrap(inlines))
            }
            FrameKind::Ignore => Node::None,
        }
    }
}

/// 解析树节点（出栈产物）。
enum Node {
    Block(Block),
    Inline(Inline),
    /// 列表项（task 标记 + 子块），List 出栈时转 ListItem。
    Item { task: Option<bool>, blocks: Vec<Block> },
    /// 表格单元格内容（独立游标通道，不经 Node 灌入）。
    TableCellCell(Vec<Inline>),
    None,
}

impl Node {
    fn into_block_or_empty(self) -> Block {
        match self {
            Node::Block(b) => b,
            Node::Item { task, blocks } => Block::ListItem { task, blocks },
            _ => Block::Paragraph(Vec::new()),
        }
    }
}

trait IntoInlines {
    fn into_inlines(self) -> Vec<Inline>;
}

impl IntoInlines for Vec<Node> {
    fn into_inlines(self) -> Vec<Inline> {
        self.into_iter()
            .filter_map(|n| match n {
                Node::Inline(i) => Some(i),
                _ => None,
            })
            .collect()
    }
}

trait IntoBlocks {
    fn into_blocks(self) -> Vec<Block>;
}

impl IntoBlocks for Vec<Node> {
    fn into_blocks(self) -> Vec<Block> {
        self.into_iter()
            .filter_map(|n| match n {
                Node::Block(b) => Some(b),
                _ => None,
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// 节点灌入
// ---------------------------------------------------------------------------

fn push_block(block: Block, frames: &mut [Frame], blocks: &mut Vec<Block>) {
    match frames.last_mut() {
        None => blocks.push(block),
        Some(frame) => match &mut frame.kind {
            FrameKind::Quote | FrameKind::Item | FrameKind::List { .. } => {
                frame.children.push(Node::Block(block))
            }
            FrameKind::Paragraph | FrameKind::Heading { .. } => {
                // 防御：块进段落（pulldown 正常不会发生）→ 按纯文本并入
                frame.children.push(Node::Inline(Inline::Text(
                    crate::ast::block_plain_text(&block),
                )));
            }
            FrameKind::Code { text, .. } => {
                text.push_str(&crate::ast::block_plain_text(&block));
            }
            _ => blocks.push(block),
        },
    }
}

fn push_inline(inline: Inline, frames: &mut [Frame]) {
    let Some(frame) = frames.last_mut() else {
        return;
    };
    match &mut frame.kind {
        FrameKind::Paragraph | FrameKind::Heading { .. } => {
            frame.children.push(Node::Inline(inline));
        }
        FrameKind::Item => {
            // 紧凑列表项没有 Paragraph 容器：行内文本直接包 Paragraph 落块
            let has_open_paragraph =
                matches!(frame.children.last(), Some(Node::Block(Block::Paragraph(_))));
            if !has_open_paragraph {
                frame.children.push(Node::Block(Block::Paragraph(Vec::new())));
            }
            if let Some(Node::Block(Block::Paragraph(v))) = frame.children.last_mut() {
                v.push(inline);
            }
        }
        FrameKind::Code { text, .. } => text.push_str(&inline.plain_text()),
        FrameKind::Table(acc) => {
            if let Some(cell) = acc.current_cell.as_mut() {
                cell.push(inline);
            }
        }
        FrameKind::Inline(_) => frame.children.push(Node::Inline(inline)),
        FrameKind::Quote | FrameKind::List { .. } => {
            // 引用/列表里悬空的行内文本：包 Paragraph（防御）
            frame.children.push(Node::Block(Block::Paragraph(vec![inline])));
        }
        FrameKind::Ignore => {}
    }
}

// ---------------------------------------------------------------------------
// 协议化表格（```table 围栏 + JSON）
// ---------------------------------------------------------------------------

/// 表格协议载荷（docs/table-protocol.md）。
#[derive(serde::Deserialize)]
struct TableProtocol {
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
}

/// 解析表格协议 → (headers, rows)（单元格为纯文本 Inline）。
///
/// 两种格式（docs/table-protocol.md）：
/// 1. **TSV/分隔符**（推荐，LLM 最不易错）：首行表头，每行一记录，
///    优先 `\t` 分隔，行内无 `\t` 时用 `|`；
/// 2. **JSON**（兼容旧协议）：`{"headers":[...],"rows":[...]}`。
///
/// 校验：表头非空、每行列数与表头一致；全部失败返回 None（回退字面）。
fn parse_table_protocol(text: &str) -> Option<(Vec<Vec<Inline>>, Vec<Vec<Vec<Inline>>>)> {
    let trimmed = text.trim();
    // 1) JSON 兼容
    if let Ok(p) = serde_json::from_str::<TableProtocol>(trimmed)
        && !p.headers.is_empty()
        && p.rows.iter().all(|r| r.len() == p.headers.len())
    {
        return Some((
            p.headers
                .into_iter()
                .map(|s| vec![Inline::Text(s)])
                .collect(),
            p.rows
                .into_iter()
                .map(|r| r.into_iter().map(|s| vec![Inline::Text(s)]).collect())
                .collect(),
        ));
    }
    // 2) TSV / 分隔符
    parse_table_tsv(trimmed)
}

/// TSV 解析：首行表头；分隔符检测（`\t` 优先，否则 `|`）。
fn parse_table_tsv(text: &str) -> Option<(Vec<Vec<Inline>>, Vec<Vec<Vec<Inline>>>)> {
    let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let header = *lines.first()?;
    let sep = if header.contains('\t') { '\t' } else { '|' };
    if !header.contains(sep) {
        return None; // 无分隔符 → 不是表格
    }
    let split = |s: &str| -> Vec<String> {
        s.split(sep).map(str::trim).map(String::from).collect()
    };
    let headers = split(header);
    if headers.is_empty() {
        return None;
    }
    let mut rows = Vec::new();
    for line in lines.iter().skip(1) {
        let cells = split(line);
        if cells.len() != headers.len() {
            return None; // 列数不一致 → 拒绝（防错位）
        }
        rows.push(cells);
    }
    let headers = headers.into_iter().map(|s| vec![Inline::Text(s)]).collect();
    let rows = rows
        .into_iter()
        .map(|r| r.into_iter().map(|s| vec![Inline::Text(s)]).collect())
        .collect();
    Some((headers, rows))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 协议表格：```table JSON → Block::Table（渲染层走 Grid 表格通道）
    #[test]
    fn table_protocol_parses() {
        let md = "```table\n{\"headers\":[\"操作\",\"允许\"],\"rows\":[[\"读借用\",\"✅\"],[\"写借用\",\"✅\"]]}\n```";
        let blocks = parse_final(md);
        assert_eq!(blocks.len(), 1);
        let Block::Table { headers, rows } = &blocks[0] else {
            panic!("expect table block: {blocks:?}");
        };
        assert_eq!(headers.len(), 2);
        assert_eq!(rows.len(), 2);
        assert_eq!(crate::ast::concat_inlines(&rows[0][0]), "读借用");
    }

    /// 无效 JSON → 回退字面代码块（内容不丢失）
    #[test]
    fn table_protocol_invalid_falls_back_to_code() {
        let md = "```table\n{not json}\n```";
        let blocks = parse_final(md);
        assert_eq!(blocks.len(), 1);
        assert!(
            matches!(&blocks[0], Block::Code { lang: Some(l), text } if l == "table" && text.contains("not json")),
            "无效协议必须按字面代码块保留: {blocks:?}"
        );
    }

    /// 列数不一致 → 回退（防错位渲染）
    #[test]
    fn table_protocol_mismatched_columns_falls_back() {
        let md = "```table\n{\"headers\":[\"a\",\"b\"],\"rows\":[[\"1\"]]}\n```";
        let blocks = parse_final(md);
        assert!(matches!(&blocks[0], Block::Code { .. }));
    }

    /// TSV（制表符分隔）：首行表头 + 数据行
    #[test]
    fn table_tsv_tab_separated_parses() {
        let md = "```table\n借用方式\t数量\t可写\n&T\t多个\t否\n&mut T\t独占\t是\n```";
        let blocks = parse_final(md);
        let Block::Table { headers, rows } = &blocks[0] else {
            panic!("expect table block: {blocks:?}");
        };
        assert_eq!(headers.len(), 3);
        assert_eq!(rows.len(), 2);
        assert_eq!(crate::ast::concat_inlines(&rows[1][0]), "&mut T");
        assert_eq!(crate::ast::concat_inlines(&rows[1][2]), "是");
    }

    /// TSV（`|` 分隔）：行内无 `\t` 时回退管道分隔
    #[test]
    fn table_tsv_pipe_separated_parses() {
        let md = "```table\n名称 | 类型\nsize | usize\nname | String\n```";
        let blocks = parse_final(md);
        let Block::Table { headers, rows } = &blocks[0] else {
            panic!("expect table block");
        };
        assert_eq!(headers.len(), 2);
        assert_eq!(rows.len(), 2);
        assert_eq!(crate::ast::concat_inlines(&rows[0][1]), "usize");
    }

    /// TSV 列数不一致 → 回退代码块
    #[test]
    fn table_tsv_mismatched_columns_falls_back() {
        let md = "```table\na\tb\n1\n```";
        let blocks = parse_final(md);
        assert!(matches!(&blocks[0], Block::Code { .. }), "{blocks:?}");
    }

    /// 无分隔符的普通文本 → 不是表格，回退代码块
    #[test]
    fn table_tsv_no_separator_falls_back() {
        let md = "```table\nplain text only\n```";
        let blocks = parse_final(md);
        assert!(matches!(&blocks[0], Block::Code { .. }));
    }
}

enum InlineKind {
    Emphasis,
    Strong,
    Strikethrough,
    Link { url: String },
    Image { url: String, alt: String },
}

impl InlineKind {
    fn wrap(self, inlines: Vec<Inline>) -> Inline {
        match self {
            Self::Emphasis => Inline::Italic(inlines),
            Self::Strong => Inline::Bold(inlines),
            Self::Strikethrough => Inline::Strikethrough(inlines),
            Self::Link { url } => Inline::Link { text: inlines, url },
            Self::Image { url, alt } => {
                // alt 取 children 纯文本；空则回退 URL（marked 行为）
                let alt = if alt.is_empty() {
                    let text = crate::ast::concat_inlines(&inlines);
                    if text.is_empty() { url.clone() } else { text }
                } else {
                    alt
                };
                Inline::Image { url, alt }
            }
        }
    }
}
