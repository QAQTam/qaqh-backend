//! P2-6 subagent recovery scanner.
//!
//! The scanner consumes committed canonical facts only. It never invents a
//! `SubagentSpawned` edge; an edge without a child log is an integrity error,
//! while a child log without an edge is an orphan that must be tombstoned.
//! Child terminal facts may be projected into a parent `SubagentFinished`
//! recovery step, and input facts are deduplicated by stable `input_id`.

use std::collections::{HashMap, HashSet};

use qaqh_session::session_fact_v2::{
    EventId, FactPayload, InputId, InputPurpose, SessionFact, SessionId, SubagentTerminalStatus,
    TurnFinished, TurnId, TurnTerminal,
};

/// One child canonical log supplied to the scanner.
#[derive(Debug, Clone)]
pub struct ChildLog {
    pub session_id: SessionId,
    pub facts: Vec<SessionFact>,
}

/// Recovery actions are deterministic and append-only. They never rewrite an
/// existing spawn edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryAction {
    /// Child reached a terminal `TurnFinished` but the parent edge is open.
    CompleteSubagentFinished {
        parent_session_id: SessionId,
        child_session_id: SessionId,
        parent_call_id: String,
        status: SubagentTerminalStatus,
        terminal_fact_seq: u64,
        terminal_event_id: EventId,
    },
    /// Child log exists but has no parent `SubagentSpawned` edge.
    TombstoneOrphanChild { child_session_id: SessionId },
    /// A `trigger_turn` input was accepted but has no `TurnStarted`.
    AppendTurnStarted {
        child_session_id: SessionId,
        input_id: InputId,
        turn_id: TurnId,
    },
}

/// Integrity issues that must not be auto-repaired.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryIssue {
    EdgeWithoutChildLog {
        parent_session_id: SessionId,
        child_session_id: SessionId,
    },
    DuplicateSpawnEdge {
        parent_session_id: SessionId,
        child_session_id: SessionId,
    },
    ChildParentMismatch {
        parent_session_id: SessionId,
        child_session_id: SessionId,
        observed_parent_session_id: Option<SessionId>,
    },
    DuplicateInputAccepted {
        child_session_id: SessionId,
        input_id: InputId,
    },
    DuplicateTurnStarted {
        child_session_id: SessionId,
        input_id: InputId,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecoveryPlan {
    pub actions: Vec<RecoveryAction>,
    pub issues: Vec<RecoveryIssue>,
}

/// Scan one parent log and all known child logs.
pub fn scan(
    parent_session_id: &SessionId,
    parent_facts: &[SessionFact],
    child_logs: &[ChildLog],
) -> RecoveryPlan {
    let mut plan = RecoveryPlan::default();
    let mut spawned: HashMap<SessionId, Vec<&qaqh_session::session_fact_v2::SubagentSpawned>> =
        HashMap::new();
    let mut finished: HashSet<SessionId> = HashSet::new();

    for fact in parent_facts {
        match &fact.payload {
            FactPayload::SubagentSpawned(payload) => {
                spawned
                    .entry(payload.child_session_id.clone())
                    .or_default()
                    .push(payload);
            }
            FactPayload::SubagentFinished(payload) => {
                finished.insert(payload.child_session_id.clone());
            }
            _ => {}
        }
    }

    let logs_by_child: HashMap<SessionId, &ChildLog> = child_logs
        .iter()
        .map(|log| (log.session_id.clone(), log))
        .collect();

    for (child, edges) in &spawned {
        if edges.len() > 1 {
            plan.issues.push(RecoveryIssue::DuplicateSpawnEdge {
                parent_session_id: parent_session_id.clone(),
                child_session_id: child.clone(),
            });
        }
        let Some(log) = logs_by_child.get(child) else {
            plan.issues.push(RecoveryIssue::EdgeWithoutChildLog {
                parent_session_id: parent_session_id.clone(),
                child_session_id: child.clone(),
            });
            continue;
        };
        let observed_parent = log.facts.iter().find_map(|fact| match &fact.payload {
            FactPayload::SessionCreated(created) => created.parent_session_id.clone(),
            _ => None,
        });
        if observed_parent.as_ref() != Some(parent_session_id) {
            plan.issues.push(RecoveryIssue::ChildParentMismatch {
                parent_session_id: parent_session_id.clone(),
                child_session_id: child.clone(),
                observed_parent_session_id: observed_parent,
            });
            continue;
        }
        if !finished.contains(child)
            && let Some(terminal) = child_terminal(&log.facts)
        {
            plan.actions.push(RecoveryAction::CompleteSubagentFinished {
                parent_session_id: parent_session_id.clone(),
                child_session_id: child.clone(),
                parent_call_id: edges[0].parent_call_id.as_str().to_string(),
                status: terminal.status,
                terminal_fact_seq: terminal.fact_seq,
                terminal_event_id: terminal.event_id,
            });
        }
    }

    for log in child_logs {
        if !spawned.contains_key(&log.session_id) {
            plan.actions.push(RecoveryAction::TombstoneOrphanChild {
                child_session_id: log.session_id.clone(),
            });
        }
        scan_inputs(&mut plan, &log.session_id, &log.facts);
    }

    plan
}

struct Terminal {
    status: SubagentTerminalStatus,
    fact_seq: u64,
    event_id: EventId,
}

fn child_terminal(facts: &[SessionFact]) -> Option<Terminal> {
    facts
        .iter()
        .filter_map(|fact| match &fact.payload {
            FactPayload::TurnFinished(payload) => Some((fact, payload)),
            _ => None,
        })
        .max_by_key(|(fact, _)| fact.fact_seq)
        .map(|(fact, payload)| Terminal {
            status: terminal_status(payload),
            fact_seq: fact.fact_seq,
            event_id: fact.event_id.clone(),
        })
}

fn terminal_status(payload: &TurnFinished) -> SubagentTerminalStatus {
    match payload.terminal {
        TurnTerminal::Completed => SubagentTerminalStatus::Completed,
        TurnTerminal::Cancelled => SubagentTerminalStatus::Cancelled,
        TurnTerminal::Failed
            if payload
                .error
                .as_ref()
                .is_some_and(|error| error.code == "subagent_timed_out") =>
        {
            SubagentTerminalStatus::TimedOut
        }
        TurnTerminal::Failed => SubagentTerminalStatus::Failed,
    }
}

fn scan_inputs(plan: &mut RecoveryPlan, child: &SessionId, facts: &[SessionFact]) {
    let mut accepted: HashMap<InputId, (InputPurpose, usize)> = HashMap::new();
    let mut started: HashMap<InputId, usize> = HashMap::new();
    for fact in facts {
        match &fact.payload {
            FactPayload::InputAccepted(payload) => {
                let count = accepted
                    .entry(payload.input_id.clone())
                    .or_insert((payload.input_purpose, 0));
                count.1 += 1;
            }
            FactPayload::TurnStarted(payload) => {
                *started.entry(payload.input_id.clone()).or_default() += 1;
            }
            _ => {}
        }
    }

    for (input_id, (purpose, count)) in accepted {
        if count > 1 {
            plan.issues.push(RecoveryIssue::DuplicateInputAccepted {
                child_session_id: child.clone(),
                input_id: input_id.clone(),
            });
        }
        let starts = started.get(&input_id).copied().unwrap_or(0);
        if starts > 1 {
            plan.issues.push(RecoveryIssue::DuplicateTurnStarted {
                child_session_id: child.clone(),
                input_id: input_id.clone(),
            });
            continue;
        }
        if purpose == InputPurpose::TriggerTurn && starts == 0 {
            plan.actions.push(RecoveryAction::AppendTurnStarted {
                child_session_id: child.clone(),
                input_id: input_id.clone(),
                turn_id: TurnId::new(format!("turn_recovery_{}", input_id.as_str())),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_session::session_fact_v2::{
        ActorKind, ActorRef, FactSchema, InputAccepted, InputKind, LogId, SessionCreated,
        SubagentSpawned, TurnError,
    };

    fn session_id(raw: &str) -> SessionId {
        SessionId::new(raw)
    }

    fn log_id(raw: &str) -> LogId {
        LogId::new(raw)
    }

    fn event_id(seq: u64) -> EventId {
        EventId::new(format!("01J{seq:023}"))
    }

    fn fact(session: &SessionId, seq: u64, payload: FactPayload) -> SessionFact {
        SessionFact {
            schema: FactSchema::v2(),
            session_id: session.clone(),
            log_id: log_id("0198f1a0-0000-7000-8000-000000000000"),
            fact_seq: seq,
            event_id: event_id(seq),
            ts_ms: seq as i64,
            causation_id: None,
            turn_id: None,
            call_id: None,
            interaction_id: None,
            payload,
        }
    }

    fn created(parent: &SessionId) -> FactPayload {
        FactPayload::SessionCreated(SessionCreated {
            created_at_ms: 1,
            cwd: "/tmp".into(),
            model: "test".into(),
            parent_session_id: Some(parent.clone()),
            schema_caps: vec![],
        })
    }

    fn spawned(child: &SessionId) -> FactPayload {
        FactPayload::SubagentSpawned(SubagentSpawned {
            child_session_id: child.clone(),
            parent_call_id: qaqh_session::session_fact_v2::ToolCallId::new("call-1"),
            role: None,
            spawned_at_ms: 1,
        })
    }

    fn accepted(input_id: &str, purpose: InputPurpose) -> FactPayload {
        FactPayload::InputAccepted(InputAccepted {
            input_id: InputId::new(input_id),
            input_kind: InputKind::System,
            input_purpose: purpose,
            content_ref: None,
            inline_text: Some("hello".into()),
            attachments: vec![],
            actor: ActorRef {
                kind: ActorKind::System,
                id: "test".into(),
                display_name: None,
            },
            client_request_id: None,
        })
    }

    #[test]
    fn edge_without_child_log_is_integrity_issue() {
        let parent = session_id("0198f1a0-0000-7000-8000-000000000001");
        let child = session_id("0198f1a0-0000-7000-8000-000000000002");
        let plan = scan(&parent, &[fact(&parent, 1, spawned(&child))], &[]);
        assert_eq!(
            plan.issues,
            vec![RecoveryIssue::EdgeWithoutChildLog {
                parent_session_id: parent,
                child_session_id: child,
            }]
        );
        assert!(plan.actions.is_empty());
    }

    #[test]
    fn child_without_edge_is_tombstoned() {
        let parent = session_id("0198f1a0-0000-7000-8000-000000000001");
        let child = session_id("0198f1a0-0000-7000-8000-000000000002");
        let plan = scan(
            &parent,
            &[],
            &[ChildLog {
                session_id: child.clone(),
                facts: vec![fact(&child, 1, created(&parent))],
            }],
        );
        assert_eq!(
            plan.actions,
            vec![RecoveryAction::TombstoneOrphanChild {
                child_session_id: child,
            }]
        );
    }

    #[test]
    fn terminal_child_with_open_edge_gets_finished_recovery() {
        let parent = session_id("0198f1a0-0000-7000-8000-000000000001");
        let child = session_id("0198f1a0-0000-7000-8000-000000000002");
        let terminal = fact(
            &child,
            2,
            FactPayload::TurnFinished(TurnFinished {
                turn_id: TurnId::new("turn-1"),
                terminal: TurnTerminal::Failed,
                usage: None,
                error: Some(TurnError {
                    code: "subagent_timed_out".into(),
                    message: "timeout".into(),
                    retryable: false,
                    details_ref: None,
                }),
                finished_at_ms: 2,
            }),
        );
        let plan = scan(
            &parent,
            &[fact(&parent, 1, spawned(&child))],
            &[ChildLog {
                session_id: child.clone(),
                facts: vec![fact(&child, 1, created(&parent)), terminal],
            }],
        );
        assert_eq!(plan.issues, vec![]);
        assert_eq!(
            plan.actions,
            vec![RecoveryAction::CompleteSubagentFinished {
                parent_session_id: parent,
                child_session_id: child,
                parent_call_id: "call-1".into(),
                status: SubagentTerminalStatus::TimedOut,
                terminal_fact_seq: 2,
                terminal_event_id: event_id(2),
            }]
        );
    }

    #[test]
    fn trigger_turn_missing_turn_started_is_repaired_once() {
        let parent = session_id("0198f1a0-0000-7000-8000-000000000001");
        let child = session_id("0198f1a0-0000-7000-8000-000000000002");
        let plan = scan(
            &parent,
            &[fact(&parent, 1, spawned(&child))],
            &[ChildLog {
                session_id: child.clone(),
                facts: vec![
                    fact(&child, 1, created(&parent)),
                    fact(&child, 2, accepted("input-1", InputPurpose::TriggerTurn)),
                ],
            }],
        );
        assert_eq!(
            plan.actions,
            vec![RecoveryAction::AppendTurnStarted {
                child_session_id: child,
                input_id: InputId::new("input-1"),
                turn_id: TurnId::new("turn_recovery_input-1"),
            }]
        );
    }

    #[test]
    fn queue_only_never_gets_turn_started() {
        let parent = session_id("0198f1a0-0000-7000-8000-000000000001");
        let child = session_id("0198f1a0-0000-7000-8000-000000000002");
        let plan = scan(
            &parent,
            &[fact(&parent, 1, spawned(&child))],
            &[ChildLog {
                session_id: child.clone(),
                facts: vec![
                    fact(&child, 1, created(&parent)),
                    fact(&child, 2, accepted("input-1", InputPurpose::QueueOnly)),
                ],
            }],
        );
        assert!(plan.actions.is_empty());
    }

    #[test]
    fn duplicate_input_is_reported_and_not_repaired_twice() {
        let parent = session_id("0198f1a0-0000-7000-8000-000000000001");
        let child = session_id("0198f1a0-0000-7000-8000-000000000002");
        let plan = scan(
            &parent,
            &[fact(&parent, 1, spawned(&child))],
            &[ChildLog {
                session_id: child.clone(),
                facts: vec![
                    fact(&child, 1, created(&parent)),
                    fact(&child, 2, accepted("input-1", InputPurpose::TriggerTurn)),
                    fact(&child, 3, accepted("input-1", InputPurpose::TriggerTurn)),
                ],
            }],
        );
        assert_eq!(
            plan.issues,
            vec![RecoveryIssue::DuplicateInputAccepted {
                child_session_id: child.clone(),
                input_id: InputId::new("input-1"),
            }]
        );
        assert_eq!(plan.actions.len(), 1);
    }

    #[test]
    fn duplicate_spawn_edge_is_integrity_issue_and_never_rewritten() {
        let parent = session_id("0198f1a0-0000-7000-8000-000000000001");
        let child = session_id("0198f1a0-0000-7000-8000-000000000002");
        let plan = scan(
            &parent,
            &[
                fact(&parent, 1, spawned(&child)),
                fact(&parent, 2, spawned(&child)),
            ],
            &[ChildLog {
                session_id: child.clone(),
                facts: vec![fact(&child, 1, created(&parent))],
            }],
        );
        assert_eq!(
            plan.issues,
            vec![RecoveryIssue::DuplicateSpawnEdge {
                parent_session_id: parent,
                child_session_id: child,
            }]
        );
    }

    #[test]
    fn child_parent_mismatch_is_rejected() {
        let parent = session_id("0198f1a0-0000-7000-8000-000000000001");
        let other = session_id("0198f1a0-0000-7000-8000-000000000009");
        let child = session_id("0198f1a0-0000-7000-8000-000000000002");
        let plan = scan(
            &parent,
            &[fact(&parent, 1, spawned(&child))],
            &[ChildLog {
                session_id: child.clone(),
                facts: vec![fact(&child, 1, created(&other))],
            }],
        );
        assert_eq!(
            plan.issues,
            vec![RecoveryIssue::ChildParentMismatch {
                parent_session_id: parent,
                child_session_id: child,
                observed_parent_session_id: Some(other),
            }]
        );
    }
}
