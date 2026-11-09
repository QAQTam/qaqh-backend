//! 领域模型定义：Provider、Model 与 Profile（参考 ZCode 设计规范）。
//!
//! 核心原则：
//! 1. Provider 承载端点、协议、认证槽位与通用请求头，一个 Provider 拥有多个 Models；
//! 2. Model 承载能力元数据（上下文窗口、最大输出 Token、推理思考支持度）；
//! 3. 全局默认选择是一枚 `default_model_selection` 指针（provider_id + model_id），
//!    不存在第二层「预设组合」——按 Spec §3.1，providers + active 就是全部。

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
    /// 单次回合最大输出 Token 上限。**必填**：缺失/为 0 在解析与选择时
    /// 一律报错，不做 default/alias 兜底（模型配置统一重构 Spec §3.5）。
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

fn default_enabled() -> bool { true }

impl Default for ModelDef {
    fn default() -> Self {
        Self {
            enabled: true,
            display_name: None,
            context_window: default_context_window(),
            max_output_tokens: 8_192,
            reasoning_levels: Vec::new(),
            capabilities: ModelCapabilities::default(),
            defaults: ModelDefaults::default(),
        }
    }
}

/// 模型提供商 / 服务端点定义
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
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

/// 完整的 providers.toml 配置树
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct ProfilesDocument {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model_selection: Option<ModelSelection>,
    /// 所有已注册的服务商字典 (ProviderId -> ProviderDef)
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderDef>,
}

impl Default for ProfilesDocument {
    fn default() -> Self {
        Self { schema_version: 1, revision: 0, default_model_selection: None,
            providers: BTreeMap::new() }
    }
}

fn default_schema_version() -> u32 { 1 }

impl ProfilesDocument {
    pub fn is_empty(&self) -> bool { self.providers.is_empty() }

    pub fn model_is_referenced(&self, provider_id: &str, model_id: &str) -> bool {
        self.default_model_selection.as_ref().is_some_and(|s| s.provider_id == provider_id && s.model_id == model_id)
    }
}
