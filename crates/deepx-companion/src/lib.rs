mod bridge;
mod hub;
mod interaction;
mod journal;
mod secret;
mod supervisor;
mod visual;

pub use bridge::{ResponseFrameError, interaction_from_agent_event, response_to_agent_frame};
pub use hub::{CompanionHub, CompanionHubHandle};
pub use interaction::{BeginClaim, InteractionClaim, InteractionCoordinator, InteractionSource};
pub use journal::{CompanionState, PublishedEvent};
pub use secret::generate_secret_hex;
pub use supervisor::{PetSupervisor, RestartPolicy};
pub use visual::{
    next_visual_state_for_agent_event, notification_for_agent_event, visual_state_for_agent_event,
};

#[cfg(test)]
mod tests {
    use deepx_proto::{
        CompanionCommandStatus, CompanionEvent, CompanionInteraction, CompanionInteractionKey,
        CompanionInteractionKind, CompanionInteractionPayload, CompanionSession,
        SessionActivityState,
    };

    use super::{BeginClaim, CompanionState, InteractionCoordinator, InteractionSource};

    fn session(seed: &str, session_seq: u64) -> CompanionSession {
        CompanionSession {
            seed: seed.into(),
            title: None,
            workspace: None,
            state: SessionActivityState::Working,
            visual_state: deepx_proto::CompanionVisualState::Working,
            turn_id: Some("turn-1".into()),
            session_seq,
            updated_at: 100 + session_seq,
        }
    }

    fn interaction(generation: u64) -> CompanionInteraction {
        CompanionInteraction {
            key: CompanionInteractionKey {
                seed: "deadbeef".into(),
                generation,
                kind: CompanionInteractionKind::PlanReview,
                request_id: "plan-1".into(),
            },
            payload: CompanionInteractionPayload::PlanReview {
                plan_content: "# Plan".into(),
            },
        }
    }

    #[test]
    fn journal_assigns_monotonic_sequence_and_builds_snapshot() {
        let mut state = CompanionState::new("epoch-1");
        let first = state.publish(CompanionEvent::SessionActivity {
            session: session("deadbeef", 1),
        });
        let second = state.publish(CompanionEvent::InteractionRequested {
            interaction: interaction(3),
        });

        assert_eq!(first.seq, 1);
        assert_eq!(second.seq, 2);
        assert_eq!(first.server_epoch, "epoch-1");
        let snapshot = state.snapshot();
        assert_eq!(snapshot.snapshot_seq, 2);
        assert_eq!(snapshot.sessions, vec![session("deadbeef", 1)]);
        assert_eq!(snapshot.pending_interactions, vec![interaction(3)]);
    }

    #[test]
    fn older_session_sequence_cannot_overwrite_newer_state() {
        let mut state = CompanionState::new("epoch-1");
        state.publish(CompanionEvent::SessionActivity {
            session: session("deadbeef", 5),
        });
        state.publish(CompanionEvent::SessionActivity {
            session: session("deadbeef", 4),
        });
        assert_eq!(state.snapshot().sessions[0].session_seq, 5);
    }

    #[test]
    fn first_interaction_claim_wins_and_commit_is_idempotent() {
        let coordinator = InteractionCoordinator::default();
        let pending = interaction(7);
        coordinator.register(pending.clone());

        let claim =
            match coordinator.begin(&pending.key, "command-main", InteractionSource::Desktop) {
                BeginClaim::Claimed(claim) => claim,
                other => panic!("expected claim, got {other:?}"),
            };
        assert_eq!(
            coordinator.begin(&pending.key, "command-pet", InteractionSource::Pet),
            BeginClaim::Rejected(CompanionCommandStatus::AlreadyResolved)
        );
        assert!(coordinator.commit(&claim));
        assert_eq!(
            coordinator.begin(&pending.key, "command-main", InteractionSource::Desktop),
            BeginClaim::Duplicate(CompanionCommandStatus::Accepted)
        );
    }

    #[test]
    fn failed_agent_write_rolls_back_claim_for_another_ui() {
        let coordinator = InteractionCoordinator::default();
        let pending = interaction(7);
        coordinator.register(pending.clone());
        let claim =
            match coordinator.begin(&pending.key, "command-main", InteractionSource::Desktop) {
                BeginClaim::Claimed(claim) => claim,
                other => panic!("expected claim, got {other:?}"),
            };
        assert!(coordinator.rollback(&claim));
        assert!(matches!(
            coordinator.begin(&pending.key, "command-pet", InteractionSource::Pet),
            BeginClaim::Claimed(_)
        ));
    }

    #[test]
    fn old_generation_is_rejected_after_new_generation_registers() {
        let coordinator = InteractionCoordinator::default();
        let old = interaction(1);
        let current = interaction(2);
        coordinator.register(old.clone());
        coordinator.register(current);
        assert_eq!(
            coordinator.begin(&old.key, "command-old", InteractionSource::Pet),
            BeginClaim::Rejected(CompanionCommandStatus::StaleGeneration)
        );
    }

    #[test]
    fn new_generation_invalidates_old_requests_even_when_request_ids_differ() {
        let coordinator = InteractionCoordinator::default();
        let old = interaction(1);
        coordinator.register(old.clone());

        let expired = coordinator.advance_generation("deadbeef", 2);
        assert_eq!(expired, vec![old.key.clone()]);
        assert_eq!(
            coordinator.begin(&old.key, "late-old", InteractionSource::Pet),
            BeginClaim::Rejected(CompanionCommandStatus::StaleGeneration)
        );
    }

    #[test]
    fn disconnect_invalidates_all_pending_requests_for_that_process() {
        let coordinator = InteractionCoordinator::default();
        let pending = interaction(4);
        coordinator.register(pending.clone());

        assert_eq!(
            coordinator.invalidate_generation("deadbeef", 4),
            vec![pending.key.clone()]
        );
        assert_eq!(
            coordinator.begin(&pending.key, "after-disconnect", InteractionSource::Desktop),
            BeginClaim::Rejected(CompanionCommandStatus::AlreadyResolved)
        );
    }

    #[test]
    fn duplicate_registration_cannot_reset_a_claim_or_reopen_a_resolved_request() {
        let coordinator = InteractionCoordinator::default();
        let pending = interaction(9);
        coordinator.register(pending.clone());
        let claim = match coordinator.begin(&pending.key, "first", InteractionSource::Pet) {
            BeginClaim::Claimed(claim) => claim,
            other => panic!("expected claim, got {other:?}"),
        };
        coordinator.register(pending.clone());
        assert_eq!(
            coordinator.begin(&pending.key, "second", InteractionSource::Desktop),
            BeginClaim::Rejected(CompanionCommandStatus::AlreadyResolved)
        );
        assert!(coordinator.commit(&claim));
        coordinator.register(pending.clone());
        assert_eq!(
            coordinator.begin(&pending.key, "third", InteractionSource::Desktop),
            BeginClaim::Rejected(CompanionCommandStatus::AlreadyResolved)
        );
    }

    #[test]
    fn backend_resolution_closes_pending_interaction() {
        let coordinator = InteractionCoordinator::default();
        let pending = interaction(4);
        coordinator.register(pending.clone());
        assert!(coordinator.resolve(&pending.key));
        assert_eq!(
            coordinator.begin(&pending.key, "late-command", InteractionSource::Pet),
            BeginClaim::Rejected(CompanionCommandStatus::AlreadyResolved)
        );
    }
}
