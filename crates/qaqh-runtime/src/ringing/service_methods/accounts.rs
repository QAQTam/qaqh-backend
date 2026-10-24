//! Typed account RPC contracts. All methods require administrator scope.

use super::{READ_ADMIN, TypedMethodInfo, WRITE_ADMIN, validate_params, validate_result};
use crate::QaqhService;
pub use qaqh_dsaccount::service::*;
pub use qaqh_siwc::service::*;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct AccountsEmptyParams {}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct SiwcProfileParams {
    #[cfg_attr(feature = "ts", ts(optional))]
    pub profile_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct SiwcSignInStartParams {
    #[cfg_attr(feature = "ts", ts(optional))]
    pub profile_id: Option<String>,
    #[cfg_attr(feature = "ts", ts(optional))]
    pub new_profile: Option<bool>,
    #[cfg_attr(feature = "ts", ts(optional))]
    pub label: Option<String>,
    #[cfg_attr(feature = "ts", ts(optional))]
    pub reconsent: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct SiwcFlowParams {
    pub flow_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct SiwcProfileSelectParams {
    pub profile_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct DsAccountSignInStartParams {
    #[cfg_attr(feature = "ts", ts(optional))]
    pub locale: Option<String>,
    #[cfg_attr(feature = "ts", ts(optional))]
    pub port: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct DsAccountAttemptParams {
    pub attempt_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct DsAccountBonusAckParams {
    pub order_id: String,
}

macro_rules! method {
    ($method:expr, $params:ty, $result:ty, $result_name:literal, $info:expr) => {
        TypedMethodInfo {
            method: $method,
            params_type: stringify!($params),
            result_type: $result_name,
            info: $info,
            validate_params: validate_params::<$params>,
            validate_result: validate_result::<$result>,
        }
    };
}

/// Exact DTO and authorization descriptors for all account methods.
pub const TYPED_METHODS: &[TypedMethodInfo] = &[
    method!(
        qaqh_siwc::methods::STATUS,
        AccountsEmptyParams,
        SiwcStatusResult,
        "SiwcStatusResult",
        READ_ADMIN
    ),
    method!(
        qaqh_siwc::methods::PROFILES,
        AccountsEmptyParams,
        SiwcProfilesResult,
        "SiwcProfilesResult",
        READ_ADMIN
    ),
    method!(
        qaqh_siwc::methods::MODELS,
        SiwcProfileParams,
        Vec<SiwcModel>,
        "SiwcModel[]",
        READ_ADMIN
    ),
    method!(
        qaqh_siwc::methods::SIGN_IN_START,
        SiwcSignInStartParams,
        SiwcSignInStartResult,
        "SiwcSignInStartResult",
        WRITE_ADMIN
    ),
    method!(
        qaqh_siwc::methods::SIGN_IN_STATUS,
        SiwcFlowParams,
        SiwcSignInStatusResult,
        "SiwcSignInStatusResult",
        READ_ADMIN
    ),
    method!(
        qaqh_siwc::methods::SIGN_IN_CANCEL,
        SiwcFlowParams,
        (),
        "null",
        WRITE_ADMIN
    ),
    method!(
        qaqh_siwc::methods::PROFILE_SELECT,
        SiwcProfileSelectParams,
        SiwcStatusResult,
        "SiwcStatusResult",
        WRITE_ADMIN
    ),
    method!(
        qaqh_siwc::methods::SIGN_OUT,
        SiwcProfileParams,
        SiwcSignOutResult,
        "SiwcSignOutResult",
        WRITE_ADMIN
    ),
    method!(
        qaqh_dsaccount::methods::STATUS,
        AccountsEmptyParams,
        DsAccountStatusResult,
        "DsAccountStatusResult",
        READ_ADMIN
    ),
    method!(
        qaqh_dsaccount::methods::BALANCE,
        AccountsEmptyParams,
        DsAccountBalanceResult,
        "DsAccountBalanceResult",
        READ_ADMIN
    ),
    method!(
        qaqh_dsaccount::methods::BONUSES,
        AccountsEmptyParams,
        Vec<DsAccountBonus>,
        "DsAccountBonus[]",
        READ_ADMIN
    ),
    method!(
        qaqh_dsaccount::methods::SIGN_IN_START,
        DsAccountSignInStartParams,
        DsAccountSignInStartResult,
        "DsAccountSignInStartResult",
        WRITE_ADMIN
    ),
    method!(
        qaqh_dsaccount::methods::SIGN_IN_STATUS,
        AccountsEmptyParams,
        DsAccountSignInStatusResult,
        "DsAccountSignInStatusResult",
        READ_ADMIN
    ),
    method!(
        qaqh_dsaccount::methods::SIGN_IN_CANCEL,
        DsAccountAttemptParams,
        (),
        "null",
        WRITE_ADMIN
    ),
    method!(
        qaqh_dsaccount::methods::SIGN_OUT,
        AccountsEmptyParams,
        (),
        "null",
        WRITE_ADMIN
    ),
    method!(
        qaqh_dsaccount::methods::ACK_BONUS,
        DsAccountBonusAckParams,
        (),
        "null",
        WRITE_ADMIN
    ),
    method!(
        qaqh_dsaccount::methods::MODELS,
        AccountsEmptyParams,
        Vec<DsAccountModel>,
        "DsAccountModel[]",
        READ_ADMIN
    ),
    method!(
        qaqh_dsaccount::methods::VERIFY,
        AccountsEmptyParams,
        DsAccountVerifyResult,
        "DsAccountVerifyResult",
        WRITE_ADMIN
    ),
];

fn decode<T: DeserializeOwned>(value: &Value) -> Result<T, String> {
    serde_json::from_value(value.clone()).map_err(|error| format!("invalid account DTO: {error}"))
}

fn encode<T: Serialize>(value: T) -> Result<Value, String> {
    serde_json::to_value(value).map_err(|error| format!("cannot serialize account DTO: {error}"))
}

/// Existing crate projections pass through their credential-free DTO before
/// serialization. This rejects drift instead of forwarding arbitrary JSON.
fn project<T: DeserializeOwned + Serialize>(value: Value) -> Result<Value, String> {
    encode(decode::<T>(&value)?)
}

/// Called by the parent dispatch after method lookup and parameter validation.
/// Returning None leaves unrelated methods to the other service modules.
pub(super) fn dispatch(
    service: &QaqhService,
    method: &str,
    params: &Value,
) -> Option<Result<Value, String>> {
    if !TYPED_METHODS.iter().any(|entry| entry.method == method) {
        return None;
    }
    Some(dispatch_account(service, method, params))
}

fn dispatch_account(service: &QaqhService, method: &str, params: &Value) -> Result<Value, String> {
    match method {
        "siwc.status" => project::<SiwcStatusResult>(service.siwc()?.status()),
        "siwc.profiles" => project::<SiwcProfilesResult>(service.siwc()?.profiles()),
        "siwc.models" => {
            let params: SiwcProfileParams = decode(params)?;
            let models = service
                .siwc()?
                .list_models(params.profile_id.as_deref())
                .map_err(String::from)?;
            encode(
                models
                    .into_iter()
                    .map(|model| SiwcModel {
                        slug: model.slug,
                        display_name: model.display_name,
                    })
                    .collect::<Vec<_>>(),
            )
        }
        "siwc.sign_in.start" => {
            let params: SiwcSignInStartParams = decode(params)?;
            let start = service
                .siwc()?
                .begin_sign_in(qaqh_siwc::SignInOptions {
                    profile_id: params.profile_id,
                    new_profile: params.new_profile.unwrap_or(false),
                    label: params.label,
                    reconsent: params.reconsent.unwrap_or(false),
                })
                .map_err(String::from)?;
            encode(SiwcSignInStartResult {
                flow_id: start.flow_id,
                authorize_url: start.authorize_url,
                redirect_uri: start.redirect_uri,
                expires_at_ms: start.expires_at_ms,
                usage_url: qaqh_siwc::SiwcManager::usage_url().to_string(),
            })
        }
        "siwc.sign_in.status" => {
            let params: SiwcFlowParams = decode(params)?;
            project::<SiwcSignInStatusResult>(
                service
                    .siwc()?
                    .sign_in_status(&params.flow_id)
                    .map_err(String::from)?,
            )
        }
        "siwc.sign_in.cancel" => {
            let params: SiwcFlowParams = decode(params)?;
            service
                .siwc()?
                .cancel_sign_in(&params.flow_id)
                .map_err(String::from)?;
            encode(())
        }
        "siwc.profile.select" => {
            let params: SiwcProfileSelectParams = decode(params)?;
            project::<SiwcStatusResult>(
                service
                    .siwc()?
                    .select_profile(&params.profile_id)
                    .map_err(String::from)?,
            )
        }
        "siwc.sign_out" => {
            let params: SiwcProfileParams = decode(params)?;
            project::<SiwcSignOutResult>(
                service
                    .siwc()?
                    .sign_out(params.profile_id.as_deref())
                    .map_err(String::from)?,
            )
        }
        "dsaccount.status" => {
            // The crate's raw status has snake_case fields. The public service
            // aggregate preserves its existing camelCase normalization.
            let raw = service.dsaccount()?.status();
            let flow: DsAccountSignInStatusResult = decode(&raw["flow"])?;
            encode(DsAccountStatusResult {
                signed_in: decode(&raw["signed_in"])?,
                account: decode(&raw["account"])?,
                storage_error: decode(&raw["storage_error"])?,
                inference_origin: decode(&raw["inference_origin"])?,
                usage_url: qaqh_dsaccount::protocol::USAGE_URL.to_string(),
                top_up_url: qaqh_dsaccount::protocol::TOP_UP_URL.to_string(),
                flow: DsAccountStatusFlow {
                    attempt_id: flow.attempt_id,
                    state: flow.state,
                    authorize_url: flow.authorize_url,
                    expires_at_ms: flow.expires_at_ms,
                    error: flow.error,
                },
                revoke: decode(&raw["revoke"])?,
            })
        }
        "dsaccount.balance" => {
            let summary = service
                .dsaccount()?
                .balance()
                .map_err(|error| error.message().to_string())?;
            let wallet = |wallets: &[qaqh_dsaccount::Wallet]| {
                wallets
                    .iter()
                    .map(|wallet| DsAccountWallet {
                        currency: wallet.currency.clone(),
                        balance: wallet.balance.clone(),
                        token_estimation: wallet.token_estimation.clone(),
                    })
                    .collect()
            };
            encode(DsAccountBalanceResult {
                recharge: wallet(&summary.normal_wallets),
                bonus: wallet(&summary.bonus_wallets),
                total_cost: summary
                    .total_costs
                    .iter()
                    .map(|cost| DsAccountTotalCost {
                        currency: cost.currency.clone(),
                        amount: cost.amount.clone(),
                    })
                    .collect(),
                has_usable_balance: summary.has_usable_balance(),
            })
        }
        "dsaccount.bonuses" => {
            let bonuses = service
                .dsaccount()?
                .bonuses()
                .map_err(|error| error.message().to_string())?;
            encode(
                bonuses
                    .into_iter()
                    .map(|bonus| DsAccountBonus {
                        order_id: bonus.order_id,
                        campaign: bonus.campaign,
                        amount: bonus.amount,
                        currency: bonus.currency,
                        granted_at: bonus.granted_at,
                        expires_at: bonus.expires_at,
                        message: bonus.msg,
                    })
                    .collect::<Vec<_>>(),
            )
        }
        "dsaccount.sign_in.start" => {
            let params: DsAccountSignInStartParams = decode(params)?;
            let locale =
                qaqh_dsaccount::Locale::from_ui_language(params.locale.as_deref().unwrap_or("zh"));
            let port = params
                .port
                .map(|port| u16::try_from(port).unwrap_or(0))
                .unwrap_or(qaqh_dsaccount::protocol::DEFAULT_CALLBACK_PORT);
            let start = service
                .dsaccount()?
                .sign_in_start(locale, qaqh_dsaccount::auth::LOGIN_SOURCE_DESKTOP, port)
                .map_err(|error| error.message().to_string())?;
            encode(DsAccountSignInStartResult {
                attempt_id: start.attempt_id,
                authorize_url: start.authorize_url,
                redirect_uri: start.redirect_uri,
                expires_in: start.expires_in,
            })
        }
        "dsaccount.sign_in.status" => {
            project::<DsAccountSignInStatusResult>(service.dsaccount()?.sign_in_status())
        }
        "dsaccount.sign_in.cancel" => {
            let params: DsAccountAttemptParams = decode(params)?;
            service
                .dsaccount()?
                .sign_in_cancel(&params.attempt_id)
                .map_err(|error| error.message().to_string())?;
            encode(())
        }
        "dsaccount.sign_out" => {
            service
                .dsaccount()?
                .sign_out()
                .map_err(|error| error.message().to_string())?;
            encode(())
        }
        "dsaccount.bonus.ack" => {
            let params: DsAccountBonusAckParams = decode(params)?;
            service
                .dsaccount()?
                .ack_bonus(&params.order_id)
                .map_err(|error| error.message().to_string())?;
            encode(())
        }
        "dsaccount.models" => {
            let catalog = service
                .dsaccount()?
                .models()
                .map_err(|error| error.message().to_string())?;
            let data = catalog
                .get("data")
                .cloned()
                .unwrap_or_else(|| Value::Array(Vec::new()));
            project::<Vec<DsAccountModel>>(data)
        }
        "dsaccount.verify" => project::<DsAccountVerifyResult>(service.dsaccount()?.verify()),
        _ => Err(format!("unknown account service method: {method}")),
    }
}

/// Export every account service root and its transitive DTO dependencies.
#[cfg(feature = "ts")]
pub fn export_types(config: &ts_rs::Config) -> Result<(), ts_rs::ExportError> {
    use ts_rs::TS;
    macro_rules! roots { ($($ty:ty),* $(,)?) => { $(<$ty>::export_all(config)?;)* }; }
    roots!(
        AccountsEmptyParams,
        SiwcProfileParams,
        SiwcSignInStartParams,
        SiwcFlowParams,
        SiwcProfileSelectParams,
        DsAccountSignInStartParams,
        DsAccountAttemptParams,
        DsAccountBonusAckParams,
        SiwcStatusResult,
        SiwcProfilesResult,
        SiwcModel,
        SiwcSignInStartResult,
        SiwcSignInStatusResult,
        SiwcSignOutResult,
        DsAccountStatusResult,
        DsAccountBalanceResult,
        DsAccountBonus,
        DsAccountSignInStartResult,
        DsAccountSignInStatusResult,
        DsAccountVerifyResult,
        DsAccountModel,
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn descriptor(method: &str) -> &TypedMethodInfo {
        TYPED_METHODS
            .iter()
            .find(|entry| entry.method == method)
            .expect("account descriptor")
    }

    #[test]
    fn descriptors_cover_both_account_catalogs_exactly_and_require_admin() {
        let mut actual: Vec<_> = TYPED_METHODS
            .iter()
            .map(|entry| {
                assert!(
                    entry.info.admin_only,
                    "{} must remain admin-only",
                    entry.method
                );
                assert!(!entry.info.requires_session);
                assert_eq!(Some(entry.info), super::super::lookup(entry.method));
                entry.method
            })
            .collect();
        let mut expected: Vec<_> = qaqh_siwc::methods::ALL
            .iter()
            .chain(qaqh_dsaccount::methods::ALL)
            .copied()
            .collect();
        actual.sort_unstable();
        expected.sort_unstable();
        assert_eq!(actual, expected);
        actual.dedup();
        assert_eq!(actual.len(), TYPED_METHODS.len());
        assert_eq!(descriptor("dsaccount.verify").info, WRITE_ADMIN);
    }

    #[test]
    fn required_account_parameters_and_types_are_enforced() {
        for method in [
            "siwc.sign_in.status",
            "siwc.sign_in.cancel",
            "siwc.profile.select",
            "dsaccount.sign_in.cancel",
            "dsaccount.bonus.ack",
        ] {
            assert!(
                (descriptor(method).validate_params)(&json!({})).is_err(),
                "{method}"
            );
        }
        for (method, invalid) in [
            ("siwc.models", json!({"profile_id": 7})),
            ("siwc.sign_in.start", json!({"new_profile": "true"})),
            ("siwc.sign_in.start", json!({"reconsent": 1})),
            ("siwc.sign_out", json!({"profile_id": false})),
            ("dsaccount.sign_in.start", json!({"locale": false})),
            ("dsaccount.sign_in.start", json!({"port": -1})),
            ("dsaccount.sign_in.start", json!({"port": 1.5})),
            ("dsaccount.sign_in.start", json!({"port": "1234"})),
        ] {
            assert!(
                (descriptor(method).validate_params)(&invalid).is_err(),
                "{method}"
            );
        }
        // Keep the existing port fallback for values outside u16, not a wire change.
        (descriptor("dsaccount.sign_in.start").validate_params)(&json!({"port": 65536})).unwrap();
        (descriptor("siwc.sign_in.start").validate_params)(&json!({})).unwrap();
        (descriptor("siwc.models").validate_params)(&json!({})).unwrap();
    }

    #[test]
    fn result_fixtures_cover_every_account_method() {
        let siwc_status = json!({"status": "disconnected", "sharing": false, "profiles": 0,
            "usageUrl": "https://example.invalid/usage", "apiBase": "https://example.invalid/v1"});
        let ds_flow = json!({"attempt_id": null, "state": "idle", "authorize_url": null,
            "expires_at_ms": 0, "error": null});
        let fixtures = vec![
            ("siwc.status", siwc_status.clone()),
            (
                "siwc.profiles",
                json!({"active": null, "profiles": [], "pending": []}),
            ),
            (
                "siwc.models",
                json!([{"slug": "model", "displayName": "Model"}]),
            ),
            (
                "siwc.sign_in.start",
                json!({"flowId": "flow", "authorizeUrl": "https://example.invalid/auth",
                "redirectUri": "http://127.0.0.1:1234/callback", "expiresAtMs": 123,
                "usageUrl": "https://example.invalid/usage"}),
            ),
            (
                "siwc.sign_in.status",
                json!({"flowId": "flow", "state": "pending", "expiresAtMs": 123}),
            ),
            ("siwc.sign_in.cancel", Value::Null),
            ("siwc.profile.select", siwc_status),
            (
                "siwc.sign_out",
                json!({"profileId": "profile", "revoked": false, "hadRefreshToken": true,
                "usageUrl": "https://example.invalid/usage"}),
            ),
            (
                "dsaccount.status",
                json!({"signedIn": false, "account": null, "storageError": null,
                "inferenceOrigin": "https://example.invalid", "usageUrl": "https://example.invalid/usage",
                "topUpUrl": "https://example.invalid/topup", "flow": {"attemptId": null,
                    "state": "idle", "authorizeUrl": null, "expiresAtMs": 0, "error": null},
                "revoke": {"state": "idle", "attempts": 0, "message": null}}),
            ),
            (
                "dsaccount.balance",
                json!({"recharge": [{"currency": "CNY", "balance": "0E-16",
                "tokenEstimation": null}], "bonus": [], "totalCost": [{"currency": "CNY", "amount": "1.23"}],
                "hasUsableBalance": false}),
            ),
            (
                "dsaccount.bonuses",
                json!([{"orderId": "order", "campaign": "campaign", "amount": "10",
                "currency": "CNY", "grantedAt": "2026-01-01", "expiresAt": "2027-01-01", "message": "bonus"}]),
            ),
            (
                "dsaccount.sign_in.start",
                json!({"attemptId": "attempt", "authorizeUrl": "https://example.invalid/auth",
                "redirectUri": "http://127.0.0.1:1234/callback", "expiresIn": 120}),
            ),
            ("dsaccount.sign_in.status", ds_flow),
            ("dsaccount.sign_in.cancel", Value::Null),
            ("dsaccount.sign_out", Value::Null),
            ("dsaccount.bonus.ack", Value::Null),
            (
                "dsaccount.models",
                json!([{"id": "model", "effort": {"supported_levels": ["high"]}}]),
            ),
            (
                "dsaccount.verify",
                json!({"valid": false, "reason": "synthetic failure", "cleared": false}),
            ),
        ];
        assert_eq!(fixtures.len(), TYPED_METHODS.len());
        for (method, value) in fixtures {
            (descriptor(method).validate_result)(&value)
                .unwrap_or_else(|error| panic!("{method}: {error}"));
            assert!(
                (descriptor(method).validate_result)(&json!(42)).is_err(),
                "{method}"
            );
        }
    }

    #[test]
    fn tagged_flow_results_reject_bad_discriminants_and_missing_variant_fields() {
        for value in [
            json!({"flowId": "f", "state": "unknown"}),
            json!({"flowId": "f", "state": "completed"}),
            json!({"flowId": "f", "state": "pending"}),
            json!({"flowId": "f", "state": "failed", "error": {"message": "failed"}}),
        ] {
            assert!((descriptor("siwc.sign_in.status").validate_result)(&value).is_err());
        }
        for state in [
            "idle",
            "authorizing",
            "exchanging",
            "signed-in",
            "failed",
            "cancelled",
        ] {
            (descriptor("dsaccount.sign_in.status").validate_result)(&json!({"attempt_id": null,
                "state": state, "authorize_url": null, "expires_at_ms": 0, "error": null}))
            .unwrap();
        }
        assert!(
            (descriptor("dsaccount.models").validate_result)(&json!([{"effort": {}}])).is_err()
        );
    }

    #[test]
    fn present_nullable_fields_are_not_optional_keys() {
        assert!(
            (descriptor("siwc.profiles").validate_result)(&json!({"profiles": [], "pending": []}))
                .is_err()
        );
        assert!(
            (descriptor("dsaccount.verify").validate_result)(
                &json!({"valid": true, "cleared": false})
            )
            .is_err()
        );
        assert!(
            (descriptor("dsaccount.sign_in.status").validate_result)(
                &json!({"state": "idle", "expires_at_ms": 0})
            )
            .is_err()
        );
    }

    #[test]
    fn response_dtos_preserve_presence_amounts_and_model_extensions_without_secrets() {
        let status = json!({"status": "connected", "sharing": true, "profiles": 1, "profileId": "p",
            "label": "Synthetic", "usageUrl": "https://example.invalid", "apiBase": "https://example.invalid/v1",
            "access_token": "synthetic-secret", "credentials": "synthetic-ciphertext"});
        let projected = project::<SiwcStatusResult>(status.clone()).unwrap();
        assert!(projected.get("access_token").is_none());
        assert!(projected.get("credentials").is_none());
        assert!(projected.get("email").is_none());
        assert_eq!(projected["profileId"], status["profileId"]);
        let model = json!([{"id": "synthetic", "context_length": 12345,
            "effort": {"supported_levels": ["high", "low"]}, "provider_extension": {"x": true}}]);
        assert_eq!(
            project::<Vec<DsAccountModel>>(model.clone()).unwrap(),
            model
        );
        let wallet = json!({"currency": "CNY", "balance": "0E-16", "tokenEstimation": null});
        assert_eq!(project::<DsAccountWallet>(wallet.clone()).unwrap(), wallet);
    }
}
