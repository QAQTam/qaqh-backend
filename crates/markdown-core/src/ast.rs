//! 渲染中间 AST —— 与 Web 端 `marked` 产出语义对齐，但剥离 HTML。
//!
//! 设计原则（对应 REFERENCE §3 关键语义）：
//! - AST 是**最终产物**（final）与**流式预览**（live）的公共中间表示：
//!   live 只产出 `Inline` 级别的子集（已闭合语法），final 产出完整 `Block` 树。
//! - 未闭合语法在 live 阶段按**字面文本**输出（`Inline::Text`），不产生破损结构。
//! - 代码块 / 图表等块级内容**等 final**：live 阶段不会出现 `Block::Code`。

/// 块级节点（final 渲染产物）。
#[derive(Clone, Debug, PartialEq)]
pub enum Block {
    /// 段落（含行内节点）。
    Paragraph(Vec<Inline>),
    /// 标题。`level` 1..=3；h4+ 在解析层降级为加粗段落（对齐 Web 行为）。
    Heading { level: u8, inlines: Vec<Inline> },
    /// 有序 / 无序列表。`start` 仅有序列表有意义。
    List {
        ordered: bool,
        start: u64,
        items: Vec<ListItem>,
    },
    /// 列表项（内部表示，仅在 List 内出现；解析层转 ListItem 收集）。
    ListItem {
        task: Option<bool>,
        blocks: Vec<Block>,
    },
    /// 引用块（可嵌套）。
    Quote(Vec<Block>),
    /// GFM 表格。
    Table {
        headers: Vec<Vec<Inline>>,
        rows: Vec<Vec<Vec<Inline>>>,
    },
    /// 围栏 / 缩进代码块。`lang` 已做别名归一；`None` = 未标注语言。
    Code { lang: Option<String>, text: String },
    /// 分隔线 `---`。
    Rule,
}

/// 列表项：`- [ ]` 任务列表在 `checked` 上区分。
#[derive(Clone, Debug, PartialEq)]
pub struct ListItem {
    pub task: Option<bool>,
    pub blocks: Vec<Block>,
}

/// 行内节点。
///
/// live 阶段只产出 `Text` / `Bold` / `Italic` / `Strikethrough` / `Code` /
/// `Link` / `Math` 中**已闭合**的部分；未闭合一律折叠为 `Text`。
#[derive(Clone, Debug, PartialEq)]
pub enum Inline {
    Text(String),
    Bold(Vec<Inline>),
    Italic(Vec<Inline>),
    Strikethrough(Vec<Inline>),
    /// 行内代码（`` `code` ``）。
    Code(String),
    /// 链接。`url` 已做实体解码。
    Link {
        text: Vec<Inline>,
        url: String,
    },
    /// 图片（远程 URL，最终渲染；live 阶段不产出）。
    Image {
        url: String,
        alt: String,
    },
    /// 数学公式。`display=true` 对应 `$$..$$` / `\[..\]`。
    /// 渲染失败时由上层回退为字面文本（`throwOnError: false` 语义）。
    Math {
        source: String,
        display: bool,
    },
    /// 软换行（markdown 单换行，`breaks: false` 时不产生 `<br>`）。
    SoftBreak,
}

impl Inline {
    /// 以纯文本形式取出行内内容的拼接（复制按钮 / 回退路径用）。
    pub fn plain_text(&self) -> String {
        match self {
            Self::Text(s) => s.clone(),
            Self::Code(s) => s.clone(),
            Self::Math { source, .. } => source.clone(),
            Self::Bold(v) | Self::Italic(v) | Self::Strikethrough(v) => concat_inlines(v),
            Self::Link { text, .. } => concat_inlines(text),
            Self::Image { alt, .. } => alt.clone(),
            Self::SoftBreak => "\n".to_string(),
        }
    }
}

/// 拼接一组行内节点为纯文本。
pub fn concat_inlines(inlines: &[Inline]) -> String {
    inlines.iter().map(Inline::plain_text).collect()
}

/// 块级内容转纯文本（复制 / 无障碍 / 降级阶梯保底路径）。
pub fn block_plain_text(block: &Block) -> String {
    match block {
        Block::Paragraph(v) | Block::Heading { inlines: v, .. } => concat_inlines(v),
        Block::List { items, .. } => items
            .iter()
            .map(|item| {
                let prefix = match item.task {
                    Some(true) => "- [x] ",
                    Some(false) => "- [ ] ",
                    None => "- ",
                };
                let mut out = String::from(prefix);
                for b in &item.blocks {
                    out.push_str(&block_plain_text(b));
                    out.push('\n');
                }
                out
            })
            .collect(),
        Block::ListItem { blocks, .. } => blocks
            .iter()
            .map(block_plain_text)
            .collect::<Vec<_>>()
            .join("\n"),
        Block::Quote(blocks) => blocks
            .iter()
            .map(block_plain_text)
            .collect::<Vec<_>>()
            .join("\n"),
        Block::Table { headers, rows } => {
            let mut out = String::new();
            out.push_str(&concat_inlines(&headers.concat()));
            for row in rows {
                for cell in row {
                    out.push_str(&concat_inlines(cell));
                    out.push('|');
                }
                out.push('\n');
            }
            out
        }
        Block::Code { text, .. } => text.clone(),
        Block::Rule => "---".to_string(),
    }
}
