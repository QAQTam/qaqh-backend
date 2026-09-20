//! SessionActor mailbox and TurnCore terminal contract.

use qaqh_session::actor::{
    SessionActor, SessionActorEffect, SessionActorError, TurnCommand, TurnCore, TurnCoreError,
    TurnEffect,
};
use qaqh_session::session_fact_v2::{InputId, TurnId, TurnMode, TurnTerminal};

fn turn_id(value: &str) -> TurnId {
    TurnId::new(value)
}

fn input_id(value: &str) -> InputId {
    InputId::new(value)
}

fn start(turn: &str) -> TurnCommand {
    TurnCommand::Start {
        turn_id: turn_id(turn),
        input_id: input_id("input-1"),
        mode: TurnMode::Normal,
    }
}

#[test]
fn turn_core_allows_only_one_active_turn_and_one_terminal() {
    let mut core = TurnCore::default();
    assert!(matches!(
        core.step(start("t1")),
        Ok(TurnEffect::Started { .. })
    ));
    assert!(matches!(
        core.step(start("t2")),
        Err(TurnCoreError::TurnAlreadyActive(id)) if id == turn_id("t1")
    ));

    assert!(matches!(
        core.step(TurnCommand::RoundStarted {
            turn_id: turn_id("t1"),
            round: 1,
        }),
        Ok(TurnEffect::RoundStarted { round: 1, .. })
    ));
    assert!(matches!(
        core.step(TurnCommand::Finish {
            turn_id: turn_id("t1"),
            terminal: TurnTerminal::Completed,
        }),
        Ok(TurnEffect::Finished {
            terminal: TurnTerminal::Completed,
            ..
        })
    ));
    assert_eq!(
        core.step(TurnCommand::Finish {
            turn_id: turn_id("t1"),
            terminal: TurnTerminal::Completed,
        }),
        Ok(TurnEffect::Noop)
    );
    assert_eq!(
        core.step(TurnCommand::Finish {
            turn_id: turn_id("t1"),
            terminal: TurnTerminal::Failed,
        }),
        Err(TurnCoreError::ConflictingTerminal)
    );
}

#[test]
fn cancel_is_idempotent_and_closes_the_turn_once() {
    let mut core = TurnCore::default();
    core.step(start("t1")).expect("start");

    assert!(matches!(
        core.step(TurnCommand::Cancel {
            turn_id: turn_id("t1"),
        }),
        Ok(TurnEffect::Interrupted { .. })
    ));
    assert_eq!(
        core.step(TurnCommand::Cancel {
            turn_id: turn_id("t1"),
        }),
        Ok(TurnEffect::Noop)
    );
    assert_eq!(
        core.step(TurnCommand::Finish {
            turn_id: turn_id("t1"),
            terminal: TurnTerminal::Completed,
        }),
        Err(TurnCoreError::ConflictingTerminal)
    );
}

#[test]
fn suspend_blocks_rounds_until_resume() {
    let mut core = TurnCore::default();
    core.step(start("t1")).expect("start");
    assert!(matches!(
        core.step(TurnCommand::Suspend {
            turn_id: turn_id("t1"),
        }),
        Ok(TurnEffect::Suspended { .. })
    ));
    assert_eq!(
        core.step(TurnCommand::RoundStarted {
            turn_id: turn_id("t1"),
            round: 1,
        }),
        Err(TurnCoreError::TurnSuspended)
    );
    assert!(matches!(
        core.step(TurnCommand::Resume {
            turn_id: turn_id("t1"),
        }),
        Ok(TurnEffect::Resumed { .. })
    ));
    assert!(matches!(
        core.step(TurnCommand::RoundStarted {
            turn_id: turn_id("t1"),
            round: 1,
        }),
        Ok(TurnEffect::RoundStarted { .. })
    ));
}

#[test]
fn mailbox_preserves_fifo_and_enforces_capacity() {
    let mut actor = SessionActor::new(2);
    actor
        .submit(qaqh_session::actor::SessionCommand::Turn(start("t1")))
        .expect("start");
    actor
        .submit(qaqh_session::actor::SessionCommand::Turn(
            TurnCommand::RoundStarted {
                turn_id: turn_id("t1"),
                round: 1,
            },
        ))
        .expect("round");
    assert_eq!(
        actor.submit(qaqh_session::actor::SessionCommand::Turn(
            TurnCommand::Finish {
                turn_id: turn_id("t1"),
                terminal: TurnTerminal::Completed,
            },
        )),
        Err(SessionActorError::MailboxFull)
    );

    let effects = actor.drain().expect("drain");
    assert!(matches!(
        effects.as_slice(),
        [
            SessionActorEffect::Turn(TurnEffect::Started { .. }),
            SessionActorEffect::Turn(TurnEffect::RoundStarted { round: 1, .. })
        ]
    ));
}

#[test]
fn shutdown_is_terminal_for_the_mailbox() {
    let mut actor = SessionActor::new(2);
    actor
        .submit(qaqh_session::actor::SessionCommand::Shutdown)
        .expect("shutdown");
    assert_eq!(
        actor.drain().expect("drain"),
        vec![SessionActorEffect::Shutdown]
    );
    assert_eq!(
        actor.submit(qaqh_session::actor::SessionCommand::Turn(start("t1"))),
        Err(SessionActorError::Shutdown)
    );
}
