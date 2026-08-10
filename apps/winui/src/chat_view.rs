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
//! Mermaid 与代码高亮均走 Rust + 原生 XAML；数学公式仍按字面文本降级。

use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use deepx_fluent::{StatusTone, motion, tokens};
use markdown_winui::{
    AnswerView, LiveSegment, RichTextOutput, RoundView, Transcript, TurnStatus, TurnView,
};
use windows_reactor::*;

use crate::bridge::Bridge;
use crate::chat_adapter;

/// XAML 提交批次：事件泵由 `CompositionTarget::Rendering`（vsync 对齐）
/// 驱动——60Hz 屏 60 次/s、120Hz 屏 120 次/s，天然跟随客户显示器刷新
/// 率（DispatcherTimer 受系统时钟 15.6ms 粒度限制，最多 ~64fps，
/// 120Hz 屏只有一半帧率——"渲染慢"吐槽的根因）。队列中的 token delta
/// 先合并，再在 UI 线程的一次 retained-mode 更新中提交；不逐 token
/// 触碰控件树。
///
/// 跟随尾部滚动请求节流：live 流式期间 100ms 一次贴底请求。vsync 泵每
/// 帧都请求会让滚动与用户滚轮/滚动条抢占，并形成"滚动 → 行 realize
/// → 渲染 → 再滚动"反馈循环，UI 线程满载（表现为滚动条卡死）；100ms
/// 是经实机验证的折中（原 50ms 在长文本流式下仍会触发卡顿）。结构性
/// 变化（restore / 新 turn / round 完成）不受此限，立即滚底。
const SCROLL_REQUEST_THROTTLE: Duration = Duration::from_millis(100);

/// Live（非结构性）正文提交间隔：约 15fps。协议事件仍逐帧归并到模型，
/// 但 growing markdown / Element / XAML 文本只按此频率提交；结构变化
/// （新 turn、工具卡、round 完成）仍立即提交。
///
/// 66ms 经 DEEPX_PERF_LOG 实测校准：稳态单次提交 reconcile 7-17ms
/// （created 70-104 个 XAML 控件；turn memo 已把全量重建降为增量），
/// 33ms 下 30fps × ~12ms ≈ 50% CPU 持续满载（输入排队卡顿）；66ms
/// 减半到 ~25%，流式 15fps 逐字仍流畅（视觉 ≥24fps 无感）。
const LIVE_RENDER_INTERVAL: Duration = Duration::from_millis(66);

/// 单帧事件上限与 reducer 时间预算。数量上限限制极端微小事件，时间预算
/// 则限制长 checkpoint / markdown 解析；任一先到即把余量留给下一帧。
const CHAT_EVENTS_PER_FRAME: usize = 512;
const CHAT_REDUCER_BUDGET: Duration = Duration::from_millis(4);

/// transport drain 节流：事件密集期 60fps 每帧 drain 512 + reducer 4ms
/// 预算 ≈ 98% CPU（DEEPX_PERF_LOG 实测渲染仅占 2.5%，瓶颈在帧回调的
/// 事件处理）。32ms 节流 → 30fps 批处理，事件多等一帧（≤32ms）无感；
/// 结构事件（restore/新 turn）同队列最多延迟 32ms，可接受。
const DRAIN_INTERVAL: Duration = Duration::from_millis(32);

/// 后台时长超过此值（窗口不可见，vsync 回调暂停）→ 恢复时走快照
/// resume：丢弃积压增量事件，拉 timeline 快照一次到位（复用 restore
/// 分支，毫秒级），而不是重放后台期间积压的增量（可能数千条）。
const BACKGROUND_RESUME_AFTER: Duration = Duration::from_secs(30);

/// 显式跟随状态机：Following = 新内容贴底；Idle = 用户上滚离开
/// （浮层"回到最新"按钮出现；点击 → force_tail 回底 + 重新跟随）。
/// 进入跟随必须是显式动作（点击 / 新会话 / restore 到尾部），不做
/// 每帧隐式判定——用户完全掌控"我在底部"的意图。
#[derive(Clone, Copy, PartialEq, Debug)]
enum FollowState {
    Following,
    Idle,
}

/// 顶部预加载分页大小：滚动接近窗口顶部时一次扩展的回合数。
/// 与 `markdown_winui::WINDOW_DEFAULT_LEN` 同量级，可经实机手感调优。
const WINDOW_PAGE: usize = 30;

/// near-top 判定阈值（DIPs）：滚动到距列表顶部此距离内触发预加载。
/// 与 reactor 贴底阈值（120px）对称。
const NEAR_TOP_THRESHOLD_PX: f64 = 120.0;

/// Retain a small native presentation cache so switching tabs does not blank
/// the transcript while the canonical timeline refresh is in flight.
const SESSION_CACHE_CAPACITY: usize = 8;

#[derive(Default)]
struct SessionTranscriptCache {
    entries: HashMap<String, Transcript>,
    order: VecDeque<String>,
}

impl SessionTranscriptCache {
    fn store(&mut self, seed: String, transcript: Transcript) {
        if seed.is_empty() {
            return;
        }
        self.order.retain(|item| item != &seed);
        self.order.push_back(seed.clone());
        self.entries.insert(seed, transcript);
        while self.order.len() > SESSION_CACHE_CAPACITY {
            if let Some(expired) = self.order.pop_front() {
                self.entries.remove(&expired);
            }
        }
    }

    fn restore(&mut self, seed: &str) -> Option<Transcript> {
        let transcript = self.entries.get(seed)?.clone();
        self.order.retain(|item| item != seed);
        self.order.push_back(seed.to_string());
        Some(transcript)
    }
}

/// 只合并 deferred 队尾的同目标流式事件；任何结构事件都会留在队尾，
/// 因而不会跨 turn/round/tool/final 边界重排协议顺序。
fn merge_deferred_event(
    deferred: &mut VecDeque<markdown_winui::ConversationEvent>,
    event: markdown_winui::ConversationEvent,
) {
    use markdown_winui::ConversationEvent;

    match event {
        ConversationEvent::RoundDelta {
            turn_id,
            round_num,
            kind,
            delta,
        } => {
            if let Some(ConversationEvent::RoundDelta {
                turn_id: previous_turn,
                round_num: previous_round,
                kind: previous_kind,
                delta: previous_delta,
            }) = deferred.back_mut()
                && *previous_turn == turn_id
                && *previous_round == round_num
                && *previous_kind == kind
            {
                previous_delta.push_str(&delta);
            } else {
                deferred.push_back(ConversationEvent::RoundDelta {
                    turn_id,
                    round_num,
                    kind,
                    delta,
                });
            }
        }
        checkpoint @ ConversationEvent::BlockCheckpoint { .. } => {
            let replaces_previous = deferred.back().is_some_and(|previous| {
                same_stream_target(previous, &checkpoint)
                    && matches!(
                        previous,
                        ConversationEvent::RoundDelta { .. }
                            | ConversationEvent::BlockCheckpoint { .. }
                    )
            });
            if replaces_previous {
                deferred.pop_back();
            }
            deferred.push_back(checkpoint);
        }
        other => deferred.push_back(other),
    }
}

fn same_stream_target(
    previous: &markdown_winui::ConversationEvent,
    next: &markdown_winui::ConversationEvent,
) -> bool {
    use markdown_winui::ConversationEvent;

    fn target(event: &ConversationEvent) -> Option<(&str, u32, markdown_winui::RoundDeltaKind)> {
        match event {
            ConversationEvent::RoundDelta {
                turn_id,
                round_num,
                kind,
                ..
            }
            | ConversationEvent::BlockCheckpoint {
                turn_id,
                round_num,
                kind,
                ..
            } => Some((turn_id.as_str(), *round_num, *kind)),
            _ => None,
        }
    }

    target(previous) == target(next) && target(next).is_some()
}

pub fn chat_view(cx: &mut RenderCx, bridge: Arc<Bridge>) -> Element {
    let color_scheme = cx.use_color_scheme();
    let transcript = cx.use_ref::<Transcript>(Transcript::new());
    let frame_revoker = cx.use_ref::<Option<EventRevoker>>(None);
    let last_rev = cx.use_ref::<u64>(0);
    let last_seed = cx.use_ref::<String>(String::new());
    let session_cache = cx.use_ref::<SessionTranscriptCache>(SessionTranscriptCache::default());
    let session_viewports = cx.use_ref::<HashMap<String, TemplatedViewport>>(HashMap::new());
    let pending_viewport_restore = cx.use_ref::<Option<(String, u64, TemplatedViewport)>>(None);
    // 显式跟随状态机：按钮/会话切换进入，on_view_changed 离开驱动
    let follow_state = cx.use_ref::<FollowState>(FollowState::Following);
    // 数据规模诊断日志节流基准（5s；见 timer 泵内的 scale 快照）
    let last_scale_log = cx.use_ref::<std::time::Instant>(std::time::Instant::now());
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
    // deferred（快照 seed 不匹配）日志限频：vsync 泵每帧都会命中，
    // 不节流会刷爆日志（spawn_timeline_refresh 本身有 1s 节流）。
    let last_deferred_log = cx.use_ref::<std::time::Instant>(std::time::Instant::now());
    // UI 提交代次与 transport rev 解耦：seed、快照、分页和事件批次都可
    // 独立提交，不会因构造 rev 与下一条传输 rev 碰撞而漏帧。
    let render_generation = cx.use_ref::<u64>(0);
    let (_, set_render_generation) = cx.use_state::<u64>(0);
    // Live 正文提交节流基准；Structural 变更立即提交。
    let last_live_render = cx.use_ref::<std::time::Instant>(std::time::Instant::now());
    // 跨帧 reducer 余量：bridge 的数量限额先取一批，再按 UI 时间预算
    // 分段应用；未处理事件保留在此，下一次 CompositionTarget 帧继续。
    let deferred_events =
        cx.use_ref::<VecDeque<markdown_winui::ConversationEvent>>(VecDeque::new());
    // 后台恢复检测：上一次 vsync 帧回调时刻。窗口最小化/不可见时合成器
    // 暂停回调，恢复可见后首帧 elapsed 即后台时长——超过阈值走"快照
    // 恢复"（丢弃积压增量 + 拉 timeline 快照一次到位），而非重放后台
    // 期间积压的几千条增量事件。vsync 未停（可见）则增量正常消费，
    // 此检测永不触发——逻辑自洽。
    let last_frame_at = cx.use_ref::<std::time::Instant>(std::time::Instant::now());
    // 本"可见期"是否已做过后台恢复（每帧检测，只处理一次）。
    let background_resumed = cx.use_ref::<bool>(false);
    // 后台恢复进行中（`Some(seed)` = 快照已重拉、尚未到达）：渲染
    // shimmer 加载覆盖层盖住内容（不白屏）；快照 restore 后清除。
    let recovering = cx.use_ref::<Option<String>>(None);
    // 覆盖层 shimmer 光带相位（90ms 步进，覆盖层存活期间每帧递增）。
    let shimmer_tick = cx.use_ref::<u64>(0);
    // 锚定补偿挂起标记：`Some(rows)` = 本帧渲染需用 within 滚动（顶部
    // 预加载后把「原窗口首行」锚回原位，视口不跳）。rows = 扩展前移量
    // = 原首行的新下标。渲染闭包 take 后随 preserve_anchor 下发。
    let pending_anchor = cx.use_ref::<Option<usize>>(None);
    // 窗口快照缓存：按 Transcript 投影 rev + theme 复用整包；有变化时
    // 按 turn_id + turn mutation_rev 结构共享未变化行。主题纳入缓存键，
    // 避免 ActualThemeChanged 时仍把旧主题的 realized 行当作同一数据包。
    let window_cache = cx.use_ref::<Option<((u64, ColorScheme), Rc<Vec<Rc<TurnView>>>)>>(None);
    // transport drain 节流闸：事件密集期降批处理频率（见 DRAIN_INTERVAL）。
    let last_drain = cx.use_ref::<Option<std::time::Instant>>(None);

    // 事件泵：drain bridge 队列 → Transcript；rev 变化触发重渲染。
    cx.use_effect((), {
        let bridge = bridge.clone();
        let transcript = transcript.clone();
        let frame_revoker = frame_revoker.clone();
        let last_rev = last_rev.clone();
        let last_seed = last_seed.clone();
        let session_cache = session_cache.clone();
        let session_viewports = session_viewports.clone();
        let pending_viewport_restore = pending_viewport_restore.clone();
        let last_restored_seed = last_restored_seed.clone();
        let last_deferred_log = last_deferred_log.clone();
        let last_scroll_request = last_scroll_request.clone();
        let scroll_version = scroll_version.clone();
        let force_tail_version = force_tail_version.clone();
        let follow_state = follow_state.clone();
        let last_scale_log = last_scale_log.clone();
        let pending_anchor = pending_anchor.clone();
        let render_generation = render_generation.clone();
        let set_render_generation = set_render_generation.clone();
        let last_drain = last_drain.clone();
        let last_live_render = last_live_render.clone();
        let deferred_events = deferred_events.clone();
        let last_frame_at = last_frame_at.clone();
        let background_resumed = background_resumed.clone();
        let recovering = recovering.clone();
        let shimmer_tick = shimmer_tick.clone();
        move || {
            if frame_revoker.borrow().is_some() {
                return;
            }
            match on_frame({
                let bridge = bridge.clone();
                let transcript = transcript.clone();
                let last_rev = last_rev.clone();
                let last_seed = last_seed.clone();
                let session_cache = session_cache.clone();
                let session_viewports = session_viewports.clone();
                let pending_viewport_restore = pending_viewport_restore.clone();
                let last_restored_seed = last_restored_seed.clone();
                let last_deferred_log = last_deferred_log.clone();
                let last_scroll_request = last_scroll_request.clone();
                let scroll_version = scroll_version.clone();
                let force_tail_version = force_tail_version.clone();
                let render_generation = render_generation.clone();
                let set_render_generation = set_render_generation.clone();
                let last_live_render = last_live_render.clone();
                let deferred_events = deferred_events.clone();
                let last_frame_at = last_frame_at.clone();
                let background_resumed = background_resumed.clone();
                let recovering = recovering.clone();
                let shimmer_tick = shimmer_tick.clone();
                let pending_anchor = pending_anchor.clone();
                move || {
                    // 后台恢复检测：窗口不可见时 vsync 回调暂停，恢复可见
                    // 后首帧 elapsed = 后台时长。超过阈值 → 丢弃积压增量
                    // （快照会覆盖 transcript）+ 清 last_restored_seed 触发
                    // 快照重拉 → 复用 restore 分支一次到位（毫秒级）。
                    // 同时点亮 shimmer 覆盖层（快照到达 restore 后熄灭）。
                    let now = std::time::Instant::now();
                    if now.duration_since(*last_frame_at.borrow())
                        >= BACKGROUND_RESUME_AFTER
                        && !*background_resumed.borrow()
                    {
                        *background_resumed.borrow_mut() = true;
                        bridge.core().chat_drain();
                        deferred_events.borrow_mut().clear();
                        bridge.core().chat_output_drain();
                        *last_restored_seed.borrow_mut() = String::new();
                        *recovering.borrow_mut() = Some(bridge.core().active_seed());
                        log_diag("chat_view: background resume (drop backlog + snapshot refresh)");
                    }
                    *last_frame_at.borrow_mut() = now;
                    // DeepX perf diagnostic: data-scale snapshot every 5s
                    // (turns in memory vs windowed render rows vs viewports).
                    if now.duration_since(*last_scale_log.borrow())
                        >= std::time::Duration::from_secs(5)
                    {
                        *last_scale_log.borrow_mut() = now;
                        let t = transcript.borrow();
                        log_diag(&format!(
                            "chat_view scale: turns={} window={} rev={} viewports={}",
                            t.turns().len(),
                            t.window_len(),
                            t.mutation_rev(),
                            session_viewports.borrow().len(),
                        ));
                    }
                    // 会话切换：缓存旧会话的原生投影，立即恢复目标会话；
                    // canonical 快照仍会异步刷新并覆盖缓存。
                    let seed = bridge.core().active_seed();
                    if seed != *last_seed.borrow() {
                        let previous_seed = last_seed.borrow().clone();
                        if !previous_seed.is_empty() {
                            let mut cached = transcript.borrow().clone();
                            cached.trim_to_window();
                            session_cache
                                .borrow_mut()
                                .store(previous_seed, cached);
                        }
                        *last_seed.borrow_mut() = seed.clone();
                        // UI 侧尚未归并的旧会话事件不能跨 seed 留存。bridge
                        // 队列有 seed 过滤，但本地 deferred 队列也必须清空。
                        deferred_events.borrow_mut().clear();
                        let cached = session_cache.borrow_mut().restore(&seed);
                        *transcript.borrow_mut() = cached.clone().unwrap_or_else(Transcript::new);
                        *last_restored_seed.borrow_mut() = if cached.is_some() {
                            seed.clone()
                        } else {
                            String::new()
                        };
                        *last_rev.borrow_mut() = 0;
                        *scroll_version.borrow_mut() += 1;
                        let generation = *scroll_version.borrow();
                        let viewport = session_viewports.borrow().get(&seed).copied();
                        *pending_viewport_restore.borrow_mut() = viewport
                            .filter(|viewport| !viewport.following_tail)
                            .map(|viewport| (seed.clone(), generation, viewport));
                        *force_tail_version.borrow_mut() = match viewport {
                            Some(viewport) if viewport.following_tail => Some(generation),
                            None if cached.is_some() => Some(generation),
                            _ => None,
                        };
                        // 显式状态机：只有"用户之前滚走"的会话恢复为 Idle
                        // （浮层按钮出现）；其余（全新/尾部离开）进入跟随。
                        *follow_state.borrow_mut() = match &viewport {
                            Some(v) if !v.following_tail => FollowState::Idle,
                            _ => FollowState::Following,
                        };
                        *render_generation.borrow_mut() += 1;
                        set_render_generation.call(*render_generation.borrow());
                        if !seed.is_empty() {
                            bridge.core().spawn_timeline_refresh(&seed);
                        }
                        log_diag(&format!("chat_view: switched to seed {seed}"));
                    }
                    // drain 节流：事件密集时 60fps 每帧 drain 512 + reducer 4ms
                    // ≈ 98% CPU（瓶颈在事件处理，不在渲染）。32ms 节流 →
                    // 30fps 批处理；deferred 非空时仍每帧放行小批（/8）避免
                    // 结构事件被 32ms 闸整体堵死（restore/新 turn 及时性）。
                    let drain_gate = last_drain
                        .borrow()
                        .is_none_or(|t| now.duration_since(t) >= DRAIN_INTERVAL);
                    if drain_gate {
                        *last_drain.borrow_mut() = Some(now);
                    }
                    let fetch_limit = if !drain_gate && deferred_events.borrow().is_empty() {
                        0
                    } else if deferred_events.borrow().is_empty() {
                        CHAT_EVENTS_PER_FRAME
                    } else {
                        CHAT_EVENTS_PER_FRAME / 4
                    };
                    let (events, rev) = bridge.core().chat_drain_limit(fetch_limit);
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
                            let preserve_viewport = pending_viewport_restore
                                .borrow()
                                .as_ref()
                                .is_some_and(|(restore_seed, _, _)| restore_seed == &seed);
                            let prior_window_len = transcript.borrow().window_len();
                            let mut transcript = transcript.borrow_mut();
                            transcript.restore(turns);
                            let restored_window_len = transcript.window_len();
                            if preserve_viewport && prior_window_len > restored_window_len {
                                transcript.expand_window(prior_window_len - restored_window_len);
                            }
                            drop(transcript);
                            *last_restored_seed.borrow_mut() = seed.clone();
                            // 后台恢复完成：熄灭 shimmer 覆盖层（快照内容已
                            // 就位 + 贴底滚动在同一帧下发，无白屏窗口）。
                            if recovering.borrow().as_ref() == Some(&seed) {
                                *recovering.borrow_mut() = None;
                                log_diag("chat_view: recovery overlay dismissed");
                            }
                            // Fresh restore defaults to the tail. A cached
                            // session that the user had scrolled away from
                            // keeps its captured native viewport instead.
                            let now = std::time::Instant::now();
                            *scroll_version.borrow_mut() += 1;
                            if !preserve_viewport {
                                *force_tail_version.borrow_mut() = Some(*scroll_version.borrow());
                            }
                            // restore 到用户滚走的位置 → 保持 Idle（按钮回底）；
                            // 否则贴底并进入跟随。
                            *follow_state.borrow_mut() = if preserve_viewport {
                                FollowState::Idle
                            } else {
                                FollowState::Following
                            };
                            *last_scroll_request.borrow_mut() = now;
                            *render_generation.borrow_mut() += 1;
                            set_render_generation.call(*render_generation.borrow());
                            log_diag(&format!("chat_view: restored {n} turns for {seed}"));
                        } else {
                            // 快照 seed 不匹配（旧会话残留/并发交错）：主动重拉
                            // active seed（1s 节流在 bridge 内）。日志限频，
                            // 避免 vsync 泵每帧刷屏。
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
                    // 2) 增量事件（新对话流式）。先映射进跨帧队列，再按
                    // reducer 时间预算应用；积压显式保留，下一帧继续。
                    if rev != *last_rev.borrow() {
                        *last_rev.borrow_mut() = rev;
                    }
                    if !events.is_empty() {
                        let mapped = events
                            .iter()
                            .filter_map(|event| chat_adapter::render_event(event));
                        let mut deferred = deferred_events.borrow_mut();
                        for event in mapped {
                            merge_deferred_event(&mut deferred, event);
                        }
                    }
                    let reducer_started = std::time::Instant::now();
                    let mut update = markdown_winui::TranscriptChange::default();
                    let pending_outputs = {
                        let mut t = transcript.borrow_mut();
                        while reducer_started.elapsed() < CHAT_REDUCER_BUDGET {
                            let Some(event) = deferred_events.borrow_mut().pop_front() else {
                                break;
                            };
                            update.merge(t.apply_coalesced(event));
                        }
                        t.take_pending_outputs()
                    };
                    if update.changed() || !pending_outputs.is_empty() {
                        for pending in pending_outputs {
                            bridge.core().spawn_resolve_chat_output(&seed, pending);
                        }
                        if update.changed() {
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
                            if structural
                                || now.duration_since(*last_live_render.borrow())
                                    >= LIVE_RENDER_INTERVAL
                            {
                                *last_live_render.borrow_mut() = now;
                                *render_generation.borrow_mut() += 1;
                                set_render_generation.call(*render_generation.borrow());
                            }
                        }
                    }
                    // 覆盖层/加载态存活期间（后台恢复未完成，或快照未达
                    // 的加载空态）：每帧推进 shimmer 光带相位并强制渲染
                    // （无事件也要动起来）。加载态判定与渲染分支一致：
                    // turns 空 + seed 非空 + 尚未 restore。
                    let loading_state = {
                        let t = transcript.borrow();
                        !seed.is_empty()
                            && t.turns().is_empty()
                            && *last_restored_seed.borrow() != seed
                    };
                    if recovering.borrow().is_some() || loading_state {
                        *shimmer_tick.borrow_mut() += 1;
                        *render_generation.borrow_mut() += 1;
                        set_render_generation.call(*render_generation.borrow());
                    }
                }
            }) {
                Ok(r) => {
                    *frame_revoker.borrow_mut() = Some(r);
                    log_diag("chat_view: vsync pump started (CompositionTarget.Rendering)");
                }
                Err(e) => log_diag(&format!("chat_view: on_frame failed: {e}")),
            }
        }
    });

    // 投影渲染（reactor diff：只更新变化节点）。
    // 内容态：ListView——WinUI 原生虚拟化，行内容只在滚入视口时构建
    // （长会话不再全量渲染，view 闭包只对 realized 行调用）+ 声明式滚动
    // 请求（跟随尾部：restore 滚底 / 新 turn / live 增长；near_bottom
    // 120px 判定在 reconciler/backend，用户上滚浏览历史时不打扰）。
    let active_seed = bridge.core().active_seed();
    let s = transcript.borrow();
    if s.turns().is_empty() {
        // 空态：无 active seed = 新对话；有 seed 但快照未 restore = 加载中；
        // 快照已 restore 但 turns 为空 = 全新空会话（或已清空），非加载中。
        let label = if active_seed.is_empty() {
            "开始新的对话…"
        } else if *last_restored_seed.borrow() == active_seed {
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
        // 加载中统一用 shimmer 覆盖层（与后台恢复同款：转圈 + 光带
        // 流动，加载全程有动画，不白屏）。shimmer 相位由 vsync 泵在
        // 加载态期间每帧推进（见回调末尾的 shimmer 驱动）。
        if busy {
            let tick = *shimmer_tick.borrow();
            return deepx_fluent::loading_overlay("chat-loading", title, "加载中，请稍候", tick)
                .automation_name(title)
                .automation_id("chat-loading")
                .with_key(format!("chat-loading-{active_seed}"));
        }
        return deepx_fluent::empty_state(title, detail, busy)
            .transition(motion::session_enter(), motion::session_exit())
            .automation_name(title)
            .automation_id("chat-empty")
            .with_key(format!("chat-empty-{active_seed}"));
    }
    let turns: Rc<Vec<Rc<TurnView>>> = {
        let cache_key = (s.mutation_rev(), color_scheme);
        let mut cache = window_cache.borrow_mut();
        match cache.as_ref() {
            Some((key, rc)) if *key == cache_key => rc.clone(),
            _ => {
                let previous_by_id: HashMap<&str, &Rc<TurnView>> = cache
                    .as_ref()
                    .map(|(_, rows)| {
                        rows.iter()
                            .map(|turn| (turn.turn_id.as_str(), turn))
                            .collect()
                    })
                    .unwrap_or_default();
                let rows: Vec<Rc<TurnView>> = s
                    .window_turns()
                    .iter()
                    .map(|turn| {
                        previous_by_id
                            .get(turn.turn_id.as_str())
                            .filter(|old| old.mutation_rev == turn.mutation_rev)
                            .cloned()
                            .cloned()
                            .unwrap_or_else(|| Rc::new(turn.clone()))
                    })
                    .collect();
                let rc = Rc::new(rows);
                *cache = Some((cache_key, rc.clone()));
                rc
            }
        }
    };
    // 顶部预加载后本帧需要锚定补偿：取走挂起标记，把
    // 「原窗口首行」（新下标 = 扩展前移量）锚回原位，视口不跳。
    let anchor_rows = pending_anchor.borrow_mut().take();
    let mut builder = list_view_rc(turns, move |turn: &Rc<TurnView>, i: usize| {
        // turn 级 memo：mutation_rev 未变 → reconciler 完全跳过 turn_view
        // （不深克隆 RichTextParagraph / 不重跑代码高亮 / 不 diff 行内部），
        // 只有本 turn 内容变化才重建。每次 XAML 提交从「重建整个可见
        // 窗口」降为「重建变化的 1 行」——这是输入卡顿/CPU 满载的根治。
        memo(
            turn_memo,
            TurnProps {
                turn: turn.clone(),
                index: i,
                color_scheme: color_scheme.clone(),
            },
        )
        .with_key(turn.turn_id.clone())
    })
    .with_key_selector(|turn: &Rc<TurnView>| turn.turn_id.clone())
    .selection_mode(SelectionMode::None);
    if let Some(anchor_rows) = anchor_rows {
        builder = builder.preserve_anchor(*scroll_version.borrow(), anchor_rows, 0.0);
    } else if let Some((seed, generation, viewport)) =
        pending_viewport_restore.borrow().as_ref().cloned()
        && seed == active_seed
    {
        builder = builder.restore_offset(
            generation,
            viewport.vertical_offset,
            viewport.following_tail,
        );
    } else if *force_tail_version.borrow() == Some(*scroll_version.borrow()) {
        force_tail_version.borrow_mut().take();
        builder = builder.force_tail(*scroll_version.borrow());
    } else if *follow_state.borrow() == FollowState::Following {
        // 显式跟随：仅 Following 态下发（Idle = 用户上滚，不打扰）
        builder = builder.follow_tail(*scroll_version.borrow());
    }
    let viewport_seed = active_seed.clone();
    let transcript_list: Element = builder
        .on_view_changed({
            let follow_state = follow_state.clone();
            let render_generation = render_generation.clone();
            let set_render_generation = set_render_generation.clone();
            let session_viewports = session_viewports.clone();
            let pending_viewport_restore = pending_viewport_restore.clone();
            move |viewport: TemplatedViewport| {
                // 显式状态机驱动：backend 的 near-tail 判定 → 状态切换 →
                // 重渲染（浮层按钮显隐）。状态切换不产生滚动请求，无反馈环。
                let next = if viewport.following_tail {
                    FollowState::Following
                } else {
                    FollowState::Idle
                };
                if *follow_state.borrow() != next {
                    *follow_state.borrow_mut() = next;
                    *render_generation.borrow_mut() += 1;
                    set_render_generation.call(*render_generation.borrow());
                }
                session_viewports
                    .borrow_mut()
                    .insert(viewport_seed.clone(), viewport);
                let restored = pending_viewport_restore.borrow().as_ref().is_some_and(
                    |(seed, _, requested)| {
                        seed == &viewport_seed
                            && (requested.vertical_offset - viewport.vertical_offset).abs() <= 1.0
                    },
                );
                if restored {
                    pending_viewport_restore.borrow_mut().take();
                }
            }
        })
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
        .with_key(format!("chat-transcript-{active_seed}"))
        .into();
    // 显式跟随入口：Idle 时浮层"回到最新"按钮（点击 → force_tail +
    // 重新跟随；跟随态隐藏，不挡内容）
    let jump_button: Element = if *follow_state.borrow() == FollowState::Idle {
        let btn: Element = button("回到最新 ↓")
            .on_click({
                let follow_state = follow_state.clone();
                let scroll_version = scroll_version.clone();
                let force_tail_version = force_tail_version.clone();
                let render_generation = render_generation.clone();
                let set_render_generation = set_render_generation.clone();
                move || {
                    *scroll_version.borrow_mut() += 1;
                    *force_tail_version.borrow_mut() = Some(*scroll_version.borrow());
                    *follow_state.borrow_mut() = FollowState::Following;
                    *render_generation.borrow_mut() += 1;
                    set_render_generation.call(*render_generation.borrow());
                }
            })
            .into();
        btn.vertical_alignment(VerticalAlignment::Bottom)
            .horizontal_alignment(HorizontalAlignment::Center)
            .margin(Thickness {
                left: 0.0,
                top: 0.0,
                right: 0.0,
                bottom: tokens::SPACE_4,
            })
            .with_key("jump-tail")
    } else {
        Element::Empty
    };
    // 后台恢复覆盖层：快照未到达期间盖住内容（半透明 + 转圈 + shimmer
    // 光带），不白屏不闪烁；restore 完成后同帧熄灭（见恢复分支）。
    let recovery_overlay: Element = if recovering.borrow().as_ref() == Some(&active_seed) {
        let tick = *shimmer_tick.borrow();
        deepx_fluent::loading_overlay("recovery-overlay", "正在恢复对话…", "恢复中，请稍候", tick)
    } else {
        Element::Empty
    };
    let transcript: Element = grid([transcript_list, jump_button, recovery_overlay])
        .columns([GridLength::Star(1.0)])
        .rows([GridLength::Star(1.0)])
        .into();
    transcript
        .transition(motion::session_enter(), motion::session_exit())
        .automation_name("对话记录")
        .automation_id("chat-transcript")
}

#[cfg(test)]
mod session_cache_tests {
    use super::*;
    use markdown_winui::{ConversationEvent, RoundDeltaKind};

    #[test]
    fn deferred_queue_merges_across_bridge_drains() {
        let mut deferred = VecDeque::new();
        for delta in ["a", "b"] {
            merge_deferred_event(
                &mut deferred,
                ConversationEvent::RoundDelta {
                    turn_id: "t1".into(),
                    round_num: 0,
                    kind: RoundDeltaKind::Answering,
                    delta: delta.into(),
                },
            );
        }
        assert_eq!(deferred.len(), 1);
        assert!(matches!(
            deferred.front(),
            Some(ConversationEvent::RoundDelta { delta, .. }) if delta == "ab"
        ));
        merge_deferred_event(
            &mut deferred,
            ConversationEvent::BlockCheckpoint {
                turn_id: "t1".into(),
                round_num: 0,
                kind: RoundDeltaKind::Answering,
                text: "authoritative".into(),
            },
        );
        assert_eq!(deferred.len(), 1);
        assert!(matches!(
            deferred.front(),
            Some(ConversationEvent::BlockCheckpoint { text, .. })
                if text == "authoritative"
        ));
    }

    #[test]
    fn cache_restores_recent_transcript_and_evicts_oldest() {
        let mut cache = SessionTranscriptCache::default();
        for i in 0..=SESSION_CACHE_CAPACITY {
            let mut transcript = Transcript::new();
            transcript.apply(&markdown_winui::ConversationEvent::TurnStarted {
                turn_id: format!("t{i}"),
                user_text: format!("q{i}"),
            });
            cache.store(format!("s{i}"), transcript);
        }
        assert!(cache.restore("s0").is_none());
        assert_eq!(cache.restore("s8").unwrap().turns()[0].turn_id, "t8");
    }
}

// ── turn / round / answer 渲染（移植自 streaming-demo）─────────────

/// turn 级 memo 的 props。等价 = 同一份 TurnView 数据（Rc 指针命中，或
/// turn_id + mutation_rev 相同）+ 位置 + 主题。memo 命中时 reconciler
/// 复用上次渲染输出（不执行组件函数），行 diff 也直接走 props 比较。
#[derive(Clone)]
struct TurnProps {
    turn: Rc<TurnView>,
    index: usize,
    color_scheme: ColorScheme,
}

impl PartialEq for TurnProps {
    fn eq(&self, other: &Self) -> bool {
        self.index == other.index
            && self.color_scheme == other.color_scheme
            && (Rc::ptr_eq(&self.turn, &other.turn)
                || (self.turn.turn_id == other.turn.turn_id
                    && self.turn.mutation_rev == other.turn.mutation_rev))
    }
}

/// turn 渲染的 memo 组件：props 等价时 reconciler 跳过执行（复用旧输出）。
fn turn_memo(props: &TurnProps, _cx: &mut RenderCx) -> Element {
    turn_view(props.index, props.turn.as_ref(), props.color_scheme.clone())
}

fn turn_view(i: usize, turn: &TurnView, color_scheme: ColorScheme) -> Element {
    let (status, tone) = match turn.status {
        TurnStatus::Running => ("正在处理", StatusTone::Running),
        TurnStatus::Completed => ("已完成", StatusTone::Success),
        TurnStatus::Failed => ("失败", StatusTone::Critical),
    };
    let mut items: Vec<Element> = vec![deepx_fluent::user_message(
        text_block(&turn.user_text)
            .font_size(tokens::TYPE_BODY)
            .line_height(tokens::TYPE_BODY_LINE_HEIGHT)
            .wrap()
            .selectable(),
        deepx_fluent::status_badge(status, tone),
    )];
    if turn.status == TurnStatus::Failed {
        if let Some(error) = &turn.failed_error {
            items.push(
                text_block(format!("回合失败：{error}"))
                    .font_size(tokens::TYPE_CAPTION)
                    .foreground(ThemeRef::SystemCritical)
                    .wrap()
                    .selectable()
                    .automation_name("回合失败详情")
                    .with_key(format!("t{i}-failed-error"))
                    .into(),
            );
        }
    }
    for round in &turn.rounds {
        items.push(round_view(i, round, color_scheme));
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

fn round_view(turn_idx: usize, round: &RoundView, color_scheme: ColorScheme) -> Element {
    let mut items: Vec<Element> = Vec::new();
    if let Some(thinking) = &round.thinking {
        // 斜体 RichTextBlock（思考链专用；TextBlock 不支持 FontStyle）。
        let mut run = RichTextRun::plain(thinking);
        run.is_italic = true;
        let mut rt = RichTextBlock::new();
        rt.paragraphs = vec![RichTextParagraph::new(vec![RichTextInline::Run(run)])];
        rt.font_size = Some(tokens::TYPE_BODY);
        rt.line_height = Some(tokens::TYPE_BODY_LINE_HEIGHT);
        rt.text_wrapping = TextWrapping::Wrap;
        rt.is_text_selection_enabled = true;
        // RichTextBlock 不参与 XAML 字体属性继承（host 全局字体只覆盖
        // TextBlock 系），必须显式指定 UI 字体，否则 fallback 系统字体。
        rt.modifiers.font_family = Some(tokens::DEFAULT_UI_FONT_FAMILY.to_string());
        items.push(
            Expander::new(rt)
                .header("思考过程")
                .expanded(true)
                // WinUI's default ExpanderMinHeight is 48 DIP (settings-card
                // density). Transcript disclosure rows use the native template at
                // a compact desktop height while preserving its animation/UIA.
                .min_height(36.0)
                .padding(Thickness::xy(tokens::SPACE_3, tokens::SPACE_2))
                .tooltip("展开或折叠思考过程")
                .automation_name("思考过程")
                .automation_id(format!("chat-thinking-{turn_idx}-{}", round.round_num))
                .with_key(format!("t{turn_idx}r{}-thinking", round.round_num))
                .into(),
        );
    }
    for card in &round.tool_calls {
        items.push(tool_card(turn_idx, round.round_num, card, color_scheme));
    }
    items.push(deepx_fluent::assistant_message(answer_view(
        turn_idx,
        round.round_num,
        &round.answer,
        color_scheme,
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

fn answer_view(
    turn_idx: usize,
    round_num: u32,
    answer: &AnswerView,
    color_scheme: ColorScheme,
) -> Element {
    match answer {
        AnswerView::Streaming { segments, .. } => live_view(turn_idx, round_num, segments),
        AnswerView::Final { rich, .. } => final_view(turn_idx, round_num, rich, color_scheme),
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
                    .font_size(tokens::TYPE_BODY)
                    .line_height(tokens::TYPE_BODY_LINE_HEIGHT)
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
/// 连续段落合并进同一 RichTextBlock，遇表格/代码块/图表断开。
fn final_view(
    turn_idx: usize,
    round_num: u32,
    rich: &RichTextOutput,
    color_scheme: ColorScheme,
) -> Element {
    let mut items: Vec<Element> = Vec::new();
    // 连续段落累积；遇 Table/Code flush 成 RichTextBlock。
    let mut pending: Vec<RichTextParagraph> = Vec::new();
    let flush = |items: &mut Vec<Element>, pending: &mut Vec<RichTextParagraph>| {
        if pending.is_empty() {
            return;
        }
        let mut rt = RichTextBlock::new();
        rt.paragraphs = std::mem::take(pending);
        rt.font_size = Some(tokens::TYPE_BODY);
        rt.line_height = Some(tokens::TYPE_BODY_LINE_HEIGHT);
        rt.text_wrapping = TextWrapping::Wrap;
        rt.is_text_selection_enabled = true;
        // RichTextBlock 不参与 XAML 字体属性继承（host 全局字体只覆盖
        // TextBlock 系），必须显式指定 UI 字体，否则 fallback 系统字体。
        rt.modifiers.font_family = Some(tokens::DEFAULT_UI_FONT_FAMILY.to_string());
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
                    let highlighted = markdown_winui::highlighted_code_block(
                        code,
                        color_scheme,
                        tokens::CODE_FONT_FAMILY,
                    );
                    items.push(deepx_fluent::code_surface_content(
                        code.lang.as_deref().unwrap_or(""),
                        highlighted,
                        format!("t{turn_idx}r{round_num}-code-{n}", n = items.len()),
                    ));
                }
                markdown_winui::FinalBlock::Diagram(diagram) => {
                    flush(&mut items, &mut pending);
                    items.push(markdown_winui::diagram_view(
                        diagram,
                        color_scheme,
                        &format!("t{turn_idx}r{round_num}-diagram-{n}", n = items.len()),
                    ));
                }
            }
        }
        flush(&mut items, &mut pending);
    } else {
        // 降级路径（blocks 为空的历史数据）：按通道渲染，保底不空白。
        let mut rt = RichTextBlock::new();
        rt.paragraphs = rich.paragraphs.clone();
        rt.font_size = Some(tokens::TYPE_BODY);
        rt.line_height = Some(tokens::TYPE_BODY_LINE_HEIGHT);
        rt.text_wrapping = TextWrapping::Wrap;
        rt.is_text_selection_enabled = true;
        // RichTextBlock 不参与 XAML 字体属性继承（host 全局字体只覆盖
        // TextBlock 系），必须显式指定 UI 字体，否则 fallback 系统字体。
        rt.modifiers.font_family = Some(tokens::DEFAULT_UI_FONT_FAMILY.to_string());
        items.push(rt.into());
        for (ti, table) in rich.tables.iter().enumerate() {
            items.push(markdown_winui::table_view(
                table,
                &format!("t{turn_idx}r{round_num}-table-{ti}"),
            ));
        }
        for (ci, code) in rich.code_blocks.iter().enumerate() {
            let highlighted = markdown_winui::highlighted_code_block(
                code,
                color_scheme,
                tokens::CODE_FONT_FAMILY,
            );
            items.push(deepx_fluent::code_surface_content(
                code.lang.as_deref().unwrap_or(""),
                highlighted,
                format!("t{turn_idx}r{round_num}-code-{ci}"),
            ));
        }
        for (di, diagram) in rich.diagrams.iter().enumerate() {
            items.push(markdown_winui::diagram_view(
                diagram,
                color_scheme,
                &format!("t{turn_idx}r{round_num}-diagram-{di}"),
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
fn tool_card(
    turn_idx: usize,
    round_num: u32,
    card: &markdown_winui::ToolCardView,
    color_scheme: ColorScheme,
) -> Element {
    let status = if card.done {
        "已完成"
    } else {
        "正在运行"
    };
    let name = card.name.as_deref().unwrap_or("<解析中>");
    let content_key = format!("t{turn_idx}r{round_num}-card-{}-body", card.id);
    let body: Element = if !matches!(card.body, markdown_winui::ToolBody::Empty) {
        markdown_winui::tool_body_view(
            &card.body,
            color_scheme,
            tokens::CODE_FONT_FAMILY,
            &content_key,
        )
    } else if card.args_display.trim().is_empty() {
        text_block("").font_size(tokens::TYPE_CAPTION).into()
    } else {
        text_block(&card.args_display)
            .font_size(tokens::TYPE_CAPTION)
            .wrap()
            .selectable()
            .into()
    };
    let header = card
        .changes
        .as_ref()
        .filter(|changes| !changes.is_empty())
        .map(|changes| format!("{status} · {name} · {}", changes.label()))
        .unwrap_or_else(|| format!("{status} · {name}"));
    Expander::new(body)
        .header(header)
        .expanded(false)
        .min_height(36.0)
        .padding(Thickness::xy(tokens::SPACE_3, tokens::SPACE_2))
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
