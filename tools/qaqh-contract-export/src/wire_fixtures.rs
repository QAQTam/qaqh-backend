//! Synthetic wire boundary samples shared with SDK consumers.

use std::path::Path;

use qaqh_ringing::{
    CanonicalCursor, CursorToken, RingingCommandAckStatus, RingingCommandState,
    RingingV2AskOutcome, RingingV2CommandAck, RingingV2CommandResult, RingingV2CommandStatus,
    RingingV2DeviceWire, RingingV2DevicesResponse, RingingV2DriverClaimResponse,
    RingingV2DriverReleaseResponse, RingingV2ExistingResult, RingingV2LeaseRenewResponse,
    RingingV2PairRequest, RingingV2PairResponse, RingingV2PairTokenRequest,
    RingingV2PairTokenResponse, RingingV2ResetReason, RingingV2ResetRequired,
};

pub(super) fn write(stage: &Path) -> Result<(), Box<dyn std::error::Error>> {
    super::write_fixture(
        stage,
        "receipt-policy.json",
        &qaqh_runtime::ringing::pending_store::RECEIPT_POLICY,
    )?;
    super::write_fixture(
        stage,
        "lease-renew.json",
        &RingingV2LeaseRenewResponse {
            ok: true,
            lease_ttl_ms: 30_000,
            renew_interval_ms: 10_000,
        },
    )?;
    let cursor = CursorToken::encode_snapshot(&CanonicalCursor::snapshot("fixture-log", 3))?;
    let reset = RingingV2ResetRequired {
        schema: qaqh_ringing::RINGING_SCHEMA.into(),
        version: qaqh_ringing::RINGING_V2_VERSION,
        server_epoch: "fixture-epoch".into(),
        session_id: "session-fixture".into(),
        log_id: Some("fixture-log".into()),
        snapshot_cursor: Some(cursor),
        reason: RingingV2ResetReason::ReplayOverflow,
    };
    reset
        .validate()
        .map_err(|code| format!("invalid reset fixture: {code}"))?;
    super::write_fixture(stage, "reset-required.json", &reset)?;
    super::write_fixture(
        stage,
        "driver-claim-busy.json",
        &RingingV2DriverClaimResponse {
            accepted: false,
            holder: Some("other-client-fixture".into()),
            driver_epoch: 4,
            reason: "driver_busy".into(),
        },
    )?;
    super::write_fixture(
        stage,
        "driver-release.json",
        &RingingV2DriverReleaseResponse {
            accepted: true,
            holder: None,
            driver_epoch: 5,
            reason: "released".into(),
        },
    )?;
    for (name, result) in [
        (
            "ask",
            RingingV2CommandResult::AskResolved {
                interaction_id: "interaction-fixture".into(),
                outcome: RingingV2AskOutcome::Answered,
            },
        ),
        (
            "plan",
            RingingV2CommandResult::PlanReviewResolved {
                interaction_id: "interaction-fixture".into(),
                approved: true,
            },
        ),
        (
            "permission",
            RingingV2CommandResult::PermissionResolved {
                interaction_id: "interaction-fixture".into(),
                approved: false,
            },
        ),
    ] {
        super::write_fixture(
            stage,
            &format!("command-status-{name}-succeeded.json"),
            &RingingV2CommandStatus {
                command_id: "cmd-fixture-1".into(),
                state: RingingCommandState::Succeeded,
                payload_fingerprint: "synthetic-fingerprint".into(),
                terminal_event_id: Some("event-fixture".into()),
                error_code: None,
                result: Some(result.clone()),
            },
        )?;
        super::write_fixture(
            stage,
            &format!("command-ack-{name}-existing.json"),
            &RingingV2CommandAck {
                command_id: "cmd-fixture-1".into(),
                status: RingingCommandAckStatus::Rejected,
                code: Some("interaction_already_resolved".into()),
                message: None,
                retry_after_ms: None,
                existing: Some(RingingV2ExistingResult::InteractionResolved { result }),
            },
        )?;
    }
    super::write_fixture(
        stage,
        "pair-token-params.json",
        &RingingV2PairTokenRequest {
            scope_grant: "view".into(),
            device_name: "Synthetic device".into(),
            platform: "fixture".into(),
        },
    )?;
    // Token-shaped values are intentionally non-secret synthetic sentinels.
    super::write_fixture(
        stage,
        "pair-token-result.json",
        &RingingV2PairTokenResponse {
            pairing_token: "SYNTHETIC_NOT_A_CREDENTIAL".into(),
            expires_in_ms: 60_000,
            tls_fp: String::new(),
        },
    )?;
    super::write_fixture(
        stage,
        "pair-params.json",
        &RingingV2PairRequest {
            pairing_token: "SYNTHETIC_NOT_A_CREDENTIAL".into(),
            device_name: "Synthetic device".into(),
            platform: "fixture".into(),
        },
    )?;
    super::write_fixture(
        stage,
        "pair-result.json",
        &RingingV2PairResponse {
            device_id: "device-fixture".into(),
            device_token: "SYNTHETIC_NOT_A_CREDENTIAL".into(),
            scope: "view".into(),
            daemon_version: env!("CARGO_PKG_VERSION").into(),
            protocol_version: qaqh_ringing::RINGING_V2_VERSION,
        },
    )?;
    super::write_fixture(
        stage,
        "devices.json",
        &RingingV2DevicesResponse {
            devices: vec![RingingV2DeviceWire {
                device_id: "device-fixture".into(),
                name: "Synthetic device".into(),
                platform: "fixture".into(),
                scope: "view".into(),
                created_at_ms: 1_700_000_000_000,
                last_seen_ms: 1_700_000_000_001,
            }],
        },
    )?;
    Ok(())
}
