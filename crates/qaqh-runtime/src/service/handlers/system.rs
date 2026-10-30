//! `daemon.*` / `diagnostics.*` / `config.*` / `profile.*` 服务面。

use serde_json::{Value, json};

use crate::error::{QaqhError, QaqhResult};
use crate::service::QaqhService;
use crate::service::command::SystemCommand;
use crate::service::stats::load_config;

pub(crate) fn handle(service: &QaqhService, command: SystemCommand) -> QaqhResult<Value> {
    match command {
        SystemCommand::DaemonVersion(_) => Ok(json!(env!("QAQH_BUILD_VERSION"))),
        SystemCommand::DiagnosticsMemoryStart(_) => {
            let monitor = qaqh_memwatch::global();
            monitor.start();
            service.refresh_memory_components();
            Ok(json!({
                "enabled": true,
                "started_at_ms": monitor.started_at_ms()
            }))
        }
        SystemCommand::DiagnosticsMemoryStop(_) => {
            qaqh_memwatch::global().stop();
            Ok(json!({ "enabled": false }))
        }
        SystemCommand::DiagnosticsMemorySnapshot(args) => service
            .diagnostics_memory_snapshot(args.after_sequence)
            .map_err(QaqhError::message),
        SystemCommand::ConfigLoad(_) => load_config().map_err(QaqhError::message),
        SystemCommand::ConfigSave(args) => {
            let patch: qaqh_config_api::ConfigPatch =
                serde_json::from_value(Value::Object(args.patch)).map_err(|error| {
                    QaqhError::invalid(format!("invalid config.save payload: {error}"))
                })?;
            service.save_config(patch)?;
            Ok(Value::Null)
        }
        // 权限档位（三档：read-only / workspace-write / skip-permissions）：
        // 与 config.save 共用 Config::update 单写口；校验 → 写 config.toml →
        // 广播 AgentReloadConfig 让所有活跃 worker（含子代理，子代理继承同一
        // 全局权限）重载。
        SystemCommand::ConfigSetPermissionLevel(args) => {
            let level = args.level;
            if !(1..=3).contains(&level) {
                return Err(QaqhError::invalid(format!(
                    "invalid permission level {level} (must be 1-3: 1=read-only, 2=workspace-write, 3=skip-permissions)"
                )));
            }
            service.update_config_and_reload(|cfg| {
                cfg.permission_level = level;
                Ok(())
            })?;
            log::info!("[config] permission level set to {level}");
            Ok(json!({ "permission_level": level }))
        }
        SystemCommand::ProfileList(_) => {
            let store = load_profiles()?;
            QaqhError::to_value("profiles", store.document())
        }
        SystemCommand::ProfileSetDefault(args) => {
            let id = args.target()?.to_string();
            let path = qaqh_profile::default_profiles_path();
            let mut store = qaqh_profile::ProfilesStore::load_or_create(&path)
                .map_err(|error| QaqhError::context("failed to load profiles", error))?;
            store
                .set_default_profile(&id)
                .map_err(|error| QaqhError::context("failed to set default profile", error))?;
            service.update_config_and_reload(|cfg| {
                let _ = cfg.apply_profile(&id);
                Ok(())
            })?;
            Ok(Value::Null)
        }
        SystemCommand::ProfileCreate(args) => create_profile(service, args),
        SystemCommand::ProfileApply(args) => {
            let name = args.name;
            service.update_config_and_reload(|cfg| {
                if cfg.apply_profile(&name).is_none() {
                    return Err(format!("profile '{name}' not found"));
                }
                Ok(())
            })?;
            Ok(Value::Null)
        }
        SystemCommand::ProfileSaveCurrent(args) => {
            let name = args.name;
            // 顺序不可反：先把当前主密钥写进该 profile 的 secret 槽，再落
            // `"set"` 标记。反了就会留下"声称有 key 却取不到"的 profile。
            // 无 key 时同步清掉旧槽位，保证标记与 secret 始终一致。
            let secrets = qaqh_config::secrets::SecretStore::default_location();
            let key = qaqh_config::watch::authoritative()
                .map(|cfg| cfg.api_key)
                .unwrap_or_default();
            let saved_key = !key.is_empty();
            let stored = if saved_key {
                secrets.set_profile_key(&name, &key)
            } else {
                secrets.delete_profile_key(&name)
            };
            stored.map_err(|error| QaqhError::context("failed to store profile api key", error))?;
            service.update_config_and_reload(|cfg| {
                cfg.save_profile(&name, saved_key);
                Ok(())
            })?;
            Ok(Value::Null)
        }
        SystemCommand::ProfileDelete(args) => {
            let name = args.name;
            service.update_config_and_reload(|cfg| {
                if !cfg.delete_profile(&name) {
                    return Err(format!(
                        "profile '{name}' cannot be deleted (not found or default)"
                    ));
                }
                Ok(())
            })?;
            // profile 已从配置摘除；secret 槽位随后清理。名字不合法等失败
            // 只告警——配置里已经不存在引用它的条目了。
            if let Err(error) =
                qaqh_config::secrets::SecretStore::default_location().delete_profile_key(&name)
            {
                log::warn!("[profile] secret cleanup for profile '{name}' skipped: {error}");
            }
            Ok(Value::Null)
        }
    }
}

fn load_profiles() -> QaqhResult<qaqh_profile::ProfilesStore> {
    let path = qaqh_profile::default_profiles_path();
    qaqh_profile::ProfilesStore::load_or_create(&path)
        .map_err(|error| QaqhError::context("failed to load profiles", error))
}

fn create_profile(
    service: &QaqhService,
    args: crate::service::command::ProfileCreateArgs,
) -> QaqhResult<Value> {
    let name = args.name.clone();
    let id = args.profile_id();
    let model = args.model.clone();
    let mut store = load_profiles()?;

    let provider_id = if let Some(provider) = args.provider.clone() {
        provider
    } else if let Some(base_url) = args.base_url.clone() {
        let pid = format!("provider-{id}");
        let provider = qaqh_profile::ProviderDef {
            name: format!("{name} 服务商"),
            base_url,
            wire: args.wire(),
            credential: qaqh_types::CredentialSource::ApiKey,
            api_key: None,
            headers: std::collections::BTreeMap::new(),
            compat: None,
            models: std::collections::BTreeMap::new(),
        };
        store
            .upsert_provider(pid.clone(), provider)
            .map_err(|error| QaqhError::context("failed to create provider", error))?;
        pid
    } else {
        store.document().default_profile.clone()
    };

    let reasoning_effort = args.reasoning_effort.clone();
    let context_length = args.context_length;
    let max_tokens = args.max_tokens;

    let profile = qaqh_profile::ProfileDef {
        name: name.clone(),
        provider: provider_id,
        model: model.clone(),
        reasoning_effort: reasoning_effort.clone(),
        context_length,
        max_tokens,
    };

    if let Some(key) = args.api_key() {
        let trimmed_key = key.trim();
        if !trimmed_key.is_empty() {
            let secrets = qaqh_config::secrets::SecretStore::default_location();
            secrets
                .set_profile_key(&id, trimmed_key)
                .map_err(|error| QaqhError::context("failed to store api key", error))?;
        }
    }

    store
        .create_profile(id.clone(), profile)
        .map_err(|error| QaqhError::context("failed to create profile", error))?;

    let base_url = args.base_url.clone().unwrap_or_default();
    let wire = args.wire();
    let has_key = args.api_key().is_some_and(|key| !key.trim().is_empty());

    service.update_config_and_reload(|cfg| {
        cfg.profiles.insert(
            id.clone(),
            qaqh_types::ProfileConfig {
                model: model.clone(),
                max_tokens: max_tokens.unwrap_or(8192),
                effort: reasoning_effort.clone(),
                context_length: context_length.unwrap_or(128000),
                base_url: base_url.clone(),
                wire,
                compat: None,
                credential: qaqh_types::CredentialSource::ApiKey,
                api_key: has_key.then(|| qaqh_config::secrets::CONFIG_MARKER.to_owned()),
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
