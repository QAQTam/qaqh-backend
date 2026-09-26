//! Runtime lifecycle port.
//!
//! P2-4b moves liveness bookkeeping, session lifecycle calls, and the
//! first-turn title hook behind one boundary. The default implementation keeps
//! the legacy synchronous behavior; later actor/task migration can replace the
//! port without teaching `Loop` those details.

use std::sync::Arc;

use super::engine_session::SessionEngine;
use super::liveness::WorkerLiveness;
use super::state::agent::AgentState;
use super::types::{CancelToken, RingContext};

pub(crate) trait LifecyclePort {
    fn dispatch_started(&self);
    fn dispatch_finished(&self, suspended: bool);

    fn create_session(&self, agent: &mut AgentState, cancel: &CancelToken);
    fn create_session_with_seed(&self, agent: &mut AgentState, cancel: &CancelToken);
    fn resume_session(&self, agent: &mut AgentState, cancel: &CancelToken, seed: &str) -> bool;
    fn reload_config(&self, agent: &mut AgentState, cancel: &CancelToken);

    fn turn_completed(&self, ctx: &mut RingContext<'_>);
}

/// Default in-process lifecycle port. It delegates to the existing runtime
/// implementations and owns the liveness handle shared with the registry.
pub(crate) struct RuntimeLifecyclePort {
    liveness: Arc<WorkerLiveness>,
    session: SessionEngine,
}

impl RuntimeLifecyclePort {
    pub(crate) fn new(liveness: Arc<WorkerLiveness>) -> Self {
        Self {
            liveness,
            session: SessionEngine::new(),
        }
    }
}

impl LifecyclePort for RuntimeLifecyclePort {
    fn dispatch_started(&self) {
        self.liveness.set_busy(true);
    }

    fn dispatch_finished(&self, suspended: bool) {
        self.liveness.set_busy(false);
        self.liveness.touch();
        self.liveness.set_suspend_pending(suspended);
    }

    fn create_session(&self, agent: &mut AgentState, cancel: &CancelToken) {
        self.session.create(agent, cancel);
    }

    fn create_session_with_seed(&self, agent: &mut AgentState, cancel: &CancelToken) {
        self.session.create_with_seed(agent, cancel);
    }

    fn resume_session(&self, agent: &mut AgentState, cancel: &CancelToken, seed: &str) -> bool {
        self.session.resume(agent, seed, cancel)
    }

    fn reload_config(&self, agent: &mut AgentState, cancel: &CancelToken) {
        self.session.reload_config(agent, cancel);
    }

    fn turn_completed(&self, ctx: &mut RingContext<'_>) {
        crate::agent::engine_title::maybe_generate_title(ctx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ActorContextGuard;

    impl Drop for ActorContextGuard {
        fn drop(&mut self) {
            qaqh_workspace::clear_actor_context();
        }
    }

    #[test]
    fn dispatch_bookkeeping_matches_legacy_liveness_contract() {
        let liveness = Arc::new(WorkerLiveness::new());
        let port = RuntimeLifecyclePort::new(liveness.clone());

        port.dispatch_started();
        assert!(!liveness.unloadable(), "dispatch must mark the worker busy");

        port.dispatch_finished(true);
        assert!(
            !liveness.unloadable(),
            "a suspended turn must keep the worker unload-blocked"
        );
        assert!(liveness.idle_secs() <= 1, "dispatch completion must touch");

        port.dispatch_finished(false);
        assert!(liveness.unloadable(), "idle worker must become unloadable");
    }

    #[test]
    fn create_session_with_seed_keeps_seed_and_resets_store() {
        qaqh_workspace::set_actor_context("", "seed-240");
        let _guard = ActorContextGuard;
        let liveness = Arc::new(WorkerLiveness::new());
        let port = RuntimeLifecyclePort::new(liveness);
        let mut agent = AgentState::new(qaqh_config::Config::default());
        agent.ephemeral = true;
        agent.session.session_id = "seed-240".to_string();

        port.create_session_with_seed(&mut agent, &CancelToken::new());

        assert_eq!(agent.session.session_id, "seed-240");
        assert_eq!(agent.msg.turn_count(), 0);
    }
}
