//! Public v2 API compile checks for TUI/Windows shells.
//! Shells must be able to name the v2 control surface through `qaqh-client`
//! without importing `qaqh-ringing`, `qaqh-domain` or `qaqh-session`.

use qaqh_client::{
    ClientError, ClientV2Bootstrap, ClientV2Event, ClientV2EventEnvelope, ClientV2ResyncRequired,
    ClientV2SessionState, ClientV2Subscription, ClientV2SubscriptionEvent, RINGING_V2_BASE_PATH,
    RINGING_V2_VERSION,
};

use qaqh_client::{fact, ringing_v2};

#[test]
fn v2_surface_is_nameable_from_the_client_root() {
    let _: Option<ClientV2Bootstrap> = None;
    let _: Option<ClientV2Event> = None;
    let _: Option<ClientV2EventEnvelope> = None;
    let _: Option<ClientV2ResyncRequired> = None;
    let _: Option<ClientV2SessionState> = None;
    let _: Option<ClientV2Subscription> = None;
    let _: Option<ClientV2SubscriptionEvent> = None;

    let result = ringing_v2::RingingV2CommandResult::AskResolved {
        interaction_id: "i1".into(),
        outcome: ringing_v2::RingingV2AskOutcome::Answered,
    };
    let _: Option<ringing_v2::RingingV2CommandAck> = None;
    let _: Option<ringing_v2::RingingV2CommandStatus> = None;
    let _: Option<ringing_v2::RingingV2ExistingResult> = None;
    assert_eq!(
        serde_json::to_string(&result).expect("json"),
        r#"{"kind":"ask_resolved","interaction_id":"i1","outcome":"answered"}"#
    );

    assert_eq!(RINGING_V2_VERSION, 2);
    assert_eq!(RINGING_V2_BASE_PATH, "/ringing/v2");
    assert_eq!(
        serde_json::to_string(&ringing_v2::RingingV2ResetReason::SnapshotMissing).expect("json"),
        "\"snapshot_missing\""
    );
}

/// #323 缺口 1：壳层必须能只靠 `qaqh-client` **命名并 match** control delta 的
/// interaction 分支（match 枚举变体必须写出枚举名，只导出 `fact::ProjectionPayload` 不够）。
#[test]
fn control_delta_interaction_branches_are_matchable_from_the_client_root() {
    fn classify(delta: &fact::ControlDelta) -> &'static str {
        match delta {
            fact::ControlDelta::InteractionRequested {
                interaction_id,
                call_id,
                kind,
                request,
                expires_at_ms,
                ..
            } => {
                // 字段必须能用 client 导出的类型命名。
                let _: &fact::InteractionId = interaction_id;
                let _: &Option<fact::ToolCallId> = call_id;
                let _: &fact::InteractionKind = kind;
                let _: &fact::ContentValue = request;
                let _: &Option<i64> = expires_at_ms;
                "requested"
            }
            fact::ControlDelta::InteractionResolved {
                interaction_id,
                decision,
                verdict,
                resolved_by,
                ..
            } => {
                let _: &fact::InteractionId = interaction_id;
                let _: &fact::ContentValue = decision;
                let _: &Option<fact::InteractionDecision> = verdict;
                let _: &fact::ActorRef = resolved_by;
                "resolved"
            }
            fact::ControlDelta::InteractionExpired {
                interaction_id,
                reason,
                ..
            } => {
                let _: &fact::InteractionId = interaction_id;
                let _: &fact::InteractionExpiryReason = reason;
                "expired"
            }
            fact::ControlDelta::DriverChanged {
                holder,
                driver_epoch,
                ..
            } => {
                let _: &Option<String> = holder;
                let _: &u64 = driver_epoch;
                "driver"
            }
            _ => "other",
        }
    }

    let requested: fact::ProjectionPayload = serde_json::from_value(serde_json::json!({
        "kind": "control_delta",
        "data": {
            "kind": "interaction_requested",
            "data": {
                "revision": 3,
                "interaction_id": "int_1",
                "call_id": "call_1",
                "kind": "plan",
                "request": { "kind": "inline", "data": { "text": "approve?" } },
                "expires_at_ms": null
            }
        }
    }))
    .expect("decode interaction_requested payload");
    let fact::ProjectionPayload::ControlDelta(delta) = &requested else {
        panic!("expected control_delta, got {requested:?}");
    };
    assert_eq!(classify(delta), "requested");

    // wire 事实：delta 与 bootstrap 的 plan kind 都已统一为 `plan`；两个
    // Rust 枚举都在 client 面可命名，壳层按类型匹配即可。
    let resolved: fact::ProjectionPayload = serde_json::from_value(serde_json::json!({
        "kind": "control_delta",
        "data": {
            "kind": "interaction_resolved",
            "data": {
                "revision": 4,
                "interaction_id": "int_1",
                "decision": { "kind": "inline", "data": { "text": "approved" } },
                "verdict": "approved",
                "resolved_by": { "kind": "user", "id": "local" },
                "resolution_seq": 1
            }
        }
    }))
    .expect("decode interaction_resolved payload");
    let fact::ProjectionPayload::ControlDelta(delta) = &resolved else {
        panic!("expected control_delta, got {resolved:?}");
    };
    assert_eq!(classify(delta), "resolved");

    let expired: fact::ProjectionPayload = serde_json::from_value(serde_json::json!({
        "kind": "control_delta",
        "data": {
            "kind": "interaction_expired",
            "data": { "revision": 5, "interaction_id": "int_1", "reason": "timeout" }
        }
    }))
    .expect("decode interaction_expired payload");
    let fact::ProjectionPayload::ControlDelta(delta) = &expired else {
        panic!("expected control_delta, got {expired:?}");
    };
    assert_eq!(classify(delta), "expired");
}

#[test]
fn server_error_codes_are_available_to_shells() {
    let error = ClientError::Api {
        status: 409,
        code: "interaction_already_resolved".into(),
        message: "already resolved".into(),
    };
    assert_eq!(error.code(), Some("interaction_already_resolved"));
}
