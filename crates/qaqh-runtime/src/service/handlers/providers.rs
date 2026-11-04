//! 新 BYOK 目录体系（[`qaqh_profile::ProfilesDocument`]）的类型化业务适配。
//!
//! 统一切换设计 §8.7：这些函数是「typed application handlers」——只被统一
//! 命令 dispatch 直接调用，**不进** `handle(method, Value)` 字符串 decoder；
//! 旧 service 方法名不在这里登记，也不为旧面开任何别名。
//!
//! 凭据纪律：API key 明文只进 `secrets.toml` 加密槽（`providers.<id>`），
//! `providers.toml` 只落 `"set"` 标记；任何路径不得把明文写进目录、日志或回执。

use qaqh_profile::{ModelDef, ModelSelection, ProfilesStore, ProviderDef};
use qaqh_ringing::{
    ProviderUpsertDto, RingingCommand, SecretWrite, SessionSettingsPatch, SettingValue,
    ToolsSettingPatch,
};
use serde_json::{Value, json};

use crate::error::{QaqhError, QaqhResult};
use crate::service::QaqhService;
use qaqh_ringing_contract::fact::SessionMetadataPatch;

fn load_catalog() -> QaqhResult<ProfilesStore> {
    ProfilesStore::load_or_create(&qaqh_profile::default_providers_path())
        .map_err(|error| QaqhError::context("load providers catalog failed", error))
}

/// provider id 兼作 secrets 槽名：与 profile 名同一字符集（小写字母/数字/
/// `_`/`-`，≤64）。进不了槽位的 id 无法自带密钥，直接拒绝。
fn validate_provider_id(id: &str) -> QaqhResult<()> {
    let ok = !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    if ok {
        Ok(())
    } else {
        Err(QaqhError::invalid(format!(
            "provider id {id:?} is invalid: must be 1..=64 chars of [a-z0-9_-]"
        )))
    }
}

fn check_revision(store: &ProfilesStore, expected: Option<u64>) -> QaqhResult<()> {
    if let Some(expected) = expected {
        store
            .check_revision(expected)
            .map_err(|error| match error {
                // CAS 冲突必须以 Conflict 语义上抛(409),与 settings_revision
                // 同档——目录漂移是并发修订事实,不是"参数非法"。
                qaqh_profile::ProfileStoreError::RevisionConflict { expected, actual } => {
                    QaqhError::Conflict {
                        message: format!(
                            "config revision conflict: expected {expected}, current {actual}"
                        ),
                    }
                }
                other => QaqhError::message(other.to_string()),
            })?;
    }
    Ok(())
}

/// 目录变更后的统一收口：Config 单写口重载（内部重新装载 providers.toml →
/// `profiles_catalog`/`default_model_selection`），并广播所有 worker 重读。
fn refresh_config(service: &QaqhService, revision: u64) -> QaqhResult<()> {
    service
        .update_config_and_reload(|_cfg| Ok(()))
        .map_err(QaqhError::message)?;
    log::info!("[providers] catalog updated, revision={revision}");
    Ok(())
}

impl QaqhService {
    /// 目录全量读（脱敏：api_key 只含 "set" 标记，无明文）。
    pub fn provider_catalog(&self) -> QaqhResult<Value> {
        let store = load_catalog()?;
        serde_json::to_value(store.document())
            .map_err(|error| QaqhError::invalid(format!("serialize catalog: {error}")))
    }

    /// 新增或整体替换一个 Provider，目录 revision 由公共命令信封携带。
    pub fn provider_upsert(
        &self,
        id: &str,
        dto: &ProviderUpsertDto,
        expected_revision: Option<u64>,
    ) -> QaqhResult<Value> {
        validate_provider_id(id)?;
        if dto.name.trim().is_empty() {
            return Err(QaqhError::invalid("provider name must be non-empty"));
        }
        if dto.base_url.trim().is_empty() {
            return Err(QaqhError::invalid("provider base_url must be non-empty"));
        }
        let mut store = load_catalog()?;
        check_revision(&store, expected_revision)?;

        let existing = store.document().providers.get(id).cloned();
        // api_key 三态：Keep 保留现有标记；Set 写加密槽 + "set"；Clear 删槽 + 无标记。
        let api_key = match &dto.api_key {
            SecretWrite::Keep => existing.as_ref().and_then(|p| p.api_key.clone()),
            SecretWrite::Set(key) => {
                if key.is_empty() {
                    return Err(QaqhError::invalid("provider api_key must be non-empty"));
                }
                qaqh_config::secrets::SecretStore::default_location()
                    .set_provider_key(id, key)
                    .map_err(QaqhError::message)?;
                Some("set".to_string())
            }
            SecretWrite::Clear => {
                qaqh_config::secrets::SecretStore::default_location()
                    .delete_provider_key(id)
                    .map_err(QaqhError::message)?;
                None
            }
        };

        let models = existing.map(|p| p.models).unwrap_or_default();
        let def = ProviderDef {
            name: dto.name.trim().to_string(),
            base_url: dto.base_url.trim().to_string(),
            wire: dto.wire,
            credential: dto.credential,
            api_key,
            headers: dto.headers.clone(),
            compat: dto.compat.clone(),
            models,
        };
        store
            .upsert_provider(id.to_string(), def)
            .map_err(QaqhError::message)?;
        let revision = store.document().revision;
        refresh_config(self, revision)?;
        Ok(json!({ "revision": revision }))
    }

    /// 删除 Provider（目录守卫：被 profile 或 default_model_selection 引用时拒绝）。
    pub fn provider_delete(&self, id: &str, expected_revision: Option<u64>) -> QaqhResult<Value> {
        validate_provider_id(id)?;
        let mut store = load_catalog()?;
        check_revision(&store, expected_revision)?;
        store.delete_provider(id).map_err(QaqhError::message)?;
        // 目录已不再引用该 id；密钥槽随删。失败只告警——目录里已无引用条目。
        if let Err(error) =
            qaqh_config::secrets::SecretStore::default_location().delete_provider_key(id)
        {
            log::warn!("[providers] secret cleanup for provider {id:?} skipped: {error}");
        }
        let revision = store.document().revision;
        refresh_config(self, revision)?;
        Ok(json!({ "revision": revision }))
    }

    /// 新增/替换一个模型定义（挂在指定 Provider 下）。
    pub fn provider_model_upsert(
        &self,
        provider_id: &str,
        model_id: &str,
        expected_revision: Option<u64>,
        model: ModelDef,
    ) -> QaqhResult<Value> {
        let mut store = load_catalog()?;
        check_revision(&store, expected_revision)?;
        store
            .upsert_model(provider_id, model_id, model)
            .map_err(QaqhError::message)?;
        let revision = store.document().revision;
        refresh_config(self, revision)?;
        Ok(json!({ "revision": revision }))
    }

    /// 删除模型定义（被 profile/default_model_selection 引用时目录守卫拒绝）。
    pub fn provider_model_delete(
        &self,
        provider_id: &str,
        model_id: &str,
        expected_revision: Option<u64>,
    ) -> QaqhResult<Value> {
        let mut store = load_catalog()?;
        check_revision(&store, expected_revision)?;
        store
            .delete_model(provider_id, model_id)
            .map_err(QaqhError::message)?;
        let revision = store.document().revision;
        refresh_config(self, revision)?;
        Ok(json!({ "revision": revision }))
    }

    /// 设置全局默认模型选择。落盘前先做一次权威解析（fail-fast：不存在的
    /// provider/model、禁用模型、非法档位在提交侧即拒绝，不留脏默认值）。
    pub fn set_default_model_selection(
        &self,
        expected_revision: Option<u64>,
        selection: ModelSelection,
    ) -> QaqhResult<Value> {
        let mut store = load_catalog()?;
        check_revision(&store, expected_revision)?;
        qaqh_profile::resolve_model_selection(store.document(), &selection)
            .map_err(QaqhError::message)?;
        store
            .set_default_model_selection(selection)
            .map_err(QaqhError::message)?;
        let revision = store.document().revision;
        refresh_config(self, revision)?;
        Ok(json!({ "revision": revision }))
    }

    /// 会话设置 patch（§8.1 的 `session.settings.patch`）：model_selection /
    /// mode / tools 一次提交；落 meta 后定向 reload 目标会话，worker 下一轮
    /// 应用。空 patch 拒绝。
    pub fn patch_session_settings(
        &self,
        session_id: &str,
        patch: SessionSettingsPatch,
        expected_revision: u64,
    ) -> QaqhResult<u64> {
        let SessionSettingsPatch {
            model_selection,
            profile,
            effort,
            mode,
            tools,
        } = &patch;
        if model_selection.is_none()
            && profile.is_none()
            && effort.is_none()
            && mode.is_none()
            && tools.is_none()
        {
            return Err(QaqhError::invalid(
                "session.settings.patch: at least one setting is required",
            ));
        }

        let mut meta = SessionMetadataPatch::default();
        if let Some(selection) = model_selection {
            // 提交侧 fail-fast：选择必须能在当前目录解析（禁用/缺档位即拒绝）。
            if let SettingValue::Use { value } = selection {
                let catalog = qaqh_config::watch::authoritative()
                    .map(|cfg| cfg.profiles_catalog.clone())
                    .unwrap_or_default();
                qaqh_profile::resolve_model_selection(&catalog, value)
                    .map_err(QaqhError::message)?;
            }
            meta.model_selection = Some(selection.clone());
            meta.requested_model_selection = Some(selection.clone());
        }
        if let Some(profile) = profile {
            if !profile.is_empty()
                && !qaqh_config::watch::authoritative()
                    .is_some_and(|cfg| cfg.profiles.contains_key(profile))
            {
                return Err(QaqhError::not_found(format!(
                    "profile '{profile}' not found"
                )));
            }
            meta.profile = Some(profile.clone());
        }
        if let Some(effort) = effort {
            meta.effort = Some(effort.clone());
        }
        if let Some(mode) = mode {
            meta.mode = Some(match mode.as_str() {
                "code" => 0,
                "plan" => 1,
                other => {
                    return Err(QaqhError::invalid(format!(
                        "session.settings.patch: unknown mode {other:?} (code|plan)"
                    )));
                }
            });
        }
        if let Some(tools) = tools {
            apply_tools_patch(&mut meta, tools)?;
        }

        let revision = crate::session_meta::patch_with_settings_revision(
            &self.owners,
            session_id,
            meta,
            expected_revision,
        )
        .map_err(|error| match error {
            crate::session_meta::MetadataPatchError::NotFound { session_id } => {
                QaqhError::not_found(format!("session {session_id} not found"))
            }
            crate::session_meta::MetadataPatchError::RevisionConflict { expected, actual } => {
                QaqhError::Conflict {
                    message: format!(
                        "settings revision conflict: expected {expected}, current {actual}"
                    ),
                }
            }
            crate::session_meta::MetadataPatchError::Storage(error) => {
                QaqhError::context("session.settings.patch write failed", error)
            }
        })?;
        // 定向重载（只影响目标会话）；worker 下一轮按 meta 的新选择装配请求。
        self.send_ringing_cmd(session_id.to_string(), RingingCommand::AgentReloadConfig)?;
        Ok(revision)
    }
}

/// tools patch → tool_mode/custom_tools 映射。词表真源是
/// `qaqh_types::tool_mode`（standard/minimal|minimal:b|minimal:c/custom）。
fn apply_tools_patch(meta: &mut SessionMetadataPatch, tools: &ToolsSettingPatch) -> QaqhResult<()> {
    match tools {
        ToolsSettingPatch::Default | ToolsSettingPatch::All => {
            meta.tool_mode = Some(qaqh_types::tool_mode::STANDARD.to_string());
            meta.custom_tools = Some(Vec::new());
        }
        ToolsSettingPatch::Custom { names } => {
            if names.is_empty() {
                return Err(QaqhError::invalid(
                    "tools.custom requires a non-empty allowlist",
                ));
            }
            meta.tool_mode = Some(qaqh_types::tool_mode::CUSTOM.to_string());
            meta.custom_tools = Some(names.clone());
        }
        ToolsSettingPatch::Preset { name } => {
            if !qaqh_types::tool_mode::KNOWN_MODES.contains(&name.as_str()) {
                return Err(QaqhError::invalid(format!(
                    "tools.preset {name:?} is not a known tool mode"
                )));
            }
            meta.tool_mode = Some(name.clone());
            meta.custom_tools = Some(Vec::new());
        }
        // 「零工具会话」不是现有词表里的模式（标准面始终保留最小探测能力）；
        // 显式拒绝而不是静默折叠成 standard。
        ToolsSettingPatch::None => {
            return Err(QaqhError::invalid(
                "tools: none is not a supported session tool mode",
            ));
        }
    }
    Ok(())
}
