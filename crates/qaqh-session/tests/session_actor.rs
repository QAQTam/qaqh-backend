//! SessionActor mailbox and TurnCore terminal contract.

use qaqh_domain::RingingChannel;
use qaqh_session::actor::{
    ConnectionId, SessionActor, SessionActorEffect, SessionActorError, SubscriptionCommand,
    SubscriptionEffect, ToolAdmission, TurnCommand, TurnCore, TurnCoreError, TurnEffect,
};
use qaqh_session::canonical::{ToolLedger, WriterId};
use qaqh_session::session_fact_v2::{
    ContentHash, EventId, ExecutionId, InputId, LogId, PolicyDecisionRef, SessionId,
    SideEffectClass, ToolCallId, ToolFinished, ToolIntent, ToolIntentPolicyOutcome,
    ToolReplayCapability, ToolTerminalStatus, TurnId, TurnMode, TurnTerminal,
};

const NOW_MS: i64 = 1_789_830_000_000;

fn turn_id(value: &str) -> TurnId {
    TurnId::new(value)
}

fn ledger_turn_id() -> TurnId {
    TurnId::new("turn_01J00000000000000000000001")
}

fn session_id() -> SessionId {
    SessionId::new("0198f1a0-0000-7000-8000-000000000001")
}

fn log_id() -> LogId {
    LogId::new("0198f1a0-0000-7000-8000-000000000002")
}

fn call_id(ordinal: u64) -> ToolCallId {
    ToolCallId::new(format!("call_{ordinal:026}"))
}

fn execution_id(ordinal: u64) -> ExecutionId {
    ExecutionId::new(format!("exec_{ordinal:026}"))
}

fn event_id(ordinal: u64) -> EventId {
    EventId::new(format!("01J{ordinal:023}"))
}

fn intent(call: &ToolCallId, execution: &ExecutionId) -> ToolIntent {
    ToolIntent {
        call_id: call.clone(),
        execution_id: execution.clone(),
        idempotency_key: None,
        replay_capability: ToolReplayCapability::NoReplay,
        policy_decision: PolicyDecisionRef {
            outcome: ToolIntentPolicyOutcome::Allow,
            rule_id: "allow_readonly".into(),
            decided_at_ms: NOW_MS,
            reason_ref: None,
        },
        effective_args_ref: None,
        effective_args_hash: None,
        sandbox_spec_hash: ContentHash::new(format!("sha256:{:064x}", 1)),
        side_effect_class: SideEffectClass::ReadOnly,
        intent_at_ms: NOW_MS,
    }
}

fn open_ledger(dir: &std::path::Path) -> ToolLedger {
    ToolLedger::open(
        dir,
        session_id(),
        log_id(),
        WriterId::new("session-actor-test"),
        NOW_MS,
        10_000,
    )
    .expect("open ledger")
}

fn start_actor(actor: &mut SessionActor, turn: &str) {
    actor
        .submit(qaqh_session::actor::SessionCommand::Turn(start(turn)))
        .expect("submit start");
    actor.step().expect("start step");
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

#[test]
fn actor_admits_tool_intent_and_resumes_suspended_turn() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut ledger = open_ledger(temp.path());
    let mut actor = SessionActor::new(4);
    let turn = ledger_turn_id();
    start_actor(&mut actor, turn.as_str());
    actor
        .submit(qaqh_session::actor::SessionCommand::Turn(
            TurnCommand::Suspend {
                turn_id: turn.clone(),
            },
        ))
        .expect("submit suspend");
    actor.step().expect("suspend step");

    let call = call_id(1);
    let execution = execution_id(1);
    let admission = actor
        .admit_tool_intent(
            &mut ledger,
            &turn,
            &turn,
            event_id(1),
            intent(&call, &execution),
            NOW_MS + 1,
        )
        .expect("admit intent");
    assert_eq!(
        admission,
        ToolAdmission::Admitted {
            execution_id: execution,
            intent_at_ms: NOW_MS,
        }
    );
    assert!(matches!(
        actor.state(),
        qaqh_session::actor::TurnCoreState::Active {
            suspended: false,
            ..
        }
    ));
    assert!(ledger.get(&call).expect("ledger entry").is_open());
}

#[test]
fn actor_returns_existing_terminal_without_writing_another_intent() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut ledger = open_ledger(temp.path());
    let mut actor = SessionActor::new(4);
    let turn = ledger_turn_id();
    start_actor(&mut actor, turn.as_str());

    let call = call_id(2);
    let execution = execution_id(2);
    actor
        .admit_tool_intent(
            &mut ledger,
            &turn,
            &turn,
            event_id(2),
            intent(&call, &execution),
            NOW_MS + 2,
        )
        .expect("admit intent");
    ledger
        .append_finished(
            event_id(3),
            Some(turn.clone()),
            ToolFinished {
                call_id: call.clone(),
                execution_id: Some(execution.clone()),
                terminal_status: ToolTerminalStatus::Indeterminate,
                output_ref: None,
                error: None,
                metrics: qaqh_session::session_fact_v2::ToolMetrics {
                    started_at_ms: NOW_MS,
                    finished_at_ms: NOW_MS + 3,
                    retry_count: 0,
                    output_bytes: 0,
                    progress_bytes_total: 0,
                },
                reconciled: false,
                evidence_ref: None,
                evidence_fact_seq: None,
                evidence_event_id: None,
                recovery_ref: None,
                finished_at_ms: NOW_MS + 3,
            },
            NOW_MS + 3,
        )
        .expect("append terminal");

    let admission = actor
        .admit_tool_intent(
            &mut ledger,
            &turn,
            &turn,
            event_id(4),
            intent(&call, &execution),
            NOW_MS + 4,
        )
        .expect("existing terminal is an idempotent admission result");
    assert!(matches!(
        admission,
        ToolAdmission::ExistingFinished {
            finished: ToolFinished {
                terminal_status: ToolTerminalStatus::Indeterminate,
                ..
            }
        }
    ));
    assert_eq!(
        ledger.open_intents().len(),
        0,
        "existing terminal must not append another intent"
    );
}

#[test]
fn cancel_tool_batch_closes_turn_and_writes_executionless_cancelled() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut ledger = open_ledger(temp.path());
    let mut actor = SessionActor::new(4);
    let turn = ledger_turn_id();
    start_actor(&mut actor, turn.as_str());

    let call = call_id(3);
    let appended = actor
        .cancel_tool_batch(&mut ledger, &turn, &turn, vec![call.clone()], NOW_MS + 5)
        .expect("cancel batch");
    assert_eq!(appended, vec![call.clone()]);
    assert!(matches!(
        actor.state(),
        qaqh_session::actor::TurnCoreState::Terminal {
            terminal: TurnTerminal::Cancelled,
            ..
        }
    ));
    let finished = ledger
        .get(&call)
        .and_then(|entry| entry.finished())
        .expect("cancelled terminal");
    assert_eq!(finished.terminal_status, ToolTerminalStatus::Cancelled);
    assert_eq!(finished.execution_id, None);

    let again = actor
        .cancel_tool_batch(&mut ledger, &turn, &turn, vec![call.clone()], NOW_MS + 6)
        .expect("repeat cancel is idempotent");
    assert!(again.is_empty());
}

#[test]
fn cancel_before_resume_admission_returns_terminal_and_never_appends_intent() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut ledger = open_ledger(temp.path());
    let mut actor = SessionActor::new(4);
    let turn = ledger_turn_id();
    start_actor(&mut actor, turn.as_str());
    actor
        .submit(qaqh_session::actor::SessionCommand::Turn(
            TurnCommand::Suspend {
                turn_id: turn.clone(),
            },
        ))
        .expect("submit suspend");
    actor.step().expect("suspend step");

    let call = call_id(4);
    actor
        .cancel_tool_batch(&mut ledger, &turn, &turn, vec![call.clone()], NOW_MS + 7)
        .expect("cancel suspended turn");
    let admission = actor
        .admit_tool_intent(
            &mut ledger,
            &turn,
            &turn,
            event_id(5),
            intent(&call, &execution_id(4)),
            NOW_MS + 8,
        )
        .expect("admission returns existing terminal");
    assert!(matches!(
        admission,
        ToolAdmission::TurnTerminal {
            terminal: TurnTerminal::Cancelled,
            ..
        }
    ));
    assert!(
        ledger.get(&call).expect("ledger entry").intent().is_none(),
        "cancelled admission must not create a ToolIntent"
    );
}
