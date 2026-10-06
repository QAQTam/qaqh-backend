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
use crate::agent::util;

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
    let result = qaqh_gate::chat_stream(
        provider,
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
            qaqh_gate::StreamEvent::ReasoningDelta(r) => {
                if ctx.cancel.is_set() {
                    return;
                }
                reasoning.push_str(&r);
                let block_id = ensure_stream_block(
                    ctx,
                    turn_id,
                    round_num,
                    &mut active_stream_block,
                    &mut timeline_segment,
                    qaqh_domain::TimelineBlockKind::Reasoning,
                    &stream_block_id,
                    &stream_block_text,
                );
                ctx.emitter
                    .emit_timeline(qaqh_domain::TimelineIntent::TextDelta {
                        turn_id: turn_id.to_string(),
                        round_num,
                        block_id: block_id.clone(),
                        delta: r.clone(),
                    });
                ctx.emitter
                    .emit_domain(qaqh_domain::DomainEvent::Conversation(
                        qaqh_domain::ConversationEvent::RoundDelta {
                            turn_id: turn_id.to_string(),
                            round_num,
                            kind: qaqh_domain::RoundDeltaKind::Thinking,
                            delta: r.clone(),
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
                    &r,
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
                    ctx.agent.session.record_usage(u);
                    if !ctx.agent.ephemeral {
                        ctx.agent.enqueue_meta_op(
                            crate::agent::state::agent::MetaOp::PersistUsage {
                                session_id: ctx.agent.session.session_id.clone(),
                                totals: ctx.agent.session.usage_totals.clone(),
                                last_usage: ctx.agent.session.last_usage.clone(),
                                requests: ctx.agent.session.usage_requests,
                                cache_reported_requests: ctx.agent.session.cache_reported_requests,
                            },
                        );
                    }
                    util::record_token_usage(u, &ctx.agent.config.model);
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
                        qaqh_types::ContentBlock::Reasoning { reasoning: r } => {
                            reasoning.push_str(r)
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
                ctx.agent.session.tokens = ctx.agent.session.tokens.max(u.total_tokens as u64);
                // A3：记住"已播报过"的用量水位（~1s 节流），Done 分支据此判断终值要不要补发。
                let due = last_usage_emit_at.is_none_or(|at| at.elapsed() >= USAGE_EMIT_INTERVAL);
                if due {
                    last_usage_emit_at = Some(Instant::now());
                    last_emitted_usage_total = u.total_tokens;
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
    );
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
pub(crate) fn provider_for(ctx: &RingContext, request_tag: &str) -> qaqh_gate::ProviderConfig {
    // BYOK：端点由配置自述——`wire` 决定形态，`compat` 决定与该 wire 缺省的差异。
    // 不再解析 (provider_id, endpoint) 预设坐标，也不再有按模型名兜底的专项。
    let cfg = &ctx.agent.config;
    let compat = cfg.compat.clone();
    // 路径覆写按 wire 生效；未覆写传 None，由 gate 用该 wire 的规范路径。
    let provider = match cfg.wire {
        qaqh_types::Wire::Anthropic => {
            let mut p = qaqh_gate::ProviderConfig::anthropic(
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
            };
            p
        }
        qaqh_types::Wire::OpenAi => {
            let mut p = qaqh_gate::ProviderConfig::openai(
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
    };
    provider
        .with_opencode_headers(&ctx.agent.session.session_id, request_tag)
        .with_retry(compat.retry.clone())
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
