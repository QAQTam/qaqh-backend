//! Versioned configuration resource commands. These are pure wire contracts;
//! this crate performs no persistence, secret handling, or routing.

use serde::{Deserialize, Serialize};

fn deserialize_field_patch<'de, D, T>(deserializer: D) -> Result<Option<FieldPatch<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    FieldPatch::<T>::deserialize(deserializer).map(Some)
}

/// A canonical positive decimal revision on the wire. The repository owns
/// allocation; callers may only echo revisions returned by a read/receipt.
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct Revision(pub String);

impl Revision {
    pub fn validate(&self) -> Result<(), &'static str> {
        let value = self.0.as_bytes();
        if value.is_empty()
            || value == b"0"
            || value[0] == b'0'
            || !value.iter().all(u8::is_ascii_digit)
        {
            return Err("revision must be a positive canonical decimal string");
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for Revision {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let revision = Self(String::deserialize(deserializer)?);
        revision.validate().map_err(serde::de::Error::custom)?;
        Ok(revision)
    }
}

/// Idempotency key supplied by the caller. Its syntax is bounded by the service;
/// it is not a secret and is not a revision.
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct RequestId(pub String);

impl RequestId {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.0.is_empty() || self.0.len() > 128 || self.0.chars().any(char::is_control) {
            return Err("requestId must contain 1..=128 non-control bytes");
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for RequestId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let request_id = Self(String::deserialize(deserializer)?);
        request_id.validate().map_err(serde::de::Error::custom)?;
        Ok(request_id)
    }
}

/// Typed three-state update. Omitted containing field means Unchanged;
/// `{"op":"set","value":...}` means Set; `{"op":"clear"}` means Clear.
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum FieldPatch<T> {
    Set { value: T },
    Clear,
}

/// Opaque credential binding reference. It carries no account token or secret.
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CredentialBindingRef {
    pub credential_id: String,
    pub revision: Revision,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProfileVersionRef {
    pub profile_id: String,
    pub revision: Revision,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProfileResource {
    pub profile_id: String,
    pub revision: Revision,
    pub display_name: String,
    pub model: String,
    pub base_url: String,
    pub wire: crate::Wire,
    pub max_tokens: u32,
    pub context_length: u32,
    pub reasoning_effort: Option<String>,
    pub compat: crate::EndpointCompat,
    pub credential: Option<CredentialBindingRef>,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateProfileRequest {
    pub request_id: RequestId,
    pub display_name: String,
    pub model: String,
    pub base_url: String,
    pub wire: crate::Wire,
    pub max_tokens: u32,
    pub context_length: u32,
    pub reasoning_effort: Option<String>,
    pub compat: crate::EndpointCompat,
    pub credential: Option<CredentialBindingRef>,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PublishProfileRequest {
    pub request_id: RequestId,
    pub profile_id: String,
    pub expected_revision: Revision,
    pub patch: ProfilePatch,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProfilePatch {
    #[serde(default, deserialize_with = "deserialize_field_patch")]
    pub display_name: Option<FieldPatch<String>>,
    #[serde(default, deserialize_with = "deserialize_field_patch")]
    pub model: Option<FieldPatch<String>>,
    #[serde(default, deserialize_with = "deserialize_field_patch")]
    pub base_url: Option<FieldPatch<String>>,
    #[serde(default, deserialize_with = "deserialize_field_patch")]
    pub wire: Option<FieldPatch<crate::Wire>>,
    #[serde(default, deserialize_with = "deserialize_field_patch")]
    pub max_tokens: Option<FieldPatch<u32>>,
    #[serde(default, deserialize_with = "deserialize_field_patch")]
    pub context_length: Option<FieldPatch<u32>>,
    #[serde(default, deserialize_with = "deserialize_field_patch")]
    pub reasoning_effort: Option<FieldPatch<String>>,
    #[serde(default, deserialize_with = "deserialize_field_patch")]
    pub compat: Option<FieldPatch<crate::EndpointCompat>>,
    #[serde(default, deserialize_with = "deserialize_field_patch")]
    pub credential: Option<FieldPatch<CredentialBindingRef>>,
}

impl ProfilePatch {
    /// Reject Clear for fields whose resource contract requires a value.
    /// `None` remains Unchanged; the two optional fields accept explicit Clear.
    pub fn validate(&self) -> Result<(), &'static str> {
        if matches!(self.display_name, Some(FieldPatch::Clear))
            || matches!(self.model, Some(FieldPatch::Clear))
            || matches!(self.base_url, Some(FieldPatch::Clear))
            || matches!(self.wire, Some(FieldPatch::Clear))
            || matches!(self.max_tokens, Some(FieldPatch::Clear))
            || matches!(self.context_length, Some(FieldPatch::Clear))
            || matches!(self.compat, Some(FieldPatch::Clear))
        {
            return Err("clear is not allowed for required profile fields");
        }
        Ok(())
    }
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeleteProfileRequest {
    pub request_id: RequestId,
    pub profile_id: String,
    pub expected_revision: Revision,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UpdateDefaultsRequest {
    pub request_id: RequestId,
    pub expected_revision: Revision,
    pub profile: FieldPatch<ProfileVersionRef>,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UpdateHostRequest {
    pub request_id: RequestId,
    pub expected_revision: Revision,
    pub patch: HostPatch,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HostPatch {
    #[serde(default, deserialize_with = "deserialize_field_patch")]
    pub permission_level: Option<FieldPatch<u8>>,
    #[serde(default, deserialize_with = "deserialize_field_patch")]
    pub auto_compact_threshold: Option<FieldPatch<f64>>,
    #[serde(default, deserialize_with = "deserialize_field_patch")]
    pub compliance_enabled: Option<FieldPatch<bool>>,
    #[serde(default, deserialize_with = "deserialize_field_patch")]
    pub default_shell: Option<FieldPatch<String>>,
    #[serde(default, deserialize_with = "deserialize_field_patch")]
    pub session_idle_unload_secs: Option<FieldPatch<u64>>,
}

impl HostPatch {
    /// Only the optional shell setting can be cleared; other host settings
    /// require an explicit Set value.
    pub fn validate(&self) -> Result<(), &'static str> {
        if matches!(self.permission_level, Some(FieldPatch::Clear))
            || matches!(self.auto_compact_threshold, Some(FieldPatch::Clear))
            || matches!(self.compliance_enabled, Some(FieldPatch::Clear))
            || matches!(self.session_idle_unload_secs, Some(FieldPatch::Clear))
        {
            return Err("clear is not allowed for required host fields");
        }
        Ok(())
    }
}

/// Secret-bearing input intentionally has no Debug implementation. Provider
/// account linking uses a separately authorized binding flow, never raw tokens here.
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SetCredentialRequest {
    pub request_id: RequestId,
    pub credential_id: String,
    pub expected_revision: Option<Revision>,
    pub provider_id: String,
    pub secret: String,
}

impl std::fmt::Debug for SetCredentialRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SetCredentialRequest")
            .field("request_id", &self.request_id)
            .field("credential_id", &self.credential_id)
            .field("expected_revision", &self.expected_revision)
            .field("provider_id", &self.provider_id)
            .field("secret", &"[REDACTED]")
            .finish()
    }
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClearCredentialRequest {
    pub request_id: RequestId,
    pub credential_id: String,
    pub expected_revision: Revision,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PersistedReceipt {
    pub request_id: RequestId,
    pub resource_id: String,
    pub revision: Revision,
    pub persisted: bool,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CommandFailure {
    pub code: CommandErrorCode,
    pub current_profile: Option<ProfileResource>,
    pub current_revision: Option<Revision>,
    pub current_host: Option<crate::HostConfigView>,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandErrorCode {
    InvalidRequest,
    RevisionConflict,
    ProfileUnavailable,
    CredentialUnavailable,
    InvalidBinding,
    PersistenceUnavailable,
    RepositoryClosed,
    ExternalModification,
    IdempotencyConflict,
    RestartRequired,
    ConfigurationUpgradeRequired,
}

impl CommandErrorCode {
    pub const fn code(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request",
            Self::RevisionConflict => "revision_conflict",
            Self::ProfileUnavailable => "profile_unavailable",
            Self::CredentialUnavailable => "credential_unavailable",
            Self::InvalidBinding => "invalid_binding",
            Self::PersistenceUnavailable => "persistence_unavailable",
            Self::RepositoryClosed => "repository_closed",
            Self::ExternalModification => "external_modification",
            Self::IdempotencyConflict => "idempotency_conflict",
            Self::RestartRequired => "restart_required",
            Self::ConfigurationUpgradeRequired => "configuration_upgrade_required",
        }
    }
}
