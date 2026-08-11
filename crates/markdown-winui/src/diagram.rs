use std::sync::{Arc, OnceLock};

use mermaid_rs_renderer::{RenderOptions, Theme, render_with_options};
use windows_reactor::{
    AccessibilityExt, BackgroundExt, Callback, ColorScheme, Element, HorizontalAlignment, Image,
    ImageSource, InputExt, KeyExt, LayoutExt, PaddingExt, ScrollBarVisibility, Stretch, ThemeRef,
    Thickness, TextStyleExt, VerticalAlignment, border, scroll_viewer, text_block, vstack,
};

/// 全局字体库：进程内加载系统字体一次（含中文字体），供 usvg 文字轮廓化使用。
///
/// usvg 0.47 的 `Options.fontdb` 是 `Arc<Database>`，可安全共享克隆。
fn system_fontdb() -> &'static Arc<usvg::fontdb::Database> {
    static DB: OnceLock<Arc<usvg::fontdb::Database>> = OnceLock::new();
    DB.get_or_init(|| {
        let mut db = usvg::fontdb::Database::new();
        db.load_system_fonts();
        Arc::new(db)
    })
}

/// 把 SVG 里的 `<text>`/`<tspan>` 轮廓化为 `<path>`。
///
/// WinUI `SvgImageSource` 底层是 Direct2D SVG，**不支持 text 元素**（官方
/// 支持列表无 text/tspan），遇到会静默忽略——表现为「骨架可见、文字不可见」。
/// usvg 写出时默认 `preserve_text = false`，自动完成轮廓化；字体由
/// [`system_fontdb`] 提供（中英文均可）。转换失败回退原始 SVG（保持旧行为）。
fn text_to_path(svg: &str) -> Result<String, String> {
    let opt = usvg::Options {
        fontdb: system_fontdb().clone(),
        ..Default::default()
    };
    let tree = usvg::Tree::from_str(svg, &opt).map_err(|e| e.to_string())?;
    Ok(tree.to_string(&usvg::WriteOptions {
        // 默认 8 位精度对文字 path 过大；3 位足够且体积小一个量级。
        coordinates_precision: 3,
        ..Default::default()
    }))
}

/// 删除第一个 `<rect .../>`（mermaid 输出的全幅背景矩形）。
///
/// WinUI `SvgImageSource` 渲染时 SVG 无背景即透明，图表可融入卡片主题背景；
/// 部分图类型（sequence）的背景 rect 带负偏移，故按「第一个 rect」匹配。
fn strip_background_rect(svg: &str) -> String {
    if let Some(start) = svg.find("<rect") {
        if let Some(end_rel) = svg[start..].find("/>") {
            let end = start + end_rel + 2;
            return format!("{}{}", &svg[..start], &svg[end..]);
        }
    }
    svg.to_string()
}

/// 渲染 + text→path。
///
/// `Ok(None)` = mermaid 渲染成功但 text→path 失败（回退原始 SVG）；
/// `Err` = mermaid 渲染失败（携带原始错误，供兜底 UI 展示）。
fn render_to_svg(source: &str, theme: Theme) -> Result<Option<String>, String> {
    match render_with_options(
        source,
        RenderOptions {
            theme,
            ..RenderOptions::default()
        },
    ) {
        Ok(svg) => {
            let stripped = strip_background_rect(&svg);
            Ok(match text_to_path(&stripped) {
                Ok(converted) => Some(converted),
                Err(_) => Some(stripped),
            })
        }
        Err(e) => Err(e.to_string()),
    }
}

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
        let light = render_to_svg(&source, Theme::modern());
        let dark = render_to_svg(&source, Theme::dark());
        let error = light
            .as_ref()
            .err()
            .or_else(|| dark.as_ref().err())
            .cloned();
        Self {
            source,
            light_svg: light.ok().flatten(),
            dark_svg: dark.ok().flatten(),
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
///
/// `on_open`：点击图表卡片时触发（壳侧用于打开放大查看覆盖层）。
pub fn diagram_view(
    diagram: &DiagramBlock,
    scheme: ColorScheme,
    key: &str,
    on_open: Option<Callback<()>>,
) -> Element {
    if let Some(svg) = diagram.svg(scheme) {
        let image: Element = Image::new(ImageSource::svg(svg))
            .stretch(Stretch::Uniform)
            .min_height(120.0)
            // 缩略图高度上限：ChatView 内图表按缩略展示，点击放大看细节。
            // 纵向长图（TD）被限制后按比例缩小居中，不再撑爆卡片。
            .max_height(360.0)
            .max_width(640.0)
            .horizontal_alignment(HorizontalAlignment::Center)
            .vertical_alignment(VerticalAlignment::Center)
            .automation_name("Mermaid 图表")
            .into();
        let mut card = border(image)
            .background(ThemeRef::CardBackground)
            .border_brush(ThemeRef::CardStroke)
            .border_thickness(Thickness::uniform(1.0))
            .corner_radius(8.0)
            .padding(12.0)
            .with_key(key);
        if let Some(cb) = on_open {
            card = card.on_tapped(cb);
        }
        return card.into();
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
        // text→path 后：SVG 有效且不再含 <text>（SvgImageSource 不支持 text）
        assert!(
            block
                .svg(ColorScheme::Light)
                .is_some_and(|svg| svg.contains("<svg") && !svg.contains("<text"))
        );
        assert!(
            block
                .svg(ColorScheme::Dark)
                .is_some_and(|svg| svg.contains("<svg") && !svg.contains("<text"))
        );
    }

    #[test]
    fn chinese_labels_are_outlined() {
        // 中文字体走 fontdb 系统字体加载；断言转换后无 text 残留。
        let block =
            DiagramBlock::render("flowchart LR; A[开始] --> B[完成]");
        assert!(block.error.is_none(), "{:?}", block.error);
        assert!(
            block
                .svg(ColorScheme::Light)
                .is_some_and(|svg| !svg.contains("<text"))
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
