//! Shared gate types — provider config and unified stream events.

use deepx_types::Message;
use deepx_types::{CacheTokenField, ThinkingParamMode};

#[derive(Debug, Clone, PartialEq)]
pub enum ProviderKind {
    OpenAi,
}

impl ProviderKind {
    pub fn from_str(_s: &str) -> Self {
        Self::OpenAi
    }
}

#[derive(Debug, Clone)]
pub struct ProviderConfig {
    pub kind: ProviderKind,
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub user_id_mode: Option<deepx_types::UserSendMode>,

    // ── Multi-provider adaptation fields ──
    pub chat_path: Option<String>,
    pub thinking_mode: ThinkingParamMode,
    pub cache_field: CacheTokenField,
    pub supports_thinking: bool,
    /// When Some, explicitly sets `do_sample` in the request body. Used by GLM for
    /// deterministic codegen (do_sample=false). None means don't send the field.
    pub do_sample: Option<bool>,

    // ── Stateful proxy mode (e.g. DeepSeek Web CDP proxy) ──
    /// When true, only send incremental messages (not full history).
    /// The proxy remembers conversation context.
    pub stateful: bool,
    /// Whether the endpoint accepts a system message after history/tools.
    pub supports_tail_system: bool,
}

impl ProviderConfig {
    pub fn openai(
        base_url: &str,
        api_key: &str,
        model: &str,
        user_id_mode: Option<deepx_types::UserSendMode>,
        chat_path: Option<String>,
        thinking_mode: ThinkingParamMode,
        cache_field: CacheTokenField,
        supports_thinking: bool,
        do_sample: Option<bool>,
    ) -> Self {
        Self {
            kind: ProviderKind::OpenAi,
            base_url: base_url.to_string(),
            api_key: api_key.to_string(),
            model: model.to_string(),
            user_id_mode,
            chat_path,
            thinking_mode,
            cache_field,
            supports_thinking,
            do_sample,
            stateful: false,
            supports_tail_system: true,
        }
    }

    /// Configure this provider for stateful mode (web proxy).
    pub fn with_stateful(mut self, stateful: bool) -> Self {
        self.stateful = stateful;
        self
    }

    pub fn with_tail_system_support(mut self, supported: bool) -> Self {
        self.supports_tail_system = supported;
        self
    }
}

// ── StreamEvent ──

#[derive(Debug, Clone)]
pub enum StreamEvent {
    ContentDelta(String),
    ReasoningDelta(String),
    ToolCallProgress {
        index: usize,
        id: String,
        name: String,
        args_so_far: String,
    },
    Done {
        raw_message: Message,
        usage: Option<deepx_types::UsageInfo>,
        stop_reason: Option<String>,
    },
    /// Emitted whenever the API reports updated usage mid-stream (cache hits may appear in any chunk).
    UsageUpdate(deepx_types::UsageInfo),
    Error(String),
    /// Emitted when the gate is retrying after a retryable error.
    Retrying {
        attempt: u32,
        max_retries: u32,
        delay_secs: u64,
        error: String,
    },
}
