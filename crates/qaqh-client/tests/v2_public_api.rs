//! Public v2 API compile checks for TUI/Windows shells.
//! Shells must be able to name the v2 control surface through `qaqh-client`
//! without importing `qaqh-ringing`, `qaqh-domain` or `qaqh-session`.

use qaqh_client::{
    ClientError, ClientV2Bootstrap, ClientV2Event, ClientV2ResyncRequired, ClientV2SessionState,
    ClientV2Subscription, ClientV2SubscriptionEvent, RINGING_V2_BASE_PATH, RINGING_V2_VERSION,
};

use qaqh_client::{contract_state, fact, ringing_v2};

#[test]
fn v2_surface_is_nameable_from_the_client_root() {
    let _: Option<ClientV2Bootstrap> = None;
    let _: Option<ClientV2Event> = None;
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
}

/// 壳层必须能只靠 `qaqh-client` **命名并 match** 会话事件流的 interaction /
/// tool 分支(`ClientV2Event` = 契约 `SessionEventFrame`,事件内部 tagged
/// `type`)。match 枚举变体必须写出枚举名,别名导出不够。
#[test]
fn session_event_interaction_branches_are_matchable_from_the_client_root() {
    use qaqh_client::contract_events::SessionEvent;

    fn classify(frame: &ClientV2Event) -> &'static str {
        match &frame.event {
            SessionEvent::InteractionRequested { interaction } => {
                // 字段必须能用 client 导出的契约类型命名。
                let _: &fact::InteractionId = &interaction.interaction_id;
                let _: &Option<fact::ToolCallId> = &interaction.call_id;
                let _: &fact::InteractionKind = &interaction.kind;
                let _: &fact::ContentValue = &interaction.request;
                "requested"
            }
            SessionEvent::InteractionResolved { interaction_id, .. } => {
                let _: &fact::InteractionId = interaction_id;
                "resolved"
            }
            SessionEvent::ToolFinished {
                call_id,
                status,
                error,
                ..
            } => {
                let _: &fact::ToolCallId = call_id;
                let _: &contract_state::ToolCallStatus = status;
                let _: &Option<fact::ToolError> = error;
                "tool"
            }
            _ => "other",
        }
    }

    let requested: ClientV2Event = serde_json::from_value(serde_json::json!({
        "seq": 3,
        "type": "interaction_requested",
        "interaction": {
            "interaction_id": "int_1",
            "call_id": "call_1",
            "kind": "plan",
            "request": { "kind": "inline", "data": { "text": "approve?" } },
            "expires_at_ms": null
        }
    }))
    .expect("decode interaction_requested frame");
    assert_eq!(requested.seq, 3);
    assert_eq!(classify(&requested), "requested");

    let finished: ClientV2Event = serde_json::from_value(serde_json::json!({
        "seq": 4,
        "type": "tool_finished",
        "call_id": "call_1",
        "status": "finished",
        "output": null,
        "output_truncated": false,
        "error": null,
        "metrics": null
    }))
    .expect("decode tool_finished frame");
    assert_eq!(classify(&finished), "tool");
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
