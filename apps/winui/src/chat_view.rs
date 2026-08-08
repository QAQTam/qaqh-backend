//! 原生 ChatView：conversation 事件直连 → `Transcript` → reactor 控件树。
//!
//! 数据源：`bridge.chat_drain()`——bridge 在 conversation 频道把 canonical
//! typed events 缓存入队，本组件以 16ms XAML 帧批次 drain，经
//! `chat_adapter::render_event` 映射后先合并同目标的相邻 delta，再喂
//! `Transcript` 状态机；紧凑的模型失效摘要决定是否声明新的 Element 树，
//! `windows-reactor` 负责 keyed diff 与 XAML 提交。
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

use deepx_fluent::{StatusTone, motion, tokens};
use markdown_winui::{
    AnswerView, LiveSegment, RichTextOutput, RoundView, Transcript, TurnStatus, TurnView,
};
use windows_reactor::*;

use crate::bridge::Bridge;
use crate::chat_adapter;

/// XAML 提交批次（16ms ≈ 60fps）：队列中的 token delta 先合并，再在
/// UI 线程的一次 retained-mode 更新中提交；不逐 token 触碰控件树。
const PUMP_INTERVAL: Duration = Duration::from_millis(16);

/// 跟随尾部滚动请求节流：live 流式期间 50ms 一次贴底请求（原 100ms——
/// 文本更新与视口贴底存在半拍延迟，视觉呈"内容在底部下方攒动"；减半
/// 提升跟手性）。16ms 泵每 tick 都请求会让滚动与用户滚轮/滚动条抢占，
/// 并形成"滚动 → 行 realize → 渲染 → 再滚动"反馈循环，UI 线程满载
/// （表现为滚动条卡死）——若实机出现该现象回退 100ms。结构性变化
/// （restore / 新 turn / round 完成）不受此限，立即滚底。
const SCROLL_REQUEST_THROTTLE: Duration = Duration::from_millis(50);

/// 顶部预加载分页大小：滚动接近窗口顶部时一次扩展的回合数。
/// 与 `markdown_winui::WINDOW_DEFAULT_LEN` 同量级，可经实机手感调优。
const WINDOW_PAGE: usize = 30;

/// near-top 判定阈值（DIPs）：滚动到距列表顶部此距离内触发预加载。
/// 与 reactor 贴底阈值（120px）对称。
const NEAR_TOP_THRESHOLD_PX: f64 = 120.0;

pub fn chat_view(cx: &mut RenderCx, bridge: Arc<Bridge>) -> Element {
    let transcript = cx.use_ref::<Transcript>(Transcript::new());
    let timer = cx.use_ref::<Option<DispatcherTimer>>(None);
    let last_rev = cx.use_ref::<u64>(0);
    let last_seed = cx.use_ref::<String>(String::new());
    // 已成功 restore 的 seed：空态文案区分"快照已加载但会话为空"（全新
    // 空会话 → "开始新的对话…"）与"快照未到达仍在加载"（→ "加载会话…"）。
    let last_restored_seed = cx.use_ref::<String>(String::new());
    // 跟随尾部滚动请求版本：pump 内容变化时递增（restore/新 turn 立即、
    // live 增量节流），render 时随 list_view.follow_tail 下发
    // ——reconciler 检测版本变化后按 near-tail 判定执行贴底滚动
    // （用户离开底部时不打扰）。
    let scroll_version = cx.use_ref::<u64>(0);
    // restore 是唯一需要无条件贴底的路径；普通增量必须尊重用户上滚。
    let force_tail_version = cx.use_ref::<Option<u64>>(None);
    // 滚动请求节流基准（live 流式限频，见 SCROLL_REQUEST_THROTTLE）。
    let last_scroll_request = cx.use_ref::<std::time::Instant>(std::time::Instant::now());
    // deferred（快照 seed 不匹配）日志限频：16ms 泵每 tick 都会命中，
    // 不节流会刷爆日志（spawn_timeline_refresh 本身有 1s 节流）。
    let last_deferred_log = cx.use_ref::<std::time::Instant>(std::time::Instant::now());
    // UI 提交代次与 transport rev 解耦：seed、快照、分页和事件批次都可
    // 独立提交，不会因构造 rev 与下一条传输 rev 碰撞而漏帧。
    let render_generation = cx.use_ref::<u64>(0);
    let (_, set_render_generation) = cx.use_state::<u64>(0);
    // 锚定补偿挂起标记：`Some(rows)` = 本帧渲染需用 within 滚动（顶部
    // 预加载后把「原窗口首行」锚回原位，视口不跳）。rows = 扩展前移量
    // = 原首行的新下标。渲染闭包 take 后随 preserve_anchor 下发。
    let pending_anchor = cx.use_ref::<Option<usize>>(None);

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
        let scroll_version = scroll_version.clone();
        let force_tail_version = force_tail_version.clone();
        let pending_anchor = pending_anchor.clone();
        let render_generation = render_generation.clone();
        let set_render_generation = set_render_generation.clone();
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
                let scroll_version = scroll_version.clone();
                let force_tail_version = force_tail_version.clone();
                let render_generation = render_generation.clone();
                let set_render_generation = set_render_generation.clone();
                move || {
                    // 会话切换：active_seed 变化 → 重置 Transcript（旧会话内容
                    // 不残留），等新快照/增量。
                    let seed = bridge.core().active_seed();
                    if seed != *last_seed.borrow() {
                        *last_seed.borrow_mut() = seed.clone();
                        *transcript.borrow_mut() = Transcript::new();
                        *last_restored_seed.borrow_mut() = String::new();
                        *force_tail_version.borrow_mut() = None;
                        *last_rev.borrow_mut() = 0;
                        *render_generation.borrow_mut() += 1;
                        set_render_generation.call(*render_generation.borrow());
                        log_diag(&format!("chat_view: switched to seed {seed}"));
                    }
                    // 先 drain 拿 rev：restore 分支渲染也要用（快照到达时
                    // chat_rev 已 +1，直接下发触发重渲染）。
                    let (events, rev) = bridge.core().chat_drain();
                    let output_resolutions = bridge.core().chat_output_drain();
                    // 1) timeline 快照（resume 历史；peek + seed 校验）：
                    //    匹配才消费；不匹配**保留**快照（不丢弃）并主动重拉
                    //    active seed 的快照——原 take 语义消费即弃，丢弃后
                    //    daemon 不重推，快照永久丢失 → ChatView 永远停在
                    //    "加载会话…"。
                    if let Some((snap_seed, snap)) = bridge.core().chat_timeline_peek() {
                        if snap_seed == seed {
                            bridge.core().chat_timeline_consume();
                            let turns = chat_adapter::restored_turns(&snap);
                            let n = turns.len();
                            transcript.borrow_mut().restore(turns);
                            *last_restored_seed.borrow_mut() = seed.clone();
                            // restore 是结构性变化：立即滚底 + 立即提交。
                            let now = std::time::Instant::now();
                            *scroll_version.borrow_mut() += 1;
                            *force_tail_version.borrow_mut() = Some(*scroll_version.borrow());
                            *last_scroll_request.borrow_mut() = now;
                            *render_generation.borrow_mut() += 1;
                            set_render_generation.call(*render_generation.borrow());
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
                    // 1.5) 分页页（上滚翻页的更早回合）：drain → 前插 →
                    //     锚定补偿。用户上滚浏览中（tail_following=false），
                    //     前插后窗口起点已右移，用 pending_anchor 把
                    //     「原窗口首行」锚回原位，视口不跳。
                    let pages = bridge.core().chat_prepend_drain();
                    if !pages.is_empty() {
                        let mut t = transcript.borrow_mut();
                        let mut prepended = 0usize;
                        for (_, page) in pages {
                            let turns = chat_adapter::restored_turns(&page);
                            prepended += t.prepend_turns(turns);
                        }
                        if prepended > 0 {
                            *pending_anchor.borrow_mut() = Some(prepended);
                            *scroll_version.borrow_mut() += 1;
                            *render_generation.borrow_mut() += 1;
                            set_render_generation.call(*render_generation.borrow());
                            log_diag(&format!(
                                "chat_view: prepended {prepended} turns for {seed}"
                            ));
                        }
                    }
                    // 1.75) 外置终态正文：transport 在后台校验并下载，UI
                    // 线程只把结果归并回 Transcript 的同一个 round 状态。
                    if !output_resolutions.is_empty() {
                        let mut t = transcript.borrow_mut();
                        let mut changed = false;
                        for resolution in output_resolutions {
                            let change = match resolution.result {
                                Ok(text) => t.resolve_output(
                                    &resolution.turn_id,
                                    resolution.round_num,
                                    &text,
                                ),
                                Err(error) => t.fail_output(
                                    &resolution.turn_id,
                                    resolution.round_num,
                                    error,
                                ),
                            };
                            changed |= change.changed();
                        }
                        drop(t);
                        if changed {
                            *scroll_version.borrow_mut() += 1;
                            *last_scroll_request.borrow_mut() = std::time::Instant::now();
                            *render_generation.borrow_mut() += 1;
                            set_render_generation.call(*render_generation.borrow());
                        }
                    }
                    // 2) 增量事件（新对话流式）
                    if rev != *last_rev.borrow() {
                        *last_rev.borrow_mut() = rev;
                    }
                    if !events.is_empty() {
                        let frame_events = events
                            .into_iter()
                            .filter_map(|event| chat_adapter::render_event(&event));
                        let mut t = transcript.borrow_mut();
                        let update = t.apply_frame(frame_events);
                        let pending_outputs = t.take_pending_outputs();
                        drop(t);
                        for pending in pending_outputs {
                            bridge.core().spawn_resolve_chat_output(&seed, pending);
                        }
                        if !update.changed() {
                            return;
                        }
                        let structural = update.is_structural();
                        let now = std::time::Instant::now();
                        // 滚动只随真正改变内容 extent 的 XAML 提交发生；
                        // 状态徽标更新不再制造多余 ScrollViewer 请求。
                        if update.extent_changed
                            && (structural
                                || now.duration_since(*last_scroll_request.borrow())
                                    >= SCROLL_REQUEST_THROTTLE)
                        {
                            // 窗口跟随尾部：仅当窗口未被用户上滚扩展时滑动
                            // （保持最近 N 个回合，长会话不退化）；用户上滚
                            // 扩展后窗口保持，避免浏览内容跳动。
                            if structural && transcript.borrow().tail_following() {
                                transcript.borrow_mut().slide_window_tail();
                            }
                            *scroll_version.borrow_mut() += 1;
                            *last_scroll_request.borrow_mut() = now;
                        }
                        *render_generation.borrow_mut() += 1;
                        set_render_generation.call(*render_generation.borrow());
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
        let (title, detail, busy) = if label == "加载会话…" {
            ("正在恢复对话", "正在读取时间线与最近的消息。", true)
        } else {
            (
                "开始新的对话",
                "输入消息，或使用斜杠命令开始一项任务。",
                false,
            )
        };
        return deepx_fluent::empty_state(title, detail, busy)
            .transition(motion::content_enter(), motion::content_exit())
            .automation_name(title)
            .automation_id("chat-empty")
            .with_key("chat-empty");
    }
    let turns = s.window_turns().to_vec();
    // 顶部预加载后本帧需要锚定补偿：取走挂起标记，把
    // 「原窗口首行」（新下标 = 扩展前移量）锚回原位，视口不跳。
    let anchor_rows = pending_anchor.borrow_mut().take();
    let mut builder = list_view(turns, |turn: &TurnView, i: usize| turn_view(i, turn))
        .with_key_selector(|turn: &TurnView| turn.turn_id.clone())
        .selection_mode(SelectionMode::None);
    if let Some(anchor_rows) = anchor_rows {
        builder = builder.preserve_anchor(*scroll_version.borrow(), anchor_rows, 0.0);
    } else if *force_tail_version.borrow() == Some(*scroll_version.borrow()) {
        force_tail_version.borrow_mut().take();
        builder = builder.force_tail(*scroll_version.borrow());
    } else {
        builder = builder.follow_tail(*scroll_version.borrow());
    }
    let transcript_list: Element = builder
        .on_top_reached({
            let bridge = bridge.clone();
            let transcript = transcript.clone();
            let pending_anchor = pending_anchor.clone();
            let scroll_version = scroll_version.clone();
            let render_generation = render_generation.clone();
            let set_render_generation = set_render_generation.clone();
            move |_| {
                // 滚动接近窗口顶部（边沿触发一次）：先扩展窗口内预加载更早
                // 回合，渲染时锚定补偿保持视口。
                let mut t = transcript.borrow_mut();
                let moved = t.expand_window(WINDOW_PAGE);
                if moved > 0 {
                    *pending_anchor.borrow_mut() = Some(moved);
                    // 锚定补偿随 scroll_version 变化触发（reconciler 按版本
                    // diff）；独立 UI 代次保证触发渲染。
                    *scroll_version.borrow_mut() += 1;
                    drop(t);
                    *render_generation.borrow_mut() += 1;
                    set_render_generation.call(*render_generation.borrow());
                    return;
                }
                drop(t);
                // 窗口内已全量放行：若服务端还有更早回合 → 翻页拉取
                // （异步前插，bridge 在途防重入 + has_more 自动维护）。
                let seed = bridge.core().active_seed();
                if seed.is_empty() || !bridge.core().timeline_has_more(&seed) {
                    return;
                }
                let before = transcript
                    .borrow()
                    .turns()
                    .first()
                    .map(|t| t.turn_id.clone());
                if let Some(before) = before {
                    bridge.core().spawn_fetch_earlier(&seed, &before);
                }
            }
        })
        .top_threshold(NEAR_TOP_THRESHOLD_PX)
        .with_key("chat-transcript")
        .into();
    transcript_list
        .automation_name("对话记录")
        .automation_id("chat-transcript")
}

// ── turn / round / answer 渲染（移植自 streaming-demo）─────────────

fn turn_view(i: usize, turn: &TurnView) -> Element {
    let (status, tone) = match turn.status {
        TurnStatus::Running => ("正在处理", StatusTone::Running),
        TurnStatus::Completed => ("已完成", StatusTone::Success),
        TurnStatus::Failed => ("失败", StatusTone::Critical),
    };
    let mut items: Vec<Element> = vec![deepx_fluent::user_message(
        text_block(&turn.user_text)
            .font_size(tokens::TYPE_BODY)
            .wrap()
            .selectable(),
        deepx_fluent::status_badge(status, tone),
    )];
    for round in &turn.rounds {
        items.push(round_view(i, round));
    }
    vstack(items)
        .spacing(tokens::SPACE_3)
        .padding(Thickness {
            left: tokens::SPACE_6,
            top: tokens::SPACE_3,
            right: tokens::SPACE_6,
            bottom: tokens::SPACE_3,
        })
        .max_width(tokens::CONVERSATION_MAX_WIDTH)
        .horizontal_alignment(HorizontalAlignment::Stretch)
        .with_key(turn.turn_id.clone())
        .into()
}

fn round_view(turn_idx: usize, round: &RoundView) -> Element {
    let mut items: Vec<Element> = Vec::new();
    if let Some(thinking) = &round.thinking {
        items.push(
            Expander::new(
                text_block(thinking)
                    .font_size(tokens::TYPE_BODY)
                    .foreground(ThemeRef::SecondaryText)
                    .wrap()
                    .selectable(),
            )
            .header("思考过程")
            .expanded(false)
            .tooltip("展开或折叠思考过程")
            .automation_name("思考过程")
            .automation_id(format!("chat-thinking-{turn_idx}-{}", round.round_num))
            .with_key(format!("t{turn_idx}r{}-thinking", round.round_num))
            .into(),
        );
    }
    for card in &round.tool_calls {
        items.push(tool_card(turn_idx, round.round_num, card));
    }
    items.push(deepx_fluent::assistant_message(answer_view(
        turn_idx,
        round.round_num,
        &round.answer,
    )));
    if round.output_loading {
        items.push(
            text_block("正在读取完整回答…")
                .font_size(tokens::TYPE_CAPTION)
                .foreground(ThemeRef::SecondaryText)
                .automation_name("正在读取完整回答")
                .with_key(format!("t{turn_idx}r{}-output-loading", round.round_num))
                .into(),
        );
    }
    if let Some(error) = &round.output_error {
        items.push(
            text_block(format!("完整回答读取失败：{error}"))
                .font_size(tokens::TYPE_CAPTION)
                .foreground(ThemeRef::SystemCritical)
                .wrap()
                .selectable()
                .automation_name("完整回答读取失败")
                .with_key(format!("t{turn_idx}r{}-output-error", round.round_num))
                .into(),
        );
    }
    vstack(items)
        .spacing(tokens::SPACE_2)
        .max_width(tokens::READING_MAX_WIDTH)
        .horizontal_alignment(HorizontalAlignment::Stretch)
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
        .spacing(tokens::SPACE_2)
        .with_key(format!("t{turn_idx}r{round_num}-live"))
        .into()
}

/// 权威终态视图：按文档块顺序渲染（正文/表格/代码块交错），
/// 连续段落合并进同一 RichTextBlock，遇表格/代码块断开。
fn final_view(turn_idx: usize, round_num: u32, rich: &RichTextOutput) -> Element {
    let mut items: Vec<Element> = Vec::new();
    // 连续段落累积；遇 Table/Code flush 成 RichTextBlock。
    let mut pending: Vec<RichTextParagraph> = Vec::new();
    let flush = |items: &mut Vec<Element>, pending: &mut Vec<RichTextParagraph>| {
        if pending.is_empty() {
            return;
        }
        let mut rt = RichTextBlock::new();
        rt.paragraphs = std::mem::take(pending);
        rt.text_wrapping = TextWrapping::Wrap;
        rt.is_text_selection_enabled = true;
        items.push(rt.into());
    };
    if !rich.blocks.is_empty() {
        for b in &rich.blocks {
            match b {
                markdown_winui::FinalBlock::Paragraph(p) => pending.push(p.clone()),
                markdown_winui::FinalBlock::Table(td) => {
                    flush(&mut items, &mut pending);
                    items.push(markdown_winui::table_view(
                        td,
                        &format!("t{turn_idx}r{round_num}-table-{n}", n = items.len()),
                    ));
                }
                markdown_winui::FinalBlock::Code(code) => {
                    flush(&mut items, &mut pending);
                    items.push(deepx_fluent::code_surface(
                        code.lang.as_deref().unwrap_or(""),
                        &code.text,
                        format!("t{turn_idx}r{round_num}-code-{n}", n = items.len()),
                    ));
                }
            }
        }
        flush(&mut items, &mut pending);
    } else {
        // 降级路径（blocks 为空的历史数据）：按通道渲染，保底不空白。
        let mut rt = RichTextBlock::new();
        rt.paragraphs = rich.paragraphs.clone();
        rt.text_wrapping = TextWrapping::Wrap;
        rt.is_text_selection_enabled = true;
        items.push(rt.into());
        for (ti, table) in rich.tables.iter().enumerate() {
            items.push(markdown_winui::table_view(
                table,
                &format!("t{turn_idx}r{round_num}-table-{ti}"),
            ));
        }
        for (ci, code) in rich.code_blocks.iter().enumerate() {
            items.push(deepx_fluent::code_surface(
                code.lang.as_deref().unwrap_or(""),
                &code.text,
                format!("t{turn_idx}r{round_num}-code-{ci}"),
            ));
        }
    }
    vstack(items)
        .spacing(tokens::SPACE_3)
        .transition(motion::content_enter(), None)
        .with_key(format!("t{turn_idx}r{round_num}-final"))
        .into()
}

/// 工具卡（流式累积，id 稳定）。折叠器承载：
/// header = 状态 + 工具名；展开 = 参数 raw（DeepX 工具）或
/// 执行状态（provider 内建工具，如 web_search 搜索中…）。
fn tool_card(turn_idx: usize, round_num: u32, card: &markdown_winui::ToolCardView) -> Element {
    let status = if card.done {
        "已完成"
    } else {
        "正在运行"
    };
    let name = card.name.as_deref().unwrap_or("<解析中>");
    // 展开区：provider 卡显示状态文案（args_display 承载），其余显示参数。
    let body: Element = if card.args_display.trim().is_empty() {
        text_block("").font_size(tokens::TYPE_CAPTION).into()
    } else {
        text_block(&card.args_display)
            .font_size(tokens::TYPE_CAPTION)
            .wrap()
            .selectable()
            .into()
    };
    Expander::new(body)
        .header(format!("{status} · {name}"))
        .expanded(false)
        .tooltip(format!("展开或折叠工具详情：{name}"))
        .automation_name(format!("工具 {name}，{status}"))
        .automation_id(format!("chat-tool-{}", card.id))
        .transition(motion::reveal(), motion::content_exit())
        .with_key(format!("t{turn_idx}r{round_num}-card-{}", card.id))
        .into()
}

/// 诊断日志（窗口程序无控制台：写 %TEMP%）。
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
