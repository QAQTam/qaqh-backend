//! Diff 抽屉（V4）：turn 末尾「查看详情」→ 右侧滑入面板。
//! V4 链路验证 #2：总结行修复后的真实 edit 工具事件。
//!
//! 复用 `diagram_zoom_overlay` 的「写端组件 / 读端覆盖层」分离模式：
//! - 写端：`open_diff_drawer`（总结行 / 详情按钮写入静态槽）
//! - 读端：`diff_drawer_overlay`（挂 main.rs 覆盖层 grid，80ms 轮询弹出）
//!
//! 布局：全窗遮罩（半透明黑，点击关闭）+ 右侧抽屉（60% 宽）
//! ┌ 头部：Diff 详情 + ✕ 关闭 ┐
//! ├ 左：文件列表（状态+路径+±N）│ 右：单列 unified diff（选中文件）┤
//! └ 底部：合计 +N −N ┘
//! 动画：enter 从右侧偏移滑入（Visual.Translation），exit 反向滑出。

use std::sync::Mutex;
use std::time::Duration;

use markdown_winui::{DiffFile, diff_file_view};
use windows_reactor::*;

/// 抽屉数据：按文件的 diff 列表（工具块 ToolBody::Diff 消费后的视图数据）。
#[derive(Clone, PartialEq)]
pub struct DiffDrawerRequest {
    pub turn_id: String,
    pub files: Vec<DrawerFile>,
}

/// 单文件抽屉条目：路径 + 统计 + 状态 + 已解析 diff（渲染用）。
#[derive(Clone, PartialEq)]
pub struct DrawerFile {
    pub path: String,
    pub added: usize,
    pub removed: usize,
    /// 对应工具失败（✕ 红；diff 可能为空）。
    pub failed: bool,
    /// 已解析的单文件 diff（path 与其 display_path 一致）。
    pub file: DiffFile,
}

/// 静态槽（写端组件 / 读端覆盖层分离；同 DIAGRAM_ZOOM 模式）。
pub static DIFF_DRAWER: Mutex<Option<DiffDrawerRequest>> = Mutex::new(None);

/// 写端：打开抽屉。
pub fn open_diff_drawer(req: DiffDrawerRequest) {
    if let Ok(mut slot) = DIFF_DRAWER.lock() {
        *slot = Some(req);
    }
}

const DRAWER_WIDTH_RATIO: f64 = 0.82;
const FILE_LIST_WIDTH: f64 = 280.0;
const POLL_INTERVAL: Duration = Duration::from_millis(80);

/// 读端覆盖层：轮询槽 → 右侧滑入抽屉（遮罩点击 / ✕ / Esc 关闭）。
pub fn diff_drawer_overlay(cx: &mut RenderCx) -> Element {
    // ⚠ hooks 必须在所有条件分支之前（React 规则：drawer=None 提前 return
    // 时顺序不能变，否则状态错位导致按钮/状态更新不生效）。
    let (drawer, set_drawer) = cx.use_state::<Option<DiffDrawerRequest>>(None);
    let (selected, set_selected) = cx.use_state::<usize>(0);
    let timer = cx.use_ref::<Option<DispatcherTimer>>(None);

    cx.use_effect((), {
        let set_drawer = set_drawer.clone();
        let timer = timer.clone();
        move || {
            if timer.borrow().is_some() {
                return;
            }
            if let Ok(t) = DispatcherTimer::new(POLL_INTERVAL, move || {
                if let Ok(slot) = DIFF_DRAWER.lock()
                    && let Some(req) = slot.as_ref()
                {
                    let req = req.clone();
                    set_drawer.call(Some(req));
                }
            }) {
                *timer.borrow_mut() = Some(t);
            }
        }
    });

    let Some(req) = drawer else {
        // 无请求：空 grid（无背景 → 点击穿透）。
        return grid(()).into();
    };

    // 关闭：清本地 state + 清静态槽（下次可重新弹开）。
    let close = {
        let set_drawer = set_drawer.clone();
        move || {
            set_drawer.call(None);
            if let Ok(mut slot) = DIFF_DRAWER.lock() {
                *slot = None;
            }
        }
    };
    let esc = KeyboardAccelerator::new(VirtualKey::Escape, VirtualKeyModifiers::None, close.clone());

    // 右滑入（enter = 从该偏移滑入到 0；exit 反向）。
    let slide_in = AnimationConfig {
        opacity: Some(0.0),
        scale: None,
        translation: Some(windows_numerics::Vector3 {
            x: 520.0,
            y: 0.0,
            z: 0.0,
        }),
        duration: Duration::from_millis(220),
        easing: Easing::EaseOut,
    };
    let slide_out = AnimationConfig {
        opacity: Some(0.0),
        scale: None,
        translation: Some(windows_numerics::Vector3 {
            x: 520.0,
            y: 0.0,
            z: 0.0,
        }),
        duration: Duration::from_millis(140),
        easing: Easing::EaseIn,
    };

    let win = cx.use_inner_size();
    let drawer_w = (win.width * DRAWER_WIDTH_RATIO).clamp(760.0, 1440.0);

    // 文件列表（左列）：状态 + 路径 + ±N；选中高亮。
    let file_rows: Vec<Element> = req
        .files
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let (marker, fg) = if f.failed {
                ("✕", ThemeRef::SystemCritical)
            } else {
                ("✓", ThemeRef::SystemSuccess)
            };
            let is_selected = i == selected;
            let row = border(
                hstack((
                    text_block(marker).font_size(12.0).foreground(fg),
                    text_block(&f.path)
                        .font_size(12.0)
                        .text_trimming(TextTrimming::CharacterEllipsis)
                        .foreground(if is_selected {
                            ThemeRef::AccentText
                        } else {
                            ThemeRef::PrimaryText
                        })
                        .max_width(drawer_w - FILE_LIST_WIDTH - 96.0),
                    text_block(format!("+{} −{}", f.added, f.removed))
                        .font_size(11.0)
                        .foreground(ThemeRef::SecondaryText),
                ))
                .spacing(8.0)
                .padding(Thickness::xy(10.0, 7.0)),
            )
            .background(if is_selected {
                ThemeRef::AccentSecondary
            } else {
                ThemeRef::LayerFill
            })
            .corner_radius(4.0)
            .on_tapped({
                let set_selected = set_selected.clone();
                move || set_selected.call(i)
            })
            .with_key(format!("drawer-file-{i}"))
            .into();
            row
        })
        .collect();
    let total_added: usize = req.files.iter().map(|f| f.added).sum();
    let total_removed: usize = req.files.iter().map(|f| f.removed).sum();

    // 右列：选中文件的单列 unified diff。
    let current = req.files.get(selected).cloned();
    let diff_panel: Element = match &current {
        Some(f) if !f.file.rows.is_empty() => diff_file_view(
            &f.file,
            "ms-appx:///Assets/fonts/CascadiaCode.ttf#Cascadia Code",
            &format!("drawer-diff-{}-{}", req.turn_id, f.path),
        ),
        _ => text_block(if current.as_ref().map(|f| f.failed).unwrap_or(false) {
            "该文件无 diff（工具执行失败）"
        } else {
            "无 diff 数据"
        })
        .font_size(12.0)
        .foreground(ThemeRef::SecondaryText)
        .into(),
    };

    let drawer_el: Element = border(
        vstack((
            // 头部
            hstack((
                text_block("Diff 详情").font_size(14.0).semibold(),
                text_block(format!(
                    "{} 个文件  +{}  −{}",
                    req.files.len(),
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
            .rows([GridLength::STAR]),
        ))
        .spacing(0.0),
    )
    .background(ThemeRef::SolidBackground)
    .border_brush(ThemeRef::CardStroke)
    .border_thickness(Thickness::uniform(1.0))
    .width(drawer_w)
    .height(win.height)
    .transition(Some(slide_in), Some(slide_out))
    .keyboard_accelerator(esc)
    .automation_name("Diff 详情抽屉")
    .with_key("diff-drawer")
    .into();

    // 遮罩（半透明黑，点击关闭；不挡住抽屉本身——抽屉叠在遮罩上层）。
    let mask = grid(())
        .background(Color {
            a: 0x8C,
            r: 0,
            g: 0,
            b: 0,
        })
        .on_tapped(close.clone())
        .with_key("diff-drawer-mask");

    grid((mask, drawer_el))
        .rows([GridLength::STAR])
        .columns([GridLength::STAR])
        .with_key("diff-drawer-layer")
        .into()
}
