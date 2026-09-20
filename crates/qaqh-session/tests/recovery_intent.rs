//! Recovery intent persistence and active/stale identity contract.

use std::fs;

use qaqh_session::canonical::{
    CanonicalError, RECOVERY_INTENT_FILE, RECOVERY_INTENT_SCHEMA, RecoveryIntent,
    RecoveryIntentStatus, RecoveryIntentWriteOutcome, load_recovery_intent,
    persist_recovery_intent, remove_recovery_intent_if_stale, sha256_content_hash,
};
use qaqh_session::session_fact_v2::{
    ContentHash, EventId, FactPayload, LogId, RecoveryId, RecoveryOutcome, SessionFact, SessionId,
    SessionRecovered,
};

const ENVELOPE_FIXTURE: &str = include_str!("fixtures/session_fact_v2/envelope.jsonl");

fn log_id() -> LogId {
    LogId::new("0198f1a0-0000-7000-8000-000000000002")
}

fn hash(byte: char) -> ContentHash {
    ContentHash::new(format!("sha256:{}", byte.to_string().repeat(64)))
}

fn intent() -> RecoveryIntent {
    RecoveryIntent::new(
        RecoveryId::new("recovery_01J00000000000000000000001"),
        EventId::new("01J00000000000000000000002"),
        log_id(),
        7,
        vec!["turn_02".into(), "turn_01".into(), "turn_02".into()],
        hash('a'),
        hash('b'),
        hash('c'),
    )
    .expect("build recovery intent")
}

fn recovered_fact(
    recovery_id: &str,
    recovery_event_id: &str,
    fingerprint: &ContentHash,
    last_good_fact_seq: u64,
) -> SessionFact {
    let mut fact: SessionFact =
        serde_json::from_str(ENVELOPE_FIXTURE.trim()).expect("parse envelope fixture");
    fact.session_id = SessionId::new("0198f1a0-0000-7000-8000-000000000001");
    fact.log_id = log_id();
    fact.fact_seq = 8;
    fact.event_id = EventId::new("01J00000000000000000000003");
    fact.payload = FactPayload::SessionRecovered(SessionRecovered {
        recovery_id: RecoveryId::new(recovery_id),
        recovery_event_id: EventId::new(recovery_event_id),
        recovery_input_fingerprint: fingerprint.clone(),
        outcome: RecoveryOutcome::Writable,
        last_good_fact_seq,
        torn_tail: false,
        torn_bytes: None,
        actions: Vec::new(),
        recovered_at_ms: fact.ts_ms,
    });
    fact
}

#[test]
fn constructor_normalizes_open_ids_and_computes_stable_fingerprint() {
    let first = intent();
    let second = RecoveryIntent::new(
        RecoveryId::new("recovery_01J00000000000000000000009"),
        EventId::new("01J00000000000000000000009"),
        log_id(),
        7,
        vec!["turn_01".into(), "turn_02".into()],
        hash('a'),
        hash('b'),
        hash('c'),
    )
    .expect("build equivalent intent");

    assert_eq!(first.sorted_open_ids, ["turn_01", "turn_02"]);
    assert_eq!(
        first.recovery_ref.recovery_input_fingerprint,
        second.recovery_ref.recovery_input_fingerprint
    );
    assert_eq!(first.batch_key(), second.batch_key());
}

#[test]
fn round_trip_persists_schema_and_removes_temp_file() {
    let temp = tempfile::tempdir().expect("tempdir");
    let expected = intent();

    assert_eq!(
        persist_recovery_intent(temp.path(), &expected, &[]).expect("persist recovery intent"),
        RecoveryIntentWriteOutcome::Written
    );

    let loaded = load_recovery_intent(temp.path())
        .expect("load recovery intent")
        .expect("intent exists");
    assert_eq!(loaded, expected);
    assert_eq!(loaded.schema, RECOVERY_INTENT_SCHEMA);
    assert!(!temp.path().join("recovery.intent.tmp").exists());
}

#[test]
fn corrupt_intent_fails_closed() {
    let temp = tempfile::tempdir().expect("tempdir");
    fs::write(temp.path().join(RECOVERY_INTENT_FILE), b"not json").expect("write corrupt intent");

    let error = load_recovery_intent(temp.path()).expect_err("corrupt intent must fail closed");
    assert!(matches!(error, CanonicalError::Json(_)));
}

#[test]
fn schema_mismatch_fails_closed() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut intent = intent();
    intent.schema = "qaqh.recovery-intent/v0".into();

    let error = persist_recovery_intent(temp.path(), &intent, &[])
        .expect_err("schema mismatch must fail closed");
    assert!(matches!(error, CanonicalError::InvalidRecoveryIntent(_)));
    assert!(!temp.path().join(RECOVERY_INTENT_FILE).exists());
}

#[test]
fn unsorted_or_duplicate_open_ids_fail_closed() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut intent = intent();
    intent.sorted_open_ids = vec!["turn_02".into(), "turn_01".into()];

    let error = persist_recovery_intent(temp.path(), &intent, &[])
        .expect_err("unsorted open ids must fail closed");
    assert!(matches!(error, CanonicalError::InvalidRecoveryIntent(_)));

    intent.sorted_open_ids = vec!["turn_01".into(), "turn_01".into()];
    let error = persist_recovery_intent(temp.path(), &intent, &[])
        .expect_err("duplicate open ids must fail closed");
    assert!(matches!(error, CanonicalError::InvalidRecoveryIntent(_)));
}

#[test]
fn fingerprint_mismatch_fails_closed() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut intent = intent();
    intent.sorted_open_ids = vec!["turn_03".into()];

    let error = persist_recovery_intent(temp.path(), &intent, &[])
        .expect_err("fingerprint mismatch must fail closed");
    assert!(matches!(error, CanonicalError::InvalidRecoveryIntent(_)));
}

#[test]
fn intent_without_matching_session_recovered_is_active() {
    let intent = intent();

    assert_eq!(
        intent.status(&[]).expect("active intent"),
        RecoveryIntentStatus::Active
    );
}

#[test]
fn matching_session_recovered_makes_intent_stale() {
    let intent = intent();
    let recovered = recovered_fact(
        intent.recovery_ref.recovery_id.as_str(),
        intent.recovery_ref.recovery_event_id.as_str(),
        &intent.recovery_ref.recovery_input_fingerprint,
        intent.last_good_fact_seq,
    );

    assert_eq!(
        intent.status(&[recovered]).expect("stale intent"),
        RecoveryIntentStatus::Stale
    );
}

#[test]
fn same_batch_with_different_identity_fails_closed() {
    let intent = intent();
    let recovered = recovered_fact(
        "recovery_01J00000000000000000000009",
        "01J00000000000000000000009",
        &intent.recovery_ref.recovery_input_fingerprint,
        intent.last_good_fact_seq,
    );

    let error = intent
        .status(&[recovered])
        .expect_err("same batch identity conflict must fail closed");
    assert!(matches!(error, CanonicalError::RecoveryIntentConflict(_)));
}

#[test]
fn active_intent_must_be_reused_and_cannot_be_replaced() {
    let temp = tempfile::tempdir().expect("tempdir");
    let original = intent();
    assert_eq!(
        persist_recovery_intent(temp.path(), &original, &[]).expect("persist original"),
        RecoveryIntentWriteOutcome::Written
    );
    assert_eq!(
        persist_recovery_intent(temp.path(), &original, &[]).expect("reuse original"),
        RecoveryIntentWriteOutcome::Reused
    );

    let replacement = RecoveryIntent::new(
        RecoveryId::new("recovery_01J00000000000000000000009"),
        EventId::new("01J00000000000000000000009"),
        log_id(),
        8,
        vec!["turn_03".into()],
        hash('d'),
        hash('e'),
        hash('f'),
    )
    .expect("build replacement");
    let error = persist_recovery_intent(temp.path(), &replacement, &[])
        .expect_err("active intent must not be replaced");
    assert!(matches!(error, CanonicalError::RecoveryIntentConflict(_)));
}

#[test]
fn stale_intent_can_be_replaced_only_by_a_different_batch() {
    let temp = tempfile::tempdir().expect("tempdir");
    let original = intent();
    persist_recovery_intent(temp.path(), &original, &[]).expect("persist original");
    let recovered = recovered_fact(
        original.recovery_ref.recovery_id.as_str(),
        original.recovery_ref.recovery_event_id.as_str(),
        &original.recovery_ref.recovery_input_fingerprint,
        original.last_good_fact_seq,
    );

    let mut same_batch_new_plan = original.clone();
    same_batch_new_plan.plan_hash = hash('d');
    let error = persist_recovery_intent(
        temp.path(),
        &same_batch_new_plan,
        std::slice::from_ref(&recovered),
    )
    .expect_err("same batch cannot change plan");
    assert!(matches!(error, CanonicalError::RecoveryIntentConflict(_)));

    let replacement = RecoveryIntent::new(
        RecoveryId::new("recovery_01J00000000000000000000009"),
        EventId::new("01J00000000000000000000009"),
        log_id(),
        8,
        vec!["turn_03".into()],
        hash('d'),
        hash('e'),
        hash('f'),
    )
    .expect("build replacement");
    assert_eq!(
        persist_recovery_intent(temp.path(), &replacement, &[recovered])
            .expect("replace stale batch"),
        RecoveryIntentWriteOutcome::Written
    );
    assert_eq!(
        load_recovery_intent(temp.path())
            .expect("load replacement")
            .expect("replacement exists"),
        replacement
    );
}

#[test]
fn only_stale_intent_can_be_removed() {
    let temp = tempfile::tempdir().expect("tempdir");
    let intent = intent();
    persist_recovery_intent(temp.path(), &intent, &[]).expect("persist intent");

    assert!(
        !remove_recovery_intent_if_stale(temp.path(), &intent, &[])
            .expect("active intent is retained")
    );
    assert!(temp.path().join(RECOVERY_INTENT_FILE).exists());

    let recovered = recovered_fact(
        intent.recovery_ref.recovery_id.as_str(),
        intent.recovery_ref.recovery_event_id.as_str(),
        &intent.recovery_ref.recovery_input_fingerprint,
        intent.last_good_fact_seq,
    );
    assert!(
        remove_recovery_intent_if_stale(temp.path(), &intent, &[recovered])
            .expect("stale intent is removed")
    );
    assert!(!temp.path().join(RECOVERY_INTENT_FILE).exists());
}

#[test]
fn empty_torn_tail_hash_is_sha256_empty() {
    assert_eq!(
        sha256_content_hash(b"").as_str(),
        "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
}
