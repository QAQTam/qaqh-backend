//! XAML 原生 Info 面板（P4a）— Web `InfoPopover` 的壳侧承载（第一版：用量核心区块）。
//!
//! 数据源：`bridge.core().info_snapshot()`——`client.bootstrap` 的
//! `conversation.state` 投影（`shell_store::SessionDetail`）；500ms rev 比对
//! 轮询（同 sidebar / home_view 模式）。打开时机：轮询 `header_snapshot()`
//! 的 `info_open`（Web `shell.setHeader` 投影，与标题栏按钮 active 同源）——
//! 翻转打开瞬间触发 `spawn_refresh_info(active_seed)`；面板打开期间 control
//! 频道活动边界事件（回合结束）由 bridge 顺手刷新缓存，打开即见最新。
//!
//! 布局（对齐 Web InfoPopover，Fluent 2 语义色；main.rs 按 `info_open` 切
//! 列宽 0 ↔ [`PANEL_WIDTH`]）：
//! ```text
//! ┌ 卡片（CardBackground + CardStroke + 圆角 8px）────────────┐
//! │ 环境                                            (11px 600) │
//! │ ● model 名                         live / 等待用量          │
//! │ 上下文  123,456 / 1,000,000      ▓▓▓▓▓░░░        12.3%     │
//! │ 当前请求                                                    │
//! │ 输入 12,345 │ 输出 678 │ 推理 90 │ 总计 13,113（等宽数值）    │
//! │ [缓存卡：命中 83.3%  ▓▓▓▓▓░░  命中 100 · 未命中 20]  (绿系)  │
//! │ 会话累计 · 3 次请求                                          │
//! │ 输入 … │ 输出 … │ 推理 … │ 总计 …                           │
//! │ [会话缓存卡]                                     (accent 系) │
//! └────────────────────────────────────────────────────────────┘
//! ```
//!
//! 复刻偏差（WORKFLOW §9.2 / P4a 研究报告）：
//! - 毛玻璃/双层阴影/内高光 → CardBackground + CardStroke（原生主题语义）
//! - RollingNumber 滚动动画 → 静态数字（千分位格式化保留"仪表感"）
//! - 进度条宽度补间 → 无（Composition 无宽度补间；值变化瞬间切换）
//! - live dot 3px 光晕 → 外环 `SystemSuccessBackground` + 内点（近似）
//! - hover 背景过渡 → 无颜色补间，瞬间切换（Fluent 2 语义色即时切换规范）
//! - 变更行/变更文件/cache prefix 警告 → 二期（environment 通道未立项）
//!
//! D15 结构稳定：面板区块全部固定结构 + `with_key`；cache 区块有/无两种
//! 形态按 keyed 干净重建（同 settings rows 模式，杜绝 kind 跳变错位）。

use std::sync::Arc;
use std::time::Duration;

use windows_reactor::*;

use deepx_fluent::{motion, tokens};

use crate::bridge::Bridge;
use crate::shell_store::{DashboardSnapshot, SessionDetail, UsageInfo};

/// 诊断日志（同 main.rs log_diag 约定：GUI 子系统无控制台，写文件）。
fn log_diag(msg: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(std::env::var("DEEPX_WINUI_LOG").unwrap_or_else(|_| ".deepx-winui.log".into()))
    {
        let _ = writeln!(f, "[info_panel] {msg}");
    }
}

/// 快照轮询间隔（同 sidebar / home_view）。
const POLL_INTERVAL: Duration = Duration::from_millis(500);
/// 面板宽度（main.rs 列宽；对齐 Web 340px − 阴影/内边距余量）。
pub const PANEL_WIDTH: f64 = 320.0;
/// 内置等宽字体；无资产时由 WinUI 回退到 Consolas。
const MONO_FONT: &str = deepx_fluent::tokens::CODE_FONT_FAMILY;

/// 千分位格式化（对齐 Web `formatRawNumber`：无缩写，逗号分组）。
fn fmt_thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// 上下文占用百分比（Web `contextPct`：contextTokens/contextLimit，截断 100）。
fn context_pct(d: &SessionDetail) -> f64 {
    if d.context_limit == 0 {
        return 0.0;
    }
    (d.usage.prompt_tokens as f64 * 100.0 / d.context_limit as f64).min(100.0)
}

/// 缓存命中百分比（Web `cacheHitPct`：hit/(hit+miss)；无样本 → None）。
fn cache_hit_pct(u: &UsageInfo) -> Option<f64> {
    let total = u.prompt_cache_hit_tokens + u.prompt_cache_miss_tokens;
    if total == 0 {
        None
    } else {
        Some(u.prompt_cache_hit_tokens as f64 * 100.0 / total as f64)
    }
}

/// 区块标题：使用 Fluent caption 级别，避免 9–11 DIP 字号在高 DPI 下过密。
fn section_heading(text: &str) -> Element {
    text_block(text)
        .font_size(tokens::TYPE_CAPTION)
        .semibold()
        .foreground(ThemeRef::SecondaryText)
        .into()
}

/// 等宽数值（仪表感核心：mono + semibold）。
fn mono_text(text: String, size: f64) -> Element {
    text_block(text)
        .font_size(size)
        .font_family(MONO_FONT)
        .semibold()
        .into()
}

/// 进度条（自绘：轨道 ControlFillSecondary + 填充 fill 色，5px 圆头）。
/// 比例列 `Star(pct)` 实现百分比宽度；Border 圆角自动裁切子元素。
fn progress_bar(pct: f64, fill: ThemeRef) -> Element {
    let pct = pct.clamp(0.0, 100.0);
    let fill_w = if pct <= 0.0 { 0.0001 } else { pct };
    let empty_w = if pct >= 100.0 { 0.0001 } else { 100.0 - pct };
    border(
        grid((
            border(text_block("")).background(fill).grid_column(0),
            border(text_block(""))
                .background(ThemeRef::ControlFillSecondary)
                .grid_column(1),
        ))
        .columns([GridLength::Star(fill_w), GridLength::Star(empty_w)]),
    )
    .height(5.0)
    .corner_radius(2.5)
    .into()
}

/// live 状态点（Web `info-live-dot`：6px 圆；active = 绿点 + 3px 光晕近似）。
fn live_dot(active: bool) -> Element {
    if active {
        border(
            border(text_block(""))
                .width(6.0)
                .height(6.0)
                .corner_radius(3.0)
                .background(ThemeRef::SystemSuccess),
        )
        .width(12.0)
        .height(12.0)
        .corner_radius(6.0)
        .background(ThemeRef::SystemSuccessBackground)
        .vertical_alignment(VerticalAlignment::Center)
        .into()
    } else {
        border(text_block(""))
            .width(6.0)
            .height(6.0)
            .corner_radius(3.0)
            .background(ThemeRef::ControlFill)
            .vertical_alignment(VerticalAlignment::Center)
            .into()
    }
}

/// token 四格：caption 标签 + 13px 等宽数值，
/// 列间 1px DividerStroke 分隔线，第一格无）。
fn token_grid(u: &UsageInfo) -> Element {
    let (p, c, r, t) = (
        u.prompt_tokens,
        u.completion_tokens,
        u.reasoning_tokens,
        u.total_tokens,
    );
    let cell = |label: &str, value: u64| -> Element {
        vstack((
            text_block(label)
                .font_size(tokens::TYPE_CAPTION)
                .foreground(ThemeRef::SecondaryText),
            mono_text(fmt_thousands(value), 13.0),
        ))
        .spacing(2.0)
        .into()
    };
    let vline: Element = border(text_block(""))
        .width(1.0)
        .height(16.0)
        .background(ThemeRef::DividerStroke)
        .vertical_alignment(VerticalAlignment::Center)
        .into();
    grid((
        cell("输入", p).grid_column(0),
        vline.clone().grid_column(1),
        cell("输出", c).grid_column(2),
        vline.clone().grid_column(3),
        cell("推理", r).grid_column(4),
        vline.grid_column(5),
        cell("总计", t).grid_column(6),
    ))
    .columns([
        GridLength::STAR,
        GridLength::Auto,
        GridLength::STAR,
        GridLength::Auto,
        GridLength::STAR,
        GridLength::Auto,
        GridLength::STAR,
    ])
    .into()
}

/// 缓存卡（Web `info-cache`：绿系 = 当前请求 / accent 系 = 会话累计；
/// 背景语义浅色 + 命中百分比 + hit/miss 明细 + 进度条）。
fn cache_card(label: &str, u: &UsageInfo, accent: bool) -> Option<Element> {
    let pct = cache_hit_pct(u)?;
    let (fill, bg, strong) = if accent {
        (ThemeRef::Accent, ThemeRef::SubtleFill, ThemeRef::AccentText)
    } else {
        (
            ThemeRef::SystemSuccess,
            ThemeRef::SystemSuccessBackground,
            ThemeRef::SystemSuccess,
        )
    };
    // label 行 Grid 两列（同 context_card，避免 hstack 双 Stretch 重叠）。
    // 显式 Element 标注：内层 into 的目标类型无法从 vstack 泛型 tuple 推断。
    let header: Element = grid((
        text_block(label)
            .font_size(tokens::TYPE_CAPTION)
            .foreground(ThemeRef::SecondaryText)
            .grid_column(0),
        text_block(format!("{pct:.1}%"))
            .font_size(14.0)
            .font_family(MONO_FONT)
            .font_weight(650)
            .foreground(strong)
            .horizontal_alignment(HorizontalAlignment::Right)
            .grid_column(1),
    ))
    .columns([GridLength::STAR, GridLength::Auto])
    .into();
    Some(
        border(
            vstack((
                header,
                text_block(format!(
                    "命中 {} · 未命中 {}",
                    fmt_thousands(u.prompt_cache_hit_tokens),
                    fmt_thousands(u.prompt_cache_miss_tokens)
                ))
                .font_size(tokens::TYPE_CAPTION)
                .foreground(ThemeRef::SecondaryText),
                progress_bar(pct, fill),
            ))
            .spacing(4.0),
        )
        .background(bg)
        .corner_radius(8.0)
        .padding(Thickness::xy(10.0, 9.0))
        .with_key("cache")
        .into(),
    )
}

/// 上下文卡（Web `info-context`：label 行 + 进度条 + 百分比，浅底圆角）。
fn context_card(d: &SessionDetail) -> Element {
    let pct = context_pct(d);
    // label 行用 Grid 两列（label STAR + 数值 Auto），避免 hstack 双 Stretch
    // 在水平 StackPanel 中的剩余空间分配异常导致文字挤压重叠。
    let label_row: Element = grid((
        text_block("上下文")
            .font_size(tokens::TYPE_CAPTION)
            .foreground(ThemeRef::SecondaryText)
            .grid_column(0),
        mono_text(
            format!(
                "{} / {}",
                fmt_thousands(d.usage.prompt_tokens),
                fmt_thousands(d.context_limit)
            ),
            tokens::TYPE_CAPTION,
        )
        .horizontal_alignment(HorizontalAlignment::Right)
        .grid_column(1),
    ))
    .columns([GridLength::STAR, GridLength::Auto])
    .into();
    let left: Element = vstack((label_row, progress_bar(pct, ThemeRef::Accent)))
        .spacing(6.0)
        .into();
    let pct_el: Element = text_block(format!("{pct:.1}%"))
        .font_size(tokens::TYPE_CAPTION)
        .font_family(MONO_FONT)
        .foreground(ThemeRef::SecondaryText)
        .vertical_alignment(VerticalAlignment::Center)
        .grid_column(1)
        .into();
    border(
        grid((left.grid_column(0), pct_el))
            .columns([GridLength::STAR, GridLength::Auto])
            .column_spacing(10.0),
    )
    .background(ThemeRef::SubtleFill)
    .corner_radius(8.0)
    .padding(Thickness::xy(11.0, 10.0))
    .with_key("context")
    .into()
}

/// 面板主体（main.rs 挂到内容区 Grid 右列；宽度由列宽 0 ↔ PANEL_WIDTH 控制）。
pub fn info_panel(cx: &mut RenderCx, bridge: Arc<Bridge>) -> Element {
    let (open, set_open) = cx.use_state::<bool>(false);
    let (detail, set_detail) = cx.use_state::<Option<SessionDetail>>(None);
    let timer = cx.use_ref::<Option<DispatcherTimer>>(None);
    let last_rev = cx.use_ref::<u64>(0);
    let last_open = cx.use_ref::<bool>(false);
    // 任务进度区块（dashboard 投影，control 事件驱动；P6 合并）。
    let (dashboard, set_dashboard) = cx.use_state::<Option<DashboardSnapshot>>(None);
    let last_dash_rev = cx.use_ref::<u64>(0);
    log_diag(&format!("info_panel render open={open}"));

    // 500ms 轮询：info 数据 rev（同 sidebar 模式）+ 标题栏 info_open 投影。
    cx.use_effect((), {
        let bridge = bridge.clone();
        let set_open = set_open.clone();
        let set_detail = set_detail.clone();
        let timer = timer.clone();
        let last_rev = last_rev.clone();
        let last_open = last_open.clone();
        move || {
            log_diag("info_panel effect (mount)");
            if timer.borrow().is_none() {
                match DispatcherTimer::new(POLL_INTERVAL, {
                    let core = bridge.core();
                    let set_open = set_open.clone();
                    let set_detail = set_detail.clone();
                    let set_dashboard = set_dashboard.clone();
                    let last_rev = last_rev.clone();
                    let last_open = last_open.clone();
                    let last_dash_rev = last_dash_rev.clone();
                    move || {
                        // 用量数据。
                        let (detail_, rev) = core.info_snapshot();
                        if rev != *last_rev.borrow() {
                            *last_rev.borrow_mut() = rev;
                            set_detail.call(detail_);
                        }
                        // 任务进度（dashboard 快照 rev）。
                        let (dash, dash_rev) = core.dashboard_snapshot();
                        if dash_rev != *last_dash_rev.borrow() {
                            *last_dash_rev.borrow_mut() = dash_rev;
                            set_dashboard.call(dash);
                        }
                        // 面板开关（Web shell.setHeader 投影的 info_open）。
                        let hdr = core.header_snapshot();
                        let o = hdr.0.info_open;
                        log_diag(&format!(
                            "info_panel tick open={o} rev={rev} header_rev={}",
                            hdr.1
                        ));
                        if o != *last_open.borrow() {
                            *last_open.borrow_mut() = o;
                            log_diag(&format!("open -> {o}"));
                            set_open.call(o);
                            if o {
                                // 打开瞬间拉取当前会话详情（防旧缓存）。
                                let seed = core.active_seed();
                                log_diag(&format!("refresh requested, seed={seed:?}"));
                                core.spawn_refresh_info(seed);
                            }
                        }
                    }
                }) {
                    Ok(t) => {
                        *timer.borrow_mut() = Some(t);
                        log_diag("info_panel timer created");
                    }
                    Err(e) => log_diag(&format!("info_panel timer failed: {e}")),
                }
            }
        }
    });

    if !open {
        // 列宽 0（main.rs）已隐藏；返回空占位避免无谓渲染。
        return text_block("").with_key("info-hidden").into();
    }

    // ── 面板内容（全部固定结构 + keyed，D15）───────────────────────
    let mut blocks: Vec<Element> = Vec::new();

    // ① 环境标题
    blocks.push(section_heading("环境").with_key("heading").into());

    match detail.as_ref() {
        Some(d) => {
            let live = d.usage.total_tokens > 0;
            // ② model 行：dot + model（等宽）+ live/等待用量（Grid 三列，
            // 避免 hstack 双 Stretch 重叠——model 撑满中间列）。
            let model_row: Element = grid((
                live_dot(live).grid_column(0),
                text_block(if d.model.is_empty() {
                    "未知模型"
                } else {
                    &d.model
                })
                .font_size(tokens::TYPE_CAPTION)
                .font_family(MONO_FONT)
                .foreground(ThemeRef::SecondaryText)
                .text_trimming(TextTrimming::CharacterEllipsis)
                .grid_column(1),
                text_block(if live { "live" } else { "等待用量" })
                    .font_size(tokens::TYPE_CAPTION)
                    .foreground(ThemeRef::TertiaryText)
                    .grid_column(2),
            ))
            .columns([GridLength::Auto, GridLength::STAR, GridLength::Auto])
            .column_spacing(7.0)
            .with_key("model")
            .into();
            blocks.push(model_row);

            // ③ 上下文卡
            blocks.push(context_card(d));

            // ④ 当前请求
            blocks.push(
                section_heading("当前请求")
                    .with_key("request-heading")
                    .into(),
            );
            if d.usage.total_tokens > 0 {
                blocks.push(token_grid(&d.usage).with_key("request-grid").into());
                if let Some(card) = cache_card("缓存", &d.usage, false) {
                    blocks.push(card);
                }
            } else {
                blocks.push(
                    text_block("等待用量")
                        .font_size(tokens::TYPE_CAPTION)
                        .foreground(ThemeRef::TertiaryText)
                        .with_key("request-empty")
                        .into(),
                );
            }

            // ⑤ 会话累计
            let session_heading: Element = if d.usage_requests > 0 {
                hstack((
                    text_block("会话累计")
                        .font_size(tokens::TYPE_CAPTION)
                        .semibold()
                        .foreground(ThemeRef::SecondaryText),
                    text_block(format!("{} 次请求", d.usage_requests))
                        .font_size(tokens::TYPE_CAPTION)
                        .foreground(ThemeRef::TertiaryText),
                ))
                .spacing(8.0)
                .with_key("session-heading")
                .into()
            } else {
                section_heading("会话累计")
                    .with_key("session-heading")
                    .into()
            };
            blocks.push(session_heading);
            blocks.push(token_grid(&d.totals).with_key("session-grid").into());
            if d.cache_reported_requests > 0 {
                if let Some(card) = cache_card("缓存（会话累计）", &d.totals, true) {
                    blocks.push(card);
                }
            }
        }
        None => {
            blocks.push(
                text_block("暂无用量数据")
                    .font_size(tokens::TYPE_CAPTION)
                    .foreground(ThemeRef::TertiaryText)
                    .with_key("empty")
                    .into(),
            );
        }
    }

    // ⑥ 任务进度区块（P6 合并：dashboard 投影——当前任务 + 状态计数 + 列表）。
    // 数据源 `bridge.core().dashboard_snapshot()`（control 事件驱动，同 composer goalBar）。
    if let Some(snap) = dashboard.as_ref() {
        if !snap.tasks.is_empty() {
            blocks.push(section_heading("任务").with_key("todo-heading").into());
            let current = snap
                .current_todo_id
                .as_deref()
                .and_then(|id| snap.tasks.iter().find(|t| t.id == id));
            let pending = snap.tasks.iter().filter(|t| t.status == "pending").count();
            let in_progress = snap
                .tasks
                .iter()
                .filter(|t| t.status == "in_progress")
                .count();
            let done = snap
                .tasks
                .iter()
                .filter(|t| t.status == "completed")
                .count();
            if let Some(task) = current {
                blocks.push(
                    hstack((
                        live_dot(true).with_key("todo-dot"),
                        text_block(&task.subject)
                            .font_size(tokens::TYPE_CAPTION)
                            .semibold()
                            .wrap(),
                        text_block(&task.status)
                            .font_size(tokens::TYPE_CAPTION)
                            .foreground(ThemeRef::SystemCaution),
                    ))
                    .spacing(8.0)
                    .with_key("todo-current")
                    .into(),
                );
            }
            blocks.push(
                text_block(format!(
                    "待处理 {pending} · 进行中 {in_progress} · 已完成 {done}"
                ))
                .font_size(tokens::TYPE_CAPTION)
                .foreground(ThemeRef::SecondaryText)
                .with_key("todo-counts")
                .into(),
            );
            let mut todo_rows: Vec<Element> = Vec::new();
            for (i, t) in snap.tasks.iter().enumerate() {
                let status_color = match t.status.as_str() {
                    "completed" => ThemeRef::SystemSuccess,
                    "in_progress" => ThemeRef::SystemCaution,
                    "cancelled" => ThemeRef::SystemNeutral,
                    _ => ThemeRef::TertiaryText,
                };
                let row: Element = grid((
                    text_block(if t.status == "completed" {
                        "✓"
                    } else {
                        "○"
                    })
                    .font_size(tokens::TYPE_CAPTION)
                    .foreground(status_color)
                    .grid_column(0),
                    text_block(&t.subject)
                        .font_size(tokens::TYPE_CAPTION)
                        .foreground(ThemeRef::SecondaryText)
                        .text_trimming(TextTrimming::CharacterEllipsis)
                        .grid_column(1),
                ))
                .columns([GridLength::Pixel(16.0), GridLength::STAR])
                .into();
                todo_rows.push(row.with_key(format!("todo-row-{i}-{}", t.id)));
            }
            blocks.push(vstack(todo_rows).spacing(3.0).with_key("todo-list").into());
        }
    }

    // 面板内容可滚动（P6 加任务区块后可能超高；scroll_viewer 纵向 Auto）。
    scroll_viewer(
        border(vstack(blocks).spacing(10.0))
            .background(ThemeRef::CardBackground)
            .border_brush(ThemeRef::CardStroke)
            .border_thickness(Thickness::uniform(1.0))
            .corner_radius(8.0)
            .padding(Thickness::xy(14.0, 14.0))
            .margin(Thickness::xy(8.0, 8.0)),
    )
    // 打开时淡入（open 翻转 → 内容首次 mount → ImplicitShowAnimation）。
    .transition(motion::content_enter(), motion::content_exit())
    .into()
}
