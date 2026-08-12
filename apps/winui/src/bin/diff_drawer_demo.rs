//! Diff 弹层演示（V7）：模拟 turn 视图 + 总结行 → 全屏覆盖层面板。
//!
//! 运行：`cargo run -p deepx-winui --bin diff_drawer_demo`
//!
//! ⚠ 包为纯 bin crate（无 lib.rs），bin 之间无法共享模块；本 demo 内联
//! `diff_drawer.rs` 的 V7 渲染逻辑（数据解析仍走共享的 markdown-winui）。
//! 与真实 overlay 同构：静态槽语义简化为 open state + 覆盖层 grid。
//!
//! 验证对象：
//! - 总结行点击 → 覆盖层面板弹出（70% 尺寸真实生效，无 ContentDialog 模板钳制）
//! - 弹层 = 主窗口 70%（保持纵横比），内容：文件列表 + 单列 diff + 底部合计
//! - 关闭：✕ / Esc（KeyboardAccelerator）

use markdown_winui::{parse_unified_diff, diff_file_view};
use windows_reactor::*;

/// 演示数据：file_edit.rs 两个 hunk（同文件多 op → 合并验证）。
const DEMO_DIFF_A: &str = "\
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

/// 演示数据：chat_adapter.rs 单 op。
const DEMO_DIFF_B: &str = "\
diff --git a/src/chat_adapter.rs b/src/chat_adapter.rs
--- a/src/chat_adapter.rs
+++ b/src/chat_adapter.rs
@@ -3,8 +3,6 @@
-use deepx_domain::RoundDelta;
-use deepx_domain::ToolCallPrepared;
 fn render_event(_event: &Event) -> Option<Element> {
-    match _event.kind {
-        RoundDelta => render_round(_event),
-        ToolCallPrepared => render_tool(_event),
-        _ => None,
-    }
+    None
 }
";

/// 与真实 overlay 同构的常量。
const DRAWER_RATIO: f64 = 0.70;
const DRAWER_W_MIN: f64 = 640.0;
const DRAWER_W_MAX: f64 = 1280.0;
const DRAWER_H_MIN: f64 = 480.0;
const DRAWER_H_MAX: f64 = 920.0;
const FILE_LIST_WIDTH: f64 = 280.0;

fn demo_files() -> Vec<(String, usize, usize, bool, markdown_winui::DiffFile)> {
    let mut out = Vec::new();
    for text in [DEMO_DIFF_A, DEMO_DIFF_B] {
        let doc = parse_unified_diff(text);
        for f in &doc.files {
            out.push((
                f.display_path().to_string(),
                f.lines_added,
                f.lines_removed,
                false,
                f.clone(),
            ));
        }
    }
    out
}

fn render_app(cx: &mut RenderCx) -> Element {
    let (open, set_open) = cx.use_state::<bool>(false);
    let (selected, set_selected) = cx.use_state::<usize>(0);
    let (dark, set_dark) = cx.use_state::<bool>(false);
    let win = cx.use_inner_size();
    let files = demo_files();

    let content = vstack((
        // 明暗主题切换（验证 ThemeRef 跟随）
        hstack((
            text_block("主题：")
                .font_size(11.0)
                .foreground(ThemeRef::SecondaryText),
            button(if dark { "☀️ 亮色" } else { "🌙 暗色" })
                .on_click({
                    let set_dark = set_dark.clone();
                    move || {
                        let next = !dark;
                        set_dark.call(next);
                        set_requested_theme(if next {
                            RequestedTheme::Dark
                        } else {
                            RequestedTheme::Light
                        });
                    }
                }),
        ))
        .spacing(8.0),
        // 模拟 turn 视图：工具胶囊行
        border(
            hstack((
                text_block("✓").font_size(12.0).foreground(ThemeRef::SystemSuccess),
                text_block("修改文件 · src/file_edit.rs").font_size(12.0),
                text_block("+8  −5").font_size(12.0).foreground(ThemeRef::SystemSuccess),
            ))
            .spacing(8.0)
            .padding(Thickness::xy(8.0, 5.0)),
        )
        .background(ThemeRef::LayerFill)
        .border_brush(ThemeRef::CardStroke)
        .border_thickness(Thickness::uniform(1.0))
        .corner_radius(6.0),
        // 模拟答案块
        border(
            text_block("已把 file_edit.rs 的字符串定位改为块定位，旧接口保留兼容壳。")
                .wrap()
                .selectable(),
        )
        .background(ThemeRef::CardBackground)
        .padding(Thickness::xy(12.0, 8.0))
        .corner_radius(8.0),
        // 总结行（点击 → 覆盖层面板）
        border(
            hstack((
                text_block("已修改 2 个文件（+8 −5）")
                    .font_size(12.0)
                    .foreground(ThemeRef::SecondaryText),
                text_block("查看详情 ›")
                    .font_size(12.0)
                    .semibold()
                    .foreground(ThemeRef::AccentText),
            ))
            .spacing(8.0)
            .padding(Thickness::xy(10.0, 6.0)),
        )
        .background(ThemeRef::LayerFill)
        .border_brush(ThemeRef::CardStroke)
        .border_thickness(Thickness::uniform(1.0))
        .corner_radius(6.0)
        .on_tapped({
            let set_selected = set_selected.clone();
            let set_open = set_open.clone();
            move || {
                set_selected.call(0);
                set_open.call(true);
            }
        }),
        text_block("点击「查看详情 ›」→ 覆盖层面板（70% 尺寸）；✕ / Esc 关闭")
            .font_size(11.0)
            .foreground(ThemeRef::SecondaryText),
    ))
    .spacing(14.0)
    .padding(Thickness::uniform(32.0))
    .max_width(760.0)
    .horizontal_alignment(HorizontalAlignment::Center)
    .vertical_alignment(VerticalAlignment::Top);

    // 覆盖层弹层（V7）：open 时全屏遮罩 + 居中卡片；否则空 grid 穿透。
    let drawer_w = (win.width * DRAWER_RATIO).clamp(DRAWER_W_MIN, DRAWER_W_MAX);
    let drawer_h = (win.height * DRAWER_RATIO).clamp(DRAWER_H_MIN, DRAWER_H_MAX);

    // 文件列表（左列）
    let file_rows: Vec<Element> = files
        .iter()
        .enumerate()
        .map(|(i, (path, added, removed, failed, _))| {
            let (marker, fg) = if *failed {
                ("✕", ThemeRef::SystemCritical)
            } else {
                ("✓", ThemeRef::SystemSuccess)
            };
            let is_sel = i == selected;
            border(
                hstack((
                    text_block(marker).font_size(12.0).foreground(fg),
                    text_block(path)
                        .font_size(12.0)
                        .text_trimming(TextTrimming::CharacterEllipsis)
                        .foreground(if is_sel {
                            ThemeRef::AccentText
                        } else {
                            ThemeRef::PrimaryText
                        }),
                    text_block(format!("+{added} −{removed}"))
                        .font_size(11.0)
                        .foreground(ThemeRef::SecondaryText),
                ))
                .spacing(8.0)
                .padding(Thickness::xy(10.0, 7.0)),
            )
            .background(if is_sel {
                ThemeRef::AccentSecondary
            } else {
                ThemeRef::LayerFill
            })
            .corner_radius(4.0)
            .on_tapped({
                let set_selected = set_selected.clone();
                move || set_selected.call(i)
            })
            .into()
        })
        .collect();
    let total_added: usize = files.iter().map(|f| f.1).sum();
    let total_removed: usize = files.iter().map(|f| f.2).sum();
    let current = files.get(selected).cloned();
    let diff_panel: Element = match &current {
        Some((_, _, _, _, file)) if !file.rows.is_empty() => diff_file_view(
            file,
            "ms-appx:///Assets/fonts/CascadiaCode.ttf#Cascadia Code",
            &format!("demo-drawer-diff-{selected}"),
        ),
        _ => text_block("无 diff 数据").font_size(12.0).into(),
    };

    let dialog_content = grid((
        // 头部
        hstack((
            text_block("Diff 详情").font_size(14.0).semibold(),
            text_block(format!(
                "{} 个文件  +{}  −{}",
                files.len(),
                total_added,
                total_removed
            ))
            .font_size(12.0)
            .foreground(ThemeRef::SecondaryText),
            button("✕ 关闭")
                .on_click({
                    let set_open = set_open.clone();
                    move || set_open.call(false)
                })
                .horizontal_alignment(HorizontalAlignment::Right),
        ))
        .spacing(12.0)
        .padding(Thickness::xy(16.0, 12.0))
        .grid_row(0),
        // 主体：左文件列表 + 右 diff
        grid((
            border(
                vstack(file_rows)
                    .spacing(2.0)
                    .padding(Thickness::xy(10.0, 8.0)),
            )
            .background(ThemeRef::LayerFill)
            .grid_column(0),
            border(diff_panel)
                .padding(Thickness::xy(16.0, 8.0))
                .grid_column(1),
        ))
        .columns([GridLength::Pixel(FILE_LIST_WIDTH), GridLength::STAR])
        .rows([GridLength::STAR])
        .grid_row(1),
        // 底部：合计
        hstack((
            text_block(format!("合计  +{}  −{}", total_added, total_removed))
                .font_size(12.0)
                .foreground(ThemeRef::SecondaryText),
            text_block("Esc 或 ✕ 关闭")
                .font_size(11.0)
                .foreground(ThemeRef::TertiaryText)
                .horizontal_alignment(HorizontalAlignment::Right),
        ))
        .spacing(12.0)
        .padding(Thickness::xy(16.0, 10.0))
        .grid_row(2),
    ))
    .rows([GridLength::Auto, GridLength::STAR, GridLength::Auto])
    .width(drawer_w)
    .height(drawer_h);

    // 覆盖层（V7）：open 时全屏遮罩 + 居中卡片；否则空 grid（无背景 →
    // 不参与命中测试，点击穿透）。与真实 overlay 同构——本 demo 无静态槽，
    // 直接以 open state 驱动显隐。
    let dialog_layer: Element = if open {
        let on_close = {
            let set_open = set_open.clone();
            move || set_open.call(false)
        };
        let esc = KeyboardAccelerator::new(
            VirtualKey::Escape,
            VirtualKeyModifiers::None,
            on_close.clone(),
        );
        let card: Element = border(dialog_content)
            .background(ThemeRef::SolidBackground)
            .border_brush(ThemeRef::CardStroke)
            .border_thickness(Thickness::uniform(1.0))
            .corner_radius(8.0)
            .keyboard_accelerator(esc)
            .horizontal_alignment(HorizontalAlignment::Center)
            .vertical_alignment(VerticalAlignment::Center)
            .with_key("demo-drawer-card")
            .into();
        grid((card,))
            .rows([GridLength::STAR])
            .columns([GridLength::STAR])
            // 半透明遮罩：拦截主界面输入（模态）；点击遮罩不关闭。
            .background(Color {
                a: 140,
                r: 0,
                g: 0,
                b: 0,
            })
            .with_key("demo-drawer-overlay")
            .into()
    } else {
        grid(()).into()
    };

    // 内容 + 弹层同 cell。
    // ⚠ 根元素必须带背景，否则窗口纯白（已实证）。
    border(
        grid((content, dialog_layer))
            .rows([GridLength::STAR])
            .columns([GridLength::STAR]),
    )
    .background(ThemeRef::SolidBackground)
    .into()
}

fn main() -> windows_reactor::Result<()> {
    App::new()
        .title("DeepX — Diff 弹层演示")
        .inner_size(1100.0, 800.0)
        .render(render_app)
}
