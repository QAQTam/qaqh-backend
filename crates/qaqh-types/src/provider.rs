//! Provider / endpoint model.
//!
//! Provider = the company/service (e.g. DeepSeek).
//! Endpoint = a concrete protocol endpoint for that provider (e.g. OpenAI-compatible, Anthropic-native).
//!
//! The user picks (provider_id, endpoint_id) and the rest auto-fills:
//!   protocol + base_url from EndpointSpec
//!   models from GET /models (with fallback to default_model)
//!
//! TOML schema（T9）：`EndpointSpec`/`ProviderSpec` 即 assets/providers.toml 的
//! 序列化形态；用户覆盖面用 `ProviderPatch`/`EndpointPatch`（全 Option），
//! 缺省 = 不覆盖 baseline 字段（bool 用 Option 区分"未设置"与 false）。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
/// Where the user identifier is placed in the API request.
pub enum UserSendMode {
    /// User ID is sent in the JSON request body.
    #[default]
    Body,
}

/// How the reasoning/thinking parameter is sent to the model.
///
/// Different providers expect different formats for the thinking/reasoning
/// toggle parameter in the request body.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub enum ThinkingParamMode {
    /// Standard OpenAI format: `{"type": "enabled"|"disabled"}` at top-level body.
    /// Used by: DeepSeek, GLM, Kimi, MiMo, Doubao, OpenAI.
    #[default]
    OpenAi,
    /// Boolean `enable_thinking: true/false` at top-level body. Used by: Qwen.
    QwenEnableThinking,
    /// `{"type": "adaptive"}` + `reasoning_split: true` at top-level. Used by: MiniMax.
    MiniMaxAdaptive,
}

/// Where the cache token count is located in the usage response.
///
/// Different providers return cache hit/miss info in different JSON paths.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub enum CacheTokenField {
    /// Top-level: `usage.prompt_cache_hit_tokens` + `usage.prompt_cache_miss_tokens`.
    /// Used by: DeepSeek.
    #[default]
    PromptCacheHitTokens,
    /// Nested: `usage.prompt_tokens_details.cached_tokens`. Used by: Qwen, GLM.
    PromptDetailsCached,
    /// Top-level single value: `usage.cached_tokens`. Used by: Kimi.
    UsageCachedTokens,
    /// No cache information returned. Used by: MiMo, MiniMax.
    None,
}

/// Per-endpoint retry policy (T9). Absent → gate built-in defaults
/// (5 attempts / 1s base / 30s cap / 300s idle watchdog).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RetrySpec {
    /// 最大尝试次数（含首次；与 gate 既有 MAX_RETRIES 语义一致）。
    #[serde(default)]
    pub max_retries: u32,
    /// 首次重试基础退避秒数（指数翻倍 + ±10% jitter，封顶 max_delay_secs）。
    #[serde(default)]
    pub base_delay_secs: u64,
    /// 单次等待上限秒数。
    #[serde(default)]
    pub max_delay_secs: u64,
    /// 流空闲看门狗秒数（连续无新字节视为半开连接）。
    #[serde(default)]
    pub idle_timeout_secs: u64,
}

/// Configuration for a single API endpoint (protocol variant) of a provider.
///
/// Each provider may expose multiple endpoints (e.g. OpenAI-compatible and
/// Anthropic-native). The user selects a (provider, endpoint) pair, and
/// the protocol, base URL, and model list are auto-filled from this spec.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EndpointSpec {
    /// Internal endpoint identifier (e.g. "openai", "anthropic").
    pub id: String,
    /// Human-readable label shown in settings UI (e.g. "OpenAI-compatible").
    pub display: String,
    /// Protocol name: "openai" or "anthropic". Determines the HTTP API format.
    pub protocol: String,
    /// Base URL for API requests without trailing path (e.g. "https://api.deepseek.com").
    pub base_url: String,
    /// Fallback model when no model is selected by the user.
    pub default_model: String,
    /// Cached list of available model names fetched from the API.
    #[serde(default)]
    pub models: Vec<String>,
    /// URL for the `GET /models` endpoint. `None` = use `base_url/models`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models_url: Option<String>,
    /// Where to send the user identifier parameter. `None` = not sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id_mode: Option<UserSendMode>,

    // ── Multi-provider adaptation fields ──
    /// Chat completions path override (appended to `base_url`).
    /// `None` = default `/chat/completions`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_path: Option<String>,
    /// Responses API path override (appended to `base_url`).
    /// `None` = default `/responses`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub responses_path: Option<String>,
    /// Anthropic Messages API path override (appended to `base_url`).
    /// `None` = default `/v1/messages` (`/api/anthropic/v1/messages` for ZCode).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anthropic_path: Option<String>,
    /// Balance query path override (appended to `base_url`).
    /// `None` = default `/user/balance`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub balance_path: Option<String>,
    /// How the thinking/reasoning parameter is formatted for this endpoint.
    /// Default: `OpenAi`.
    #[serde(default)]
    pub thinking_mode: ThinkingParamMode,
    /// Which field in the usage response carries the cache token count.
    /// Default: `PromptCacheHitTokens`.
    #[serde(default)]
    pub cache_field: CacheTokenField,
    /// Request an additional usage chunk before the streaming terminator.
    /// Kept per endpoint because not every OpenAI-compatible provider accepts
    /// `stream_options.include_usage`.
    #[serde(default)]
    pub include_stream_usage: bool,
    /// Whether this endpoint has a balance/info endpoint. Default: true.
    #[serde(default = "default_true")]
    pub has_balance: bool,
    /// Whether this endpoint supports the thinking/reasoning parameter. Default: true.
    #[serde(default = "default_true")]
    pub supports_thinking: bool,
    /// 大上下文模型的 thinking 预算档位（如 GLM-5.3 的 1M 窗口）：
    /// low/medium/high/xhigh/max ↔ 16k/32k/64k/80k/96k（默认档 1k-16k）。
    /// 仅供 gate 的 Anthropic thinking 分支消费——新 provider 只加配置，
    /// gate 不写 provider 特判。
    #[serde(default)]
    pub thinking_budget_large: bool,
    /// Whether this endpoint accepts OpenAI's `reasoning_effort` parameter.
    /// Kept separately because an endpoint may support neither vendor-specific
    /// thinking toggles nor OpenAI reasoning effort.
    #[serde(default = "default_true")]
    pub supports_reasoning_effort: bool,
    /// Sparse allowlist of accepted `reasoning_effort` values (router models
    /// often expose a non-contiguous set, e.g. OpenRouter ox-alpha only takes
    /// max/high/low). When set, the gate clamps the requested effort to the
    /// nearest allowed value on the global effort ladder instead of passing it
    /// through verbatim — off-domain values are otherwise silently ignored or
    /// rejected by routers. `None` = no clamping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort_allowlist: Option<Vec<String>>,
    /// Whether assistant history entries that contain tool calls must include
    /// an explicit `content: null`. Some OpenAI-compatible upstreams reject a
    /// missing content member even though the OpenAI schema permits null.
    #[serde(default)]
    pub tool_call_content_null: bool,
    /// Whether provider-specific `reasoning_content` may be sent back as
    /// assistant history. This is disabled for routers that target many
    /// different upstream schemas.
    #[serde(default = "default_true")]
    pub supports_reasoning_content: bool,
    /// Ask a router to select only upstreams that implement every request
    /// parameter. Used by OpenRouter tool calls to avoid lax fallback routing.
    #[serde(default)]
    pub require_provider_parameters: bool,
    /// Per-model vision allowlist for routers whose endpoints serve
    /// heterogeneous models (`supports_image_tool` alone is too coarse there).
    ///
    /// Semantics when `supports_image_tool` is true:
    /// - `None`  → every model on this endpoint accepts images (uniform
    ///   first-party endpoints like opencode-go);
    /// - `Some`  → only listed models accept images. Entries match exactly,
    ///   or by prefix when ending in `*` (e.g. `"google/gemini-*"`).
    ///   Matched case-insensitively against the active model id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_models: Option<Vec<String>>,
    /// When true, the gate sends only incremental messages instead of full conversation
    /// history. Used for stateful proxy endpoints (e.g. DeepSeek Web CDP proxy).
    #[serde(default)]
    pub stateful: bool,
    /// Experimental/Beta endpoint flag surfaced to the settings UI as a badge.
    /// Beta endpoints are additive: they never replace a stable endpoint.
    #[serde(default)]
    pub beta: bool,
    /// Explicitly set `do_sample` in the request body. GLM-5.2 benefits from `false` for
    /// deterministic code generation. `None` means the field is not sent (API default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub do_sample: Option<bool>,

    // ── Responses API adaptation fields ──
    /// Inject the built-in `web_search` tool so the model can search on its own
    /// (server-side execution). OpenAI / DeepSeek support it; some compatible
    /// endpoints ignore unknown tool types. Default: true.
    #[serde(default = "default_true")]
    pub responses_web_search: bool,
    /// Allow echoing `web_search_call` output items back verbatim in the next
    /// turn so the server restores its search results (stateless multi-turn).
    /// DeepSeek documents this; OpenAI stateless mode behaves the same.
    /// Default: true.
    #[serde(default = "default_true")]
    pub responses_echo_web_search_call: bool,
    /// Send `include: ["reasoning.encrypted_content"]`. OpenAI supports it;
    /// DeepSeek silently ignores it; a few strict compatible endpoints reject
    /// unknown members with 400. Default: true (OpenAI semantics).
    #[serde(default = "default_true")]
    pub responses_send_include: bool,
    /// Upper bound for `reasoning.effort`. OpenAI: `"high"`; DeepSeek extends
    /// the ladder to `"xhigh"` / `"max"`. Values above the bound are clamped.
    /// Default: "high".
    #[serde(default = "default_effort_max")]
    pub responses_effort_max: String,
    /// Send the `user` field (rate-limit & KVCache isolation per end user).
    /// Default: true.
    #[serde(default = "default_true")]
    pub responses_supports_user: bool,
    /// Provider-facing alias for QAQ-Harness's canonical `search` function tool.
    /// Some Responses-compatible providers reserve `search` while their
    /// built-in `web_search` tool is enabled. `None` keeps the canonical name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub responses_search_function_alias: Option<String>,
    /// Echo assistant `reasoning` items back verbatim in the next turn's
    /// input. Default: true — DeepSeek / MiMo reject tool-loop continuations
    /// without them (HTTP 400), Kimi K3 & k2.7-code require them for preserved
    /// thinking, and GLM / Qwen / MiniMax / OpenAI accept them silently.
    #[serde(default = "default_true")]
    pub responses_echo_reasoning_content: bool,
    /// Whether this endpoint accepts image parts in the conversation
    /// (vision input). Gates the `read_image` client tool: endpoints without
    /// it never see the tool in their tool list. Default: false.
    #[serde(default)]
    pub supports_image_tool: bool,
    /// Per-endpoint retry policy override (T9). `None` = gate built-in defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<RetrySpec>,
}

fn default_true() -> bool {
    true
}

fn default_effort_max() -> String {
    "high".into()
}

impl Default for EndpointSpec {
    fn default() -> Self {
        Self {
            id: String::new(),
            display: String::new(),
            protocol: "openai".into(),
            base_url: String::new(),
            default_model: String::new(),
            models: Vec::new(),
            models_url: None,
            user_id_mode: None,
            chat_path: None,
            responses_path: None,
            anthropic_path: None,
            balance_path: None,
            thinking_mode: ThinkingParamMode::default(),
            cache_field: CacheTokenField::default(),
            include_stream_usage: false,
            has_balance: true,
            supports_thinking: true,
            thinking_budget_large: false,
            supports_reasoning_effort: true,
            effort_allowlist: None,
            tool_call_content_null: false,
            supports_reasoning_content: true,
            require_provider_parameters: false,
            image_models: None,
            stateful: false,
            beta: false,
            do_sample: None,
            responses_web_search: true,
            responses_echo_web_search_call: true,
            responses_send_include: true,
            responses_effort_max: "high".into(),
            responses_supports_user: true,
            responses_search_function_alias: None,
            responses_echo_reasoning_content: true,
            supports_image_tool: false,
            retry: None,
        }
    }
}

/// Top-level provider definition (e.g. DeepSeek, Qwen, OpenAI).
///
/// A provider is a company or service that hosts LLM models. Each provider
/// may have one or more endpoints (protocol variants).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderSpec {
    /// Unique provider identifier (e.g. "deepseek", "qwen").
    pub id: String,
    /// Human-readable display name for settings UI.
    pub display: String,
    /// Available API endpoints for this provider.
    pub endpoints: Vec<EndpointSpec>,
}

// ── TOML 覆盖层（T9）：全 Option 稀疏补丁，缺省 = 不覆盖 baseline ──

/// Sparse endpoint override（用户 TOML 用）。bool 用 Option 区分"未设置"与
/// `false`，避免覆盖文件误伤 baseline 的 false 值。`None` 字段不参与合并。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EndpointPatch {
    pub display: Option<String>,
    pub protocol: Option<String>,
    pub base_url: Option<String>,
    pub default_model: Option<String>,
    pub models_url: Option<String>,
    pub chat_path: Option<String>,
    pub responses_path: Option<String>,
    pub anthropic_path: Option<String>,
    pub balance_path: Option<String>,
    pub thinking_mode: Option<ThinkingParamMode>,
    pub cache_field: Option<CacheTokenField>,
    pub include_stream_usage: Option<bool>,
    pub has_balance: Option<bool>,
    pub supports_thinking: Option<bool>,
    pub thinking_budget_large: Option<bool>,
    pub supports_reasoning_effort: Option<bool>,
    pub effort_allowlist: Option<Vec<String>>,
    pub tool_call_content_null: Option<bool>,
    pub supports_reasoning_content: Option<bool>,
    pub require_provider_parameters: Option<bool>,
    pub image_models: Option<Vec<String>>,
    pub stateful: Option<bool>,
    pub beta: Option<bool>,
    pub do_sample: Option<bool>,
    pub responses_web_search: Option<bool>,
    pub responses_echo_web_search_call: Option<bool>,
    pub responses_send_include: Option<bool>,
    pub responses_effort_max: Option<String>,
    pub responses_supports_user: Option<bool>,
    pub responses_search_function_alias: Option<String>,
    pub responses_echo_reasoning_content: Option<bool>,
    pub supports_image_tool: Option<bool>,
    pub retry: Option<RetrySpec>,
}

impl EndpointPatch {
    /// 把非 None 字段写入 `spec`（原地覆盖）。
    pub fn apply_to(&self, spec: &mut EndpointSpec) {
        if let Some(v) = &self.display {
            spec.display = v.clone();
        }
        if let Some(v) = &self.protocol {
            spec.protocol = v.clone();
        }
        if let Some(v) = &self.base_url {
            spec.base_url = v.clone();
        }
        if let Some(v) = &self.default_model {
            spec.default_model = v.clone();
        }
        if let Some(v) = &self.models_url {
            spec.models_url = Some(v.clone());
        }
        if let Some(v) = &self.chat_path {
            spec.chat_path = Some(v.clone());
        }
        if let Some(v) = &self.responses_path {
            spec.responses_path = Some(v.clone());
        }
        if let Some(v) = &self.anthropic_path {
            spec.anthropic_path = Some(v.clone());
        }
        if let Some(v) = &self.balance_path {
            spec.balance_path = Some(v.clone());
        }
        if let Some(v) = &self.thinking_mode {
            spec.thinking_mode = v.clone();
        }
        if let Some(v) = &self.cache_field {
            spec.cache_field = v.clone();
        }
        if let Some(v) = self.include_stream_usage {
            spec.include_stream_usage = v;
        }
        if let Some(v) = self.has_balance {
            spec.has_balance = v;
        }
        if let Some(v) = self.supports_thinking {
            spec.supports_thinking = v;
        }
        if let Some(v) = self.thinking_budget_large {
            spec.thinking_budget_large = v;
        }
        if let Some(v) = self.supports_reasoning_effort {
            spec.supports_reasoning_effort = v;
        }
        if let Some(v) = &self.effort_allowlist {
            spec.effort_allowlist = Some(v.clone());
        }
        if let Some(v) = self.tool_call_content_null {
            spec.tool_call_content_null = v;
        }
        if let Some(v) = self.supports_reasoning_content {
            spec.supports_reasoning_content = v;
        }
        if let Some(v) = self.require_provider_parameters {
            spec.require_provider_parameters = v;
        }
        if let Some(v) = &self.image_models {
            spec.image_models = Some(v.clone());
        }
        if let Some(v) = self.stateful {
            spec.stateful = v;
        }
        if let Some(v) = self.beta {
            spec.beta = v;
        }
        if let Some(v) = self.do_sample {
            spec.do_sample = Some(v);
        }
        if let Some(v) = self.responses_web_search {
            spec.responses_web_search = v;
        }
        if let Some(v) = self.responses_echo_web_search_call {
            spec.responses_echo_web_search_call = v;
        }
        if let Some(v) = self.responses_send_include {
            spec.responses_send_include = v;
        }
        if let Some(v) = &self.responses_effort_max {
            spec.responses_effort_max = v.clone();
        }
        if let Some(v) = self.responses_supports_user {
            spec.responses_supports_user = v;
        }
        if let Some(v) = &self.responses_search_function_alias {
            spec.responses_search_function_alias = Some(v.clone());
        }
        if let Some(v) = self.responses_echo_reasoning_content {
            spec.responses_echo_reasoning_content = v;
        }
        if let Some(v) = self.supports_image_tool {
            spec.supports_image_tool = v;
        }
        if let Some(v) = &self.retry {
            spec.retry = Some(v.clone());
        }
    }
}

/// Sparse provider override（用户 TOML 用）。`endpoints` 按 `id` 匹配覆盖；
/// `remove` 声明要隐藏的 endpoint id（本地不可用的端点）。`id` 定位要覆盖的
/// provider；未匹配到时在尾部新建。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub display: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub endpoints: Vec<EndpointPatchRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
}

/// 指向 endpoint 的 patch（id 定位 + 稀疏字段）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EndpointPatchRef {
    pub id: String,
    #[serde(flatten)]
    pub patch: EndpointPatch,
}

/// baseline TOML 文件形态（assets/providers.toml，全量 `ProviderSpec`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProvidersFile {
    #[serde(default)]
    pub providers: Vec<ProviderSpec>,
}

/// 用户覆盖 TOML 文件形态（providers.override.toml / config.toml [providers]，
/// 稀疏 `ProviderPatch`）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProvidersOverrideFile {
    #[serde(default)]
    pub providers: Vec<ProviderPatch>,
}
