//! # markdown-core —— ChatView markdown 渲染核心原型
//!
//! 对齐 `CHATVIEW-RENDERING-REFERENCE.md` §3-§6 的纯 Rust 渲染核心：
//! 不依赖 WinUI / DOM，可进 worker / 后台线程（性能契约 §8）。
//!
//! 模块地图：
//! - [`ast`]：渲染中间 AST（final 产物 + live 预览的公共表示）
//! - [`parse`]：final 解析（pulldown-cmark GFM → AST）
//! - [`live`]：流式行内解析（未闭合语法字面输出）
//! - [`stream`]：增量追加 + 块哈希缓存 + 渲染计划（O(1) 追加）
//! - [`math`]：katex 分隔符扫描（代码内 `$` 不误渲染、失败回退字面）
//! - [`code`]：代码块 27 语言表 + 别名归一
//! - [`live_table`]：协议表格（```table）流式渐进跟踪（P0）

pub mod ast;
pub mod code;
pub mod live;
pub mod live_table;
pub mod math;
pub mod parse;
pub mod stream;

pub use ast::{Block, Inline, ListItem};
pub use code::{LANGUAGES, is_supported, normalize_lang};
pub use live::parse_live;
pub use math::{MathSpan, scan_math};
pub use parse::parse_final;
pub use stream::{RenderOp, RenderPlan, StreamingMarkdown};
