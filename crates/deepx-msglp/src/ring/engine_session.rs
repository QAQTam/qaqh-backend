//! SessionEngine: session lifecycle management.
//!
//! Handles: create, resume, reload_config.
//! Delegates to: lifecycle.rs for core session operations.

use super::types::*;
use crate::state::lifecycle;
use crate::util;

/// Number of recent turns sent on session restore.
const INITIAL_LOAD_COUNT: usize = 20;

pub struct SessionEngine;

impl SessionEngine {
    pub fn new() -> Self {
        Self
    }

    /// Create a new session with a fresh seed.
    pub fn create(&self, agent: &mut crate::state::agent::AgentState, _cancel: &CancelToken) {
        lifecycle::create_session(agent);
        agent.rebind_store();
        deepx_tools::runtime::set_context(&agent.session.seed, agent.config.permission_level);
    }

    /// Create a new session with a pre-set seed (from CLI --seed).
    pub fn create_with_seed(&self, agent: &mut crate::state::agent::AgentState, _cancel: &CancelToken) {
        lifecycle::create_session_with_seed(agent);
        agent.rebind_store();
        deepx_tools::runtime::set_context(&agent.session.seed, agent.config.permission_level);
    }

    /// Resume an existing session. Returns false if the session doesn't exist.
    pub fn resume(
        &self,
        agent: &mut crate::state::agent::AgentState,
        seed: &str,
        _cancel: &CancelToken,
    ) -> bool {
        log::info!("[SESSION] resume seed={seed}");
        if lifecycle::init_session(agent, Some(seed)) {
            agent.rebind_store();
            deepx_tools::runtime::set_context(&agent.session.seed, agent.config.permission_level);

            // Restore persisted agent mode
            let saved_mode = agent.session.mode;
            if saved_mode != 0 {
                deepx_tools::runtime::set_mode(saved_mode);
            }

            // Build initial turn batch and emit
            let total = agent.msg.turn_count() as u32;
            let start = total.saturating_sub(INITIAL_LOAD_COUNT as u32) as usize;
            let recent =
                util::build_turns_from_context(agent, Some(start), Some(INITIAL_LOAD_COUNT));
            let has_more = start > 0;

            // SessionRestored is emitted by the caller (Loop::dispatch)
            // since it needs access to the emitter.
            log::info!(
                "[SESSION] restored, {} turns (has_more={})",
                recent.len(),
                has_more
            );
            true
        } else {
            log::info!("[SESSION] init_session returned false for {seed}");
            false
        }
    }

    /// Reload config from disk and apply to agent.
    pub fn reload_config(&self, agent: &mut crate::state::agent::AgentState, _cancel: &CancelToken) {
        if let Ok(cfg) = deepx_config::Config::load() {
            agent.config.api_key = cfg.api_key;
            agent.config.model = cfg.model;
            agent.config.base_url = cfg.base_url;
            agent.config.endpoint = cfg.endpoint;
            agent.config.provider_id = cfg.provider_id;
            agent.config.reasoning_effort = cfg.reasoning_effort;
            agent.config.max_tokens = cfg.max_tokens;
            agent.config.context_limit = cfg.context_limit;
            agent.config.permission_level = cfg.permission_level;
            agent.config.permission_level = cfg.permission_level;
            deepx_tools::workspace::load_session_workspace(&agent.session.seed);
            deepx_session::SessionManager::global().set_turso_enabled(cfg.database.enabled);
        }
    }
}
