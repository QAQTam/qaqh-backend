//! BYOK 迁移层：把旧配置里的 `(provider_id, endpoint)` 坐标解析成一条具体端点记录。
//!
//! 内置 provider 目录已退役——设置面只有六个字段（endpoint / wire / apikey /
//! model / max_token / context_length）。本模块只读 `assets/legacy-providers.toml`
//! （一次性迁移数据，不再是可选项），把老配置指向的预设翻译成
//! `base_url + wire + compat`，使升级不改变任何在途端点的请求形状。
//! 迁移落盘后（下个大版本）连同本模块一起删除。

use serde::Deserialize;

use qaqh_types::{CacheTokenField, EndpointCompat, ThinkingParamMode, UserSendMode, Wire};

/// `assets/legacy-providers.toml` 的编译期快照。
const LEGACY_TOML: &str = include_str!("../../../assets/legacy-providers.toml");

/// One migrated preset coordinate, expressed the way BYOK config states it.
#[derive(Debug, Clone, PartialEq)]
pub struct LegacyPreset {
    pub wire: Wire,
    pub base_url: String,
    /// 预设的兜底模型：仅在用户配置里还没有模型时使用，绝不覆盖用户选定的模型。
    pub model: String,
    pub compat: EndpointCompat,
}

/// 查一条旧预设。`endpoint_id` 缺失或对不上时取该 provider 的第一条。
pub fn legacy_preset(provider_id: &str, endpoint_id: &str) -> Option<LegacyPreset> {
    let provider = legacy_rows().iter().find(|p| p.id == provider_id)?;
    let endpoint = provider
        .endpoints
        .iter()
        .find(|e| e.id == endpoint_id)
        .or_else(|| provider.endpoints.first())?;
    Some(endpoint.to_preset())
}

/// 旧 provider id 是否还在迁移表里。
pub fn legacy_provider_exists(provider_id: &str) -> bool {
    legacy_rows().iter().any(|p| p.id == provider_id)
}

/// 旧 provider 的第一条 endpoint id（配置里没写 endpoint 时的缺省）。
pub fn legacy_first_endpoint(provider_id: &str) -> Option<String> {
    legacy_rows()
        .iter()
        .find(|p| p.id == provider_id)
        .and_then(|p| p.endpoints.first())
        .map(|e| e.id.clone())
}

/// 校验 BYOK 的 endpoint URL：`https`，或仅限 loopback 的 `http`
/// （设计文档 §7 风险项）。空串由调用方按"未配置"处理，不在此报错。
pub fn validate_endpoint_url(url: &str) -> Result<(), String> {
    if url.is_empty() {
        return Ok(());
    }
    // 轻量校验，不引入 url crate：按 `scheme://rest` 切分。
    let Some((scheme, rest)) = url.split_once("://") else {
        return Err(format!("endpoint 缺少 scheme: {url}"));
    };
    match scheme {
        "https" => {}
        "http" => {
            let host = rest.split(['/', ':']).next().unwrap_or("");
            if host != "localhost" && host != "127.0.0.1" {
                return Err(format!(
                    "endpoint 的 http 仅允许 localhost/127.0.0.1（生产端点必须 https）: {url}"
                ));
            }
        }
        _ => return Err(format!("endpoint scheme 必须是 https/http: {url}")),
    }
    Ok(())
}

// ── 迁移数据形态 ──

#[derive(Debug, Deserialize)]
struct LegacyFile {
    #[serde(default)]
    providers: Vec<LegacyProvider>,
}

#[derive(Debug, Deserialize)]
struct LegacyProvider {
    id: String,
    #[serde(default)]
    endpoints: Vec<LegacyEndpoint>,
}

/// 旧预设端点的键集：只取迁移用得上的字段。其余（`display` / `models` /
/// `models_url` / `beta` / `has_balance` / `balance_path` / `stateful`）随预设
/// 一起退役，解析时忽略。
#[derive(Debug, Clone, Deserialize)]
struct LegacyEndpoint {
    id: String,
    #[serde(default)]
    protocol: String,
    #[serde(default)]
    base_url: String,
    #[serde(default)]
    default_model: String,
    #[serde(default)]
    user_id_mode: Option<UserSendMode>,
    #[serde(default)]
    chat_path: Option<String>,
    #[serde(default)]
    responses_path: Option<String>,
    #[serde(default)]
    anthropic_path: Option<String>,
    #[serde(default)]
    thinking_mode: Option<ThinkingParamMode>,
    #[serde(default)]
    cache_field: Option<CacheTokenField>,
    #[serde(default)]
    include_stream_usage: Option<bool>,
    #[serde(default)]
    supports_thinking: Option<bool>,
    #[serde(default)]
    thinking_budget_large: Option<bool>,
    #[serde(default)]
    supports_reasoning_effort: Option<bool>,
    #[serde(default)]
    effort_allowlist: Option<Vec<String>>,
    #[serde(default)]
    tool_call_content_null: Option<bool>,
    #[serde(default)]
    supports_reasoning_content: Option<bool>,
    #[serde(default)]
    require_provider_parameters: Option<bool>,
    #[serde(default)]
    do_sample: Option<bool>,
    #[serde(default)]
    responses_web_search: Option<bool>,
    #[serde(default)]
    responses_echo_web_search_call: Option<bool>,
    #[serde(default)]
    responses_send_include: Option<bool>,
    #[serde(default)]
    responses_effort_max: Option<String>,
    #[serde(default)]
    responses_supports_user: Option<bool>,
    #[serde(default)]
    responses_search_function_alias: Option<String>,
    #[serde(default)]
    responses_echo_reasoning_content: Option<bool>,
    #[serde(default)]
    retry: Option<qaqh_types::RetrySpec>,
}

impl LegacyEndpoint {
    fn wire(&self) -> Wire {
        Wire::parse(&self.protocol).unwrap_or_default()
    }

    /// 旧平铺字段 → compat：只保留"与该 wire 缺省不同"的表达，重复缺省的值归一掉，
    /// 免得迁移后的配置里塞满无意义声明。
    fn compat(&self) -> EndpointCompat {
        let defaults = EndpointCompat::default();
        let wire = self.wire();
        let path = match wire {
            Wire::OpenAi => self.chat_path.clone(),
            Wire::Responses => self.responses_path.clone(),
            Wire::Anthropic => self.anthropic_path.clone(),
            // 迁移表里没有 Gemini 端点（该 wire 是 BYOK 之后新增的）。
            Wire::Gemini => None,
        }
        .filter(|p| *p != defaults.path_for(wire));
        let flag = |given: Option<bool>, fallback: bool| given.unwrap_or(fallback);
        EndpointCompat {
            path,
            thinking_mode: self.thinking_mode.clone().unwrap_or(defaults.thinking_mode),
            cache_field: self.cache_field.clone().unwrap_or(defaults.cache_field),
            include_stream_usage: flag(self.include_stream_usage, defaults.include_stream_usage),
            supports_thinking: flag(self.supports_thinking, defaults.supports_thinking),
            thinking_budget_large: flag(self.thinking_budget_large, defaults.thinking_budget_large),
            supports_reasoning_effort: flag(
                self.supports_reasoning_effort,
                defaults.supports_reasoning_effort,
            ),
            effort_allowlist: self.effort_allowlist.clone(),
            tool_call_content_null: flag(
                self.tool_call_content_null,
                defaults.tool_call_content_null,
            ),
            supports_reasoning_content: flag(
                self.supports_reasoning_content,
                defaults.supports_reasoning_content,
            ),
            require_provider_parameters: flag(
                self.require_provider_parameters,
                defaults.require_provider_parameters,
            ),
            do_sample: self.do_sample,
            user_id_mode: self.user_id_mode.clone(),
            responses_web_search: flag(self.responses_web_search, defaults.responses_web_search),
            responses_echo_web_search_call: flag(
                self.responses_echo_web_search_call,
                defaults.responses_echo_web_search_call,
            ),
            responses_send_include: flag(
                self.responses_send_include,
                defaults.responses_send_include,
            ),
            responses_effort_max: self
                .responses_effort_max
                .clone()
                .unwrap_or(defaults.responses_effort_max),
            responses_supports_user: flag(
                self.responses_supports_user,
                defaults.responses_supports_user,
            ),
            responses_search_function_alias: self.responses_search_function_alias.clone(),
            responses_echo_reasoning_content: flag(
                self.responses_echo_reasoning_content,
                defaults.responses_echo_reasoning_content,
            ),
            retry: self.retry.clone(),
        }
    }

    fn to_preset(&self) -> LegacyPreset {
        LegacyPreset {
            wire: self.wire(),
            base_url: self.base_url.clone(),
            model: self.default_model.clone(),
            compat: self.compat(),
        }
    }
}

/// 迁移表：进程内解析一次并缓存（纯只读数据，无需失效）。
fn legacy_rows() -> &'static [LegacyProvider] {
    static TABLE: std::sync::OnceLock<Vec<LegacyProvider>> = std::sync::OnceLock::new();
    TABLE
        .get_or_init(|| {
            toml::from_str::<LegacyFile>(LEGACY_TOML)
                .unwrap_or_else(|e| {
                    // 构建期由 tests::legacy_table_parses_and_covers_every_coordinate
                    // 兜底；运行期解析失败只能是构建产物损坏。
                    panic!("assets/legacy-providers.toml parse failed: {e}")
                })
                .providers
        })
        .as_slice()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_table_parses_and_covers_every_coordinate() {
        let rows = legacy_rows();
        assert_eq!(rows.len(), 13, "迁移表应有 13 个旧 provider");
        let endpoints: usize = rows.iter().map(|p| p.endpoints.len()).sum();
        assert_eq!(endpoints, 19, "迁移表应有 19 条旧端点");
        for provider in rows {
            for endpoint in &provider.endpoints {
                assert!(
                    !endpoint.base_url.is_empty(),
                    "{}/{} 缺 base_url",
                    provider.id,
                    endpoint.id
                );
                // 旧 protocol 字面量必须都能落到某条 wire。
                assert!(
                    Wire::parse(&endpoint.protocol).is_some(),
                    "{}/{} 的 protocol 无法识别: {}",
                    provider.id,
                    endpoint.id,
                    endpoint.protocol
                );
            }
        }
    }

    #[test]
    fn legacy_preset_maps_wire_url_and_compat_defaults() {
        let preset = legacy_preset("deepseek", "openai").expect("deepseek/openai");
        assert_eq!(preset.wire, Wire::OpenAi);
        assert_eq!(preset.base_url, "https://api.deepseek.com");
        assert!(preset.compat.include_stream_usage);
        assert_eq!(
            preset.compat.cache_field,
            CacheTokenField::PromptCacheHitTokens
        );

        // responses 端点预设里的路径就是该 wire 的缺省路径 → 归一为不声明。
        let responses = legacy_preset("deepseek", "responses").expect("deepseek/responses");
        assert_eq!(responses.wire, Wire::Responses);
        assert_eq!(responses.compat.path, None);
        assert_eq!(responses.compat.responses_effort_max, "max");
    }

    #[test]
    fn legacy_preset_keeps_request_shape_deviations() {
        // 每一项都是"该 wire 之外的差异"，迁移后必须由 compat 原样表达。
        let qwen = legacy_preset("qwen", "openai").expect("qwen/openai");
        assert_eq!(
            qwen.compat.thinking_mode,
            ThinkingParamMode::QwenEnableThinking
        );
        assert_eq!(
            qwen.compat.cache_field,
            CacheTokenField::PromptDetailsCached
        );
        assert_eq!(
            qwen.compat.path.as_deref(),
            Some("/compatible-mode/v1/chat/completions")
        );

        let kimi = legacy_preset("kimi", "openai").expect("kimi/openai");
        assert_eq!(kimi.compat.cache_field, CacheTokenField::UsageCachedTokens);

        let mimo = legacy_preset("mimo", "openai").expect("mimo/openai");
        assert_eq!(mimo.compat.cache_field, CacheTokenField::None);

        let glm = legacy_preset("glm", "openai").expect("glm/openai");
        assert_eq!(glm.compat.do_sample, Some(false));

        let minimax = legacy_preset("minimax", "openai").expect("minimax/openai");
        assert_eq!(
            minimax.compat.thinking_mode,
            ThinkingParamMode::MiniMaxAdaptive
        );

        let openrouter = legacy_preset("openrouter", "openai").expect("openrouter/openai");
        assert_eq!(
            openrouter.compat.effort_allowlist,
            Some(vec![
                "max".to_string(),
                "high".to_string(),
                "low".to_string()
            ])
        );

        let zcode = legacy_preset("zcode", "anthropic").expect("zcode/anthropic");
        assert_eq!(zcode.wire, Wire::Anthropic);
        assert_eq!(
            zcode.compat.path.as_deref(),
            Some("/api/anthropic/v1/messages")
        );
        assert!(zcode.compat.thinking_budget_large);
    }

    #[test]
    fn unknown_coordinates_have_no_preset() {
        assert!(legacy_preset("nope", "openai").is_none());
        assert!(legacy_first_endpoint("nope").is_none());
        assert!(!legacy_provider_exists("nope"));
        assert!(legacy_provider_exists("deepseek"));
        // endpoint id 缺失时对不上号 → 取该 provider 的第一条。
        assert_eq!(
            legacy_preset("deepseek", ""),
            legacy_preset("deepseek", "openai")
        );
    }

    #[test]
    fn endpoint_url_validation_accepts_https_and_loopback_http() {
        assert!(validate_endpoint_url("https://api.example.com/v1").is_ok());
        assert!(validate_endpoint_url("http://localhost:8317/v1").is_ok());
        assert!(validate_endpoint_url("http://127.0.0.1:11434").is_ok());
        assert!(
            validate_endpoint_url("").is_ok(),
            "空值由调用方按未配置处理"
        );

        let remote_http = validate_endpoint_url("http://api.example.com").unwrap_err();
        assert!(remote_http.contains("localhost"), "{remote_http}");
        assert!(
            validate_endpoint_url("api.example.com")
                .unwrap_err()
                .contains("scheme")
        );
        assert!(
            validate_endpoint_url("ftp://api.example.com")
                .unwrap_err()
                .contains("https")
        );
    }
}
