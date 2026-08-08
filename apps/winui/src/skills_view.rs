//! XAML 原生技能页 — 壳主导视图族第一步（WORKFLOW §8）。
//!
//! 数据源：`bridge.skills_snapshot()`——Ringing `skills_updated` 事件完整载荷
//! （事件即权威快照，`shell_store::parse_skills_event`）；首次进入且无缓存时
//! `bridge.ensure_skills()` 兜底拉 bootstrap `control.skills`。
//!
//! 交互（对齐 Web `SkillsView`）：
//!   - 四列分组：目录(catalog) / 请求中(requested) / 已启用(active) / 不可用(unavailable)；
//!   - 搜索过滤：名称/描述，不区分大小写；
//!   - 动作：catalog→ToggleSwitch 开=request；active→ToggleSwitch 关=release；
//!     requested→取消(release)；unavailable→重试(request)；头部刷新=reload；
//!   - pending 防重入：动作发起后 8s 内该行禁用（目标态到达提前解除——
//!     渲染期按当前快照判断，无需事件驱动复位）；
//!   - 行点击展开详情（source path / 加载 error）。
//!
//! 状态管理约定（reactor 无 SetState::get）：可变交互态（pending 时刻表、
//! refreshing）放 `use_ref`，渲染期直接读取；快照/搜索/展开等渲染驱动态
//! 放 `use_state`（快照由 500ms rev 比对轮询驱动，同 sidebar）。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows_reactor::*;

use crate::bridge::Bridge;
use crate::shell_store::SkillsSnapshot;

/// 快照轮询间隔（同 sidebar 500ms rev 比对模式）。
const POLL_INTERVAL: Duration = Duration::from_millis(500);
/// pending 动作 / 刷新 8s 超时兜底（Web 版 window.setTimeout 同语义）。
const PENDING_TIMEOUT: Duration = Duration::from_secs(8);

/// 分组列状态（对齐 Web `ColumnState`）。
const COLUMNS: [&str; 4] = ["catalog", "requested", "active", "unavailable"];

/// 渲染条目：runtime 合并 catalog 元数据（scope/path）。
#[derive(Debug, Clone)]
struct ViewSkill {
    name: String,
    description: String,
    state: String,
    scope: String,
    path: String,
    token_count: u64,
    error: Option<String>,
}

fn column_label(state: &str) -> &'static str {
    match state {
        "catalog" => "目录",
        "requested" => "请求中",
        "active" => "已启用",
        "unavailable" => "不可用",
        _ => "其他",
    }
}

/// 技能页主体（放入内容区 Grid 第 (0,1) cell；由 main.rs 内容区四行视图族
/// 行高切换控制显隐，非当前视图零命中零渲染）。
pub fn skills_view(cx: &mut RenderCx, bridge: Arc<Bridge>) -> Element {
    let (snapshot, set_snapshot) = cx.use_state::<Option<SkillsSnapshot>>(None);
    let (search, set_search) = cx.use_state::<String>(String::new());
    let (expanded, set_expanded) = cx.use_state::<HashSet<String>>(HashSet::new());
    // 动作防重入：name → 发起时刻（渲染期判断超时/目标态，无需事件复位）。
    let pending_at = cx.use_ref::<HashMap<String, Instant>>(HashMap::new());
    // 期望目标态（request/retain → 进入 requested/active；release → 回 catalog）。
    let pending_targets = cx.use_ref::<HashMap<String, bool>>(HashMap::new());
    // 刷新进行中（8s 兜底复位 + 强制重渲染）。
    let refreshing = cx.use_ref::<bool>(false);
    let reloaded_at = cx.use_ref::<Instant>(Instant::now());
    let timer = cx.use_ref::<Option<DispatcherTimer>>(None);
    let last_rev = cx.use_ref::<u64>(0);

    // ── 轮询：rev 比对刷新快照 + pending/refreshing 超时兜底 ────────
    cx.use_effect((), {
        let bridge = bridge.clone();
        let set_snapshot = set_snapshot.clone();
        let pending_at = pending_at.clone();
        let refreshing = refreshing.clone();
        let reloaded_at = reloaded_at.clone();
        let timer = timer.clone();
        let last_rev = last_rev.clone();
        move || {
            let core = bridge.core();
            // 首次进入：兜底拉权威快照（事件路径未覆盖时）。
            core.ensure_skills();
            *last_rev.borrow_mut() = core.skills_snapshot().1;
            if let Ok(t) = DispatcherTimer::new(POLL_INTERVAL, {
                let core = core.clone();
                let set_snapshot = set_snapshot.clone();
                let pending_at = pending_at.clone();
                let refreshing = refreshing.clone();
                let reloaded_at = reloaded_at.clone();
                let last_rev = last_rev.clone();
                move || {
                    let now = Instant::now();
                    // pending 超时清理（目标态到达由渲染期判断解除）。
                    pending_at
                        .borrow_mut()
                        .retain(|_, at| now.duration_since(*at) < PENDING_TIMEOUT);
                    // 刷新 8s 兜底复位；无事件时强制重渲染一次刷新按钮。
                    if *refreshing.borrow()
                        && now.duration_since(*reloaded_at.borrow()) >= PENDING_TIMEOUT
                    {
                        *refreshing.borrow_mut() = false;
                        set_snapshot.call(core.skills_snapshot().0);
                    }
                    let (snap, rev) = core.skills_snapshot();
                    if rev != *last_rev.borrow() {
                        *last_rev.borrow_mut() = rev;
                        set_snapshot.call(snap);
                    }
                }
            }) {
                *timer.borrow_mut() = Some(t);
            }
        }
    });

    // ── 派生：过滤 + 四列分组 ──────────────────────────────────────
    let q = search.trim().to_lowercase();
    let mut columns: HashMap<&str, Vec<ViewSkill>> = HashMap::new();
    if let Some(snap) = &snapshot {
        let meta: HashMap<&str, (&str, &str)> = snap
            .available
            .iter()
            .map(|a| (a.name.as_str(), (a.scope.as_str(), a.source.as_str())))
            .collect();
        for item in &snap.runtime {
            let (scope, path) = meta
                .get(item.name.as_str())
                .copied()
                .unwrap_or(("project", ""));
            let desc = if item.description.is_empty() {
                // 目录元数据缺失时用 runtime 描述兜底（一般同源）。
                item.description.as_str()
            } else {
                item.description.as_str()
            };
            if !q.is_empty()
                && !item.name.to_lowercase().contains(&q)
                && !desc.to_lowercase().contains(&q)
            {
                continue;
            }
            columns
                .entry(item.state.as_str())
                .or_default()
                .push(ViewSkill {
                    name: item.name.clone(),
                    description: desc.to_string(),
                    state: item.state.clone(),
                    scope: scope.to_string(),
                    path: path.to_string(),
                    token_count: item.token_count,
                    error: item.error.clone(),
                });
        }
    }
    let has_session = snapshot
        .as_ref()
        .map(|s| !s.seed.is_empty())
        .unwrap_or(false);
    let has_items = columns.values().any(|v| !v.is_empty());

    // ── 动作闭包（渲染期重建；捕获当次渲染的值）────────────────────
    let reload = {
        let core = bridge.core();
        let refreshing = refreshing.clone();
        let reloaded_at = reloaded_at.clone();
        move || {
            if *refreshing.borrow() {
                return;
            }
            *refreshing.borrow_mut() = true;
            *reloaded_at.borrow_mut() = Instant::now();
            core.spawn_skill_reload();
        }
    };

    // ── 头部：标题 + 摘要 + 搜索 + 刷新 ────────────────────────────
    let header: Element = {
        let (workspace, rev8, epoch, used, budget) = match &snapshot {
            Some(s) => (
                s.seed.chars().take(8).collect::<String>(),
                if s.catalog_revision.is_empty() {
                    "-".to_string()
                } else {
                    s.catalog_revision.chars().take(8).collect()
                },
                s.context_epoch,
                s.token_usage,
                s.token_budget,
            ),
            None => ("-".to_string(), "-".to_string(), 0, 0, 0),
        };
        let title: Element = text_block("技能")
            .font_size(28.0)
            .semibold()
            .vertical_alignment(VerticalAlignment::Center)
            .into();
        let meta: Element = hstack((
            text_block(&workspace)
                .font_size(12.0)
                .foreground(ThemeRef::SecondaryText),
            text_block(format!("目录 {rev8}"))
                .font_size(12.0)
                .foreground(ThemeRef::SecondaryText),
            text_block(format!("epoch {epoch}"))
                .font_size(12.0)
                .foreground(ThemeRef::SecondaryText),
            text_block(format!("{used}/{budget} tokens"))
                .font_size(12.0)
                .foreground(ThemeRef::SecondaryText),
        ))
        .spacing(12.0)
        .into();
        let search_box: Element = text_box(search.clone())
            .placeholder_text("搜索技能…")
            .width(220.0)
            .on_text_changed(set_search)
            .vertical_alignment(VerticalAlignment::Center)
            .into();
        let refresh_btn: Element = button(if *refreshing.borrow() {
            "…"
        } else {
            "刷新"
        })
        .subtle()
        .enabled(!*refreshing.borrow())
        .on_click({
            let reload = reload.clone();
            move || reload()
        })
        .vertical_alignment(VerticalAlignment::Center)
        .into();
        let actions: Element = hstack((search_box, refresh_btn)).spacing(8.0).into();
        let left: Element = vstack((title, meta)).spacing(4.0).into();
        let row: Element = hstack((left, actions))
            .spacing(12.0)
            .horizontal_alignment(HorizontalAlignment::Stretch)
            .into();
        grid((row,)).padding(Thickness::xy(20.0, 16.0)).into()
    };
    // ── 主体：四列分组 ─────────────────────────────────────────────
    let body: Element = if !has_session {
        let el: Element = text_block("请先选择或新建一个会话")
            .foreground(ThemeRef::SecondaryText)
            .into();
        el.margin(Thickness::xy(20.0, 24.0))
    } else if !has_items {
        let el: Element = text_block(if q.is_empty() {
            "暂无技能"
        } else {
            "没有匹配的技能"
        })
        .foreground(ThemeRef::SecondaryText)
        .into();
        el.margin(Thickness::xy(20.0, 24.0))
    } else {
        let mut column_els: Vec<Element> = Vec::new();
        for (i, state) in COLUMNS.iter().enumerate() {
            let items = columns.get(state).cloned().unwrap_or_default();
            let header_el: Element =
                hstack((
                    text_block(format!("{} ({})", column_label(state), items.len()))
                        .font_size(14.0)
                        .semibold(),
                ))
                .margin(Thickness::xy(12.0, 8.0))
                .into();
            let list_el: Element = if items.is_empty() {
                let el: Element = text_block("—")
                    .font_size(12.0)
                    .foreground(ThemeRef::SecondaryText)
                    .into();
                el.margin(Thickness::xy(12.0, 4.0))
            } else {
                let cards: Vec<Element> = items
                    .iter()
                    .map(|item| {
                        build_card(
                            item,
                            &expanded,
                            &pending_at,
                            &pending_targets,
                            &bridge,
                            &set_expanded,
                        )
                    })
                    .collect();
                scroll_viewer(vstack(cards).spacing(6.0)).into()
            };
            let col: Element = grid((header_el.grid_row(0), list_el.grid_row(1)))
                .rows([GridLength::Auto, GridLength::STAR])
                .grid_column(i as i32)
                .into();
            column_els.push(col);
        }
        grid(column_els)
            .columns([
                GridLength::STAR,
                GridLength::STAR,
                GridLength::STAR,
                GridLength::STAR,
            ])
            .column_spacing(8.0)
            .padding(Thickness::xy(12.0, 0.0))
            .into()
    };

    // ── 根：头部 + 主体 ────────────────────────────────────────────
    grid((header.grid_row(0), body.grid_row(1)))
        .rows([GridLength::Auto, GridLength::STAR])
        .into()
}

/// 单张技能卡片：名称 + scope 徽章 + 描述 + token 数 + 动作控件 + 展开详情。
///
/// 动作闭包在卡片内部构建（捕获 bridge/pending refs），避免跨函数传递
/// 非 'static 的 `&dyn Fn` 引用（reactor 回调要求 'static）。
fn build_card(
    item: &ViewSkill,
    expanded: &HashSet<String>,
    pending_at: &HookRef<HashMap<String, Instant>>,
    pending_targets: &HookRef<HashMap<String, bool>>,
    bridge: &Arc<Bridge>,
    set_expanded: &SetState<HashSet<String>>,
) -> Element {
    let operate = {
        let bridge = bridge.clone();
        let pending_at = pending_at.clone();
        let pending_targets = pending_targets.clone();
        std::sync::Arc::new(move |action: &'static str, name: String| {
            if pending_at.borrow().contains_key(&name) {
                return; // 防重入（8s 内）
            }
            pending_at.borrow_mut().insert(name.clone(), Instant::now());
            pending_targets
                .borrow_mut()
                .insert(name.clone(), action == "request" || action == "retain");
            bridge.core().spawn_skill_operation(action, &name);
        })
    };
    let operate: std::sync::Arc<dyn Fn(&'static str, String) + 'static> = operate;
    let name = item.name.clone();
    let is_expanded = expanded.contains(&name);
    // pending：8s 内且目标态未到达（目标态到达即提前解除，无需事件复位）。
    let is_pending = pending_at
        .borrow()
        .get(&name)
        .map(|at| {
            let target = pending_targets
                .borrow()
                .get(&name)
                .copied()
                .unwrap_or(false);
            let resolved = match item.state.as_str() {
                "requested" | "active" => target,
                "catalog" | "unavailable" => !target,
                _ => false,
            };
            at.elapsed() < PENDING_TIMEOUT && !resolved
        })
        .unwrap_or(false);

    // ── 动作控件（按状态；pending 时转圈）──
    let action_el: Element = if is_pending {
        ProgressRing::indeterminate()
            .width(20.0)
            .height(20.0)
            .vertical_alignment(VerticalAlignment::Center)
            .into()
    } else {
        match item.state.as_str() {
            "catalog" => ToggleSwitch::new(false)
                .on_toggled({
                    let operate = operate.clone();
                    let name = name.clone();
                    move |on: bool| {
                        if on {
                            operate("request", name.clone())
                        }
                    }
                })
                .vertical_alignment(VerticalAlignment::Center)
                .into(),
            "active" => ToggleSwitch::new(true)
                .on_toggled({
                    let operate = operate.clone();
                    let name = name.clone();
                    move |on: bool| {
                        if !on {
                            operate("release", name.clone())
                        }
                    }
                })
                .vertical_alignment(VerticalAlignment::Center)
                .into(),
            "requested" => button("取消")
                .subtle()
                .on_click({
                    let operate = operate.clone();
                    let name = name.clone();
                    move || operate("release", name.clone())
                })
                .vertical_alignment(VerticalAlignment::Center)
                .into(),
            _ => button("重试")
                .subtle()
                .on_click({
                    let operate = operate.clone();
                    let name = name.clone();
                    move || operate("request", name.clone())
                })
                .vertical_alignment(VerticalAlignment::Center)
                .into(),
        }
    };

    // ── 主行：名称 + scope 徽章 + 描述 + token（点击展开）──
    let name_el: Element = text_block(&item.name)
        .font_size(14.0)
        .semibold()
        .text_trimming(TextTrimming::CharacterEllipsis)
        .into();
    let scope_el: Element = text_block(if item.scope == "user" {
        "用户"
    } else {
        "项目"
    })
    .font_size(11.0)
    .foreground(ThemeRef::AccentText)
    .into();
    let desc_el: Element = text_block(&item.description)
        .font_size(12.0)
        .foreground(ThemeRef::SecondaryText)
        .text_trimming(TextTrimming::CharacterEllipsis)
        .into();
    let token_el: Element = text_block(format!("{}t", item.token_count))
        .font_size(11.0)
        .foreground(ThemeRef::SecondaryText)
        .vertical_alignment(VerticalAlignment::Center)
        .into();
    let left: Element = vstack((hstack((name_el, scope_el)).spacing(6.0), desc_el))
        .spacing(2.0)
        .into();
    let main_row: Element = grid((
        left.grid_column(0),
        token_el.grid_column(1),
        action_el.grid_column(2),
    ))
    .columns([GridLength::STAR, GridLength::Auto, GridLength::Auto])
    .column_spacing(8.0)
    .on_pointer_pressed({
        let set_expanded = set_expanded.clone();
        let name = name.clone();
        let expanded = expanded.clone();
        move |_| {
            let mut next = expanded.clone();
            if next.contains(&name) {
                next.remove(&name);
            } else {
                next.insert(name.clone());
            }
            set_expanded.call(next);
        }
    })
    .into();

    // ── 详情行（展开时）：路径 + 加载错误 ──
    let detail_el: Element = if is_expanded {
        let path_el: Element = text_block(&item.path)
            .font_size(11.0)
            .foreground(ThemeRef::SecondaryText)
            .text_trimming(TextTrimming::CharacterEllipsis)
            .into();
        let mut rows: Vec<Element> = vec![path_el];
        if let Some(err) = &item.error {
            rows.push(
                text_block(err)
                    .font_size(11.0)
                    .foreground(ThemeRef::SystemCritical)
                    .into(),
            );
        }
        vstack(rows)
            .spacing(2.0)
            .margin(Thickness::xy(12.0, 4.0))
            .into()
    } else {
        text_block("").into()
    };

    border(vstack((main_row, detail_el)).spacing(2.0))
        .background(ThemeRef::LayerFill)
        .corner_radius(8.0)
        .padding(Thickness::xy(12.0, 8.0))
        .into()
}
