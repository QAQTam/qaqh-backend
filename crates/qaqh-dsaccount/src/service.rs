//! Credential-free DeepSeek service projections.
//!
//! Polling uses snake_case while the service aggregate uses camelCase. Amounts
//! stay as provider decimal strings, including exponent notation.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Current sign-in state, including the platform's hyphenated success spelling.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
#[serde(rename_all = "kebab-case")]
pub enum DsAccountFlowState {
    /// No attempt.
    Idle,
    /// Waiting for browser authorization.
    Authorizing,
    /// Exchanging the callback code.
    Exchanging,
    /// Credentials are persisted.
    SignedIn,
    /// Attempt failed.
    Failed,
    /// Attempt cancelled.
    Cancelled,
}

/// Remote revocation progress, independent from local sign-out.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
#[serde(rename_all = "snake_case")]
pub enum DsAccountRevokeState {
    /// No revocation requested.
    Idle,
    /// Remote revocation in progress.
    Running,
    /// Remote revocation succeeded.
    Done,
    /// Remote revocation failed after bounded retries.
    Failed,
}

/// Account projection; neither plaintext nor encrypted credentials are present.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct DsAccountPublicAccount {
    /// Stable user ID.
    pub user_id: String,
    /// Display name.
    pub label: String,
    /// Login provider, or null.
    #[serde(deserialize_with = "required_nullable")]
    pub provider: Option<String>,
    /// Settlement currency, or null.
    #[serde(deserialize_with = "required_nullable")]
    pub currency: Option<String>,
    /// Bound inference origin.
    pub inference_origin: String,
    /// Persistence timestamp.
    pub saved_at_ms: u64,
    /// Credential presence, not the credential itself.
    pub has_credential: bool,
}

/// Polling response: snake_case is intentional.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct DsAccountSignInStatusResult {
    /// Attempt ID, or null before the first attempt.
    #[serde(deserialize_with = "required_nullable")]
    pub attempt_id: Option<String>,
    /// Attempt state.
    pub state: DsAccountFlowState,
    /// Browser URL, or null.
    #[serde(deserialize_with = "required_nullable")]
    pub authorize_url: Option<String>,
    /// Expiry timestamp, zero in idle state.
    pub expires_at_ms: u64,
    /// Displayable failure, or null.
    #[serde(deserialize_with = "required_nullable")]
    pub error: Option<String>,
}

/// Flow embedded in the aggregate status: camelCase is intentional.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
#[serde(rename_all = "camelCase")]
pub struct DsAccountStatusFlow {
    /// Attempt ID, or null.
    #[serde(deserialize_with = "required_nullable")]
    pub attempt_id: Option<String>,
    /// Attempt state.
    pub state: DsAccountFlowState,
    /// Browser URL, or null.
    #[serde(deserialize_with = "required_nullable")]
    pub authorize_url: Option<String>,
    /// Expiry timestamp.
    pub expires_at_ms: u64,
    /// Failure, or null.
    #[serde(deserialize_with = "required_nullable")]
    pub error: Option<String>,
}

/// Last remote revocation outcome.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct DsAccountRevoke {
    /// Revocation state.
    pub state: DsAccountRevokeState,
    /// Requests issued so far.
    pub attempts: u32,
    /// Last failure, or null.
    #[serde(deserialize_with = "required_nullable")]
    pub message: Option<String>,
}

/// Aggregate account status.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
#[serde(rename_all = "camelCase")]
pub struct DsAccountStatusResult {
    /// Whether a persisted account exists.
    pub signed_in: bool,
    /// Public account, or null.
    #[serde(deserialize_with = "required_nullable")]
    pub account: Option<DsAccountPublicAccount>,
    /// Storage failure, or null.
    #[serde(deserialize_with = "required_nullable")]
    pub storage_error: Option<String>,
    /// Bound inference origin.
    pub inference_origin: String,
    /// Usage management URL.
    pub usage_url: String,
    /// Top-up URL.
    pub top_up_url: String,
    /// Current attempt.
    pub flow: DsAccountStatusFlow,
    /// Last revocation.
    pub revoke: DsAccountRevoke,
}

/// A wallet in the service response (amounts are unchanged strings).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
#[serde(rename_all = "camelCase")]
pub struct DsAccountWallet {
    /// Currency code.
    pub currency: String,
    /// Balance as returned by the provider.
    pub balance: String,
    /// Token estimation, or null.
    #[serde(deserialize_with = "required_nullable")]
    pub token_estimation: Option<String>,
}

/// Cumulative expenditure in one currency.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct DsAccountTotalCost {
    /// Currency code.
    pub currency: String,
    /// Provider decimal string.
    pub amount: String,
}

/// Recharge and bonus wallets remain separate.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
#[serde(rename_all = "camelCase")]
pub struct DsAccountBalanceResult {
    /// Recharge wallets.
    pub recharge: Vec<DsAccountWallet>,
    /// Bonus wallets.
    pub bonus: Vec<DsAccountWallet>,
    /// Total expenditure.
    pub total_cost: Vec<DsAccountTotalCost>,
    /// Whether any wallet has usable funds.
    pub has_usable_balance: bool,
}

/// A not-yet-acknowledged bonus.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
#[serde(rename_all = "camelCase")]
pub struct DsAccountBonus {
    /// ID used for acknowledgement.
    pub order_id: String,
    /// Campaign identifier.
    pub campaign: String,
    /// Provider decimal string.
    pub amount: String,
    /// Currency code.
    pub currency: String,
    /// Grant time as supplied by the platform.
    pub granted_at: String,
    /// Expiry time as supplied by the platform.
    pub expires_at: String,
    /// Localized provider message.
    pub message: String,
}

/// Browser authorization handoff.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
#[serde(rename_all = "camelCase")]
pub struct DsAccountSignInStartResult {
    /// Attempt ID.
    pub attempt_id: String,
    /// URL opened by the host, not the daemon.
    pub authorize_url: String,
    /// Actual bound callback URI.
    pub redirect_uri: String,
    /// Lifetime in seconds.
    pub expires_in: u64,
}

/// Verification may clear invalid local credentials, hence this is a write call.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct DsAccountVerifyResult {
    /// Whether the credential was accepted.
    pub valid: bool,
    /// Failure reason, or null.
    #[serde(deserialize_with = "required_nullable")]
    pub reason: Option<String>,
    /// Whether invalid local credentials were actually cleared.
    pub cleared: bool,
}

/// One upstream model entry. Only provider-defined metadata is dynamic; the
/// response remains an array of identified models, not an arbitrary JSON value.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct DsAccountModel {
    /// Stable model identifier.
    pub id: String,
    /// Provider-owned model capabilities and extension keys, forwarded losslessly.
    #[serde(flatten)]
    pub metadata: BTreeMap<String, Value>,
}

// A nullable wire field is still required to be present. Serde's default Option
// handling alone would silently accept missing keys as null.
fn required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AccountStore, DsAccountManager, DsClientConfig};

    #[test]
    fn actual_idle_polling_matches_public_dto_without_camelcase_normalization() {
        let dir = tempfile::tempdir().unwrap();
        let manager = DsAccountManager::new(
            AccountStore::new(dir.path().join("account.json")),
            DsClientConfig::default(),
        );
        let value = manager.sign_in_status();
        let dto: DsAccountSignInStatusResult = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(dto).unwrap(), value);
        assert!(value.get("attempt_id").is_some());
        assert!(value.get("attemptId").is_none());
    }

    #[test]
    fn stored_account_projection_keeps_nulls_but_never_credentials() {
        let stored = crate::StoredAccount {
            user_id: "synthetic-user".into(),
            label: "Synthetic".into(),
            provider: None,
            currency: Some("CNY".into()),
            inference_origin: "https://example.invalid".into(),
            credential: "synthetic-ciphertext".into(),
            saved_at_ms: 123,
        };
        let value = stored.to_public_json();
        let dto: DsAccountPublicAccount = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(dto).unwrap(), value);
        assert_eq!(value["provider"], Value::Null);
        assert!(value.get("credential").is_none());
        assert!(value.get("token").is_none());
    }
}
