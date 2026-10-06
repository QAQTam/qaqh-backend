//! Wire protocol and per-endpoint compatibility model (BYOK).
//!
//! There is no built-in provider catalogue: a user supplies six fields
//! (endpoint, wire, api key, model, max tokens, context length) and the harness
//! talks to whatever answers. The knobs that used to live in a vendor's preset
//! entry are now optional statements about *that one endpoint* —
//! [`EndpointCompat`] — and none of them is a provider-name branch: they are
//! request-shape switches the gate forwards without special-casing.

use serde::{Deserialize, Serialize};

/// Where the user identifier is placed in the API request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum UserSendMode {
    /// User ID is sent in the JSON request body.
    #[default]
    Body,
}

/// The wire protocol an endpoint speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Wire {
    /// OpenAI Chat Completions (`/chat/completions`).
    #[default]
    OpenAi,
    /// OpenAI Responses (`/responses`).
    Responses,
    /// Anthropic Messages (`/v1/messages`).
    Anthropic,
}

impl Wire {
    /// Stable identifier, matching the serialized form and the gate's dispatch key.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Responses => "responses",
            Self::Anthropic => "anthropic",
        }
    }

    /// Parse a wire name. Legacy protocol spellings are accepted so migrated
    /// configurations keep resolving (`"openai-compatible"` → [`Wire::OpenAi`]).
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "openai" | "openai-compatible" | "chat" | "chat_completions" | "chat/completions" => {
                Some(Self::OpenAi)
            }
            "responses" | "openai-responses" => Some(Self::Responses),
            "anthropic" | "messages" | "claude" => Some(Self::Anthropic),
            _ => None,
        }
    }
}

/// How the reasoning/thinking parameter is sent to the model.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ThinkingParamMode {
    /// Standard OpenAI format: `{"type": "enabled"|"disabled"}` at top-level body.
    #[default]
    OpenAi,
    /// Boolean `enable_thinking: true/false` at top-level body.
    QwenEnableThinking,
    /// `{"type": "adaptive"}` + `reasoning_split: true` at top-level.
    MiniMaxAdaptive,
}

/// Where the cache token count is located in the usage response.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum CacheTokenField {
    /// Top-level: `usage.prompt_cache_hit_tokens` + `usage.prompt_cache_miss_tokens`.
    #[default]
    PromptCacheHitTokens,
    /// Nested: `usage.prompt_tokens_details.cached_tokens`.
    PromptDetailsCached,
    /// Top-level single value: `usage.cached_tokens`.
    UsageCachedTokens,
    /// No cache information returned.
    None,
}

/// Per-endpoint retry policy. Absent → gate built-in defaults
/// (5 attempts / 1s base / 30s cap / 300s idle watchdog), which is also what
/// the unified transport SDK is configured with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RetrySpec {
    /// 最大尝试次数（含首次）。
    #[serde(default)]
    pub max_retries: u32,
    /// 首次重试基础退避秒数（指数翻倍 + ±10% jitter，封顶 `max_delay_secs`）。
    #[serde(default)]
    pub base_delay_secs: u64,
    /// 单次等待上限秒数。
    #[serde(default)]
    pub max_delay_secs: u64,
    /// 流空闲看门狗秒数（连续无新字节视为半开连接）。
    #[serde(default)]
    pub idle_timeout_secs: u64,
}

/// Optional statements about one BYOK endpoint's request shape.
///
/// Every field's default matches the protocol's own semantics, so an endpoint
/// that behaves the way its wire says declares nothing at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointCompat {
    /// Path appended to the endpoint base URL for the active wire.
    /// `None` = the wire's canonical path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// How the thinking/reasoning toggle is formatted. Default: `OpenAi`.
    #[serde(default)]
    pub thinking_mode: ThinkingParamMode,
    /// Which usage field carries the cache token count. Default: `PromptCacheHitTokens`.
    #[serde(default)]
    pub cache_field: CacheTokenField,
    /// Request an extra usage chunk before the streaming terminator. Not every
    /// Chat-Completions-compatible endpoint accepts `stream_options.include_usage`.
    #[serde(default)]
    pub include_stream_usage: bool,
    /// Whether the endpoint accepts a thinking/reasoning toggle. Default: true.
    #[serde(default = "default_true")]
    pub supports_thinking: bool,
    /// Large-context thinking budget tier (16k-96k instead of 1k-16k) on the
    /// Anthropic Messages path.
    #[serde(default)]
    pub thinking_budget_large: bool,
    /// Whether the endpoint accepts OpenAI's `reasoning_effort` parameter. Default: true.
    #[serde(default = "default_true")]
    pub supports_reasoning_effort: bool,
    /// Sparse allowlist of accepted `reasoning_effort` values. When set the gate
    /// clamps the requested effort to the nearest allowed rung instead of
    /// passing it through verbatim — routers otherwise silently ignore or reject
    /// off-domain values. `None` = no clamping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort_allowlist: Option<Vec<String>>,
    /// Whether assistant history entries containing tool calls must carry an
    /// explicit `content: null`. Some upstreams reject a missing member even
    /// though the OpenAI schema permits it.
    #[serde(default)]
    pub tool_call_content_null: bool,
    /// Whether provider-specific `reasoning_content` may be sent back as
    /// assistant history. Disable for routers that fan out to heterogeneous
    /// upstream schemas. Default: true.
    #[serde(default = "default_true")]
    pub supports_reasoning_content: bool,
    /// Ask a router to select only upstreams implementing every request
    /// parameter (`provider.require_parameters`).
    #[serde(default)]
    pub require_provider_parameters: bool,
    /// Explicit `do_sample` in the request body. `None` = not sent (API default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub do_sample: Option<bool>,
    /// Where to send the user identifier. `None` = not sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id_mode: Option<UserSendMode>,

    // ── Responses wire ──
    /// Inject the built-in `web_search` tool for server-side execution. Default: true.
    #[serde(default = "default_true")]
    pub responses_web_search: bool,
    /// Echo `web_search_call` output items back next turn so the server restores
    /// its search results. Default: true.
    #[serde(default = "default_true")]
    pub responses_echo_web_search_call: bool,
    /// Send `include: ["reasoning.encrypted_content"]`. Strict compatible
    /// endpoints that reject unknown members turn this off. Default: true.
    #[serde(default = "default_true")]
    pub responses_send_include: bool,
    /// Upper bound for `reasoning.effort`; values above it are clamped. Default: `"high"`.
    #[serde(default = "default_effort_max")]
    pub responses_effort_max: String,
    /// Send the `user` field (per-end-user rate limiting and KV-cache isolation).
    /// Default: true.
    #[serde(default = "default_true")]
    pub responses_supports_user: bool,
    /// Provider-facing alias for the canonical `search` function tool, for
    /// endpoints that reserve that name while their built-in web search is on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub responses_search_function_alias: Option<String>,
    /// Echo assistant `reasoning` items back verbatim next turn. Some endpoints
    /// reject tool-loop continuations without them. Default: true.
    #[serde(default = "default_true")]
    pub responses_echo_reasoning_content: bool,

    // ── Capability gates ──
    /// Whether the endpoint accepts image parts (vision input). Gates the
    /// `read_image` client tool so a model never sees a tool it cannot use.
    /// Default: false — a BYOK endpoint opts in.
    #[serde(default)]
    pub supports_image_tool: bool,
    /// Per-model vision allowlist for endpoints serving heterogeneous models.
    /// When `supports_image_tool` is true: `None` = every model accepts images,
    /// `Some` = only the listed ids, matched exactly or by `*` suffix
    /// (e.g. `"gemini-*"`), case-insensitively against the active model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_models: Option<Vec<String>>,
    /// Per-endpoint retry policy override. `None` = transport defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<RetrySpec>,
}

fn default_true() -> bool {
    true
}

fn default_effort_max() -> String {
    "high".into()
}

impl Default for EndpointCompat {
    fn default() -> Self {
        Self {
            path: None,
            thinking_mode: ThinkingParamMode::default(),
            cache_field: CacheTokenField::default(),
            include_stream_usage: false,
            supports_thinking: true,
            thinking_budget_large: false,
            supports_reasoning_effort: true,
            effort_allowlist: None,
            tool_call_content_null: false,
            supports_reasoning_content: true,
            require_provider_parameters: false,
            do_sample: None,
            user_id_mode: None,
            responses_web_search: true,
            responses_echo_web_search_call: true,
            responses_send_include: true,
            responses_effort_max: "high".into(),
            responses_supports_user: true,
            responses_search_function_alias: None,
            responses_echo_reasoning_content: true,
            supports_image_tool: false,
            image_models: None,
            retry: None,
        }
    }
}

impl EndpointCompat {
    /// The path to request: the override when set, else the wire's canonical suffix.
    pub fn path_for(&self, wire: Wire) -> String {
        self.path.clone().unwrap_or_else(|| match wire {
            Wire::OpenAi => "/chat/completions".to_string(),
            Wire::Responses => "/responses".to_string(),
            Wire::Anthropic => "/v1/messages".to_string(),
        })
    }

    /// Whether `model` accepts image input on this endpoint — the gate for the
    /// `read_image` client tool, so a model never sees a tool it cannot use.
    ///
    /// `image_models` is the finer cut for endpoints serving heterogeneous
    /// models: absent = every model accepts images, present = only the listed
    /// ids, matched exactly or by `*` suffix, case-insensitively.
    pub fn supports_image_for_model(&self, model: &str) -> bool {
        if !self.supports_image_tool {
            return false;
        }
        match &self.image_models {
            None => true,
            Some(patterns) => {
                let model = model.to_lowercase();
                patterns.iter().any(|pattern| {
                    match pattern.strip_suffix('*') {
                        Some(prefix) => model.starts_with(&prefix.to_lowercase()),
                        None => model == pattern.to_lowercase(),
                    }
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_round_trips_through_its_stable_name() {
        #[derive(Deserialize)]
        struct Row {
            wire: Wire,
        }
        for (wire, name) in [
            (Wire::OpenAi, "openai"),
            (Wire::Responses, "responses"),
            (Wire::Anthropic, "anthropic"),
        ] {
            assert_eq!(wire.as_str(), name);
            assert_eq!(Wire::parse(name), Some(wire));
            let row: Row = toml::from_str(&format!("wire = \"{name}\"")).expect(name);
            assert_eq!(row.wire, wire);
        }
        assert_eq!(Wire::parse("  OpenAI-Compatible "), Some(Wire::OpenAi));
        assert_eq!(Wire::parse("unknown"), None);
        assert_eq!(Wire::default(), Wire::OpenAi);
    }

    #[test]
    fn compat_needs_no_declaration_to_use_wire_defaults() {
        let compat: EndpointCompat = toml::from_str("").expect("empty table");
        assert!(compat.supports_thinking);
        assert!(!compat.supports_image_tool);
        assert_eq!(compat.responses_effort_max, "high");
        assert_eq!(compat.path, None);
        assert!(compat.retry.is_none());
    }

    #[test]
    fn compat_round_trips_without_losing_overrides() {
        let source = r#"
path = "/api/v1/messages"
thinking_mode = "MiniMaxAdaptive"
cache_field = "UsageCachedTokens"
supports_image_tool = true
image_models = ["vision-*"]
responses_effort_max = "max"
[retry]
max_retries = 3
idle_timeout_secs = 60
"#;
        let compat: EndpointCompat = toml::from_str(source).expect("parse");
        assert_eq!(compat.path.as_deref(), Some("/api/v1/messages"));
        assert_eq!(compat.thinking_mode, ThinkingParamMode::MiniMaxAdaptive);
        assert_eq!(compat.cache_field, CacheTokenField::UsageCachedTokens);
        assert!(compat.supports_image_tool);
        assert_eq!(
            compat.image_models.as_deref(),
            Some(["vision-*".to_string()].as_slice())
        );

        let written = toml::to_string(&compat).expect("serialize");
        let back: EndpointCompat = toml::from_str(&written).expect("reparse");
        assert_eq!(back.responses_effort_max, "max");
        assert_eq!(
            back.retry,
            Some(RetrySpec {
                max_retries: 3,
                idle_timeout_secs: 60,
                ..Default::default()
            })
        );
    }

    #[test]
    fn path_for_prefers_the_override_then_the_wire_suffix() {
        assert_eq!(
            EndpointCompat::default().path_for(Wire::Responses),
            "/responses"
        );
        let compat = EndpointCompat {
            path: Some("/v1/inner/responses".into()),
            ..Default::default()
        };
        assert_eq!(compat.path_for(Wire::OpenAi), "/v1/inner/responses");
    }

    #[test]
    fn image_gate_layers_endpoint_flag_and_model_allowlist() {
        let off = EndpointCompat::default();
        assert!(!off.supports_image_for_model("anything"));

        let uniform = EndpointCompat {
            supports_image_tool: true,
            ..Default::default()
        };
        assert!(uniform.supports_image_for_model("any-model"));

        let allowlist = EndpointCompat {
            supports_image_tool: true,
            image_models: Some(vec!["Vision-*".into(), "exact-id".into()]),
            ..Default::default()
        };
        assert!(allowlist.supports_image_for_model("vision-pro"));
        assert!(allowlist.supports_image_for_model("EXACT-ID"));
        assert!(
            !allowlist.supports_image_for_model("text-only"),
            "文本模型不得拿到 read_image"
        );
    }
}
