//! Runtime adapter from existing turn outcomes to the canonical `SessionActor`.
//!
//! P2-1 deliberately keeps `run_lap` and its persistence/tool behavior
//! unchanged. This adapter observes the existing outcome boundary and mirrors
//! lifecycle transitions into the pure `TurnCore`, establishing one place to
//! enforce active-turn and terminal invariants before later mailbox migration.

use std::collections::BTreeSet;
use std::fmt;

use qaqh_session::actor::{
    SessionActor, SessionActorEffect, SessionActorError, SessionCommand, TurnCommand,
    TurnCoreState, TurnEffect,
};
use qaqh_session::session_fact_v2::{InputId, TurnId, TurnMode, TurnTerminal};

use super::types::Outcome;

const MAILBOX_CAPACITY: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InteractionAdmission {
    Accepted { remaining: usize },
    AlreadyResolved,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InteractionState {
    Pending,
    AlreadyResolved,
    Unknown,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TurnActorError {
    Actor(SessionActorError),
    ActiveTurnConflict {
        active: TurnId,
        incoming: TurnId,
    },
    ConflictingTerminal {
        turn_id: TurnId,
    },
    TerminalTurnCannotAdvance {
        turn_id: TurnId,
    },
    RoundRegression {
        turn_id: TurnId,
        current: u32,
        incoming: u32,
    },
    UnexpectedEffect,
}

impl fmt::Display for TurnActorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Actor(error) => write!(formatter, "{error}"),
            Self::ActiveTurnConflict { active, incoming } => {
                write!(
                    formatter,
                    "turn {incoming} cannot start while {active} is active"
                )
            }
            Self::ConflictingTerminal { turn_id } => {
                write!(formatter, "turn {turn_id} already has a different terminal")
            }
            Self::TerminalTurnCannotAdvance { turn_id } => {
                write!(
                    formatter,
                    "terminal turn {turn_id} cannot advance to another round"
                )
            }
            Self::RoundRegression {
                turn_id,
                current,
                incoming,
            } => write!(
                formatter,
                "turn {turn_id} cannot move from round {current} back to {incoming}"
            ),
            Self::UnexpectedEffect => {
                write!(formatter, "session actor returned an unexpected effect")
            }
        }
    }
}

impl std::error::Error for TurnActorError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Actor(error) => Some(error),
            _ => None,
        }
    }
}

impl From<SessionActorError> for TurnActorError {
    fn from(error: SessionActorError) -> Self {
        Self::Actor(error)
    }
}

#[derive(Debug)]
pub(crate) struct TurnActor {
    actor: SessionActor,
    pending_interactions: BTreeSet<String>,
    resolved_interactions: BTreeSet<String>,
}

impl Default for TurnActor {
    fn default() -> Self {
        Self::new()
    }
}

impl TurnActor {
    pub(crate) fn new() -> Self {
        Self {
            actor: SessionActor::new(MAILBOX_CAPACITY),
            pending_interactions: BTreeSet::new(),
            resolved_interactions: BTreeSet::new(),
        }
    }

    pub(crate) fn state(&self) -> &TurnCoreState {
        self.actor.state()
    }

    /// Admit a newly allocated input before its message is written to storage.
    pub(crate) fn begin_input(
        &mut self,
        turn_id: &str,
        input_id: &str,
    ) -> Result<(), TurnActorError> {
        self.start_with_input(turn_id, input_id)
    }

    /// Mirror one existing runtime outcome into the canonical turn state.
    ///
    /// `ContinueTurn { round_num: 0 }` starts a turn. Later rounds first
    /// resume a suspended actor, matching the legacy permission/ask resume
    /// path, then advance the round.
    #[cfg(test)]
    pub(crate) fn observe_outcome(&mut self, outcome: &Outcome) -> Result<(), TurnActorError> {
        self.observe_outcome_with_interactions(outcome, &[])
    }

    pub(crate) fn observe_outcome_with_interactions(
        &mut self,
        outcome: &Outcome,
        pending_interactions: &[String],
    ) -> Result<(), TurnActorError> {
        match outcome {
            Outcome::ContinueTurn {
                turn_id, round_num, ..
            } if *round_num == 0 => self.start(turn_id),
            Outcome::ContinueTurn {
                turn_id, round_num, ..
            } => self.round_started(turn_id, *round_num),
            Outcome::YieldToUser { turn_id, .. } => self.suspend(turn_id, pending_interactions),
            Outcome::TurnComplete { turn_id, .. } => self.finish(turn_id, TurnTerminal::Completed),
            Outcome::TurnAborted { turn_id, .. } => self.finish(turn_id, TurnTerminal::Cancelled),
            Outcome::TurnFailed { turn_id, .. } => self.finish(turn_id, TurnTerminal::Failed),
            Outcome::Handled | Outcome::Error(_) | Outcome::Shutdown => Ok(()),
        }
    }

    pub(crate) fn admit_interaction_resolution(
        &mut self,
        interaction_id: &str,
    ) -> InteractionAdmission {
        if self.pending_interactions.remove(interaction_id) {
            self.resolved_interactions
                .insert(interaction_id.to_string());
            return InteractionAdmission::Accepted {
                remaining: self.pending_interactions.len(),
            };
        }
        if self.resolved_interactions.contains(interaction_id) {
            return InteractionAdmission::AlreadyResolved;
        }
        InteractionAdmission::Unknown
    }

    pub(crate) fn interaction_state(&self, interaction_id: &str) -> InteractionState {
        if self.pending_interactions.contains(interaction_id) {
            return InteractionState::Pending;
        }
        if self.resolved_interactions.contains(interaction_id) {
            return InteractionState::AlreadyResolved;
        }
        InteractionState::Unknown
    }

    /// Record an explicit cancellation. Late cancellation of an idle or
    /// already-terminal turn is a no-op; cancelling a different active turn is
    /// rejected.
    pub(crate) fn cancel(&mut self, turn_id: &str) -> Result<(), TurnActorError> {
        match self.state().clone() {
            TurnCoreState::Idle | TurnCoreState::Terminal { .. } => Ok(()),
            TurnCoreState::Active {
                turn_id: active, ..
            } if active.as_str() == turn_id => {
                self.apply(TurnCommand::Cancel {
                    turn_id: TurnId::new(turn_id),
                })?;
                self.pending_interactions.clear();
                Ok(())
            }
            TurnCoreState::Active {
                turn_id: active, ..
            } => Err(TurnActorError::ActiveTurnConflict {
                active,
                incoming: TurnId::new(turn_id),
            }),
        }
    }

    pub(crate) fn cancel_active(&mut self) -> Result<(), TurnActorError> {
        match self.state().clone() {
            TurnCoreState::Active { turn_id, .. } => {
                self.apply(TurnCommand::Cancel { turn_id })?;
                self.pending_interactions.clear();
                Ok(())
            }
            TurnCoreState::Idle | TurnCoreState::Terminal { .. } => Ok(()),
        }
    }

    pub(crate) fn reset(&mut self) {
        self.actor = SessionActor::new(MAILBOX_CAPACITY);
        self.pending_interactions.clear();
        self.resolved_interactions.clear();
    }

    fn start(&mut self, turn_id: &str) -> Result<(), TurnActorError> {
        self.start_with_input(turn_id, &format!("input:{turn_id}"))
    }

    fn start_with_input(&mut self, turn_id: &str, input_id: &str) -> Result<(), TurnActorError> {
        match self.state().clone() {
            TurnCoreState::Active {
                turn_id: active, ..
            } if active.as_str() == turn_id => Ok(()),
            TurnCoreState::Active {
                turn_id: active, ..
            } => Err(TurnActorError::ActiveTurnConflict {
                active,
                incoming: TurnId::new(turn_id),
            }),
            _ => {
                self.apply(TurnCommand::Start {
                    turn_id: TurnId::new(turn_id),
                    input_id: InputId::new(input_id),
                    mode: TurnMode::Normal,
                })?;
                self.pending_interactions.clear();
                self.resolved_interactions.clear();
                Ok(())
            }
        }
    }

    fn round_started(&mut self, turn_id: &str, round: u32) -> Result<(), TurnActorError> {
        match self.state().clone() {
            TurnCoreState::Active {
                turn_id: active,
                round: current,
                suspended,
                ..
            } if active.as_str() == turn_id => {
                if round == current {
                    return Ok(());
                }
                if round < current {
                    return Err(TurnActorError::RoundRegression {
                        turn_id: active,
                        current,
                        incoming: round,
                    });
                }
                if suspended {
                    self.apply(TurnCommand::Resume {
                        turn_id: active.clone(),
                    })?;
                }
                self.apply(TurnCommand::RoundStarted {
                    turn_id: active,
                    round,
                })?;
                self.pending_interactions.clear();
                Ok(())
            }
            TurnCoreState::Idle => {
                self.start(turn_id)?;
                self.apply(TurnCommand::RoundStarted {
                    turn_id: TurnId::new(turn_id),
                    round,
                })?;
                self.pending_interactions.clear();
                Ok(())
            }
            TurnCoreState::Terminal {
                turn_id: terminal_turn,
                ..
            } if terminal_turn.as_str() == turn_id => {
                Err(TurnActorError::TerminalTurnCannotAdvance {
                    turn_id: terminal_turn,
                })
            }
            TurnCoreState::Terminal { .. } => {
                self.start(turn_id)?;
                self.apply(TurnCommand::RoundStarted {
                    turn_id: TurnId::new(turn_id),
                    round,
                })?;
                self.pending_interactions.clear();
                Ok(())
            }
            TurnCoreState::Active {
                turn_id: active, ..
            } => Err(TurnActorError::ActiveTurnConflict {
                active,
                incoming: TurnId::new(turn_id),
            }),
        }
    }

    fn suspend(
        &mut self,
        turn_id: &str,
        pending_interactions: &[String],
    ) -> Result<(), TurnActorError> {
        match self.state().clone() {
            TurnCoreState::Active {
                turn_id: active, ..
            } if active.as_str() == turn_id => {
                self.apply(TurnCommand::Suspend { turn_id: active })?;
                self.pending_interactions = pending_interactions.iter().cloned().collect();
                Ok(())
            }
            TurnCoreState::Idle => {
                self.start(turn_id)?;
                self.apply(TurnCommand::Suspend {
                    turn_id: TurnId::new(turn_id),
                })?;
                self.pending_interactions = pending_interactions.iter().cloned().collect();
                Ok(())
            }
            TurnCoreState::Terminal { .. } => Err(TurnActorError::TerminalTurnCannotAdvance {
                turn_id: TurnId::new(turn_id),
            }),
            TurnCoreState::Active {
                turn_id: active, ..
            } => Err(TurnActorError::ActiveTurnConflict {
                active,
                incoming: TurnId::new(turn_id),
            }),
        }
    }

    fn finish(&mut self, turn_id: &str, terminal: TurnTerminal) -> Result<(), TurnActorError> {
        match self.state().clone() {
            TurnCoreState::Idle => Ok(()),
            TurnCoreState::Active {
                turn_id: active, ..
            } if active.as_str() == turn_id => {
                self.apply(TurnCommand::Finish {
                    turn_id: active,
                    terminal,
                })?;
                self.pending_interactions.clear();
                Ok(())
            }
            TurnCoreState::Active {
                turn_id: active, ..
            } => Err(TurnActorError::ActiveTurnConflict {
                active,
                incoming: TurnId::new(turn_id),
            }),
            TurnCoreState::Terminal {
                turn_id: terminal_turn,
                terminal: existing,
            } if terminal_turn.as_str() == turn_id && existing == terminal => Ok(()),
            TurnCoreState::Terminal {
                turn_id: terminal_turn,
                ..
            } if terminal_turn.as_str() == turn_id => Err(TurnActorError::ConflictingTerminal {
                turn_id: terminal_turn,
            }),
            TurnCoreState::Terminal { .. } => Ok(()),
        }
    }

    fn apply(&mut self, command: TurnCommand) -> Result<TurnEffect, TurnActorError> {
        self.actor
            .submit(SessionCommand::Turn(command))
            .map_err(TurnActorError::Actor)?;
        match self.actor.step().map_err(TurnActorError::Actor)? {
            Some(SessionActorEffect::Turn(effect)) => Ok(effect),
            Some(SessionActorEffect::Subscription(_))
            | Some(SessionActorEffect::Shutdown)
            | None => Err(TurnActorError::UnexpectedEffect),
        }
    }
}

#[cfg(test)]
mod tests {
    use qaqh_session::actor::TurnCoreState;
    use qaqh_session::session_fact_v2::TurnTerminal;

    use super::{InteractionAdmission, InteractionState, TurnActor, TurnActorError};
    use crate::agent::types::Outcome;

    fn continue_round(turn_id: &str, round_num: u32) -> Outcome {
        Outcome::ContinueTurn {
            turn_id: turn_id.to_string(),
            round_num,
            usage: None,
        }
    }

    fn complete(turn_id: &str) -> Outcome {
        Outcome::TurnComplete {
            turn_id: turn_id.to_string(),
            usage: None,
        }
    }

    #[test]
    fn maps_round_yield_resume_and_terminal() {
        let mut actor = TurnActor::new();
        actor
            .observe_outcome(&continue_round("t1", 0))
            .expect("start turn");
        assert!(matches!(
            actor.state(),
            TurnCoreState::Active {
                round: 0,
                suspended: false,
                ..
            }
        ));

        actor
            .observe_outcome(&Outcome::YieldToUser {
                turn_id: "t1".into(),
                reason: crate::agent::types::YieldReason::PermissionPending,
            })
            .expect("suspend turn");
        assert!(matches!(
            actor.state(),
            TurnCoreState::Active {
                suspended: true,
                ..
            }
        ));

        actor
            .observe_outcome(&continue_round("t1", 1))
            .expect("resume round");
        assert!(matches!(
            actor.state(),
            TurnCoreState::Active {
                round: 1,
                suspended: false,
                ..
            }
        ));

        actor
            .observe_outcome(&complete("t1"))
            .expect("complete turn");
        assert!(matches!(
            actor.state(),
            TurnCoreState::Terminal {
                terminal: TurnTerminal::Completed,
                ..
            }
        ));
    }

    #[test]
    fn duplicate_terminal_is_idempotent_and_conflict_fails_closed() {
        let mut actor = TurnActor::new();
        actor
            .observe_outcome(&continue_round("t1", 0))
            .expect("start turn");
        actor
            .observe_outcome(&complete("t1"))
            .expect("complete turn");
        actor
            .observe_outcome(&complete("t1"))
            .expect("duplicate terminal");

        let error = actor
            .observe_outcome(&Outcome::TurnFailed {
                turn_id: "t1".into(),
                usage: None,
                message: "boom".into(),
            })
            .expect_err("conflicting terminal must fail");
        assert_eq!(
            error,
            TurnActorError::ConflictingTerminal {
                turn_id: qaqh_session::session_fact_v2::TurnId::new("t1")
            }
        );
    }

    #[test]
    fn active_turn_conflict_is_rejected() {
        let mut actor = TurnActor::new();
        actor
            .observe_outcome(&continue_round("t1", 0))
            .expect("start t1");
        let error = actor
            .observe_outcome(&continue_round("t2", 0))
            .expect_err("second active turn must fail");
        assert!(matches!(
            error,
            TurnActorError::ActiveTurnConflict { active, incoming }
                if active.as_str() == "t1" && incoming.as_str() == "t2"
        ));
    }

    #[test]
    fn input_admission_starts_the_turn_before_storage() {
        let mut actor = TurnActor::new();
        actor
            .begin_input("t1", "input-1")
            .expect("admit first input");
        assert!(matches!(
            actor.state(),
            TurnCoreState::Active {
                input_id,
                round: 0,
                ..
            } if input_id.as_str() == "input-1"
        ));

        let error = actor
            .begin_input("t2", "input-2")
            .expect_err("second active input must fail");
        assert!(matches!(
            error,
            TurnActorError::ActiveTurnConflict { active, incoming }
                if active.as_str() == "t1" && incoming.as_str() == "t2"
        ));
    }

    #[test]
    fn interaction_resolution_is_first_answer_wins() {
        let mut actor = TurnActor::new();
        actor
            .begin_input("t1", "input-1")
            .expect("admit first input");
        actor
            .observe_outcome_with_interactions(
                &Outcome::YieldToUser {
                    turn_id: "t1".into(),
                    reason: crate::agent::types::YieldReason::PermissionPending,
                },
                &["p1".into(), "p2".into()],
            )
            .expect("suspend with interactions");

        assert_eq!(
            actor.admit_interaction_resolution("p1"),
            InteractionAdmission::Accepted { remaining: 1 }
        );
        assert_eq!(
            actor.admit_interaction_resolution("p1"),
            InteractionAdmission::AlreadyResolved
        );
        assert_eq!(
            actor.admit_interaction_resolution("missing"),
            InteractionAdmission::Unknown
        );
        assert_eq!(
            actor.admit_interaction_resolution("p2"),
            InteractionAdmission::Accepted { remaining: 0 }
        );
    }

    #[test]
    fn interaction_state_tracks_ask_and_plan_until_terminal() {
        let mut actor = TurnActor::new();
        actor
            .begin_input("t1", "input-1")
            .expect("admit first input");
        actor
            .observe_outcome_with_interactions(
                &Outcome::YieldToUser {
                    turn_id: "t1".into(),
                    reason: crate::agent::types::YieldReason::PlanReview,
                },
                &["ask-1".into(), "plan-1".into()],
            )
            .expect("suspend with ask and plan");

        assert_eq!(actor.interaction_state("ask-1"), InteractionState::Pending);
        assert_eq!(actor.interaction_state("plan-1"), InteractionState::Pending);
        assert_eq!(
            actor.admit_interaction_resolution("ask-1"),
            InteractionAdmission::Accepted { remaining: 1 }
        );
        assert_eq!(
            actor.interaction_state("ask-1"),
            InteractionState::AlreadyResolved
        );
        assert_eq!(actor.interaction_state("plan-1"), InteractionState::Pending);

        actor
            .observe_outcome(&complete("t1"))
            .expect("complete turn");
        assert_eq!(
            actor.interaction_state("ask-1"),
            InteractionState::AlreadyResolved
        );
        assert_ne!(actor.interaction_state("plan-1"), InteractionState::Pending);
    }

    #[test]
    fn cancel_is_idempotent() {
        let mut actor = TurnActor::new();
        actor
            .observe_outcome(&continue_round("t1", 0))
            .expect("start turn");
        actor.cancel("t1").expect("cancel turn");
        actor.cancel("t1").expect("duplicate cancel");
        assert!(matches!(
            actor.state(),
            TurnCoreState::Terminal {
                terminal: TurnTerminal::Cancelled,
                ..
            }
        ));
    }

    #[test]
    fn cancel_active_handles_commands_without_turn_id() {
        let mut actor = TurnActor::new();
        actor
            .observe_outcome(&continue_round("t1", 0))
            .expect("start turn");
        actor.cancel_active().expect("cancel active turn");
        actor.cancel_active().expect("duplicate cancel active");
        assert!(matches!(
            actor.state(),
            TurnCoreState::Terminal {
                terminal: TurnTerminal::Cancelled,
                ..
            }
        ));
    }
}
