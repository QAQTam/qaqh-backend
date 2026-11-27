//! 新 BYOK 目录体系（[`qaqh_profile::ProfilesDocument`]）的类型化业务适配。
//!
//! 统一切换设计 §8.7：这些函数是「typed application handlers」——只被统一
//! 命令 dispatch 直接调用，**不进** `handle(method, Value)` 字符串 decoder；
//! 旧 service 方法名不在这里登记，也不为旧面开任何别名。
//!
//! 凭据纪律：API key 明文只进 `secrets.toml` 加密槽（`providers.<id>`），
//! `providers.toml` 只落 `"set"` 标记；任何路径不得把明文写进目录、日志或回执。

use qaqh_dsaccount::service::DsAccountModel;
use qaqh_profile::{ModelDef, ModelDefaults, ModelSelection, ProfilesDocument, ProfilesStore, ProviderDef};
use qaqh_ringing::{
    ProviderUpsertDto, RingingCommand, SecretWrite, SessionSettingsPatch, SettingValue,
    ToolsSettingPatch,
};
use qaqh_types::{CredentialSource, ModelCapabilities, Wire};
use serde_json::{Value, json};
use std::collections::BTreeMap;

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

/// base_url 必须是 http(s) 绝对 URL 且带主机名。只查形状不做连通性探测：
/// 端点族纪律是提交侧 fail-fast，坏 URL 不留到第一回合才炸。
fn validate_base_url(raw: &str) -> QaqhResult<()> {
    let ok = raw.trim().split_once("://").is_some_and(|(scheme, rest)| {
        (scheme.eq_ignore_ascii_case("https") || scheme.eq_ignore_ascii_case("http"))
            && rest
                .split(['/', '?', '#'])
                .next()
                .is_some_and(|host| !host.is_empty())
            && !rest.contains(' ')
    });
    if ok {
        Ok(())
    } else {
        Err(QaqhError::invalid(format!(
            "provider base_url {raw:?} is invalid: must be http(s)://<host>"
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

    /// 内置预设静态表（Spec §3.2）：只读快照，预设 id → 默认 endpoint/协议/
    /// 凭据类型。选择预设 = 前端把默认值复制进 provider.upsert 落自己的目录。
    pub fn provider_presets(&self) -> QaqhResult<Value> {
        serde_json::to_value(qaqh_profile::PROVIDER_PRESETS)
            .map_err(|error| QaqhError::invalid(format!("serialize presets: {error}")))
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
        validate_base_url(&dto.base_url)?;
        if dto.credential == qaqh_types::CredentialSource::DeepseekAccount {
            if dto.wire != qaqh_types::Wire::Responses {
                return Err(QaqhError::invalid(
                    "DeepSeek account credential requires wire = responses",
                ));
            }
            if !qaqh_dsaccount::is_known_inference_origin(dto.base_url.trim()) {
                return Err(QaqhError::invalid(format!(
                    "DeepSeek account credential requires base_url = {}",
                    qaqh_dsaccount::protocol::INFERENCE_ORIGIN
                )));
            }
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
        if model.max_output_tokens == 0 {
            return Err(QaqhError::invalid(format!(
                "model {provider_id}/{model_id}: max_tokens must be greater than 0"
            )));
        }
        if model.context_window == 0 {
            return Err(QaqhError::invalid(format!(
                "model {provider_id}/{model_id}: context_window must be greater than 0"
            )));
        }
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
            tools,
            team_mode,
        } = &patch;
        if model_selection.is_none() && tools.is_none() && team_mode.is_none() {
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
        if let Some(tools) = tools {
            apply_tools_patch(&mut meta, tools)?;
        }

        // ADR-0033:team_mode 仅根可改(后代继承存活根);独立写路径,
        // 与 settings revision 解耦(切换不 bump settings revision)。
        if let Some(flag) = team_mode {
            if self.owners.session_parent(session_id).map_err(QaqhError::message)?.is_some() {
                return Err(QaqhError::invalid(
                    "session.settings.patch: team_mode is root-only; descendants inherit the root",
                ));
            }
            let root = session_id.to_string();
            let flag = *flag;
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_millis() as i64)
                .unwrap_or_default();
            self.owners
                .db()
                .write_blocking(move |conn| {
                    qaqh_session::db::store::set_session_team_mode(&conn, &root, flag, now_ms)
                })
                .map_err(|error| QaqhError::context("team_mode write failed", error.to_string()))?;
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

    /// 把 DeepSeek 账号模型目录原子收敛进 `providers.toml`。
    /// 失败只记日志，不阻断 `dsaccount.models` / `dsaccount.sign_in.status` 正常返回。
    pub(crate) fn reconcile_dsaccount_catalog(
        &self,
        inference_origin: &str,
        models: &[DsAccountModel],
    ) {
        let mut store = match load_catalog() {
            Ok(store) => store,
            Err(error) => {
                log::warn!("[providers] load catalog for dsaccount reconcile failed: {error:?}");
                return;
            }
        };
        match store.mutate_if_changed(|doc| reconcile_dsaccount_document(doc, inference_origin, models))
        {
            Ok(true) => {
                let revision = store.document().revision;
                if let Err(error) = refresh_config(self, revision) {
                    log::warn!("[providers] refresh config after dsaccount reconcile failed: {error:?}");
                }
            }
            Ok(false) => {}
            Err(error) => {
                log::warn!("[providers] save catalog after dsaccount reconcile failed: {error}");
            }
        }
    }

    /// best-effort 拉取 DeepSeek 账号远端 `/models` 并原子收敛目录；
    /// 即使远端拉取失败，仍保证 `DeepseekAccount` Provider 骨架先落盘。
    pub(crate) fn sync_dsaccount_catalog_best_effort(&self) {
        let Ok(manager) = self.dsaccount() else {
            return;
        };
        let origin = manager.config().inference_origin.clone();
        let models: Vec<DsAccountModel> = match manager.models() {
            Ok(catalog) => {
                let data = catalog
                    .get("data")
                    .cloned()
                    .unwrap_or_else(|| Value::Array(Vec::new()));
                match serde_json::from_value::<Vec<DsAccountModel>>(data) {
                    Ok(models) => models,
                    Err(error) => {
                        log::warn!("[providers] parse dsaccount models during auto-sync failed: {error}");
                        Vec::new()
                    }
                }
            }
            Err(error) => {
                log::warn!("[providers] fetch dsaccount models during auto-sync failed: {error}");
                Vec::new()
            }
        };
        self.reconcile_dsaccount_catalog(&origin, &models);
    }
}

fn positive_u32(value: &Value) -> Option<u32> {
    value
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .filter(|&n| n > 0)
}

fn strict_string_array(value: &Value) -> Option<Vec<String>> {
    value
        .as_array()?
        .iter()
        .map(|item| item.as_str().map(ToOwned::to_owned))
        .collect()
}

/// 将 DeepSeek 账号 `/models` 条目与现有目录 [`ModelDef`] 合并；
/// 缺关键上下文/输出上限时返回 `None`，绝不猜测或虚构上限。
pub(crate) fn merge_dsaccount_model(
    raw: &DsAccountModel,
    existing: Option<&ModelDef>,
) -> Option<ModelDef> {
    let model_id = raw.id.trim();
    if model_id.is_empty() {
        return None;
    }
    let context_window = raw
        .metadata
        .get("context_window")
        .or_else(|| raw.metadata.get("context_length"))
        .and_then(positive_u32)
        .or_else(|| existing.map(|m| m.context_window).filter(|&n| n > 0))?;
    let max_output_tokens = raw
        .metadata
        .get("max_output_tokens")
        .or_else(|| raw.metadata.get("max_tokens"))
        .and_then(positive_u32)
        .or_else(|| existing.map(|m| m.max_output_tokens).filter(|&n| n > 0))?;

    let display_name = raw
        .metadata
        .get("display_name")
        .or_else(|| raw.metadata.get("name"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != model_id)
        .map(ToOwned::to_owned)
        .or_else(|| existing.and_then(|m| m.display_name.clone()));

    let effort_obj = raw.metadata.get("effort").and_then(|v| v.as_object());
    let declared_levels = effort_obj
        .and_then(|o| o.get("supported_levels"))
        .or_else(|| raw.metadata.get("reasoning_levels"))
        .and_then(strict_string_array);
    let reasoning_levels = declared_levels
        .clone()
        .unwrap_or_else(|| existing.map(|m| m.reasoning_levels.clone()).unwrap_or_default());

    let default_level = effort_obj
        .and_then(|o| o.get("default_level"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let existing_effort = existing
        .and_then(|m| m.defaults.reasoning_effort.as_deref())
        .filter(|level| reasoning_levels.iter().any(|item| item == level))
        .map(ToOwned::to_owned);
    let reasoning_effort = existing_effort.or_else(|| {
        default_level
            .filter(|level| reasoning_levels.iter().any(|item| item == level))
            .map(ToOwned::to_owned)
    });

    let caps_obj = raw.metadata.get("capabilities").and_then(|v| v.as_object());
    let cap_bool = |key: &str| caps_obj.and_then(|o| o.get(key)).and_then(|v| v.as_bool());
    let modalities = raw
        .metadata
        .get("input_modalities")
        .and_then(strict_string_array);

    let image_input = modalities
        .map(|items| items.iter().any(|item| item == "image"))
        .or_else(|| cap_bool("image_input"))
        .unwrap_or_else(|| existing.is_some_and(|m| m.capabilities.image_input));
    let tool_calls = cap_bool("tool_calls")
        .unwrap_or_else(|| existing.map(|m| m.capabilities.tool_calls).unwrap_or(true));
    let native_web_search = cap_bool("native_web_search").unwrap_or_else(|| {
        existing
            .map(|m| m.capabilities.native_web_search)
            .unwrap_or_else(|| model_id.starts_with("deepseek-v4-pro"))
    });
    let reasoning = if declared_levels.is_some() {
        !reasoning_levels.is_empty()
    } else {
        cap_bool("reasoning").unwrap_or_else(|| {
            existing
                .map(|m| m.capabilities.reasoning)
                .unwrap_or_else(|| model_id.contains("reasoner"))
        })
    };

    Some(ModelDef {
        enabled: existing.map(|m| m.enabled).unwrap_or(true),
        display_name,
        context_window,
        max_output_tokens,
        reasoning_levels,
        capabilities: ModelCapabilities {
            tool_calls,
            image_input,
            native_web_search,
            reasoning,
        },
        defaults: ModelDefaults {
            max_output_tokens: existing
                .and_then(|m| m.defaults.max_output_tokens)
                .map(|v| v.min(max_output_tokens)),
            reasoning_effort,
            native_web_search: existing
                .is_some_and(|m| m.defaults.native_web_search && native_web_search),
        },
    })
}

fn allocate_dsaccount_provider_id(providers: &BTreeMap<String, ProviderDef>) -> String {
    for candidate in ["deepseek-oauth", "deepseek-account"] {
        if !providers.contains_key(candidate) {
            return candidate.to_string();
        }
    }
    for index in 2.. {
        let candidate = format!("deepseek-oauth-{index}");
        if !providers.contains_key(&candidate) {
            return candidate;
        }
    }
    unreachable!()
}

/// 幂等把 DeepSeek OAuth 服务商与可用模型列表写入 [`ProfilesDocument`]。
/// 返回 `true` 表示文档发生实质变更，需要递增 `revision` 并落盘。
pub(crate) fn reconcile_dsaccount_document(
    doc: &mut ProfilesDocument,
    inference_origin: &str,
    models: &[DsAccountModel],
) -> bool {
    let origin = if qaqh_dsaccount::is_known_inference_origin(inference_origin) {
        inference_origin.trim().trim_end_matches('/').to_string()
    } else {
        qaqh_dsaccount::protocol::INFERENCE_ORIGIN.to_string()
    };

    let mut changed = false;
    let existing_id = doc
        .providers
        .iter()
        .find(|(_, provider)| provider.credential == CredentialSource::DeepseekAccount)
        .map(|(id, _)| id.clone());

    let provider_id = if let Some(id) = existing_id {
        id
    } else {
        let id = allocate_dsaccount_provider_id(&doc.providers);
        let name = qaqh_profile::provider_preset("deepseek-oauth")
            .map(|preset| preset.name)
            .unwrap_or("DeepSeek 官方（OAuth 套餐）")
            .to_string();
        doc.providers.insert(
            id.clone(),
            ProviderDef {
                name,
                base_url: origin.clone(),
                wire: Wire::Responses,
                credential: CredentialSource::DeepseekAccount,
                api_key: None,
                headers: BTreeMap::new(),
                compat: None,
                models: BTreeMap::new(),
            },
        );
        changed = true;
        id
    };

    if let Some(provider) = doc.providers.get_mut(&provider_id) {
        if provider.base_url != origin {
            provider.base_url = origin;
            changed = true;
        }
        if provider.wire != Wire::Responses {
            provider.wire = Wire::Responses;
            changed = true;
        }
        for raw in models {
            let model_id = raw.id.trim();
            if model_id.is_empty() {
                continue;
            }
            let existing_model = provider.models.get(model_id);
            if let Some(merged) = merge_dsaccount_model(raw, existing_model)
                && existing_model != Some(&merged)
            {
                provider.models.insert(model_id.to_string(), merged);
                changed = true;
            }
        }
    }

    if doc.default_model_selection.is_none() {
        let first_valid = models.iter().find_map(|raw| {
            let model_id = raw.id.trim();
            if model_id.is_empty() {
                return None;
            }
            let sel = ModelSelection {
                provider_id: provider_id.clone(),
                model_id: model_id.to_string(),
                options: Default::default(),
            };
            qaqh_profile::resolve_model_selection(doc, &sel)
                .is_ok()
                .then_some(sel)
        });
        if let Some(selection) = first_valid {
            doc.default_model_selection = Some(selection);
            changed = true;
        }
    }

    changed
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

#[cfg(test)]
mod tests {
    use super::*;

    fn ds_model(id: &str, name: &str, ctx: u64, out: u64, levels: &[&str], default: &str) -> DsAccountModel {
        let mut metadata = serde_json::Map::new();
        metadata.insert("name".into(), json!(name));
        metadata.insert("context_window".into(), json!(ctx));
        metadata.insert("max_output_tokens".into(), json!(out));
        metadata.insert("input_modalities".into(), json!(["text", "image"]));
        metadata.insert(
            "effort".into(),
            json!({ "supported_levels": levels, "default_level": default }),
        );
        DsAccountModel {
            id: id.into(),
            metadata: metadata.into_iter().collect(),
        }
    }

    #[test]
    fn reconcile_dsaccount_creates_provider_and_is_idempotent() {
        let mut doc = ProfilesDocument::default();
        let models = vec![
            ds_model("deepseek-flash", "DeepSeek Flash", 1_048_576, 393_216, &["low", "high", "max"], "high"),
            ds_model("deepseek-v4-pro", "DeepSeek V4 Pro", 1_048_576, 393_216, &["low", "high", "max"], "high"),
            DsAccountModel {
                id: "incomplete-model".into(),
                metadata: BTreeMap::new(),
            },
        ];

        assert!(reconcile_dsaccount_document(&mut doc, "https://api.deepseek.com/", &models));
        let provider = doc.providers.get("deepseek-oauth").expect("provider created");
        assert_eq!(provider.credential, CredentialSource::DeepseekAccount);
        assert_eq!(provider.wire, Wire::Responses);
        assert_eq!(provider.base_url, "https://api.deepseek.com");
        assert!(!provider.models.contains_key("incomplete-model"));

        let flash = &provider.models["deepseek-flash"];
        assert_eq!(flash.display_name.as_deref(), Some("DeepSeek Flash"));
        assert_eq!(flash.context_window, 1_048_576);
        assert_eq!(flash.max_output_tokens, 393_216);
        assert_eq!(flash.defaults.reasoning_effort.as_deref(), Some("high"));
        assert!(flash.capabilities.image_input);
        assert!(flash.capabilities.tool_calls);
        assert!(!flash.capabilities.native_web_search);
        assert!(provider.models["deepseek-v4-pro"].capabilities.native_web_search);

        assert_eq!(
            doc.default_model_selection.as_ref().map(|s| (s.provider_id.as_str(), s.model_id.as_str())),
            Some(("deepseek-oauth", "deepseek-flash"))
        );

        assert!(!reconcile_dsaccount_document(&mut doc, "https://api.deepseek.com", &models));
    }

    #[test]
    fn reconcile_dsaccount_reuses_existing_provider_preserves_preferences_and_clamps_limits() {
        let mut doc = ProfilesDocument::default();
        doc.default_model_selection = Some(ModelSelection {
            provider_id: "byok".into(),
            model_id: "custom".into(),
            options: Default::default(),
        });
        doc.providers.insert(
            "deepseek-account".into(),
            ProviderDef {
                name: "My DS".into(),
                base_url: "https://old.invalid".into(),
                wire: Wire::Chat,
                credential: CredentialSource::DeepseekAccount,
                api_key: None,
                headers: BTreeMap::new(),
                compat: None,
                models: BTreeMap::from([(
                    "deepseek-flash".into(),
                    ModelDef {
                        enabled: false,
                        display_name: Some("Old".into()),
                        context_window: 128_000,
                        max_output_tokens: 64_000,
                        reasoning_levels: vec!["low".into(), "high".into()],
                        capabilities: ModelCapabilities {
                            tool_calls: true,
                            image_input: false,
                            native_web_search: false,
                            reasoning: true,
                        },
                        defaults: ModelDefaults {
                            max_output_tokens: Some(32_000),
                            reasoning_effort: Some("low".into()),
                            native_web_search: false,
                        },
                    },
                )]),
            },
        );

        let models = vec![ds_model(
            "deepseek-flash",
            "DeepSeek Flash",
            1_048_576,
            16_384,
            &["low", "high", "max"],
            "high",
        )];
        assert!(reconcile_dsaccount_document(&mut doc, "https://api.deepseek.com", &models));
        assert!(!doc.providers.contains_key("deepseek-oauth"));
        let provider = &doc.providers["deepseek-account"];
        assert_eq!(provider.base_url, "https://api.deepseek.com");
        assert_eq!(provider.wire, Wire::Responses);
        let flash = &provider.models["deepseek-flash"];
        assert!(!flash.enabled);
        assert_eq!(flash.defaults.max_output_tokens, Some(16_384));
        assert_eq!(flash.defaults.reasoning_effort.as_deref(), Some("low"));
        assert_eq!(
            doc.default_model_selection.as_ref().map(|s| s.provider_id.as_str()),
            Some("byok")
        );
    }
}
