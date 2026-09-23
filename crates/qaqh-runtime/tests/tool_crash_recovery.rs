//! P3 gate: real child-process crashes across the durable tool ledger windows.

#![allow(clippy::unwrap_used)] // integration test fixture

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use qaqh_session::canonical::{
    CanonicalSessionIdentity, CommittedFactReader, RecoveryExecutionOutcome, ToolLedger, WriterId,
    execute_recovery_intent, generate_ulid, persist_recovery_intent, plan_recovery_intent,
    sha256_content_hash,
};
use qaqh_session::session_fact_v2::{
    EventId, ExecutionId, FactPayload, PolicyDecisionRef, RecoveryId, SideEffectClass, ToolCallId,
    ToolIntent, ToolIntentPolicyOutcome, ToolReplayCapability, ToolTerminalStatus,
};

const CHILD_MODE_ENV: &str = "QAQH_CRASH_CHILD_MODE";
const CHILD_DIR_ENV: &str = "QAQH_CRASH_CHILD_DIR";
const SIDE_EFFECT_FILE: &str = "handler-side-effect.marker";

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
                call_id: ToolCallId::new(format!("call_{}", generate_ulid())),
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

#[test]
fn process_crash_after_intent_is_sealed_without_replay() {
    let dir = tempfile::tempdir().expect("tempdir");
    run_child(dir.path(), "after_intent");
    assert!(!dir.path().join(SIDE_EFFECT_FILE).exists());
    recover(dir.path());
    assert_single_indeterminate_terminal(dir.path());
}

#[test]
fn process_crash_after_side_effect_before_terminal_is_not_replayed() {
    let dir = tempfile::tempdir().expect("tempdir");
    run_child(dir.path(), "after_handler");
    assert!(
        dir.path().join(SIDE_EFFECT_FILE).exists(),
        "side effect must survive the crash"
    );
    recover(dir.path());
    assert_single_indeterminate_terminal(dir.path());
    assert!(
        dir.path().join(SIDE_EFFECT_FILE).exists(),
        "recovery must not erase or replay the side effect"
    );
}
