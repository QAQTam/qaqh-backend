//! Synthetic account service fixtures serialized from the canonical DTOs.
//!
//! Every sample is built from the same Rust DTOs the runtime dispatch projects
//! through, so fixture drift fails the serde round-trip in [`super::write_fixture`].
//! No real tokens, accounts or provider URLs belong here.

use std::path::Path;

use qaqh_siwc::ProfileStatus;

use qaqh_runtime::ringing::service_methods::accounts::{
    AccountsEmptyParams, DsAccountAttemptParams, DsAccountBalanceResult, DsAccountBonus,
    DsAccountBonusAckParams, DsAccountFlowState, DsAccountModel, DsAccountRevoke,
    DsAccountRevokeState, DsAccountSignInStartParams, DsAccountSignInStartResult,
    DsAccountSignInStatusResult, DsAccountStatusFlow, DsAccountStatusResult, DsAccountTotalCost,
    DsAccountVerifyResult, DsAccountWallet, SiwcConnectionStatus, SiwcFlowParams, SiwcModel,
    SiwcProfileParams, SiwcProfileSelectParams, SiwcProfilesResult, SiwcPublicProfile,
    SiwcSignInStartParams, SiwcSignInStartResult, SiwcSignInStatusResult, SiwcSignOutResult,
    SiwcStatusResult,
};

fn fixture_name(method: &str, side: &str) -> String {
    format!("service-{}-{side}.json", method.replace('.', "-"))
}

pub(super) fn write(stage: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let empty = AccountsEmptyParams {};

    let siwc_status = || SiwcStatusResult {
        status: SiwcConnectionStatus::Connected,
        sharing: true,
        profiles: 1,
        usage_url: "https://example.invalid/usage".into(),
        api_base: "https://example.invalid/v1".into(),
        profile_id: Some("profile-fixture".into()),
        label: Some("Synthetic".into()),
        email: None,
        name: None,
        error: None,
    };

    write_pair(stage, "siwc.status", &empty, &siwc_status())?;
    write_pair(
        stage,
        "siwc.profiles",
        &empty,
        &SiwcProfilesResult {
            active: Some("profile-fixture".into()),
            profiles: vec![SiwcPublicProfile {
                id: "profile-fixture".into(),
                label: "Synthetic".into(),
                status: ProfileStatus::Connected,
                sharing: true,
                client_id: "client-fixture".into(),
                scopes: vec!["scope-fixture".into()],
                saved_at_ms: 1_700_000_000_000,
                email: None,
                name: None,
            }],
            pending: Vec::new(),
        },
    )?;
    write_pair(
        stage,
        "siwc.models",
        &SiwcProfileParams { profile_id: None },
        &vec![SiwcModel {
            slug: "model-fixture".into(),
            display_name: "Synthetic Model".into(),
        }],
    )?;
    write_pair(
        stage,
        "siwc.sign_in.start",
        &SiwcSignInStartParams {
            profile_id: None,
            new_profile: Some(true),
            label: Some("Synthetic".into()),
            reconsent: None,
        },
        &SiwcSignInStartResult {
            flow_id: "flow-fixture".into(),
            authorize_url: "https://example.invalid/auth".into(),
            redirect_uri: "http://127.0.0.1:1234/callback".into(),
            expires_at_ms: 123,
            usage_url: "https://example.invalid/usage".into(),
        },
    )?;
    let flow_params = SiwcFlowParams {
        flow_id: "flow-fixture".into(),
    };
    write_pair(
        stage,
        "siwc.sign_in.status",
        &flow_params,
        &SiwcSignInStatusResult::Pending {
            flow_id: "flow-fixture".into(),
            expires_at_ms: 123,
        },
    )?;
    write_pair(
        stage,
        "siwc.sign_in.cancel",
        &flow_params,
        &Option::<()>::None,
    )?;
    write_pair(
        stage,
        "siwc.profile.select",
        &SiwcProfileSelectParams {
            profile_id: "profile-fixture".into(),
        },
        &siwc_status(),
    )?;
    write_pair(
        stage,
        "siwc.sign_out",
        &SiwcProfileParams {
            profile_id: Some("profile-fixture".into()),
        },
        &SiwcSignOutResult {
            profile_id: "profile-fixture".into(),
            revoked: false,
            had_refresh_token: true,
            usage_url: "https://example.invalid/usage".into(),
        },
    )?;

    let idle_polling = DsAccountSignInStatusResult {
        attempt_id: None,
        state: DsAccountFlowState::Idle,
        authorize_url: None,
        expires_at_ms: 0,
        error: None,
    };
    write_pair(
        stage,
        "dsaccount.status",
        &empty,
        &DsAccountStatusResult {
            signed_in: false,
            account: None,
            storage_error: None,
            inference_origin: "https://example.invalid".into(),
            usage_url: "https://example.invalid/usage".into(),
            top_up_url: "https://example.invalid/topup".into(),
            flow: DsAccountStatusFlow {
                attempt_id: None,
                state: DsAccountFlowState::Idle,
                authorize_url: None,
                expires_at_ms: 0,
                error: None,
            },
            revoke: DsAccountRevoke {
                state: DsAccountRevokeState::Idle,
                attempts: 0,
                message: None,
            },
        },
    )?;
    write_pair(
        stage,
        "dsaccount.balance",
        &empty,
        &DsAccountBalanceResult {
            recharge: vec![DsAccountWallet {
                currency: "CNY".into(),
                balance: "0E-16".into(),
                token_estimation: None,
            }],
            bonus: Vec::new(),
            total_cost: vec![DsAccountTotalCost {
                currency: "CNY".into(),
                amount: "1.23".into(),
            }],
            has_usable_balance: false,
        },
    )?;
    write_pair(
        stage,
        "dsaccount.bonuses",
        &empty,
        &vec![DsAccountBonus {
            order_id: "order-fixture".into(),
            campaign: "campaign-fixture".into(),
            amount: "10".into(),
            currency: "CNY".into(),
            granted_at: "2026-01-01".into(),
            expires_at: "2027-01-01".into(),
            message: "synthetic bonus".into(),
        }],
    )?;
    write_pair(
        stage,
        "dsaccount.sign_in.start",
        &DsAccountSignInStartParams {
            locale: Some("zh".into()),
            port: Some(1234),
        },
        &DsAccountSignInStartResult {
            attempt_id: "attempt-fixture".into(),
            authorize_url: "https://example.invalid/auth".into(),
            redirect_uri: "http://127.0.0.1:1234/callback".into(),
            expires_in: 120,
        },
    )?;
    write_pair(stage, "dsaccount.sign_in.status", &empty, &idle_polling)?;
    write_pair(
        stage,
        "dsaccount.sign_in.cancel",
        &DsAccountAttemptParams {
            attempt_id: "attempt-fixture".into(),
        },
        &Option::<()>::None,
    )?;
    write_pair(stage, "dsaccount.sign_out", &empty, &Option::<()>::None)?;
    write_pair(
        stage,
        "dsaccount.bonus.ack",
        &DsAccountBonusAckParams {
            order_id: "order-fixture".into(),
        },
        &Option::<()>::None,
    )?;
    let mut model_metadata = std::collections::BTreeMap::new();
    model_metadata.insert(
        "effort".to_owned(),
        serde_json::json!({"supported_levels": ["high"]}),
    );
    write_pair(
        stage,
        "dsaccount.models",
        &empty,
        &vec![DsAccountModel {
            id: "model-fixture".into(),
            metadata: model_metadata,
        }],
    )?;
    write_pair(
        stage,
        "dsaccount.verify",
        &empty,
        &DsAccountVerifyResult {
            valid: false,
            reason: Some("synthetic failure".into()),
            cleared: false,
        },
    )?;
    Ok(())
}

fn write_pair<P, R>(
    stage: &Path,
    method: &str,
    params: &P,
    result: &R,
) -> Result<(), Box<dyn std::error::Error>>
where
    P: serde::Serialize + serde::de::DeserializeOwned,
    R: serde::Serialize + serde::de::DeserializeOwned,
{
    super::write_fixture(stage, &fixture_name(method, "params"), params)?;
    super::write_fixture(stage, &fixture_name(method, "result"), result)?;
    Ok(())
}
