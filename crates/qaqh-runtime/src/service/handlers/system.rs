//! Read-only system queries and RingingV2 global configuration mutations.

use serde_json::{Value, json};

use qaqh_ringing::RingingCommand;

use crate::error::{QaqhError, QaqhResult};
use crate::service::QaqhService;
use crate::service::command::SystemCommand;
use crate::service::stats::load_config;

pub(crate) fn handle(service: &QaqhService, command: SystemCommand) -> QaqhResult<Value> {
    match command {
        SystemCommand::DaemonVersion(_) => Ok(json!(env!("QAQH_BUILD_VERSION"))),
        SystemCommand::DiagnosticsMemorySnapshot(args) => service
            .diagnostics_memory_snapshot(args.after_sequence)
            .map_err(QaqhError::message),
        SystemCommand::ConfigLoad(_) => load_config().map_err(QaqhError::message),
        SystemCommand::ProfileList(_) => {
            let store = load_profiles()?;
            QaqhError::to_value("profiles", store.document())
        }
    }
}

pub(crate) fn handle_ringing(service: &QaqhService, command: &RingingCommand) -> QaqhResult<Value> {
    match command {
        RingingCommand::ConfigSave { patch } => {
            let patch: qaqh_config_api::ConfigPatch = serde_json::from_value(patch.clone())
                .map_err(|error| {
                    QaqhError::invalid(format!("invalid config.save payload: {error}"))
                })?;
            service.save_config(patch)?;
            load_config().map_err(QaqhError::message)
        }
        RingingCommand::ConfigSetPermissionLevel { level } => {
            if !(1..=3).contains(level) {
                return Err(QaqhError::invalid(format!(
                    "invalid permission level {level} (must be 1-3: 1=read-only, 2=workspace-write, 3=skip-permissions)"
                )));
            }
            service.update_config_and_reload(|cfg| {
                cfg.permission_level = *level;
                Ok(())
            })?;
            log::info!("[config] permission level set to {level}");
            Ok(json!({ "permission_level": level }))
        }
        RingingCommand::ProfileCreate {
            name,
            model,
            id,
            provider,
            base_url,
            wire,
            reasoning_effort,
            context_length,
            max_tokens,
            api_key,
        } => create_profile(
            service,
            name,
            model,
            id.as_deref(),
            provider.as_deref(),
            base_url.as_deref(),
            wire.as_deref(),
            reasoning_effort.as_deref(),
            *context_length,
            *max_tokens,
            api_key.as_deref(),
        ),
        RingingCommand::ProfileSetDefault { id, name } => {
            let id = id
                .as_deref()
                .or(name.as_deref())
                .ok_or_else(|| QaqhError::invalid("missing 'id' or 'name' parameter"))?;
            let path = qaqh_profile::default_profiles_path();
            let mut store = qaqh_profile::ProfilesStore::load_or_create(&path)
                .map_err(|error| QaqhError::context("failed to load profiles", error))?;
            store
                .set_default_profile(id)
                .map_err(|error| QaqhError::context("failed to set default profile", error))?;
            service.update_config_and_reload(|cfg| {
                let _ = cfg.apply_profile(id);
                Ok(())
            })?;
            Ok(Value::Null)
        }
        RingingCommand::ProfileApply { name } => {
            service.update_config_and_reload(|cfg| {
                if cfg.apply_profile(name).is_none() {
                    return Err(format!("profile '{name}' not found"));
                }
                Ok(())
            })?;
            Ok(Value::Null)
        }
        RingingCommand::ProfileSaveCurrent { name } => {
            let secrets = qaqh_config::secrets::SecretStore::default_location();
            let key = qaqh_config::watch::authoritative()
                .map(|cfg| cfg.api_key)
                .unwrap_or_default();
            let saved_key = !key.is_empty();
            let stored = if saved_key {
                secrets.set_profile_key(name, &key)
            } else {
                secrets.delete_profile_key(name)
            };
            stored.map_err(|error| QaqhError::context("failed to store profile api key", error))?;
            service.update_config_and_reload(|cfg| {
                cfg.save_profile(name, saved_key);
                Ok(())
            })?;
            Ok(Value::Null)
        }
        RingingCommand::ProfileDelete { name } => {
            service.update_config_and_reload(|cfg| {
                if !cfg.delete_profile(name) {
                    return Err(format!(
                        "profile '{name}' cannot be deleted (not found or default)"
                    ));
                }
                Ok(())
            })?;
            if let Err(error) =
                qaqh_config::secrets::SecretStore::default_location().delete_profile_key(name)
            {
                log::warn!("[profile] secret cleanup for profile '{name}' skipped: {error}");
            }
            Ok(Value::Null)
        }
        RingingCommand::DiagnosticsMemoryStart {} => {
            let monitor = qaqh_memwatch::global();
            monitor.start();
            service.refresh_memory_components();
            Ok(json!({
                "enabled": true,
                "started_at_ms": monitor.started_at_ms()
            }))
        }
        RingingCommand::DiagnosticsMemoryStop {} => {
            qaqh_memwatch::global().stop();
            Ok(json!({ "enabled": false }))
        }
        _ => Err(QaqhError::invalid(format!(
            "{} is not a global configuration command",
            command.name()
        ))),
    }
}

fn load_profiles() -> QaqhResult<qaqh_profile::ProfilesStore> {
    let path = qaqh_profile::default_profiles_path();
    qaqh_profile::ProfilesStore::load_or_create(&path)
        .map_err(|error| QaqhError::context("failed to load profiles", error))
}

#[allow(clippy::too_many_arguments)]
fn create_profile(
    service: &QaqhService,
    name: &str,
    model: &str,
    requested_id: Option<&str>,
    provider: Option<&str>,
    base_url: Option<&str>,
    wire: Option<&str>,
    reasoning_effort: Option<&str>,
    context_length: Option<u32>,
    max_tokens: Option<u32>,
    api_key: Option<&str>,
) -> QaqhResult<Value> {
    let id = requested_id.unwrap_or(name).trim().to_string();
    let mut store = load_profiles()?;
    let wire = qaqh_types::Wire::parse(wire.unwrap_or("chat")).unwrap_or_default();

    let provider_id = if let Some(provider) = provider {
        provider.to_string()
    } else if let Some(base_url) = base_url {
        let provider_id = format!("provider-{id}");
        let provider = qaqh_profile::ProviderDef {
            name: format!("{name} 服务商"),
            base_url: base_url.to_string(),
            wire,
            credential: qaqh_types::CredentialSource::ApiKey,
            api_key: None,
            headers: std::collections::BTreeMap::new(),
            compat: None,
            models: std::collections::BTreeMap::new(),
        };
        store
            .upsert_provider(provider_id.clone(), provider)
            .map_err(|error| QaqhError::context("failed to create provider", error))?;
        provider_id
    } else {
        store.document().default_profile.clone().unwrap_or_default()
    };

    let profile = qaqh_profile::ProfileDef {
        name: name.to_string(),
        provider: provider_id,
        model: model.to_string(),
        reasoning_effort: reasoning_effort.map(str::to_string),
        context_length,
        max_tokens,
    };

    if let Some(key) = api_key.map(str::trim).filter(|key| !key.is_empty()) {
        qaqh_config::secrets::SecretStore::default_location()
            .set_profile_key(&id, key)
            .map_err(|error| QaqhError::context("failed to store api key", error))?;
    }

    store
        .create_profile(id.clone(), profile)
        .map_err(|error| QaqhError::context("failed to create profile", error))?;

    let api_key_saved = api_key.is_some_and(|key| !key.trim().is_empty());
    service.update_config_and_reload(|cfg| {
        cfg.profiles.insert(
            id.clone(),
            qaqh_types::ProfileConfig {
                model: model.to_string(),
                max_tokens: max_tokens.unwrap_or(8192),
                effort: reasoning_effort.map(str::to_string),
                context_length: context_length.unwrap_or(128000),
                base_url: base_url.unwrap_or_default().to_string(),
                wire,
                compat: None,
                credential: qaqh_types::CredentialSource::ApiKey,
                api_key: api_key_saved.then(|| qaqh_config::secrets::CONFIG_MARKER.to_owned()),
                preset_endpoint: None,
                context_limit: None,
                context_window: None,
            },
        );
        Ok(())
    })?;

    log::info!(
        "[profile] successfully created profile '{id}' without polluting global active config"
    );
    Ok(Value::Null)
}
