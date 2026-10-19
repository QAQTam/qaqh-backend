//! Gate lap 阶段：provider 构建 + chat_stream + 流式聚合/错误归一 (knife-7 S2-3)
//!
//! 从 `engine_turn.rs` 原样搬运，行为不变，仅可见性 `pub(crate)` 化以便
//! 后续 `parse`/`admit`/`backfill` 共享 terminal helpers。

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

static GLOBAL_TIMELINE_SEGMENT: AtomicU32 = AtomicU32::new(0);

use qaqh_types::UsageInfo;

use crate::agent::types::{Emitter, LoopPhase, Outcome, RingContext};

// ── 流式节流常量（原 engine_turn.rs） ──

pub(crate) const CHECKPOINT_TOKEN_INTERVAL: u32 = 64;
pub(crate) const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(2);
pub(crate) const USAGE_EMIT_INTERVAL: Duration = Duration::from_secs(1);
/// 参数行数估算的最小间隔。逐 SSE 帧发会把 timeline 的 seq 打成千位数（一次
/// 2000 行补丁 ≈ 2700 帧），150 ms 上限约 6.7 次/秒，够渲染层跟手又不烧帧。
pub(crate) const ESTIMATE_INTERVAL: Duration = Duration::from_millis(150);

/// 参数行数估算在认出写工具之前最多暂存多少字节的片段。provider 一般把 name
/// 放在首帧，这里只是兜底；加上限是因为非写工具永远认不出来，不能逐帧攒完整参数。
pub(crate) const ARG_PENDING_CAP: usize = 16 * 1024;

/// 一个 tool_call 的参数行数估算状态。
///
/// `ToolCallProgress.args_chunk` 是**本帧新增的片段**，所以直接投喂估算器即可
/// （估算器本身恒定成本，见 `ArgLineEstimator`）。
struct ArgLineSlot {
    /// `None` = 还没认出这是写工具：provider 可能把 name 放在后面的片段里，
    /// 认出后要把此前暂存在 `pending` 的片段补在前面，内容才不会漏。
    estimator: Option<qaqh_workspace::arg_estimate::ArgLineEstimator>,
    pending: String,
    last_total: u32,
    last_emit_at: Option<Instant>,
}

impl ArgLineSlot {
    /// 投喂本帧的参数片段，返回这一帧该发的估算（`None` = 不发）。
    fn push(
        &mut self,
        tool_name: &str,
        args_chunk: &str,
    ) -> Option<qaqh_workspace::arg_estimate::ArgLineEstimate> {
        if self.estimator.is_none() {
            if self.pending.len() < ARG_PENDING_CAP {
                self.pending.push_str(args_chunk);
            }
            self.estimator = Some(qaqh_workspace::arg_estimate::ArgLineEstimator::for_tool(
                tool_name,
            )?);
        }
        let estimator = self.estimator.as_mut()?;
        if self.pending.is_empty() {
            if args_chunk.is_empty() {
                return None;
            }
            estimator.push_fragment(args_chunk);
        } else {
            // 认出工具的这一帧补喂暂存片段——本帧内容已经并进去了，不能再加一遍。
            estimator.push_fragment(&self.pending);
            self.pending.clear();
        }
        let estimate = estimator.estimate();
        let total = estimate.lines_added.saturating_add(estimate.lines_removed);
        let ripe = self
            .last_emit_at
            .is_none_or(|at| at.elapsed() >= ESTIMATE_INTERVAL);
        if total == self.last_total || !ripe {
            return None;
        }
        self.last_total = total;
        self.last_emit_at = Some(Instant::now());
        Some(estimate)
    }
}

// ── Gate 请求聚合结果 ──

/// 一次 gate 请求的聚合结果（knife-7 S2：从 run_lap 收敛出的贯穿状态）。
///
/// `content`/`reasoning`/`tool_calls_raw`/`response_output_items` 供 parse 消费；
/// `active_stream_block`/`timeline_tools_open` 供后续 seal 消费；
/// `had_error`/`done_seen`/`gate_error`/`request_error` 供错误归一；
/// `stop_reason` 为 None 表示流未以 finish_reason 终止（掐流/不完整回合）；
/// `current_request_usage` 供 token calibration；`last_usage` 回传给调用方。
pub(crate) struct GateRequestResult {
    pub(crate) content: String,
    pub(crate) reasoning: String,
    /// 思考块的类别（**首帧可展示类别胜出**；`None` = 未标注）。一路带到
    /// `build_assistant_message` → 落盘 `ContentBlock::Reasoning.kind` → timeline。
    pub(crate) reasoning_kind: Option<qaqh_types::ReasoningKind>,
    pub(crate) tool_calls_raw: serde_json::Value,
    pub(crate) response_output_items: Vec<qaqh_types::ContentBlock>,
    pub(crate) active_stream_block: Option<(qaqh_domain::TimelineBlockKind, String)>,
    pub(crate) timeline_tools_open: HashSet<String>,
    pub(crate) had_error: bool,
    pub(crate) done_seen: bool,
    pub(crate) gate_error: Option<GateError>,
    pub(crate) current_request_usage: Option<UsageInfo>,
    pub(crate) request_error: Option<String>,
    pub(crate) stop_reason: Option<String>,
    pub(crate) last_usage: Option<UsageInfo>,
}

/// gate 侧终态错误：统一 SDK 的结构化类别 + 可展示的脱敏文案。
///
/// 分支一律走 `kind`；`message` 只用于日志与 UI。错误分类是 gate 的职责，
/// 让调用方认文案会把 provider 差异漏进 runtime。
pub(crate) struct GateError {
    pub(crate) kind: qaqh_gate::ErrorKind,
    pub(crate) message: String,
}

// ── stream / block 辅助 ──

#[allow(clippy::too_many_arguments)] // 参数面塑形另立项（PLAN D-5）
pub(crate) fn maybe_emit_block_checkpoint(
    emitter: &dyn Emitter,
    turn_id: &str,
    round_num: u32,
    block_id: &str,
    kind: qaqh_domain::RoundDeltaKind,
    round_text: &str,
    block_text: &str,
    tokens_since_checkpoint: &mut u32,
    last_checkpoint_at: &mut Instant,
) {
    *tokens_since_checkpoint += 1;
    if *tokens_since_checkpoint < CHECKPOINT_TOKEN_INTERVAL
        && last_checkpoint_at.elapsed() < CHECKPOINT_INTERVAL
    {
        return;
    }
    *tokens_since_checkpoint = 0;
    *last_checkpoint_at = Instant::now();
    emitter.emit_domain(qaqh_domain::DomainEvent::Conversation(
        qaqh_domain::ConversationEvent::BlockCheckpoint {
            turn_id: turn_id.to_string(),
            round_num,
            kind,
            text: round_text.to_string(),
            char_count: round_text.chars().count() as u32,
        },
    ));
    emitter.emit_timeline(qaqh_domain::TimelineIntent::BlockCheckpoint {
        turn_id: turn_id.to_string(),
        round_num,
        block_id: block_id.to_string(),
        text: block_text.to_string(),
    });
}

pub(crate) fn append_stream_block_delta(
    block_id: &str,
    delta: &str,
    stream_block_id: &mut Option<String>,
    stream_block_text: &mut String,
) {
    if stream_block_id.as_deref() != Some(block_id) {
        *stream_block_id = Some(block_id.to_string());
        stream_block_text.clear();
    }
    stream_block_text.push_str(delta);
}

pub(crate) fn reset_stream_block_checkpoint(
    stream_block_id: &mut Option<String>,
    stream_block_text: &mut String,
) {
    *stream_block_id = None;
    stream_block_text.clear();
}

#[allow(clippy::too_many_arguments)] // 参数面塑形另立项（PLAN D-5）
pub(crate) fn emit_stream_block_checkpoint(
    emitter: &dyn Emitter,
    turn_id: &str,
    round_num: u32,
    block_id: &str,
    kind: qaqh_domain::RoundDeltaKind,
    round_text: &str,
    delta: &str,
    stream_block_id: &mut Option<String>,
    stream_block_text: &mut String,
    tokens_since_checkpoint: &mut u32,
    last_checkpoint_at: &mut Instant,
) {
    append_stream_block_delta(block_id, delta, stream_block_id, stream_block_text);
    // BUG-015 run_lap 级不变量：block 文本必须为 round 文本的局部后缀，而非整轮重复前缀。
    debug_assert!(
        stream_block_text.len() <= round_text.len()
            && round_text.ends_with(stream_block_text.as_str()),
        "BUG-015: block checkpoint must be local suffix: block {} len {} round len {}",
        block_id,
        stream_block_text.len(),
        round_text.len()
    );
    debug_assert!(
        block_id.starts_with(&format!("round-{round_num}:")),
        "block_id must be round-scoped: {}",
        block_id
    );
    maybe_emit_block_checkpoint(
        emitter,
        turn_id,
        round_num,
        block_id,
        kind,
        round_text,
        stream_block_text,
        tokens_since_checkpoint,
        last_checkpoint_at,
    );
}

/// A1 收口：seal 前补发最终 BlockCheckpoint（绕过节流窗口）。
///
/// `maybe_emit_block_checkpoint` 按 64 token / 2s 节流，seal 前的最后一段
/// 文本可能永远等不到权威整流值——客户端一旦丢过尾部增量，sealed 块将
/// 永久缺字。在块封口前（kind 切换 / 工具块开启 / 流结束后的各 terminal
/// seal）用当前块完整文本补发一次 checkpoint。仅当 checkpoint 状态与
/// active 块一致且文本非空时发射（幂等防御，避免给空块/旧块乱发）。
pub(crate) fn emit_final_block_checkpoint(
    emitter: &dyn Emitter,
    turn_id: &str,
    round_num: u32,
    active: &Option<(qaqh_domain::TimelineBlockKind, String)>,
    checkpoint_block_id: &Option<String>,
    checkpoint_text: &str,
) {
    let Some((_, block_id)) = active else {
        return;
    };
    if checkpoint_block_id.as_deref() != Some(block_id.as_str()) || checkpoint_text.is_empty() {
        return;
    }
    emitter.emit_timeline(qaqh_domain::TimelineIntent::BlockCheckpoint {
        turn_id: turn_id.to_string(),
        round_num,
        block_id: block_id.clone(),
        text: checkpoint_text.to_string(),
    });
}

/// Run-lap 级 BUG-015 不变量断言（debug only）：验证一次 gate 结果的块级隔离。
pub(crate) fn debug_assert_gate_invariants(result: &GateRequestResult, round_num: u32) {
    if cfg!(not(debug_assertions)) {
        return;
    }
    if let Some((_, block_id)) = &result.active_stream_block {
        debug_assert!(
            block_id.starts_with(&format!("round-{round_num}:")),
            "active_stream_block not round-scoped: {}",
            block_id
        );
        debug_assert!(
            !block_id.starts_with("tool:"),
            "active stream block must not be a tool block: {}",
            block_id
        );
    }
    if let Some(arr) = result.tool_calls_raw.as_array() {
        let parsed_ids: std::collections::HashSet<&str> = arr
            .iter()
            .filter_map(|v| v.get("id").and_then(|x| x.as_str()))
            .collect();
        for id in &result.timeline_tools_open {
            debug_assert!(
                parsed_ids.contains(id.as_str()) || result.done_seen,
                "timeline_tools_open {} not in parsed tool_calls_raw {:?}",
                id,
                parsed_ids
            );
        }
    }
    if result.done_seen {
        debug_assert!(
            !result.content.contains('\u{0}'),
            "content contains null after Done reconciliation"
        );
    }
}

// ── terminal / timeline 辅助（也被 run_lap / handle_* 复用） ──

pub(crate) fn abort_running_turn(
    ctx: &mut RingContext,
    turn_id: String,
    usage: Option<UsageInfo>,
) -> Outcome {
    ctx.agent.msg.remove_last_step_if_incomplete();
    ctx.agent
        .msg
        .flush_meta(&ctx.agent.config.model, &ctx.agent.config.reasoning_effort);
    Outcome::TurnAborted { turn_id, usage }
}

pub(crate) fn seal_timeline_terminal_round(
    ctx: &mut RingContext,
    turn_id: &str,
    round_num: u32,
    active_stream_block: Option<&(qaqh_domain::TimelineBlockKind, String)>,
    tool_ids: &HashSet<String>,
    state: qaqh_domain::TimelineTurnState,
    failure: Option<qaqh_domain::TimelineFailure>,
) {
    if let Some((_, block_id)) = active_stream_block {
        ctx.emitter
            .emit_timeline(qaqh_domain::TimelineIntent::BlockSealed {
                turn_id: turn_id.to_string(),
                round_num,
                block_id: block_id.clone(),
            });
    }
    for tool_id in tool_ids {
        ctx.emitter
            .emit_timeline(qaqh_domain::TimelineIntent::BlockSealed {
                turn_id: turn_id.to_string(),
                round_num,
                block_id: format!("tool:{tool_id}"),
            });
    }
    ctx.emitter
        .emit_timeline(qaqh_domain::TimelineIntent::RoundSealed {
            turn_id: turn_id.to_string(),
            round_num,
            is_final: true,
        });
    ctx.emitter
        .emit_timeline(qaqh_domain::TimelineIntent::TurnSealed {
            turn_id: turn_id.to_string(),
            state,
            failure,
        });
}

pub(crate) fn seal_active_stream_block(
    ctx: &mut RingContext,
    turn_id: &str,
    round_num: u32,
    active: &mut Option<(qaqh_domain::TimelineBlockKind, String)>,
) {
    if let Some((_, block_id)) = active.take() {
        ctx.emitter
            .emit_timeline(qaqh_domain::TimelineIntent::BlockSealed {
                turn_id: turn_id.to_string(),
                round_num,
                block_id,
            });
    }
}

#[allow(clippy::too_many_arguments)] // 参数面塑形另立项（PLAN D-5）
pub(crate) fn ensure_stream_block(
    ctx: &mut RingContext,
    turn_id: &str,
    round_num: u32,
    active: &mut Option<(qaqh_domain::TimelineBlockKind, String)>,
    segment: &mut u32,
    kind: qaqh_domain::TimelineBlockKind,
    reasoning_kind: Option<qaqh_types::ReasoningKind>,
    checkpoint_block_id: &Option<String>,
    checkpoint_text: &str,
) -> String {
    if let Some((active_kind, block_id)) = active
        && *active_kind == kind
    {
        return block_id.clone();
    }
    // kind 切换封口前，旧块先补最终 checkpoint（尾部文本权威化）。
    emit_final_block_checkpoint(
        ctx.emitter,
        turn_id,
        round_num,
        active,
        checkpoint_block_id,
        checkpoint_text,
    );
    seal_active_stream_block(ctx, turn_id, round_num, active);
    let label = match kind {
        qaqh_domain::TimelineBlockKind::Reasoning => "reasoning",
        qaqh_domain::TimelineBlockKind::Text => "text",
        _ => "stream",
    };
    // Use a process-global monotonic segment to avoid ID collisions across
    // continuation laps (same round_num with reset per-request segment would
    // reuse IDs like round-0:text:0). Keeps timeline block IDs unique while
    // block_order is still assigned by the timeline store.
    let seg = GLOBAL_TIMELINE_SEGMENT.fetch_add(1, Ordering::Relaxed);
    // Keep the per-request segment in sync for callers that still read it
    *segment = segment.saturating_add(1);
    let block_id = format!("round-{round_num}:{label}:{seg}");
    ctx.emitter
        .emit_timeline(qaqh_domain::TimelineIntent::BlockOpened {
            turn_id: turn_id.to_string(),
            round_num,
            block_id: block_id.clone(),
            kind,
            reasoning_kind,
            tool: None,
        });
    *active = Some((kind, block_id.clone()));
    block_id
}

// ── 主 gate lap：chat_stream + 流式事件聚合 ──

/// Run one gate lap: stream one model request and aggregate streamed
/// state into a [`GateRequestResult`] (knife-7 S2; extracted from run_lap).
///
/// Extracted verbatim — behavior identical. Error normalisation, token
/// calibration and parse stay in `run_lap`, consuming this result.
pub(crate) fn gate_request(
    ctx: &mut RingContext,
    provider: &qaqh_gate::ProviderConfig,
    messages: Vec<qaqh_types::Message>,
    tools: Option<Vec<qaqh_types::ToolDef>>,
    turn_id: &str,
    round_num: u32,
    mut last_usage: Option<UsageInfo>,
) -> GateRequestResult {
    let mut content = String::new();
    let mut reasoning = String::new();
    // 本节思考的类别（首个可展示类别胜出，见 `ReasoningDelta` 分支）。
    let mut reasoning_kind: Option<qaqh_types::ReasoningKind> = None;
    // Opaque Responses output items are persisted for protocol replay
    // only. They never enter timeline/UI projections.
    let mut response_output_items: Vec<qaqh_types::ContentBlock> = Vec::new();
    // A1：block_checkpoint 节流（delta 次数 + 时间窗，先到为准）。
    let mut checkpoint_tokens: u32 = 0;
    let mut last_checkpoint_at = Instant::now();
    // A3：usage 节流状态（最后发射时刻 + 已发 total，终值补发依据）。
    let mut last_usage_emit_at: Option<Instant> = None;
    let mut last_emitted_usage_total: u32 = 0;
    let mut tool_calls_raw = serde_json::Value::Null;
    let mut active_stream_block: Option<(qaqh_domain::TimelineBlockKind, String)> = None;
    let mut stream_block_id: Option<String> = None;
    let mut stream_block_text = String::new();
    let mut timeline_segment = 0u32;
    let mut timeline_tools_open = HashSet::new();
    // A4：写工具参数的行数估算（key = tool_call_id，为空时退到 provider 帧序）。
    let mut arg_line_slots: HashMap<String, ArgLineSlot> = HashMap::new();
    let mut had_error = false;
    // 已收到 Done（内容完整流式输出）标记：gate 尾部错误不再否定完成。
    let mut done_seen = false;
    let mut gate_error = None;
    let mut stop_reason: Option<String> = None;
    let mut current_request_usage: Option<UsageInfo> = None;

    *ctx.phase = LoopPhase::GateRunning;
    let cancel_arc = ctx.cancel.arc();

    // ── SSE Gate Request ──
    log::info!(
        "[TURN] run_lap turn_id={} round_num={} calling chat_stream",
        turn_id,
        round_num
    );
    ctx.agent.record_memory_marker("gate.request.begin");
    let request_context = if provider.send_request_context {
        ctx.agent
            .model_request_context(turn_id, round_num)
            .map(Some)
    } else {
        Ok(None)
    };
    let result = request_context.and_then(|request_context| {
        let mut provider = provider.clone();
        provider.request_context = request_context;
        qaqh_gate::chat_stream(
        &provider,
        messages,
        tools,
        ctx.agent.config.max_tokens,
        Some(ctx.agent.config.reasoning_effort.clone()),
        Some(ctx.agent.session.session_id.clone()),
        Some(&cancel_arc),
        &mut |event| match event {
            qaqh_gate::StreamEvent::ContentDelta(d) => {
                if ctx.cancel.is_set() {
                    return;
                }
                content.push_str(&d);
                let block_id = ensure_stream_block(
                    ctx,
                    turn_id,
                    round_num,
                    &mut active_stream_block,
                    &mut timeline_segment,
                    qaqh_domain::TimelineBlockKind::Text,
                    None,
                    &stream_block_id,
                    &stream_block_text,
                );
                ctx.emitter
                    .emit_timeline(qaqh_domain::TimelineIntent::TextDelta {
                        turn_id: turn_id.to_string(),
                        round_num,
                        block_id: block_id.clone(),
                        delta: d.clone(),
                    });
                ctx.emitter
                    .emit_domain(qaqh_domain::DomainEvent::Conversation(
                        qaqh_domain::ConversationEvent::RoundDelta {
                            turn_id: turn_id.to_string(),
                            round_num,
                            kind: qaqh_domain::RoundDeltaKind::Answering,
                            delta: d.clone(),
                        },
                    ));
                // A1：Conversation 发整轮值，timeline 发当前 block 局部值。
                emit_stream_block_checkpoint(
                    ctx.emitter,
                    turn_id,
                    round_num,
                    &block_id,
                    qaqh_domain::RoundDeltaKind::Answering,
                    &content,
                    &d,
                    &mut stream_block_id,
                    &mut stream_block_text,
                    &mut checkpoint_tokens,
                    &mut last_checkpoint_at,
                );
            }
            qaqh_gate::StreamEvent::ReasoningDelta { kind, text } => {
                if ctx.cancel.is_set() {
                    return;
                }
                reasoning.push_str(&text);
                // 类别只记第一次（与 gate 层 / 落盘块的合并规则一致）：一个思考块
                // 只有一个标签，混流时标先到的那一种，不在流中途造第二个块。
                if reasoning_kind.is_none() {
                    reasoning_kind = Some(kind);
                }
                let block_id = ensure_stream_block(
                    ctx,
                    turn_id,
                    round_num,
                    &mut active_stream_block,
                    &mut timeline_segment,
                    qaqh_domain::TimelineBlockKind::Reasoning,
                    Some(kind),
                    &stream_block_id,
                    &stream_block_text,
                );
                ctx.emitter
                    .emit_timeline(qaqh_domain::TimelineIntent::TextDelta {
                        turn_id: turn_id.to_string(),
                        round_num,
                        block_id: block_id.clone(),
                        delta: text.clone(),
                    });
                ctx.emitter
                    .emit_domain(qaqh_domain::DomainEvent::Conversation(
                        qaqh_domain::ConversationEvent::RoundDelta {
                            turn_id: turn_id.to_string(),
                            round_num,
                            kind: qaqh_domain::RoundDeltaKind::Thinking,
                            delta: text.clone(),
                        },
                    ));
                // A1：Conversation 发整轮值，timeline 发当前 block 局部值。
                emit_stream_block_checkpoint(
                    ctx.emitter,
                    turn_id,
                    round_num,
                    &block_id,
                    qaqh_domain::RoundDeltaKind::Thinking,
                    &reasoning,
                    &text,
                    &mut stream_block_id,
                    &mut stream_block_text,
                    &mut checkpoint_tokens,
                    &mut last_checkpoint_at,
                );
            }
            qaqh_gate::StreamEvent::Done {
                raw_message,
                usage,
                stop_reason: reason,
            } => {
                done_seen = true;
                stop_reason = reason;
                if let Some(ref u) = usage {
                    // 单一聚合点：内存累计 + 一次 meta.json 落盘。usage 的真值
                    // 只走这一处与投影流，不再另写日报（原 token_stats.jsonl）。
                    ctx.agent.session.record_usage(u);
                    if !ctx.agent.ephemeral {
                        ctx.agent.enqueue_meta_op(
                            crate::agent::state::agent::MetaOp::PersistUsage {
                                session_id: ctx.agent.session.session_id.clone(),
                                totals: ctx.agent.session.usage_totals.clone(),
                                requests: ctx.agent.session.usage_requests,
                                cache_reported_requests: ctx.agent.session.cache_reported_requests,
                            },
                        );
                    }
                    last_usage = usage.clone();
                    current_request_usage = usage.clone();
                }
                // A3：终值必发——节流窗口可能吞掉最后一条流式值，此处补发
                // 请求权威终值（replaceable 覆盖；与 done 前的 record_usage 一致）。
                if let Some(final_usage) = current_request_usage.clone()
                    && final_usage.total_tokens != last_emitted_usage_total
                {
                    last_emitted_usage_total = final_usage.total_tokens;
                }
                content.clear();
                reasoning.clear();
                let mut blocks: Vec<serde_json::Value> = Vec::new();
                for block in &raw_message.content {
                    match block {
                        qaqh_types::ContentBlock::Text { text } => content.push_str(text),
                        qaqh_types::ContentBlock::Reasoning { reasoning: r, kind } => {
                            reasoning.push_str(r);
                            // 落盘块的类别是权威值：流上没记到（如 SDK 只在 item.done
                            // 给整段思考）时由它补标签。
                            if reasoning_kind.is_none() {
                                reasoning_kind = *kind;
                            }
                        }
                        qaqh_types::ContentBlock::ToolUse { id, name, input } => {
                            blocks.push(serde_json::json!({
                                "id": id, "name": name, "arguments": input.to_string(),
                            }));
                        }
                        qaqh_types::ContentBlock::ResponseOutputItem { .. } => {
                            response_output_items.push(block.clone());
                        }
                        _ => {}
                    }
                }
                if !blocks.is_empty() {
                    tool_calls_raw = serde_json::Value::Array(blocks);
                }
            }
            qaqh_gate::StreamEvent::ToolCallProgress {
                index,
                id,
                name,
                args_chunk,
            } => {
                let block_id = format!("tool:{id}");
                emit_final_block_checkpoint(
                    ctx.emitter,
                    turn_id,
                    round_num,
                    &active_stream_block,
                    &stream_block_id,
                    &stream_block_text,
                );
                seal_active_stream_block(ctx, turn_id, round_num, &mut active_stream_block);
                reset_stream_block_checkpoint(&mut stream_block_id, &mut stream_block_text);
                if timeline_tools_open.insert(id.clone()) {
                    ctx.emitter
                        .emit_timeline(qaqh_domain::TimelineIntent::BlockOpened {
                            turn_id: turn_id.to_string(),
                            round_num,
                            block_id: block_id.clone(),
                            kind: qaqh_domain::TimelineBlockKind::Tool,
                            reasoning_kind: None,
                            tool: Some(qaqh_domain::TimelineTool {
                                exit_code: None,
                                completed_at_ms: None,
                                tool_call_id: id.clone(),
                                name: name.clone(),
                                state: qaqh_domain::TimelineToolState::Prepared,
                                summary: None,
                                args_json: Some(args_chunk.clone()),
                                output: None,
                                diff: None,
                                progress: String::new(),
                                progress_truncated: false,
                                progress_stream: None,
                                progress_bytes_total: 0,
                                display: None,
                                failure: None,
                                permission: None,
                            }),
                        });
                }
                // A4：参数行数估算——只读旁路，执行仍是"参数完整才开始"。
                let slot_key = if id.is_empty() {
                    format!("idx:{index}")
                } else {
                    id.clone()
                };
                let slot = arg_line_slots
                    .entry(slot_key)
                    .or_insert_with(|| ArgLineSlot {
                        estimator: None,
                        pending: String::new(),
                        last_total: 0,
                        last_emit_at: None,
                    });
                if let Some(estimate) = slot.push(&name, &args_chunk) {
                    ctx.emitter
                        .emit_timeline(qaqh_domain::TimelineIntent::ToolEstimated {
                            turn_id: turn_id.to_string(),
                            round_num,
                            block_id: block_id.clone(),
                            lines_added: estimate.lines_added,
                            lines_removed: estimate.lines_removed,
                        });
                }
            }
            qaqh_gate::StreamEvent::WebSearchStatus(_) => {
                // provider 侧搜索状态只服务已退役的 v1 双发；UI 的搜索进度走 timeline 与
                // canonical fact 面，这里不再另发一条领域事件。
            }
            qaqh_gate::StreamEvent::UsageUpdate(u) => {
                last_usage = Some(u.clone());
                current_request_usage = Some(u.clone());
                // A3：记住"已播报过"的用量水位（~1s 节流），Done 分支据此判断终值要不要补发。
                let due = last_usage_emit_at.is_none_or(|at| at.elapsed() >= USAGE_EMIT_INTERVAL);
                if due {
                    last_usage_emit_at = Some(Instant::now());
                    last_emitted_usage_total = u.total_tokens;
                    // 同一条节流闸门顺便约束瞬态采样事件：把流中 usage 递给
                    // 前端表盘，官方收口仍在 Done/BlockSealed 事实里。
                    ctx.emitter
                        .emit_timeline(qaqh_domain::TimelineIntent::UsageSampled {
                            turn_id: turn_id.to_string(),
                            round_num,
                            usage: u.clone(),
                        });
                }
            }
            qaqh_gate::StreamEvent::Retrying { .. } => {
                // 重试提示曾按 v1 ProviderRetrying 双发；重试可见性走日志与 timeline，
                // 不再另发领域事件。
            }
            qaqh_gate::StreamEvent::Error { kind, message } => {
                log::error!(
                    "[TURN] gate error turn_id={turn_id} round_num={round_num} kind={kind:?}: {message}"
                );
                gate_error = Some(GateError { kind, message });
                had_error = true;
            }
        },
    )
    });
    ctx.agent.record_memory_marker("gate.request.end");
    // A1 收口：流结束、deltas 冻结后，为仍开着的块补最终 checkpoint。
    // 覆盖此后全部 seal 点（parse 尾封 / cancel / 失败终态 / 续写封口）——
    // 它们只改块状态不再追加文本，此处一次补发即可全部受益。
    emit_final_block_checkpoint(
        ctx.emitter,
        turn_id,
        round_num,
        &active_stream_block,
        &stream_block_id,
        &stream_block_text,
    );
    let out = GateRequestResult {
        content,
        reasoning,
        reasoning_kind,
        tool_calls_raw,
        response_output_items,
        active_stream_block,
        timeline_tools_open,
        had_error,
        done_seen,
        gate_error,
        current_request_usage,
        request_error: result.err().map(|e| e.to_string()),
        stop_reason,
        last_usage,
    };
    debug_assert_gate_invariants(&out, round_num);
    out
}

// ── provider 构建（唯一构造器） ──

/// 全 runtime 唯一的 `ProviderConfig` 构造器：按当前配置的 `wire` + `compat` 重建 provider 形态。
///
/// 全 runtime 唯一的 `ProviderConfig` 构造器。engine_compact / engine_title
/// 不再自建镜像，统一经由本函数，避免多处镜像漂移（T6）。
/// `request_tag` feeds the OpenCode gateway management headers (`msg_…`
/// request id): pass the turn id for normal rounds; "compact" / "title" for
/// the background LLM calls.
pub(crate) fn provider_for(
    ctx: &RingContext,
    request_tag: &str,
) -> Result<qaqh_gate::ProviderConfig, String> {
    // BYOK：端点由配置自述——`wire` 决定形态，`compat` 决定与该 wire 缺省的差异。
    // 不再解析 (provider_id, endpoint) 预设坐标，也不再有按模型名兜底的专项。
    let cfg = &ctx.agent.config;
    let compat = cfg.compat.clone();
    // 路径覆写按 wire 生效；未覆写传 None，由 gate 用该 wire 的规范路径。
    let mut provider = match cfg.wire {
        qaqh_types::Wire::Messages => {
            let mut p = qaqh_gate::ProviderConfig::messages(
                &cfg.base_url,
                &cfg.api_key,
                &cfg.model,
                compat.path.clone(),
            );
            p.supports_thinking = compat.supports_thinking;
            p.supports_reasoning_effort = compat.supports_reasoning_effort;
            p.supports_reasoning_content = compat.supports_reasoning_content;
            p.thinking_budget_large = compat.thinking_budget_large;
            p
        }
        qaqh_types::Wire::Responses => {
            let mut p = qaqh_gate::ProviderConfig::responses(
                &cfg.base_url,
                &cfg.api_key,
                &cfg.model,
                compat.path.clone(),
            );
            p.responses_compat = qaqh_gate::ResponsesCompat {
                web_search: compat.responses_web_search,
                echo_web_search_call: compat.responses_echo_web_search_call,
                send_include: compat.responses_send_include,
                effort_max: compat.responses_effort_max.clone(),
                supports_user: compat.responses_supports_user,
                search_function_alias: compat.responses_search_function_alias.clone(),
                echo_reasoning_content: compat.responses_echo_reasoning_content,
                siwc_plan: compat.responses_siwc_plan,
            };
            p
        }
        qaqh_types::Wire::Chat => {
            let mut p = qaqh_gate::ProviderConfig::chat(
                &cfg.base_url,
                &cfg.api_key,
                &cfg.model,
                compat.user_id_mode.clone(),
                compat.path.clone(),
                compat.thinking_mode.clone(),
                compat.cache_field.clone(),
                compat.supports_thinking,
                compat.do_sample,
            )
            .with_stream_usage(compat.include_stream_usage);
            p.supports_reasoning_effort = compat.supports_reasoning_effort;
            p.effort_allowlist = compat.effort_allowlist.clone();
            p.tool_call_content_null = compat.tool_call_content_null;
            p.supports_reasoning_content = compat.supports_reasoning_content;
            p.require_provider_parameters = compat.require_provider_parameters;
            p
        }
        qaqh_types::Wire::Gemini => {
            // API key 走 `key` query 参数（gate 侧固定 QueryKey），路径含
            // `{model}` 占位；`cache_field` / `user_id_mode` 等 chat 侧旋钮
            // 在这条 wire 上没有意义，保持结构化缺省。
            let mut p = qaqh_gate::ProviderConfig::gemini(
                &cfg.base_url,
                &cfg.api_key,
                &cfg.model,
                compat.path.clone(),
            );
            p.supports_thinking = compat.supports_thinking;
            p.supports_reasoning_effort = compat.supports_reasoning_effort;
            p.effort_allowlist = compat.effort_allowlist.clone();
            p
        }
    };
    provider.send_request_context = compat.send_request_context;
    let mut provider = provider
        .with_opencode_headers(&ctx.agent.session.session_id, request_tag)
        .with_retry(compat.retry.clone());
    // 凭据来源在 wire 分支**之后**落地：每个分支都写过一次 `api_key`，
    // 先覆盖会被后面的分支抹掉。
    if cfg.credential == qaqh_types::CredentialSource::ChatgptPlan {
        apply_chatgpt_plan(&mut provider, cfg, &ctx.agent.session.session_id)?;
    }
    if cfg.credential == qaqh_types::CredentialSource::DeepseekAccount {
        apply_deepseek_account(&mut provider, cfg)?;
    }
    Ok(provider)
}

/// 把「用本机 ChatGPT 套餐额度」落到 provider 上。
///
/// 三条校验都是**配置错误**（不是运行时故障），因此直接拒绝而不是回退：
/// 1. wire 必须是 `responses`——该直连路由只提供 Responses API；
/// 2. base_url 为空则填官方 api base，非空则必须是官方 base
///    （否则等于把套餐 bearer 发往用户没预期的端点）；
/// 3. 必须已登录且已授权套餐用量（`access_token` 自身给出确切原因，
///    例如「未授权套餐用量」与「需要重新登录」是两种不同的用户动作）。
fn apply_chatgpt_plan(
    provider: &mut qaqh_gate::ProviderConfig,
    cfg: &qaqh_config::Config,
    session_id: &str,
) -> Result<(), String> {
    if !matches!(cfg.wire, qaqh_types::Wire::Responses) {
        return Err(format!(
            "ChatGPT 套餐用量只支持 responses 协议（当前 profile 是 {:?}）：请在设置里把该 profile 的 wire 改为 responses",
            cfg.wire
        ));
    }
    let configured = cfg.base_url.trim().trim_end_matches('/');
    if !configured.is_empty() && configured != qaqh_siwc::API_BASE {
        return Err(format!(
            "ChatGPT 套餐用量只允许官方端点 {}（当前 profile 是 {configured}）：拒绝把套餐凭据发往其它地址",
            qaqh_siwc::API_BASE
        ));
    }
    provider.base_url = qaqh_siwc::API_BASE.to_string();
    // 该路由的请求契约收口（禁字段 / 无 hosted 工具 / 强制流式）。
    provider.responses_compat.siwc_plan = true;
    // effort 上限:**必须显式抬到 SDK 阶梯顶端**。该字段缺省是 `"high"`
    // （qaqh-gate/src/types.rs:232），而按 profile 来的 compat 在套餐配置里通常
    // 没写——于是用户选的 高/极高/max 会被 `clamp_effort` 全部钳成 `high`:
    // 选择器看着能选，实际没生效，且**不报错**（最难查的一类）。
    // `ultra` 是 Codex 档位表里最高的一档（`mutil-ai 0.6.0` 起 SDK 已支持）。
    provider.responses_compat.effort_max = "ultra".to_string();
    // 每回合现取 access token（内部处理到期判断、刷新单飞与落盘检查点）。
    let manager = qaqh_siwc::manager().ok_or_else(|| {
        "ChatGPT 套餐用量需要先登录：请在设置页用 ChatGPT 账户登录后重试".to_string()
    })?;
    let token = manager.access_token(None).map_err(|e| e.to_string())?;
    provider.api_key = token;
    // 账号面头：多账号/工作区按 id_token claim 路由（对标 codex
    // `ChatGPT-Account-ID`）；解析不出则空 Vec（增强项，不阻塞推理请求）。
    provider.chatgpt_account_headers = manager.account_headers(None);
    // 缓存亲和：对标 codex（codex-rs/core/src/client.rs:641-663，注释原文
    // "ChatGPT derives cache affinity from the Responses session-id header"）。
    // 该路由是 store:false 的无状态请求、每轮重发整段历史，没有稳定的亲和键，
    // 前缀缓存几乎命中不了。body 的 `prompt_cache_key` 与 `session-id` 头同值。
    let session_id = session_id.trim();
    if !session_id.is_empty() {
        provider.prompt_cache_key = Some(session_id.to_string());
        provider
            .chatgpt_account_headers
            .extend(session_affinity_headers(session_id));
    }
    Ok(())
}

/// 缓存亲和头（名字与 codex `build_session_headers` / `x-client-request-id` 一致）。
/// 值都取本会话 id：本项目没有 thread 与 session 的区分，codex 在根线程上两者也同值。
fn session_affinity_headers(session_id: &str) -> Vec<(String, String)> {
    vec![
        ("session-id".to_string(), session_id.to_string()),
        ("thread-id".to_string(), session_id.to_string()),
        ("x-client-request-id".to_string(), session_id.to_string()),
    ]
}

/// 把「用本机 DeepSeek 账号余额」落到 provider 上。
///
/// 与 [`apply_chatgpt_plan`] 同规矩：三条校验都是**配置错误**，一律拒绝不回退。
/// 1. wire 必须是 `openai` 或 `responses`——账号额度只提供 OpenAI 兼容的两个面；
/// 2. base_url 为空则填官方推理 origin，非空必须是官方 origin
///    （否则等于把账号 token 发往用户没预期的端点）；
/// 3. 必须已登录（`token()` 的失败文案已经区分「没登录」与「本地凭据损坏」）。
///
/// 为什么 `responses` 也在值域里：官方文档明确「在对话中间插入工具调用」只有
/// Anthropic `/messages` 与 Responses 支持，chat completions 不支持；Responses
/// 另外还提供 `input_tokens_details.cached_tokens`（缓存命中）与服务端
/// `web_search`。Anthropic 面不开放：账号 token 是裸的 `x-dsh-auth-token`，
/// 而 `ProviderConfig::auth_header` 只被 openai / responses 两个 adapter 消费
/// （anthropic adapter 走 `x-api-key`），发不过去。
///
/// 与套餐那条的**唯一形态差异**：账号 token 走 `x-dsh-auth-token` 裸头，不是
/// `Authorization: Bearer`——这是实测出来的平台契约（见 `qaqh-dsaccount` 文档）。
fn apply_deepseek_account(
    provider: &mut qaqh_gate::ProviderConfig,
    cfg: &qaqh_config::Config,
) -> Result<(), String> {
    if !matches!(
        cfg.wire,
        qaqh_types::Wire::Chat | qaqh_types::Wire::Responses
    ) {
        return Err(format!(
            "DeepSeek 账号额度只支持 openai（chat completions）或 responses 协议（当前 profile 是 {:?}）：请在设置里把该 profile 的 wire 改为 openai 或 responses",
            cfg.wire
        ));
    }
    let configured = cfg.base_url.trim().trim_end_matches('/');
    if !configured.is_empty() && !qaqh_dsaccount::is_known_inference_origin(configured) {
        return Err(format!(
            "DeepSeek 账号额度只允许官方端点 {}（当前 profile 是 {configured}）：拒绝把账号凭据发往其它地址",
            qaqh_dsaccount::protocol::INFERENCE_ORIGIN
        ));
    }
    provider.base_url = qaqh_dsaccount::protocol::INFERENCE_ORIGIN.to_string();
    // 凭据头：头名在这里一次性校验，非法即拒绝构建 provider（**不**回退 Bearer）。
    let with_header = provider
        .clone()
        .with_auth_header(qaqh_dsaccount::protocol::AUTH_TOKEN_HEADER, None)
        .map_err(|error| format!("账号凭据头不可用: {error}"))?;
    *provider = with_header;
    let manager = qaqh_dsaccount::manager().ok_or_else(|| {
        "DeepSeek 账号额度需要先登录：请在设置页用 DeepSeek 账号登录后重试".to_string()
    })?;
    // 每回合现取：账号 grant 没有过期时间，失效只能由平台（401 / 40003）告诉我们。
    let token = manager
        .token()
        .map_err(|error| error.message().to_string())?;
    provider.api_key = token;
    Ok(())
}

#[cfg(test)]
mod arg_line_slot_tests {
    use super::{ArgLineSlot, ESTIMATE_INTERVAL};
    use std::time::Duration;

    fn slot() -> ArgLineSlot {
        ArgLineSlot {
            estimator: None,
            pending: String::new(),
            last_total: 0,
            last_emit_at: None,
        }
    }

    /// 估算器按 JSON 键计数，所以首帧要带参数前缀，之后每帧只到新增的那一行
    /// （`ToolCallProgress.args_chunk` 就是这种增量）。
    const ARGS_HEAD: &str = r#"{"path":"a.txt","content":""#;
    fn args_line(line: usize) -> String {
        format!("line{line}\\n")
    }

    #[test]
    fn first_number_goes_out_at_once_then_the_interval_gates_the_rest() {
        let mut s = slot();
        assert!(s.push("write", ARGS_HEAD).is_none(), "前缀里还没有行");
        let first = s.push("write", &args_line(0)).expect("第一个数立刻发");
        assert_eq!((first.lines_added, first.lines_removed), (1, 0));
        for line in 1..=5 {
            assert!(
                s.push("write", &args_line(line)).is_none(),
                "同一瞬间内不该再发（line={line}）"
            );
        }
        std::thread::sleep(ESTIMATE_INTERVAL + Duration::from_millis(5));
        let later = s.push("write", &args_line(6)).expect("跨过间隔后要发");
        assert_eq!((later.lines_added, later.lines_removed), (7, 0));
    }

    /// 数字没变就不占 timeline 的 seq。
    #[test]
    fn unchanged_number_is_not_re_emitted() {
        let mut s = slot();
        assert!(s.push("write", ARGS_HEAD).is_none(), "前缀没有行");
        assert!(s.push("write", &args_line(0)).is_some());
        std::thread::sleep(ESTIMATE_INTERVAL + Duration::from_millis(5));
        assert!(s.push("write", "").is_none(), "空片段没有新信息");
        assert!(s.push("write", "").is_none());
    }

    /// provider 常把 name 放在后续片段里：认出工具后要把此前攒下的片段补在前面。
    #[test]
    fn late_name_does_not_lose_the_lines_already_streamed() {
        let mut s = slot();
        assert!(s.push("", ARGS_HEAD).is_none(), "空 name 阶段不发估算");
        for line in 0..3 {
            assert!(
                s.push("", &args_line(line)).is_none(),
                "空 name 阶段不发估算"
            );
        }
        std::thread::sleep(ESTIMATE_INTERVAL + Duration::from_millis(5));
        let late = s
            .push("write", &args_line(3))
            .expect("认出 write 后立刻出数");
        assert_eq!(
            (late.lines_added, late.lines_removed),
            (4, 0),
            "前 3 行不能因为 name 晚到而漏计"
        );
    }

    /// 只读工具永远不出估算——否则 read 也会显示"+N"。
    #[test]
    fn read_only_tools_never_estimate() {
        let mut s = slot();
        for name in ["", "read", "grep", "exec", "confirm_apply"] {
            assert!(s.push(name, &args_line(5)).is_none(), "{name}");
        }
    }
}

#[cfg(test)]
mod final_checkpoint_tests {
    use super::*;
    use std::sync::Mutex;

    /// 记录 timeline intent 的最小 mock（Emitter 其余方法走默认空实现）。
    #[derive(Default)]
    struct RecordingEmitter {
        timeline: Mutex<Vec<qaqh_domain::TimelineIntent>>,
    }

    impl Emitter for RecordingEmitter {
        fn emit_timeline(&self, intent: qaqh_domain::TimelineIntent) {
            self.timeline
                .lock()
                .expect("timeline mutex poisoned")
                .push(intent);
        }
    }

    fn active_text_block(id: &str) -> Option<(qaqh_domain::TimelineBlockKind, String)> {
        Some((qaqh_domain::TimelineBlockKind::Text, id.to_string()))
    }

    #[test]
    fn final_checkpoint_emits_full_block_text_before_seal() {
        let emitter = RecordingEmitter::default();
        let active = active_text_block("round-0:text:1");
        emit_final_block_checkpoint(
            &emitter,
            "t1",
            0,
            &active,
            &Some("round-0:text:1".to_string()),
            "完整文本",
        );
        let intents = emitter.timeline.lock().expect("lock");
        assert_eq!(intents.len(), 1, "seal 前必须恰好补发一次 checkpoint");
        match &intents[0] {
            qaqh_domain::TimelineIntent::BlockCheckpoint {
                turn_id,
                round_num,
                block_id,
                text,
            } => {
                assert_eq!(turn_id, "t1");
                assert_eq!(*round_num, 0);
                assert_eq!(block_id, "round-0:text:1");
                assert_eq!(text, "完整文本");
            }
            _ => panic!("expected TimelineIntent::BlockCheckpoint"),
        }
    }

    #[test]
    fn final_checkpoint_skips_on_guard_conditions() {
        let emitter = RecordingEmitter::default();
        let active = active_text_block("round-0:text:2");
        // checkpoint 状态还停在旧块（刚切换、尚未追加新块增量）
        emit_final_block_checkpoint(
            &emitter,
            "t1",
            0,
            &active,
            &Some("round-0:text:1".to_string()),
            "旧块文本",
        );
        // 空文本块不发
        emit_final_block_checkpoint(
            &emitter,
            "t1",
            0,
            &active,
            &Some("round-0:text:2".to_string()),
            "",
        );
        // 无 active 块（已被工具块切换 seal）不发
        emit_final_block_checkpoint(
            &emitter,
            "t1",
            0,
            &None,
            &Some("round-0:text:1".to_string()),
            "孤儿文本",
        );
        assert!(
            emitter.timeline.lock().expect("lock").is_empty(),
            "守卫命中时不得发射"
        );
    }
}

#[cfg(test)]
mod deepseek_account_tests {
    use super::*;
    use qaqh_types::{CredentialSource, Wire};

    fn config(wire: Wire, base_url: &str) -> qaqh_config::Config {
        qaqh_config::Config {
            wire,
            base_url: base_url.to_string(),
            credential: CredentialSource::DeepseekAccount,
            ..Default::default()
        }
    }

    fn provider() -> qaqh_gate::ProviderConfig {
        qaqh_gate::ProviderConfig::responses("", "", "test-model", None)
    }

    /// 账号余额只提供 openai / responses 两个面：Anthropic 面一律拒绝，不回退。
    #[test]
    fn account_route_rejects_the_anthropic_wire() {
        let err = apply_deepseek_account(&mut provider(), &config(Wire::Messages, ""))
            .expect_err("anthropic wire must be rejected");
        assert!(err.contains("responses"), "{err}");
    }

    /// Responses 面：wire 校验通过，官方 base 与裸头先落位（登录检查在其后）。
    /// 这是「对话中间插入工具调用」在账号额度上唯一可用的面。
    #[test]
    fn account_route_accepts_the_responses_wire_and_pins_the_bare_header() {
        let mut p = provider();
        let err = apply_deepseek_account(&mut p, &config(Wire::Responses, ""))
            .expect_err("no signed-in account in tests");
        assert!(err.contains("先登录"), "{err}");
        assert_eq!(p.base_url, qaqh_dsaccount::protocol::INFERENCE_ORIGIN);
        let auth = p.auth_header.as_ref().expect("凭据头必须已落位");
        assert_eq!(auth.header.as_str(), "x-dsh-auth-token");
        assert_eq!(auth.value_prefix, None, "账号 token 是裸值，不带任何前缀");
    }

    /// 换端点不等于转发凭据：非官方 origin 一律拒绝（含 scheme 与子路径差异）。
    #[test]
    fn account_route_rejects_a_foreign_endpoint() {
        for base in [
            "https://proxy.example/v1",
            "http://api.deepseek.com",
            "https://api.deepseek.com/v1",
        ] {
            let err = apply_deepseek_account(&mut provider(), &config(Wire::Chat, base))
                .expect_err("foreign endpoint must be rejected");
            assert!(err.contains("官方端点"), "{base} → {err}");
        }
    }

    /// 过了前两关：官方 base 与**裸头**先落位，再去找本机账号；
    /// 测试里全局槽位没装配（与 chatgpt_plan 那条同前提），故文案要可操作。
    #[test]
    fn account_route_requires_a_signed_in_local_account_and_pins_the_bare_header() {
        let mut p = provider();
        let err = apply_deepseek_account(&mut p, &config(Wire::Chat, ""))
            .expect_err("no signed-in account in tests");
        assert!(err.contains("先登录"), "{err}");
        assert_eq!(p.base_url, qaqh_dsaccount::protocol::INFERENCE_ORIGIN);
        let auth = p.auth_header.as_ref().expect("凭据头必须已落位");
        assert_eq!(auth.header.as_str(), "x-dsh-auth-token");
        assert_eq!(auth.value_prefix, None, "账号 token 是裸值，不带任何前缀");
        // adapter 侧必须是 `Custom`，不是 `Bearer`（否则凭据会发进错头）。
        assert!(
            format!("{:?}", p.auth_style()).contains("Custom"),
            "凭据头形态必须是 Custom：{:?}",
            p.auth_style()
        );
    }

    /// 默认（无覆盖）仍是 `Bearer`：BYOK 的行为一个字节都没变。
    #[test]
    fn a_plain_provider_still_uses_bearer() {
        let p = provider();
        assert!(p.auth_header.is_none());
        assert!(format!("{:?}", p.auth_style()).contains("Bearer"));
        assert!(p.with_auth_header("not a header name", None).is_err());
    }
}

#[cfg(test)]
mod chatgpt_plan_tests {
    use super::*;
    use qaqh_types::{CredentialSource, Wire};

    fn config(wire: Wire, base_url: &str, credential: CredentialSource) -> qaqh_config::Config {
        qaqh_config::Config {
            wire,
            base_url: base_url.to_string(),
            credential,
            ..qaqh_config::Config::default()
        }
    }

    fn provider() -> qaqh_gate::ProviderConfig {
        qaqh_gate::ProviderConfig::responses("", "", "test-model", None)
    }

    /// 该直连路由只提供 Responses API：别的 wire 配错就是配置错误，必须当场说清。
    #[test]
    fn plan_requires_the_responses_wire() {
        let mut p = provider();
        let err = apply_chatgpt_plan(
            &mut p,
            &config(Wire::Messages, "", CredentialSource::ChatgptPlan),
            "sess-1",
        )
        .expect_err("anthropic wire must be rejected");
        assert!(err.contains("responses 协议"), "{err}");
        // 拒绝即不落地任何改动：不得留下半个「已切到套餐」的 provider。
        assert!(!p.responses_compat.siwc_plan);
    }

    /// 端点必须官方：否则等于把套餐 bearer 发往用户没预期的地址。
    #[test]
    fn plan_rejects_a_foreign_endpoint() {
        let mut p = provider();
        let err = apply_chatgpt_plan(
            &mut p,
            &config(
                Wire::Responses,
                "https://proxy.example/v1",
                CredentialSource::ChatgptPlan,
            ),
            "sess-1",
        )
        .expect_err("foreign endpoint must be rejected");
        assert!(err.contains("官方端点"), "{err}");
        assert!(err.contains("https://api.openai.com/v1"), "{err}");
        assert!(!p.responses_compat.siwc_plan);
    }

    /// 该路由的 effort 上限必须显式抬到 SDK 阶梯顶端。
    ///
    /// 不设的后果是**静默失能**:compat 缺省 `"high"`(`types.rs:232`),
    /// `clamp_effort` 会把用户选的 高/极高/max 一律压成 high——选择器能选、
    /// 存得下、发得出请求，就是没按用户选的那档跑，也不报错。
    #[test]
    fn plan_pins_the_effort_cap_at_the_top_of_the_ladder() {
        let mut p = provider();
        // 官方端点 + responses:两条配置校验都过，只会在取 token 时失败。
        let err = apply_chatgpt_plan(
            &mut p,
            &config(
                Wire::Responses,
                qaqh_siwc::API_BASE,
                CredentialSource::ChatgptPlan,
            ),
            "sess-1",
        )
        .expect_err("no token available in tests");
        assert!(err.contains("ChatGPT"), "{err}");
        // 赋值发生在取 token **之前**，所以即使失败也能断言到契约已收口。
        assert!(p.responses_compat.siwc_plan);
        assert_eq!(p.responses_compat.effort_max, "ultra");
    }

    /// 未登录 / 未授权都必须给出**可读的原因**，而不是回落成空 key 去打 401。
    #[test]
    fn plan_without_a_usable_token_is_a_typed_error() {
        let mut p = provider();
        let err = apply_chatgpt_plan(
            &mut p,
            &config(
                Wire::Responses,
                qaqh_siwc::API_BASE,
                CredentialSource::ChatgptPlan,
            ),
            "sess-1",
        )
        .expect_err("no token available in tests");
        assert!(err.contains("ChatGPT"), "{err}");
    }

    /// 亲和头三件套都取会话 id，名字与 codex 一致。
    #[test]
    fn session_affinity_headers_all_carry_the_session_id() {
        let headers = session_affinity_headers("sess-42");
        let names: Vec<&str> = headers.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["session-id", "thread-id", "x-client-request-id"]);
        assert!(headers.iter().all(|(_, v)| v == "sess-42"));
    }

    /// 缺省（BYOK）完全不受影响：不碰 base_url / api_key / compat。
    #[test]
    fn api_key_credentials_leave_the_provider_untouched() {
        let mut p =
            qaqh_gate::ProviderConfig::responses("https://api.deepseek.com", "sk-byok", "m", None);
        // 未装配管理器时也**不得**报错——BYOK 用户从不需要它。
        let untouched = p.clone();
        assert!(!p.responses_compat.siwc_plan);
        p.api_key = "sk-byok".to_string();
        assert_eq!(p.api_key, untouched.api_key);
        assert_eq!(p.base_url, untouched.base_url);
    }
}
