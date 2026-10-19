//! CompactEngine: context compaction — token-split → prompt → LLM → apply.
//!
//! Two-step flow (a background thread runs the LLM call between the two steps):
//! 1. `build_prompt_and_meta()` — synchronous, fast (token split + prompt build)
//! 2. `apply_result()` — synchronous, fast (apply on main thread)
//!
//! Between them, a background `chat_stream()` call runs in a thread (non-blocking,
//! streaming tokens to frontend via CompactProgress events).

use crate::agent::types::*;
use crate::agent::util;

/// Result produced by the background compact thread.
pub(crate) struct CompactMeta {
    pub compact_id: String,
    pub summary: String,
    pub kept_user_count: usize,
    pub context_revision: u64,
    pub error: Option<String>,
}

/// Compaction prompt V2: structured handoff protocol.
///
/// The LLM must produce a decision-first summary with mandatory anchor fields
/// (file line counts, build status, complexity-labeled remaining work).
/// The Thinking Appendix is optional and only included under strict rules.
const COMPACT_PROMPT: &str = "\
You are performing a CONTEXT CHECKPOINT COMPACTION. Create a structured \
handoff summary for another LLM that will resume the task.\n\
\n\
## OUTPUT FORMAT (follow strictly)\n\
\n\
### Decision Log\n\
For each key decision made during the work:\n\
- **Decision**: {what was chosen}\n\
- **Alternatives**: {what was rejected and why, if any}\n\
- **Status**: done / started but not finished / cancelled\n\
\n\
### State Snapshot\n\
- **Key files** (path + approximate line count): file.rs(~L123), ...\n\
- **Build status**: cargo check output summary (errors, warnings)\n\
- **Last successful action**: {command or tool} at {timestamp or turn}\n\
\n\
### Remaining Work\n\
Use complexity labels: [small], [medium], [large]. Include rough estimates.\n\
- [small] {task}  — ~1 edit or trivial fix\n\
- [medium] {task} — multiple edits, one file\n\
- [large] {task}  — new file or cross-crate changes\n\
\n\
### Thinking Appendix (OPTIONAL — include ONLY when applicable)\n\
Include ONLY when:\n\
- ≥2 dead-end investigation paths were tried before finding the root cause\n\
  → briefly note each dead-end and why it was wrong\n\
- A cross-crate or cross-module causality chain was needed to diagnose an issue\n\
  → note the chain (e.g. \"qaqh-workspace → qaqh-message → qaqh-session\")\n\
\n\
If neither condition is met, OMIT this section entirely.\n\
\n\
## RULES\n\
- Be concise: minimum tokens with maximum information density.\n\
- Do NOT mention the compaction process itself.\n\
- Decision Log, State Snapshot, and Remaining Work are MANDATORY.\n\
- Thinking Appendix is OPTIONAL — omit if no dead-ends or cross-crate chains.\n\
- If a section has no content, write \"None\" instead of omitting it.";

/// Prefix injected before a previous summary in UPDATE MODE.
/// Tells the LLM to merge new context with the prior structured handoff,
/// preserving existing Decision Log entries unless superseded.
const SUMMARY_PREFIX: &str = "\
Another language model previously worked on this task and produced a \
structured handoff summary (below). Merge the new context with the \
previous summary to create an updated checkpoint:\n\
- Preserve existing Decision Log entries unless the new context shows \
  they were completed or superseded.\n\
- Update State Snapshot and Remaining Work with the latest information.\n\
- If the previous summary has a Thinking Appendix, decide if the new \
  context adds more dead-end paths worth preserving; otherwise drop it.";

/// 一次压缩调用所需的全部参数（[`build_prompt_and_meta`] 的产物）。
pub(crate) struct CompactPlan {
    pub prompt: String,
    pub kept_user_count: usize,
    pub provider: qaqh_gate::ProviderConfig,
    pub compact_id: String,
}

/// 压缩**没有发起**的原因。
///
/// 与"压缩失败"分开：这些分支连 LLM 都没调用。每条都带稳定 code（日志与
/// 埋点聚合用）和中文说明（直接作为终态事件的 `reason`）。旧实现把它们折叠
/// 成一个 `None`，调用方只能笼统地说"没有可压缩内容"，与真实原因常常不符
/// （provider 不可用也会被说成"历史已经够短"）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CompactRefusal {
    /// 活跃历史里没有任何可压缩的消息。
    EmptyHistory,
    /// 头部只剩上一份摘要，没有新的轮次可折叠。
    NothingEligible,
    /// 端点/provider 配置不完整，无法发起压缩请求。
    ProviderUnavailable(String),
}

impl CompactRefusal {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::EmptyHistory => "compact_empty_history",
            Self::NothingEligible => "compact_nothing_eligible",
            Self::ProviderUnavailable(_) => "compact_provider_unavailable",
        }
    }

    /// 面向用户的中文说明（进 `CompactFinished.reason`）。
    pub(crate) fn reason(&self) -> String {
        match self {
            Self::EmptyHistory => "没有可压缩的历史：当前上下文里还没有任何消息。".into(),
            Self::NothingEligible => "没有可压缩的轮次：历史只剩最近保留区和上一份摘要。".into(),
            Self::ProviderUnavailable(detail) => {
                format!("压缩未发起：端点配置不可用（{detail}）。")
            }
        }
    }
}

/// Step 1: Token-split, serialize, build prompt — fast, synchronous.
///
/// 返回 [`CompactPlan`]；没有可压缩内容或 provider 不可用时返回
/// [`CompactRefusal`]，调用方必须把它的中文原因写进终态事件。
/// `compact_id` 在同一函数内生成，与随后发出的 `CompactStarted` 关联。
pub(crate) fn build_prompt_and_meta(ctx: &mut RingContext) -> Result<CompactPlan, CompactRefusal> {
    const KEEP_TOKENS: usize = 4_000;
    ctx.agent.record_memory_marker("compact.prompt.build.begin");
    let turns_total = ctx.agent.msg.turn_count();
    log::info!("[COMPACT] {} turns", turns_total);

    let all = ctx.agent.msg.build_context_for_gate(&[]);
    let msgs: Vec<&qaqh_types::Message> = all.iter().filter(|m| m.role != "system").collect();
    if msgs.is_empty() {
        return Err(CompactRefusal::EmptyHistory);
    }

    let mut kept_idx = msgs.len();
    let mut kept_tokens = 0usize;
    for (i, m) in msgs.iter().enumerate().rev() {
        let t = estimate_message_tokens(m);
        if kept_tokens + t > KEEP_TOKENS {
            kept_idx = i + 1;
            break;
        }
        kept_tokens += t;
        kept_idx = i;
    }

    // 保护：当尾部消息本身超过 KEEP_TOKENS 时（如超大 tool result），
    // 标记扫描会导致 kept 区间为空 → apply_compact 清空全部 turn。
    // 回退到至少保留最后一条 user 消息所在的 turn。
    if kept_idx == msgs.len() {
        for (i, m) in msgs.iter().enumerate().rev() {
            if m.role == "user" {
                kept_idx = i;
                break;
            }
        }
        // 如果连一条 user 消息都没有（极端情况），放弃本次 compact。
        if kept_idx == msgs.len() {
            return Err(CompactRefusal::NothingEligible);
        }
    }

    let head_msgs = &msgs[..kept_idx];
    if head_msgs.is_empty() {
        return Err(CompactRefusal::NothingEligible);
    }

    let previous_summary = ctx.agent.msg.previous_compact_summary();
    let head_user_count = compactable_head_user_count(head_msgs);
    // Immediately after a successful compact, the only item before the
    // 4K tail can be the synthetic checkpoint itself. Re-compacting that
    // checkpoint removes no real turn and can repeat every gate lap when
    // fixed system/tool overhead still keeps the prompt above threshold.
    if head_user_count == 0 {
        log::info!("[COMPACT] refused(compact_nothing_eligible): 头部只剩上一份摘要");
        return Err(CompactRefusal::NothingEligible);
    }
    // G2：keep 必须是"真实 turn 数"而非扁平 user 消息数——注入密集时
    // 扁平计数 ≥ turns.len()，apply_compact 早退（skip==0）却谎报成功。
    let kept_user_count = ctx.agent.msg.count_live_turns_from(msgs[kept_idx].msg_id);
    // 保留区已经覆盖全部活跃 turn：apply 阶段会整体 no-op，先在这里挡掉，
    // 不白花一次摘要请求。
    if kept_user_count >= turns_total {
        log::info!(
            "[COMPACT] refused(compact_nothing_eligible): kept={kept_user_count} >= turns={turns_total}"
        );
        return Err(CompactRefusal::NothingEligible);
    }

    // T6: 唯一构造器（turn_lap::gate::provider_for），与主 turn 完全同构；
    // 历史镜像缺 thinking_budget_large / effort_allowlist / stateful /
    // stream_usage / muse-spark 专项，均在唯一构造器内补齐。
    //
    // 必须在发出 `CompactStarted` **之前**构造：provider 失败意味着本次压缩
    // 从未开始，提前发开始事件会留下一条永远不终态的"压缩中"卡片。
    let provider = crate::agent::turn_lap::gate::provider_for(ctx, "compact").map_err(|e| {
        log::warn!("[COMPACT] refused(compact_provider_unavailable): {e}");
        CompactRefusal::ProviderUnavailable(e)
    })?;

    let compact_id = format!(
        "compact-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    );
    // Ringing 双发：CompactStarted（权威开始事件，携带 compact_id）
    ctx.emitter
        .emit_domain(qaqh_domain::DomainEvent::Conversation(
            qaqh_domain::ConversationEvent::CompactStarted {
                compact_id: compact_id.clone(),
                turns_total: turns_total as u32,
                turns_keeping: kept_user_count as u32,
            },
        ));

    // The previous checkpoint already appears in <previous-summary> below.
    // Do not serialize the synthetic `[Compacted ...]` turn into HISTORY
    // again, or repeated compaction duplicates and recursively amplifies it.
    let history_head = compact_history_head(head_msgs, previous_summary.is_some());
    let contexts = serialize_messages(&history_head, &msgs[kept_idx..]);
    let timeline = {
        let created = ctx.agent.session.created_at;
        let updated = ctx.agent.session.updated_at.max(qaqh_session::now_epoch());
        let start_str = util::epoch_to_date(created);
        let dur = updated.saturating_sub(created);
        format!(
            "- Session started: {start_str} (UTC)\n- Session duration: {}h {}m real-time",
            dur / 3600,
            (dur % 3600) / 60
        )
    };

    // ── 长程导航锚点（file_state + todo）──
    //
    // 为什么必须在这里补：`<file_state>` 只在**会话首次** `build_context` 时
    // 冻结进 `frozen_annotation`（agent.rs `build_context` 文档），而压缩会把
    // 携带该注解的**首条 user 消息连同旧 turn 一起折叠掉**。折叠后 file_state
    // 不再出现在任何存活消息里，模型从此丢失「改过哪些文件」的事实。
    // todo 同理：它是独立于消息历史的会话级状态，压缩不会动它，但也不会
    // 把它带进摘要。两者都是「长程工作不迷失方向」的锚点，在此显式补上。
    let anchors = build_navigation_anchors(ctx);

    let prompt = if let Some(ref prev) = previous_summary {
        format!(
            "[COMPACT — UPDATE MODE]\n\n\
                 {SUMMARY_PREFIX}\n\n\
                 <previous-summary>\n{prev}\n</previous-summary>\n\n\
                 {anchors}\n\
                 --- HISTORY (newer context to merge) ---\n\
                 {}\n\
                 --- END HISTORY ---\n\n\
                 {COMPACT_PROMPT}",
            contexts.join("\n\n"),
        )
    } else {
        format!(
            "[COMPACT]\n\n\
                 Create a new checklist summary from the conversation history.\n\n\
                 {anchors}\n\
                 --- HISTORY ---\n\
                 {}\n\
                 --- END HISTORY ---\n\n\
                 Timeline:\n{timeline}\n\n\
                 {COMPACT_PROMPT}",
            contexts.join("\n\n"),
        )
    };

    if !ctx.agent.session.session_id.is_empty() {
        qaqh_memwatch::global().record_phase(
            "compact.prompt.build.end",
            Some(&ctx.agent.session.session_id),
            Some(ctx.agent.msg.memory_usage_estimate().heap_estimate_bytes),
            Some(prompt.len() as u64),
            None,
        );
    }

    log::info!(
        "[COMPACT] start {}: turns_total={turns_total} keeping={kept_user_count} head_user={head_user_count} prompt_bytes={}",
        compact_id,
        prompt.len()
    );
    Ok(CompactPlan {
        prompt,
        kept_user_count,
        provider,
        compact_id,
    })
}

/// D10 fact 产生侧：把成功的压缩终态写成 canonical `CompactionApplied` fact。
///
/// 调用时序（两处成功路径共用）：`persist_compaction` 之后、`CompactFinished`
/// 域事件之前——durable append 先于投影发布，是 fact bus 的不变量。
/// `checkpoint_id` 与流式阶段的 `compact-<millis>` 关联 id 不同域：canonical
/// 校验要求 `ckpt_` + ULID。失败只降级记录——messages.jsonl 仍是压缩真相，
/// fact 缺失只损失 v2 投影/回执折叠，不允许拖垮本轮 turn。
pub(crate) fn publish_compaction_fact(ctx: &mut RingContext, summary: &str) {
    use crate::agent::state::agent::{tool_ledger_lease_ms, unix_ms};

    let now = unix_ms();
    let summary_ref = qaqh_session::session_fact_v2::ContentRef::new(
        qaqh_session::canonical::sha256_content_hash(summary.as_bytes()),
    );
    let checkpoint_id = qaqh_session::session_fact_v2::CheckpointId::new(format!(
        "ckpt_{}",
        qaqh_session::canonical::generate_ulid()
    ));
    let context_revision = ctx.agent.msg.context_revision();
    let Ok(Some(ledger)) = ctx.agent.tool_ledger_mut() else {
        return;
    };
    if let Err(error) = ledger.ensure_lease(now, tool_ledger_lease_ms()) {
        log::warn!("[COMPACT] compaction fact lease unavailable: {error}");
        return;
    }
    let event_id =
        qaqh_session::session_fact_v2::EventId::new(qaqh_session::canonical::generate_ulid());
    if let Err(error) = ledger.append_compaction_applied(
        event_id,
        checkpoint_id,
        summary_ref,
        context_revision,
        now,
    ) {
        log::warn!("[COMPACT] compaction fact append failed (degraded): {error}");
    }
}

/// 压缩终态的唯一出口。
///
/// 无论成功、跳过、取消还是失败，都从这里发 `CompactFinished` 并打一条
/// `[COMPACT] finish` 埋点——终态与原因必须成对出现，任何只给 `status` 的
/// 路径都会让界面显示"已取消"而没人知道为什么。
struct CompactTerminal {
    compact_id: String,
    status: qaqh_domain::CompactStatus,
    summary_chars: usize,
    /// 被折叠进摘要的真实 turn 数。
    folded_turns: usize,
    reason: Option<String>,
}

impl CompactTerminal {
    fn emit(self, ctx: &RingContext) {
        log::info!(
            "[COMPACT] finish {}: status={:?} folded_turns={} summary_chars={} turns_after={} reason={}",
            self.compact_id,
            self.status,
            self.folded_turns,
            self.summary_chars,
            ctx.agent.msg.turn_count(),
            self.reason.as_deref().unwrap_or("-")
        );
        ctx.emitter
            .emit_domain(qaqh_domain::DomainEvent::Conversation(
                qaqh_domain::ConversationEvent::CompactFinished {
                    compact_id: self.compact_id,
                    status: self.status,
                    summary_chars: Some(self.summary_chars),
                    turns_compacted: Some(self.folded_turns as u32),
                    turns_removed: Some(self.folded_turns as u32),
                    reason: self.reason,
                },
            ));
    }
}

/// 压缩失败的结构化错误（`Control` 频道）。v1 广播面已删，这条只对仍挂着
/// 老 hub 的宿主有效；面向客户端的原因以 `CompactFinished.reason` 为准。
fn emit_compact_error(ctx: &RingContext, compact_id: &str, code: &str, message: &str) {
    log::error!("[COMPACT] {compact_id} {code}: {message}");
    ctx.emitter.emit_domain(qaqh_domain::DomainEvent::Control(
        qaqh_domain::ControlEvent::OperationFailed {
            occurrence_id: format!("occ-compact-{compact_id}"),
            scope: qaqh_domain::ErrorScope::Conversation,
            error: qaqh_domain::DomainError {
                error_id: format!("err-compact-{compact_id}"),
                code: code.to_string(),
                message: message.to_string(),
                retryable: true,
                dedupe_key: Some(code.to_string()),
            },
            operation_id: Some(compact_id.to_string()),
        },
    ));
}

/// 压缩应用阶段的 memwatch 埋点（仅 opt-in 采集时生效）。
fn record_compact_phase(ctx: &RingContext, phase: &str, summary_chars: usize) {
    let session_id = &ctx.agent.session.session_id;
    if session_id.is_empty() {
        return;
    }
    qaqh_memwatch::global().record_phase(
        phase,
        Some(session_id),
        Some(ctx.agent.msg.memory_usage_estimate().heap_estimate_bytes),
        Some(summary_chars as u64),
        None,
    );
}

/// Step 2: Apply compact result on the live message store (called from main thread).
pub(crate) fn apply_result(ctx: &mut RingContext, meta: &CompactMeta) {
    let current_revision = ctx.agent.msg.context_revision();
    if meta.context_revision != current_revision {
        // 结果基于旧上下文：应用它会覆盖压缩期间新落盘的内容（用户新消息、
        // 注入等）。丢弃是对的，但必须说清楚为什么——只报"已取消"会让用户
        // 以为压缩功能坏了，也无从判断要不要重试。
        let reason = format!(
            "压缩期间上下文已更新（revision {} → {}），为避免覆盖新内容已丢弃本次摘要；请重新发起压缩。",
            meta.context_revision, current_revision
        );
        emit_compact_error(ctx, &meta.compact_id, "compact_stale_result", &reason);
        CompactTerminal {
            compact_id: meta.compact_id.clone(),
            status: qaqh_domain::CompactStatus::Cancelled,
            summary_chars: 0,
            folded_turns: 0,
            reason: Some(reason),
        }
        .emit(ctx);
        return;
    }
    if let Some(ref err) = meta.error {
        let reason = err.clone();
        emit_compact_error(ctx, &meta.compact_id, "compact_failed", &reason);
        CompactTerminal {
            compact_id: meta.compact_id.clone(),
            status: qaqh_domain::CompactStatus::Failed,
            summary_chars: 0,
            folded_turns: 0,
            reason: Some(reason),
        }
        .emit(ctx);
        return;
    }
    let chars = meta.summary.chars().count();

    ctx.agent.record_memory_marker("compact.apply.begin");
    // 压缩前后的有效载荷字节数：一次"压缩"若让上下文变长（摘要比被折叠的
    // 内容还大），就在这里留痕——旧实现没有任何地方记录这个差值。
    let payload_before = ctx.agent.msg.memory_usage_estimate().payload_bytes;
    let effect = ctx
        .agent
        .msg
        .apply_compact(&meta.summary, meta.kept_user_count);
    ctx.agent
        .msg
        .persist_compaction(&ctx.agent.config.model, &ctx.agent.config.reasoning_effort);

    if !effect.applied() {
        // 保留区已覆盖全部历史：此刻写摘要不会压掉任何东西，只会让上下文
        // 凭空多出一段检查点。如实报"跳过"而不是"取消/失败"。
        let reason = format!(
            "没有可折叠的轮次：保留区已覆盖全部历史（保留 {} 个 turn），本次未写入摘要。",
            meta.kept_user_count.max(1)
        );
        CompactTerminal {
            compact_id: meta.compact_id.clone(),
            status: qaqh_domain::CompactStatus::Skipped,
            summary_chars: 0,
            folded_turns: 0,
            reason: Some(reason),
        }
        .emit(ctx);
        return;
    }

    // D10 fact 产生侧：durable append 先于 CompactFinished 域事件发布。
    publish_compaction_fact(ctx, &meta.summary);
    record_compact_phase(ctx, "compact.apply.end", chars);
    ctx.agent.record_memory_marker("compact.apply.end");

    let payload_after = ctx.agent.msg.memory_usage_estimate().payload_bytes;
    log::info!(
        "[COMPACT] applied {}: folded_turns={} payload_bytes {payload_before} → {payload_after}",
        meta.compact_id,
        effect.folded_turns
    );
    if payload_after >= payload_before {
        log::warn!(
            "[COMPACT] {} did not shrink the context (payload_bytes {payload_before} → {payload_after}): summary_bytes={chars}",
            meta.compact_id
        );
    }

    CompactTerminal {
        compact_id: meta.compact_id.clone(),
        status: qaqh_domain::CompactStatus::Completed,
        summary_chars: chars,
        folded_turns: effect.folded_turns,
        reason: None,
    }
    .emit(ctx);
}

/// 长程导航锚点：把压缩后**会丢失**的会话级事实补回摘要输入。
///
/// 两块内容都来自压缩之外的实时数据源，而不是消息历史：
/// - `<file_state>`：`qaqh_workspace::file_state::summary()`（最近 20 个被触碰
///   的文件 + 行数 + 操作）。压缩会折叠携带冻结注解的首条 user 消息，
///   该块从此不再出现在任何存活消息里。
/// - todo：`load_todo_for(seed)` 的会话级计划（含 Goal 模式当前步骤）。
///
/// 两者都非致命：读失败（无会话/无 todo.json）时静默跳过，不阻断压缩。
/// 无内容时返回空串，调用方模板中的占位会退化为一个空行。
fn build_navigation_anchors(ctx: &RingContext) -> String {
    build_navigation_anchors_for(&ctx.agent.session.session_id)
}

/// 锚点构造主体（与 `RingContext` 解耦，便于单测直接调用）。
fn build_navigation_anchors_for(session_id: &str) -> String {
    let mut out = String::new();

    let files = qaqh_workspace::file_state::summary();
    if !files.is_empty() {
        out.push_str(
            "Current file state (files touched this session; use these paths and \
             line counts instead of re-reading):\n",
        );
        out.push_str(&files);
        out.push('\n');
    }

    if !session_id.is_empty()
        && let Ok(store) = qaqh_workspace::todo::load_todo_for(session_id)
        && !store.items.is_empty()
    {
        let current = store.current_id.as_deref();
        out.push_str("Task checklist (session-scoped; preserve this in the summary):\n");
        if store.mode == qaqh_workspace::todo::TodoMode::Goal {
            out.push_str("  mode: goal (autonomous execution)\n");
        }
        for item in &store.items {
            let mark = match item.status {
                qaqh_workspace::todo::TodoStatus::Completed => "x",
                qaqh_workspace::todo::TodoStatus::InProgress => ">",
                qaqh_workspace::todo::TodoStatus::Cancelled => "-",
                qaqh_workspace::todo::TodoStatus::Pending => " ",
            };
            let cursor = if Some(item.id.as_str()) == current {
                "  <- current"
            } else {
                ""
            };
            out.push_str(&format!(
                "  [{}] T{}: {}{}\n",
                mark, item.id, item.title, cursor
            ));
        }
    }

    out
}

fn estimate_message_tokens(message: &qaqh_types::Message) -> usize {
    use crate::agent::state::token_calibration::{image_token_charge, redact_image_payloads};

    // Same accounting rule as the gate pre-flight: inline image bytes are
    // charged a fixed per-image budget, not their base64 length
    // (BUG-2026-09-16-05 / D-14).
    let mut accounted = [message.clone()];
    let images = redact_image_payloads(&mut accounted);
    let serialized = serde_json::to_string(&accounted).unwrap_or_default();
    (u64::from(qaqh_types::count_tokens(&serialized)) + image_token_charge(images)) as usize
}

// ═══════════════════════════════════════════════════════
// Background worker — runs in a separate thread
// ═══════════════════════════════════════════════════════

/// 压缩请求的 system 占位（方案 B）。
///
/// 上游渠道校验要求**首条消息是 system prompt**（实测 qaqh 压缩请求因首条为
/// `user` 被 `code=11128 "first message is not system prompt"` 拒绝，HTTP 400）。
/// 同仓另两条旁路调用都自带 system（标题生成 `engine_title.rs`、正常轮次
/// `build_context_for_gate` 前置 `system_messages`），仅压缩路径漏了。
///
/// 这里**只放一句角色定位**，不动原有 prompt 的任何措辞与位置（HISTORY 与
/// `COMPACT_PROMPT` 仍全在 user 消息里）——把指令搬进 system 会改变模型行为，
/// 不属本次修复范围。
const COMPACT_SYSTEM: &str =
    "You are a context-compaction assistant. Summarize the conversation history below.";

/// 压缩请求的消息体唯一构造器（manual / auto-compact 共用）。
///
/// 首条必须是 system：上游渠道硬校验（见 `COMPACT_SYSTEM` 文档）。此前两条
/// 压缩入口各自内联 `vec![Message::user(&prompt)]`，因此同一缺陷存在两份；
/// 收敛到此处后不会再次漂移。
pub(crate) fn compact_request_messages(prompt: &str) -> Vec<qaqh_types::Message> {
    vec![
        qaqh_types::Message::system(COMPACT_SYSTEM),
        qaqh_types::Message::user(prompt),
    ]
}

/// Run the LLM compaction call in a background thread.
/// Uses streaming so the user can see the model output in real-time
/// via `CompactProgress` events pushed through `event_tx`.
/// Returns CompactMeta via the channel.
#[allow(clippy::too_many_arguments)] // 参数面塑形另立项（PLAN D-5）
pub(crate) fn run_compact_worker(
    session_id: String,
    compact_id: String,
    prompt: String,
    provider: qaqh_gate::ProviderConfig,
    kept_user_count: usize,
    context_revision: u64,
    event_tx: std::sync::mpsc::SyncSender<crate::agent::types::WriterEvent>,
    causation_id: Option<String>,
) -> CompactMeta {
    // 首条必须是 system：上游渠道硬校验（见 `COMPACT_SYSTEM` 文档）。
    let msgs_vec = compact_request_messages(&prompt);
    let mut summary = String::new();
    let mut progress_seq = 0u64;

    let mut on_event = |ev: qaqh_gate::StreamEvent| match ev {
        qaqh_gate::StreamEvent::ContentDelta(delta) => {
            summary.push_str(&delta);
            // Ringing 双发：CompactProgress（replaceable 流式摘要）
            progress_seq += 1;
            let env = qaqh_ringing::RingingWorkerEventEnvelope::new(
                session_id.as_str(),
                format!("w-compact-{compact_id}-{progress_seq}"),
                qaqh_domain::DomainEvent::Conversation(
                    qaqh_domain::ConversationEvent::CompactProgress {
                        compact_id: compact_id.clone(),
                        delta,
                    },
                )
                .into(),
            );
            let env = match causation_id.as_deref() {
                Some(command_id) => env.with_causation(command_id),
                None => env,
            };
            let _ = event_tx.send(crate::agent::types::WriterEvent::Ringing(env));
        }
        qaqh_gate::StreamEvent::ReasoningDelta { text, .. } => {
            // legacy CompactDelta reasoning 透传已退役：Ringing 无 reasoning 专用事件，
            // 压缩过程的思考链由 CompactProgress（摘要流）覆盖（convergence-plan §4.2 登记）。
            let _ = text;
        }
        _ => {}
    };

    match qaqh_gate::chat_stream(
        &provider,
        msgs_vec,
        None,
        20480,
        None,
        None,
        None,
        &mut on_event,
    ) {
        Ok(()) if !summary.trim().is_empty() => CompactMeta {
            compact_id,
            summary,
            kept_user_count,
            context_revision,
            error: None,
        },
        Ok(()) => CompactMeta {
            compact_id,
            summary: String::new(),
            kept_user_count,
            context_revision,
            error: Some("压缩失败：模型返回了空摘要。".into()),
        },
        Err(e) => CompactMeta {
            compact_id,
            summary: String::new(),
            kept_user_count,
            context_revision,
            error: Some(format!("压缩失败：摘要请求未完成（{e}）。")),
        },
    }
}

// ═══════════════════════════════════════════════════════
// Message serialization helpers
// ═══════════════════════════════════════════════════════

fn is_compact_summary_message(message: &qaqh_types::Message) -> bool {
    message.role == "user"
        && message.content.iter().any(|block| {
            matches!(
                block,
                qaqh_types::ContentBlock::Text { text } if text.starts_with("[Compacted ")
            )
        })
}

fn compact_history_head<'a>(
    head: &[&'a qaqh_types::Message],
    has_previous_summary: bool,
) -> Vec<&'a qaqh_types::Message> {
    if has_previous_summary {
        head.iter()
            .copied()
            .filter(|message| !is_compact_summary_message(message))
            .collect()
    } else {
        head.to_vec()
    }
}

fn compactable_head_user_count(head: &[&qaqh_types::Message]) -> usize {
    head.iter()
        .filter(|message| message.role == "user" && !is_compact_summary_message(message))
        .count()
}

fn serialize_messages(head: &[&qaqh_types::Message], kept: &[&qaqh_types::Message]) -> Vec<String> {
    let mut out = Vec::new();
    for m in head {
        let role = &m.role;
        let lines: Vec<String> = m
            .content
            .iter()
            .filter_map(|b| match b {
                qaqh_types::ContentBlock::Text { text } => Some(format!("[{role}]: {text}")),
                qaqh_types::ContentBlock::Reasoning { .. } => None,
                qaqh_types::ContentBlock::ToolUse { name, input, .. } => {
                    let args = serde_json::to_string(input).unwrap_or_default();
                    let end = args.floor_char_boundary(args.len().min(120));
                    Some(format!(
                        "[{role} tool call]: {}({})",
                        name,
                        args.get(..end).unwrap_or(&args)
                    ))
                }
                qaqh_types::ContentBlock::WebSearchCall { action, .. } => {
                    let action_str = serde_json::to_string(action).unwrap_or_default();
                    Some(format!("[{role} web search]: {action_str}"))
                }
                qaqh_types::ContentBlock::ToolResult { result, .. } => {
                    let compact: String = result
                        .model_text()
                        .lines()
                        .take(5)
                        .map(|l| l.chars().take(200).collect::<String>())
                        .collect::<Vec<_>>()
                        .join(" | ");
                    let end = compact.floor_char_boundary(compact.len().min(600));
                    Some(format!(
                        "[Tool result]: {}",
                        compact.get(..end).unwrap_or(&compact)
                    ))
                }
                qaqh_types::ContentBlock::Image { .. }
                | qaqh_types::ContentBlock::ImageRef { .. } => {
                    Some(format!("[{role}]: [Image attached]"))
                }
                qaqh_types::ContentBlock::ResponseOutputItem { .. } => None,
            })
            .collect();
        if !lines.is_empty() {
            out.push(lines.join("\n"));
        }
    }
    for m in kept {
        if m.role == "tool"
            && let Some(qaqh_types::ContentBlock::ToolResult { result, .. }) = m.content.first()
        {
            let compact: String = result
                .model_text()
                .lines()
                .take(3)
                .map(|l| l.chars().take(200).collect::<String>())
                .collect::<Vec<_>>()
                .join(" | ");
            let end = compact.floor_char_boundary(compact.len().min(400));
            out.push(format!(
                "[Tool result (recent)]: {}",
                compact.get(..end).unwrap_or(&compact)
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{
        build_navigation_anchors_for, compact_history_head, compactable_head_user_count,
        estimate_message_tokens, serialize_messages,
    };

    #[test]
    fn update_mode_does_not_repeat_previous_summary_in_history() {
        let old = qaqh_types::Message::user("[Compacted 4 turns]\nold checkpoint body");
        let newer = qaqh_types::Message::user("new work after checkpoint");
        let head = vec![&old, &newer];

        let filtered = compact_history_head(&head, true);
        let history = serialize_messages(&filtered, &[]).join("\n");

        assert!(!history.contains("old checkpoint body"));
        assert!(history.contains("new work after checkpoint"));
    }

    #[test]
    fn initial_mode_keeps_ordinary_history_unchanged() {
        let message = qaqh_types::Message::user("ordinary history");
        let head = vec![&message];

        let filtered = compact_history_head(&head, false);
        let history = serialize_messages(&filtered, &[]).join("\n");

        assert!(history.contains("ordinary history"));
    }

    #[test]
    fn previous_summary_alone_is_not_a_new_compaction_candidate() {
        let old = qaqh_types::Message::user("[Compacted 4 turns]\nold checkpoint body");
        let head = vec![&old];

        assert_eq!(compactable_head_user_count(&head), 0);
    }

    #[test]
    fn real_turn_after_previous_summary_is_compactable() {
        let old = qaqh_types::Message::user("[Compacted 4 turns]\nold checkpoint body");
        let real = qaqh_types::Message::user("new completed work");
        let head = vec![&old, &real];

        assert_eq!(compactable_head_user_count(&head), 1);
    }

    #[test]
    fn tail_budget_uses_tokenizer_for_cjk_content() {
        let message = qaqh_types::Message::user(&"上下文压缩".repeat(100));

        assert!(estimate_message_tokens(&message) > 300);
    }

    /// 回归（BUG-2026-09-16-05 / D-14 第二处盲点）：图片字节按固定预算计，
    /// 不按 base64 长度线性增长。
    #[test]
    fn message_tokens_charge_images_a_fixed_budget() {
        let mut small = qaqh_types::Message::user("see this");
        small.content.push(qaqh_types::ContentBlock::image(
            "image/png",
            &"A".repeat(1_024),
        ));
        let mut huge = qaqh_types::Message::user("see this");
        huge.content.push(qaqh_types::ContentBlock::image(
            "image/png",
            &"A".repeat(1_048_576),
        ));

        let small_tokens = estimate_message_tokens(&small);
        let huge_tokens = estimate_message_tokens(&huge);
        assert!(
            huge_tokens <= small_tokens + 64,
            "image bytes must not grow the estimate linearly: small={small_tokens} huge={huge_tokens}"
        );
    }

    /// 回归（上游 `code=11128 "first message is not system prompt"`）：
    /// 压缩请求的**首条必须是 system**。
    ///
    /// 实测背景：qaqh 压缩请求曾只发一条 `user`，被上游渠道校验以 HTTP 400
    /// 拒绝（`data/fault-bodies/…-seq2394`）；同仓标题生成与正常轮次都自带
    /// system，仅压缩路径漏了。两条压缩入口（manual / auto）现共用
    /// [`compact_request_messages`]，本用例锁住该契约。
    #[test]
    fn compact_request_starts_with_a_system_message() {
        let msgs = super::compact_request_messages("[COMPACT]\n\n--- HISTORY ---\n...");

        assert_eq!(msgs.len(), 2, "system 占位 + 原 prompt 的 user 消息");
        assert_eq!(
            msgs[0].role, "system",
            "上游要求首条为 system，否则 11128 拒绝"
        );
        assert_eq!(msgs[1].role, "user");
        // 原有 prompt 措辞与位置不变（方案 B：零扰动）。
        let user_text = msgs[1]
            .content
            .iter()
            .find_map(|b| match b {
                qaqh_types::ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
            .expect("user prompt text");
        assert!(user_text.starts_with("[COMPACT]"));
        assert!(user_text.contains("--- HISTORY ---"));
    }

    /// 回归：压缩后 file_state 锚点必须出现在摘要输入里。
    ///
    /// `<file_state>` 只在会话首次 `build_context` 时冻结进首条 user 消息，
    /// 而压缩会把那条消息连同旧 turn 一起折叠——不在此补回，模型就再也
    /// 看不到「改过哪些文件」。
    ///
    /// 注：todo 需 `sessions/{seed}/todo.json`（依赖 RUNTIME_CTX 会话目录），
    /// 不在单测里伪造；该分支读失败即静默跳过，不影响本用例。
    #[test]
    fn navigation_anchors_include_file_state() {
        let temp = tempfile::tempdir().expect("tempdir");
        qaqh_workspace::set_workspace(&temp.path().to_string_lossy());
        qaqh_workspace::file_state::clear();
        qaqh_workspace::file_state::record_read("src/lib.rs", "fn main() {}\n", 3);

        let anchors = build_navigation_anchors_for("");
        assert!(
            anchors.contains("<file_state>"),
            "file_state 必须进锚点，got: {anchors}"
        );
        assert!(anchors.contains("src/lib.rs"), "got: {anchors}");

        qaqh_workspace::file_state::clear();
    }

    /// 压缩"没有发起"的每种原因都必须给出可区分的中文说明。
    ///
    /// 客户端把 `CompactFinished.reason` 直接展示给用户；旧实现把所有分支
    /// 折叠成同一个 `None`，界面只能说"没有可压缩内容"，provider 不可用也
    /// 被说成"历史已经够短"。
    #[test]
    fn compact_refusals_are_specific_chinese_reasons() {
        let cases = [
            super::CompactRefusal::EmptyHistory,
            super::CompactRefusal::NothingEligible,
            super::CompactRefusal::ProviderUnavailable("缺少 api_key".into()),
        ];
        let mut codes = Vec::new();
        let mut reasons = Vec::new();
        for case in &cases {
            let reason = case.reason();
            assert!(
                reason.contains("压缩") || reason.contains("没有"),
                "原因必须说明压缩为何没发生：{reason}"
            );
            assert!(
                reason
                    .chars()
                    .any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)),
                "原因必须是中文：{reason}"
            );
            codes.push(case.code().to_string());
            reasons.push(reason);
        }
        codes.sort();
        codes.dedup();
        assert_eq!(codes.len(), 3, "每种原因的 code 必须唯一");
        reasons.sort();
        reasons.dedup();
        assert_eq!(reasons.len(), 3, "每种原因的说明必须可区分");
    }
}
