//! P0-6 Ringing v2 acceptance fixture matrix.
//!
//! Drives the real [`V2ProjectionHub`] against seeded canonical facts and
//! asserts the frozen rows of `docs/spec/2026-09-23-TUI-Ringing-v2冻结语义-spec.md`
//! §13 that are reachable today.
//!
//! Rows covered here:
//!
//! | row   | scenario                                   |
//! |-------|--------------------------------------------|
//! | V2-C1 | snapshot + subscribe (no gap, no dup)      |
//! | V2-C2 | reliable reconnect replays only after cursor |
//! | V2-C4 | ephemeral never enters replay/cursor       |
//! | V2-C5 | log_id mismatch -> `log_id_mismatch`       |
//! | V2-C6 | cursor expired / unknown fact reset        |
//! | V2-C7 | snapshot missing                           |
//! | —     | live `replay_overflow` reset               |
//!
//! Rows covered elsewhere: V2-R1..R4 and V2-D1..D3 live in the daemon route
//! tests (`crates/qaqh-daemon/src/axum_server.rs`), which own lease, admission
//! and HTTP concerns.
//!
//! Known gaps (see the P0-6 handoff): V2-C3 (replaceable) and the v1 cursor
//! mapping (V2-V1) have no production producer yet, so they are asserted at the
//! wire level only.

use std::path::Path;

use qaqh_domain::RingingChannel;
use qaqh_ringing::{
    CanonicalCursor, CursorToken, RingingV2Delivery, RingingV2EventEnvelope, RingingV2ResetReason,
};
use qaqh_runtime::ringing::{V2HubError, V2ProjectionHub, V2StreamItem};
use qaqh_session::canonical::{
    CanonicalSessionIdentity, CanonicalSessionStore, WriterId, generate_ulid,
};
use qaqh_session::projection::ProjectionSink;
use qaqh_session::session_fact_v2::{
    EventId, FactPayload, FactSchema, MetadataSource, SessionCreated, SessionFact,
    SessionMetadataChanged, SessionMetadataPatch,
};

const NOW_MS: i64 = 1_789_830_000_000;

/// One seeded canonical session: a temp dir, its identity, and a live writer
/// lease used to append facts.
struct Fixture {
    dir: tempfile::TempDir,
    identity: CanonicalSessionIdentity,
    store: CanonicalSessionStore,
    lease: qaqh_session::canonical::WriterLease,
    seq: i64,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let identity = CanonicalSessionIdentity::open_or_create(dir.path()).expect("identity");
        let store = CanonicalSessionStore::open(
            dir.path(),
            identity.session_id.clone(),
            identity.log_id.clone(),
        )
        .expect("store");
        let mut store = store;
        let lease = store
            .acquire_writer(WriterId::new(tag), NOW_MS, 600_000)
            .expect("writer lease");
        Self {
            dir,
            identity,
            store,
            lease,
            seq: 0,
        }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn envelope(&self, ts_ms: i64, payload: FactPayload) -> SessionFact {
        SessionFact {
            schema: FactSchema::v2(),
            session_id: self.identity.session_id.clone(),
            log_id: self.identity.log_id.clone(),
            fact_seq: 0,
            event_id: EventId::new(generate_ulid()),
            ts_ms,
            causation_id: None,
            turn_id: None,
            call_id: None,
            interaction_id: None,
            payload,
        }
    }

    fn created(&mut self) -> SessionFact {
        let fact = self.envelope(
            NOW_MS,
            FactPayload::SessionCreated(SessionCreated {
                created_at_ms: NOW_MS,
                cwd: "/tmp".into(),
                model: "fixture".into(),
                parent_session_id: None,
                schema_caps: Vec::new(),
            }),
        );
        self.append(fact)
    }

    /// Build (but do not append) a metadata change — one reliable `MetaDelta`.
    fn metadata_fact(&mut self, model: &str) -> SessionFact {
        self.seq += 1;
        let ts = NOW_MS + self.seq;
        self.envelope(
            ts,
            FactPayload::SessionMetadataChanged(SessionMetadataChanged {
                patch: SessionMetadataPatch {
                    cwd: None,
                    model: Some(model.to_string()),
                    archived: None,
                    search_visibility: None,
                    parent_session_id: None,
                    schema_caps: None,
                },
                source: MetadataSource::Api,
                changed_at_ms: ts,
            }),
        )
    }

    fn append(&mut self, fact: SessionFact) -> SessionFact {
        let ts = fact.ts_ms;
        self.store
            .append(&self.lease, fact, ts)
            .expect("append")
            .fact
    }

    /// Append and publish the fact through the hub's live path.
    fn append_and_publish(&mut self, hub: &V2ProjectionHub, fact: SessionFact) -> SessionFact {
        let ts = fact.ts_ms;
        let outcome = self.store.append(&self.lease, fact, ts).expect("append");
        ProjectionSink::publish(hub, self.path(), &outcome.fact, &outcome.events);
        outcome.fact
    }
}

fn fact_seq(
    event: &RingingV2EventEnvelope<qaqh_session::session_fact_v2::ProjectionPayload>,
) -> u64 {
    event.fact_seq.expect("reliable envelope carries fact_seq")
}

/// V2-C1: a fresh subscribe from the bootstrap cursor sees no replayed history,
/// and a fact committed afterwards arrives live exactly once with a reliable
/// cursor that points at it.
#[tokio::test]
async fn v2_c1_snapshot_subscribe_has_no_gap_or_dup() {
    let mut fixture = Fixture::new("v2-c1");
    fixture.created();
    let hub = V2ProjectionHub::new("epoch-c1");

    let bootstrap = hub.bootstrap(fixture.path(), "seed").expect("bootstrap");
    let mut subscription = hub
        .subscribe(
            fixture.path(),
            "seed",
            RingingChannel::Control,
            Some(&bootstrap.snapshot_cursor),
        )
        .expect("subscribe");

    // No replayed history: the first item must be the live fact below.
    let fact = fixture.metadata_fact("m2");
    let published = fixture.append_and_publish(&hub, fact);
    let item = tokio::time::timeout(std::time::Duration::from_secs(2), subscription.next())
        .await
        .expect("live event must arrive");
    match item {
        V2StreamItem::Event(event) => {
            assert_eq!(fact_seq(&event), published.fact_seq);
            assert_eq!(event.delivery, RingingV2Delivery::Reliable);
            let cursor = event.cursor_value().expect("valid").expect("reliable");
            assert_eq!(cursor.log_id, fixture.identity.log_id.as_str());
            assert_eq!(cursor.fact_seq, published.fact_seq);
        }
        other => panic!("expected live event, got {other:?}"),
    }
}

/// V2-C2: reconnecting from an older cursor replays only the facts committed
/// after it, in strict `(fact_seq, projection_index)` order.
#[tokio::test]
async fn v2_c2_reliable_reconnect_replays_only_after_cursor() {
    let mut fixture = Fixture::new("v2-c2");
    fixture.created();
    let second = fixture.metadata_fact("m2");
    fixture.append(second);
    let hub = V2ProjectionHub::new("epoch-c2");

    // Snapshot cursor pinned *before* the third fact.
    let bootstrap = hub.bootstrap(fixture.path(), "seed").expect("bootstrap");
    let resumed_after = bootstrap.last_fact_seq;
    let fact = fixture.metadata_fact("m3");
    let published = fixture.append_and_publish(&hub, fact);

    let mut subscription = hub
        .subscribe(
            fixture.path(),
            "seed",
            RingingChannel::Control,
            Some(&bootstrap.snapshot_cursor),
        )
        .expect("subscribe");

    // The replay queue holds exactly the post-cursor fact; the next `next()`
    // would block on live traffic, so bound the read.
    let first = tokio::time::timeout(std::time::Duration::from_secs(2), subscription.next())
        .await
        .expect("replay must be queued, not blocked on live traffic");
    let seen = match first {
        V2StreamItem::Event(event) => event,
        other => panic!("expected replayed event, got {other:?}"),
    };
    assert_eq!(fact_seq(&seen), published.fact_seq);
    assert!(
        fact_seq(&seen) > resumed_after,
        "no event at or before the cursor may replay"
    );
}

/// V2-C4: an ephemeral envelope carries no cursor and never advances canonical
/// state, so it can never be replayed. (The projection path has no ephemeral
/// producer yet; this pins the wire contract.)
#[test]
fn v2_c4_ephemeral_is_cursorless_and_unreplayable() {
    let ephemeral = RingingV2EventEnvelope::<qaqh_session::session_fact_v2::ProjectionPayload> {
        schema: qaqh_ringing::RINGING_SCHEMA.into(),
        version: qaqh_ringing::RINGING_V2_VERSION,
        server_epoch: "epoch".into(),
        seed: "seed".into(),
        event_id: "evt-ephemeral".into(),
        stream_key: qaqh_ringing::RingingV2StreamKey::Channel(RingingChannel::Conversation),
        delivery: RingingV2Delivery::Ephemeral,
        cursor: None,
        log_id: None,
        fact_seq: None,
        projection_index: None,
        revision: None,
        causation_id: None,
        correlation_id: None,
        payload: qaqh_session::session_fact_v2::ProjectionPayload::Unknown(
            qaqh_session::session_fact_v2::UnknownProjection {
                raw_ref: qaqh_session::session_fact_v2::ContentRef::new(
                    qaqh_session::canonical::sha256_content_hash(b"ephemeral"),
                ),
            },
        ),
    };
    ephemeral.validate().expect("ephemeral envelope is valid");
    assert_eq!(ephemeral.cursor_value().expect("cursor lookup"), None);

    // An ephemeral envelope that smuggles a cursor must fail validation.
    let mut poisoned = ephemeral.clone();
    poisoned.cursor = Some(CursorToken::from_opaque("v2.garbage"));
    assert!(poisoned.validate().is_err());
}

/// V2-C5: a cursor minted for a different canonical log forces a rebaseline.
#[tokio::test]
async fn v2_c5_log_id_mismatch_requires_reset() {
    let mut fixture = Fixture::new("v2-c5");
    fixture.created();
    let hub = V2ProjectionHub::new("epoch-c5");

    let foreign = CanonicalCursor::snapshot("0198f1a0-0000-7000-8000-0000000000ff", 1);
    let token = CursorToken::encode_snapshot(&foreign).expect("token");
    let mut subscription = hub
        .subscribe(
            fixture.path(),
            "seed",
            RingingChannel::Control,
            Some(&token),
        )
        .expect("subscribe");
    match subscription.next().await {
        V2StreamItem::Reset(reset) => {
            assert_eq!(reset.reason, RingingV2ResetReason::LogIdMismatch);
            assert_eq!(reset.seed, "seed");
            assert!(
                reset.snapshot_cursor.is_some(),
                "reset must hand back a usable rebaseline cursor"
            );
        }
        other => panic!("expected log_id reset, got {other:?}"),
    }
}

/// V2-C6: a cursor ahead of the committed log is `unknown_fact`; an
/// undecodable cursor is rejected as an invalid (expired) cursor.
#[tokio::test]
async fn v2_c6_cursor_expired_and_unknown_fact() {
    let mut fixture = Fixture::new("v2-c6");
    fixture.created();
    let hub = V2ProjectionHub::new("epoch-c6");

    let ahead = CanonicalCursor::snapshot(fixture.identity.log_id.as_str(), 99);
    let ahead_token = CursorToken::encode_snapshot(&ahead).expect("token");
    let mut subscription = hub
        .subscribe(
            fixture.path(),
            "seed",
            RingingChannel::Control,
            Some(&ahead_token),
        )
        .expect("subscribe");
    match subscription.next().await {
        V2StreamItem::Reset(reset) => {
            assert_eq!(reset.reason, RingingV2ResetReason::UnknownFact);
            assert!(reset.snapshot_cursor.is_some());
        }
        other => panic!("expected unknown_fact reset, got {other:?}"),
    }

    let garbage = CursorToken::from_opaque("v2.not-a-real-cursor");
    match hub.subscribe(
        fixture.path(),
        "seed",
        RingingChannel::Control,
        Some(&garbage),
    ) {
        Err(V2HubError::InvalidCursor(_)) => {}
        Err(other) => panic!("expected invalid cursor, got {other:?}"),
        Ok(_) => panic!("expected invalid cursor, got a live subscription"),
    }
}

/// V2-C7: a session with no committed facts cannot mint a snapshot cursor.
#[test]
fn v2_c7_snapshot_missing_is_reported() {
    let dir = tempfile::tempdir().expect("tempdir");
    CanonicalSessionIdentity::open_or_create(dir.path()).expect("identity");
    let hub = V2ProjectionHub::new("epoch-c7");
    match hub.bootstrap(dir.path(), "seed") {
        Err(V2HubError::SnapshotMissing(seed)) => assert_eq!(seed, "seed"),
        other => panic!("expected snapshot_missing, got {other:?}"),
    }
}

/// Live overflow: a subscriber that falls behind the broadcast buffer gets a
/// `replay_overflow` reset instead of silently skipping events.
#[tokio::test]
async fn v2_live_overflow_signals_replay_overflow() {
    let mut fixture = Fixture::new("v2-overflow");
    fixture.created();
    let hub = V2ProjectionHub::with_live_capacity("epoch-overflow", 2);

    // Warm the session so `subscribe` and `publish` share one live channel.
    hub.bootstrap(fixture.path(), "seed").expect("bootstrap");
    let mut subscription = hub
        .subscribe(fixture.path(), "seed", RingingChannel::Control, None)
        .expect("subscribe");

    for i in 0..6 {
        let fact = fixture.metadata_fact(&format!("m{i}"));
        fixture.append_and_publish(&hub, fact);
    }

    let item = tokio::time::timeout(std::time::Duration::from_secs(2), subscription.next())
        .await
        .expect("overflow reset must arrive");
    match item {
        V2StreamItem::Reset(reset) => {
            assert_eq!(reset.reason, RingingV2ResetReason::ReplayOverflow);
        }
        other => panic!("expected replay_overflow reset, got {other:?}"),
    }
}
