//! Session persistence: save/load conversations, repair alternation.
use crate::conversation::ContextAssembler;
use crate::session;
use crate::agent::AgentState;

/// Repair messages by re-importing through ContextAssembler.
pub fn repair_messages(state: &mut AgentState) {
    let msgs = std::mem::take(&mut state.messages);
    let (new_ctx, repairs) = ContextAssembler::from_legacy(msgs);
    state.ctx = new_ctx;
    state.sync_messages();
    if !repairs.is_empty() {
        log::warn!("repair: {}", repairs.join("; "));
    }
}

/// Save session to disk if there are messages and a seed.
pub fn maybe_save_session(state: &mut AgentState) {
    repair_messages(state);
    let msgs = state.ctx.to_vec();
    if msgs.len() > 1 && !state.session_seed.is_empty() {
        session::finalize_session(
            &state.session_seed,
            &msgs,
            &state.config.model,
            state.config.effort.as_deref(),
            None,
        );
    }
}
