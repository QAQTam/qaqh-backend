//! 顶部会话标签条（TabView）——会话切换导航（方案 A）。
//!
//! - 数据源：`sessions` 快照 **filter(!archived)**（归档会话不出现在标签条，
//!   左侧列表归档组可见可恢复）；500ms 轮询 rev 同步（与 sidebar 同模式）。
//! - 选中标签 → `spawn_resume`（单 chat_view 实例 + 快照恢复，后台会话
//!   继续运行）；`×` → `spawn_archive`（daemon 关实例 + 归档标记，自动切
//!   邻居由 bridge 侧完成）；`+` → `spawn_new_session`。
//! - 标签头 = 状态圆点 + 标题（`TabItem.header_element` 通道，reactor
//!   `set_header_element` 挂载）；标题固定宽 + 省略号防标签条被撑爆。
//! - selected_index 受控同步 active_seed：reactor prop diff 只在值变化时
//!   设置，用户点击后（resume 异步期间）控件内部选中态不被重置。

use std::sync::Arc;
use std::time::Duration;

use windows_reactor::*;

use crate::bridge::Bridge;
use crate::shell_store::SessionItem;
use crate::sidebar::state_dot;

/// 标签条高度（main.rs 内容区 row0）。
pub const TAB_STRIP_HEIGHT: f64 = 44.0;
/// 标签标题最大像素宽度（截断后兜底，防极端字体/全角溢出撑爆标签）。
const TAB_TITLE_MAX_WIDTH: f64 = 110.0;
/// 标签标题显示宽度上限：ASCII=1、其他字符（中文等）=2 → 14 = 7 汉字
/// / 14 英文字符。超出截断加省略号，标签条不随标题长度膨胀。
const TAB_TITLE_WIDTH_LIMIT: usize = 14;

/// 按显示宽度截断标题（中文字符计 2、ASCII 计 1），超限截断 + "…"。
fn truncate_title(title: &str) -> String {
    let mut width = 0usize;
    let mut end = 0usize;
    for (i, c) in title.char_indices() {
        let w = if c.is_ascii() { 1 } else { 2 };
        if width + w > TAB_TITLE_WIDTH_LIMIT {
            break;
        }
        width += w;
        end = i + c.len_utf8();
    }
    if end >= title.len() {
        title.to_string()
    } else {
        format!("{}…", &title[..end])
    }
}

/// 标签头组合：状态圆点 + 标题（字符级截断：7 汉字 / 14 英文字符）。
fn tab_header(item: &SessionItem) -> Element {
    let dot: Element = state_dot(item.state).grid_column(0);
    let title: Element = text_block(truncate_title(&item.title))
        .text_trimming(TextTrimming::CharacterEllipsis)
        .width(TAB_TITLE_MAX_WIDTH)
        .foreground(ThemeRef::PrimaryText)
        .vertical_alignment(VerticalAlignment::Center)
        .grid_column(1)
        .into();
    grid((dot, title))
        .columns([GridLength::Auto, GridLength::Pixel(TAB_TITLE_MAX_WIDTH)])
        .column_spacing(6.0)
        .padding(Thickness::xy(4.0, 0.0))
        .into()
}

/// 顶部会话标签条组件（放入右侧文档工作区 row0，不跨导航侧栏）。
pub fn session_tabs(cx: &mut RenderCx, bridge: Arc<Bridge>) -> Element {
    let (items, set_items) = cx.use_state::<Vec<SessionItem>>(Vec::new());
    let (active, set_active) = cx.use_state::<String>(String::new());
    let timer = cx.use_ref::<Option<DispatcherTimer>>(None);
    let last_rev = cx.use_ref::<u64>(0);

    // 首次挂载：触发初始刷新；之后 500ms 轮询 rev，变化才 set_state 重渲染。
    cx.use_effect((), {
        let bridge = bridge.clone();
        let set_items = set_items.clone();
        let set_active = set_active.clone();
        let timer = timer.clone();
        let last_rev = last_rev.clone();
        move || {
            let core = bridge.core();
            bridge.spawn_refresh_sessions();
            *last_rev.borrow_mut() = core.session_snapshot().1;
            if let Ok(t) = DispatcherTimer::new(Duration::from_millis(500), {
                let core = core.clone();
                let set_items = set_items.clone();
                let set_active = set_active.clone();
                let last_rev = last_rev.clone();
                move || {
                    let (items, rev) = core.session_snapshot();
                    if rev != *last_rev.borrow() {
                        *last_rev.borrow_mut() = rev;
                        set_items.call(items);
                        set_active.call(core.active_seed());
                    }
                }
            }) {
                *timer.borrow_mut() = Some(t);
            }
        }
    });

    // 非归档会话 → TabItem（content 空占位：内容区在 TabView 外，单
    // chat_view 实例由 seed 切换驱动，不建每会话 pageview）。
    let tabs: Vec<TabItem> = items
        .iter()
        .filter(|s| !s.archived)
        .map(|item| {
            TabItem::new(item.title.clone(), grid(()))
                .with_key(item.seed.clone())
                .closable(true)
                .header_element(tab_header(item))
        })
        .collect();

    // selected_index：active 在标签中的位置；无（空/全部归档）→ -1。
    let selected = tabs
        .iter()
        .position(|t| t.key.as_deref() == Some(active.as_str()))
        .map(|i| i as i32)
        .unwrap_or(-1);

    TabView::new(tabs)
        .selected_index(selected)
        .is_add_tab_button_visible(true)
        .on_selection_changed({
            let bridge = bridge.clone();
            move |index: i32| {
                // 回调在用户点击时触发（受控刷新不走此路径）；index 越界防御。
                let items = bridge.core().session_snapshot().0;
                let seed = items
                    .iter()
                    .filter(|s| !s.archived)
                    .nth(index as usize)
                    .map(|s| s.seed.clone());
                if let Some(seed) = seed {
                    bridge.spawn_resume(&seed);
                }
            }
        })
        .on_close_requested({
            let bridge = bridge.clone();
            move |key: String| bridge.spawn_archive(&key)
        })
        .on_add_tab_button_click({
            let bridge = bridge.clone();
            move |_| bridge.spawn_new_session()
        })
        .into()
}
