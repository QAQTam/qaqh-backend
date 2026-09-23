//! Public v2 API compile checks for TUI/Windows shells.
//!
//! Shells must be able to name the v2 control surface through `qaqh-client`
//! without importing `qaqh-ringing`, `qaqh-domain` or `qaqh-session`.

use qaqh_client::{
    ClientError, ClientV2Bootstrap, ClientV2ControlState, ClientV2ConversationState,
    ClientV2Cursor, ClientV2CursorToken, ClientV2Event, ClientV2EventEnvelope, ClientV2Payload,
    ClientV2Reset, ClientV2ResetReason, ClientV2SessionState, ClientV2Subscription,
    ClientV2SubscriptionEvent, ClientV2ToolState, RINGING_V2_BASE_PATH, RINGING_V2_VERSION,
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

    assert_eq!(RINGING_V2_VERSION, 2);
    assert_eq!(RINGING_V2_BASE_PATH, "/ringing/v2");
    assert_eq!(
        serde_json::to_string(&ClientV2ResetReason::SnapshotMissing).expect("json"),
        "\"snapshot_missing\""
    );
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
