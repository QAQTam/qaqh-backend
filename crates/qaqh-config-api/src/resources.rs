//! Resource reads for configuration editors. These are declarations, not actor execution state.

use serde::{Deserialize, Serialize};

use crate::{CredentialSource, EndpointCompat, Wire};

/// Shared method identifiers for the typed client and the service table.
pub mod methods {
    pub const CATALOG: &str = "config.catalog";
    pub const PROFILE: &str = "config.profile";
    pub const HOST: &str = "config.host";
    pub const ALL: &[&str] = &[CATALOG, PROFILE, HOST];
}

/// Stable failures for resource editors. Error messages never include source file contents.
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigQueryError {
    #[serde(rename = "invalid_config_request")]
    InvalidRequest,
    ConfigurationUnavailable,
    InvalidConfiguration,
    #[serde(rename = "config_migration_required")]
    MigrationRequired,
    ProfileNotFound,
    #[serde(rename = "invalid_default_profile")]
    InvalidDefault,
}

impl ConfigQueryError {
    pub fn code(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_config_request",
            Self::ConfigurationUnavailable => "configuration_unavailable",
            Self::InvalidConfiguration => "invalid_configuration",
            Self::MigrationRequired => "config_migration_required",
            Self::ProfileNotFound => "profile_not_found",
            Self::InvalidDefault => "invalid_default_profile",
        }
    }
}

impl std::fmt::Display for ConfigQueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
}

impl std::error::Error for ConfigQueryError {}

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConfigProfileRequest {
    /// The current on-disk profile name. Renames/versioned identity are not implied.
    pub profile_id: String,
}

/// A single read of the profile directory. Editing a row does not select a default.
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigCatalog {
    pub default_profile_id: String,
    /// Sorted by profile_id; includes non-default profiles in full.
    pub profiles: Vec<ProfileView>,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileView {
    pub profile_id: String,
    pub model: String,
    pub base_url: String,
    pub wire: Wire,
    pub credential: CredentialDeclaration,
    pub max_tokens: u32,
    pub context_length: u32,
    /// None means endpoint default, not an inferred effort setting.
    pub reasoning_effort: Option<String>,
    pub compat: EndpointCompat,
}

/// Local configuration declarations only. No token, secret value, path or remote validity claim.
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CredentialDeclaration {
    /// The profile declares its own secret slot; this does not prove it can be decrypted.
    ProfileKey,
    /// The existing format explicitly inherits the host key; the new binding model will replace it.
    HostKey,
    ProviderAccount {
        source: CredentialSource,
    },
}

/// Host-wide service settings, deliberately excluding client preferences and model selection.
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostConfigView {
    pub permission_level: u8,
    pub auto_compact_threshold: f64,
    pub compliance_enabled: bool,
    pub default_shell: Option<String>,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub session_idle_unload_secs: u64,
    /// Restart-required host policy; not a per-session limit or an execution receipt.
    pub subagent_max_depth: u32,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub message_in_flight_per_pair: u64,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub message_outbound_per_sender: u64,
    pub mcp: IntegrationSettingsView,
    pub lsp: IntegrationSettingsView,
}

/// Configuration only. Availability/health belongs to the integration supervisor.
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IntegrationSettingsView {
    pub enabled: bool,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub idle_shutdown_secs: u64,
    /// Sorted local server names; no env/header/argv secret-bearing values are exposed.
    pub servers: Vec<String>,
}
