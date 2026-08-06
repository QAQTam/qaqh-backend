//! XAML 原生首页（P1）— StartupView 的壳侧承载。
//!
//! 数据源：`bridge.core().session_snapshot()`——`session.list` + `session.activity`
//! 投影（与侧栏同源，shell_store::SessionItem）；500ms rev 比对轮询（同
//! sidebar / skills_view 模式）。
//!
//! 布局（对齐 Web `StartupView`）：
//!   scroll_viewer(
//!     hero（DeepX 品牌 + 副标题 + DevTools 按钮）
//!     输入区（text_box 单行 + 发送按钮 → `spawn_send_new_session`）
//!     活动热力图（近 30 天，`updated_at` 天粒度计数，5 级色阶）
//!     会话卡片网格（最近 12 个，点击 → `spawn_resume`）
//!   )
//!
//! 交互偏差（reactor 能力边界）：Web 版 textarea Enter 提交/自动增高——
//! reactor text_box 无键盘事件 API，改为发送按钮提交（行为等价，按键差异
//! 记录于 WORKFLOW 偏差表）；热力图色阶用 Fluent 语义色令牌近似。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use windows_reactor::*;

use crate::bridge::Bridge;
use crate::shell_store::SessionItem;

/// 快照轮询间隔（同 sidebar / skills_view）。
const POLL_INTERVAL: Duration = Duration::from_millis(500);
/// 首页展示的会话卡片数（Web `sessions.slice(0, 12)`）。
const CARD_LIMIT: usize = 12;
/// 热力图天数（Web `lastNDays(30)`）。
const HEATMAP_DAYS: i64 = 30;

// ── 日期工具（无 chrono 依赖；Howard Hinnant civil_from_days 算法）──

/// epoch 秒 → UTC (年, 月, 日)。
fn civil_from_epoch(secs: u64) -> (i64, u32, u32) {
    let z = (secs as i64) / 86_400;
    // Howard Hinnant: civil_from_days
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// UTC 日期键 `YYYY-MM-DD`。
fn day_key(secs: u64) -> String {
    let (y, m, d) = civil_from_epoch(secs);
    format!("{y:04}-{m:02}-{d:02}")
}

/// 近 N 天日期键列表（含今天，旧 → 新）。
fn last_n_days(n: i64) -> Vec<String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let today = now / 86_400 * 86_400; // 当天 00:00 UTC
    (0..n as u64)
        .rev()
        .map(|i| day_key(today.saturating_sub(i * 86_400)))
        .collect()
}

/// 热力等级（对齐 Web `levelClass`：0/1-3/4-8/9-20/21+）。
fn heat_level(count: u64) -> u8 {
    match count {
        0 => 0,
        1..=3 => 1,
        4..=8 => 2,
        9..=20 => 3,
        _ => 4,
    }
}

/// 相对时间（对齐 Web `SessionCard.formatDate`；中文简化）。
fn relative_time(updated_at: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let diff = now.saturating_sub(updated_at);
    if diff < 3600 {
        format!("{}分钟前", (diff / 60).max(1))
    } else if diff < 86_400 {
        format!("{}小时前", diff / 3600)
    } else {
        format!("{}天前", diff / 86_400)
    }
}

/// 热力色阶 → Fluent 语义色（hm-l0..l4 近似）。
fn heat_color(level: u8) -> ThemeRef {
    match level {
        0 => ThemeRef::LayerFill,
        1 => ThemeRef::SubtleFill,
        2 => ThemeRef::AccentSecondary,
        3 => ThemeRef::Accent,
        _ => ThemeRef::SystemSuccess,
    }
}

/// 单张会话卡片：状态点 + 标题 + 相对时间 + running 徽章。
fn session_card(item: &SessionItem, bridge: &Arc<Bridge>) -> Element {
    let seed = item.seed.clone();
    let dot: Element = border(text_block(""))
        .width(8.0)
        .height(8.0)
        .corner_radius(4.0)
        .background(match item.state {
            crate::shell_store::ActivityState::Working => ThemeRef::SystemSuccess,
            crate::shell_store::ActivityState::WaitingUser => ThemeRef::Accent,
            crate::shell_store::ActivityState::Starting => ThemeRef::SystemAttention,
            crate::shell_store::ActivityState::Disconnected => ThemeRef::SystemCritical,
            crate::shell_store::ActivityState::Idle => ThemeRef::SystemNeutral,
        })
        .vertical_alignment(VerticalAlignment::Center)
        .into();
    let title: Element = text_block(&item.title)
        .font_size(14.0)
        .semibold()
        .trim_ellipsis()
        .into();
    let meta: Element = text_block(relative_time(item.updated_at))
        .font_size(11.0)
        .foreground(ThemeRef::SecondaryText)
        .into();
    let mut rows: Vec<Element> = vec![
        hstack((dot, title)).spacing(6.0).into(),
        meta,
    ];
    if item.running {
        rows.push(
            text_block("运行中")
                .font_size(10.0)
                .foreground(ThemeRef::AccentText)
                .into(),
        );
    }
    border(vstack(rows).spacing(4.0))
        .background(ThemeRef::LayerFill)
        .corner_radius(8.0)
        .padding(Thickness::xy(12.0, 10.0))
        .on_pointer_pressed({
            let bridge = bridge.clone();
            let seed = seed.clone();
            move |_| bridge.spawn_resume(&seed)
        })
        // 新卡片出现时淡入（ImplicitShowAnimation；会话列表刷新新增行时触发）。
        .transition(
            Some(AnimationConfig::fade_in(Duration::from_millis(200))),
            None,
        )
        .into()
}

/// 首页主体（放入内容区 Grid；由 main.rs 按 `current_view == "home"` 切换）。
pub fn home_view(cx: &mut RenderCx, bridge: Arc<Bridge>) -> Element {
    let (items, set_items) = cx.use_state::<Vec<SessionItem>>(Vec::new());
    let (text, set_text) = cx.use_state::<String>(String::new());
    let timer = cx.use_ref::<Option<DispatcherTimer>>(None);
    let last_rev = cx.use_ref::<u64>(0);

    // 首次挂载：初始刷新 + 500ms rev 轮询（同 sidebar 模式）。
    cx.use_effect((), {
        let bridge = bridge.clone();
        let set_items = set_items.clone();
        let timer = timer.clone();
        let last_rev = last_rev.clone();
        move || {
            let core = bridge.core();
            bridge.spawn_refresh_sessions();
            *last_rev.borrow_mut() = core.session_snapshot().1;
            if let Ok(t) = DispatcherTimer::new(POLL_INTERVAL, {
                let core = core.clone();
                let set_items = set_items.clone();
                let last_rev = last_rev.clone();
                move || {
                    let (items, rev) = core.session_snapshot();
                    if rev != *last_rev.borrow() {
                        *last_rev.borrow_mut() = rev;
                        set_items.call(items);
                    }
                }
            }) {
                *timer.borrow_mut() = Some(t);
            }
        }
    });

    // ── 热力图数据：近 30 天 updated_at 计数 ────────────────────────
    let days = last_n_days(HEATMAP_DAYS);
    let mut counts: HashMap<String, u64> = HashMap::new();
    for item in &items {
        *counts.entry(day_key(item.updated_at)).or_insert(0) += 1;
    }

    // ── hero：品牌 + 副标题 + DevTools ──────────────────────────────
    let hero: Element = {
        let logo: Element = text_block(">_")
            .font_size(20.0)
            .semibold()
            .foreground(ThemeRef::AccentText)
            .vertical_alignment(VerticalAlignment::Center)
            .into();
        let title_el: Element = text_block("DeepX")
            .font_size(28.0)
            .semibold()
            .vertical_alignment(VerticalAlignment::Center)
            .into();
        let subtitle: Element = text_block("原生桌面壳 · XAML 视图族")
            .font_size(13.0)
            .foreground(ThemeRef::SecondaryText)
            .vertical_alignment(VerticalAlignment::Center)
            .into();
        let devtools: Element = button("</> DevTools")
            .subtle()
            .icon(Icon::symbol(Symbol::Repair))
            .on_click({
                let bridge = bridge.clone();
                move || {
                    bridge.open_devtools();
                }
            })
            .vertical_alignment(VerticalAlignment::Center)
            .into();
        let brand: Element = hstack((logo, title_el)).spacing(8.0).into();
        let left: Element = vstack((brand, subtitle)).spacing(2.0).into();
        hstack((left, devtools))
            .spacing(12.0)
            .horizontal_alignment(HorizontalAlignment::Stretch)
            .into()
    };

    // ── 输入区：text_box + 发送（→ 新建会话并发送）────────────────
    let send = {
        let bridge = bridge.clone();
        let text = text.clone();
        let set_text = set_text.clone();
        move || {
            let t = text.trim().to_string();
            if t.is_empty() {
                return;
            }
            set_text.call(String::new());
            bridge.spawn_send_new_session(&t);
        }
    };
    let composer: Element = {
        let input: Element = text_box(text.clone())
            .placeholder_text("输入消息，回车开始新任务…")
            .on_text_changed(set_text.clone())
            .vertical_alignment(VerticalAlignment::Center)
            .into();
        let btn: Element = button("发送")
            .accent()
            .enabled(!text.trim().is_empty())
            .on_click({
                let send = send.clone();
                move || send()
            })
            .vertical_alignment(VerticalAlignment::Center)
            .into();
        hstack((input, btn))
            .spacing(8.0)
            .horizontal_alignment(HorizontalAlignment::Stretch)
            .into()
    };

    // ── 热力图卡片（30 色块 + 图例）────────────────────────────────
    let heatmap: Element = {
        let cells: Vec<Element> = days
            .iter()
            .enumerate()
            .map(|(i, day)| {
                let level = heat_level(*counts.get(day).unwrap_or(&0));
                border(text_block(""))
                    .height(14.0)
                    .corner_radius(3.0)
                    .background(heat_color(level))
                    .grid_column((i % 10) as i32)
                    .grid_row((i / 10) as i32)
                    .into()
            })
            .collect();
        let legend: Element = hstack((
            text_block("少").font_size(11.0).foreground(ThemeRef::SecondaryText),
            border(text_block("")).width(10.0).height(10.0).corner_radius(2.0).background(heat_color(0)),
            border(text_block("")).width(10.0).height(10.0).corner_radius(2.0).background(heat_color(1)),
            border(text_block("")).width(10.0).height(10.0).corner_radius(2.0).background(heat_color(2)),
            border(text_block("")).width(10.0).height(10.0).corner_radius(2.0).background(heat_color(3)),
            border(text_block("")).width(10.0).height(10.0).corner_radius(2.0).background(heat_color(4)),
            text_block("多").font_size(11.0).foreground(ThemeRef::SecondaryText),
        ))
        .spacing(4.0)
        .into();
        let grid_el: Element = grid(cells)
            .rows((0..3).map(|_| GridLength::Pixel(14.0)))
            .columns((0..10).map(|_| GridLength::STAR))
            .column_spacing(4.0)
            .row_spacing(4.0)
            .into();
        border(vstack((
            hstack((
                text_block("活动").font_size(13.0).semibold(),
                text_block(format!("{} 个任务", items.len()))
                    .font_size(11.0)
                    .foreground(ThemeRef::SecondaryText),
            ))
            .spacing(8.0),
            grid_el,
            legend,
        ))
        .spacing(8.0))
        .background(ThemeRef::LayerFill)
        .corner_radius(8.0)
        .padding(Thickness::xy(16.0, 12.0))
        .into()
    };

    // ── 会话卡片网格（最近 12 个，4 列）────────────────────────────
    let mut cards: Vec<SessionItem> = items.clone();
    cards.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    cards.truncate(CARD_LIMIT);
    let sessions_section: Element = if cards.is_empty() {
        text_block("还没有任务，输入消息开始第一个")
            .foreground(ThemeRef::SecondaryText)
            .margin(Thickness::xy(4.0, 16.0))
            .with_key("sessions")
            .into()
    } else {
        let card_els: Vec<Element> = cards
            .iter()
            .enumerate()
            .map(|(i, item)| session_card(item, &bridge).grid_column((i % 4) as i32).grid_row((i / 4) as i32))
            .collect();
        // key："sessions" 固定——空/非空两种结构（TextBlock↔vstack）之间
        // 切换时 key 相同但 kind 不同 → keyed reconcile 干净重建，杜绝
        // 同 index 类型跳变的控件复用错位（settings nav 同款防护）。
        vstack((
            text_block(format!("最近任务 · {}", cards.len()))
                .font_size(13.0)
                .semibold(),
            grid(card_els)
                .rows((0..3usize).map(|_| GridLength::Auto))
                .columns((0..4).map(|_| GridLength::STAR))
                .column_spacing(8.0)
                .row_spacing(8.0),
        ))
        .spacing(8.0)
        .with_key("sessions")
        .into()
    };

    // ── 根：scroll_viewer(hero / composer / heatmap / sessions) ────
    let content: Element = vstack((hero, composer, heatmap, sessions_section))
        .spacing(16.0)
        .padding(Thickness::xy(32.0, 24.0))
        .into();
    scroll_viewer(content).into()
}
