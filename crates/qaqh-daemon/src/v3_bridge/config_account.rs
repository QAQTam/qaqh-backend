use std::collections::BTreeSet;

use qaqh_core_vocab::JsonText;
use qaqh_ringing_v3::{
    AccountRequest, AccountResponse, ClientKind, ConfigRequest, ConfigResponse, ConfigSection,
    DeviceView, ErrorCode, PairingRequest, PairingResponse, RpcError,
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use super::{RealBackend, bad_request, internal, json_text};
use crate::v3_server::error::{denied, not_found, rpc};
use crate::v3_server::facade::CallContext;

mod account_map;
mod support;

use support::{identity_scope, now_ms, redact};

#[rustfmt::skip]
pub(crate) async fn handle_config(
    backend: &RealBackend,
    context: &CallContext,
    request: ConfigRequest,
) -> Result<ConfigResponse, RpcError> {
    if matches!(
        request,
        ConfigRequest::Set { .. }
            | ConfigRequest::Reload
            | ConfigRequest::PatchFull { .. }
            | ConfigRequest::UpsertProvider { .. }
            | ConfigRequest::DeleteProvider { .. }
            | ConfigRequest::UpsertProviderModel { .. }
            | ConfigRequest::DeleteProviderModel { .. }
            | ConfigRequest::SetDefaultModel { .. }
            | ConfigRequest::ReloadAgents
    ) {
        context.identity.require_owner("Config::Write/Reload")?;
    }
    match request {
        ConfigRequest::Get { section } => {
            let value = config_section(backend, section).await?;
            Ok(ConfigResponse::Value {
                section,
                value: json_text(&redact(value))?,
            })
        }
        ConfigRequest::Set { section, value } => {
            set_config_section(backend, section, &value).await?;
            Ok(ConfigResponse::Stored)
        }
        ConfigRequest::Reload => {
            RealBackend::blocking(qaqh_config::Config::load).await?;
            Ok(ConfigResponse::Reloaded)
        }
        ConfigRequest::LoadFull => {
            let value = backend.json_call("config.load", json!({})).await?;
            Ok(ConfigResponse::FullDocument {
                raw: json_text(&redact(value))?,
            })
        }
        ConfigRequest::PatchFull { patch } => {
            let patch = parse_json(&patch, "ConfigPatch")?;
            save_patch(backend, patch).await?;
            Ok(ConfigResponse::Stored)
        }
        ConfigRequest::ListProviders => {
            let raw = provider_catalog(backend).await?;
            let revision = revision(&raw)?;
            Ok(ConfigResponse::Providers {
                raw: json_text(&redact(raw))?,
                revision,
            })
        }
        ConfigRequest::ListProviderPresets => {
            let service = backend.service.clone();
            let presets = RealBackend::blocking(move || service.provider_presets()).await?;
            let entries = presets
                .as_array()
                .ok_or_else(|| internal("provider presets must be an array"))?;
            let mut views = Vec::with_capacity(entries.len());
            for entry in entries {
                let id = entry["id"]
                    .as_str()
                    .ok_or_else(|| internal("provider preset missing id"))?
                    .to_string();
                let name = entry["name"]
                    .as_str()
                    .ok_or_else(|| internal("provider preset missing name"))?
                    .to_string();
                let base_url = entry["baseUrl"]
                    .as_str()
                    .ok_or_else(|| internal("provider preset missing baseUrl"))?
                    .to_string();
                let wire = match entry["wire"].as_str() {
                    Some("chat") => qaqh_core_vocab::Wire::Chat,
                    Some("responses") => qaqh_core_vocab::Wire::Responses,
                    Some("messages") => qaqh_core_vocab::Wire::Messages,
                    Some("gemini") => qaqh_core_vocab::Wire::Gemini,
                    other => {
                        return Err(internal(format!(
                            "unknown provider preset wire: {other:?}"
                        )));
                    }
                };
                let credential = match entry["credential"].as_str() {
                    Some("api_key") => qaqh_core_vocab::CredentialSource::ApiKey,
                    Some("chatgpt_plan") => qaqh_core_vocab::CredentialSource::ChatgptPlan,
                    Some("deepseek_account") => qaqh_core_vocab::CredentialSource::DeepseekAccount,
                    other => {
                        return Err(internal(format!(
                            "unknown provider preset credential: {other:?}"
                        )));
                    }
                };
                views.push(qaqh_ringing_v3::ProviderPresetView {
                    id,
                    name,
                    base_url,
                    wire,
                    credential,
                });
            }
            Ok(ConfigResponse::ProviderPresets(views))
        }
        ConfigRequest::UpsertProvider {
            provider,
            expected_revision,
        } => {
            let provider: Value = parse_json(&provider, "ProviderUpsertDto")?;
            let service = backend.service.clone();
            let provider = RealBackend::blocking(move || {
                let id = provider
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "ProviderUpsertDto requires id".to_string())?;
                let dto = serde_json::from_value(provider.clone())
                    .map_err(|error| format!("invalid ProviderUpsertDto: {error}"))?;
                service.provider_upsert(id, &dto, Some(expected_revision))
            })
            .await?;
            let _ = provider;
            Ok(ConfigResponse::Stored)
        }
        ConfigRequest::DeleteProvider {
            provider_id,
            expected_revision,
        } => {
            let service = backend.service.clone();
            RealBackend::blocking(move || {
                service.provider_delete(&provider_id, Some(expected_revision))
            })
            .await?;
            Ok(ConfigResponse::Stored)
        }
        ConfigRequest::UpsertProviderModel {
            provider_id,
            model,
            expected_revision,
        } => {
            let model: Value = parse_json(&model, "ModelDef")?;
            let model_id = model
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| bad_request("ModelDef requires id"))?
                .to_string();
            let command: qaqh_ringing::RingingCommand = serde_json::from_value(json!({
                "type": "provider.model.upsert",
                "provider_id": provider_id,
                "model_id": model_id,
                "model": model
            }))
            .map_err(|error| bad_request(format!("invalid provider model: {error}")))?;
            let qaqh_ringing::RingingCommand::ProviderModelUpsert { model, .. } = command else {
                unreachable!()
            };
            let service = backend.service.clone();
            RealBackend::blocking(move || {
                service.provider_model_upsert(
                    &provider_id,
                    &model_id,
                    Some(expected_revision),
                    model,
                )
            })
            .await?;
            Ok(ConfigResponse::Stored)
        }
        ConfigRequest::DeleteProviderModel {
            provider_id,
            model_id,
            expected_revision,
        } => {
            let service = backend.service.clone();
            RealBackend::blocking(move || {
                service.provider_model_delete(&provider_id, &model_id, Some(expected_revision))
            })
            .await?;
            Ok(ConfigResponse::Stored)
        }
        ConfigRequest::SetDefaultModel {
            selection,
            expected_revision,
        } => {
            let selection = super::session::model_selection_stored(selection);
            let service = backend.service.clone();
            RealBackend::blocking(move || {
                service.set_default_model_selection(Some(expected_revision), selection)
            })
            .await?;
            Ok(ConfigResponse::Stored)
        }
        ConfigRequest::ReloadAgents => {
            for session in backend.state.sessions() {
                let service = backend.service.clone();
                let session_id = session.id.as_str().to_string();
                RealBackend::blocking(move || {
                    let envelope = qaqh_ringing::RingingWorkerCommandEnvelope::new(
                        session_id.clone(),
                        qaqh_ringing_contract::ulid::generate_ulid(),
                        qaqh_ringing::RingingCommand::AgentReloadConfig,
                    );
                    service.send_ringing_command(&session_id, &envelope)
                })
                .await?;
            }
            Ok(ConfigResponse::Reloaded)
        }
    }
}

async fn provider_catalog(backend: &RealBackend) -> Result<Value, RpcError> {
    let service = backend.service.clone();
    RealBackend::blocking(move || service.provider_catalog()).await
}

async fn config_section(backend: &RealBackend, section: ConfigSection) -> Result<Value, RpcError> {
    if section == ConfigSection::Providers {
        return provider_catalog(backend).await;
    }
    let full = backend.json_call("config.load", json!({})).await?;
    match section {
        ConfigSection::Providers => unreachable!(),
        ConfigSection::Tools => Ok(json!({
            "toolAllowlistEnabled": &full["toolAllowlistEnabled"],
            "toolAllowlist": &full["toolAllowlist"]
        })),
        ConfigSection::Sandbox => Ok(full["sandbox"].clone()),
        ConfigSection::Ui => Ok(json!({
            "lang": &full["lang"], "fontFamily": &full["fontFamily"],
            "theme": &full["theme"], "notificationsEnabled": &full["notificationsEnabled"]
        })),
        ConfigSection::Hooks => {
            RealBackend::blocking(move || {
                let config = qaqh_config::Config::load().map_err(|error| error.to_string())?;
                serde_json::to_value(config.hooks).map_err(|error| error.to_string())
            })
            .await
        }
    }
}

async fn set_config_section(
    backend: &RealBackend,
    section: ConfigSection,
    value: &JsonText,
) -> Result<(), RpcError> {
    let value: Value = parse_json(value, "config section")?;
    if !value.is_object() {
        return Err(bad_request("config section must be a JSON object"));
    }
    match section {
        ConfigSection::Providers => Err(bad_request(
            "whole-catalog Config::Set is unsupported; use UpsertProvider/DeleteProvider",
        )),
        ConfigSection::Tools | ConfigSection::Sandbox => {
            save_patch(backend, value).await.map(|_| ())
        }
        ConfigSection::Ui | ConfigSection::Hooks => {
            RealBackend::blocking(move || {
                qaqh_config::Config::update(|config| {
                    if section == ConfigSection::Ui {
                        apply_ui(config, &value)?;
                    } else {
                        config.hooks = serde_json::from_value(value)
                            .map_err(|error| format!("invalid hooks: {error}"))?;
                    }
                    Ok(())
                })
                .map(|_| ())
                .map_err(|error| error.to_string())
            })
            .await
        }
    }
}

fn apply_ui(config: &mut qaqh_config::Config, value: &Value) -> Result<(), String> {
    let allowed = ["lang", "fontFamily", "theme", "notificationsEnabled"];
    if value
        .as_object()
        .expect("validated object")
        .keys()
        .any(|key| !allowed.contains(&key.as_str()))
    {
        return Err(format!("unknown UI config field; allowed: {allowed:?}"));
    }
    if let Some(value) = value.get("lang") {
        config.lang = Some(required_string(value)?.to_string());
    }
    if let Some(value) = value.get("fontFamily") {
        config.font_family = required_string(value)?.to_string();
    }
    if let Some(value) = value.get("theme") {
        config.theme = Some(required_string(value)?.to_string());
    }
    if let Some(value) = value.get("notificationsEnabled") {
        config.notifications_enabled = Some(
            value
                .as_bool()
                .ok_or("notificationsEnabled must be boolean")?,
        );
    }
    Ok(())
}

async fn save_patch(backend: &RealBackend, patch: Value) -> Result<Value, RpcError> {
    let service = backend.service.clone();
    RealBackend::blocking(move || {
        service.execute_ringing_command(
            &qaqh_ringing::CommandTarget::Global,
            &qaqh_ringing::RingingCommand::ConfigSave { patch },
        )
    })
    .await
}

#[rustfmt::skip]
pub(crate) async fn handle_account(
    backend: &RealBackend,
    context: &CallContext,
    request: AccountRequest,
) -> Result<AccountResponse, RpcError> {
    context.identity.require_owner("Account")?;
    match request {
        AccountRequest::List | AccountRequest::Current | AccountRequest::Select { .. } | AccountRequest::Logout { .. } => {
            Err(rpc(ErrorCode::Unavailable, "generic account manager does not exist; use provider-specific SIWC/DeepSeek account operations"))
        }
        AccountRequest::SiwcStatus => Ok(account_map::siwc_status(account_decode(backend, "siwc.status", json!({})).await?)),
        AccountRequest::SiwcProfiles => Ok(account_map::siwc_profiles(account_decode(backend, "siwc.profiles", json!({})).await?)),
        AccountRequest::SiwcModels => Ok(account_map::siwc_models(account_decode(backend, "siwc.models", json!({})).await?)),
        AccountRequest::SiwcSignInStart => Ok(account_map::siwc_sign_in_start(account_decode(backend, "siwc.sign_in.start", json!({})).await?)),
        AccountRequest::SiwcSignInPoll { flow_id } => {
            Ok(account_map::siwc_sign_in_poll(account_decode(backend, "siwc.sign_in.status", json!({"flow_id": flow_id})).await?))
        }
        AccountRequest::SiwcSignInCancel { flow_id } => {
            account_call(backend, "siwc.sign_in.cancel", json!({"flow_id": flow_id})).await?;
            Ok(AccountResponse::Ack)
        }
        AccountRequest::SiwcSignOut { profile_id } => {
            Ok(account_map::siwc_sign_out(account_decode(backend, "siwc.sign_out", json!({"profile_id": profile_id})).await?))
        }
        AccountRequest::DsStatus => Ok(account_map::ds_status(account_decode(backend, "dsaccount.status", json!({})).await?)),
        AccountRequest::DsSignInStart { locale } => {
            Ok(account_map::ds_sign_in_start(account_decode(backend, "dsaccount.sign_in.start", json!({"locale": locale})).await?))
        }
        AccountRequest::DsSignInPoll { attempt_id } => {
            let status = account_decode(backend, "dsaccount.sign_in.status", json!({})).await?;
            account_map::ds_sign_in_poll(status, &attempt_id)
        }
        AccountRequest::DsSignInCancel { attempt_id } => {
            account_call(backend, "dsaccount.sign_in.cancel", json!({"attempt_id": attempt_id})).await?;
            Ok(AccountResponse::Ack)
        }
        AccountRequest::DsSignOut => {
            account_call(backend, "dsaccount.sign_out", json!({})).await?;
            Ok(AccountResponse::Ack)
        }
        AccountRequest::DsVerify => Ok(account_map::ds_verify(account_decode(backend, "dsaccount.verify", json!({})).await?)),
        AccountRequest::DsBalance => Ok(account_map::ds_balance(account_decode(backend, "dsaccount.balance", json!({})).await?)),
        AccountRequest::DsBonuses => Ok(account_map::ds_bonuses(account_decode(backend, "dsaccount.bonuses", json!({})).await?)),
        AccountRequest::DsAckBonus { order_id } => {
            account_call(backend, "dsaccount.bonus.ack", json!({"order_id": order_id})).await?;
            Ok(AccountResponse::Ack)
        }
        AccountRequest::DsModels => account_map::ds_models(account_decode(backend, "dsaccount.models", json!({})).await?),
    }
}

async fn account_call(
    backend: &RealBackend,
    method: &str,
    params: Value,
) -> Result<Value, RpcError> {
    backend
        .json_call(method, params)
        .await
        .map_err(|error| internal(format!("account operation {method}: {}", error.message)))
}

async fn account_decode<T: DeserializeOwned>(
    backend: &RealBackend,
    method: &str,
    params: Value,
) -> Result<T, RpcError> {
    decode(account_call(backend, method, params).await?, method)
}

#[rustfmt::skip]
pub(crate) async fn handle_pairing(
    backend: &RealBackend,
    context: &CallContext,
    request: PairingRequest,
) -> Result<PairingResponse, RpcError> {
    context.identity.require_owner("Pairing")?;
    match request {
        PairingRequest::CreateInvite { ttl_ms } => {
            let (code, actual_ttl) = {
                let mut pairings = backend
                    .pairings
                    .lock()
                    .map_err(|_| internal("pairing lock poisoned"))?;
                if ttl_ms != pairings.ttl_ms() {
                    return Err(bad_request(format!(
                        "pairing TTL must be {} ms",
                        pairings.ttl_ms()
                    )));
                }
                (
                    pairings.issue(
                        qaqh_ringing_runtime::Scope::Interact,
                        "paired device".into(),
                        String::new(),
                    ),
                    ttl_ms,
                )
            };
            let expires_at_ms = now_ms().saturating_add(actual_ttl);
            Ok(PairingResponse::Invite {
                code,
                expires_at_ms,
            })
        }
        PairingRequest::Redeem { code, client_kind } => {
            let grant = backend
                .pairings
                .lock()
                .map_err(|_| internal("pairing lock poisoned"))?
                .consume(&code)
                .map_err(|error| denied(format!("pairing token rejected: {error}")))?;
            let platform = match client_kind {
                ClientKind::Desktop => "desktop",
                ClientKind::Mobile => "mobile",
                ClientKind::Headless => "headless",
            };
            let name = if grant.device_name.trim().is_empty() {
                platform.to_string()
            } else {
                grant.device_name
            };
            let (device_id, token) = backend
                .devices
                .issue(&name, platform, grant.scope)
                .await
                .map_err(|error| internal(format!("register paired device: {error}")))?;
            let record = backend
                .devices
                .get(&device_id)
                .await
                .map_err(|error| internal(format!("read paired device: {error}")))?
                .ok_or_else(|| internal("issued device missing from persistent registry"))?;
            backend.state.identity.register_device_digest(
                &record.token_digest,
                &record.device_id,
                &record.name,
                identity_scope(record.scope),
                record.created_at_ms,
            );
            Ok(PairingResponse::Redeemed { device_id, token })
        }
        PairingRequest::ListDevices => {
            let records = backend
                .devices
                .list()
                .await
                .map_err(|error| internal(format!("list devices: {error}")))?;
            let online: BTreeSet<String> = backend
                .state
                .sessions()
                .into_iter()
                .flat_map(|session| session.attached().into_keys())
                .collect();
            let devices = records
                .into_iter()
                .map(|record| DeviceView {
                    online: online.contains(&format!("device-digest:{}", record.token_digest)),
                    device_id: record.device_id,
                    label: record.name,
                    paired_at_ms: record.created_at_ms,
                })
                .collect();
            Ok(PairingResponse::Devices(devices))
        }
        PairingRequest::RevokeDevice { device_id } => {
            if backend
                .devices
                .get(&device_id)
                .await
                .map_err(|error| internal(format!("get device: {error}")))?
                .is_none()
            {
                return Err(not_found(format!("unknown device `{device_id}`")));
            }
            backend
                .devices
                .revoke(&device_id)
                .await
                .map_err(|error| internal(format!("revoke device: {error}")))?;
            let key = backend
                .state
                .identity
                .revoke_device_with_key(&device_id)
                .ok_or_else(|| {
                    internal(format!("device `{device_id}` missing from identity table"))
                })?;
            for session in backend.state.sessions() {
                session.evict_device(&key);
            }
            backend.service.shutdown_shells_of(&device_id);
            Ok(PairingResponse::Revoked)
        }
    }
}

fn parse_json<T: DeserializeOwned>(text: &JsonText, what: &str) -> Result<T, RpcError> {
    serde_json::from_str(text.as_str())
        .map_err(|error| bad_request(format!("invalid {what}: {error}")))
}

fn decode<T: DeserializeOwned>(value: Value, what: &str) -> Result<T, RpcError> {
    serde_json::from_value(value).map_err(|error| internal(format!("decode {what}: {error}")))
}

fn revision(value: &Value) -> Result<u64, RpcError> {
    value["revision"]
        .as_u64()
        .ok_or_else(|| internal("provider catalog missing revision"))
}

fn required_string(value: &Value) -> Result<&str, &'static str> {
    value.as_str().ok_or("UI config values must be strings")
}
