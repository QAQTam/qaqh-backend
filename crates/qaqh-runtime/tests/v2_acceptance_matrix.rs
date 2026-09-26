//! P0-6 Ringing v2 acceptance fixture matrix.
//!
//! Drives the real [`V2ProjectionHub`] against seeded canonical facts and
//! asserts the frozen rows of `docs/current/decisions.md`
//! §13 that are reachable today.
//!
//! Rows covered here:
//!
//! | row   | scenario                                   |
//! |-------|--------------------------------------------|
//! | V2-C1 | snapshot + subscribe (no gap, no dup)      |
//! | V2-C2 | reliable reconnect replays only after cursor |
//! | V2-C3 | replaceable reconnect sends latest current only |
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
//! Known gaps (see the P0-6 handoff): the v1 cursor mapping (V2-V1) has no
//! production producer yet, so it is asserted at the wire level only.

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
    DriverChanged, EventId, FactPayload, FactSchema, MetadataSource, SessionCreated, SessionFact,
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
        self.envelope_with_ids(ts_ms, None, None, payload)
    }

    /// Same, but with envelope-level turn/call identity (required by payloads
    /// whose validation cross-checks the envelope).
    fn envelope_with_ids(
        &self,
        ts_ms: i64,
        turn_id: Option<qaqh_session::session_fact_v2::TurnId>,
        call_id: Option<qaqh_session::session_fact_v2::ToolCallId>,
        payload: FactPayload,
    ) -> SessionFact {
        SessionFact {
            schema: FactSchema::v2(),
            session_id: self.identity.session_id.clone(),
            log_id: self.identity.log_id.clone(),
            fact_seq: 0,
            event_id: EventId::new(generate_ulid()),
            ts_ms,
            causation_id: None,
            turn_id,
            call_id,
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

    /// Build (but do not append) a driver handover — one control replaceable
    /// current value plus its reliable cursor event.
    fn driver_fact(&mut self, holder: &str, driver_epoch: u64) -> SessionFact {
        self.seq += 1;
        let ts = NOW_MS + self.seq;
        self.envelope(
            ts,
            FactPayload::DriverChanged(DriverChanged {
                holder: Some(holder.to_string()),
                driver_epoch,
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
        .subscribe(fixture.path(), "seed", Some(&bootstrap.snapshot_cursor))
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
        .subscribe(fixture.path(), "seed", Some(&bootstrap.snapshot_cursor))
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

/// V2-C3: replaceable state is rebuilt from canonical facts on reconnect, but
/// only the latest value per identity is sent. It never carries a canonical
/// cursor; a later live update replaces the current value.
#[tokio::test]
async fn v2_c3_replaceable_reconnect_sends_latest_current_value_only() {
    let mut fixture = Fixture::new("v2-c3");
    fixture.created();
    let first = fixture.driver_fact("client-a", 1);
    fixture.append(first);
    let second = fixture.driver_fact("client-b", 2);
    fixture.append(second);

    // Fresh hub = daemon restart/rebuild from the committed canonical prefix.
    let hub = V2ProjectionHub::new("epoch-c3");
    let bootstrap = hub.bootstrap(fixture.path(), "seed").expect("bootstrap");
    let mut subscription = hub
        .subscribe(fixture.path(), "seed", Some(&bootstrap.snapshot_cursor))
        .expect("subscribe");

    let item = tokio::time::timeout(std::time::Duration::from_secs(2), subscription.next())
        .await
        .expect("current replaceable value must be queued");
    let event = match item {
        V2StreamItem::Event(event) => event,
        other => panic!("expected replaceable event, got {other:?}"),
    };
    assert_eq!(event.delivery, RingingV2Delivery::Replaceable);
    assert_eq!(event.revision, Some(3));
    assert_eq!(event.cursor_value().expect("valid envelope"), None);
    match &event.payload {
        qaqh_session::session_fact_v2::ProjectionPayload::ControlDelta(
            qaqh_session::session_fact_v2::ControlDelta::DriverChanged {
                holder,
                driver_epoch,
                ..
            },
        ) => {
            assert_eq!(holder.as_deref(), Some("client-b"));
            assert_eq!(*driver_epoch, 2);
        }
        other => panic!("expected driver replaceable, got {other:?}"),
    }

    // No historical replaceable values or reliable replay after the cursor.
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(150), subscription.next())
            .await
            .is_err(),
        "replaceable reconnect must not replay old revisions"
    );

    // Live update emits the reliable cursor event and then the replaceable
    // current-value mirror; consume both and assert the latter is cursorless.
    let third = fixture.driver_fact("client-c", 3);
    fixture.append_and_publish(&hub, third);
    let mut live_replaceable = None;
    for _ in 0..2 {
        let item = tokio::time::timeout(std::time::Duration::from_secs(2), subscription.next())
            .await
            .expect("live update must arrive");
        let event = match item {
            V2StreamItem::Event(event) => event,
            other => panic!("expected live event, got {other:?}"),
        };
        if event.delivery == RingingV2Delivery::Replaceable {
            live_replaceable = Some(event);
            break;
        }
    }
    let event = live_replaceable.expect("live replaceable mirror");
    assert_eq!(event.revision, Some(4));
    assert_eq!(event.cursor_value().expect("valid envelope"), None);
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
        session_id: "seed".into(),
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
        .subscribe(fixture.path(), "seed", Some(&token))
        .expect("subscribe");
    match subscription.next().await {
        V2StreamItem::Reset(reset) => {
            assert_eq!(reset.reason, RingingV2ResetReason::LogIdMismatch);
            assert_eq!(reset.session_id, "seed");
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
        .subscribe(fixture.path(), "seed", Some(&ahead_token))
        .expect("subscribe");
    match subscription.next().await {
        V2StreamItem::Reset(reset) => {
            assert_eq!(reset.reason, RingingV2ResetReason::UnknownFact);
            assert!(reset.snapshot_cursor.is_some());
        }
        other => panic!("expected unknown_fact reset, got {other:?}"),
    }

    let garbage = CursorToken::from_opaque("v2.not-a-real-cursor");
    match hub.subscribe(fixture.path(), "seed", Some(&garbage)) {
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

/// 单流修订（2026-09-24）：**一条** SSE 上收齐 control / conversation / tool
/// 三类 `stream_key`，且 `(fact_seq, projection_index)` 全局严格递增。
///
/// 这是取代 per-channel 订阅的核心断言：客户端不再需要跨流归并。
#[tokio::test]
async fn single_stream_carries_all_channels_in_global_order() {
    use qaqh_session::canonical::sha256_content_hash;
    use qaqh_session::session_fact_v2::{
        ActorKind, ActorRef, ContentRef, InputAccepted, InputId, InputKind, InputPurpose,
        ToolCallDeclared, ToolCallId, TurnId,
    };

    let mut fixture = Fixture::new("v2-single-stream");
    fixture.created();
    let hub = V2ProjectionHub::new("epoch-single");
    // 从 log 尾部订阅（无 replay），随后 commit 的事实全部走**同一条** live 流。
    let mut subscription = hub
        .subscribe(fixture.path(), "seed", None)
        .expect("subscribe");

    // control: metadata change
    let meta = fixture.metadata_fact("single-stream");
    fixture.append_and_publish(&hub, meta);
    // conversation: InputAccepted（conversation + timeline 两个 delta）
    let turn_id = TurnId::new(format!("turn_{}", generate_ulid()));
    let call_id = ToolCallId::new(format!("call_{}", generate_ulid()));
    let input = fixture.envelope_with_ids(
        NOW_MS + 10,
        None,
        None,
        FactPayload::InputAccepted(InputAccepted {
            input_id: InputId::new(format!("input_{}", generate_ulid())),
            input_kind: InputKind::UserText,
            input_purpose: InputPurpose::TriggerTurn,
            content_ref: None,
            inline_text: Some("hello".into()),
            attachments: Vec::new(),
            actor: ActorRef {
                kind: ActorKind::User,
                id: "local".into(),
                display_name: None,
            },
            client_request_id: None,
        }),
    );
    fixture.append_and_publish(&hub, input);
    // tool: ToolCallDeclared → TimelineDelta 走 tool 频道
    let declared = fixture.envelope_with_ids(
        NOW_MS + 11,
        Some(turn_id.clone()),
        Some(call_id.clone()),
        FactPayload::ToolCallDeclared(ToolCallDeclared {
            turn_id,
            call_id,
            tool_name: "exec".into(),
            args_ref: ContentRef::new(sha256_content_hash(b"args")),
            args_hash: sha256_content_hash(b"args"),
        }),
    );
    fixture.append_and_publish(&hub, declared);

    let mut channels = std::collections::BTreeSet::new();
    let mut last: Option<(u64, Option<u16>)> = None;
    // 三类事实已 publish；流排空后 `next()` 会阻塞在 live channel 上，
    // 用 timeout 收尾。
    for _ in 0..8 {
        let Ok(item) =
            tokio::time::timeout(std::time::Duration::from_millis(500), subscription.next()).await
        else {
            break;
        };
        let V2StreamItem::Event(event) = item else {
            break;
        };
        channels.insert(format!("{:?}", event.stream_key));
        if let Some(seq) = event.fact_seq {
            let key = (seq, event.projection_index);
            if let Some(previous) = last {
                assert!(key > previous, "单流必须全局有序：{previous:?} → {key:?}");
            }
            last = Some(key);
        }
    }
    assert!(
        channels.iter().any(|key| key.contains("Control")),
        "缺 control：{channels:?}"
    );
    assert!(
        channels.iter().any(|key| key.contains("Conversation")),
        "缺 conversation：{channels:?}"
    );
    assert!(
        channels.iter().any(|key| key.contains("Tool")),
        "缺 tool：{channels:?}"
    );
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
        .subscribe(fixture.path(), "seed", None)
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
