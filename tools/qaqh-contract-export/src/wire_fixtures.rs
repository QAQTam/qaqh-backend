//! Synthetic wire boundary samples shared with SDK consumers.

use std::path::Path;

use qaqh_ringing::{
    RingingCommandAckStatus, RingingCommandState, RingingV2AskOutcome, RingingV2CommandAck,
    RingingV2CommandResult, RingingV2CommandStatus, RingingV2DeviceWire, RingingV2DevicesResponse,
    RingingV2DriverClaimResponse, RingingV2DriverReleaseResponse, RingingV2ExistingResult,
    RingingV2LeaseRenewResponse, RingingV2PairRequest, RingingV2PairResponse,
    RingingV2PairTokenRequest, RingingV2PairTokenResponse, RingingV2SyncResult,
};

pub(super) fn write(stage: &Path) -> Result<(), Box<dyn std::error::Error>> {
    super::write_fixture(
        stage,
        "receipt-policy.json",
        &qaqh_ringing_runtime::pending_store::RECEIPT_POLICY,
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
                sync_result: None,
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
                result: None,
                operation: None,
                revision: None,
            },
        )?;
    }
    // B05:同步命令结果的 receipt 回放样本(session.create 形状)。
    super::write_fixture(
        stage,
        "command-status-sync-result.json",
        &RingingV2CommandStatus {
            command_id: "cmd-fixture-create".into(),
            state: RingingCommandState::Succeeded,
            payload_fingerprint: "synthetic-fingerprint".into(),
            terminal_event_id: None,
            error_code: None,
            result: None,
            sync_result: Some(RingingV2SyncResult {
                operation: "session.create".into(),
                result: serde_json::json!({ "session_id": "0199a0f0-0000-7000-8000-000000000001" }),
                revision: None,
            }),
        },
    )?;
    super::write_fixture(
        stage,
        "command-ack-sync-replay.json",
        &RingingV2CommandAck {
            command_id: "cmd-fixture-create".into(),
            status: RingingCommandAckStatus::Accepted,
            code: None,
            message: Some("duplicate command_id (already recorded)".into()),
            retry_after_ms: None,
            existing: Some(RingingV2ExistingResult::CommandReceipt {
                state: RingingCommandState::Succeeded,
                terminal_event_id: None,
                error_code: None,
                result: None,
                sync_result: Some(RingingV2SyncResult {
                    operation: "session.create".into(),
                    result: serde_json::json!({ "session_id": "0199a0f0-0000-7000-8000-000000000001" }),
                    revision: None,
                }),
            }),
            result: None,
            operation: None,
            revision: None,
        },
    )?;
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
