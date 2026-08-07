//! 原生 ChatView：conversation 事件直连 → `Transcript` → reactor 控件树。
//!
//! 数据源：`bridge.chat_drain()`——bridge 在 conversation 频道把渲染相关
//! 事件（turn/round/delta/checkpoint）缓存入队（wire JSON），本组件以
//! 16ms 事件泵 drain → `chat_adapter::internal_event` 反序列化 → 喂
//! `Transcript` 状态机；每次有变化以 rev 触发重渲染（reactor diff 只
//! 更新变化节点——与 demo 同模式）。
//!
//! 渲染模型（对齐 CHATVIEW-RENDERING-REFERENCE）：
//! - turn 壳：用户气泡 + 状态徽标；
//! - round：思考折叠区 + 工具卡 + 答案（live 字面/表格交错 → final 富文本）；
//! - 协议表格流式渐进（LiveSegment::Table 网格，残行逐字生长）。
//!
//! 已知缺口（后续补能力）：代码高亮（syntect）、mermaid/katex 自绘、
//! 数据分页加载（ISupportIncrementalLoading，当前全量快照）。

use std::sync::Arc;
use std::time::Duration;

use markdown_winui::{
    AnswerView, ConversationEvent, LiveSegment, RichTextOutput, RoundView, Transcript, TurnStatus,
    TurnView,
};
use windows_reactor::*;

use crate::bridge::Bridge;
use crate::chat_adapter;

/// 事件泵间隔（16ms ≈ 60fps；高吞吐时批量消费，同 demo 模式）。
const PUMP_INTERVAL: Duration = Duration::from_millis(16);

/// 跟随尾部滚动请求节流：live 流式期间 100ms 一次贴底请求。16ms 泵每
/// tick 都请求会让滚动与用户滚轮/滚动条抢占，并形成"滚动 → 行 realize →
/// 渲染 → 再滚动"反馈循环，UI 线程满载（表现为滚动条卡死）。结构性
/// 变化（restore / 新 turn / round 完成）不受此限，立即滚底。
const SCROLL_REQUEST_THROTTLE: Duration = Duration::from_millis(100);

/// 渲染降频：live 流式 33ms 合并一次（≈30fps）。Transcript 状态在泵内
/// 累积，渲染永远基于最新状态，文本更新 30fps 对用户感知无差，CPU 减半。
const RENDER_THROTTLE: Duration = Duration::from_millis(33);

pub fn chat_view(cx: &mut RenderCx, bridge: Arc<Bridge>) -> Element {
    let transcript = cx.use_ref::<Transcript>(Transcript::new());
    let timer = cx.use_ref::<Option<DispatcherTimer>>(None);
    let last_rev = cx.use_ref::<u64>(0);
    let last_seed = cx.use_ref::<String>(String::new());
    // 已成功 restore 的 seed：空态文案区分"快照已加载但会话为空"（全新
    // 空会话 → "开始新的对话…"）与"快照未到达仍在加载"（→ "加载会话…"）。
    let last_restored_seed = cx.use_ref::<String>(String::new());
    // 跟随尾部滚动请求版本：pump 内容变化时递增（restore/新 turn 立即、
    // live 增量 100ms 节流），render 时随 list_view.scroll_to_index 下发
    // ——reconciler 检测版本变化后按 near_bottom 判定执行贴底滚动
    // （用户离开底部时不打扰）。
    let scroll_version = cx.use_ref::<u64>(0);
    // 滚动请求节流基准（live 流式限频，见 SCROLL_REQUEST_THROTTLE）。
    let last_scroll_request = cx.use_ref::<std::time::Instant>(std::time::Instant::now());
    // 渲染节流基准（live 流式降频，见 RENDER_THROTTLE）。
    let last_render = cx.use_ref::<std::time::Instant>(std::time::Instant::now());
    // deferred（快照 seed 不匹配）日志限频：16ms 泵每 tick 都会命中，
    // 不节流会刷爆日志（spawn_timeline_refresh 本身有 1s 节流）。
    let last_deferred_log = cx.use_ref::<std::time::Instant>(std::time::Instant::now());
    let (_, set_rev) = cx.use_state::<u64>(0);

    // 事件泵：drain bridge 队列 → Transcript；rev 变化触发重渲染。
    cx.use_effect((), {
        let bridge = bridge.clone();
        let transcript = transcript.clone();
        let timer = timer.clone();
        let last_rev = last_rev.clone();
        let last_seed = last_seed.clone();
        let last_restored_seed = last_restored_seed.clone();
        let last_deferred_log = last_deferred_log.clone();
        let last_scroll_request = last_scroll_request.clone();
        let last_render = last_render.clone();
        let scroll_version = scroll_version.clone();
        let set_rev = set_rev.clone();
        move || {
            if timer.borrow().is_some() {
                return;
            }
            match DispatcherTimer::new(PUMP_INTERVAL, {
                let bridge = bridge.clone();
                let transcript = transcript.clone();
                let last_rev = last_rev.clone();
                let last_seed = last_seed.clone();
                let last_restored_seed = last_restored_seed.clone();
                let last_deferred_log = last_deferred_log.clone();
                let last_scroll_request = last_scroll_request.clone();
                let last_render = last_render.clone();
                let scroll_version = scroll_version.clone();
                let set_rev = set_rev.clone();
                move || {
                    // 会话切换：active_seed 变化 → 重置 Transcript（旧会话内容
                    // 不残留），等新快照/增量。
                    let seed = bridge.core().active_seed();
                    if seed != *last_seed.borrow() {
                        *last_seed.borrow_mut() = seed.clone();
                        *transcript.borrow_mut() = Transcript::new();
                        *last_restored_seed.borrow_mut() = String::new();
                        *last_rev.borrow_mut() = 0;
                        log_diag(&format!("chat_view: switched to seed {seed}"));
                    }
                    // 先 drain 拿 rev：restore 分支渲染也要用（快照到达时
                    // chat_rev 已 +1，直接下发触发重渲染）。
                    let (events, rev) = bridge.core().chat_drain();
                    // 1) timeline 快照（resume 历史；peek + seed 校验）：
                    //    匹配才消费；不匹配**保留**快照（不丢弃）并主动重拉
                    //    active seed 的快照——原 take 语义消费即弃，丢弃后
                    //    daemon 不重推，快照永久丢失 → ChatView 永远停在
                    //    "加载会话…"。
                    if let Some((snap_seed, snap)) = bridge.core().chat_timeline_peek() {
                        if snap_seed == seed {
                            bridge.core().chat_timeline_consume();
                            let turns = chat_adapter::timeline_turns(&snap);
                            let n = turns.len();
                            transcript.borrow_mut().restore(turns);
                            *last_restored_seed.borrow_mut() = seed.clone();
                            // restore 是结构性变化：立即滚底 + 立即渲染
                            // （不节流；顺带更新节流基准防紧随的 live 抖动）。
                            let now = std::time::Instant::now();
                            *scroll_version.borrow_mut() += 1;
                            *last_scroll_request.borrow_mut() = now;
                            *last_render.borrow_mut() = now;
                            set_rev.call(rev);
                            log_diag(&format!("chat_view: restored {n} turns for {seed}"));
                        } else {
                            // 快照 seed 不匹配（旧会话残留/并发交错）：主动重拉
                            // active seed（1s 节流在 bridge 内）。日志限频，
                            // 避免 16ms 泵每 tick 刷屏。
                            let now = std::time::Instant::now();
                            if now.duration_since(*last_deferred_log.borrow())
                                >= std::time::Duration::from_secs(1)
                            {
                                *last_deferred_log.borrow_mut() = now;
                                log_diag(&format!(
                                    "chat_view: snapshot for {snap_seed} deferred (active {seed}); refreshing"
                                ));
                            }
                            bridge.core().spawn_timeline_refresh(&seed);
                        }
                    } else {
                        // 快照缺失（activate_timeline 失败/未达/从未激活）：
                        // 主动重拉——否则冷启动/重建后 ChatView 永久停在
                        // "加载会话…"，只有发送消息（增量事件）才出现内容。
                        // 空会话 restore 后 last_restored_seed == seed，不再重拉。
                        if !seed.is_empty() && *last_restored_seed.borrow() != seed {
                            bridge.core().spawn_timeline_refresh(&seed);
                        }
                    }
                    // 2) 增量事件（新对话流式）
                    if rev != *last_rev.borrow() && !events.is_empty() {
                        *last_rev.borrow_mut() = rev;
                        let mut t = transcript.borrow_mut();
                        let mut structural = false;
                        for ev_json in events {
                            if let Some(ev) = chat_adapter::internal_event(&ev_json) {
                                // 结构性变化（新 turn / 回合封口）：立即跟随
                                // 滚底；live 增量（delta/checkpoint）节流。
                                if matches!(
                                    ev,
                                    ConversationEvent::TurnStarted { .. }
                                        | ConversationEvent::TurnCompleted { .. }
                                        | ConversationEvent::TurnFailed { .. }
                                        | ConversationEvent::RoundCompleted { .. }
                                ) {
                                    structural = true;
                                }
                                t.apply(&ev);
                            }
                        }
                        // 滚动请求：结构性变化立即；live 增量 ≥100ms 节流
                        // （16ms 泵每 tick 请求会与滚轮抢占，并形成
                        // "滚动 → realize → 渲染 → 再滚动"反馈循环）。
                        if structural
                            || std::time::Instant::now()
                                .duration_since(*last_scroll_request.borrow())
                                >= SCROLL_REQUEST_THROTTLE
                        {
                            *scroll_version.borrow_mut() += 1;
                            *last_scroll_request.borrow_mut() = std::time::Instant::now();
                        }
                        // 渲染：结构性变化立即；live 增量 ≥33ms 合并
                        // （Transcript 状态累积，渲染基于最新状态）。
                        if structural
                            || std::time::Instant::now()
                                .duration_since(*last_render.borrow())
                                >= RENDER_THROTTLE
                        {
                            *last_render.borrow_mut() = std::time::Instant::now();
                            set_rev.call(rev);
                        }
                    }
                }
            }) {
                Ok(t) => *timer.borrow_mut() = Some(t),
                Err(e) => log_diag(&format!("chat_view: pump timer failed: {e}")),
            }
        }
    });

    // 投影渲染（reactor diff：只更新变化节点）。
    // 内容态：ListView——WinUI 原生虚拟化，行内容只在滚入视口时构建
    // （长会话不再全量渲染，view 闭包只对 realized 行调用）+ 声明式滚动
    // 请求（跟随尾部：restore 滚底 / 新 turn / live 增长；near_bottom
    // 120px 判定在 reconciler/backend，用户上滚浏览历史时不打扰）。
    let s = transcript.borrow();
    if s.turns().is_empty() {
        // 空态：无 active seed = 新对话；有 seed 但快照未 restore = 加载中；
        // 快照已 restore 但 turns 为空 = 全新空会话（或已清空），非加载中。
        let label = if bridge.core().active_seed().is_empty() {
            "开始新的对话…"
        } else if *last_restored_seed.borrow() == bridge.core().active_seed() {
            "开始新的对话…"
        } else {
            "加载会话…"
        };
        return text_block(label)
            .font_size(13.0)
            .foreground(Color {
                a: 255,
                r: 130,
                g: 130,
                b: 130,
            })
            .with_key("chat-empty")
            .into();
    }
    let turns = s.turns().to_vec();
    let last = turns.len() as i32 - 1;
    list_view(turns, |turn: &TurnView, i: usize| turn_view(i, turn))
        .with_key_selector(|turn: &TurnView| turn.turn_id.clone())
        .scroll_to_index(*scroll_version.borrow(), last)
        .with_key("chat-transcript")
        .into()
}

// ── turn / round / answer 渲染（移植自 streaming-demo）─────────────

fn turn_view(i: usize, turn: &TurnView) -> Element {
    let status = match turn.status {
        TurnStatus::Running => "⏳",
        TurnStatus::Completed => "✅",
        TurnStatus::Failed => "❌",
    };
    let user_line = format!("{status} {}\n", turn.user_text);
    let mut items: Vec<Element> = vec![
        // 用户气泡（Border 无 padding builder，用换行撑开）
        border(text_block(user_line).wrap().selectable())
            .corner_radius(10.0)
            .border_brush(Color {
                a: 255,
                r: 0,
                g: 120,
                b: 212,
            })
            .border_thickness(Thickness {
                left: 1.0,
                top: 1.0,
                right: 1.0,
                bottom: 1.0,
            })
            .into(),
    ];
    for round in &turn.rounds {
        items.push(round_view(i, round));
    }
    vstack(items)
        .spacing(6.0)
        .with_key(turn.turn_id.clone())
        .into()
}

fn round_view(turn_idx: usize, round: &RoundView) -> Element {
    let mut items: Vec<Element> = Vec::new();
    if let Some(thinking) = &round.thinking {
        // 思考链路：气泡框（同用户 chat 的 border 样式，灰边区分）——
        // 完整内容直接展示，不再用折叠器（折叠让给 tool 卡）。
        items.push(
            border(
                vstack([
                    text_block("🧠 思考")
                        .font_size(11.0)
                        .semibold()
                        .foreground(Color {
                            a: 255,
                            r: 140,
                            g: 140,
                            b: 140,
                        }),
                    text_block(thinking).wrap().selectable(),
                ])
                .spacing(4.0),
            )
            .corner_radius(10.0)
            .border_brush(Color {
                a: 255,
                r: 160,
                g: 160,
                b: 160,
            })
            .border_thickness(Thickness {
                left: 1.0,
                top: 1.0,
                right: 1.0,
                bottom: 1.0,
            })
            .with_key(format!("t{turn_idx}r{}-thinking", round.round_num))
            .into(),
        );
    }
    for card in &round.tool_calls {
        items.push(tool_card(turn_idx, round.round_num, card));
    }
    items.push(answer_view(turn_idx, round.round_num, &round.answer));
    vstack(items)
        .spacing(4.0)
        .with_key(format!("t{turn_idx}r{}", round.round_num))
        .into()
}

fn answer_view(turn_idx: usize, round_num: u32, answer: &AnswerView) -> Element {
    match answer {
        AnswerView::Streaming { segments, .. } => live_view(turn_idx, round_num, segments),
        AnswerView::Final { rich, .. } => final_view(turn_idx, round_num, rich),
    }
}

/// 流式答案视图：字面/表格交错序列按序渲染
/// （协议表格渐进长出；残行逐字生长在网格末行）。
fn live_view(turn_idx: usize, round_num: u32, segments: &[LiveSegment]) -> Element {
    let mut items: Vec<Element> = Vec::new();
    for (si, seg) in segments.iter().enumerate() {
        match seg {
            LiveSegment::Text(t) if !t.is_empty() => items.push(
                text_block(t)
                    .wrap()
                    .selectable()
                    .with_key(format!("t{turn_idx}r{round_num}-live-t{si}"))
                    .into(),
            ),
            LiveSegment::Table(td) => items.push(markdown_winui::table_view(
                td,
                &format!("t{turn_idx}r{round_num}-live-table-{si}"),
            )),
            LiveSegment::Text(_) => {}
        }
    }
    if items.is_empty() {
        // 空内容：占位保持 key 稳定
        items.push(
            text_block("")
                .with_key(format!("t{turn_idx}r{round_num}-live"))
                .into(),
        );
    }
    vstack(items)
        .spacing(4.0)
        .with_key(format!("t{turn_idx}r{round_num}-live"))
        .into()
}

/// 权威终态视图：RichTextBlock + 表格网格 + 代码块卡片。
fn final_view(turn_idx: usize, round_num: u32, rich: &RichTextOutput) -> Element {
    let mut items: Vec<Element> = Vec::new();
    // RichTextBlock：真实富文本（加粗/列表/链接）
    let mut rt = RichTextBlock::new();
    rt.paragraphs = rich.paragraphs.clone();
    rt.text_wrapping = TextWrapping::Wrap;
    rt.is_text_selection_enabled = true;
    items.push(rt.into());
    // 表格通道（Grid 拼装：表头加粗 + 等分列）
    for (ti, table) in rich.tables.iter().enumerate() {
        items.push(markdown_winui::table_view(
            table,
            &format!("t{turn_idx}r{round_num}-table-{ti}"),
        ));
    }
    // 代码块通道（独立卡片；高亮器未接入，先 plain）
    for (ci, code) in rich.code_blocks.iter().enumerate() {
        let lang = code.lang.as_deref().unwrap_or("");
        items.push(
            border(
                vstack([
                    text_block(if lang.is_empty() { "code" } else { lang })
                        .font_size(10.0)
                        .semibold()
                        .foreground(Color {
                            a: 255,
                            r: 150,
                            g: 150,
                            b: 150,
                        }),
                    text_block(&code.text).wrap().selectable(),
                ])
                .spacing(4.0),
            )
            .corner_radius(8.0)
            .border_brush(Color {
                a: 255,
                r: 140,
                g: 140,
                b: 140,
            })
            .border_thickness(Thickness {
                left: 1.0,
                top: 1.0,
                right: 1.0,
                bottom: 1.0,
            })
            .with_key(format!("t{turn_idx}r{round_num}-code-{ci}"))
            .into(),
        );
    }
    vstack(items)
        .spacing(6.0)
        .with_key(format!("t{turn_idx}r{round_num}-final"))
        .into()
}

/// 工具卡（流式累积，id 稳定；done 打勾）。折叠器承载：
/// header = 图标 + 状态 + 工具名；展开 = 参数 raw（DeepX 工具）或
/// 执行状态（provider 内建工具，如 web_search 搜索中…）。
fn tool_card(turn_idx: usize, round_num: u32, card: &markdown_winui::ToolCardView) -> Element {
    let status = if card.done { "✓" } else { "…" };
    let name = card.name.as_deref().unwrap_or("<解析中>");
    let icon = if card.provider { "🔍" } else { "🛠" };
    // 展开区：provider 卡显示状态文案（args_display 承载），其余显示参数。
    let body: Element = if card.args_display.trim().is_empty() {
        text_block("").font_size(11.0).into()
    } else {
        text_block(&card.args_display).font_size(11.0).wrap().selectable().into()
    };
    Expander::new(body)
        .header(format!("{icon} {status} {name}"))
        .expanded(false)
        .with_key(format!("t{turn_idx}r{round_num}-card-{}", card.id))
        .into()
}

/// 诊断日志（窗口程序无控制台：写 %TEMP%）。
fn log_diag(msg: &str) {
    use std::io::Write;
    let path = std::env::temp_dir().join("deepx-winui.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
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
