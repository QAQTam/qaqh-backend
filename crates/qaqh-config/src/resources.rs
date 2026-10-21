//! Read-only configuration resources. No secret resolution, migration, caching or file creation.

use std::path::PathBuf;

use qaqh_config_api::{
    ConfigCatalog, CredentialDeclaration, CredentialSource, HostConfigView,
    IntegrationSettingsView, ProfileView,
};
use qaqh_types::{PersistentConfig, ProfileConfig};

/// JSON numbers must be exactly representable by TypeScript consumers.
const MAX_JSON_INTEGER: u64 = (1_u64 << 53) - 1;

/// Failures belong to this read boundary. Parser diagnostics are intentionally not public:
/// they can contain the source line of a secret-bearing configuration file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigReadError {
    Unavailable,
    InvalidDocument,
    MigrationRequired,
    ProfileNotFound { profile_id: String },
    InvalidDefault,
}

impl std::fmt::Display for ConfigReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable => f.write_str("configuration unavailable"),
            Self::InvalidDocument => f.write_str("invalid configuration document"),
            Self::MigrationRequired => f.write_str("configuration requires host migration"),
            Self::ProfileNotFound { profile_id } => write!(f, "profile not found: {profile_id}"),
            Self::InvalidDefault => f.write_str("default profile is missing"),
        }
    }
}

impl std::error::Error for ConfigReadError {}

/// An explicit path makes separate hosts/tests independent. The host owns write exclusion.
#[derive(Debug, Clone)]
pub struct ConfigReader {
    path: PathBuf,
}

impl ConfigReader {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    fn document(&self) -> Result<PersistentConfig, ConfigReadError> {
        let text = std::fs::read_to_string(&self.path).map_err(|_| ConfigReadError::Unavailable)?;
        let document: PersistentConfig =
            toml::from_str(&text).map_err(|_| ConfigReadError::InvalidDocument)?;
        if document.provider_id.is_some()
            || document.model.is_some()
            || document.base_url.is_some()
            || document.max_tokens.is_some()
            || document.context_limit.is_some()
            || document.endpoint.is_some()
            || document.reasoning_effort.is_some()
            || document.permission_level.is_some()
            || document.profiles.as_ref().is_some_and(|profiles| {
                profiles.values().any(|profile| {
                    profile.preset_endpoint.is_some()
                        || profile.context_limit.is_some()
                        || profile.context_window.is_some()
                })
            })
        {
            return Err(ConfigReadError::MigrationRequired);
        }
        if document.profiles.as_ref().is_some_and(|profiles| {
            profiles.values().any(|profile| {
                profile
                    .compat
                    .as_ref()
                    .and_then(|compat| compat.retry.as_ref())
                    .is_some_and(|retry| {
                        [
                            retry.base_delay_secs,
                            retry.max_delay_secs,
                            retry.idle_timeout_secs,
                        ]
                        .into_iter()
                        .any(|value| value > MAX_JSON_INTEGER)
                    })
            })
        }) {
            return Err(ConfigReadError::InvalidDocument);
        }
        Ok(document)
    }

    pub fn catalog(&self) -> Result<ConfigCatalog, ConfigReadError> {
        let document = self.document()?;
        let default_profile_id = document
            .active_profile
            .ok_or(ConfigReadError::InvalidDefault)?;
        let profiles = document.profiles.unwrap_or_default();
        if !profiles.contains_key(&default_profile_id) {
            return Err(ConfigReadError::InvalidDefault);
        }
        let mut profiles: Vec<_> = profiles
            .into_iter()
            .map(|(id, profile)| profile_view(id, profile))
            .collect();
        profiles.sort_by(|left, right| left.profile_id.cmp(&right.profile_id));
        Ok(ConfigCatalog {
            default_profile_id,
            profiles,
        })
    }

    pub fn profile(&self, profile_id: &str) -> Result<ProfileView, ConfigReadError> {
        self.document()?
            .profiles
            .unwrap_or_default()
            .remove(profile_id)
            .map(|profile| profile_view(profile_id.to_owned(), profile))
            .ok_or_else(|| ConfigReadError::ProfileNotFound {
                profile_id: profile_id.to_owned(),
            })
    }

    pub fn host(&self) -> Result<HostConfigView, ConfigReadError> {
        let document = self.document()?;
        let permission_level = document.permission_tier.unwrap_or(1);
        if !(1..=3).contains(&permission_level) {
            return Err(ConfigReadError::InvalidDocument);
        }
        let threshold = document.auto_compact_threshold.unwrap_or(0.75);
        if !threshold.is_finite() || !(0.0..=1.0).contains(&threshold) {
            return Err(ConfigReadError::InvalidDocument);
        }
        let subagent = document.subagent.unwrap_or_default();
        let subagent_defaults = crate::config::SubagentConfig::default();
        let mcp = document.mcp.unwrap_or_default();
        let lsp = document.lsp.unwrap_or_default();
        let mcp_defaults = crate::config::McpConfig::default();
        let lsp_defaults = crate::config::LspConfig::default();
        let mut mcp_servers: Vec<_> = mcp.servers.unwrap_or_default().into_keys().collect();
        let mut lsp_servers: Vec<_> = lsp.servers.unwrap_or_default().into_keys().collect();
        mcp_servers.sort();
        lsp_servers.sort();
        let view = HostConfigView {
            permission_level,
            auto_compact_threshold: threshold,
            compliance_enabled: document.compliance_enabled.unwrap_or(true),
            default_shell: document.exec.and_then(|exec| exec.default_shell),
            session_idle_unload_secs: document.session_idle_unload_secs.unwrap_or(0),
            subagent_max_depth: subagent.max_depth.unwrap_or(subagent_defaults.max_depth),
            message_in_flight_per_pair: subagent
                .message_in_flight_per_pair
                .unwrap_or(subagent_defaults.message_in_flight_per_pair),
            message_outbound_per_sender: subagent
                .message_outbound_per_sender
                .unwrap_or(subagent_defaults.message_outbound_per_sender),
            mcp: IntegrationSettingsView {
                enabled: mcp.enabled.unwrap_or(!mcp_servers.is_empty()),
                idle_shutdown_secs: mcp
                    .idle_shutdown_secs
                    .unwrap_or(mcp_defaults.idle_shutdown_secs),
                servers: mcp_servers,
            },
            lsp: IntegrationSettingsView {
                enabled: lsp.enabled.unwrap_or(!lsp_servers.is_empty()),
                idle_shutdown_secs: lsp
                    .idle_shutdown_secs
                    .unwrap_or(lsp_defaults.idle_shutdown_secs),
                servers: lsp_servers,
            },
        };
        if [
            view.session_idle_unload_secs,
            view.message_in_flight_per_pair,
            view.message_outbound_per_sender,
            view.mcp.idle_shutdown_secs,
            view.lsp.idle_shutdown_secs,
        ]
        .into_iter()
        .any(|value| value > MAX_JSON_INTEGER)
        {
            return Err(ConfigReadError::InvalidDocument);
        }
        Ok(view)
    }
}

fn profile_view(profile_id: String, profile: ProfileConfig) -> ProfileView {
    let credential = match profile.credential {
        CredentialSource::ApiKey if profile.api_key.is_some() => CredentialDeclaration::ProfileKey,
        CredentialSource::ApiKey => CredentialDeclaration::HostKey,
        source => CredentialDeclaration::ProviderAccount { source },
    };
    ProfileView {
        profile_id,
        compat: profile.compat_or_default(),
        model: profile.model,
        base_url: profile.base_url,
        wire: profile.wire,
        credential,
        max_tokens: profile.max_tokens,
        context_length: profile.context_length,
        reasoning_effort: profile.effort,
    }
}
