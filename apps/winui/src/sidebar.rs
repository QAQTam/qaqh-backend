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
use crate::shell_store::{ActivityState, SessionItem, WorkspaceItem};

/// 诊断日志（窗口程序无控制台：写 %TEMP%/deepx-winui.log；同 chat_view 模式）。
fn log_diag(msg: &str) {
    use std::io::Write;
    let path = std::env::temp_dir().join("deepx-winui.log");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(
            f,
            "[{}] {msg}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
        );
    }
}

/// 侧边栏宽度约束（拖拽范围）。
pub const SIDEBAR_MIN_WIDTH: f64 = 180.0;
pub const SIDEBAR_MAX_WIDTH: f64 = 400.0;
pub const SIDEBAR_DEFAULT_WIDTH: f64 = 260.0;

/// 构建标识（版本号标识数据源）。
///
/// 优先读 exe 同目录 `build-info.json`（installer 打包时由
/// collect-payload-winui.ps1 注入，与 BuildId/staging 目录名同源：
/// `<版本>-<commit12>-<UTC yyyyMMddHHmmss>`，已到秒粒度）；
/// dev 运行（无该文件）fallback 到编译期 `CARGO_PKG_VERSION`。
fn build_id_label() -> String {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let info = dir.join("build-info.json");
            if let Ok(text) = std::fs::read_to_string(&info) {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                    if let Some(id) = v.get("build_id").and_then(|x| x.as_str()) {
                        return format!("v{id}");
                    }
                }
            }
        }
    }
    format!("v{}", env!("CARGO_PKG_VERSION"))
}

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
/// 活动区行实现已由 `workspace_session_row` 取代（D15 变体含「移出」列）。

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
        .max_lines(1)
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

/// workspace 展开区内的会话行：D15 变体 `border(grid(竖条, 圆点, 标题,
/// 移动, 删除))`——在活动行基础上多一列移动按钮。`attach_to` 为 None 时
/// 按钮 = 移出到未分组（`workspace.detach`）；`Some(ws_id)` 时按钮 =
/// 移入该 workspace（`workspace.move_session`，未分组区行使用）。
/// 结构契约与 `session_row` 同纪律：active 只改修饰字段，不切换元素类型。
fn workspace_session_row(
    item: &SessionItem,
    active: bool,
    bridge: Arc<Bridge>,
    set_confirm: SetState<Option<String>>,
    attach_to: Option<String>,
) -> Element {
    let seed = item.seed.clone();
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
        .max_lines(1)
        .text_trimming(TextTrimming::CharacterEllipsis)
        .foreground(ThemeRef::PrimaryText)
        .vertical_alignment(VerticalAlignment::Center)
        .on_pointer_pressed({
            let seed = seed.clone();
            let bridge = bridge.clone();
            move |_| bridge.spawn_resume(&seed)
        })
        .into();
    // 移动按钮：移出（workspace 区内）或移入当前 workspace（未分组区内）。
    let move_btn: Element = match attach_to.clone() {
        Some(target) => icon_button(Icon::symbol(Symbol::Folder), {
            let seed = seed.clone();
            let bridge = bridge.clone();
            move || bridge.spawn_workspace_move_session(seed.clone(), target.clone())
        })
        .vertical_alignment(VerticalAlignment::Center),
        None => icon_button(Icon::symbol(Symbol::Undo), {
            let seed = seed.clone();
            let bridge = bridge.clone();
            move || bridge.spawn_workspace_detach(seed.clone())
        })
        .vertical_alignment(VerticalAlignment::Center),
    };
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
        move_btn.grid_column(3),
        delete.grid_column(4),
    ))
    .columns([
        GridLength::Auto,
        GridLength::Auto,
        GridLength::STAR,
        GridLength::Auto,
        GridLength::Auto,
    ])
    .column_spacing(8.0)
    .padding(Thickness::xy(10.0, 6.0))
    .into();
    let item_el = border(row).corner_radius(8.0);
    let item_el = if active {
        item_el.background(ThemeRef::SubtleFill)
    } else {
        item_el
    };
    item_el.into()
}

/// 一个 workspace 行：文件夹图标 + 标题 + 会话数徽标 + 重命名/删除按钮。
/// 点击行主体 = 选中（`set_current_workspace`，标签页随之过滤）；选中态 =
/// 药丸高亮（同会话行语义）。结构恒为 `border(grid(图标, 标题, 徽标, ✎, 🗑))`。
fn workspace_row(
    ws: &WorkspaceItem,
    selected: bool,
    count: usize,
    bridge: Arc<Bridge>,
    set_ws_confirm: SetState<Option<String>>,
    set_ws_rename: SetState<Option<(String, String)>>,
) -> Element {
    let id = ws.id.clone();
    let title = ws.title.clone();
    let icon = icon_button(Icon::symbol(Symbol::Folder), {
        let id = id.clone();
        let bridge = bridge.clone();
        move || bridge.set_current_workspace(Some(id.clone()))
    })
    .vertical_alignment(VerticalAlignment::Center);
    let title_el: Element = text_block(title.clone())
        .max_lines(1)
        .text_trimming(TextTrimming::CharacterEllipsis)
        .foreground(ThemeRef::PrimaryText)
        .vertical_alignment(VerticalAlignment::Center)
        .on_pointer_pressed({
            let id = id.clone();
            let bridge = bridge.clone();
            move |_| bridge.set_current_workspace(Some(id.clone()))
        })
        .into();
    // 会话数徽标（SecondaryText 小字）。
    let badge: Element = text_block(count.to_string())
        .font_size(11.0)
        .foreground(ThemeRef::SecondaryText)
        .vertical_alignment(VerticalAlignment::Center)
        .into();
    let rename = icon_button(Icon::symbol(Symbol::Edit), {
        let id = id.clone();
        let title = title.clone();
        let set_ws_rename = set_ws_rename.clone();
        move || set_ws_rename.call(Some((id.clone(), title.clone())))
    })
    .vertical_alignment(VerticalAlignment::Center);
    let delete = icon_button(Icon::symbol(Symbol::Delete), {
        let id = id.clone();
        let set_ws_confirm = set_ws_confirm.clone();
        move || set_ws_confirm.call(Some(id.clone()))
    })
    .vertical_alignment(VerticalAlignment::Center);
    let row: Element = grid((
        icon.grid_column(0),
        title_el.grid_column(1),
        badge.grid_column(2),
        rename.grid_column(3),
        delete.grid_column(4),
    ))
    .columns([
        GridLength::Auto,
        GridLength::STAR,
        GridLength::Auto,
        GridLength::Auto,
        GridLength::Auto,
    ])
    .column_spacing(8.0)
    .padding(Thickness::xy(10.0, 6.0))
    .into();
    let item_el = border(row).corner_radius(8.0);
    let item_el = if selected {
        item_el.background(ThemeRef::SubtleFill)
    } else {
        item_el
    };
    item_el.into()
}

/// 未分组固定项：点击 = 未分组视图（`current_workspace=None`，标签页显示
/// 全部孤儿会话）。常驻不隐藏（D6），带计数徽标。
fn ungrouped_row(
    count: usize,
    selected: bool,
    bridge: Arc<Bridge>,
) -> Element {
    let icon = icon_button(Icon::symbol(Symbol::Folder), {
        let bridge = bridge.clone();
        move || bridge.set_current_workspace(None)
    })
    .vertical_alignment(VerticalAlignment::Center);
    let title_el: Element = text_block("未分组")
        .max_lines(1)
        .text_trimming(TextTrimming::CharacterEllipsis)
        .foreground(ThemeRef::PrimaryText)
        .vertical_alignment(VerticalAlignment::Center)
        .on_pointer_pressed({
            let bridge = bridge.clone();
            move |_| bridge.set_current_workspace(None)
        })
        .into();
    let badge: Element = text_block(count.to_string())
        .font_size(11.0)
        .foreground(ThemeRef::SecondaryText)
        .vertical_alignment(VerticalAlignment::Center)
        .into();
    let row: Element = grid((
        icon.grid_column(0),
        title_el.grid_column(1),
        badge.grid_column(2),
    ))
    .columns([GridLength::Auto, GridLength::STAR, GridLength::Auto])
    .column_spacing(8.0)
    .padding(Thickness::xy(10.0, 6.0))
    .into();
    let item_el = border(row).corner_radius(8.0);
    let item_el = if selected {
        item_el.background(ThemeRef::SubtleFill)
    } else {
        item_el
    };
    item_el.into()
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
    // workspace 数据源（同 session 轮询模式，独立 rev）。
    let (workspaces, set_workspaces) = cx.use_state::<Vec<WorkspaceItem>>(Vec::new());
    let (current_ws, set_current_ws) = cx.use_state::<Option<String>>(None);
    // 彻底删除确认对话框的待删会话（None = 关闭）。行内 × / 操作区删除
    // 按钮只置位此 state，确认后（Primary）才真正 `spawn_delete`。
    let (confirm_seed, set_confirm_seed) = cx.use_state::<Option<String>>(None);
    // 删除 workspace 确认（None = 关闭）；确认后 `spawn_workspace_delete`。
    let (ws_confirm, set_ws_confirm) = cx.use_state::<Option<String>>(None);
    // 重命名对话框：`Some((id, 当前标题))` = 打开；输入文本独立 ref（
    // 对话框闭包内读写，无需驱动重渲染）。
    let (ws_rename, set_ws_rename) = cx.use_state::<Option<(String, String)>>(None);
    let ws_rename_input = cx.use_ref::<String>(String::new());
    let timer = cx.use_ref::<Option<DispatcherTimer>>(None);
    let last_rev = cx.use_ref::<u64>(0);
    let last_ws_rev = cx.use_ref::<u64>(0);
    let last_cur_ws = cx.use_ref::<Option<String>>(None);
    // 拖拽状态：`(按下时窗口 x, 按下时宽度)`——差分计算（window_x 稳定，
    // 拖拽中窗口不动则与屏幕坐标等价）。
    let drag_start = cx.use_ref::<Option<(f64, f64)>>(None);
    // 构建标识缓存（footer 版本小字；读一次避免每帧读文件）。
    let build_label = cx.use_ref::<Option<String>>(None);
    let (splitter_hover, set_splitter_hover) = cx.use_state::<bool>(false);

    // 首次挂载：触发初始刷新；之后 500ms 轮询 rev，变化才 set_state 重渲染。
    cx.use_effect((), {
        let bridge = bridge.clone();
        let set_items = set_items.clone();
        let set_active = set_active.clone();
        let set_workspaces = set_workspaces.clone();
        let set_current_ws = set_current_ws.clone();
        let timer = timer.clone();
        let last_rev = last_rev.clone();
        let last_ws_rev = last_ws_rev.clone();
        move || {
            let core = bridge.core();
            bridge.spawn_refresh_sessions();
            bridge.spawn_refresh_workspaces();
            *last_rev.borrow_mut() = core.session_snapshot().1;
            *last_ws_rev.borrow_mut() = core.workspace_snapshot().1;
            if let Ok(t) = DispatcherTimer::new(Duration::from_millis(500), {
                let core = core.clone();
                let set_items = set_items.clone();
                let set_active = set_active.clone();
                let set_workspaces = set_workspaces.clone();
                let set_current_ws = set_current_ws.clone();
                let last_rev = last_rev.clone();
                let last_ws_rev = last_ws_rev.clone();
                move || {
                    let (items, rev) = core.session_snapshot();
                    if rev != *last_rev.borrow() {
                        *last_rev.borrow_mut() = rev;
                        set_items.call(items);
                        set_active.call(core.active_seed());
                    }
                    let (workspaces, ws_rev) = core.workspace_snapshot();
                    if ws_rev != *last_ws_rev.borrow() {
                        *last_ws_rev.borrow_mut() = ws_rev;
                        set_workspaces.call(workspaces);
                    }
                    // current_workspace 可能被 tabs 侧（新建归属）或外部修改，
                    // 轮询对齐，避免两侧状态分叉（值变化才 set）。
                    let cur = core.current_workspace();
                    let prev = last_cur_ws.borrow().clone();
                    if cur != prev {
                        *last_cur_ws.borrow_mut() = cur.clone();
                        set_current_ws.call(cur);
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

    // ── 分组头：工作区 + 添加按钮（SecondaryText 主题色令牌）──────
    let group_label: Element = {
        let label: Element = text_block("工作区")
            .font_size(12.0)
            .foreground(ThemeRef::SecondaryText)
            .into();
        // ➕ 添加 workspace：目录选择对话框 → `workspace.create`。
        let add = icon_button(Icon::symbol(Symbol::Add), {
            let bridge = bridge.clone();
            move || match bridge.pick_workspace_directory() {
                // 取消 → Ok(null) → 不动作；选择 → 注册为工作区。
                Ok(serde_json::Value::String(path)) => {
                    bridge.spawn_workspace_create(path);
                }
                Ok(_) => {}
                Err(err) => log_diag(&format!("pick workspace directory failed: {err}")),
            }
        })
        .vertical_alignment(VerticalAlignment::Center);
        grid((label.grid_column(0), add.grid_column(1)))
            .columns([GridLength::STAR, GridLength::Auto])
            .margin(Thickness::xy(8.0, 2.0))
            .into()
    };

    // ── 三段列表：Workspace 树 / 未分组 / 归档 ─────────────────────
    // 分区：非归档会话按 workspace_id 归类；workspace 树由 daemon 账户驱动。
    let (active_items, archived_items): (Vec<SessionItem>, Vec<SessionItem>) =
        items.iter().cloned().partition(|s| !s.archived);

    // 会话归属查找：按 workspace_id 索引（与 daemon `session.list.workspace_id`
    // 同源，workspace.session_ids 账户不在此处二次校验——展示层以列表为准）。
    let ws_sessions = |ws_id: &str| -> Vec<SessionItem> {
        active_items
            .iter()
            .filter(|s| s.workspace_id.as_deref() == Some(ws_id))
            .cloned()
            .collect()
    };
    let ungrouped_items: Vec<SessionItem> = active_items
        .iter()
        .filter(|s| s.workspace_id.is_none())
        .cloned()
        .collect();

    let mut rows: Vec<Element> = Vec::new();
    for ws in &workspaces {
        let ws_items = ws_sessions(&ws.id);
        let ws_row = workspace_row(
            ws,
            current_ws.as_deref() == Some(ws.id.as_str()),
            ws_items.len(),
            bridge.clone(),
            set_ws_confirm.clone(),
            set_ws_rename.clone(),
        );
        let ws_list = list_view(ws_items, {
            let bridge = bridge.clone();
            let set_confirm = set_confirm_seed.clone();
            let active = active.clone();
            move |item, _| {
                workspace_session_row(
                    item,
                    item.seed == active,
                    bridge.clone(),
                    set_confirm.clone(),
                    None,
                )
            }
        })
        .with_key_selector(|item| item.seed.clone())
        .selection_mode(SelectionMode::None)
        .build();
        rows.push(ws_row);
        rows.push(ws_list);
    }
    let ungrouped_row_el = ungrouped_row(
        ungrouped_items.len(),
        current_ws.is_none(),
        bridge.clone(),
    );
    let ungrouped_list = list_view(ungrouped_items, {
        let bridge = bridge.clone();
        let set_confirm = set_confirm_seed.clone();
        let active = active.clone();
        let target = current_ws.clone();
        move |item, _| {
            workspace_session_row(
                item,
                item.seed == active,
                bridge.clone(),
                set_confirm.clone(),
                target.clone(),
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
    rows.push(ungrouped_row_el);
    rows.push(ungrouped_list);
    rows.push(archived_label);
    rows.push(list_archived);
    let session_list: Element = scroll_viewer(vstack(rows)).into();

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
        // 版本标识小字（与安装包 BuildId 同源；单行省略，tooltip 全量）。
        // ⚠ 不能写 `build_label.borrow().clone().unwrap_or_else(|| …borrow_mut()…)`：
        // Ref 临时值存活到整个语句末尾，闭包内的 borrow_mut 会触发
        // 「RefCell already borrowed」panic（rc.3 白屏根因，on_fault[render]）。
        // 必须先 clone 出 owned 值、让 Ref 在语句末 drop，再分支写入缓存。
        let cached_label = build_label.borrow().clone();
        let label = match cached_label {
            Some(s) => s,
            None => {
                let s = build_id_label();
                *build_label.borrow_mut() = Some(s.clone());
                s
            }
        };
        let version: Element = text_block(&label)
            .font_size(10.0)
            .foreground(ThemeRef::TertiaryText)
            .max_lines(1)
            .text_trimming(TextTrimming::CharacterEllipsis)
            .tooltip(format!(
                "构建标识：{label}\n格式：<版本>-<commit12>-<UTC yyyyMMddHHmmss>"
            ))
            .automation_name("构建版本")
            .into();
        vstack((sp, version)).spacing(4.0).margin(12.0).into()
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

    // ── 彻底删除会话确认（phantom child 覆盖层：同 cell 重叠渲染）──
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

    // ── 删除 workspace 确认（会话保留，变为未分组）────────────────
    let ws_delete_dialog: Element = match ws_confirm.clone() {
        Some(id) => {
            let bridge = bridge.clone();
            let set_ws_confirm = set_ws_confirm.clone();
            ContentDialog::new("删除工作区")
                .content("删除该工作区分组。其下会话不会被删除，将移入「未分组」。")
                .primary_button_text("删除")
                .close_button_text("取消")
                .is_open(true)
                .on_closed(move |result: ContentDialogResult| {
                    if result == ContentDialogResult::Primary {
                        bridge.spawn_workspace_delete(id.clone());
                    }
                    set_ws_confirm.call(None);
                })
                .into()
        }
        None => grid(()).into(),
    };

    // ── 重命名 workspace（自定义浮层：居中卡片 + TextBox 输入）────
    // ContentDialog.content 仅接受文本，故用 phantom overlay 实现
    // （与确认对话框同 cell 重叠渲染机制）。
    let ws_rename_dialog: Element = match ws_rename.clone() {
        Some((id, current_title)) => {
            let bridge = bridge.clone();
            let set_ws_rename = set_ws_rename.clone();
            let input_ref = ws_rename_input.clone();
            // 每次打开以当前标题初始化输入。
            *input_ref.borrow_mut() = current_title.clone();
            let input: Element = text_box(input_ref.borrow().clone())
                .on_text_changed({
                    let input_ref = input_ref.clone();
                    move |v| *input_ref.borrow_mut() = v
                })
                .width(280.0)
                .into();
            let confirm = button("确定").on_click({
                let bridge = bridge.clone();
                let set_ws_rename = set_ws_rename.clone();
                let input_ref = input_ref.clone();
                move || {
                    let title = input_ref.borrow().trim().to_string();
                    if !title.is_empty() {
                        bridge.spawn_workspace_rename(id.clone(), title);
                    }
                    set_ws_rename.call(None);
                }
            });
            let cancel = button("取消").on_click({
                let set_ws_rename = set_ws_rename.clone();
                move || set_ws_rename.call(None)
            });
            let card: Element = {
                let title_el: Element = text_block("重命名工作区")
                    .semibold()
                    .font_size(14.0)
                    .into();
                let buttons: Element = hstack((cancel, confirm))
                    .spacing(8.0)
                    .horizontal_alignment(HorizontalAlignment::Right)
                    .into();
                border(vstack((title_el, input, buttons)).spacing(12.0))
                    .corner_radius(8.0)
                    .background(ThemeRef::CardBackground)
                    .border_brush(ThemeRef::CardStroke)
                    .border_thickness(Thickness::xy(1.0, 1.0))
                    .padding(Thickness::xy(16.0, 16.0))
                    .into()
            };
            grid((card,))
                .columns([GridLength::STAR])
                .rows([GridLength::STAR])
                .horizontal_alignment(HorizontalAlignment::Center)
                .vertical_alignment(VerticalAlignment::Center)
                .into()
        }
        None => grid(()).into(),
    };

    // ── 根容器（无事件：指针已由 splitter 捕获）────────────────────
    grid((
        grid((content.grid_column(0), splitter.grid_column(1)))
            .columns([GridLength::STAR, GridLength::Pixel(12.0)])
            .rows([GridLength::STAR])
            .grid_row(0)
            .grid_column(0),
        dialog.grid_row(0).grid_column(0),
        ws_delete_dialog.grid_row(0).grid_column(0),
        ws_rename_dialog.grid_row(0).grid_column(0),
    ))
    .rows([GridLength::STAR])
    .into()
}
