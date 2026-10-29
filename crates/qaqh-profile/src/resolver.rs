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
use crate::model::ProfilesDocument;

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
    /// 目标模型名称（例如 "deepseek-reasoner"）
    pub model: String,
    /// 服务端点 Base URL
    pub base_url: String,
    /// 传输协议形态
    pub wire: Wire,
    /// 凭据来源
    pub credential: CredentialSource,
    /// 对应的 secrets.toml 槽位路径（如 "secrets.providers.deepseek"）
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

/// 权威解析入口
pub fn resolve_effective(
    doc: &ProfilesDocument,
    requested_profile_id: Option<&str>,
) -> Result<EffectiveExecutionProfile, ProfileResolveError> {
    // 1. 确定最终选用的 profile_id
    let profile_id = match requested_profile_id {
        Some(id) if !id.trim().is_empty() => id.trim(),
        _ => doc.default_profile.as_str(),
    };

    // 2. 获取 ProfileDef
    let profile = doc.profiles.get(profile_id).ok_or_else(|| {
        if profile_id == doc.default_profile {
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
        .or_else(|| model_def.map(|m| m.max_tokens))
        .unwrap_or(8_192);

    let reasoning_effort = profile
        .reasoning_effort
        .clone()
        .or_else(|| model_def.and_then(|m| m.default_effort.clone()));

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

    #[test]
    fn test_resolve_default_builtin_profile() {
        let doc = ProfilesDocument::create_builtin_template();
        let eff = resolve_effective(&doc, None).expect("resolve default");
        assert_eq!(eff.profile_id, "deepseek-r1");
        assert_eq!(eff.provider_id, "deepseek");
        assert_eq!(eff.model, "deepseek-reasoner");
        assert_eq!(eff.base_url, "https://api.deepseek.com");
        assert_eq!(eff.wire, Wire::Responses);
        assert_eq!(eff.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(eff.context_length, 128_000);
        assert_eq!(eff.max_tokens, 8_192);
        assert_eq!(eff.secret_slot, "secrets.providers.deepseek");
    }

    #[test]
    fn test_resolve_explicit_profile_no_side_effects() {
        let doc = ProfilesDocument::create_builtin_template();
        let eff = resolve_effective(&doc, Some("deepseek-v3")).expect("resolve v3");
        assert_eq!(eff.profile_id, "deepseek-v3");
        assert_eq!(eff.model, "deepseek-chat");
        assert_eq!(eff.reasoning_effort, None);
        // 全局默认依然是 r1，不受解析影响
        assert_eq!(doc.default_profile, "deepseek-r1");
    }

    #[test]
    fn test_resolve_missing_profile_errors() {
        let doc = ProfilesDocument::create_builtin_template();
        let err = resolve_effective(&doc, Some("non-existent")).expect_err("should fail");
        assert_eq!(err, ProfileResolveError::ProfileNotFound("non-existent".to_string()));
    }
}
