//! Public v2 API compile checks for TUI/Windows shells.
//! Shells must be able to name the v2 control surface through `qaqh-client`
//! without importing `qaqh-ringing`, `qaqh-domain` or `qaqh-session`.

use qaqh_client::{
    ClientError, fact::ActorRef, ringing_v2::RingingV2AskOutcome, ClientV2Bootstrap, ringing_v2::RingingV2CommandAck,
    ringing_v2::RingingV2CommandResult, ringing_v2::RingingV2CommandStatus, fact::ContentValue, fact::ControlDelta,
    ClientV2ControlState, ClientV2ConversationState, ringing::CanonicalCursor, ringing::CursorToken,
    fact::InteractionKind, ClientV2Event, ClientV2EventEnvelope, ringing_v2::RingingV2ExistingResult,
    fact::InteractionDecision, fact::InteractionExpiryReason, fact::InteractionId,
    fact::ProjectionPayload, ringing_v2::RingingV2ResetRequired, ringing_v2::RingingV2ResetReason, ClientV2SessionState,
    ClientV2Subscription, ClientV2SubscriptionEvent, team::TaskBoardSnapshot,
    fact::TeamAgentResidency, fact::TeamAgentSnapshot, fact::TeamAgentStatus,
    fact::TeamBoardSnapshot, fact::InterAgentDelivery, fact::TeamDelta, fact::TeamInboxSummary,
    ClientV2TeamResponse, session_projection::TeamSnapshot, fact::TeamTaskSnapshot, fact::ToolCallId,
    ClientV2ToolState, RINGING_V2_BASE_PATH, RINGING_V2_VERSION,
};

use qaqh_client::{fact, ringing, ringing_v2, session_projection, team};

#[test]
fn v2_surface_is_nameable_from_the_client_root() {
    let cursor = ringing::CanonicalCursor::snapshot("log-1", 7);
    let token = ringing::CursorToken::encode_snapshot(&cursor).expect("cursor token");
    assert_eq!(token.decode_snapshot().expect("decoded cursor"), cursor);

    let _: Option<ClientV2Bootstrap> = None;
    let _: Option<ClientV2ControlState> = None;
    let _: Option<ClientV2ConversationState> = None;
    let _: Option<ClientV2ToolState> = None;
    let _: Option<ClientV2Event> = None;
    let _: Option<ClientV2EventEnvelope> = None;
    let _: Option<fact::ProjectionPayload> = None;
    let _: Option<ringing_v2::RingingV2ResetRequired> = None;
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

/// Phase 4（roster / inbox）：壳层必须能只靠 `qaqh-client` **命名并 match**
/// Team projection。
/// `TeamSnapshot/TeamDelta` 是 TUI/WinUI 的唯一 roster/inbox 投影
/// ，而壳层不能依赖 `qaqh-session`
/// （静态门禁 G1）。只导出 `fact::ProjectionPayload` 不够——match 枚举变体必须写出
/// 枚举名，所以这些类型必须从 client 根导出。
#[test]
fn team_projection_is_consumable_from_the_client_root() {
    // 快照面：`Client::team_v2` 的返回类型 + roster / inbox / board 各层类型。
    let _: Option<ClientV2TeamResponse> = None;
    let _: Option<session_projection::TeamSnapshot> = None;
    let _: Option<fact::TeamAgentSnapshot> = None;
    let _: Option<fact::TeamAgentStatus> = None;
    let _: Option<fact::TeamAgentResidency> = None;
    let _: Option<fact::TeamInboxSummary> = None;
    let _: Option<team::TaskBoardSnapshot> = None;
    let _: Option<fact::TeamTaskSnapshot> = None;
    let _: Option<fact::TeamBoardSnapshot> = None;

    fn classify(delta: &fact::TeamDelta) -> &'static str {
        match delta {
            fact::TeamDelta::AgentJoined { agent, .. } => {
                let _: &fact::TeamAgentSnapshot = agent;
                // roster 以 AgentPath 为主、nickname 为辅。
                let _: &str = agent.agent_path.as_str();
                let _: &Option<String> = &agent.nickname;
                "joined"
            }
            fact::TeamDelta::AgentStatusChanged { status, .. } => {
                let _: &fact::TeamAgentStatus = status;
                "status"
            }
            fact::TeamDelta::AgentResidencyChanged { residency, .. } => {
                // unloaded 是 residency，不是 status；前端不能画成 deleted。
                let _: &fact::TeamAgentResidency = residency;
                "residency"
            }
            fact::TeamDelta::AgentMessageQueued { message, .. } => {
                let _: &fact::TeamInboxSummary = message;
                let _: &str = message.author.as_str();
                let _: &str = message.recipient.as_str();
                let _: &Option<String> = &message.task_id;
                let _: fact::InterAgentDelivery = message.delivery;
                "queued"
            }
            fact::TeamDelta::AgentMessageDelivered { message_id, .. } => {
                let _: &str = message_id.as_str();
                "delivered"
            }
            fact::TeamDelta::AgentInterrupted { .. } => "interrupted",
            fact::TeamDelta::AgentCompleted { status, .. } => {
                let _: &fact::TeamAgentStatus = status;
                "completed"
            }
            fact::TeamDelta::TaskChanged { task, .. } => {
                let _: &fact::TeamTaskSnapshot = task;
                "task"
            }
            fact::TeamDelta::BoardChanged { board, .. } => {
                let _: &fact::TeamBoardSnapshot = board;
                "board"
            }
        }
    }

    let payload: fact::ProjectionPayload = serde_json::from_value(serde_json::json!({
        "kind": "team_delta",
        "data": {
            "kind": "agent_residency_changed",
            "data": {
                "revision": 9,
                "agent_id": "0199a0f0-0000-7000-8000-000000000002",
                "residency": "unloaded"
            }
        }
    }))
    .expect("decode team_delta payload");
    let fact::ProjectionPayload::TeamDelta(delta) = &payload else {
        panic!("expected team_delta, got {payload:?}");
    };
    assert_eq!(classify(delta), "residency");
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
