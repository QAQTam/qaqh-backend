use mermaid_rs_renderer::{RenderOptions, Theme, render_with_options};
use windows_reactor::{
    ColorScheme, Element, ElementExt, HorizontalAlignment, Image, ImageSource, ScrollBarVisibility,
    Stretch, ThemeRef, Thickness, VerticalAlignment, border, scroll_viewer, text_block, vstack,
};

/// A Mermaid source plus native-SVG renderings for both application themes.
#[derive(Clone, Debug, PartialEq)]
pub struct DiagramBlock {
    pub source: String,
    pub light_svg: Option<String>,
    pub dark_svg: Option<String>,
    pub error: Option<String>,
}

impl DiagramBlock {
    pub fn render(source: impl Into<String>) -> Self {
        let source = source.into();
        let light = render_with_options(
            &source,
            RenderOptions {
                theme: Theme::modern(),
                ..RenderOptions::default()
            },
        );
        let dark = render_with_options(
            &source,
            RenderOptions {
                theme: Theme::dark(),
                ..RenderOptions::default()
            },
        );
        let error = light
            .as_ref()
            .err()
            .or_else(|| dark.as_ref().err())
            .map(ToString::to_string);
        Self {
            source,
            light_svg: light.ok(),
            dark_svg: dark.ok(),
            error,
        }
    }

    pub fn svg(&self, scheme: ColorScheme) -> Option<&str> {
        match scheme {
            ColorScheme::Light => self.light_svg.as_deref(),
            ColorScheme::Dark => self.dark_svg.as_deref(),
        }
    }
}

/// Render generated SVG through WinUI's static native SVG decoder.
/// There is no HTML, JavaScript, browser host, or WebView in this path.
pub fn diagram_view(diagram: &DiagramBlock, scheme: ColorScheme, key: &str) -> Element {
    if let Some(svg) = diagram.svg(scheme) {
        let image: Element = Image::new(ImageSource::svg(svg))
            .stretch(Stretch::Uniform)
            .min_height(120.0)
            .max_height(640.0)
            .horizontal_alignment(HorizontalAlignment::Stretch)
            .vertical_alignment(VerticalAlignment::Center)
            .automation_name("Mermaid 图表")
            .into();
        return border(image)
            .background(ThemeRef::CardBackground)
            .border_brush(ThemeRef::CardStroke)
            .border_thickness(Thickness::uniform(1.0))
            .corner_radius(8.0)
            .padding(12.0)
            .with_key(key)
            .into();
    }

    let detail = diagram.error.as_deref().unwrap_or("无法生成图表");
    border(
        vstack((
            text_block(format!("Mermaid 渲染失败：{detail}"))
                .foreground(ThemeRef::SystemCritical)
                .wrap(),
            scroll_viewer(
                text_block(&diagram.source)
                    .font_family("Cascadia Mono, Consolas")
                    .font_size(13.0)
                    .selectable(),
            )
            .horizontal_scroll_bar_visibility(ScrollBarVisibility::Auto)
            .vertical_scroll_bar_visibility(ScrollBarVisibility::Disabled),
        ))
        .spacing(8.0),
    )
    .background(ThemeRef::CardBackground)
    .border_brush(ThemeRef::CardStroke)
    .border_thickness(Thickness::uniform(1.0))
    .corner_radius(8.0)
    .padding(12.0)
    .automation_name("Mermaid 图表渲染失败")
    .with_key(key)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_light_and_dark_svg() {
        let block = DiagramBlock::render("flowchart LR; A[Start] --> B[Done]");
        assert!(block.error.is_none(), "{:?}", block.error);
        assert!(
            block
                .svg(ColorScheme::Light)
                .is_some_and(|svg| svg.contains("<svg"))
        );
        assert!(
            block
                .svg(ColorScheme::Dark)
                .is_some_and(|svg| svg.contains("<svg"))
        );
    }

    #[test]
    fn invalid_source_keeps_original_for_fallback() {
        let source = "this is not a mermaid diagram";
        let block = DiagramBlock::render(source);
        assert_eq!(block.source, source);
        assert!(block.error.is_some());
        assert!(block.light_svg.is_none());
        assert!(block.dark_svg.is_none());
    }
}
