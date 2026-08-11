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
use std::sync::{Arc, Mutex};
use std::time::Duration;

use deepx_fluent::{StatusTone, motion, tokens};
use markdown_winui::{
    AnswerView, LiveSegment, RichTextOutput, RoundView, TableHover, Transcript, TurnStatus,
    TurnView,
};
use windows_reactor::*;

use crate::bridge::Bridge;
use crate::chat_adapter;

/// 图表放大请求（壳内 UI 状态）：`final_view` 点击图表卡片时写入，
/// `main.rs` 挂载的 [`diagram_zoom_overlay`] 轮询消费（与 interaction_overlay
/// 同款「写端组件 / 读端覆盖层」分离模式，避免跨组件 props 穿透）。
#[derive(Clone, Debug, PartialEq)]
pub struct DiagramZoomRequest {
    /// 图表定位标签（turn/round 索引，作覆盖层标题）。
    pub label: String,
    /// 当前主题下的 SVG（透明背景 + text→path 后的纯 path 版）。
    pub svg: String,
    /// SVG 自然尺寸（DIP，从 `<svg width height>` 解析；fit 缩放基准）。
    pub width: f64,
    pub height: f64,
}

/// 从 SVG 头部解析 `width`/`height` 属性（usvg 输出格式固定为
/// `<svg width=".." height=".."`）。解析失败回退 800x600。
fn svg_natural_size(svg: &str) -> (f64, f64) {
    let head = svg.get(..svg.len().min(300)).unwrap_or(svg);
    let mut w = 800.0;
    let mut h = 600.0;
    if let Some(i) = head.find("width=\"") {
        if let Some(v) = head
            .get(i + 7..)
            .and_then(|rest| rest.split('"').next())
            .and_then(|s| s.parse::<f64>().ok())
        {
            w = v;
        }
    }
    if let Some(i) = head.find("height=\"") {
        if let Some(v) = head
            .get(i + 8..)
            .and_then(|rest| rest.split('"').next())
            .and_then(|s| s.parse::<f64>().ok())
        {
            h = v;
        }
    }
    (w, h)
}

pub static DIAGRAM_ZOOM: Mutex<Option<DiagramZoomRequest>> = Mutex::new(None);

/// 图表放大弹窗：视口占主窗口比例（跟随窗口尺寸动态变化）与遮罩透明度。
const DIAGRAM_ZOOM_W_RATIO: f64 = 0.78;
const DIAGRAM_ZOOM_H_RATIO: f64 = 0.72;
const SCRIM_ALPHA: u8 = 140;

/// 图表点击 → 写入放大请求（覆盖层轮询消费）。
/// `Callback<()>` 的闭包签名为 `Fn(())`（reactor 约定，invoke 传 unit）。
fn zoom_request_callback(
    diagram: &markdown_winui::DiagramBlock,
    scheme: ColorScheme,
    key: &str,
) -> windows_reactor::Callback<()> {
    let label = key.to_string();
    let svg = diagram.svg(scheme).unwrap_or_default().to_string();
    let (width, height) = svg_natural_size(&svg);
    windows_reactor::Callback::new(move |_: ()| {
        if let Ok(mut slot) = DIAGRAM_ZOOM.lock() {
            *slot = Some(DiagramZoomRequest {
                label: label.clone(),
                svg: svg.clone(),
                width,
                height,
            });
        }
    })
}

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

/// Live（非结构性）正文提交间隔：8ms 下限，与 vsync 帧泵配合时在
/// 60Hz/120Hz 屏上都可逐帧提交。协议事件先归并到模型，再由一次
/// retained-mode 更新提交 growing markdown / Element / XAML 文本；结构变化
/// （新 turn、工具卡、round 完成）仍立即提交。滚动请求继续单独按 100ms
/// 节流，避免恢复“滚动 → realize → 渲染 → 滚动”的反馈环。
const LIVE_RENDER_INTERVAL: Duration = Duration::from_millis(8);

/// 单帧事件上限与 reducer 时间预算。数量上限限制极端微小事件，时间预算
/// 则限制长 checkpoint / markdown 解析；任一先到即把余量留给下一帧。
const CHAT_EVENTS_PER_FRAME: usize = 512;
const CHAT_REDUCER_BUDGET: Duration = Duration::from_millis(4);

/// transport drain 闸：8ms 下限使 60Hz/120Hz 的 vsync 回调均可逐帧取数，
/// 避免原 32ms 批处理造成明显的成批吐字。单帧仍受 512 事件上限和 reducer
/// 4ms 墙钟预算保护；滚动与 XAML 提交还有各自独立预算。
const DRAIN_INTERVAL: Duration = Duration::from_millis(8);

/// 字符 shimmer 的相位步长。ProgressRing 由 WinUI 合成器独立动画，文本
/// 光带只需低频推进；按 vsync 每帧重建 RichText 会既过快又浪费 UI 线程。
const SHIMMER_STEP_INTERVAL: Duration = Duration::from_millis(90);

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
    // 覆盖层 shimmer 光带相位（90ms 步进）。
    let shimmer_tick = cx.use_ref::<u64>(0);
    let last_shimmer_step = cx.use_ref::<std::time::Instant>(std::time::Instant::now());
    // 锚定补偿挂起标记：`Some(rows)` = 本帧渲染需用 within 滚动（顶部
    // 预加载后把「原窗口首行」锚回原位，视口不跳）。rows = 扩展前移量
    // = 原首行的新下标。渲染闭包 take 后随 preserve_anchor 下发。
    let pending_anchor = cx.use_ref::<Option<usize>>(None);
    // 窗口快照缓存：按 Transcript 投影 rev + theme 复用整包；有变化时
    // 按 turn_id + turn mutation_rev 结构共享未变化行。主题纳入缓存键，
    // 避免 ActualThemeChanged 时仍把旧主题的 realized 行当作同一数据包。
    let window_cache = cx.use_ref::<Option<((u64, ColorScheme), Rc<Vec<Rc<TurnView>>>)>>(None);
    // transport drain 快速闸：8ms 下限，随 vsync 逐帧消费（见 DRAIN_INTERVAL）。
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
        let last_shimmer_step = last_shimmer_step.clone();
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
                let last_shimmer_step = last_shimmer_step.clone();
                let pending_anchor = pending_anchor.clone();
                move || {
                    // 后台恢复检测：窗口不可见时 vsync 回调暂停，恢复可见
                    // 后首帧 elapsed = 后台时长。超过阈值 → 丢弃积压增量
                    // （快照会覆盖 transcript）+ 清 last_restored_seed 触发
                    // 快照重拉 → 复用 restore 分支一次到位（毫秒级）。
                    // 同时点亮 shimmer 覆盖层（快照到达 restore 后熄灭）。
                    let now = std::time::Instant::now();
                    let frame_interval = now.duration_since(*last_frame_at.borrow());
                    if frame_interval >= BACKGROUND_RESUME_AFTER
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
                    // 诊断：帧间隔（>2s = 暂停恢复，diagnostics 内过滤）。
                    crate::diagnostics::record_frame_interval(frame_interval.as_secs_f64() * 1000.0);
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
                        // 诊断：数据规模快照（与 log_diag 同频）。
                        crate::diagnostics::record_scale(
                            t.turns().len() as u32,
                            t.window_len() as u32,
                            t.mutation_rev(),
                        );
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
                    // drain 快速路径：8ms 下限在 60Hz/120Hz 屏上逐帧放行，
                    // 避免 32ms 攒批造成可见的成批吐字。deferred 非空时仍
                    // 放行小批（/4），单帧总成本由事件上限和 reducer 4ms
                    // 预算兜底，结构事件也不会被 backlog 长时间阻塞。
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
                    // 诊断：drain 吞吐（chat 事件 + output 决议）。
                    crate::diagnostics::record_drain(
                        (events.len() + output_resolutions.len()) as u32,
                    );
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
                    // 的加载空态）：每 90ms 推进 shimmer 光带相位并重渲染
                    // （无协议事件也要动起来）。加载态判定与渲染分支一致：
                    // turns 空 + seed 非空 + 尚未 restore。
                    let loading_state = {
                        let t = transcript.borrow();
                        !seed.is_empty()
                            && t.turns().is_empty()
                            && *last_restored_seed.borrow() != seed
                    };
                    if (recovering.borrow().is_some() || loading_state)
                        && now.duration_since(*last_shimmer_step.borrow())
                            >= SHIMMER_STEP_INTERVAL
                    {
                        *last_shimmer_step.borrow_mut() = now;
                        let next_shimmer_tick = shimmer_tick.borrow().wrapping_add(1);
                        *shimmer_tick.borrow_mut() = next_shimmer_tick;
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
    let mut builder = list_view_rc(turns, move |turn: &Rc<TurnView>, _i: usize| {
        // turn 级 memo：mutation_rev 未变 → reconciler 完全跳过 turn_view
        // （不深克隆 RichTextParagraph / 不重跑代码高亮 / 不 diff 行内部），
        // 只有本 turn 内容变化才重建。每次 XAML 提交从「重建整个可见
        // 窗口」降为「重建变化的 1 行」——这是输入卡顿/CPU 满载的根治。
        memo(
            turn_memo,
            TurnProps {
                turn: turn.clone(),
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
    } else if force_tail_version
        .borrow()
        .is_some_and(|v| v <= *scroll_version.borrow())
        && *follow_state.borrow() == FollowState::Following
    {
        // 版本握手用 `<=`（原 `==`）：restore 设 force_tail_version 后，
        // 流式内容增长（extent_changed）/ prepend / output resolve 都会
        // 递增 scroll_version，相等匹配被系统操作打断 → resume 永不贴底
        // （用户反馈"恢复不追踪最新"）。`<=` + Following 态保护：用户上滚
        // （Idle）不触发，恢复跟随或点击"回到最新"后仍消费一次。
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
        .transition(motion::session_enter(), motion::session_exit())
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
    color_scheme: ColorScheme,
}

impl PartialEq for TurnProps {
    fn eq(&self, other: &Self) -> bool {
        self.color_scheme == other.color_scheme
            && (Rc::ptr_eq(&self.turn, &other.turn)
                || (self.turn.turn_id == other.turn.turn_id
                    && self.turn.mutation_rev == other.turn.mutation_rev))
    }
}

/// 外层 memo 先截断未变化 turn；只有 props 变化时才重建 ErrorBoundary。
/// 实际 turn 声明放在边界的子组件中，确保声明阶段 panic 也能被捕获。
fn turn_memo(props: &TurnProps, _cx: &mut RenderCx) -> Element {
    let fallback_turn_id = props.turn.turn_id.clone();
    error_boundary(component(turn_body, props.clone()), move |_message| {
        turn_render_fallback(&fallback_turn_id)
    })
    .with_key(format!("{}-boundary", props.turn.turn_id))
}

fn turn_body(props: &TurnProps, cx: &mut RenderCx) -> Element {
    // 右键撤回菜单（局部状态：memo 命中时保留，随历史释放）。
    let (menu_open, set_menu_open) = cx.use_state::<bool>(false);
    let (confirm_open, set_confirm_open) = cx.use_state::<bool>(false);
    let turn_id = props.turn.turn_id.clone();
    let on_undo = {
        let set_menu_open = set_menu_open.clone();
        let set_confirm_open = set_confirm_open.clone();
        move || {
            set_menu_open.call(false);
            set_confirm_open.call(true);
        }
    };
    // 确认对话框（modal，不在 visual tree）：确认后按 turn_id 撤回
    // （删除该 turn 及之后全部内容）；关闭总是复位 is_open。
    let confirm_dialog: Element = ContentDialog::new("撤回此消息？")
        .content("将删除该消息及其后的所有内容，此操作不可恢复。")
        .primary_button_text("撤回")
        .close_button_text("取消")
        .is_open(confirm_open)
        .on_closed({
            let turn_id = turn_id.clone();
            let set_confirm_open = set_confirm_open.clone();
            move |result| {
                set_confirm_open.call(false);
                if result == ContentDialogResult::Primary {
                    Bridge::shared().spawn_undo_turn(turn_id.clone());
                }
            }
        })
        .into();
    grid((
        turn_view(
            props.turn.as_ref(),
            props.color_scheme.clone(),
            menu_open,
            on_undo,
            set_menu_open.clone(),
        ),
        confirm_dialog,
    ))
    .rows([GridLength::STAR])
    .columns([GridLength::STAR])
    .into()
}

fn turn_render_fallback(turn_id: &str) -> Element {
    vstack((
        text_block("此回合暂时无法渲染")
            .font_size(tokens::TYPE_BODY)
            .foreground(ThemeRef::SystemCritical)
            .wrap()
            .automation_name("回合渲染失败"),
        text_block("内容已被隔离；后续状态更新会自动重试。")
            .font_size(tokens::TYPE_CAPTION)
            .foreground(ThemeRef::SecondaryText)
            .wrap(),
    ))
    .spacing(tokens::SPACE_2)
    .padding(Thickness::uniform(tokens::SPACE_4))
    .max_width(tokens::CONVERSATION_MAX_WIDTH)
    .horizontal_alignment(HorizontalAlignment::Stretch)
    .with_key(format!("{turn_id}-render-fallback"))
    .into()
}

#[derive(Clone)]
struct RoundProps {
    turn_id: Rc<str>,
    round: Rc<RoundView>,
    turn_status: TurnStatus,
    is_latest: bool,
    color_scheme: ColorScheme,
}

impl PartialEq for RoundProps {
    fn eq(&self, other: &Self) -> bool {
        self.turn_id == other.turn_id
            && self.turn_status == other.turn_status
            && self.is_latest == other.is_latest
            && self.color_scheme == other.color_scheme
            && (Rc::ptr_eq(&self.round, &other.round)
                || (self.round.round_num == other.round.round_num
                    && self.round.mutation_rev == other.round.mutation_rev))
    }
}

fn round_memo(props: &RoundProps, cx: &mut RenderCx) -> Element {
    let auto_expanded =
        activity_presentation(props.round.as_ref(), props.turn_status, props.is_latest)
            .is_some_and(|presentation| presentation.state == ActivityState::Running);
    let (activity_expanded, set_activity_expanded) = cx.use_state(auto_expanded);
    cx.use_effect(auto_expanded, {
        let set_activity_expanded = set_activity_expanded.clone();
        move || set_activity_expanded.call(auto_expanded)
    });
    // 表格行悬停状态：按 round 隔离，组件销毁即随历史释放。
    let table_hover = TableHover::new(cx);
    round_view(
        props.turn_id.as_ref(),
        props.round.as_ref(),
        props.turn_status,
        props.is_latest,
        activity_expanded,
        set_activity_expanded,
        &table_hover,
        props.color_scheme.clone(),
    )
}

/// 撤回菜单浮层（右键弹出；图标 + 文字，仿 MenuFlyout 视觉）。
fn undo_menu_float(on_undo: impl Fn() + 'static) -> Element {
    border(
        button("撤回此消息（及之后内容）")
            .icon(Icon::symbol(Symbol::Undo))
            .subtle()
            .on_click(on_undo),
    )
    .background(ThemeRef::LayerFill)
    .corner_radius(8.0)
    .padding(Thickness::uniform(tokens::SPACE_1))
    .horizontal_alignment(HorizontalAlignment::Right)
    .with_key("turn-undo-menu")
    .into()
}

fn turn_view(
    turn: &TurnView,
    color_scheme: ColorScheme,
    menu_open: bool,
    on_undo: impl Fn() + 'static,
    set_menu_open: SetState<bool>,
) -> Element {
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
    // 右键菜单浮层：插在消息上方（右对齐），点消息任意处关闭。
    if menu_open {
        items.insert(0, undo_menu_float(on_undo));
    }
    if turn.status == TurnStatus::Failed {
        if let Some(error) = &turn.failed_error {
            items.push(
                text_block(format!("回合失败：{error}"))
                    .font_size(tokens::TYPE_CAPTION)
                    .foreground(ThemeRef::SystemCritical)
                    .wrap()
                    .selectable()
                    .automation_name("回合失败详情")
                    .with_key(format!("{}-failed-error", turn.turn_id))
                    .into(),
            );
        }
    }
    let turn_id: Rc<str> = Rc::from(turn.turn_id.as_str());
    let round_count = turn.rounds.len();
    for (round_index, round) in turn.rounds.iter().enumerate() {
        items.push(
            memo(
                round_memo,
                RoundProps {
                    turn_id: turn_id.clone(),
                    round: round.clone(),
                    turn_status: turn.status,
                    is_latest: round_index + 1 == round_count,
                    color_scheme: color_scheme.clone(),
                },
            )
            .with_key(format!("{}-round-{}", turn.turn_id, round.round_num)),
        );
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
        .on_right_tapped({
            let set_menu_open = set_menu_open.clone();
            move || set_menu_open.call(true)
        })
        .on_tapped({
            let set_menu_open = set_menu_open.clone();
            move || set_menu_open.call(false)
        })
        .with_key(turn.turn_id.clone())
        .into()
}

fn round_view(
    turn_id: &str,
    round: &RoundView,
    turn_status: TurnStatus,
    is_latest: bool,
    activity_expanded: bool,
    set_activity_expanded: SetState<bool>,
    table_hover: &TableHover,
    color_scheme: ColorScheme,
) -> Element {
    let mut items: Vec<Element> = Vec::new();
    if let Some(activity) = activity_group(
        turn_id,
        round,
        turn_status,
        is_latest,
        activity_expanded,
        set_activity_expanded,
        color_scheme.clone(),
    ) {
        items.push(activity);
    }
    if answer_has_visible_content(&round.answer) {
        items.push(deepx_fluent::assistant_message(answer_view(
            turn_id,
            round.round_num,
            &round.answer,
            table_hover,
            color_scheme,
        )));
    }
    if round.output_loading {
        items.push(
            text_block("正在读取完整回答…")
                .font_size(tokens::TYPE_CAPTION)
                .foreground(ThemeRef::SecondaryText)
                .automation_name("正在读取完整回答")
                .with_key(format!("{turn_id}-r{}-output-loading", round.round_num))
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
                .with_key(format!("{turn_id}-r{}-output-error", round.round_num))
                .into(),
        );
    }
    vstack(items)
        .spacing(tokens::SPACE_2)
        .max_width(tokens::READING_MAX_WIDTH)
        .horizontal_alignment(HorizontalAlignment::Stretch)
        .with_key(format!("{turn_id}-r{}-body", round.round_num))
        .into()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ActivityState {
    Running,
    Completed,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ActivityPresentation {
    state: ActivityState,
    title: String,
    meta: String,
}

fn activity_presentation(
    round: &RoundView,
    turn_status: TurnStatus,
    is_latest: bool,
) -> Option<ActivityPresentation> {
    let has_thinking = round
        .thinking
        .as_deref()
        .is_some_and(|thinking| !thinking.trim().is_empty());
    if !has_thinking && round.tool_calls.is_empty() {
        return None;
    }

    let has_running_tool = round.tool_calls.iter().any(|card| !card.done);
    let state = if has_running_tool || (is_latest && turn_status == TurnStatus::Running) {
        ActivityState::Running
    } else if is_latest && turn_status == TurnStatus::Failed {
        ActivityState::Failed
    } else {
        ActivityState::Completed
    };
    let current_action = round
        .tool_calls
        .iter()
        .rev()
        .find(|card| !card.done)
        .or_else(|| round.tool_calls.last())
        .map(tool_action)
        .unwrap_or_else(|| "整理过程摘要".to_string());
    let title = match state {
        ActivityState::Running => format!("正在{current_action}"),
        ActivityState::Completed if round.tool_calls.len() > 1 => {
            format!("已完成 · {current_action}等操作")
        }
        ActivityState::Completed => format!("已完成 · {current_action}"),
        ActivityState::Failed => format!("处理未完成 · {current_action}"),
    };
    let meta = match (round.tool_calls.len(), has_thinking) {
        (0, _) => "过程摘要".to_string(),
        (count, true) => format!("{count} 次工具调用 · 含摘要"),
        (count, false) => format!("{count} 次工具调用"),
    };
    Some(ActivityPresentation { state, title, meta })
}

fn activity_group(
    turn_id: &str,
    round: &RoundView,
    turn_status: TurnStatus,
    is_latest: bool,
    expanded: bool,
    set_expanded: SetState<bool>,
    color_scheme: ColorScheme,
) -> Option<Element> {
    let presentation = activity_presentation(round, turn_status, is_latest)?;
    let (glyph, status_color) = match presentation.state {
        ActivityState::Running => ("●", ThemeRef::SystemCaution),
        ActivityState::Completed => ("✓", ThemeRef::SystemSuccess),
        ActivityState::Failed => ("×", ThemeRef::SystemCritical),
    };
    let header = grid((
        text_block(glyph)
            .font_size(tokens::TYPE_CAPTION)
            .foreground(status_color)
            .vertical_alignment(VerticalAlignment::Center)
            .grid_column(0),
        text_block(&presentation.title)
            .font_size(tokens::TYPE_CAPTION)
            .semibold()
            .text_trimming(TextTrimming::CharacterEllipsis)
            .vertical_alignment(VerticalAlignment::Center)
            .grid_column(1),
        text_block(&presentation.meta)
            .font_size(tokens::TYPE_CAPTION)
            .foreground(ThemeRef::SecondaryText)
            .vertical_alignment(VerticalAlignment::Center)
            .grid_column(2),
    ))
    .columns([GridLength::Pixel(16.0), GridLength::STAR, GridLength::Auto])
    .column_spacing(tokens::SPACE_2)
    .horizontal_alignment(HorizontalAlignment::Stretch);

    let mut details: Vec<Element> = Vec::new();
    if let Some(thinking) = round
        .thinking
        .as_deref()
        .filter(|thinking| !thinking.trim().is_empty())
    {
        // 过程文本留在活动组内，不再单独占据一个常态展开的卡片。
        let mut run = RichTextRun::plain(thinking);
        run.is_italic = true;
        let mut summary = RichTextBlock::new();
        summary.paragraphs = vec![RichTextParagraph::new(vec![RichTextInline::Run(run)])];
        summary.font_size = Some(tokens::TYPE_BODY);
        summary.line_height = Some(tokens::TYPE_BODY_LINE_HEIGHT);
        summary.text_wrapping = TextWrapping::Wrap;
        summary.is_text_selection_enabled = true;
        // RichTextBlock 不参与 XAML 字体属性继承，继续显式使用应用 UI 字体。
        summary.modifiers.font_family = Some(tokens::DEFAULT_UI_FONT_FAMILY.to_string());
        details.push(
            vstack((
                text_block("过程摘要")
                    .font_size(tokens::TYPE_CAPTION)
                    .semibold()
                    .foreground(ThemeRef::SecondaryText),
                Element::from(summary).with_key(format!("{turn_id}-r{}-thinking", round.round_num)),
            ))
            .spacing(tokens::SPACE_1)
            .horizontal_alignment(HorizontalAlignment::Stretch)
            .with_key(format!("{turn_id}-r{}-thinking-group", round.round_num))
            .into(),
        );
    }
    if !round.tool_calls.is_empty() {
        details.push(
            text_block("工具详情")
                .font_size(tokens::TYPE_CAPTION)
                .semibold()
                .foreground(ThemeRef::SecondaryText)
                .with_key(format!("{turn_id}-r{}-tools-heading", round.round_num))
                .into(),
        );
        for card in &round.tool_calls {
            details.push(tool_card(
                turn_id,
                round.round_num,
                card,
                color_scheme.clone(),
            ));
        }
    }

    let automation_name = format!("{}，{}", presentation.title, presentation.meta);
    Some(
        Expander::new(
            vstack(details)
                .spacing(tokens::SPACE_2)
                .padding(Thickness {
                    left: tokens::SPACE_2,
                    top: tokens::SPACE_1,
                    right: 0.0,
                    bottom: tokens::SPACE_2,
                })
                .horizontal_alignment(HorizontalAlignment::Stretch),
        )
        .header_content(header)
        .expanded(expanded)
        .on_expanding(set_expanded)
        .min_height(40.0)
        .padding(Thickness::xy(tokens::SPACE_2, tokens::SPACE_1))
        .horizontal_alignment(HorizontalAlignment::Stretch)
        .tooltip("展开或折叠本轮活动详情")
        .automation_name(automation_name)
        .automation_id(format!("chat-activity-{turn_id}-{}", round.round_num))
        .with_key(format!("{turn_id}-r{}-activity", round.round_num))
        .into(),
    )
}

fn answer_has_visible_content(answer: &AnswerView) -> bool {
    match answer {
        AnswerView::Streaming { raw, .. } => !raw.trim().is_empty(),
        AnswerView::Final { blocks, rich } => {
            !blocks.is_empty()
                || !rich.paragraphs.is_empty()
                || !rich.code_blocks.is_empty()
                || !rich.diagrams.is_empty()
                || !rich.tables.is_empty()
        }
    }
}

fn answer_view(
    turn_id: &str,
    round_num: u32,
    answer: &AnswerView,
    table_hover: &TableHover,
    color_scheme: ColorScheme,
) -> Element {
    match answer {
        AnswerView::Streaming { segments, .. } => {
            live_view(turn_id, round_num, segments, table_hover)
        }
        AnswerView::Final { rich, .. } => {
            final_view(turn_id, round_num, rich, table_hover, color_scheme)
        }
    }
}

/// 流式答案视图：字面/表格交错序列按序渲染
/// （协议表格渐进长出；残行逐字生长在网格末行）。
fn live_view(
    turn_id: &str,
    round_num: u32,
    segments: &[LiveSegment],
    table_hover: &TableHover,
) -> Element {
    let mut items: Vec<Element> = Vec::new();
    for (si, seg) in segments.iter().enumerate() {
        match seg {
            LiveSegment::Text(t) if !t.is_empty() => items.push(
                text_block(t)
                    .font_size(tokens::TYPE_BODY)
                    .line_height(tokens::TYPE_BODY_LINE_HEIGHT)
                    .wrap()
                    .selectable()
                    .with_key(format!("{turn_id}-r{round_num}-live-t{si}"))
                    .into(),
            ),
            LiveSegment::Table(td) => items.push(markdown_winui::table_view(
                td,
                &format!("{turn_id}-r{round_num}-live-table-{si}"),
                Some(table_hover),
            )),
            LiveSegment::Text(_) => {}
        }
    }
    if items.is_empty() {
        // 空内容：占位保持 key 稳定
        items.push(
            text_block("")
                .with_key(format!("{turn_id}-r{round_num}-live-empty"))
                .into(),
        );
    }
    vstack(items)
        .spacing(tokens::SPACE_2)
        .with_key(format!("{turn_id}-r{round_num}-live"))
        .into()
}

/// 权威终态视图：按文档块顺序渲染（正文/表格/代码块交错），
/// 连续段落合并进同一 RichTextBlock，遇表格/代码块/图表断开。
fn final_view(
    turn_id: &str,
    round_num: u32,
    rich: &RichTextOutput,
    table_hover: &TableHover,
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
                        &format!("{turn_id}-r{round_num}-table-{n}", n = items.len()),
                        Some(table_hover),
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
                        format!("{turn_id}-r{round_num}-code-{n}", n = items.len()),
                    ));
                }
                markdown_winui::FinalBlock::Diagram(diagram) => {
                    flush(&mut items, &mut pending);
                    let key = format!("{turn_id}-r{round_num}-diagram-{n}", n = items.len());
                    items.push(markdown_winui::diagram_view(
                        diagram,
                        color_scheme,
                        &key,
                        Some(zoom_request_callback(diagram, color_scheme, &key)),
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
                &format!("{turn_id}-r{round_num}-table-{ti}"),
                Some(table_hover),
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
                format!("{turn_id}-r{round_num}-code-{ci}"),
            ));
        }
        for (di, diagram) in rich.diagrams.iter().enumerate() {
            let key = format!("{turn_id}-r{round_num}-diagram-{di}");
            items.push(markdown_winui::diagram_view(
                diagram,
                color_scheme,
                &key,
                Some(zoom_request_callback(diagram, color_scheme, &key)),
            ));
        }
    }
    vstack(items)
        .spacing(tokens::SPACE_3)
        .transition(motion::content_enter(), None)
        .with_key(format!("{turn_id}-r{round_num}-final"))
        .into()
}

/// 工具卡（流式累积，id 稳定）。折叠器承载：
/// header = 状态 + 语义动作 + 目标摘要；展开 = 参数 raw（DeepX 工具）或
/// 执行状态（provider 内建工具，如 web_search 搜索中…）。
fn tool_action(card: &markdown_winui::ToolCardView) -> String {
    let Some(name) = card.name.as_deref() else {
        return "解析工具调用".to_string();
    };
    let short_name = name
        .rsplit(|ch| matches!(ch, '.' | ':' | '/'))
        .next()
        .unwrap_or(name);
    match short_name.to_ascii_lowercase().as_str() {
        "exec" | "exec_command" | "shell" | "shell_command" => "运行命令".to_string(),
        "read" | "read_file" => "读取文件".to_string(),
        "write" | "write_file" => "写入文件".to_string(),
        "apply_patch" | "edit" | "edit_file" => "修改文件".to_string(),
        "grep" | "rg" | "search_files" | "search_text" => "搜索内容".to_string(),
        "web_search" | "search_query" => "搜索网页".to_string(),
        "view_image" | "open_image" => "查看图片".to_string(),
        "list_files" | "list_directory" => "列出文件".to_string(),
        _ => format!("调用 {short_name}"),
    }
}

fn compact_preview(value: &str, max_chars: usize) -> String {
    let compact = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= max_chars {
        return compact;
    }
    let mut preview = compact
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    preview.push('…');
    preview
}

fn tool_argument_hint(card: &markdown_winui::ToolCardView) -> Option<String> {
    let raw = card.args_json.as_deref()?;
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    const HINT_KEYS: [&str; 8] = [
        "path", "file", "command", "query", "q", "pattern", "url", "name",
    ];
    if let serde_json::Value::Object(args) = &value {
        for key in HINT_KEYS {
            if let Some(value) = args.get(key).and_then(serde_json::Value::as_str) {
                let preview = compact_preview(value, 52);
                if !preview.is_empty() {
                    return Some(preview);
                }
            }
        }
    }
    value
        .as_str()
        .map(|value| compact_preview(value, 52))
        .filter(|value| !value.is_empty())
}

fn tool_card(
    turn_id: &str,
    round_num: u32,
    card: &markdown_winui::ToolCardView,
    color_scheme: ColorScheme,
) -> Element {
    let status = if card.done {
        "已完成"
    } else {
        "正在运行"
    };
    let name = card.name.as_deref().unwrap_or("解析中");
    let action = tool_action(card);
    let action_label = tool_argument_hint(card)
        .map(|hint| format!("{action} · {hint}"))
        .unwrap_or(action);
    let content_key = format!("{turn_id}-r{round_num}-card-{}-body", card.id);
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
        .map(|changes| format!("{status} · {action_label} · {}", changes.label()))
        .unwrap_or_else(|| format!("{status} · {action_label}"));
    Expander::new(body)
        .header(header)
        .expanded(false)
        .min_height(36.0)
        .padding(Thickness::xy(tokens::SPACE_3, tokens::SPACE_2))
        .horizontal_alignment(HorizontalAlignment::Stretch)
        .tooltip(format!("展开或折叠工具详情：{name}"))
        .automation_name(format!("{action_label}，{status}"))
        .automation_id(format!("chat-tool-{}", card.id))
        .transition(motion::reveal(), motion::content_exit())
        .with_key(format!("{turn_id}-r{round_num}-card-{}", card.id))
        .into()
}

#[cfg(test)]
mod activity_group_tests {
    use super::*;

    fn card(name: &str, args_json: Option<&str>, done: bool) -> markdown_winui::ToolCardView {
        markdown_winui::ToolCardView {
            id: format!("{name}-1"),
            name: Some(name.to_string()),
            args_display: String::new(),
            args_json: args_json.map(str::to_string),
            body: markdown_winui::ToolBody::Empty,
            changes: None,
            done,
            provider: false,
        }
    }

    #[test]
    fn completed_round_collapses_to_one_activity_summary() {
        let mut round = RoundView::default();
        round.thinking = Some("核对配置".into());
        round.tool_calls = vec![card("exec", None, true), card("read_file", None, true)];

        let presentation = activity_presentation(&round, TurnStatus::Completed, true)
            .expect("round should expose activity");
        assert_eq!(presentation.state, ActivityState::Completed);
        assert_eq!(presentation.meta, "2 次工具调用 · 含摘要");
        assert!(presentation.title.starts_with("已完成"));
    }

    #[test]
    fn active_latest_round_opens_even_before_a_tool_is_created() {
        let mut round = RoundView::default();
        round.thinking = Some("正在规划".into());

        let presentation = activity_presentation(&round, TurnStatus::Running, true)
            .expect("thinking should expose activity");
        assert_eq!(presentation.state, ActivityState::Running);
        assert_eq!(presentation.title, "正在整理过程摘要");
        assert_eq!(presentation.meta, "过程摘要");
    }

    #[test]
    fn tool_header_uses_semantic_action_and_argument_hint() {
        let tool = card(
            "functions.exec",
            Some(r#"{"command":"cargo test -p deepx-winui"}"#),
            true,
        );
        assert_eq!(tool_action(&tool), "运行命令");
        assert_eq!(
            tool_argument_hint(&tool).as_deref(),
            Some("cargo test -p deepx-winui")
        );
    }

    #[test]
    fn empty_answer_does_not_create_an_assistant_shell() {
        let empty = AnswerView::default();
        assert!(!answer_has_visible_content(&empty));

        let visible = AnswerView::Streaming {
            raw: "答案".into(),
            inlines: Vec::new(),
            segments: vec![LiveSegment::Text("答案".into())],
        };
        assert!(answer_has_visible_content(&visible));
    }
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

/// 图表放大覆盖层（挂 main.rs 覆盖层 grid，P-6 同 cell 模式）。
///
/// 轮询 [`DIAGRAM_ZOOM`] 静态槽（写端在 `final_view` 的图表点击回调），
/// 80ms 周期；请求出现即弹开全窗实底大图，点遮罩/关闭按钮清除。
/// 与 interaction_overlay 同款「写端组件 / 读端覆盖层」分离模式。
pub fn diagram_zoom_overlay(cx: &mut RenderCx) -> Element {
    // ⚠ hooks 必须在所有条件分支之前（React 规则：zoom=None 提前 return
    // 时顺序不能变，否则状态错位导致按钮/状态更新不生效）。
    let (zoom, set_zoom) = cx.use_state::<Option<DiagramZoomRequest>>(None);
    let timer = cx.use_ref::<Option<DispatcherTimer>>(None);
    // 视口跟随主窗口尺寸：宽 78%、高 72%（clamp 到合理范围，窗口极小时不崩）。
    let win = cx.use_inner_size();
    let view_w = (win.width * DIAGRAM_ZOOM_W_RATIO).clamp(420.0, 1100.0);
    let view_h = (win.height * DIAGRAM_ZOOM_H_RATIO).clamp(320.0, 760.0);

    cx.use_effect((), {
        let set_zoom = set_zoom.clone();
        let timer = timer.clone();
        move || {
            if timer.borrow().is_some() {
                return;
            }
            if let Ok(t) = DispatcherTimer::new(Duration::from_millis(80), move || {
                if let Ok(slot) = DIAGRAM_ZOOM.lock()
                    && let Some(req) = slot.as_ref()
                {
                    let req = req.clone();
                    set_zoom.call(Some(req));
                }
            }) {
                *timer.borrow_mut() = Some(t);
            }
        }
    });

    let Some(req) = zoom else {
        // 无请求：空 grid（无背景 → 不参与命中测试，点击穿透，同 splash）。
        return grid(()).into();
    };

    // 关闭：清本地 state + 清静态槽（下次点击同一图表可重新弹开）。
    let close = {
        let set_zoom = set_zoom.clone();
        move || {
            set_zoom.call(None);
            if let Ok(mut slot) = DIAGRAM_ZOOM.lock() {
                *slot = None;
            }
        }
    };

    // fit 基准：完整图缩放进视口（小图不放大），原生 ZoomMode 在此基础上
    // 放大（Ctrl+滚轮 / 触摸捏合），放大后由 ScrollViewer 滚动查看细节。
    let fit = (view_w / req.width.max(1.0))
        .min(view_h / req.height.max(1.0))
        .min(1.0);
    let img_w = (req.width * fit).max(1.0);
    let img_h = (req.height * fit).max(1.0);

    let esc = KeyboardAccelerator::new(
        VirtualKey::Escape,
        VirtualKeyModifiers::None,
        close.clone(),
    );
    let card: Element = border(
        vstack((
            hstack((
                text_block(&req.label)
                    .semibold()
                    .vertical_alignment(VerticalAlignment::Center),
                button("✕ 关闭")
                    .on_click(close.clone())
                    .vertical_alignment(VerticalAlignment::Center),
            ))
            .spacing(12.0),
            scroll_viewer(
                border(
                    Image::new(ImageSource::svg(req.svg))
                        .stretch(Stretch::Uniform)
                        .width(img_w)
                        .height(img_h),
                )
                .background(ThemeRef::CardBackground)
                .padding(12.0),
            )
            .width(view_w)
            .height(view_h)
            .horizontal_scroll_bar_visibility(ScrollBarVisibility::Auto)
            .vertical_scroll_bar_visibility(ScrollBarVisibility::Auto)
            // 原生缩放：ScrollViewerZoomMode::Enabled（0=Disabled, 1=Enabled）
            .zoom_mode(1)
            .min_zoom_factor(1.0)
            .max_zoom_factor(4.0),
            text_block("Ctrl+滚轮 / 双指捏合缩放 · Esc 关闭")
                .font_size(11.0)
                .foreground(ThemeRef::SecondaryText)
                .horizontal_alignment(HorizontalAlignment::Center),
        ))
        .spacing(12.0)
        .padding(20.0),
    )
    .background(ThemeRef::SolidBackground)
    .border_brush(ThemeRef::CardStroke)
    .border_thickness(Thickness::uniform(1.0))
    .corner_radius(8.0)
    .keyboard_accelerator(esc)
    .horizontal_alignment(HorizontalAlignment::Center)
    .vertical_alignment(VerticalAlignment::Center)
    .into();

    grid((card,))
        .rows([GridLength::STAR])
        .columns([GridLength::STAR])
        // 半透明遮罩仅作背景降噪（不再拦截点击关闭——卡片内滚动会误触）。
        .background(Color {
            a: SCRIM_ALPHA,
            r: 0,
            g: 0,
            b: 0,
        })
        .with_key("diagram-zoom-overlay")
        .into()
}
