use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::OnceLock;

use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;
use windows_reactor::{
    Color, ColorScheme, RichTextBlock, RichTextInline, RichTextRun, TextWrapping,
};

use crate::CodeBlock;

static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();
static THEMES: OnceLock<ThemeSet> = OnceLock::new();

/// 高亮产物缓存上限。视口内行 rebuild 时 `highlighted_code_block` 会被反复
/// 调用（内容未变），缓存避免每次重新 syntect lexing——大代码块单次 lex
/// 可达数十毫秒，且发生在 UI 线程，是"大量 markdown/代码块卡顿"的主热点。
const HIGHLIGHT_CACHE_CAPACITY: usize = 64;

#[derive(Clone, Hash, PartialEq, Eq)]
struct HighlightKey {
    lang: String,
    text: String,
    dark: bool,
    font_family: String,
}

struct CacheEntry {
    stamp: u64,
    block: Rc<RichTextBlock>,
}

type HighlightCache = RefCell<HashMap<HighlightKey, CacheEntry>>;

thread_local! {
    /// 高亮产物缓存。渲染闭包在 UI 线程调用 `highlighted_code_block`，
    /// thread_local 免锁且保留 `Rc<RichTextBlock>` 零拷贝命中；
    /// 后台线程将来调用时仅缓存不共享（无害的 miss）。
    static HIGHLIGHT_CACHE: HighlightCache = RefCell::new(HashMap::new());
}

fn with_cache<T>(f: impl FnOnce(&mut HashMap<HighlightKey, CacheEntry>) -> T) -> T {
    HIGHLIGHT_CACHE.with(|cache| f(&mut cache.borrow_mut()))
}

fn next_stamp() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static STAMP: AtomicU64 = AtomicU64::new(1);
    STAMP.fetch_add(1, Ordering::Relaxed)
}

fn cache_get(key: &HighlightKey) -> Option<RichTextBlock> {
    with_cache(|cache| {
        let entry = cache.get_mut(key)?;
        entry.stamp = next_stamp();
        Some((*entry.block).clone())
    })
}

fn cache_put(key: HighlightKey, block: RichTextBlock) {
    with_cache(|cache| {
        if cache.len() >= HIGHLIGHT_CACHE_CAPACITY {
            // 驱逐最久未命中项（stamp 最小）。显式循环结束迭代借用。
            let mut best: Option<(u64, HighlightKey)> = None;
            for (k, e) in cache.iter() {
                match &best {
                    Some((stamp, _)) if *stamp <= e.stamp => {}
                    _ => best = Some((e.stamp, k.clone())),
                }
            }
            if let Some((_, evict)) = best {
                cache.remove(&evict);
            }
        }
        cache.insert(
            key,
            CacheEntry {
                stamp: next_stamp(),
                block: Rc::new(block),
            },
        );
    })
}

/// Convert a fenced code block into native RichText runs colored by syntect.
/// Unknown language tags intentionally remain plain text.
pub fn highlighted_code_block(
    code: &CodeBlock,
    scheme: ColorScheme,
    font_family: &str,
) -> RichTextBlock {
    let key = HighlightKey {
        lang: code.lang.clone().unwrap_or_default(),
        text: code.text.clone(),
        dark: scheme == ColorScheme::Dark,
        font_family: font_family.to_string(),
    };
    if let Some(cached) = cache_get(&key) {
        return cached;
    }

    let syntaxes = SYNTAXES.get_or_init(SyntaxSet::load_defaults_newlines);
    let themes = THEMES.get_or_init(ThemeSet::load_defaults);
    let syntax = code
        .lang
        .as_deref()
        .and_then(|lang| find_syntax(syntaxes, lang));

    let inlines = syntax
        .and_then(|syntax| {
            highlight(
                &code.text,
                syntaxes,
                syntax,
                theme(themes, scheme),
                font_family,
            )
        })
        .unwrap_or_else(|| vec![plain_run(&code.text, font_family)]);

    let mut block = RichTextBlock::single_paragraph(inlines);
    block.font_size = Some(13.0);
    block.line_height = Some(20.0);
    // Run 级已带 font_family；block 级兜底（RichTextBlock 不参与全局继承）。
    block.modifiers.font_family = Some(font_family.to_string());
    block.text_wrapping = TextWrapping::NoWrap;
    block.is_text_selection_enabled = true;
    cache_put(key, block.clone());
    block
}

fn find_syntax<'a>(syntaxes: &'a SyntaxSet, language: &str) -> Option<&'a SyntaxReference> {
    let token = language.trim();
    syntaxes
        .find_syntax_by_token(token)
        .or_else(|| syntaxes.find_syntax_by_extension(token))
        .or_else(|| {
            syntaxes
                .syntaxes()
                .iter()
                .find(|syntax| syntax.name.eq_ignore_ascii_case(token))
        })
}

fn theme(themes: &ThemeSet, scheme: ColorScheme) -> &Theme {
    let preferred = match scheme {
        ColorScheme::Light => "InspiredGitHub",
        ColorScheme::Dark => "base16-ocean.dark",
    };
    themes
        .themes
        .get(preferred)
        .or_else(|| themes.themes.values().next())
        .expect("syntect default-themes must contain at least one theme")
}

fn highlight(
    text: &str,
    syntaxes: &SyntaxSet,
    syntax: &SyntaxReference,
    theme: &Theme,
    font_family: &str,
) -> Option<Vec<RichTextInline>> {
    let mut highlighter = HighlightLines::new(syntax, theme);
    let mut inlines = Vec::new();
    for line in LinesWithEndings::from(text) {
        let ranges = highlighter.highlight_line(line, syntaxes).ok()?;
        for (style, token) in ranges {
            let mut run = RichTextRun::plain(token);
            run.foreground = Some(Color {
                a: style.foreground.a,
                r: style.foreground.r,
                g: style.foreground.g,
                b: style.foreground.b,
            });
            run.is_bold = style.font_style.contains(FontStyle::BOLD);
            run.is_italic = style.font_style.contains(FontStyle::ITALIC);
            run.font_family = Some(font_family.to_string());
            inlines.push(RichTextInline::Run(run));
        }
    }
    if inlines.is_empty() {
        inlines.push(plain_run("", font_family));
    }
    Some(inlines)
}

fn plain_run(text: &str, font_family: &str) -> RichTextInline {
    let mut run = RichTextRun::plain(text);
    run.font_family = Some(font_family.to_string());
    RichTextInline::Run(run)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(block: &RichTextBlock) -> String {
        block.paragraphs[0]
            .inlines
            .iter()
            .filter_map(|inline| match inline {
                RichTextInline::Run(run) => Some(run.text.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn known_language_produces_colored_runs_without_losing_text() {
        let code = CodeBlock {
            lang: Some("rust".into()),
            text: "fn main() {\n    println!(\"hi\");\n}\n".into(),
        };
        let block = highlighted_code_block(&code, ColorScheme::Dark, "Cascadia Mono");
        assert_eq!(text(&block), code.text);
        assert!(block.paragraphs[0].inlines.len() > 1);
        assert!(block.paragraphs[0].inlines.iter().any(|inline| {
            matches!(inline, RichTextInline::Run(run) if run.foreground.is_some())
        }));
    }

    #[test]
    fn unknown_language_is_plain_and_preserves_text() {
        let code = CodeBlock {
            lang: Some("deepx-unknown-language".into()),
            text: "alpha < beta\n".into(),
        };
        let block = highlighted_code_block(&code, ColorScheme::Light, "Consolas");
        assert_eq!(text(&block), code.text);
        assert_eq!(block.paragraphs[0].inlines.len(), 1);
        assert!(matches!(
            &block.paragraphs[0].inlines[0],
            RichTextInline::Run(run) if run.foreground.is_none()
        ));
    }
}
