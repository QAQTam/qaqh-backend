//! Explicit runtime and turn context.
//!
//! P2-4a introduces this boundary before moving compaction, title, liveness,
//! and session lifecycle out of the loop. The legacy adapter captures the
//! current actor/thread-local values once at the turn boundary; ownership and
//! execution remain on the existing path for this slice.

use std::path::{Path, PathBuf};

use super::types::{CancelToken, RingContext};

/// Sandbox mode visible to one runtime context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxKind {
    /// Main session actor.
    Main,
    /// Subagent actor with no user approval channel.
    Subagent,
}

impl SandboxKind {
    /// Capture the current actor thread's legacy sandbox flag.
    pub fn from_legacy() -> Self {
        if qaqh_workspace::authorization::is_subagent_sandbox() {
            Self::Subagent
        } else {
            Self::Main
        }
    }

    /// Whether this context is running under subagent sandbox rules.
    pub fn is_subagent(self) -> bool {
        self == Self::Subagent
    }
}

/// Explicit runtime state shared by the work belonging to one actor.
///
/// The cancellation token is cloned, so the context observes the same
/// cancellation tree as the legacy `RingContext`.
#[derive(Clone)]
pub struct RuntimeContext {
    session_id: String,
    workspace_root: PathBuf,
    sandbox: SandboxKind,
    cancellation: CancelToken,
}

impl RuntimeContext {
    /// Legacy adapter: snapshot the actor's current session/workspace/sandbox
    /// and pair them with the shared cancellation token.
    pub fn from_legacy_ambient(session_id: impl Into<String>, cancellation: CancelToken) -> Self {
        Self {
            session_id: session_id.into(),
            workspace_root: PathBuf::from(qaqh_workspace::current_workspace()),
            sandbox: SandboxKind::from_legacy(),
            cancellation,
        }
    }

    /// Session identity captured for this runtime.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Workspace root captured for this runtime.
    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    /// Sandbox mode captured for this runtime.
    pub fn sandbox(&self) -> SandboxKind {
        self.sandbox
    }

    /// Cancellation token shared with the legacy ring.
    pub fn cancellation(&self) -> &CancelToken {
        &self.cancellation
    }
}

/// Explicit identity and runtime context for one turn lap.
#[derive(Clone)]
pub struct TurnContext {
    runtime: RuntimeContext,
    turn_id: String,
    round_num: u32,
}

impl TurnContext {
    /// Legacy adapter used by `run_lap` before the explicit context becomes
    /// the owner of these values.
    pub fn from_legacy(ctx: &RingContext<'_>, turn_id: impl Into<String>, round_num: u32) -> Self {
        Self {
            runtime: RuntimeContext::from_legacy_ambient(
                ctx.agent.session.seed.clone(),
                ctx.cancel.clone(),
            ),
            turn_id: turn_id.into(),
            round_num,
        }
    }

    /// Runtime state shared by all rounds of this turn.
    pub fn runtime(&self) -> &RuntimeContext {
        &self.runtime
    }

    /// Stable turn identity.
    pub fn turn_id(&self) -> &str {
        &self.turn_id
    }

    /// Current round within the turn.
    pub fn round_num(&self) -> u32 {
        self.round_num
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct AmbientGuard;

    impl Drop for AmbientGuard {
        fn drop(&mut self) {
            qaqh_workspace::clear_actor_context();
            qaqh_workspace::authorization::set_subagent_sandbox(false);
        }
    }

    #[test]
    fn legacy_adapter_preserves_runtime_identity_workspace_sandbox_and_cancel() {
        qaqh_workspace::set_actor_context("/tmp/qaqh-p2-4a", "seed-236");
        qaqh_workspace::authorization::set_subagent_sandbox(true);
        let _guard = AmbientGuard;

        let cancel = CancelToken::new();
        let runtime = RuntimeContext::from_legacy_ambient("seed-236".to_string(), cancel.clone());

        assert_eq!(runtime.session_id(), "seed-236");
        assert_eq!(runtime.workspace_root(), Path::new("/tmp/qaqh-p2-4a"));
        assert_eq!(runtime.sandbox(), SandboxKind::Subagent);
        assert!(!runtime.cancellation().is_set());

        cancel.set();
        assert!(
            runtime.cancellation().is_set(),
            "explicit runtime context must share the legacy cancellation token"
        );
    }

    #[test]
    fn turn_context_preserves_runtime_and_turn_identity() {
        qaqh_workspace::set_actor_context("/tmp/qaqh-p2-4a-turn", "seed-turn");
        let _guard = AmbientGuard;

        let runtime =
            RuntimeContext::from_legacy_ambient("seed-turn".to_string(), CancelToken::new());
        let turn = TurnContext {
            runtime: runtime.clone(),
            turn_id: "turn-236".to_string(),
            round_num: 3,
        };

        assert_eq!(turn.runtime().session_id(), runtime.session_id());
        assert_eq!(turn.runtime().workspace_root(), runtime.workspace_root());
        assert_eq!(turn.turn_id(), "turn-236");
        assert_eq!(turn.round_num(), 3);
    }
}
