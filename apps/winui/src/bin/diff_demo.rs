//! 双栏 diff 视觉验证演示（V4）：独立窗口渲染「编辑工具胶囊行 + 双栏 diff」。
//!
//! 运行：`cargo run -p deepx-winui --bin diff_demo`
//!
//! 验证对象：
//! - 双栏 diff 行配对（删除左红 / 添加右绿 / Context 双栏 / Hunk 跨栏）
//! - fd2 半透明系统画刷 + 主题感知文字色（浅/深主题）
//! - Cascadia Mono 统一等宽字体
//! 数据走真实代码路径：`parse_unified_diff` + `tool_body_view`（markdown-winui）。

use markdown_winui::{ToolBody, parse_unified_diff, tool_body_view};
use windows_reactor::*;

/// 演示数据：把 file_edit.rs 的字符串定位改成块定位（呼应项目真实改动）。
const DEMO_DIFF: &str = "\
diff --git a/src/file_edit.rs b/src/file_edit.rs
--- a/src/file_edit.rs
+++ b/src/file_edit.rs
@@ -12,7 +12,9 @@
     pub fn replace_in_line(&mut self, old: &str, new: &str) -> bool {
         let old_start = self.find_old(old).ok_or(Error::NotFound)?;
-        let old_end = old_start + old.len();
+        let old_end = old_start + old.len();
+        let locator = self.block_locator_at(old_start);
+        let span = locator.resolve(self)?;
-        self.buf.replace_range(old_start..old_end, new);
+        self.apply_block_edit(span, new)?;
         true
     }
@@ -45,5 +47,8 @@
-    fn old_start(&self) -> usize {
-        self.cursor
+    fn old_start(&self) -> usize {
+        self.cursor
+    }
+    fn block_locator_at(&self, pos: usize) -> BlockLocator {
+        BlockLocator::from_cursor(self, pos)
     }
+
+    fn apply_block_edit(&mut self, span: LineSpan, new: &str) -> Result<()> {
+        self.lines[span.0..span.1].replace_with(new)
+    }
";

fn demo_panel() -> Element {
    // 1) 胶囊行（复刻 tool_row 视觉：✓ + 动作短语 + 摘要 + ±N + 耗时）
    let pill = border(
        hstack((
            text_block("✓")
                .font_size(12.0)
                .foreground(ThemeRef::SystemSuccess),
            text_block("修改文件 · src/file_edit.rs")
                .font_size(12.0)
                .foreground(ThemeRef::PrimaryText),
            text_block("+8  −5")
                .font_size(12.0)
                .foreground(ThemeRef::SystemSuccess),
            text_block("· 1.8s")
                .font_size(12.0)
                .foreground(ThemeRef::SecondaryText),
        ))
        .spacing(8.0)
        .padding(Thickness::xy(8.0, 5.0)),
    )
    .background(ThemeRef::LayerFill)
    .border_brush(ThemeRef::CardStroke)
    .border_thickness(Thickness::uniform(1.0))
    .corner_radius(6.0);

    // 2) 双栏 diff（真实代码路径：parse_unified_diff + tool_body_view）
    let document = parse_unified_diff(DEMO_DIFF);
    let diff = tool_body_view(
        &ToolBody::Diff(document),
        ColorScheme::Light,
        "ms-appx:///Assets/fonts/CascadiaCode.ttf#Cascadia Code",
        "diff-demo",
    );

    // 3) 说明
    let note = text_block(
        "单列 unified diff 验证（Light 主题）：行号双列（灰底右对齐，删除=旧行号/\
         添加=新行号/Context 双号），marker 列 +/−，删除行红底红字、添加行绿底绿字，\
         Hunk 跨行灰条，文本 NoWrap + 横向滚动，字体 Cascadia Mono，行间 0 间距。",
    )
    .font_size(12.0)
    .foreground(ThemeRef::SecondaryText)
    .wrap();

    border(
        vstack((pill, diff, note))
            .spacing(12.0)
            .padding(Thickness::uniform(16.0)),
    )
    .background(ThemeRef::SolidBackground)
    .padding(Thickness::uniform(24.0))
    .horizontal_alignment(HorizontalAlignment::Center)
    .vertical_alignment(VerticalAlignment::Center)
    .into()
}

fn render_app(_cx: &mut RenderCx) -> Element {
    demo_panel()
}

fn main() -> windows_reactor::Result<()> {
    App::new()
        .title("DeepX — 双栏 diff 演示")
        .inner_size(1000.0, 780.0)
        .render(render_app)
}
