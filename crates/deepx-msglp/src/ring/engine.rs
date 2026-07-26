//! Engine trait — the extension interface for the Ring.
//!
//! Each Engine implements this trait. The Loop dispatches commands
//! by iterating engines in order; the first engine that returns
//! Some(outcome) handles the command.
//!
//! To add a new feature:
//! 1. Create a new struct implementing `Engine`
//! 2. Add `Box::new(YourEngine::new())` to the engines vec in Loop::new_ipc()
//! 3. Done — no changes to Loop::dispatch() needed

use super::types::{Outcome, RingContext};
use deepx_proto::Ui2Agent;

/// An Engine processes Ui2Agent commands within a RingContext.
///
/// # Try-handle pattern
///
/// `try_handle()` returns:
/// - `Some(Outcome)` — this engine handled the command
/// - `None` — this engine doesn't handle this variant, pass to next
///
/// # Panic safety
///
/// Engines are called inside `catch_unwind`. If an engine panics,
/// the Loop calls `reset()` to restore clean state, logs the panic,
/// and continues processing.
pub trait Engine {
    /// Attempt to handle a command. Returns None if not applicable.
    fn try_handle(&mut self, ctx: &mut RingContext, cmd: &Ui2Agent) -> Option<Outcome>;

    /// Reset internal state after a panic or Cancel.
    /// Must leave the engine in a valid idle state — no leaked
    /// suspended turns, no stale pending approvals.
    fn reset(&mut self);
}

// ═══════════════════════════════════════════════════════
// Engine implementations sketch
// ═══════════════════════════════════════════════════════

impl Engine for super::engine_turn::TurnEngine {
    fn try_handle(&mut self, _ctx: &mut RingContext, _cmd: &Ui2Agent) -> Option<Outcome> {
        // TurnEngine doesn't handle commands directly — it's driven by
        // InputEngine (which returns ContinueTurn → Loop calls turn.run()).
        None
    }

    fn reset(&mut self) {
        self.suspended = None;
    }
}

impl Engine for super::engine_tool::ToolEngine {
    fn try_handle(&mut self, ctx: &mut RingContext, cmd: &Ui2Agent) -> Option<Outcome> {
        match cmd {
            Ui2Agent::ToolCall {
                id,
                name,
                action,
                args,
            } => {
                self.handle_ui_tool_call(ctx, id, name, action, args);
                Some(Outcome::Handled)
            }
            Ui2Agent::PermissionResponse {
                tool_call_id,
                approved,
                trust_folder,
            } => {
                let _ =
                    self.handle_permission_response(ctx, tool_call_id, *approved, *trust_folder);
                Some(Outcome::Handled)
            }
            _ => None,
        }
    }

    fn reset(&mut self) {
        self.clear_pending();
    }
}

impl Engine for super::engine_session::SessionEngine {
    fn try_handle(&mut self, ctx: &mut RingContext, cmd: &Ui2Agent) -> Option<Outcome> {
        match cmd {
            Ui2Agent::CreateSession => {
                self.create(ctx.agent, ctx.cancel);
                Some(Outcome::Handled)
            }
            Ui2Agent::ResumeSession { seed } => {
                self.resume(ctx.agent, seed, ctx.cancel);
                Some(Outcome::Handled)
            }
            Ui2Agent::NewSession => {
                self.create(ctx.agent, ctx.cancel);
                Some(Outcome::Handled)
            }
            Ui2Agent::ReloadConfig => {
                self.reload_config(ctx.agent, ctx.cancel);
                Some(Outcome::Handled)
            }
            _ => None,
        }
    }

    fn reset(&mut self) {
        // SessionEngine has no mutable state to reset
    }
}

impl Engine for super::engine_input::InputEngine {
    fn try_handle(&mut self, ctx: &mut RingContext, cmd: &Ui2Agent) -> Option<Outcome> {
        match cmd {
            Ui2Agent::UserInput { text } => Some(self.handle_user_input(ctx, text)),
            _ => None,
        }
    }

    fn reset(&mut self) {
        // InputEngine has no mutable state
    }
}

impl Engine for super::engine_compact::CompactEngine {
    fn try_handle(&mut self, _ctx: &mut RingContext, _cmd: &Ui2Agent) -> Option<Outcome> {
        // Compact is dispatched directly by Loop (two-step async flow).
        // The Engine trait is not used for this command.
        None
    }

    fn reset(&mut self) {
        // CompactEngine has no mutable state
    }
}

impl Engine for super::engine_misc::MiscEngine {
    fn try_handle(&mut self, _ctx: &mut RingContext, _cmd: &Ui2Agent) -> Option<Outcome> {
        // MiscEngine handles commands that need direct event_tx access
        // (Undo, SetMode, LoadMoreTurns). These are dispatched directly
        // in Loop::dispatch_one()'s fallback section.
        None
    }

    fn reset(&mut self) {
        // MiscEngine has no mutable state
    }
}
