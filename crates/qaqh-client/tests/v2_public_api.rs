//! Public v2 API compile checks for TUI/Windows shells.
//!
//! Shells must be able to name the v2 control surface through `qaqh-client`
//! without importing `qaqh-ringing`, `qaqh-domain` or `qaqh-session`.

use qaqh_client::{
    ClientError, ClientV2ActorRef, ClientV2AskOutcome, ClientV2Bootstrap, ClientV2CommandAck,
    ClientV2CommandResult, ClientV2CommandStatus, ClientV2ContentValue, ClientV2ControlDelta,
    ClientV2ControlState, ClientV2ConversationState, ClientV2Cursor, ClientV2CursorToken,
    ClientV2DeltaInteractionKind, ClientV2Event, ClientV2EventEnvelope, ClientV2ExistingResult,
    ClientV2InteractionDecision, ClientV2InteractionExpiryReason, ClientV2InteractionId,
    ClientV2Payload, ClientV2Reset, ClientV2ResetReason, ClientV2SessionState,
    ClientV2Subscription, ClientV2SubscriptionEvent, ClientV2TaskBoardSnapshot,
    ClientV2TeamAgentResidency, ClientV2TeamAgentSnapshot, ClientV2TeamAgentStatus,
    ClientV2TeamBoardSnapshot, ClientV2TeamDelivery, ClientV2TeamDelta, ClientV2TeamInboxSummary,
    ClientV2TeamResponse, ClientV2TeamSnapshot, ClientV2TeamTaskSnapshot, ClientV2ToolCallId,
    ClientV2ToolState, RINGING_V2_BASE_PATH, RINGING_V2_VERSION,
};

#[test]
fn v2_surface_is_nameable_from_the_client_root() {
    let cursor = ClientV2Cursor::snapshot("log-1", 7);
    let token = ClientV2CursorToken::encode_snapshot(&cursor).expect("cursor token");
    assert_eq!(token.decode_snapshot().expect("decoded cursor"), cursor);

    let _: Option<ClientV2Bootstrap> = None;
    let _: Option<ClientV2ControlState> = None;
    let _: Option<ClientV2ConversationState> = None;
    let _: Option<ClientV2ToolState> = None;
    let _: Option<ClientV2Event> = None;
    let _: Option<ClientV2EventEnvelope> = None;
    let _: Option<ClientV2Payload> = None;
    let _: Option<ClientV2Reset> = None;
    let _: Option<ClientV2SessionState> = None;
    let _: Option<ClientV2Subscription> = None;
    let _: Option<ClientV2SubscriptionEvent> = None;

    let result = ClientV2CommandResult::AskResolved {
        interaction_id: "i1".into(),
        outcome: ClientV2AskOutcome::Answered,
    };
    let _: Option<ClientV2CommandAck> = None;
    let _: Option<ClientV2CommandStatus> = None;
    let _: Option<ClientV2ExistingResult> = None;
    assert_eq!(
        serde_json::to_string(&result).expect("json"),
        r#"{"kind":"ask_resolved","interaction_id":"i1","outcome":"answered"}"#
    );

    assert_eq!(RINGING_V2_VERSION, 2);
    assert_eq!(RINGING_V2_BASE_PATH, "/ringing/v2");
    assert_eq!(
        serde_json::to_string(&ClientV2ResetReason::SnapshotMissing).expect("json"),
        "\"snapshot_missing\""
    );
}

/// #323 缺口 1：壳层必须能只靠 `qaqh-client` **命名并 match** control delta 的
/// interaction 分支（match 枚举变体必须写出枚举名，只导出 `ClientV2Payload` 不够）。
#[test]
fn control_delta_interaction_branches_are_matchable_from_the_client_root() {
    fn classify(delta: &ClientV2ControlDelta) -> &'static str {
        match delta {
            ClientV2ControlDelta::InteractionRequested {
                interaction_id,
                call_id,
                kind,
                request,
                expires_at_ms,
                ..
            } => {
                // 字段必须能用 client 导出的类型命名。
                let _: &ClientV2InteractionId = interaction_id;
                let _: &Option<ClientV2ToolCallId> = call_id;
                let _: &ClientV2DeltaInteractionKind = kind;
                let _: &ClientV2ContentValue = request;
                let _: &Option<i64> = expires_at_ms;
                "requested"
            }
            ClientV2ControlDelta::InteractionResolved {
                interaction_id,
                decision,
                verdict,
                resolved_by,
                ..
            } => {
                let _: &ClientV2InteractionId = interaction_id;
                let _: &ClientV2ContentValue = decision;
                let _: &Option<ClientV2InteractionDecision> = verdict;
                let _: &ClientV2ActorRef = resolved_by;
                "resolved"
            }
            ClientV2ControlDelta::InteractionExpired {
                interaction_id,
                reason,
                ..
            } => {
                let _: &ClientV2InteractionId = interaction_id;
                let _: &ClientV2InteractionExpiryReason = reason;
                "expired"
            }
            ClientV2ControlDelta::DriverChanged {
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

    let requested: ClientV2Payload = serde_json::from_value(serde_json::json!({
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
    let ClientV2Payload::ControlDelta(delta) = &requested else {
        panic!("expected control_delta, got {requested:?}");
    };
    assert_eq!(classify(delta), "requested");

    // wire 事实：delta 与 bootstrap 的 plan kind 都已统一为 `plan`；两个
    // Rust 枚举都在 client 面可命名，壳层按类型匹配即可。
    let resolved: ClientV2Payload = serde_json::from_value(serde_json::json!({
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
    let ClientV2Payload::ControlDelta(delta) = &resolved else {
        panic!("expected control_delta, got {resolved:?}");
    };
    assert_eq!(classify(delta), "resolved");

    let expired: ClientV2Payload = serde_json::from_value(serde_json::json!({
        "kind": "control_delta",
        "data": {
            "kind": "interaction_expired",
            "data": { "revision": 5, "interaction_id": "int_1", "reason": "timeout" }
        }
    }))
    .expect("decode interaction_expired payload");
    let ClientV2Payload::ControlDelta(delta) = &expired else {
        panic!("expected control_delta, got {expired:?}");
    };
    assert_eq!(classify(delta), "expired");
}

/// Phase 4（roster / inbox）：壳层必须能只靠 `qaqh-client` **命名并 match**
/// Team projection。
///
/// `TeamSnapshot/TeamDelta` 是 TUI/WinUI 的唯一 roster/inbox 投影
/// （`docs/current/architecture.md`），而壳层不能依赖 `qaqh-session`
/// （静态门禁 G1）。只导出 `ClientV2Payload` 不够——match 枚举变体必须写出
/// 枚举名，所以这些类型必须从 client 根导出。
#[test]
fn team_projection_is_consumable_from_the_client_root() {
    // 快照面：`Client::team_v2` 的返回类型 + roster / inbox / board 各层类型。
    let _: Option<ClientV2TeamResponse> = None;
    let _: Option<ClientV2TeamSnapshot> = None;
    let _: Option<ClientV2TeamAgentSnapshot> = None;
    let _: Option<ClientV2TeamAgentStatus> = None;
    let _: Option<ClientV2TeamAgentResidency> = None;
    let _: Option<ClientV2TeamInboxSummary> = None;
    let _: Option<ClientV2TaskBoardSnapshot> = None;
    let _: Option<ClientV2TeamTaskSnapshot> = None;
    let _: Option<ClientV2TeamBoardSnapshot> = None;

    fn classify(delta: &ClientV2TeamDelta) -> &'static str {
        match delta {
            ClientV2TeamDelta::AgentJoined { agent, .. } => {
                let _: &ClientV2TeamAgentSnapshot = agent;
                // roster 以 AgentPath 为主、nickname 为辅。
                let _: &str = agent.agent_path.as_str();
                let _: &Option<String> = &agent.nickname;
                "joined"
            }
            ClientV2TeamDelta::AgentStatusChanged { status, .. } => {
                let _: &ClientV2TeamAgentStatus = status;
                "status"
            }
            ClientV2TeamDelta::AgentResidencyChanged { residency, .. } => {
                // unloaded 是 residency，不是 status；前端不能画成 deleted。
                let _: &ClientV2TeamAgentResidency = residency;
                "residency"
            }
            ClientV2TeamDelta::AgentMessageQueued { message, .. } => {
                let _: &ClientV2TeamInboxSummary = message;
                let _: &str = message.author.as_str();
                let _: &str = message.recipient.as_str();
                let _: &Option<String> = &message.task_id;
                let _: ClientV2TeamDelivery = message.delivery;
                "queued"
            }
            ClientV2TeamDelta::AgentMessageDelivered { message_id, .. } => {
                let _: &str = message_id.as_str();
                "delivered"
            }
            ClientV2TeamDelta::AgentInterrupted { .. } => "interrupted",
            ClientV2TeamDelta::AgentCompleted { status, .. } => {
                let _: &ClientV2TeamAgentStatus = status;
                "completed"
            }
            ClientV2TeamDelta::TaskChanged { task, .. } => {
                let _: &ClientV2TeamTaskSnapshot = task;
                "task"
            }
            ClientV2TeamDelta::BoardChanged { board, .. } => {
                let _: &ClientV2TeamBoardSnapshot = board;
                "board"
            }
        }
    }

    let payload: ClientV2Payload = serde_json::from_value(serde_json::json!({
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
    let ClientV2Payload::TeamDelta(delta) = &payload else {
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
