//! 领域模型定义：Provider、Model 与 Profile（参考 ZCode 设计规范）。
//!
//! 核心原则：
//! 1. Provider 承载端点、协议、认证槽位与通用请求头，一个 Provider 拥有多个 Models；
//! 2. Model 承载能力元数据（上下文窗口、最大输出 Token、推理思考支持度）；
//! 3. Profile 是面向应用/会话的可复用预设组合（Provider + Model + 参数调整）；
//! 4. 彻底消除“全局扁平端点覆写”，全局仅持有一枚 `default_profile` 指针。

use std::collections::BTreeMap;
use qaqh_types::{CredentialSource, EndpointCompat, Wire};
pub use qaqh_types::{ModelCapabilities, ModelSelection, ModelSelectionOptions};
use serde::{Deserialize, Serialize};
#[cfg(feature = "ts")]
use ts_rs::TS;

/// 模型默认参数。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct ModelDefaults {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub native_web_search: bool,
}

/// 模型能力与元数据定义
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct ModelDef {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// 界面友好展示名（如 "DeepSeek R1 (深度推理)"）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// 上下文窗口大小（Token 数），默认 128,000
    #[serde(default = "default_context_window")]
    pub context_window: u32,
    /// 单次回合最大输出 Token 上限，默认 8,192
    #[serde(default = "default_max_tokens", alias = "max_tokens")]
    pub max_output_tokens: u32,
    #[serde(default)]
    pub reasoning_levels: Vec<String>,
    #[serde(default)]
    pub capabilities: ModelCapabilities,
    #[serde(default)]
    pub defaults: ModelDefaults,
}

fn default_context_window() -> u32 {
    128_000
}

fn default_max_tokens() -> u32 {
    8_192
}

fn default_enabled() -> bool { true }

impl Default for ModelDef {
    fn default() -> Self {
        Self {
            enabled: true,
            display_name: None,
            context_window: default_context_window(),
            max_output_tokens: default_max_tokens(),
            reasoning_levels: Vec::new(),
            capabilities: ModelCapabilities::default(),
            defaults: ModelDefaults::default(),
        }
    }
}

/// 模型提供商 / 服务端点定义
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderDef {
    /// 服务商展示名（如 "DeepSeek 官方"、"SiliconFlow 硅基流动"）
    pub name: String,
    /// 服务商 Base URL（如 "https://api.deepseek.com"）
    pub base_url: String,
    /// 通信协议形态（Chat / Responses / Messages / Gemini）
    #[serde(default)]
    pub wire: Wire,
    /// 凭据类型：api_key / chatgpt_plan / deepseek_account
    #[serde(default)]
    pub credential: CredentialSource,
    /// 凭据标记（"set" 表示已在 secrets.toml 存储，严禁明文落盘）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// 自定义出站请求头（如 DeepSeek 亲和头或组织 ID）
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// 端点协议补丁兼容参数
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compat: Option<EndpointCompat>,
    /// 该提供商所拥有的模型列表 (ModelId -> ModelDef)
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub models: BTreeMap<String, ModelDef>,
}

/// 档案预设定义（由 Provider + Model + 个性化参数组装）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileDef {
    /// 预设展示名称（如 "代码深度重构 (R1)"）
    pub name: String,
    /// 所引用的 Provider ID
    pub provider: String,
    /// 所引用的 Model ID
    pub model: String,
    /// 思考推理档位覆盖 ("low" | "medium" | "high" | "max")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// 上下文窗口覆盖（若未指定则继承所引用的 ModelDef）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_length: Option<u32>,
    /// 单次输出 Token 覆盖（若未指定则继承所引用的 ModelDef）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
}

/// 完整的 profiles.toml 配置树
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfilesDocument {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model_selection: Option<ModelSelection>,
    /// 旧目录迁移字段；不参与新执行路径。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_profile: Option<String>,
    /// 所有已注册的服务商字典 (ProviderId -> ProviderDef)
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderDef>,
    /// 所有可用的预设方案字典 (ProfileId -> ProfileDef)
    #[serde(default)]
    pub profiles: BTreeMap<String, ProfileDef>,
}

impl Default for ProfilesDocument {
    fn default() -> Self {
        Self { schema_version: 1, revision: 0, default_model_selection: None,
            default_profile: None, providers: BTreeMap::new(), profiles: BTreeMap::new() }
    }
}

fn default_schema_version() -> u32 { 1 }

impl ProfilesDocument {
    /// 创建开箱即用的官方规范化默认配置模板（包含 DeepSeek 官方双模型与 SiliconFlow 等）
    pub fn create_builtin_template() -> Self {
        let mut providers = BTreeMap::new();
        let mut profiles = BTreeMap::new();

        // 1. DeepSeek Provider 预设
        let mut ds_models = BTreeMap::new();
        ds_models.insert(
            "deepseek-reasoner".to_string(),
            ModelDef {
                enabled: true,
                display_name: Some("DeepSeek R1 (推理)".to_string()),
                context_window: 128_000,
                max_output_tokens: 8_192,
                reasoning_levels: vec!["low".into(), "medium".into(), "high".into()],
                capabilities: ModelCapabilities { tool_calls: true, reasoning: true, ..Default::default() },
                defaults: ModelDefaults { reasoning_effort: Some("high".into()), ..Default::default() },
            },
        );
        ds_models.insert(
            "deepseek-chat".to_string(),
            ModelDef {
                enabled: true,
                display_name: Some("DeepSeek V3 (通用)".to_string()),
                context_window: 128_000,
                max_output_tokens: 8_192,
                reasoning_levels: Vec::new(),
                capabilities: ModelCapabilities { tool_calls: true, ..Default::default() },
                defaults: ModelDefaults::default(),
            },
        );

        providers.insert(
            "deepseek".to_string(),
            ProviderDef {
                name: "DeepSeek 官方".to_string(),
                base_url: "https://api.deepseek.com".to_string(),
                wire: Wire::Responses,
                credential: CredentialSource::ApiKey,
                api_key: None,
                headers: BTreeMap::new(),
                compat: None,
                models: ds_models,
            },
        );

        // 2. Profiles 组装
        profiles.insert(
            "deepseek-r1".to_string(),
            ProfileDef {
                name: "DeepSeek R1 深度推理".to_string(),
                provider: "deepseek".to_string(),
                model: "deepseek-reasoner".to_string(),
                reasoning_effort: Some("high".to_string()),
                context_length: Some(128_000),
                max_tokens: Some(8_192),
            },
        );
        profiles.insert(
            "deepseek-v3".to_string(),
            ProfileDef {
                name: "DeepSeek V3 极速日常".to_string(),
                provider: "deepseek".to_string(),
                model: "deepseek-chat".to_string(),
                reasoning_effort: None,
                context_length: Some(128_000),
                max_tokens: Some(8_192),
            },
        );

        Self {
            schema_version: 1,
            revision: 1,
            default_model_selection: Some(ModelSelection {
                provider_id: "deepseek".into(), model_id: "deepseek-reasoner".into(),
                options: ModelSelectionOptions::default(),
            }),
            default_profile: Some("deepseek-r1".to_string()),
            providers,
            profiles,
        }
    }

    pub fn is_empty(&self) -> bool { self.providers.is_empty() && self.profiles.is_empty() }

    pub fn model_is_referenced(&self, provider_id: &str, model_id: &str) -> bool {
        self.default_model_selection.as_ref().is_some_and(|s| s.provider_id == provider_id && s.model_id == model_id)
            || self.profiles.values().any(|p| p.provider == provider_id && p.model == model_id)
    }
}
