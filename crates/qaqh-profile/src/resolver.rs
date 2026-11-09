//! 权威执行配置解析器（效仿 ZCode Selection Resolver 设计）。
//!
//! 负责从 ProfilesDocument 解析出会话所需的最终执行配置：
//! - 自动从 Provider 中拉取 BaseURL、Wire、请求头与凭据槽位；
//! - 自动将 Selection options 与 Model 默认值合并；
//! - 解析过程纯函数式、零全局副作用，不改变任何其他会话的状态。

use std::collections::BTreeMap;
use qaqh_types::{CredentialSource, EndpointCompat, Wire};
use thiserror::Error;
use crate::model::{ModelSelection, ProfilesDocument};

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

#[cfg(test)]
mod tests {
    use super::*;

    fn configured_document() -> ProfilesDocument {
        use crate::model::{ModelDef, ModelDefaults, ProviderDef};
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
        doc
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
