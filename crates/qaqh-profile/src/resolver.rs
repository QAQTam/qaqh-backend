//! 权威执行配置解析器（效仿 ZCode Selection Resolver 设计）。
//!
//! 负责从 ProfilesDocument 中层叠解析出会话所需的最终执行配置：
//! - 若会话指定了 profile，则以该 profile 为准；
//! - 若会话未指定，则回退到全局 default_profile；
//! - 自动从 Provider 中拉取 BaseURL、Wire、请求头与凭据槽位；
//! - 自动将 Profile 的覆盖参数（context_length、max_tokens、reasoning_effort）与 Model 默认值合并；
//! - 解析过程纯函数式、零全局副作用，不改变任何其他会话的状态。

use std::collections::BTreeMap;
use qaqh_types::{CredentialSource, EndpointCompat, Wire};
use thiserror::Error;
use crate::model::{ModelSelection, ProfilesDocument};

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ProfileResolveError {
    #[error("指定的 Profile「{0}」未在配置中注册")]
    ProfileNotFound(String),
    #[error("Profile「{0}」引用的 Provider「{1}」不存在")]
    ProviderNotFound(String, String),
    #[error("全局默认 Profile「{0}」未在配置中注册")]
    DefaultProfileNotFound(String),
}

/// 最终用于网络请求、网关调度和上下文压缩的权威执行配置
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveExecutionProfile {
    /// 生效的 Profile ID
    pub profile_id: String,
    /// 生效的 Profile 名称
    pub profile_name: String,
    /// 所属 Provider ID
    pub provider_id: String,
    /// 所属 Provider 名称
    pub provider_name: String,
    /// 目标模型名称（例如 "reasoning-model"）
    pub model: String,
    /// 服务端点 Base URL
    pub base_url: String,
    /// 传输协议形态
    pub wire: Wire,
    /// 凭据来源
    pub credential: CredentialSource,
    /// 对应的 secrets.toml 槽位路径（如 "secrets.providers.custom"）
    pub secret_slot: String,
    /// 上下文窗口总大小（作为软压缩阈值与 pre-flight 检查基数）
    pub context_length: u32,
    /// 单回合最大输出 Token 上限
    pub max_tokens: u32,
    /// 思考/推理强度档位（"low" | "medium" | "high" | "max"）
    pub reasoning_effort: Option<String>,
    /// 出站 HTTP 请求头
    pub headers: BTreeMap<String, String>,
    /// 端点协议补丁兼容配置
    pub compat: Option<EndpointCompat>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveModelConfig {
    pub provider_id: String,
    pub model_id: String,
    pub display_name: Option<String>,
    pub context_window: u32,
    pub max_output_tokens: u32,
    pub reasoning_effort: Option<String>,
    pub native_web_search: bool,
    pub capabilities: qaqh_types::ModelCapabilities,
    pub reasoning_levels: Vec<String>,
    pub base_url: String,
    pub wire: Wire,
    pub credential: CredentialSource,
    pub secret_slot: String,
    pub headers: BTreeMap<String, String>,
    pub compat: Option<EndpointCompat>,
    pub catalog_revision: u64,
    pub selection: ModelSelection,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ModelResolveError {
    #[error("Provider「{0}」未注册")]
    ProviderNotFound(String),
    #[error("模型 {0}/{1} 未注册")]
    ModelNotFound(String, String),
    #[error("模型 {0}/{1} 已禁用")]
    ModelDisabled(String, String),
    #[error("模型 {provider_id}/{model_id} 推理档位无效: {effort}")]
    InvalidEffort { provider_id: String, model_id: String, effort: String },
    #[error("模型 {provider_id}/{model_id} 不支持原生搜索")]
    NativeSearchUnsupported { provider_id: String, model_id: String },
    #[error("模型 {provider_id}/{model_id} 所属路由不支持原生搜索")]
    NativeSearchRouteBlocked { provider_id: String, model_id: String },
    #[error("模型 {provider_id}/{model_id} 输出上限 {requested} 超出最大值 {maximum}")]
    InvalidOutputLimit { provider_id: String, model_id: String, requested: u32, maximum: u32 },
}

pub fn resolve_model_selection(
    doc: &ProfilesDocument,
    selection: &ModelSelection,
) -> Result<EffectiveModelConfig, ModelResolveError> {
    let provider = doc.providers.get(&selection.provider_id)
        .ok_or_else(|| ModelResolveError::ProviderNotFound(selection.provider_id.clone()))?;
    let model = provider.models.get(&selection.model_id)
        .ok_or_else(|| ModelResolveError::ModelNotFound(selection.provider_id.clone(), selection.model_id.clone()))?;
    if !model.enabled {
        return Err(ModelResolveError::ModelDisabled(selection.provider_id.clone(), selection.model_id.clone()));
    }
    let max_output_tokens = selection.options.max_output_tokens
        .or(model.defaults.max_output_tokens).unwrap_or(model.max_output_tokens);
    if max_output_tokens == 0 || max_output_tokens > model.max_output_tokens {
        return Err(ModelResolveError::InvalidOutputLimit {
            provider_id: selection.provider_id.clone(), model_id: selection.model_id.clone(),
            requested: max_output_tokens, maximum: model.max_output_tokens,
        });
    }
    let reasoning_effort = selection.options.reasoning_effort.clone().or(model.defaults.reasoning_effort.clone());
    if let Some(effort) = &reasoning_effort
        && (!model.capabilities.reasoning || !model.reasoning_levels.contains(effort))
    {
        return Err(ModelResolveError::InvalidEffort {
            provider_id: selection.provider_id.clone(), model_id: selection.model_id.clone(), effort: effort.clone(),
        });
    }
    let native_web_search = selection.options.native_web_search.unwrap_or(model.defaults.native_web_search);
    if native_web_search && !model.capabilities.native_web_search {
        return Err(ModelResolveError::NativeSearchUnsupported {
            provider_id: selection.provider_id.clone(), model_id: selection.model_id.clone(),
        });
    }
    if native_web_search && (provider.credential == CredentialSource::ChatgptPlan
        || (provider.wire == Wire::Responses && !provider.compat.as_ref().is_none_or(|compat| compat.responses_web_search)))
    {
        return Err(ModelResolveError::NativeSearchRouteBlocked {
            provider_id: selection.provider_id.clone(), model_id: selection.model_id.clone(),
        });
    }
    Ok(EffectiveModelConfig {
        provider_id: selection.provider_id.clone(), model_id: selection.model_id.clone(),
        display_name: model.display_name.clone(), context_window: model.context_window,
        max_output_tokens, reasoning_effort: reasoning_effort.clone(), native_web_search,
        capabilities: model.capabilities, reasoning_levels: model.reasoning_levels.clone(),
        base_url: provider.base_url.clone(), wire: provider.wire, credential: provider.credential,
        secret_slot: format!("secrets.providers.{}", selection.provider_id),
        headers: provider.headers.clone(), compat: provider.compat.clone(), catalog_revision: doc.revision,
        selection: ModelSelection { provider_id: selection.provider_id.clone(), model_id: selection.model_id.clone(),
            options: crate::model::ModelSelectionOptions { max_output_tokens: Some(max_output_tokens),
                reasoning_effort, native_web_search: Some(native_web_search) } },
    })
}

/// 权威解析入口
pub fn resolve_effective(
    doc: &ProfilesDocument,
    requested_profile_id: Option<&str>,
) -> Result<EffectiveExecutionProfile, ProfileResolveError> {
    // 1. 确定最终选用的 profile_id
    let profile_id = match requested_profile_id {
        Some(id) if !id.trim().is_empty() => id.trim(),
        _ => doc.default_profile.as_deref().unwrap_or(""),
    };

    // 2. 获取 ProfileDef
    let profile = doc.profiles.get(profile_id).ok_or_else(|| {
        if doc.default_profile.as_deref() == Some(profile_id) {
            ProfileResolveError::DefaultProfileNotFound(profile_id.to_string())
        } else {
            ProfileResolveError::ProfileNotFound(profile_id.to_string())
        }
    })?;

    // 3. 获取对应的 ProviderDef
    let provider = doc
        .providers
        .get(&profile.provider)
        .ok_or_else(|| {
            ProfileResolveError::ProviderNotFound(profile_id.to_string(), profile.provider.clone())
        })?;

    // 4. 尝试寻找 Provider 下挂载的对应 ModelDef
    let model_def = provider.models.get(&profile.model);

    // 5. 合并参数：Profile 覆盖优先，其次取 Model 默认，最后取硬底值
    let context_length = profile
        .context_length
        .or_else(|| model_def.map(|m| m.context_window))
        .unwrap_or(128_000);

    let max_tokens = profile
        .max_tokens
        .or_else(|| model_def.map(|m| m.max_output_tokens))
        .unwrap_or(8_192);

    let reasoning_effort = profile
        .reasoning_effort
        .clone()
        .or_else(|| model_def.and_then(|m| m.defaults.reasoning_effort.clone()));

    Ok(EffectiveExecutionProfile {
        profile_id: profile_id.to_string(),
        profile_name: profile.name.clone(),
        provider_id: profile.provider.clone(),
        provider_name: provider.name.clone(),
        model: profile.model.clone(),
        base_url: provider.base_url.clone(),
        wire: provider.wire,
        credential: provider.credential,
        secret_slot: format!("secrets.providers.{}", profile.provider),
        context_length,
        max_tokens,
        reasoning_effort,
        headers: provider.headers.clone(),
        compat: provider.compat.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn configured_document() -> ProfilesDocument {
        use crate::model::{ModelDef, ModelDefaults, ProfileDef, ProviderDef};
        let mut doc = ProfilesDocument::default();
        doc.providers.insert("custom".into(), ProviderDef {
            name: "User endpoint".into(), base_url: "https://example.invalid/v1".into(),
            wire: Wire::Responses, credential: CredentialSource::ApiKey, api_key: None,
            headers: BTreeMap::new(), compat: None,
            models: BTreeMap::from([
                ("reasoning-model".into(), ModelDef {
                    reasoning_levels: vec!["low".into(), "high".into()],
                    capabilities: qaqh_types::ModelCapabilities { reasoning: true, ..Default::default() },
                    defaults: ModelDefaults { reasoning_effort: Some("high".into()), ..Default::default() },
                    ..Default::default()
                }),
                ("plain-model".into(), ModelDef::default()),
            ]),
        });
        for (id, model) in [("reasoning-profile", "reasoning-model"), ("plain-profile", "plain-model")] {
            doc.profiles.insert(id.into(), ProfileDef {
                name: id.into(), provider: "custom".into(), model: model.into(),
                reasoning_effort: None, context_length: None, max_tokens: None,
            });
        }
        doc.default_profile = Some("reasoning-profile".into());
        doc
    }


    #[test]
    fn test_resolve_default_user_profile() {
        let doc = configured_document();
        let eff = resolve_effective(&doc, None).expect("resolve default");
        assert_eq!(eff.profile_id, "reasoning-profile");
        assert_eq!(eff.provider_id, "custom");
        assert_eq!(eff.model, "reasoning-model");
        assert_eq!(eff.base_url, "https://example.invalid/v1");
        assert_eq!(eff.wire, Wire::Responses);
        assert_eq!(eff.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(eff.context_length, 128_000);
        assert_eq!(eff.max_tokens, 8_192);
        assert_eq!(eff.secret_slot, "secrets.providers.custom");
    }

    #[test]
    fn test_resolve_explicit_profile_no_side_effects() {
        let doc = configured_document();
        let eff = resolve_effective(&doc, Some("plain-profile")).expect("resolve v3");
        assert_eq!(eff.profile_id, "plain-profile");
        assert_eq!(eff.model, "plain-model");
        assert_eq!(eff.reasoning_effort, None);
        // 全局默认依然是 r1，不受解析影响
        assert_eq!(doc.default_profile.as_deref(), Some("reasoning-profile"));
    }

    #[test]
    fn test_resolve_missing_profile_errors() {
        let doc = configured_document();
        let err = resolve_effective(&doc, Some("non-existent")).expect_err("should fail");
        assert_eq!(err, ProfileResolveError::ProfileNotFound("non-existent".to_string()));
    }

    #[test]
    fn model_selection_checks_identity_and_output_range() {
        let doc = configured_document();
        let mut selection = ModelSelection {
            provider_id: "custom".into(),
            model_id: "reasoning-model".into(),
            options: Default::default(),
        };
        let effective = resolve_model_selection(&doc, &selection).expect("resolve model");
        assert_eq!(effective.provider_id, "custom");
        assert_eq!(effective.selection.options.max_output_tokens, Some(8192));
        selection.options.max_output_tokens = Some(9000);
        assert!(matches!(resolve_model_selection(&doc, &selection), Err(ModelResolveError::InvalidOutputLimit { .. })));
    }
}
