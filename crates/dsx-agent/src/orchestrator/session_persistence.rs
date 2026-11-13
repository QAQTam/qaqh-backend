//! Session persistence: save/load conversations, repair alternation.
use crate::session;
use crate::agent::AgentState;

/// Save session to disk if there are messages and a seed.
pub fn maybe_save_session(state: &mut AgentState) {
    let msgs = state.ctx.to_vec();
    if msgs.len() > 1 && !state.session_seed.is_empty() {
        session::finalize_session(
            &state.session_seed,
            &msgs,
            &state.config.model,
            state.config.effort.as_deref(),
        );
    }
}
