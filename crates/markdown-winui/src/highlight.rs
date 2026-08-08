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

/// Convert a fenced code block into native RichText runs colored by syntect.
/// Unknown language tags intentionally remain plain text.
pub fn highlighted_code_block(
    code: &CodeBlock,
    scheme: ColorScheme,
    font_family: &str,
) -> RichTextBlock {
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
    block.text_wrapping = TextWrapping::NoWrap;
    block.is_text_selection_enabled = true;
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
