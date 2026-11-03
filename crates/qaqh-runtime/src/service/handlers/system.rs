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
        SystemCommand::ProfileList(_) => profile_list_snapshot(service),
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
            // D3:legacy profiles.toml 没有 default 指针(持久化 schema 是
            // PersistentProfiles);默认档落在 config 的 active_profile。
            let id = id
                .as_deref()
                .or(name.as_deref())
                .ok_or_else(|| QaqhError::invalid("missing 'id' or 'name' parameter"))?;
            service.update_config_and_reload(|cfg| {
                cfg.apply_profile(id)
                    .ok_or_else(|| format!("profile '{id}' not found"))?;
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
                persist_legacy_profiles(cfg.profiles.clone());
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
                // 同步摘除 legacy 文件条目——Config::load 会把文件里的
                // 残留重新导入 cfg.profiles,不摘等于没删。
                persist_legacy_profiles(cfg.profiles.clone());
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

/// legacy profiles 目录的读模型:qaqh_types::PersistentProfiles 才是该文件
/// 的持久化 schema(Config::load 同款读者);不得再用新 ProfilesDocument
/// 去读它(B06 修复:此前两种 schema 共写一个文件,有 profile 的机器必炸)。
fn legacy_profile_store() -> qaqh_types::ProfileStore {
    qaqh_types::ProfileStore::new(qaqh_profile::default_profiles_path())
}

/// cfg.profiles → legacy profiles.toml(持久化 schema PersistentProfiles)。
fn persist_legacy_profiles(profiles: std::collections::HashMap<String, qaqh_types::ProfileConfig>) {
    let store = legacy_profile_store();
    if !store.save(&qaqh_types::PersistentProfiles { profiles }) {
        log::warn!("[profile] legacy profiles.toml persist failed: {}", store.path().display());
    }
}

/// `profile.list`:权威来源是当前生效 config 的 profiles 映射(它已合并
/// legacy 文件与迁移态),投影成非敏感摘要 DTO。
fn profile_list_snapshot(_service: &crate::service::QaqhService) -> QaqhResult<Value> {
    let credential_wire = |source: qaqh_types::CredentialSource| match source {
        qaqh_types::CredentialSource::ApiKey => "api_key",
        qaqh_types::CredentialSource::ChatgptPlan => "chatgpt_plan",
        qaqh_types::CredentialSource::DeepseekAccount => "deepseek_account",
    };
    // 权威来源:当前生效 config 的 profiles 映射(已合并 legacy 文件与迁移态)。
    let profiles = qaqh_config::watch::authoritative()
        .map(|cfg| cfg.profiles.clone())
        .unwrap_or_else(|| {
            legacy_profile_store()
                .load()
                .map(|doc| doc.profiles)
                .unwrap_or_default()
        });
    let snapshot = crate::ringing::service_methods::ProfileListSnapshot {
        profiles: profiles
            .into_iter()
            .map(|(name, profile)| {
                let summary = qaqh_config_api::ProfileSummaryDto {
                    name: name.clone(),
                    model: profile.model,
                    base_url: profile.base_url,
                    wire: profile.wire.as_str().to_string(),
                    effort: profile.effort,
                    context_length: u64::from(profile.context_length),
                    max_tokens: u64::from(profile.max_tokens),
                    api_key_set: profile.api_key.is_some(),
                    credential: credential_wire(profile.credential).to_string(),
                };
                (name, summary)
            })
            .collect(),
    };
    serde_json::to_value(snapshot)
        .map_err(|error| QaqhError::invalid(format!("serialize profile list: {error}")))
}

/// 平铺 `profile.create`(legacy 目录):base_url/wire 可显式给出,或从
/// providers.toml 目录按 provider id 只读解析。落盘走 legacy schema
/// (PersistentProfiles),密钥进 secrets 槽,不触碰全局活跃配置。
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
    if id.is_empty() {
        return Err(QaqhError::invalid("profile id must be non-empty"));
    }

    let (resolved_base_url, resolved_wire) = match (base_url, provider) {
        (Some(url), _) => (url.to_string(), wire.unwrap_or("chat").to_string()),
        (None, Some(provider_id)) => {
            // 新 provider 目录只读解析端点;legacy profile 不再往里写条目。
            let catalog = qaqh_profile::ProfilesStore::load_or_create(
                &qaqh_profile::default_providers_path(),
            )
            .map_err(|error| QaqhError::context("load providers catalog failed", error))?;
            let def = catalog
                .document()
                .providers
                .get(provider_id)
                .ok_or_else(|| {
                    QaqhError::not_found(format!("provider '{provider_id}' not found"))
                })?;
            (
                def.base_url.clone(),
                wire.unwrap_or(def.wire.as_str()).to_string(),
            )
        }
        (None, None) => {
            return Err(QaqhError::invalid(
                "profile.create requires base_url or provider",
            ));
        }
    };
    let wire = qaqh_types::Wire::parse(&resolved_wire).unwrap_or_default();

    if let Some(key) = api_key.map(str::trim).filter(|key| !key.is_empty()) {
        qaqh_config::secrets::SecretStore::default_location()
            .set_profile_key(&id, key)
            .map_err(|error| QaqhError::context("failed to store api key", error))?;
    }
    let api_key_saved = api_key.is_some_and(|key| !key.trim().is_empty());

    let mut legacy = legacy_profile_store().load().unwrap_or_default();
    if legacy.profiles.contains_key(&id) {
        return Err(QaqhError::invalid(format!(
            "profile '{id}' already exists (no profile.update; delete first)"
        )));
    }
    legacy.profiles.insert(
        id.clone(),
        qaqh_types::ProfileConfig {
            model: model.to_string(),
            max_tokens: max_tokens.unwrap_or(8192),
            effort: reasoning_effort.map(str::to_string),
            context_length: context_length.unwrap_or(128_000),
            base_url: resolved_base_url,
            wire,
            compat: None,
            credential: qaqh_types::CredentialSource::ApiKey,
            api_key: api_key_saved.then(|| qaqh_config::secrets::CONFIG_MARKER.to_owned()),
            preset_endpoint: None,
            context_limit: None,
            context_window: None,
        },
    );
    persist_legacy_profiles(legacy.profiles);

    service.update_config_and_reload(|cfg| {
        if let Some(profile) = legacy_profile_store()
            .load()
            .and_then(|doc| doc.profiles.get(&id).cloned())
        {
            cfg.profiles.insert(id.clone(), profile);
        }
        Ok(())
    })?;

    log::info!(
        "[profile] successfully created profile '{id}' without polluting global active config"
    );
    Ok(Value::Null)
}
