//! P3 gate: real child-process crashes across the durable tool ledger windows.

#![allow(clippy::unwrap_used)] // integration test fixture

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use qaqh_message::MessageStore;
use qaqh_session::canonical::{
    CanonicalSessionIdentity, CommittedFactReader, RecoveryExecutionOutcome, ToolLedger, WriterId,
    execute_recovery_intent, generate_ulid, persist_recovery_intent, plan_recovery_intent,
    sha256_content_hash, ulid_from_text,
};
use qaqh_session::session_fact_v2::{
    EventId, ExecutionId, FactPayload, PolicyDecisionRef, RecoveryId, SideEffectClass, ToolCallId,
    ToolIntent, ToolIntentPolicyOutcome, ToolReplayCapability, ToolTerminalStatus,
};

const CHILD_MODE_ENV: &str = "QAQH_CRASH_CHILD_MODE";
const CHILD_DIR_ENV: &str = "QAQH_CRASH_CHILD_DIR";
const SIDE_EFFECT_FILE: &str = "handler-side-effect.marker";
const WIRE_CALL_ID: &str = "wire-crash-call";

fn unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn append_intent_before_abort(dir: &Path, mode: &str) {
    let identity = CanonicalSessionIdentity::open_or_create(dir).expect("identity");
    let now = unix_ms();
    let mut ledger = ToolLedger::open(
        dir,
        identity.session_id,
        identity.log_id,
        WriterId::new("crash-child"),
        now,
        1,
    )
    .expect("open child ledger");
    ledger
        .append_intent(
            EventId::new(generate_ulid()),
            None,
            ToolIntent {
                call_id: ToolCallId::new(format!("call_{}", ulid_from_text(WIRE_CALL_ID))),
                execution_id: ExecutionId::new(format!("exec_{}", generate_ulid())),
                idempotency_key: None,
                replay_capability: ToolReplayCapability::NoReplay,
                policy_decision: PolicyDecisionRef {
                    outcome: ToolIntentPolicyOutcome::Allow,
                    rule_id: "crash-matrix".into(),
                    decided_at_ms: now,
                    reason_ref: None,
                },
                effective_args_ref: None,
                effective_args_hash: None,
                sandbox_spec_hash: sha256_content_hash(b"crash-matrix-sandbox"),
                side_effect_class: SideEffectClass::WorkspaceWrite,
                intent_at_ms: now,
            },
            now,
        )
        .expect("append crash intent");
    if mode == "after_handler" {
        std::fs::write(dir.join(SIDE_EFFECT_FILE), b"side effect committed")
            .expect("write side effect marker");
    }
    std::process::abort();
}

#[test]
fn crash_child_entrypoint() {
    let Ok(mode) = std::env::var(CHILD_MODE_ENV) else {
        return;
    };
    let dir = std::env::var(CHILD_DIR_ENV).expect("child dir");
    append_intent_before_abort(Path::new(&dir), &mode);
}

fn run_child(dir: &Path, mode: &str) {
    let status = Command::new(std::env::current_exe().expect("current exe"))
        .arg("--exact")
        .arg("crash_child_entrypoint")
        .arg("--nocapture")
        .env(CHILD_MODE_ENV, mode)
        .env(CHILD_DIR_ENV, dir)
        .status()
        .expect("spawn crash child");
    assert!(!status.success(), "crash child must terminate abnormally");
}

fn recover(dir: &Path) {
    let identity = CanonicalSessionIdentity::open_or_create(dir).expect("identity");
    let facts =
        CommittedFactReader::open(dir, identity.session_id.clone(), identity.log_id.clone())
            .expect("reader")
            .read_all()
            .expect("facts");
    let plan = plan_recovery_intent(
        dir,
        identity.session_id.clone(),
        identity.log_id.clone(),
        RecoveryId::new(format!("recovery_{}", generate_ulid())),
        EventId::new(generate_ulid()),
        sha256_content_hash(b"[]"),
    )
    .expect("plan recovery")
    .expect("open intent must produce a plan");
    persist_recovery_intent(dir, &plan, &facts).expect("persist recovery intent");

    std::thread::sleep(Duration::from_millis(2));
    let outcome = execute_recovery_intent(
        dir,
        identity.session_id,
        identity.log_id,
        WriterId::new("crash-recovery"),
        unix_ms(),
        10_000,
    )
    .expect("execute recovery");
    assert!(
        matches!(outcome, RecoveryExecutionOutcome::Recovered(_)),
        "NoReplay recovery must close the batch"
    );
}

fn assert_single_indeterminate_terminal(dir: &Path) {
    let identity = CanonicalSessionIdentity::open_or_create(dir).expect("identity");
    let facts = CommittedFactReader::open(dir, identity.session_id, identity.log_id)
        .expect("reader")
        .read_all()
        .expect("facts");
    let intents = facts
        .iter()
        .filter(|fact| matches!(fact.payload, FactPayload::ToolIntent(_)))
        .count();
    let finished = facts
        .iter()
        .filter_map(|fact| match &fact.payload {
            FactPayload::ToolFinished(finished) => Some(finished),
            _ => None,
        })
        .collect::<Vec<_>>();
    let recovered = facts
        .iter()
        .filter(|fact| matches!(fact.payload, FactPayload::SessionRecovered(_)))
        .count();
    assert_eq!(intents, 1);
    assert_eq!(finished.len(), 1, "exactly one terminal after recovery");
    assert_eq!(
        finished[0].terminal_status,
        ToolTerminalStatus::Indeterminate
    );
    assert_eq!(recovered, 1, "exactly one recovery batch marker");
}

fn orphan_restore_store() -> MessageStore {
    let mut assistant = qaqh_types::Message {
        msg_id: None,
        role: "assistant".into(),
        name: None,
        content: Vec::new(),
    };
    assistant.content.push(qaqh_types::ContentBlock::ToolUse {
        id: WIRE_CALL_ID.into(),
        name: "write".into(),
        input: serde_json::json!({"path": "crash-marker"}),
    });
    let messages = vec![qaqh_types::Message::user("do it"), assistant];
    let (store, repairs) = MessageStore::from_messages("crash-recovery", &messages, 0);
    assert_eq!(repairs.len(), 1, "orphan tool_use must be repaired");
    store
}

fn restore_note_after_reconcile(dir: &Path) -> String {
    let mut store = orphan_restore_store();
    qaqh_runtime::agent::tool_recovery::reconcile_store_in(dir, &mut store);
    store
        .turns()
        .iter()
        .flat_map(|turn| turn.steps.iter())
        .flat_map(|step| step.tool_results.iter())
        .find_map(|result| {
            result.content.iter().find_map(|block| match block {
                qaqh_types::ContentBlock::ToolResult { result, .. } => {
                    Some(result.model_text().to_string())
                }
                _ => None,
            })
        })
        .expect("restore note")
}

#[test]
fn process_crash_after_intent_is_sealed_without_replay() {
    let dir = tempfile::tempdir().expect("tempdir");
    run_child(dir.path(), "after_intent");
    assert!(!dir.path().join(SIDE_EFFECT_FILE).exists());
    assert!(
        restore_note_after_reconcile(dir.path()).contains("durable execution intent"),
        "open canonical intent must refine the pre-recovery restore placeholder"
    );
    recover(dir.path());
    assert_single_indeterminate_terminal(dir.path());
    assert!(
        restore_note_after_reconcile(dir.path()).contains("canonical terminal indeterminate"),
        "recovered terminal must refine the post-recovery restore placeholder"
    );
}

#[test]
fn process_crash_after_side_effect_before_terminal_is_not_replayed() {
    let dir = tempfile::tempdir().expect("tempdir");
    run_child(dir.path(), "after_handler");
    assert!(
        dir.path().join(SIDE_EFFECT_FILE).exists(),
        "side effect must survive the crash"
    );
    assert!(
        restore_note_after_reconcile(dir.path()).contains("durable execution intent"),
        "side-effect crash must not restore as not-executed"
    );
    recover(dir.path());
    assert_single_indeterminate_terminal(dir.path());
    assert!(
        dir.path().join(SIDE_EFFECT_FILE).exists(),
        "recovery must not erase or replay the side effect"
    );
    assert!(
        restore_note_after_reconcile(dir.path()).contains("canonical terminal indeterminate"),
        "recovered side-effect call must restore with its canonical terminal"
    );
}
