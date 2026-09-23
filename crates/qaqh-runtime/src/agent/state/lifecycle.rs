//! Session lifecycle: initialization, health status.

use super::agent::AgentState;
use qaqh_workspace;

/// cwd 宿主注入（PR-3-3 / D3）：宿主侧经注入句柄解析会话工作目录后注入
/// workspace（`set_process_workspace`），workspace 侧不再直读 qaqh_session。
/// 解析权威：meta.cwd（旧 workspace.txt 惰性迁移见
/// `SessionManager::workspace_cwd`）；无句柄（单测）或解析为空时落 "."，
/// 与旧 workspace 侧 `load_session_workspace` 行为一致。
pub(crate) fn load_session_workspace(agent: &AgentState) {
    let cwd = agent
        .session_manager
        .as_ref()
        .and_then(|sm| sm.workspace_cwd(&agent.session.seed))
        .unwrap_or_default();
    qaqh_workspace::workspace::set_process_workspace(if cwd.is_empty() { "." } else { &cwd });
}

/// L2：为真实会话的 MessageStore 启用 enqueue 级 WAL。临时（子代理）store
/// 与单测 store 不启用；`enable_wal` 自身也对重复启用幂等。
fn enable_message_wal(agent: &mut AgentState) {
    if agent.ephemeral {
        return;
    }
    let session_dir = qaqh_types::platform::sessions_dir().join(&agent.session.seed);
    agent.msg.enable_wal(&session_dir);
}

/// Run the canonical recovery batch before any tool can be dispatched.
///
/// Legacy sessions without a canonical identity/events log are left untouched.
/// Once a canonical log exists, recovery is fail-closed: a failure is logged
/// and the later ToolRuntime admission will still reject an open intent.
fn recover_canonical_tool_ledger(seed: &str) -> Result<(), String> {
    let session_dir = qaqh_types::platform::sessions_dir().join(seed);
    recover_canonical_tool_ledger_in(&session_dir, seed)
}

fn recover_canonical_tool_ledger_in(
    session_dir: &std::path::Path,
    seed: &str,
) -> Result<(), String> {
    use qaqh_session::canonical::{
        CANONICAL_IDENTITY_FILE, CanonicalSessionIdentity, CommittedFactReader, EVENTS_FILE,
        RecoveryExecutionOutcome, WriterId, execute_recovery_intent, generate_ulid,
        load_recovery_intent, persist_recovery_intent, plan_recovery_intent, sha256_content_hash,
    };
    use qaqh_session::session_fact_v2::{EventId, RecoveryId};

    if !session_dir.join(CANONICAL_IDENTITY_FILE).exists()
        && !session_dir.join(EVENTS_FILE).exists()
    {
        return Ok(());
    }

    let identity =
        CanonicalSessionIdentity::open_or_create(session_dir).map_err(|error| error.to_string())?;
    let facts = CommittedFactReader::open(
        session_dir,
        identity.session_id.clone(),
        identity.log_id.clone(),
    )
    .map_err(|error| error.to_string())?
    .read_all()
    .map_err(|error| error.to_string())?;

    match crate::agent::tool_outbox::reconcile_canonical_ledger(session_dir) {
        Ok(reconciliation) if !reconciliation.is_consistent() => {
            log::warn!(
                "[recovery] tool_outbox/canonical ledger drift for {seed}: missing_ledger={}, missing_outbox={}, status_mismatches={}",
                reconciliation.missing_ledger.len(),
                reconciliation.missing_outbox.len(),
                reconciliation.status_mismatches.len()
            );
        }
        Ok(_) => {}
        Err(error) => {
            log::error!(
                "[recovery] tool_outbox/canonical reconciliation failed for {seed}: {error}"
            );
        }
    }

    let existing = load_recovery_intent(session_dir).map_err(|error| error.to_string())?;
    if existing.is_none() {
        let Some(intent) = plan_recovery_intent(
            session_dir,
            identity.session_id.clone(),
            identity.log_id.clone(),
            RecoveryId::new(format!("recovery_{}", generate_ulid())),
            EventId::new(generate_ulid()),
            sha256_content_hash(b"[]"),
        )
        .map_err(|error| error.to_string())?
        else {
            return Ok(());
        };
        persist_recovery_intent(session_dir, &intent, &facts).map_err(|error| error.to_string())?;
    }

    let outcome = execute_recovery_intent(
        session_dir,
        identity.session_id,
        identity.log_id,
        WriterId::new(format!("recovery-{}-{}", std::process::id(), seed)),
        super::agent::unix_ms(),
        super::agent::tool_ledger_lease_ms(),
    )
    .map_err(|error| error.to_string())?;
    match outcome {
        RecoveryExecutionOutcome::Recovered(execution) => {
            log::info!(
                "[recovery] canonical tool ledger recovered for {seed}: {} action(s), intent_removed={}",
                execution.actions.len(),
                execution.intent_removed
            );
        }
        RecoveryExecutionOutcome::Pending { dispositions } => {
            log::warn!(
                "[recovery] canonical tool ledger for {seed} still has {} replay/reconcile disposition(s)",
                dispositions.len()
            );
        }
        RecoveryExecutionOutcome::NoIntent => {}
    }
    Ok(())
}

/// Load session from disk via the injected session-manager handle.
///
/// On success, restores the message store and rebinds the workspace.
/// On failure (file missing or corrupt), generates a fresh seed and
/// creates a new session as fallback. Returns `false` only when
/// `restore_seed` is `None`.
pub fn init_session(agent: &mut AgentState, restore_seed: Option<&str>) -> bool {
    let seed = match restore_seed {
        Some(s) => {
            log::info!("[LIFECYCLE] init_session: loading seed={s}");
            // Fast check: if the session directory doesn't exist at all, fail early
            // instead of silently creating a new session. This lets the caller
            // send a proper Error event rather than a confusing SessionCreated.
            if !agent
                .session_manager
                .as_ref()
                .is_some_and(|sm| sm.exists(s))
            {
                log::error!(
                    "qaqh-agent: session {} not found — directory does not exist",
                    s
                );
                return false;
            }
            if let Some((meta, archive_messages, compact_context)) = agent
                .session_manager
                .as_ref()
                .and_then(|sm| sm.load_for_resume(s))
            {
                let active_messages = compact_context
                    .as_ref()
                    .map(|context| context.messages.as_slice())
                    .unwrap_or(archive_messages.as_slice());
                log::info!(
                    "[LIFECYCLE] loaded session, {} archived messages, {} active messages",
                    archive_messages.len(),
                    active_messages.len()
                );
                agent.session = meta;
                agent.session.from_resume = true;
                agent.session.tokens = agent.session.usage_totals.total_tokens.into();
                // 如果有 compact 上下文，compact_skip 必须为 0——压缩后的消息
                // 已经是去除了旧 turn 的活跃视图，不需要再跳过任何 turn。
                let effective_compact_skip = if compact_context.is_some() {
                    0
                } else {
                    agent.session.compact_skip
                };
                let (msg, repairs) = qaqh_message::MessageStore::from_messages(
                    &agent.session.seed,
                    active_messages,
                    effective_compact_skip,
                );
                let archive_next_id = archive_messages
                    .iter()
                    .filter_map(|message| message.msg_id)
                    .max()
                    .unwrap_or(0)
                    .saturating_add(1);
                let mut msg = msg;
                msg.set_compact_context_active(compact_context.is_some());
                msg.ensure_next_msg_id(archive_next_id);
                // Turn IDs belong to the immutable archive, not the compacted
                // active view. Otherwise compacting 30 turns down to 6 would
                // make the next live turn reuse t7 and be ignored by clients.
                //
                // The turn allocator must never collide with turns the daemon
                // timeline has already recorded for this seed. `meta.turn_count`
                // is only persisted when a turn completes, so a daemon restart
                // while a turn is still running leaves it lagging: the next
                // user input would then reuse the interrupted turn's id, and
                // every timeline intent for that turn would be rejected by the
                // daemon (DuplicateTurn) — the frontend transcript goes blank
                // while the session list title still refreshes (the assistant
                // reply is persisted through the message store independently).
                //
                // The authoritative count is the message store's actual turn
                // count: `from_messages` replays every user message, including
                // the unfinished turn. `meta.turn_count` additionally covers the
                // compacted-history case where early turns were folded out of
                // the active view, so the next id must be greater than both.
                //
                // The daemon additionally injects the timeline's recorded turn
                // count (QAQH_TIMELINE_TURN_COUNT) when spawning a resume
                // worker: meta.turn_count only persists on completion, so after
                // a restart it can lag the timeline's sealed turns by more than
                // one (compaction shrinks the message view too). Without this
                // floor the allocator reuses an id the timeline already sealed
                // as Completed, and every timeline intent for the resumed turn
                // is rejected — the frontend transcript stays blank while the
                // session list title still refreshes.
                let restored_turns = msg.turn_count() as u64;
                // Timeline floor is carried per-agent (daemon injects it via
                // `AgentState.timeline_turn_count`), not a process env var, so
                // concurrent in-process actors each see their own value.
                let timeline_turns = agent.timeline_turn_count;
                let authority_turn_count = restored_turns
                    .max(agent.session.turn_count as u64)
                    .max(timeline_turns);
                msg.ensure_next_turn_seq(authority_turn_count + 1);
                // Keep the persisted metadata in sync with the authoritative
                // replay so a later flush does not write a stale count back.
                agent.session.turn_count = authority_turn_count as usize;
                // B（Tier C）：逻辑压缩前缀在内存里零依赖——分配器基线（上面
                // 已用「全量 restored 计数」校准，不受驱逐影响）固化后即可
                // 从常驻内存驱逐。驱逐会把持久化模式物理化为 compact
                // checkpoint（归档不再被 SaveFull 整写），崩溃安全分析见
                // MessageStore::evict_compacted_prefix。
                let evicted = agent.msg.evict_compacted_prefix();
                if evicted > 0 {
                    log::info!(
                        "[LIFECYCLE] evicted {evicted} compacted prefix turns from memory (Tier C)"
                    );
                }
                log::info!(
                    "[LIFECYCLE] from_messages done, {} turns, {} repairs",
                    msg.turn_count(),
                    repairs.len()
                );
                agent.msg = msg;
                // L2：为真实会话启用 enqueue 级 WAL（恢复路径的 load_for_resume
                // 已在创建 store 前重放并截断旧 WAL）。临时/子代理 store 不启用。
                enable_message_wal(agent);
                // L3：工具 outbox 对账——已执行但结果丢失的工具，修正 [RESTORE]
                // 占位符语义（"未执行" → "已执行、结果未持久化"）。
                //
                // 对账前先把上一轮 worker 的批量化 fsync 落定（BUG-2026-09-12-14
                // 之后 fsync 交由后台 flusher 合并）：worker 退出路径已 flush，但
                // 进程在同一会话重开 worker 时，读到未 fsync 的记录仍可能来自
                // OS 页缓存；显式 flush 把「恢复读到的事实」钉在盘上。
                crate::agent::tool_outbox::flush(&agent.session.seed);
                crate::agent::tool_outbox::reconcile_store(&mut agent.msg, &agent.session.seed);
                // 重建 read_image 图片注册表：registry 是内存态，daemon 重启
                // 后会丢失；但上传图片本就以 ContentBlock::Image 持久化在
                // user 消息里。按活跃视图的时序重放注册，使 [Image #N] 占位
                // 引用（gate 投影按同一视图顺序编号）在重启后依然成立。
                qaqh_workspace::read_image::reset_images(&agent.session.seed);
                for message in active_messages {
                    if message.role != "user" {
                        continue;
                    }
                    for block in &message.content {
                        match block {
                            // 旧会话 inline Image：借重建时机外置落盘（内容寻址幂等）。
                            qaqh_types::ContentBlock::Image { mime_type, data } => {
                                qaqh_workspace::read_image::store_image(
                                    &agent.session.seed,
                                    mime_type,
                                    data,
                                );
                            }
                            // 新写入一律 ImageRef：直接按引用登记，无需字节。
                            qaqh_types::ContentBlock::ImageRef {
                                sha256, mime_type, ..
                            } => {
                                qaqh_workspace::read_image::register_image_ref(
                                    &agent.session.seed,
                                    mime_type,
                                    sha256,
                                );
                            }
                            _ => {}
                        }
                    }
                }
                // V2 state is restored only from typed session metadata. Old
                // protected skill/catalog system messages must not reactivate
                // instructions by surviving in message history.
                agent
                    .msg
                    .remove_system_messages_by_prefix(qaqh_skills::ACTIVATION_MARKER);
                agent
                    .msg
                    .remove_system_messages_by_prefix("Available skills");

                qaqh_workspace::workspace::set_current_session(&agent.session.seed);
                load_session_workspace(agent);
                // BUG-2026-09-12-05：读 TLS 优先的 current_workspace 而非进程
                // 全局——多 actor daemon 中全局恒空，skills 工作区会锚到
                // daemon 进程 cwd。
                let workspace = qaqh_workspace::current_workspace();
                agent.skills.set_workspace(std::path::Path::new(&workspace));
                agent.skills.restore(&agent.session.skills.clone());
                // P0 cache fix: reuse the persisted frozen [Environment]
                // annotation so the resumed context is byte-identical to the
                // pre-restart prefix (new <today> date / empty file_state
                // ledger must NOT be regenerated). None (legacy meta or first
                // build_context never ran) → regenerate as before.
                agent.restore_frozen_annotation(agent.session.frozen_annotation.clone());
                // P2 fix: align the injection watermark with the restored
                // epoch, otherwise the first sync_skill_injection after resume
                // re-appends a duplicate envelope (epoch > 0 but watermark 0).
                agent.align_skill_injection_watermark();
                // Hot-load latest tool schema (order-stable: new tools appended at end)
                agent.tool_defs = qaqh_workspace::runtime::all_tools();
                // 应用持久化的工具模式（standard/minimal/custom，幂等）。
                // 在 tool_defs 刷新之后执行：apply_tool_mode 内部再刷一次（带 allowed 过滤）。
                let tool_mode = agent.session.tool_mode.clone();
                let custom_tools = agent.session.custom_tools.clone();
                agent.apply_tool_mode(&tool_mode, &custom_tools);
                if let Err(error) = recover_canonical_tool_ledger(&agent.session.seed) {
                    log::error!(
                        "[recovery] canonical tool ledger recovery failed for {}: {error}",
                        agent.session.seed
                    );
                }
                log::info!(
                    "qaqh-agent: restored session {} ({} msgs, {} tokens)",
                    agent.session.seed,
                    agent.msg.message_count(),
                    agent.session.tokens
                );
                if !repairs.is_empty() {
                    log::warn!("session restore: {:?} repairs", repairs);
                }
                return true;
            }
            // Directory exists but meta or messages are corrupt — generate a
            // fresh seed so we don't overwrite the corrupted files.
            //
            // BUG-2026-09-13-24：这条回退路径也**必须**做碰撞检查。旧实现
            // 直接 `generate_seed()`：若新 seed 撞上另一条既有会话目录，
            // 后续 flush 会写穿它——正是我们要避免的静默数据损坏。优先经
            // 注入的 session manager 取唯一 seed（临时/无 manager 时退回
            // 无管理器版本，此时 store 也是 ephemeral，不落盘）。
            log::error!(
                "qaqh-agent: session {} load failed (corrupt?) — creating fresh session",
                s
            );
            log::warn!("[LIFECYCLE] load failed for {s}, generating new seed");
            match agent.session_manager.as_ref() {
                Some(manager) => manager.generate_unique_session_seed(),
                None => qaqh_session::generate_unique_seed(|seed| {
                    qaqh_types::platform::sessions_dir().join(seed).exists()
                }),
            }
        }
        None => return false,
    };

    // Create fresh session (either no restore_seed, or restore failed)
    agent.session.seed = seed.clone();
    agent.session.created_at = qaqh_session::now_epoch();
    agent.session.reset_usage();
    agent.session.from_resume = false;
    agent.msg = if agent.ephemeral {
        qaqh_message::MessageStore::new_ephemeral(&seed)
    } else {
        qaqh_message::MessageStore::new(&seed)
    };
    enable_message_wal(agent);
    qaqh_workspace::workspace::set_current_session(&agent.session.seed);
    load_session_workspace(agent);
    let workspace = qaqh_workspace::CURRENT_WORKSPACE
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .clone();
    agent.skills = qaqh_skills::SkillContextManager::new(
        std::path::Path::new(&workspace),
        agent.config.context_limit as usize,
    );
    agent.msg.push_system(qaqh_types::Message::system(
        &crate::agent::prompt::system_prompt_for_mode(&agent.session.tool_mode),
    ));
    agent
        .msg
        .flush_meta(&agent.config.model, &agent.config.reasoning_effort);
    log::info!("qaqh-agent: new session {}", agent.session.seed);
    true
}

/// Create a brand-new session with a fresh seed, clearing all prior state.
pub fn create_session(agent: &mut AgentState) {
    agent.session.seed = qaqh_session::generate_seed();
    agent.session.created_at = qaqh_session::now_epoch();
    agent.session.reset_usage();
    agent.session.from_resume = false;
    agent.msg = if agent.ephemeral {
        qaqh_message::MessageStore::new_ephemeral(&agent.session.seed)
    } else {
        qaqh_message::MessageStore::new(&agent.session.seed)
    };
    enable_message_wal(agent);
    qaqh_workspace::workspace::set_current_session(&agent.session.seed);
    load_session_workspace(agent);
    // BUG-2026-09-12-05：同 resume/new 路径，改读 TLS 优先快照。
    let workspace = qaqh_workspace::current_workspace();
    agent.skills = qaqh_skills::SkillContextManager::new(
        std::path::Path::new(&workspace),
        agent.config.context_limit as usize,
    );
    agent.msg.push_system(qaqh_types::Message::system(
        &crate::agent::prompt::system_prompt_for_mode(&agent.session.tool_mode),
    ));
    agent
        .msg
        .flush_meta(&agent.config.model, &agent.config.reasoning_effort);
    log::info!("qaqh-agent: new session {}", agent.session.seed);
}

/// Create a new session with a pre-set seed (from CLI --seed).
/// Unlike create_session, this does NOT generate a new seed.
pub fn create_session_with_seed(agent: &mut AgentState) {
    agent.session.reset_usage();
    agent.session.from_resume = false;
    agent.msg = if agent.ephemeral {
        qaqh_message::MessageStore::new_ephemeral(&agent.session.seed)
    } else {
        qaqh_message::MessageStore::new(&agent.session.seed)
    };
    enable_message_wal(agent);
    qaqh_workspace::workspace::set_current_session(&agent.session.seed);
    load_session_workspace(agent);
    // BUG-2026-09-12-05：同 resume/new 路径，改读 TLS 优先快照。
    let workspace = qaqh_workspace::current_workspace();
    agent.skills = qaqh_skills::SkillContextManager::new(
        std::path::Path::new(&workspace),
        agent.config.context_limit as usize,
    );
    // 应用持久化的工具模式（PLAN-TOOL-MODES.md 4.3/4.4）：preset-seed 新建路径
    // 此前不读 meta.json，导致会话级 tool_mode 丢失、工具回退全量。
    // 这里与 resume 路径（init_session L120-134）对齐，从 meta 恢复并 apply。
    // 提前到 push_system 之前，让系统提示能读到 tool_mode（minimal:dsh → 极简 prompt）。
    if let Some(meta) = agent
        .session_manager
        .as_ref()
        .and_then(|sm| sm.load_meta(&agent.session.seed))
        && !meta.tool_mode.is_empty()
    {
        agent.apply_tool_mode(&meta.tool_mode, &meta.custom_tools);
    }
    agent.msg.push_system(qaqh_types::Message::system(
        &crate::agent::prompt::system_prompt_for_mode(&agent.session.tool_mode),
    ));
    agent
        .msg
        .flush_meta(&agent.config.model, &agent.config.reasoning_effort);
    log::info!(
        "qaqh-agent: new session with preset seed {}",
        agent.session.seed
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_session::canonical::{
        CanonicalSessionIdentity, CommittedFactReader, ToolLedger, WriterId, generate_ulid,
        load_recovery_intent, sha256_content_hash,
    };
    use qaqh_session::session_fact_v2::{
        EventId, ExecutionId, FactPayload, PolicyDecisionRef, SideEffectClass, ToolCallId,
        ToolIntent, ToolIntentPolicyOutcome, ToolReplayCapability, ToolTerminalStatus,
    };
    use std::time::Duration;

    #[test]
    fn canonical_recovery_seals_open_intent_before_session_resume() {
        let dir = tempfile::tempdir().expect("tempdir");
        let identity = CanonicalSessionIdentity::open_or_create(dir.path()).expect("identity");
        let now = super::super::agent::unix_ms();
        let mut ledger = ToolLedger::open(
            dir.path(),
            identity.session_id.clone(),
            identity.log_id.clone(),
            WriterId::new("lifecycle-recovery-setup"),
            now,
            1,
        )
        .expect("open setup ledger");
        let call = ToolCallId::new(format!("call_{}", generate_ulid()));
        let execution = ExecutionId::new(format!("exec_{}", generate_ulid()));
        ledger
            .append_intent(
                EventId::new(generate_ulid()),
                None,
                ToolIntent {
                    call_id: call.clone(),
                    execution_id: execution,
                    idempotency_key: None,
                    replay_capability: ToolReplayCapability::NoReplay,
                    policy_decision: PolicyDecisionRef {
                        outcome: ToolIntentPolicyOutcome::Allow,
                        rule_id: "test".into(),
                        decided_at_ms: now,
                        reason_ref: None,
                    },
                    effective_args_ref: None,
                    effective_args_hash: None,
                    sandbox_spec_hash: sha256_content_hash(b"sandbox"),
                    side_effect_class: SideEffectClass::ReadOnly,
                    intent_at_ms: now,
                },
                now,
            )
            .expect("append open intent");
        drop(ledger);
        std::thread::sleep(Duration::from_millis(2));

        recover_canonical_tool_ledger_in(dir.path(), "lifecycle-recovery-test")
            .expect("recover canonical ledger");

        assert!(
            load_recovery_intent(dir.path())
                .expect("load intent")
                .is_none()
        );
        let facts = CommittedFactReader::open(dir.path(), identity.session_id, identity.log_id)
            .expect("reader")
            .read_all()
            .expect("facts");
        let finished = facts
            .iter()
            .filter_map(|fact| match &fact.payload {
                FactPayload::ToolFinished(finished) => Some(finished),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(finished.len(), 1);
        assert_eq!(finished[0].call_id, call);
        assert_eq!(
            finished[0].terminal_status,
            ToolTerminalStatus::Indeterminate
        );
        assert_eq!(
            facts
                .iter()
                .filter(|fact| matches!(fact.payload, FactPayload::SessionRecovered(_)))
                .count(),
            1
        );
    }
}
