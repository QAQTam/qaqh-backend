//! Session persistence: save/load conversations, repair alternation.
use crate::conversation::ContextAssembler;
use crate::session;
use crate::tui::app::App;

/// Repair messages by re-importing through ContextAssembler.
pub fn repair_messages(app: &mut App) {
    let msgs = std::mem::take(&mut app.messages);
    let (new_ctx, repairs) = ContextAssembler::from_legacy(msgs);
    app.ctx = new_ctx;
    app.sync_messages();
    if !repairs.is_empty() {
        log::warn!("repair: {}", repairs.join("; "));
        app.status_msg = format!("Repaired: {} issue(s).", repairs.len());
    }
}

/// Save session to disk if there are messages and a seed.
pub fn maybe_save_session(app: &mut App) {
    repair_messages(app);
    let msgs = app.ctx.to_vec();
    if msgs.len() > 1 && !app.session_seed.is_empty() {
        session::finalize_session(
            &app.session_seed,
            &msgs,
            &app.config.model,
            app.config.effort.as_deref(),
            None,
        );
    }
}
