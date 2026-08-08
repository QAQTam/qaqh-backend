//! Session lifecycle: initialization, health status.

use super::agent::AgentState;
use crate::util::chrono_local_date;
use deepx_session::SessionManager;
use deepx_workspace;

/// Load session from disk via [`SessionManager`].
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
            if !SessionManager::global().exists(s) {
                log::error!(
                    "deepx-agent: session {} not found — directory does not exist",
                    s
                );
                return false;
            }
            if let Some((meta, archive_messages, compact_context)) =
                SessionManager::global().load_for_resume(s)
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
                let (msg, repairs) = deepx_message::MessageStore::from_messages(
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
                // count (DEEPX_TIMELINE_TURN_COUNT) when spawning a resume
                // worker: meta.turn_count only persists on completion, so after
                // a restart it can lag the timeline's sealed turns by more than
                // one (compaction shrinks the message view too). Without this
                // floor the allocator reuses an id the timeline already sealed
                // as Completed, and every timeline intent for the resumed turn
                // is rejected — the frontend transcript stays blank while the
                // session list title still refreshes.
                let restored_turns = msg.turn_count() as u64;
                let timeline_turns = std::env::var("DEEPX_TIMELINE_TURN_COUNT")
                    .ok()
                    .and_then(|value| value.parse::<u64>().ok())
                    .unwrap_or(0);
                let authority_turn_count = restored_turns
                    .max(agent.session.turn_count as u64)
                    .max(timeline_turns);
                msg.ensure_next_turn_seq(authority_turn_count + 1);
                // Keep the persisted metadata in sync with the authoritative
                // replay so a later flush does not write a stale count back.
                agent.session.turn_count = authority_turn_count as usize;
                log::info!(
                    "[LIFECYCLE] from_messages done, {} turns, {} repairs",
                    msg.turn_count(),
                    repairs.len()
                );
                agent.msg = msg;
                // V2 state is restored only from typed session metadata. Old
                // protected skill/catalog system messages must not reactivate
                // instructions by surviving in message history.
                agent
                    .msg
                    .remove_system_messages_by_prefix(deepx_skills::ACTIVATION_MARKER);
                agent
                    .msg
                    .remove_system_messages_by_prefix("Available skills");

                deepx_workspace::workspace::set_current_session(&agent.session.seed);
                deepx_workspace::workspace::load_session_workspace(&agent.session.seed);
                let workspace = deepx_workspace::CURRENT_WORKSPACE
                    .read()
                    .unwrap_or_else(|error| error.into_inner())
                    .clone();
                agent.skills.set_workspace(std::path::Path::new(&workspace));
                agent.skills.restore(&agent.session.skills.clone());
                // Hot-load latest tool schema (order-stable: new tools appended at end)
                agent.tool_defs = deepx_workspace::runtime::all_tools();
                log::info!(
                    "deepx-agent: restored session {} ({} msgs, {} tokens)",
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
            log::error!(
                "deepx-agent: session {} load failed (corrupt?) — creating fresh session",
                s
            );
            log::warn!("[LIFECYCLE] load failed for {s}, generating new seed");
            SessionManager::generate_seed()
        }
        None => return false,
    };

    // Create fresh session (either no restore_seed, or restore failed)
    agent.session.seed = seed.clone();
    agent.session.created_at = SessionManager::now_epoch();
    agent.session.reset_usage();
    agent.session.from_resume = false;
    agent.msg = if agent.ephemeral {
        deepx_message::MessageStore::new_ephemeral(&seed)
    } else {
        deepx_message::MessageStore::new(&seed)
    };
    deepx_workspace::workspace::set_current_session(&agent.session.seed);
    deepx_workspace::workspace::load_session_workspace(&agent.session.seed);
    let workspace = deepx_workspace::CURRENT_WORKSPACE
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .clone();
    agent.skills = super::skill_context::SkillContextManager::new(
        std::path::Path::new(&workspace),
        agent.config.context_limit as usize,
    );
    agent.msg.push_system(deepx_types::Message::system(
        &deepx_config::prompt::full_system_prompt_with_date(
            &chrono_local_date(),
            deepx_config::prompt::OS_INFO
                .get()
                .map(|s| s.as_str())
                .unwrap_or(""),
        ),
    ));
    // Freeze the skill catalog as a persistent system message so the
    // cache prefix stays stable across daemon restarts.
    let catalog = agent.skills.initial_catalog_text().to_string();
    if !catalog.is_empty() {
        agent
            .msg
            .push_system(deepx_types::Message::system(&catalog));
    }
    agent
        .msg
        .flush_meta(&agent.config.model, &agent.config.reasoning_effort);
    log::info!("deepx-agent: new session {}", agent.session.seed);
    true
}

/// Create a brand-new session with a fresh seed, clearing all prior state.
pub fn create_session(agent: &mut AgentState) {
    agent.session.seed = SessionManager::generate_seed();
    agent.session.created_at = SessionManager::now_epoch();
    agent.session.reset_usage();
    agent.session.from_resume = false;
    agent.msg = if agent.ephemeral {
        deepx_message::MessageStore::new_ephemeral(&agent.session.seed)
    } else {
        deepx_message::MessageStore::new(&agent.session.seed)
    };
    deepx_workspace::workspace::set_current_session(&agent.session.seed);
    deepx_workspace::workspace::load_session_workspace(&agent.session.seed);
    let workspace = deepx_workspace::CURRENT_WORKSPACE
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .clone();
    agent.skills = super::skill_context::SkillContextManager::new(
        std::path::Path::new(&workspace),
        agent.config.context_limit as usize,
    );
    agent.msg.push_system(deepx_types::Message::system(
        &deepx_config::prompt::full_system_prompt_with_date(
            &chrono_local_date(),
            deepx_config::prompt::OS_INFO
                .get()
                .map(|s| s.as_str())
                .unwrap_or(""),
        ),
    ));
    // Freeze the skill catalog as a persistent system message so the
    // cache prefix stays stable across daemon restarts.
    let catalog = agent.skills.initial_catalog_text().to_string();
    if !catalog.is_empty() {
        agent
            .msg
            .push_system(deepx_types::Message::system(&catalog));
    }
    agent
        .msg
        .flush_meta(&agent.config.model, &agent.config.reasoning_effort);
    log::info!("deepx-agent: new session {}", agent.session.seed);
}

/// Create a new session with a pre-set seed (from CLI --seed).
/// Unlike create_session, this does NOT generate a new seed.
pub fn create_session_with_seed(agent: &mut AgentState) {
    agent.session.reset_usage();
    agent.session.from_resume = false;
    agent.msg = if agent.ephemeral {
        deepx_message::MessageStore::new_ephemeral(&agent.session.seed)
    } else {
        deepx_message::MessageStore::new(&agent.session.seed)
    };
    deepx_workspace::workspace::set_current_session(&agent.session.seed);
    deepx_workspace::workspace::load_session_workspace(&agent.session.seed);
    let workspace = deepx_workspace::CURRENT_WORKSPACE
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .clone();
    agent.skills = super::skill_context::SkillContextManager::new(
        std::path::Path::new(&workspace),
        agent.config.context_limit as usize,
    );
    agent.msg.push_system(deepx_types::Message::system(
        &deepx_config::prompt::full_system_prompt_with_date(
            &chrono_local_date(),
            deepx_config::prompt::OS_INFO
                .get()
                .map(|s| s.as_str())
                .unwrap_or(""),
        ),
    ));
    // Freeze the skill catalog as a persistent system message so the
    // cache prefix stays stable across daemon restarts.
    let catalog = agent.skills.initial_catalog_text().to_string();
    if !catalog.is_empty() {
        agent
            .msg
            .push_system(deepx_types::Message::system(&catalog));
    }
    agent
        .msg
        .flush_meta(&agent.config.model, &agent.config.reasoning_effort);
    log::info!(
        "deepx-agent: new session with preset seed {}",
        agent.session.seed
    );
}
