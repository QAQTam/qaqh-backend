//! XAML 原生侧栏 — 第二版视觉语言 + 常驻手写布局 + 可拖拽宽度。
//!
//! 视觉还原第二版（NavigationView）设计：SymbolIcon 系统图标、分组头、
//! 图标按钮、品牌区——但容器用手写 Grid，**常驻不折叠**（NavigationView
//! 的自动收起行为不符合需求，已弃用）。
//!
//! 宽度交互（Win11 splitter 语义）：
//!   - 右缘 12px 抓握条：按住拖拽调宽（180–400px），悬停显示强调色；
//!   - 单击抓握条恢复默认宽度（260px）；
//!   - 拖拽 = `capture_pointer_on_press` 原生指针捕获 + `PointerEventInfo.window_x`
//!     差分（reactor ≥ #4782；此前用 GetCursorPos 16ms 轮询 hack——路由事件
//!     无捕获时 moved 在指针移出抓握条后即失联）。
//!
//! 交互（沿用已修复的链路）：
//!   - 行内标题 pointer-pressed → `spawn_resume`（幂等，同 seed 跳过）；
//!   - 行内 `×` 图标按钮 → `spawn_delete`；
//!   - 列表 `SelectionMode::None`——无选中机制、无自动选中副作用；
//!   - active 高亮 = SubtleFill 圆角药丸（Win11 选中态语言），由 timer
//!     同步的 `active` state 渲染期驱动。

use std::sync::Arc;
use std::time::Duration;

use windows_reactor::*;

use crate::bridge::Bridge;
use crate::shell_store::{ActivityState, SessionItem};

/// 侧边栏宽度约束（拖拽范围）。
pub const SIDEBAR_MIN_WIDTH: f64 = 180.0;
pub const SIDEBAR_MAX_WIDTH: f64 = 400.0;
pub const SIDEBAR_DEFAULT_WIDTH: f64 = 260.0;

/// 纯图标按钮（subtle 样式，无文字）——用于行内删除。
fn icon_button(icon: Icon, on_click: impl Fn() + 'static) -> Element {
    button("").icon(icon).subtle().on_click(on_click).into()
}

/// 会话活动状态 → Fluent 语义色令牌（自动适配深浅主题）。
///
/// 与 Web 版 `TaskSidebar` 的 `task-state` 圆点配色对齐：
/// working=绿 / waiting_user=强调色 / starting=蓝紫 / disconnected=红 / idle=灰。
pub(crate) fn state_color(state: ActivityState) -> ThemeRef {
    match state {
        ActivityState::Working => ThemeRef::SystemSuccess,
        ActivityState::WaitingUser => ThemeRef::Accent,
        ActivityState::Starting => ThemeRef::SystemAttention,
        ActivityState::Disconnected => ThemeRef::SystemCritical,
        ActivityState::Idle => ThemeRef::SystemNeutral,
    }
}

/// 状态圆点：8px 圆形（Border + 4px 圆角），Fluent 语义色。
pub(crate) fn state_dot(state: ActivityState) -> Element {
    border(text_block(""))
        .width(8.0)
        .height(8.0)
        .corner_radius(4.0)
        .background(state_color(state))
        .vertical_alignment(VerticalAlignment::Center)
        .into()
}

/// 一行会话：选中竖条 + 状态圆点 + 标题（单行省略）+ 删除图标。
///
/// 结构稳定性契约（D15）：**所有行恒为 `border(grid(竖条, 圆点, 标题,
/// 删除))`**——active 只改竖条颜色 + 背景（modifiers 字段 diff，原地更新），
/// 不切换元素类型（旧实现 active 时包 border / 非 active 裸 grid，kind 跳变
/// 有控件树错位风险，settings nav 同款问题已修）。
///
/// 选中语义 = Win11 NavigationView：左侧 3px Accent 竖条 + SubtleFill 药丸
/// 背景；标题恒 `PrimaryText`（不再随选中变色）。新行出现时淡入
/// （`transition` = ImplicitShowAnimation，keyed 列表新行 mount 即触发；
/// 行内 active 切换不重建行 → 不重放）。
///
/// 删除语义（B 拍板）：行内 × = **彻底删除**（真删磁盘 + 确认对话框）；
/// 归档走标签页 ×（`spawn_archive`）。
fn session_row(
    item: &SessionItem,
    active: bool,
    bridge: Arc<Bridge>,
    set_confirm: SetState<Option<String>>,
) -> Element {
    let seed = item.seed.clone();
    // 选中竖条（结构常驻，active 只改颜色）。
    let indicator = border(text_block(""))
        .width(3.0)
        .height(16.0)
        .corner_radius(1.5)
        .vertical_alignment(VerticalAlignment::Center);
    let indicator = if active {
        indicator.background(ThemeRef::Accent)
    } else {
        indicator
    };
    let dot: Element = state_dot(item.state);
    let title_el: Element = text_block(item.title.clone())
        .text_trimming(TextTrimming::CharacterEllipsis)
        .foreground(ThemeRef::PrimaryText)
        .vertical_alignment(VerticalAlignment::Center)
        .on_pointer_pressed({
            let seed = seed.clone();
            let bridge = bridge.clone();
            move |_| bridge.spawn_resume(&seed)
        })
        .into();
    let delete = icon_button(Icon::symbol(Symbol::Delete), {
        let seed = seed.clone();
        let set_confirm = set_confirm.clone();
        move || set_confirm.call(Some(seed.clone()))
    })
    .vertical_alignment(VerticalAlignment::Center);
    let row: Element = grid((
        indicator.grid_column(0),
        dot.grid_column(1),
        title_el.grid_column(2),
        delete.grid_column(3),
    ))
    .columns([
        GridLength::Auto,
        GridLength::Auto,
        GridLength::STAR,
        GridLength::Auto,
    ])
    .column_spacing(8.0)
    .padding(Thickness::xy(10.0, 6.0))
    .into();
    // 恒为 border；仅 background 随 active（diff_modifiers 原地更新）。
    let item_el = border(row).corner_radius(8.0);
    let item_el = if active {
        item_el.background(ThemeRef::SubtleFill)
    } else {
        item_el
    };
    // 不给虚拟化行挂 entrance：滚动回收/重新 realize 时重播动画会让列表
    // 看起来抖动。新增会话的状态变化由 WinUI 自带 pointer/selection 视觉表达。
    item_el.into()
}

/// 归档会话行：置灰标题 + 状态点 + ×（彻底删除确认）。
///
/// 点击标题 = 恢复归档（`spawn_unarchive` → meta 标记清除 + resume 拉起
/// 实例并打开）；× = 彻底删除（同活动行语义，走确认对话框）。
/// 结构契约同 `session_row`：恒为 border(grid(圆点, 标题, 删除))。
fn archive_row(
    item: &SessionItem,
    bridge: Arc<Bridge>,
    set_confirm: SetState<Option<String>>,
) -> Element {
    let seed = item.seed.clone();
    let dot: Element = state_dot(item.state);
    let title_el: Element = text_block(item.title.clone())
        .text_trimming(TextTrimming::CharacterEllipsis)
        // 归档 = 告一段落：标题置灰（SecondaryText）。
        .foreground(ThemeRef::SecondaryText)
        .vertical_alignment(VerticalAlignment::Center)
        .on_pointer_pressed({
            let seed = seed.clone();
            let bridge = bridge.clone();
            move |_| bridge.spawn_unarchive(&seed)
        })
        .into();
    let delete = icon_button(Icon::symbol(Symbol::Delete), {
        let seed = seed.clone();
        let set_confirm = set_confirm.clone();
        move || set_confirm.call(Some(seed.clone()))
    })
    .vertical_alignment(VerticalAlignment::Center);
    let row: Element = grid((
        dot.grid_column(0),
        title_el.grid_column(1),
        delete.grid_column(2),
    ))
    .columns([GridLength::Auto, GridLength::STAR, GridLength::Auto])
    .column_spacing(8.0)
    .padding(Thickness::xy(10.0, 6.0))
    .into();
    border(row).corner_radius(8.0).into()
}

/// XAML 侧栏组件（放入外层 Grid 第 0 列；宽度由 `width` 控制、可拖拽）。
pub fn sidebar(
    cx: &mut RenderCx,
    bridge: Arc<Bridge>,
    width: f64,
    set_width: SetState<f64>,
) -> Element {
    let (items, set_items) = cx.use_state::<Vec<SessionItem>>(Vec::new());
    let (active, set_active) = cx.use_state::<String>(String::new());
    // 彻底删除确认对话框的待删会话（None = 关闭）。行内 × / 操作区删除
    // 按钮只置位此 state，确认后（Primary）才真正 `spawn_delete`。
    let (confirm_seed, set_confirm_seed) = cx.use_state::<Option<String>>(None);
    let timer = cx.use_ref::<Option<DispatcherTimer>>(None);
    let last_rev = cx.use_ref::<u64>(0);
    // 拖拽状态：`(按下时窗口 x, 按下时宽度)`——差分计算（window_x 稳定，
    // 拖拽中窗口不动则与屏幕坐标等价）。
    let drag_start = cx.use_ref::<Option<(f64, f64)>>(None);
    let (splitter_hover, set_splitter_hover) = cx.use_state::<bool>(false);

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

    // ── 品牌区（第二版 pane_title 语义）────────────────────────
    let brand: Element = {
        let el: Element = text_block("DeepX").semibold().font_size(18.0).into();
        el.on_pointer_pressed({
            let bridge = bridge.clone();
            move |_| bridge.navigate("home", None)
        })
        .margin(12.0)
    };

    // ── 操作区：新建任务 + 删除当前会话（第二版 pane_footer 语义）──
    let actions: Element = {
        let sp: Element = hstack((
            button("新建任务")
                .icon(Icon::symbol(Symbol::Add))
                .subtle()
                .on_click({
                    let bridge = bridge.clone();
                    move || bridge.spawn_new_session()
                }),
            icon_button(Icon::symbol(Symbol::Delete), {
                let bridge = bridge.clone();
                let set_confirm = set_confirm_seed.clone();
                move || {
                    let seed = bridge.core().active_seed();
                    if !seed.is_empty() {
                        set_confirm.call(Some(seed));
                    }
                }
            }),
        ))
        .into();
        sp.margin(12.0)
    };

    // ── 分组头（SecondaryText 主题色令牌，非 opacity hack）──────
    let group_label: Element = {
        let el: Element = text_block("任务")
            .font_size(12.0)
            .foreground(ThemeRef::SecondaryText)
            .into();
        el.margin(Thickness::xy(12.0, 6.0))
    };

    // ── 会话列表（SelectionMode::None 禁用选中）────────────────
    // 分组：活动（标签条同源：非归档）在上；归档组置灰显示、点击恢复。
    let (active_items, archived_items): (Vec<SessionItem>, Vec<SessionItem>) =
        items.iter().cloned().partition(|s| !s.archived);
    let list_active = list_view(active_items.clone(), {
        let bridge = bridge.clone();
        let set_confirm = set_confirm_seed.clone();
        let active = active.clone();
        move |item, _| {
            session_row(
                item,
                item.seed == active,
                bridge.clone(),
                set_confirm.clone(),
            )
        }
    })
    .with_key_selector(|item| item.seed.clone())
    .selection_mode(SelectionMode::None)
    .build();
    // 归档组头（仅归档非空时显示）。
    let archived_label: Element = if archived_items.is_empty() {
        grid(()).into()
    } else {
        let el: Element = text_block("归档")
            .font_size(12.0)
            .foreground(ThemeRef::SecondaryText)
            .into();
        el.margin(Thickness::xy(12.0, 8.0))
    };
    let list_archived = list_view(archived_items.clone(), {
        let bridge = bridge.clone();
        let set_confirm = set_confirm_seed.clone();
        move |item, _| archive_row(item, bridge.clone(), set_confirm.clone())
    })
    .with_key_selector(|item| item.seed.clone())
    .selection_mode(SelectionMode::None)
    .build();
    let session_list: Element =
        scroll_viewer(vstack((list_active, archived_label, list_archived))).into();

    // ── 底部导航：技能 / 设置 ──────────────────────────────────
    let footer: Element = {
        let sp: Element = hstack((
            button("技能")
                .icon(Icon::symbol(Symbol::Library))
                .subtle()
                .on_click({
                    let bridge = bridge.clone();
                    move || bridge.navigate("skills", None)
                }),
            button("设置")
                .icon(Icon::symbol(Symbol::Setting))
                .subtle()
                .on_click({
                    let bridge = bridge.clone();
                    move || bridge.navigate("settings", None)
                }),
        ))
        .into();
        sp.margin(12.0)
    };

    // ── 内容列：品牌 / 操作区 / 分组头 / 列表 / 页脚 ────────────
    let content: Element = grid((
        brand.grid_row(0).grid_column(0),
        actions.grid_row(1).grid_column(0),
        group_label.grid_row(2).grid_column(0),
        session_list.grid_row(3).grid_column(0),
        footer.grid_row(4).grid_column(0),
    ))
    .rows([
        GridLength::Auto,
        GridLength::Auto,
        GridLength::Auto,
        GridLength::STAR,
        GridLength::Auto,
    ])
    .into();

    // ── 右缘抓握条（12px 命中区）：悬停强调色；双击恢复默认宽度 ──
    let bar: Element = border(text_block(""))
        .width(if splitter_hover { 2.0 } else { 1.0 })
        .background(if splitter_hover {
            ThemeRef::AccentSecondary
        } else {
            ThemeRef::DividerStroke
        })
        .horizontal_alignment(HorizontalAlignment::Center)
        .into();
    let splitter: Element = border(bar)
        .width(12.0)
        // 按下即捕获指针：moved 持续到达（含移出抓握条），无需轮询。
        .capture_pointer_on_press()
        .on_pointer_pressed({
            let drag_start = drag_start.clone();
            move |info: PointerEventInfo| {
                // 记录按下时窗口坐标 x 与当前宽度（差分基准）。
                // window_x 为窗口相对坐标：拖拽中窗口不动，差分与屏幕坐标等价。
                *drag_start.borrow_mut() = Some((info.window_x, width));
            }
        })
        .on_pointer_moved({
            let drag_start = drag_start.clone();
            let set_width = set_width.clone();
            move |info: PointerEventInfo| {
                // 防御：capture 生效后无需左键检查，但保底（capture 失败时）。
                if !info.is_left_button_pressed {
                    return;
                }
                let Some((sx, sw)) = *drag_start.borrow() else {
                    return;
                };
                let new_w = (sw + (info.window_x - sx)).clamp(SIDEBAR_MIN_WIDTH, SIDEBAR_MAX_WIDTH);
                if (new_w - sw).abs() >= 2.0 {
                    set_width.call(new_w);
                }
            }
        })
        .on_pointer_released({
            let drag_start = drag_start.clone();
            move |_| *drag_start.borrow_mut() = None
        })
        .on_pointer_capture_lost({
            // 捕获被系统收回（窗口失焦/弹窗等）→ 结束拖拽，避免 stale 状态。
            let drag_start = drag_start.clone();
            move || *drag_start.borrow_mut() = None
        })
        .on_pointer_entered({
            let set_hover = set_splitter_hover.clone();
            move |_| set_hover.call(true)
        })
        .on_pointer_exited({
            let set_hover = set_splitter_hover.clone();
            move || set_hover.call(false)
        })
        .on_tapped({
            // 单击抓握条 = 恢复默认宽度。
            // （WinUI 快速双击只触发一次 Tapped，无法做双击检测；
            //   拖拽有位移时 Tapped 不触发，两者天然互斥。）
            let set_width = set_width.clone();
            move || set_width.call(SIDEBAR_DEFAULT_WIDTH)
        })
        .into();

    // ── 彻底删除确认对话框（phantom child 覆盖层：同 cell 重叠渲染）──
    // 归档请用标签页的 ×（spawn_archive）；列表 × 与操作区删除按钮 =
    // 真删（manager.delete 磁盘目录），确认后执行。
    let dialog: Element = match confirm_seed.clone() {
        Some(seed) => {
            let bridge = bridge.clone();
            let set_confirm = set_confirm_seed.clone();
            ContentDialog::new("彻底删除会话")
                .content("将删除该会话及其全部消息文件，不可恢复。\n\n归档会话请使用标签页的关闭按钮（×）。")
                .primary_button_text("彻底删除")
                .close_button_text("取消")
                .is_open(true)
                .on_closed(move |result: ContentDialogResult| {
                    if result == ContentDialogResult::Primary {
                        bridge.spawn_delete(&seed);
                    }
                    set_confirm.call(None);
                })
                .into()
        }
        None => grid(()).into(),
    };

    // ── 根容器（无事件：指针已由 splitter 捕获）────────────────────
    grid((
        grid((content.grid_column(0), splitter.grid_column(1)))
            .columns([GridLength::STAR, GridLength::Pixel(12.0)])
            .grid_row(0)
            .grid_column(0),
        dialog.grid_row(0).grid_column(0),
    ))
    .rows([GridLength::STAR])
    .into()
}
