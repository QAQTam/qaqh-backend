//! Credential-free SIWC service projections; field names follow the existing wire.

use crate::{ProfileStatus, SiwcCode};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
#[serde(rename_all = "snake_case")]
pub enum SiwcConnectionStatus {
    Connecting,
    Connected,
    ReauthRequired,
    Disconnected,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
#[serde(rename_all = "camelCase")]
pub struct SiwcPublicError {
    pub code: SiwcCode,
    pub message: String,
    pub retryable: bool,
    pub requires_reauth: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
#[serde(rename_all = "camelCase")]
pub struct SiwcStatusResult {
    pub status: SiwcConnectionStatus,
    pub sharing: bool,
    pub profiles: usize,
    pub usage_url: String,
    pub api_base: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<SiwcPublicError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
#[serde(rename_all = "camelCase")]
pub struct SiwcPublicProfile {
    pub id: String,
    pub label: String,
    pub status: ProfileStatus,
    pub sharing: bool,
    pub client_id: String,
    pub scopes: Vec<String>,
    pub saved_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct SiwcPendingProfile {
    pub id: String,
    pub label: String,
    pub status: ProfileStatus,
    pub sharing: bool,
    pub pending: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct SiwcProfilesResult {
    #[serde(deserialize_with = "required_nullable")]
    pub active: Option<String>,
    pub profiles: Vec<SiwcPublicProfile>,
    pub pending: Vec<SiwcPendingProfile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
#[serde(rename_all = "camelCase")]
pub struct SiwcModel {
    pub slug: String,
    pub display_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
#[serde(rename_all = "camelCase")]
pub struct SiwcSignInStartResult {
    pub flow_id: String,
    pub authorize_url: String,
    pub redirect_uri: String,
    pub expires_at_ms: u64,
    pub usage_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
#[serde(tag = "state")]
pub enum SiwcSignInStatusResult {
    #[serde(rename = "pending")]
    Pending {
        #[serde(rename = "flowId")]
        flow_id: String,
        #[serde(rename = "expiresAtMs")]
        expires_at_ms: u64,
    },
    #[serde(rename = "completed")]
    Completed {
        #[serde(rename = "flowId")]
        flow_id: String,
        #[serde(rename = "profileId")]
        profile_id: String,
    },
    #[serde(rename = "failed")]
    Failed {
        #[serde(rename = "flowId")]
        flow_id: String,
        error: SiwcPublicError,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
#[serde(rename_all = "camelCase")]
pub struct SiwcSignOutResult {
    pub profile_id: String,
    pub revoked: bool,
    pub had_refresh_token: bool,
    pub usage_url: String,
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
    use crate::{SiwcConfig, SiwcError, SiwcManager};

    #[test]
    fn actual_signed_out_manager_projections_match_public_dtos() {
        let dir = tempfile::tempdir().unwrap();
        let manager = SiwcManager::new(dir.path().to_owned(), SiwcConfig::default()).unwrap();
        let status = manager.status();
        let dto: SiwcStatusResult = serde_json::from_value(status.clone()).unwrap();
        assert_eq!(serde_json::to_value(dto).unwrap(), status);
        let profiles = manager.profiles();
        let dto: SiwcProfilesResult = serde_json::from_value(profiles.clone()).unwrap();
        assert_eq!(serde_json::to_value(dto).unwrap(), profiles);
    }

    #[test]
    fn all_public_error_codes_use_the_existing_wire_spellings() {
        let codes = [
            SiwcCode::DiscoveryUnavailable,
            SiwcCode::IdentityVerificationUnavailable,
            SiwcCode::InvalidIdToken,
            SiwcCode::RegistrationIncomplete,
            SiwcCode::AccountMismatch,
            SiwcCode::InvalidTokenResponse,
            SiwcCode::InvalidGrant,
            SiwcCode::InvalidClient,
            SiwcCode::CallbackPortUnavailable,
            SiwcCode::CallbackFailed,
            SiwcCode::SignInCancelled,
            SiwcCode::SignInRequired,
            SiwcCode::RefreshNotReady,
            SiwcCode::SharingNotEnabled,
            SiwcCode::ConnectionBusy,
            SiwcCode::ProfileNotFound,
            SiwcCode::StoreFailure,
            SiwcCode::InvalidConfig,
            SiwcCode::Transport,
            SiwcCode::UsageLimitExceeded,
            SiwcCode::UserNotEligible,
            SiwcCode::UsageUnavailable,
            SiwcCode::UnsupportedCapability,
            SiwcCode::RouteNotSupported,
            SiwcCode::InvalidUser,
            SiwcCode::ScopeNotAuthorized,
            SiwcCode::ProviderError,
        ];
        for code in codes {
            let value = SiwcError::new(code, "synthetic failure").to_json();
            let dto: SiwcPublicError = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(serde_json::to_value(dto).unwrap(), value);
            assert_eq!(serde_json::to_value(code).unwrap(), code.as_str());
        }
    }
}
