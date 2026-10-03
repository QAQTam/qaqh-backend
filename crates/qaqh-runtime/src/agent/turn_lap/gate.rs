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

/// 一个 tool_call 的参数行数估算状态。
///
/// `ToolCallProgress.args_so_far` 是**累计串**，所以这里记消费量，每帧只把
/// 新到的尾巴喂给估算器（估算器本身恒定成本，见 `ArgLineEstimator`）。
struct ArgLineSlot {
    /// `None` = 还没认出这是写工具：provider 可能把 name 放在后面的片段里，
    /// 认出后从累计串第 0 字节重数，前面的内容不会漏。
    estimator: Option<qaqh_workspace::arg_estimate::ArgLineEstimator>,
    consumed: usize,
    last_total: u32,
    last_emit_at: Option<Instant>,
}

impl ArgLineSlot {
    /// 投喂累计参数串，返回这一帧该发的估算（`None` = 不发）。
    fn push(
        &mut self,
        tool_name: &str,
        args_so_far: &str,
    ) -> Option<qaqh_workspace::arg_estimate::ArgLineEstimate> {
        if self.estimator.is_none() {
            self.estimator = Some(qaqh_workspace::arg_estimate::ArgLineEstimator::for_tool(
                tool_name,
            )?);
            self.consumed = 0;
        }
        let estimator = self.estimator.as_mut()?;
        // `consumed` 只会是此前某个 `args_so_far.len()`，对同一条流必然是字符
        // 边界；provider 若换了缓冲（非追加），`.get` 失败就把偏移归零重数。
        let start = self.consumed.min(args_so_far.len());
        let Some(fragment) = args_so_far.get(start..) else {
            self.consumed = 0;
            return None;
        };
        if fragment.is_empty() {
            return None;
        }
        estimator.push_fragment(fragment);
        self.consumed = args_so_far.len();
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
    pub(crate) gate_error: Option<String>,
    pub(crate) current_request_usage: Option<UsageInfo>,
    pub(crate) request_error: Option<String>,
    pub(crate) stop_reason: Option<String>,
    pub(crate) last_usage: Option<UsageInfo>,
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
                    ctx.emitter
                        .emit_domain(qaqh_domain::DomainEvent::Conversation(
                            qaqh_domain::ConversationEvent::UsageUpdated {
                                turn_id: turn_id.to_string(),
                                round_num,
                                usage: final_usage.clone(),
                                context_limit: ctx.agent.config.context_limit,
                                model: ctx.agent.config.model.clone(),
                            },
                        ));
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
                args_so_far,
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
                                args_json: Some(args_so_far.clone()),
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
                        consumed: 0,
                        last_total: 0,
                        last_emit_at: None,
                    });
                if let Some(estimate) = slot.push(&name, &args_so_far) {
                    ctx.emitter
                        .emit_timeline(qaqh_domain::TimelineIntent::ToolEstimated {
                            turn_id: turn_id.to_string(),
                            round_num,
                            block_id: block_id.clone(),
                            lines_added: estimate.lines_added,
                            lines_removed: estimate.lines_removed,
                        });
                }
                // Ringing 双发：ToolCallPrepared（replaceable 预览，可被 ToolStarted 覆盖）
                ctx.emitter.emit_domain(qaqh_domain::DomainEvent::Tool(
                    qaqh_domain::ToolEvent::ToolCallPrepared {
                        tool_call_id: id.clone(),
                        turn_id: turn_id.to_string(),
                        round_num,
                        name: name.clone(),
                        args_so_far: args_so_far.clone(),
                    },
                ));
            }
            qaqh_gate::StreamEvent::WebSearchStatus(status) => {
                // Ringing 双发：ProviderToolStatus（replaceable，按 call_id 合并）
                let provider_state = match status.as_str() {
                    "completed" | "done" => qaqh_domain::ProviderToolState::Completed,
                    "searching" | "running" | "in_progress" => {
                        qaqh_domain::ProviderToolState::Searching
                    }
                    _ => qaqh_domain::ProviderToolState::InProgress,
                };
                ctx.emitter
                    .emit_domain(qaqh_domain::DomainEvent::Conversation(
                        qaqh_domain::ConversationEvent::ProviderToolStatus {
                            turn_id: turn_id.to_string(),
                            round_num,
                            call_id: format!("ws-{turn_id}-{round_num}"),
                            tool_kind: "web_search".into(),
                            state: provider_state,
                        },
                    ));
            }
            qaqh_gate::StreamEvent::UsageUpdate(u) => {
                last_usage = Some(u.clone());
                current_request_usage = Some(u.clone());
                ctx.agent.session.tokens = ctx.agent.session.tokens.max(u.total_tokens as u64);
                // A3：节流 ~1s（replaceable 覆盖显示）；终值由 Done 分支补发。
                let due = last_usage_emit_at.is_none_or(|at| at.elapsed() >= USAGE_EMIT_INTERVAL);
                if due {
                    last_usage_emit_at = Some(Instant::now());
                    last_emitted_usage_total = u.total_tokens;
                    ctx.emitter
                        .emit_domain(qaqh_domain::DomainEvent::Conversation(
                            qaqh_domain::ConversationEvent::UsageUpdated {
                                turn_id: turn_id.to_string(),
                                round_num,
                                usage: u.clone(),
                                context_limit: ctx.agent.config.context_limit,
                                model: ctx.agent.config.model.clone(),
                            },
                        ));
                }
            }
            qaqh_gate::StreamEvent::Retrying {
                attempt,
                max_retries,
                delay_secs,
                error,
            } => {
                // Ringing 双发：ProviderRetrying（重试可见性）
                ctx.emitter
                    .emit_domain(qaqh_domain::DomainEvent::Conversation(
                        qaqh_domain::ConversationEvent::ProviderRetrying {
                            turn_id: turn_id.to_string(),
                            round_num,
                            attempt,
                            max_retries,
                            delay_secs,
                            error_message: error,
                        },
                    ));
            }
            qaqh_gate::StreamEvent::Error(msg) => {
                log::error!("[TURN] gate error turn_id={turn_id} round_num={round_num}: {msg}");
                gate_error = Some(msg);
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

/// 全 runtime 唯一的 `ProviderConfig` 构造器：依当前 config / endpoint 重建 provider（Responses / Anthropic / OpenAI 形态）。
///
/// 全 runtime 唯一的 `ProviderConfig` 构造器。engine_compact / engine_title
/// 不再自建镜像，统一经由本函数，避免多处镜像漂移（T6）。
/// `request_tag` feeds the OpenCode gateway management headers (`msg_…`
/// request id): pass the turn id for normal rounds; "compact" / "title" for
/// the background LLM calls.
pub(crate) fn provider_for(ctx: &RingContext, request_tag: &str) -> qaqh_gate::ProviderConfig {
    let ep = ctx.agent.endpoint_spec.clone();
    let is_responses = ep.as_ref().map(|e| e.protocol.as_str()) == Some("responses");
    let is_anthropic = ep.as_ref().map(|e| e.protocol.as_str()) == Some("anthropic");
    // responses 协议没有增量语义：端点上声明 stateful 不会生效（不读 provider.stateful）。
    // 显式提示一次，避免"配置里写了 stateful 却静默按无状态全量发送"。
    if is_responses && ep.as_ref().is_some_and(|e| e.stateful) {
        static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            log::warn!(
                "[gate] endpoint {}/{} declares stateful=true, but the responses protocol has no incremental mode — sending full input",
                ctx.agent.config.provider_id,
                ctx.agent.config.endpoint
            );
        }
    }
    // T9/T10: 端点级重试策略随 EndpointSpec 一起传递（None = gate 内置缺省）。
    let retry = ep.as_ref().and_then(|e| e.retry.clone());
    if is_anthropic {
        let mut p = qaqh_gate::ProviderConfig::anthropic(
            &ctx.agent.config.base_url,
            &ctx.agent.config.api_key,
            &ctx.agent.config.model,
            ep.as_ref().and_then(|e| e.anthropic_path.clone()),
        );
        if let Some(endpoint) = ep.as_ref() {
            p.supports_thinking = endpoint.supports_thinking;
            p.supports_reasoning_effort = endpoint.supports_reasoning_effort;
            p.supports_reasoning_content = endpoint.supports_reasoning_content;
            p.thinking_budget_large = endpoint.thinking_budget_large;
        }
        return p
            .with_opencode_headers(&ctx.agent.session.session_id, request_tag)
            .with_retry(retry.clone());
    }
    if is_responses {
        let mut p = qaqh_gate::ProviderConfig::responses(
            &ctx.agent.config.base_url,
            &ctx.agent.config.api_key,
            &ctx.agent.config.model,
            ep.as_ref().and_then(|e| e.responses_path.clone()),
        );
        if let Some(endpoint) = ep.as_ref() {
            p.responses_compat = qaqh_gate::ResponsesCompat {
                web_search: endpoint.responses_web_search,
                echo_web_search_call: endpoint.responses_echo_web_search_call,
                send_include: endpoint.responses_send_include,
                effort_max: endpoint.responses_effort_max.clone(),
                supports_user: endpoint.responses_supports_user,
                search_function_alias: endpoint.responses_search_function_alias.clone(),
                echo_reasoning_content: endpoint.responses_echo_reasoning_content,
            };
        }
        // Muse Spark 专项：物理前缀缓存 + 关明文回放 + 放宽档位至 xhigh
        if p.model.contains("muse-spark") {
            p.prompt_cache_key = Some(ctx.agent.session.session_id.clone());
            p.responses_compat.echo_reasoning_content = false;
            p.responses_compat.send_include = false;
            p.responses_compat.effort_max = "xhigh".into();
            p.responses_compat.web_search = false;
            p.responses_compat.echo_web_search_call = false;
        }
        p.with_opencode_headers(&ctx.agent.session.session_id, request_tag)
            .with_retry(retry.clone())
    } else {
        let mut p = qaqh_gate::ProviderConfig::openai(
            &ctx.agent.config.base_url,
            &ctx.agent.config.api_key,
            &ctx.agent.config.model,
            ep.as_ref().and_then(|e| e.user_id_mode.clone()),
            ep.as_ref().and_then(|e| e.chat_path.clone()),
            ep.as_ref()
                .map(|e| e.thinking_mode.clone())
                .unwrap_or_default(),
            ep.as_ref()
                .map(|e| e.cache_field.clone())
                .unwrap_or_default(),
            ep.as_ref().map(|e| e.supports_thinking).unwrap_or(false),
            ep.as_ref().and_then(|e| e.do_sample),
        )
        // 缺省归一为 false：与 compact/title 镜像一致；None（端点配置错误）
        // 时欠配置端不发 thinking 参数，保守方向。
        .with_stateful(ep.as_ref().map(|e| e.stateful).unwrap_or(false))
        .with_stream_usage(ep.as_ref().map(|e| e.include_stream_usage).unwrap_or(false));
        if let Some(endpoint) = ep.as_ref() {
            p.supports_reasoning_effort = endpoint.supports_reasoning_effort;
            p.effort_allowlist = endpoint.effort_allowlist.clone();
            p.tool_call_content_null = endpoint.tool_call_content_null;
            p.supports_reasoning_content = endpoint.supports_reasoning_content;
            p.require_provider_parameters = endpoint.require_provider_parameters;
        }
        p.with_opencode_headers(&ctx.agent.session.session_id, request_tag)
            .with_retry(retry.clone())
    }
}

#[cfg(test)]
mod arg_line_slot_tests {
    use super::{ArgLineSlot, ESTIMATE_INTERVAL};
    use std::time::Duration;

    fn slot() -> ArgLineSlot {
        ArgLineSlot {
            estimator: None,
            consumed: 0,
            last_total: 0,
            last_emit_at: None,
        }
    }

    /// 累计串的每一帧必须是前一帧的扩展（provider 就是这么发的）。
    fn write_args(lines: usize) -> String {
        let mut out = String::from(r#"{"path":"a.txt","content":""#);
        for i in 0..lines {
            out.push_str(&format!("line{i}\\n"));
        }
        out
    }

    #[test]
    fn first_number_goes_out_at_once_then_the_interval_gates_the_rest() {
        let mut s = slot();
        let first = s.push("write", &write_args(1)).expect("第一个数立刻发");
        assert_eq!((first.lines_added, first.lines_removed), (1, 0));
        for lines in 2..=6 {
            assert!(
                s.push("write", &write_args(lines)).is_none(),
                "同一瞬间内不该再发（lines={lines}）"
            );
        }
        std::thread::sleep(ESTIMATE_INTERVAL + Duration::from_millis(5));
        let later = s.push("write", &write_args(7)).expect("跨过间隔后要发");
        assert_eq!((later.lines_added, later.lines_removed), (7, 0));
    }

    /// 数字没变就不占 timeline 的 seq。
    #[test]
    fn unchanged_number_is_not_re_emitted() {
        let mut s = slot();
        assert!(s.push("write", &write_args(2)).is_some());
        std::thread::sleep(ESTIMATE_INTERVAL + Duration::from_millis(5));
        let args = write_args(2);
        assert!(s.push("write", &args).is_none(), "重复帧没有新信息");
        assert!(s.push("write", &args).is_none());
    }

    /// provider 常把 name 放在后续片段里：认出工具后要能从累计串第 0 字节重数。
    #[test]
    fn late_name_does_not_lose_the_lines_already_streamed() {
        let mut s = slot();
        for lines in 1..=3 {
            assert!(
                s.push("", &write_args(lines)).is_none(),
                "空 name 阶段不发估算"
            );
        }
        std::thread::sleep(ESTIMATE_INTERVAL + Duration::from_millis(5));
        let late = s
            .push("write", &write_args(4))
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
            assert!(s.push(name, &write_args(5)).is_none(), "{name}");
        }
    }

    /// 累计串被整体替换（不是追加）时，旧偏移可能落在多字节字符中间：
    /// 必须安静归零重数，而不是切片 panic。
    #[test]
    fn non_prefixed_resync_does_not_panic() {
        let mut s = slot();
        // 先消费 48 字节（write_args(2) 的长度）。
        assert!(s.push("write", &write_args(2)).is_some());
        // 第 48 个字节落在 `汉` 的三个字节中间。
        let replaced = format!("{}汉{}", "a".repeat(47), r#"\n"#);
        assert!(
            s.push("write", &replaced).is_none(),
            "偏移越界这一帧不发估算"
        );
        std::thread::sleep(ESTIMATE_INTERVAL + Duration::from_millis(5));
        let after = s.push("write", &write_args(9)).expect("重同步后要继续出数");
        assert!(after.lines_added >= 9, "got {after:?}");
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
