//! 内置 provider 预设静态表（模型配置统一重构 Spec §3.2）。
//!
//! 预设是**代码里的一张静态表**：预设 id 对应默认 endpoint、协议与凭据类型，
//! 外加 [`preset_compat`] 的端点能力声明（文档确凿事实的按 id 映射——旧
//! 「按服务商特判表」的继承者，gate 侧只见 [`EndpointCompat`] 字段）。
//! 用户选了预设，前端把默认值复制进 `provider.upsert` 落自己的目录条目；
//! compat 未随 DTO 提交时由 upsert 侧回退预设声明。之后一律以 providers.toml
//! 为准，不再回头读这张表。
//!
//! 纪律：只登记代码里有确凿端点事实的 provider（协议 SDK / OAuth 路由 / 端点
//! facts），不新增未接通的厂商；ChatGPT plan 走 credential=chatgpt_plan 的
//! 专用路由，不在 BYOK 预设之列。

use qaqh_types::{CacheTokenField, CredentialSource, EndpointCompat, Wire};
use serde::Serialize;

/// 一条 provider 预设：默认 endpoint + 协议 + 凭据类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderPreset {
    /// 预设 id（建议直接作为 provider id；字符集 [a-z0-9_-]，与目录校验一致）
    pub id: &'static str,
    /// 展示名
    pub name: &'static str,
    /// 默认 endpoint（scheme + host + 可选前缀）
    pub base_url: &'static str,
    /// 请求/响应协议
    pub wire: Wire,
    /// 凭据类型
    pub credential: CredentialSource,
}

/// 静态预设表。顺序即设置页展示顺序。
pub const PROVIDER_PRESETS: &[ProviderPreset] = &[
    ProviderPreset {
        id: "deepseek",
        name: "DeepSeek 官方",
        base_url: "https://api.deepseek.com",
        wire: Wire::Chat,
        credential: CredentialSource::ApiKey,
    },
    ProviderPreset {
        id: "deepseek-oauth",
        name: "DeepSeek 官方（OAuth 套餐）",
        base_url: "https://api.deepseek.com",
        wire: Wire::Responses,
        credential: CredentialSource::DeepseekAccount,
    },
    ProviderPreset {
        id: "openai",
        name: "OpenAI",
        base_url: "https://api.openai.com/v1",
        wire: Wire::Chat,
        credential: CredentialSource::ApiKey,
    },
    ProviderPreset {
        id: "anthropic",
        name: "Anthropic",
        base_url: "https://api.anthropic.com",
        wire: Wire::Messages,
        credential: CredentialSource::ApiKey,
    },
    ProviderPreset {
        id: "gemini",
        name: "Google Gemini",
        base_url: "https://generativelanguage.googleapis.com",
        wire: Wire::Gemini,
        credential: CredentialSource::ApiKey,
    },
    ProviderPreset {
        id: "qianwen",
        name: "千问 AI 平台（OpenAI 兼容面）",
        base_url: "https://maas.qianwenaiapi.com/compatible-mode/v1",
        wire: Wire::Chat,
        credential: CredentialSource::ApiKey,
    },
];

/// 预设端点的能力声明（文档确凿事实，非运行时猜测）。
///
/// `provider_upsert` 在调用方未提交 `compat` 且 id 命中预设表时以此为默认；
/// 用户自拟 provider 与各预设的既有行为（返回 `None` = 目录缺省全默认）不变。
/// 这是旧「按服务商特判表」的继承者：声明跟着预设 id 走，gate 只看
/// [`EndpointCompat`] 字段，不出现 provider 名分支（I15 纪律）。
pub fn preset_compat(id: &str) -> Option<EndpointCompat> {
    match id {
        // 千问 AI 平台 OpenAI 兼容面（/compatible-mode/v1/chat/completions）：
        // 缓存命中数在 usage.prompt_tokens_details.cached_tokens（官方
        // OpenAI 形状）；顶层 prompt_cache_hit/miss_tokens 是 DashScope 原生
        // 面字段，兼容面不返回——缺省档会静默判「未上报」。
        "qianwen" => {
            let mut compat = EndpointCompat::default();
            compat.cache_field = CacheTokenField::PromptDetailsCached;
            Some(compat)
        }
        _ => None,
    }
}

/// 按 id 查预设；未注册的 id 返回 `None`（自拟 provider 不经过预设）。
pub fn provider_preset(id: &str) -> Option<&'static ProviderPreset> {
    PROVIDER_PRESETS.iter().find(|preset| preset.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preset_ids_are_valid_provider_ids_and_unique() {
        let mut seen = std::collections::BTreeSet::new();
        for preset in PROVIDER_PRESETS {
            assert!(
                !preset.id.is_empty()
                    && preset.id.len() <= 64
                    && preset
                        .id
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-'),
                "preset id {:?} must satisfy the provider id charset",
                preset.id
            );
            assert!(seen.insert(preset.id), "duplicate preset id {:?}", preset.id);
            assert!(preset.base_url.starts_with("https://"), "{:?}", preset.id);
        }
    }

    #[test]
    fn lookup_misses_return_none() {
        assert!(provider_preset("deepseek").is_some());
        assert!(provider_preset("no-such-preset").is_none());
    }

    /// 能力声明只挂确凿端点：qianwen 声明嵌套 cached_tokens；其余预设与
    /// 自拟 id 一律 None（目录缺省 = 既有行为，逐字节不变）。
    #[test]
    fn preset_compat_declares_only_documented_endpoints() {
        let qianwen = preset_compat("qianwen").expect("qianwen declares compat");
        assert_eq!(qianwen.cache_field, CacheTokenField::PromptDetailsCached);
        for id in ["deepseek", "openai", "anthropic", "gemini", "my-router"] {
            assert!(preset_compat(id).is_none(), "{id} must declare nothing");
        }
    }
}
