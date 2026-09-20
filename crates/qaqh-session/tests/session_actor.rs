//! SessionActor mailbox and TurnCore terminal contract.

use qaqh_domain::RingingChannel;
use qaqh_session::actor::{
    ConnectionId, SessionActor, SessionActorEffect, SessionActorError, SubscriptionCommand,
    SubscriptionEffect, TurnCommand, TurnCore, TurnCoreError, TurnEffect,
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

fn connection(value: &str) -> ConnectionId {
    ConnectionId::new(value)
}

fn subscribe(connection_id: &str, channel: RingingChannel) -> SubscriptionCommand {
    SubscriptionCommand::Subscribe {
        connection_id: connection(connection_id),
        channel,
    }
}

#[test]
fn subscription_commands_share_the_turn_mailbox_and_are_idempotent() {
    let mut actor = SessionActor::new(8);
    actor
        .submit(qaqh_session::actor::SessionCommand::Turn(start("t1")))
        .expect("start");
    actor
        .submit(qaqh_session::actor::SessionCommand::Subscription(
            subscribe("connection-a", RingingChannel::Control),
        ))
        .expect("subscribe");
    actor
        .submit(qaqh_session::actor::SessionCommand::Subscription(
            subscribe("connection-a", RingingChannel::Control),
        ))
        .expect("duplicate subscribe");
    actor
        .submit(qaqh_session::actor::SessionCommand::Subscription(
            subscribe("connection-b", RingingChannel::Tool),
        ))
        .expect("second connection");
    actor
        .submit(qaqh_session::actor::SessionCommand::Subscription(
            SubscriptionCommand::Unsubscribe {
                connection_id: connection("connection-a"),
                channel: RingingChannel::Control,
            },
        ))
        .expect("unsubscribe");
    actor
        .submit(qaqh_session::actor::SessionCommand::Subscription(
            SubscriptionCommand::ConnectionClosed {
                connection_id: connection("connection-b"),
            },
        ))
        .expect("close connection");

    let effects = actor.drain().expect("drain");
    assert!(matches!(
        effects.as_slice(),
        [
            SessionActorEffect::Turn(TurnEffect::Started { .. }),
            SessionActorEffect::Subscription(SubscriptionEffect::Subscribed { changed: true, .. }),
            SessionActorEffect::Subscription(SubscriptionEffect::Subscribed { changed: false, .. }),
            SessionActorEffect::Subscription(SubscriptionEffect::Subscribed { changed: true, .. }),
            SessionActorEffect::Subscription(SubscriptionEffect::Unsubscribed {
                changed: true,
                ..
            }),
            SessionActorEffect::Subscription(SubscriptionEffect::ConnectionClosed {
                removed: 1,
                ..
            }),
        ]
    ));
    assert!(actor.subscribers().is_empty());
}

#[test]
fn connection_close_is_scoped_and_idempotent() {
    let mut actor = SessionActor::new(8);
    for command in [
        subscribe("connection-a", RingingChannel::Control),
        subscribe("connection-a", RingingChannel::Tool),
        subscribe("connection-b", RingingChannel::Conversation),
        SubscriptionCommand::ConnectionClosed {
            connection_id: connection("connection-a"),
        },
        SubscriptionCommand::ConnectionClosed {
            connection_id: connection("connection-a"),
        },
    ] {
        actor
            .submit(qaqh_session::actor::SessionCommand::Subscription(command))
            .expect("submit subscription command");
    }
    actor.drain().expect("drain");

    assert!(
        !actor
            .subscribers()
            .is_subscribed(&connection("connection-a"), RingingChannel::Control)
    );
    assert!(
        actor
            .subscribers()
            .is_subscribed(&connection("connection-b"), RingingChannel::Conversation)
    );
    assert_eq!(actor.subscribers().len(), 1);
}

#[test]
fn subscription_ingress_rejects_mailbox_full_and_post_shutdown() {
    let mut actor = SessionActor::new(1);
    actor
        .submit(qaqh_session::actor::SessionCommand::Subscription(
            subscribe("connection-a", RingingChannel::Control),
        ))
        .expect("first subscription");
    assert_eq!(
        actor.submit(qaqh_session::actor::SessionCommand::Subscription(
            subscribe("connection-b", RingingChannel::Tool),
        )),
        Err(SessionActorError::MailboxFull)
    );
    actor.drain().expect("drain");

    actor
        .submit(qaqh_session::actor::SessionCommand::Shutdown)
        .expect("shutdown");
    actor.drain().expect("drain shutdown");
    assert_eq!(
        actor.submit(qaqh_session::actor::SessionCommand::Subscription(
            subscribe("connection-c", RingingChannel::Conversation),
        )),
        Err(SessionActorError::Shutdown)
    );
}
