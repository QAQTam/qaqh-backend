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
///
/// 统一命名对齐各上游官方称谓：Chat（OpenAI Chat Completions）、
/// Responses（OpenAI Responses）、Messages（Anthropic Messages）、
/// Gemini（Gemini `generateContent`）。序列化键即官方短名；历史拼写
/// （`"openai"` / `"anthropic"` / `"claude"` / `"chat_completions"` 等）
/// 经 serde alias 与 [`Wire::parse`] 继续接受，旧配置零迁移。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Wire {
    /// OpenAI Chat Completions (`/chat/completions`).
    #[default]
    #[serde(
        rename = "chat",
        alias = "openai",
        alias = "openai-compatible",
        alias = "chat_completions",
        alias = "chat/completions"
    )]
    Chat,
    /// OpenAI Responses (`/responses`).
    #[serde(alias = "openai-responses")]
    Responses,
    /// Anthropic Messages (`/v1/messages`).
    #[serde(rename = "messages", alias = "anthropic", alias = "claude")]
    Messages,
    /// Gemini `generateContent` (`/models/{model}:generateContent`).
    ///
    /// Streaming uses the sibling `:streamGenerateContent` path with
    /// `alt=sse`; authentication is the `key` query parameter, not a header.
    Gemini,
}

impl Wire {
    /// Stable identifier, matching the serialized form and the gate's dispatch key.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Responses => "responses",
            Self::Messages => "messages",
            Self::Gemini => "gemini",
        }
    }

    /// Parse a wire name. Legacy protocol spellings are accepted so migrated
    /// configurations keep resolving (`"openai-compatible"` → [`Wire::Chat`]).
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "openai" | "openai-compatible" | "chat" | "chat_completions" | "chat/completions" => {
                Some(Self::Chat)
            }
            "responses" | "openai-responses" => Some(Self::Responses),
            "anthropic" | "messages" | "claude" => Some(Self::Messages),
            "gemini"
            | "google"
            | "generatecontent"
            | "generate_content"
            | "gemini-generate-content" => Some(Self::Gemini),
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

/// 缓存用量口径：端点的 `input_tokens` 是否**已包含**缓存部分。
///
/// 真 Anthropic 是 `Exclusive`——`input_tokens` 只算未命中，`cache_read` /
/// `cache_creation` 是并列的两个桶，总输入 = 三者之和。不少把 OpenAI 口径套到
/// Anthropic wire 的反代/网关是 `Inclusive`——`input_tokens` 就是整段 prompt，
/// `cache_read_input_tokens` 只是它的子集。两种口径下"总输入"和"命中率分母"
/// 差近一倍，按错的读会把缓存前缀算两遍。
///
/// `Auto`（缺省）由 gate 按真实响应判定（两次一致才锁定，判不出时取协议契约
/// 的 `Exclusive`），判定逻辑与阈值见 `qaqh-gate/src/usage_semantics.rs`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheUsageSemantics {
    /// 按响应自动判定（缺省）。
    #[default]
    Auto,
    /// `input_tokens` 是整段 prompt（含命中），缓存桶是它的子集。
    Inclusive,
    /// `input_tokens` 只算未命中，缓存桶与它并列。
    Exclusive,
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
    /// Messages wire 的 usage 口径（`input_tokens` 是否含缓存）。默认 `Auto`。
    ///
    /// 口径已知的端点直接声明，省掉两轮自动判定；声明后不再观测。只影响
    /// message 侧 usage 的解读，不改变请求体（缓存命中本身由前缀决定）。
    #[serde(default)]
    pub cache_usage_semantics: CacheUsageSemantics,
    /// Request an extra usage chunk before the streaming terminator. Not every
    /// Chat-Completions-compatible endpoint accepts `stream_options.include_usage`.
    #[serde(default)]
    pub include_stream_usage: bool,
    /// 该端点是否提供官方 Search / Fetch 工具面（`POST /v1/tools/*`），并由工具层
    /// （`web_fetch` / `search`）改走官方端点。**默认关**。
    ///
    /// 开关只来自配置：工具层不按 URL、模型名或 provider id 特判（AGENTS-x I15）。
    /// 打开后该 profile 的模型面多一个 canonical `search` 工具；关闭时工具清单
    /// 与请求体保持与升级前逐字节一致，不会被动让缓存前缀失效。
    #[serde(default)]
    pub official_web_tools: bool,
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
    /// Send explicit session/task metadata on the OpenAI Chat request path.
    #[serde(default)]
    pub send_request_context: bool,

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
    /// 按「ChatGPT 套餐用量」直连路由的请求契约收口。**默认关**。
    ///
    /// 开启后（`crates/qaqh-gate`）：不发 `max_output_tokens`、剥离一组被禁字段
    /// （`temperature`/`top_p`/`metadata`/`user`/`prompt_cache_retention` …）、
    /// 关闭 hosted 工具，并要求走流式入口。`prompt_cache_key` 不在禁用清单里。
    /// 依据：`developers.openai.com/siwc/token-sharing-open-source/preview-limitations`。
    #[serde(default)]
    pub responses_siwc_plan: bool,

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

/// 端点凭据来源。
///
/// 显式声明而不是靠「api_key 为空 + 某个 compat 位」隐式触发：隐式推导会让一个
/// 配置错误变成「静默用了别的额度」——最难查、也最伤用户信任的一类问题。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialSource {
    /// 常规 BYOK：密钥来自 config/secrets（既有行为，缺省）。
    #[default]
    ApiKey,
    /// 用本机 ChatGPT 套餐额度：每回合向 SIWC 管理器取 access token，
    /// 并启用该直连路由的请求契约（只支持 `responses` wire + 官方 api base）。
    ChatgptPlan,
    /// 用本机 DeepSeek 账号额度：每回合向 `qaqh-dsaccount` 管理器取账号 token，
    /// 以 **`x-dsh-auth-token` 裸头**（无 `Bearer` 前缀）发往官方推理端点
    /// （只支持 `openai` wire + `https://api.deepseek.com`）。
    ///
    /// 与 [`CredentialSource::ChatgptPlan`] 是两条**互不回退**的路：没登录就报错，
    /// 绝不退回用 API key——那会让用户在不知情的情况下花自己的钱。
    DeepseekAccount,
}

impl Default for EndpointCompat {
    fn default() -> Self {
        Self {
            path: None,
            thinking_mode: ThinkingParamMode::default(),
            cache_field: CacheTokenField::default(),
            cache_usage_semantics: CacheUsageSemantics::default(),
            include_stream_usage: false,
            official_web_tools: false,
            supports_thinking: true,
            thinking_budget_large: false,
            supports_reasoning_effort: true,
            effort_allowlist: None,
            tool_call_content_null: false,
            supports_reasoning_content: true,
            require_provider_parameters: false,
            do_sample: None,
            user_id_mode: None,
            send_request_context: false,
            responses_web_search: true,
            responses_echo_web_search_call: true,
            responses_send_include: true,
            responses_effort_max: "high".into(),
            responses_supports_user: true,
            responses_search_function_alias: None,
            responses_echo_reasoning_content: true,
            responses_siwc_plan: false,
            retry: None,
        }
    }
}

impl EndpointCompat {
    /// The path to request: the override when set, else the wire's canonical suffix.
    ///
    /// The Gemini suffix carries a `{model}` placeholder that the transport
    /// substitutes; streaming switches it to the `:streamGenerateContent`
    /// sibling (`Wire::Gemini` doc).
    pub fn path_for(&self, wire: Wire) -> String {
        self.path.clone().unwrap_or_else(|| match wire {
            Wire::Chat => "/chat/completions".to_string(),
            Wire::Responses => "/responses".to_string(),
            Wire::Messages => "/v1/messages".to_string(),
            Wire::Gemini => "/models/{model}:generateContent".to_string(),
        })
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
            (Wire::Chat, "chat"),
            (Wire::Responses, "responses"),
            (Wire::Messages, "messages"),
            (Wire::Gemini, "gemini"),
        ] {
            assert_eq!(wire.as_str(), name);
            assert_eq!(Wire::parse(name), Some(wire));
            let row: Row = toml::from_str(&format!("wire = \"{name}\"")).expect(name);
            assert_eq!(row.wire, wire);
        }
        // 旧拼写经 serde alias / parse 继续可用：已发布配置零迁移。
        for (legacy, wire) in [
            ("openai", Wire::Chat),
            ("openai-compatible", Wire::Chat),
            ("chat_completions", Wire::Chat),
            ("anthropic", Wire::Messages),
            ("claude", Wire::Messages),
        ] {
            assert_eq!(Wire::parse(legacy), Some(wire), "parse: {legacy}");
            let row: Row = toml::from_str(&format!("wire = \"{legacy}\"")).expect(legacy);
            assert_eq!(row.wire, wire, "serde: {legacy}");
        }
        assert_eq!(Wire::parse("  OpenAI-Compatible "), Some(Wire::Chat));
        assert_eq!(Wire::parse("google"), Some(Wire::Gemini));
        assert_eq!(Wire::parse("unknown"), None);
        assert_eq!(Wire::default(), Wire::Chat);
    }

    #[test]
    fn compat_needs_no_declaration_to_use_wire_defaults() {
        let compat: EndpointCompat = toml::from_str("").expect("empty table");
        assert!(compat.supports_thinking);
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
responses_effort_max = "max"
[retry]
max_retries = 3
idle_timeout_secs = 60
"#;
        let compat: EndpointCompat = toml::from_str(source).expect("parse");
        assert_eq!(compat.path.as_deref(), Some("/api/v1/messages"));
        assert_eq!(compat.thinking_mode, ThinkingParamMode::MiniMaxAdaptive);
        assert_eq!(compat.cache_field, CacheTokenField::UsageCachedTokens);

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
        assert_eq!(
            EndpointCompat::default().path_for(Wire::Gemini),
            "/models/{model}:generateContent"
        );
        let compat = EndpointCompat {
            path: Some("/v1/inner/responses".into()),
            ..Default::default()
        };
        assert_eq!(compat.path_for(Wire::Chat), "/v1/inner/responses");
    }
}
