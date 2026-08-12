//! Diff 抽屉演示（V4）：模拟 turn 视图 + 总结行 → 右侧滑入抽屉。
//! V4 链路验证：真实 edit 工具事件。
//!
//! 运行：`cargo run -p deepx-winui --bin diff_drawer_demo`
//!
//! 验证对象：
//! - 总结行「已修改 2 个文件 · 查看详情 ›」点击 → 抽屉右滑入
//! - 抽屉：左文件列表（✓/✕ + 路径 + ±N，选中高亮）/ 右单列 unified diff
//! - 多 op 同文件合并（file_edit.rs 两个 hunk 合并为一块）
//! - 关闭：✕ / 遮罩点击 / Esc
//!
//! 白屏根因备忘（已实证）：
//! 1. `#[path]` 重复编译 `diff_drawer.rs` → bin 白屏（仅声明即白）——已弃用，demo 内联渲染。
//! 2. **根元素无背景 → 窗口纯白不渲染**（grid/vstack/border 根均如此）；
//!    根必须带 `.background(ThemeRef::SolidBackground)`。diff_demo 正常因为它根带背景。
//!
//! 字体：统一 Cascadia Code（2407.24 变量字体，全字重 + 连字）。

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

fn demo_files() -> Vec<(String, usize, usize, bool, markdown_winui::DiffFile)> {
    let doc_a = parse_unified_diff(DEMO_DIFF_A);
    let doc_b = parse_unified_diff(DEMO_DIFF_B);
    let mut out = Vec::new();
    for f in &doc_a.files {
        out.push((
            f.display_path().to_string(),
            f.lines_added,
            f.lines_removed,
            false,
            f.clone(),
        ));
    }
    for f in &doc_b.files {
        out.push((
            f.display_path().to_string(),
            f.lines_added,
            f.lines_removed,
            false,
            f.clone(),
        ));
    }
    out
}

fn render_app(cx: &mut RenderCx) -> Element {
    // ⚠ hooks 必须无条件调用（React 规则）。
    let (open, set_open) = cx.use_state::<bool>(false);
    let (selected, set_selected) = cx.use_state::<usize>(0);
    let (dark, set_dark) = cx.use_state::<bool>(false);
    let (use_code, set_use_code) = cx.use_state::<bool>(true);
    let win = cx.use_inner_size();
    let files = demo_files();
    let diff_font = if use_code {
        "ms-appx:///Assets/fonts/CascadiaCode.ttf#Cascadia Code"
    } else {
        // 备用：无连字等宽（仅对比用）
        "ms-appx:///Assets/fonts/CascadiaMono.ttf#Cascadia Mono, Consolas"
    };

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
            text_block("字体：")
                .font_size(11.0)
                .foreground(ThemeRef::SecondaryText),
            button(if use_code { "Cascadia Code" } else { "Cascadia Mono" }).on_click({
                let set_use_code = set_use_code.clone();
                move || set_use_code.call(!use_code)
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
        // 总结行（点击 → 抽屉）
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
        text_block("点击「查看详情 ›」→ 抽屉右滑入；✕ / 遮罩 / Esc 关闭；顶部可切明暗与字体")
            .font_size(11.0)
            .foreground(ThemeRef::SecondaryText),
    ))
    .spacing(14.0)
    .padding(Thickness::uniform(32.0))
    .max_width(760.0)
    .horizontal_alignment(HorizontalAlignment::Center)
    .vertical_alignment(VerticalAlignment::Top);

    // 抽屉层（open 时显示）
    let drawer_layer: Element = if open {
        let drawer_w = (win.width * 0.82).clamp(760.0, 1440.0);
        // 文件列表
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
                &diff_font,
                &format!("demo-drawer-diff-{selected}"),
            ),
            _ => text_block("无 diff 数据").font_size(12.0).into(),
        };
        let close = {
            let set_open = set_open.clone();
            move || set_open.call(false)
        };
        let esc = KeyboardAccelerator::new(
            VirtualKey::Escape,
            VirtualKeyModifiers::None,
            close.clone(),
        );
        let drawer_el: Element = border(
            vstack((
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
                        .on_click(close.clone())
                        .horizontal_alignment(HorizontalAlignment::Right),
                ))
                .spacing(12.0)
                .padding(Thickness::xy(16.0, 12.0)),
                grid((
                    border(vstack(file_rows).spacing(2.0).padding(Thickness::xy(10.0, 8.0)))
                        .background(ThemeRef::LayerFill)
                        .grid_column(0),
                    border(diff_panel)
                        .padding(Thickness::xy(16.0, 8.0))
                        .grid_column(1),
                ))
                .columns([GridLength::Pixel(280.0), GridLength::STAR])
                .rows([GridLength::STAR]),
            ))
            .spacing(0.0),
        )
        .background(ThemeRef::SolidBackground)
        .border_brush(ThemeRef::CardStroke)
        .border_thickness(Thickness::uniform(1.0))
        .width(drawer_w)
        .height(win.height)
        // 注：demo 环境 transition 动画不播（元素停在偏移位 → 屏幕外），
        // 真实应用的滑入动画走 main.rs diff_drawer（已挂载，独立验证）。
        .keyboard_accelerator(esc)
        .with_key("demo-drawer")
        .into();
        let mask = grid(())
            .background(Color {
                a: 0x8C,
                r: 0,
                g: 0,
                b: 0,
            })
            .on_tapped(close)
            .with_key("demo-drawer-mask");
        grid((mask, drawer_el))
            .rows([GridLength::STAR])
            .columns([GridLength::STAR])
            .into()
    } else {
        grid(()).into()
    };

    // 覆盖层 grid（同 main.rs 模式）：内容 + 抽屉层同 cell。
    // ⚠ 根元素必须带背景，否则窗口纯白（已实证）。
    border(
        grid((content, drawer_layer))
            .rows([GridLength::STAR])
            .columns([GridLength::STAR]),
    )
    .background(ThemeRef::SolidBackground)
    .into()
}

fn main() -> windows_reactor::Result<()> {
    App::new()
        .title("DeepX — Diff 抽屉演示")
        .inner_size(1100.0, 800.0)
        .render(render_app)
}
