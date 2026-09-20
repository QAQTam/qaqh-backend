//! Legacy mapping and migration stage contract.

use qaqh_session::canonical::{
    CanonicalLog, CanonicalSeq, LegacyMapping, LegacyMappingKey, LegacyMappingTarget, LegacySource,
    MIGRATION_STATUS_SCHEMA, MigrationController, MigrationError, MigrationStage,
    ReconciliationMetrics, V1DeliveryKind, append_legacy_mapping, load_mapping_keys,
};
use qaqh_session::session_fact_v2::{ContentHash, EventId, LogId, SessionFact, SessionId};

const ENVELOPE_FIXTURE: &str = include_str!("fixtures/session_fact_v2/envelope.jsonl");
const NOW_MS: i64 = 1_789_830_000_000;

fn session_id() -> SessionId {
    SessionId::new("0198f1a0-0000-7000-8000-000000000001")
}

fn log_id() -> LogId {
    LogId::new("0198f1a0-0000-7000-8000-000000000002")
}

fn fact(event_ordinal: u64) -> SessionFact {
    let mut fact: SessionFact =
        serde_json::from_str(ENVELOPE_FIXTURE.trim()).expect("parse envelope fixture");
    fact.fact_seq = event_ordinal;
    fact.event_id = EventId::new(format!("01J{event_ordinal:023}"));
    fact
}

fn hash(byte: char) -> ContentHash {
    ContentHash::new(format!("sha256:{}", byte.to_string().repeat(64)))
}

fn mapping() -> LegacyMapping {
    let legacy_identity = LegacyMapping::legacy_identity("messages-gen-7", "message-42");
    LegacyMapping {
        legacy_source: LegacySource::MessagesJsonl,
        source_generation_id: "messages-gen-7".into(),
        legacy_key: "message-42".into(),
        legacy_msg_id: Some("42".into()),
        session_id: session_id(),
        log_id: log_id(),
        targets: vec![
            LegacyMappingTarget {
                key: LegacyMappingKey {
                    legacy_identity: legacy_identity.clone(),
                    derived_ordinal: 0,
                    canonical_seq: CanonicalSeq {
                        fact_seq: 7,
                        projection_index: Some(0),
                    },
                },
                delivery: V1DeliveryKind::Reliable,
            },
            LegacyMappingTarget {
                key: LegacyMappingKey {
                    legacy_identity,
                    derived_ordinal: 1,
                    canonical_seq: CanonicalSeq {
                        fact_seq: 7,
                        projection_index: Some(1),
                    },
                },
                delivery: V1DeliveryKind::Reliable,
            },
        ],
        source_hash: hash('a'),
        mapped_at_ms: NOW_MS,
    }
}

#[test]
fn mapping_1_to_n_is_stable_and_duplicate_keys_fail_closed() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mapping = mapping();

    append_legacy_mapping(temp.path(), &mapping).expect("append mapping");
    let keys = load_mapping_keys(temp.path()).expect("load keys");
    assert_eq!(keys.len(), 2);
    assert!(keys.contains(&mapping.targets[0].key));
    assert!(keys.contains(&mapping.targets[1].key));

    let error =
        append_legacy_mapping(temp.path(), &mapping).expect_err("duplicate unique key must fail");
    assert!(matches!(error, MigrationError::MappingConflict));
}

#[test]
fn cutover_requires_zero_metrics_and_exact_next_stage() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut controller =
        MigrationController::load_or_create(temp.path(), session_id(), log_id(), "messages-gen-7")
            .expect("controller");
    let zero = ReconciliationMetrics::default();

    controller
        .cutover(MigrationStage::S2, "writer-1", 1, 1, zero, NOW_MS, 10_000)
        .expect_err("cannot skip S1");
    controller
        .cutover(MigrationStage::S1, "writer-1", 1, 1, zero, NOW_MS, 10_000)
        .expect("S1");
    controller
        .cutover(MigrationStage::S2, "writer-1", 1, 1, zero, NOW_MS, 10_000)
        .expect("S2");

    let mismatch = ReconciliationMetrics {
        missing_canonical: 1,
        ..zero
    };
    let error = controller
        .cutover(
            MigrationStage::S3,
            "writer-1",
            2,
            2,
            mismatch,
            NOW_MS,
            10_000,
        )
        .expect_err("nonzero metrics must block S3");
    assert!(matches!(error, MigrationError::CutoverGate));

    let status = controller
        .cutover(MigrationStage::S3, "writer-1", 2, 2, zero, NOW_MS, 10_000)
        .expect("S3");
    assert_eq!(status.schema, MIGRATION_STATUS_SCHEMA);
    assert_eq!(status.stage, MigrationStage::S3);
    assert_eq!(status.fencing_token, 2);
    assert_eq!(status.error_code, None);
    let json = serde_json::to_value(&status).expect("serialize status");
    assert_eq!(json["fencing_token"], "2");
}

#[test]
fn rollback_is_allowed_before_s3_and_forbidden_after_s3() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut controller =
        MigrationController::load_or_create(temp.path(), session_id(), log_id(), "messages-gen-7")
            .expect("controller");
    let zero = ReconciliationMetrics::default();

    controller
        .cutover(MigrationStage::S1, "writer-1", 1, 1, zero, NOW_MS, 10_000)
        .expect("S1");
    controller
        .cutover(MigrationStage::S2, "writer-1", 1, 1, zero, NOW_MS, 10_000)
        .expect("S2");
    controller
        .rollback(MigrationStage::S0, "writer-1", 1, 1, zero)
        .expect("rollback before S3");

    controller
        .cutover(MigrationStage::S1, "writer-2", 2, 2, zero, NOW_MS, 10_000)
        .expect("S1 again");
    controller
        .cutover(MigrationStage::S2, "writer-2", 2, 2, zero, NOW_MS, 10_000)
        .expect("S2 again");
    controller
        .cutover(MigrationStage::S3, "writer-2", 3, 3, zero, NOW_MS, 10_000)
        .expect("S3");
    let error = controller
        .rollback(MigrationStage::S2, "writer-2", 3, 3, zero)
        .expect_err("S3 rollback barrier");
    assert!(matches!(error, MigrationError::RollbackForbidden));
}

#[test]
fn persisted_state_rejects_a_stale_source_generation() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut controller =
        MigrationController::load_or_create(temp.path(), session_id(), log_id(), "messages-gen-7")
            .expect("controller");
    controller
        .cutover(
            MigrationStage::S1,
            "writer-1",
            1,
            1,
            ReconciliationMetrics::default(),
            NOW_MS,
            10_000,
        )
        .expect("S1");

    let error =
        MigrationController::load_or_create(temp.path(), session_id(), log_id(), "messages-gen-8")
            .expect_err("stale generation must fail");
    assert!(matches!(error, MigrationError::StaleGeneration(_)));
}

#[test]
fn s3_fence_rotation_rejects_the_old_writer() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut log = CanonicalLog::open(temp.path(), session_id(), log_id()).expect("open log");
    let old = log
        .acquire_writer(
            qaqh_session::canonical::WriterId::new("old-writer"),
            NOW_MS,
            10_000,
        )
        .expect("old writer");
    let new = log
        .rotate_writer_fence(
            qaqh_session::canonical::WriterId::new("new-writer"),
            2,
            2,
            NOW_MS + 1,
            10_000,
        )
        .expect("rotate fence");

    let error = log
        .append(&old, fact(1), NOW_MS + 2)
        .expect_err("old writer must be stale");
    assert!(matches!(
        error,
        qaqh_session::canonical::CanonicalError::StaleWriter(_)
    ));
    log.append(&new, fact(1), NOW_MS + 3)
        .expect("new writer append");
}
