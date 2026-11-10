//! 内置 provider 预设静态表（模型配置统一重构 Spec §3.2）。
//!
//! 预设是**代码里的一张静态表**：预设 id 对应默认 endpoint、协议与凭据类型。
//! 用户选了预设，前端把默认值复制进 `provider.upsert` 落自己的目录条目，
//! 之后一律以 providers.toml 为准，不再回头读这张表。
//!
//! 纪律：只登记代码里有确凿端点事实的 provider（协议 SDK / OAuth 路由 / 端点
//! facts），不新增未接通的厂商；ChatGPT plan 走 credential=chatgpt_plan 的
//! 专用路由，不在 BYOK 预设之列。

use serde::Serialize;
use qaqh_types::{CredentialSource, Wire};

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
        // gemini 面 canonical path 是 `/models/{model}:generateContent`
        // （不含版本段），base 必须自带 /v1beta，否则 URL 缺版本直接 404。
        base_url: "https://generativelanguage.googleapis.com/v1beta",
        wire: Wire::Gemini,
        credential: CredentialSource::ApiKey,
    },
    // 千问 AI 平台走 Anthropic 兼容面（POST /apps/anthropic/v1/messages）：
    // max_tokens 在 Chat 兼容面是「即将废弃、语义随模型漂移」的字段，而本面
    // 的 thinking/{cache_read,cache_creation}_input_tokens 全是 Anthropic 官方
    // 形状——gate 的 messages 适配器原生支持，无需任何 compat 声明。
    // base_url 以 /apps/anthropic 结尾（wire 缺省追加 /v1/messages；文档明确
    // 此面不提供 /v1/models，勿把 /v1 拼进 base）。
    ProviderPreset {
        id: "qianwen",
        name: "千问 AI 平台（Anthropic 兼容面）",
        base_url: "https://maas.qianwenaiapi.com/apps/anthropic",
        wire: Wire::Messages,
        credential: CredentialSource::ApiKey,
    },
    // 千问 Token Plan 包月专线（用户主付费通道）：host 是 token-plan 子域，
    // 面与上侧 Anthropic 兼容端点同形（base 止于 /apps/anthropic，wire
    // 追加 /v1/messages；此面无 /v1/models）。
    ProviderPreset {
        id: "qwen-token-plan-cn",
        name: "Qwen (Token-Plan CN)",
        base_url: "https://token-plan.maas.qianwenaiapi.com/apps/anthropic",
        wire: Wire::Messages,
        credential: CredentialSource::ApiKey,
    },
];

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
}
