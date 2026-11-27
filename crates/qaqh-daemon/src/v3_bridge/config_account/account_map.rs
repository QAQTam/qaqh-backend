use qaqh_core_vocab::JsonText;
use qaqh_ringing_v3::{
    AccountResponse, AccountServiceError, DsAccountFlowView, DsAccountModelView,
    DsAccountProfileView, DsAccountStatusView, DsBalanceView, DsBonusView, DsFlowState,
    DsRevokeOutcomeView, DsSignInStartView, DsSignInStatusView, DsTotalCostView, DsVerifyView,
    DsWalletView, RpcError, SiwcConnectionState, SiwcFlowState, SiwcModelView, SiwcProfileState,
    SiwcProfileView, SiwcProfilesView, SiwcSignInStartView, SiwcSignInStatusView, SiwcSignOutView,
    SiwcStatusView,
};
use qaqh_runtime::ringing::service_methods::accounts::{
    DsAccountBalanceResult, DsAccountBonus, DsAccountFlowState, DsAccountModel,
    DsAccountPublicAccount, DsAccountRevokeState, DsAccountSignInStartResult,
    DsAccountSignInStatusResult, DsAccountStatusResult, DsAccountVerifyResult, ProfileStatus,
    SiwcConnectionStatus, SiwcModel, SiwcPendingProfile, SiwcProfilesResult, SiwcPublicError,
    SiwcPublicProfile, SiwcSignInStartResult, SiwcSignInStatusResult, SiwcSignOutResult,
    SiwcStatusResult,
};
use serde_json::Value;

use super::redact;
use crate::v3_bridge::bad_request;

pub(super) fn siwc_status(value: SiwcStatusResult) -> AccountResponse {
    AccountResponse::SiwcStatus(SiwcStatusView {
        status: match value.status {
            SiwcConnectionStatus::Connecting => SiwcConnectionState::Connecting,
            SiwcConnectionStatus::Connected => SiwcConnectionState::Connected,
            SiwcConnectionStatus::ReauthRequired => SiwcConnectionState::ReauthRequired,
            SiwcConnectionStatus::Disconnected => SiwcConnectionState::Disconnected,
        },
        sharing: value.sharing,
        profiles: value.profiles as u64,
        usage_url: value.usage_url,
        api_base: value.api_base,
        profile_id: value.profile_id,
        label: value.label,
        email: value.email,
        name: value.name,
        error: value.error.map(siwc_error),
    })
}

pub(super) fn siwc_profiles(value: SiwcProfilesResult) -> AccountResponse {
    AccountResponse::SiwcProfiles(SiwcProfilesView {
        active: value.active,
        profiles: value.profiles.into_iter().map(siwc_public_profile).collect(),
        pending: value.pending.into_iter().map(siwc_pending_profile).collect(),
    })
}

pub(super) fn siwc_models(values: Vec<SiwcModel>) -> AccountResponse {
    AccountResponse::SiwcModels(
        values
            .into_iter()
            .map(|model| SiwcModelView {
                slug: model.slug,
                display_name: model.display_name,
            })
            .collect(),
    )
}

pub(super) fn siwc_sign_in_start(value: SiwcSignInStartResult) -> AccountResponse {
    AccountResponse::SiwcSignInStart(SiwcSignInStartView {
        flow_id: value.flow_id,
        authorize_url: value.authorize_url,
        redirect_uri: value.redirect_uri,
        expires_at_ms: value.expires_at_ms,
        usage_url: value.usage_url,
    })
}

pub(super) fn siwc_sign_in_poll(value: SiwcSignInStatusResult) -> AccountResponse {
    let view = match value {
        SiwcSignInStatusResult::Pending {
            flow_id,
            expires_at_ms,
        } => SiwcSignInStatusView {
            flow_id,
            state: SiwcFlowState::Pending,
            expires_at_ms: Some(expires_at_ms),
            profile_id: None,
            error: None,
        },
        SiwcSignInStatusResult::Completed {
            flow_id,
            profile_id,
        } => SiwcSignInStatusView {
            flow_id,
            state: SiwcFlowState::Completed,
            expires_at_ms: None,
            profile_id: Some(profile_id),
            error: None,
        },
        SiwcSignInStatusResult::Failed { flow_id, error } => SiwcSignInStatusView {
            flow_id,
            state: SiwcFlowState::Failed,
            expires_at_ms: None,
            profile_id: None,
            error: Some(siwc_error(error)),
        },
    };
    AccountResponse::SiwcSignInPoll(view)
}

pub(super) fn siwc_sign_out(value: SiwcSignOutResult) -> AccountResponse {
    AccountResponse::SiwcSignOut(SiwcSignOutView {
        profile_id: value.profile_id,
        revoked: value.revoked,
        had_refresh_token: value.had_refresh_token,
        usage_url: value.usage_url,
    })
}

pub(super) fn ds_status(value: DsAccountStatusResult) -> AccountResponse {
    AccountResponse::DsStatus(DsAccountStatusView {
        signed_in: value.signed_in,
        account: value.account.map(ds_account),
        storage_error: value.storage_error,
        inference_origin: value.inference_origin,
        usage_url: value.usage_url,
        top_up_url: value.top_up_url,
        flow: DsAccountFlowView {
            attempt_id: value.flow.attempt_id,
            state: ds_flow_state(value.flow.state),
            authorize_url: value.flow.authorize_url,
            expires_at_ms: value.flow.expires_at_ms,
            error: value.flow.error,
        },
        revoke: DsRevokeOutcomeView {
            state: match value.revoke.state {
                DsAccountRevokeState::Idle => "idle",
                DsAccountRevokeState::Running => "running",
                DsAccountRevokeState::Done => "succeeded",
                DsAccountRevokeState::Failed => "failed",
            }
            .to_string(),
            attempts: value.revoke.attempts,
            message: value.revoke.message,
        },
    })
}

pub(super) fn ds_sign_in_start(value: DsAccountSignInStartResult) -> AccountResponse {
    AccountResponse::DsSignInStart(DsSignInStartView {
        attempt_id: value.attempt_id,
        authorize_url: value.authorize_url,
        redirect_uri: value.redirect_uri,
        expires_in: value.expires_in,
    })
}

pub(super) fn ds_sign_in_poll(
    value: DsAccountSignInStatusResult,
    attempt_id: &str,
) -> Result<AccountResponse, RpcError> {
    if value
        .attempt_id
        .as_deref()
        .is_some_and(|actual| actual != attempt_id)
    {
        return Err(bad_request(
            "dsaccount attempt_id does not match the active login attempt",
        ));
    }
    Ok(AccountResponse::DsSignInPoll(DsSignInStatusView {
        attempt_id: value.attempt_id,
        state: ds_flow_state(value.state),
        authorize_url: value.authorize_url,
        expires_at_ms: value.expires_at_ms,
        error: value.error,
    }))
}

pub(super) fn ds_verify(value: DsAccountVerifyResult) -> AccountResponse {
    AccountResponse::DsVerify(DsVerifyView {
        valid: value.valid,
        reason: value.reason,
        cleared: value.cleared,
    })
}

pub(super) fn ds_balance(value: DsAccountBalanceResult) -> AccountResponse {
    AccountResponse::DsBalance(DsBalanceView {
        recharge: value
            .recharge
            .into_iter()
            .map(|wallet| DsWalletView {
                currency: wallet.currency,
                balance: wallet.balance,
                token_estimation: wallet.token_estimation,
            })
            .collect(),
        bonus: value
            .bonus
            .into_iter()
            .map(|wallet| DsWalletView {
                currency: wallet.currency,
                balance: wallet.balance,
                token_estimation: wallet.token_estimation,
            })
            .collect(),
        total_cost: value
            .total_cost
            .into_iter()
            .map(|cost| DsTotalCostView {
                currency: cost.currency,
                amount: cost.amount,
            })
            .collect(),
        has_usable_balance: value.has_usable_balance,
    })
}

pub(super) fn ds_bonuses(values: Vec<DsAccountBonus>) -> AccountResponse {
    AccountResponse::DsBonuses(
        values
            .into_iter()
            .map(|bonus| DsBonusView {
                order_id: bonus.order_id,
                campaign: bonus.campaign,
                amount: bonus.amount,
                currency: bonus.currency,
                granted_at: bonus.granted_at,
                expires_at: bonus.expires_at,
                message: bonus.message,
            })
            .collect(),
    )
}

pub(super) fn ds_models(values: Vec<DsAccountModel>) -> Result<AccountResponse, RpcError> {
    let mut views = Vec::with_capacity(values.len());
    for model in values {
        let mut raw = model.metadata;
        let id = Some(model.id.clone());
        raw.insert("id".to_string(), Value::String(model.id));
        let redacted = redact(Value::Object(raw.into_iter().collect()));
        views.push(DsAccountModelView {
            id,
            raw: JsonText::new(redacted.to_string()),
        });
    }
    Ok(AccountResponse::DsModels(views))
}

fn siwc_error(error: SiwcPublicError) -> AccountServiceError {
    AccountServiceError {
        code: error.code.as_str().to_string(),
        message: error.message,
        retryable: error.retryable,
        requires_reauth: error.requires_reauth,
        request_id: error.request_id,
    }
}

fn siwc_profile_state(status: ProfileStatus) -> SiwcProfileState {
    match status {
        ProfileStatus::Connected => SiwcProfileState::Connected,
        ProfileStatus::ReauthRequired => SiwcProfileState::ReauthRequired,
        ProfileStatus::Disconnected => SiwcProfileState::Disconnected,
    }
}

fn siwc_public_profile(profile: SiwcPublicProfile) -> SiwcProfileView {
    SiwcProfileView {
        id: profile.id,
        label: profile.label,
        status: siwc_profile_state(profile.status),
        sharing: profile.sharing,
        client_id: Some(profile.client_id),
        scopes: profile.scopes,
        saved_at_ms: Some(profile.saved_at_ms),
        email: profile.email,
        name: profile.name,
        pending: false,
    }
}

fn siwc_pending_profile(profile: SiwcPendingProfile) -> SiwcProfileView {
    SiwcProfileView {
        id: profile.id,
        label: profile.label,
        status: siwc_profile_state(profile.status),
        sharing: profile.sharing,
        client_id: None,
        scopes: Vec::new(),
        saved_at_ms: None,
        email: None,
        name: None,
        pending: profile.pending,
    }
}

fn ds_account(account: DsAccountPublicAccount) -> DsAccountProfileView {
    DsAccountProfileView {
        user_id: account.user_id,
        label: account.label,
        provider: account.provider,
        currency: account.currency,
        inference_origin: account.inference_origin,
        saved_at_ms: account.saved_at_ms,
        has_credential: account.has_credential,
    }
}

fn ds_flow_state(state: DsAccountFlowState) -> DsFlowState {
    match state {
        DsAccountFlowState::Idle => DsFlowState::Idle,
        DsAccountFlowState::Authorizing => DsFlowState::Authorizing,
        DsAccountFlowState::Exchanging => DsFlowState::Exchanging,
        DsAccountFlowState::SignedIn => DsFlowState::SignedIn,
        DsAccountFlowState::Failed => DsFlowState::Failed,
        DsAccountFlowState::Cancelled => DsFlowState::Cancelled,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::collections::BTreeMap;

    use qaqh_runtime::ringing::service_methods::accounts::{
        DsAccountStatusFlow, DsAccountTotalCost, DsAccountWallet, SiwcCode,
    };

    use super::*;

    #[test]
    fn siwc_poll_maps_pending_completed_and_failed_variants() {
        for (input, expected_state, expected_profile) in [
            (
                SiwcSignInStatusResult::Pending {
                    flow_id: "f1".into(),
                    expires_at_ms: 99,
                },
                SiwcFlowState::Pending,
                None,
            ),
            (
                SiwcSignInStatusResult::Completed {
                    flow_id: "f2".into(),
                    profile_id: "p1".into(),
                },
                SiwcFlowState::Completed,
                Some("p1"),
            ),
            (
                SiwcSignInStatusResult::Failed {
                    flow_id: "f3".into(),
                    error: SiwcPublicError {
                        code: SiwcCode::InvalidGrant,
                        message: "bad".into(),
                        retryable: false,
                        requires_reauth: true,
                        request_id: Some("req-1".into()),
                    },
                },
                SiwcFlowState::Failed,
                None,
            ),
        ] {
            let AccountResponse::SiwcSignInPoll(view) = siwc_sign_in_poll(input) else {
                panic!("expected SiwcSignInPoll");
            };
            assert_eq!(view.state, expected_state);
            assert_eq!(view.profile_id.as_deref(), expected_profile);
        }
    }

    #[test]
    fn ds_and_siwc_dtos_map_without_key_renaming_hacks() {
        let AccountResponse::DsStatus(status) = ds_status(DsAccountStatusResult {
            signed_in: true,
            account: Some(DsAccountPublicAccount {
                user_id: "u1".into(),
                label: "User".into(),
                provider: Some("deepseek".into()),
                currency: Some("CNY".into()),
                inference_origin: "https://api.deepseek.com".into(),
                saved_at_ms: 42,
                has_credential: true,
            }),
            storage_error: None,
            inference_origin: "https://api.deepseek.com".into(),
            usage_url: "https://platform.deepseek.com/usage".into(),
            top_up_url: "https://platform.deepseek.com/top_up".into(),
            flow: DsAccountStatusFlow {
                attempt_id: Some("a1".into()),
                state: DsAccountFlowState::SignedIn,
                authorize_url: None,
                expires_at_ms: 100,
                error: None,
            },
            revoke: qaqh_runtime::ringing::service_methods::accounts::DsAccountRevoke {
                state: DsAccountRevokeState::Done,
                attempts: 1,
                message: None,
            },
        }) else {
            panic!("expected DsStatus");
        };
        assert!(status.signed_in);
        assert_eq!(status.flow.state, DsFlowState::SignedIn);
        assert_eq!(status.revoke.state, "succeeded");

        let AccountResponse::DsBalance(balance) = ds_balance(DsAccountBalanceResult {
            recharge: vec![DsAccountWallet {
                currency: "CNY".into(),
                balance: "10.00".into(),
                token_estimation: None,
            }],
            bonus: Vec::new(),
            total_cost: vec![DsAccountTotalCost {
                currency: "CNY".into(),
                amount: "1.23".into(),
            }],
            has_usable_balance: true,
        }) else {
            panic!("expected DsBalance");
        };
        assert_eq!(balance.recharge[0].balance, "10.00");

        let AccountResponse::DsVerify(verify) = ds_verify(DsAccountVerifyResult {
            valid: true,
            reason: None,
            cleared: false,
        }) else {
            panic!("expected DsVerify");
        };
        assert!(verify.valid);

        let AccountResponse::DsBonuses(bonuses) = ds_bonuses(vec![DsAccountBonus {
            order_id: "o1".into(),
            campaign: "welcome".into(),
            amount: "5.00".into(),
            currency: "CNY".into(),
            granted_at: "2026-01-01".into(),
            expires_at: "2026-02-01".into(),
            message: "ok".into(),
        }]) else {
            panic!("expected DsBonuses");
        };
        assert_eq!(bonuses[0].order_id, "o1");

        let AccountResponse::SiwcSignOut(out) = siwc_sign_out(SiwcSignOutResult {
            profile_id: "p1".into(),
            revoked: true,
            had_refresh_token: true,
            usage_url: "https://chatgpt.com".into(),
        }) else {
            panic!("expected SiwcSignOut");
        };
        assert!(out.revoked);

        let mut meta = BTreeMap::new();
        meta.insert("api_key".into(), Value::String("secret".into()));
        let AccountResponse::DsModels(models) =
            ds_models(vec![DsAccountModel { id: "deepseek-chat".into(), metadata: meta }]).unwrap()
        else {
            panic!("expected DsModels");
        };
        assert!(!models[0].raw.as_str().contains("secret"));
    }
}
