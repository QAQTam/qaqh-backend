use deepx_types::{
    ConfigStore, PersistentConfig, PersistentMultimodalConfig,
    PersistentSubagentConfig, PersistentWorkspaceConfig,
};
use std::collections::HashMap; // still used by profiles

/// Subagent default configuration.
///
/// These are defaults applied when spawning sub-agents. Individual
/// `spawn_subagent` tool calls can override these on a per-instance basis.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SubagentConfig {
    /// Override model. Empty = inherit from parent agent config.
    #[serde(default)]
    pub model: String,
    /// Override API base URL. Empty = inherit.
    #[serde(default)]
    pub base_url: String,
    /// Override API key. Empty = inherit.
    #[serde(default)]
    pub api_key: String,
    /// Max output tokens for subagent responses. Default: 4096.
    #[serde(default = "default_subagent_max_tokens")]
    pub max_tokens: u32,
    /// Maximum lifetime in seconds before the subagent is killed. Default: 120.
    #[serde(default = "default_subagent_timeout")]
    pub timeout_secs: u64,
    /// Default tool allowlist. Empty = all tools available.
    #[serde(default)]
    pub default_tools: Vec<String>,
}

fn default_subagent_max_tokens() -> u32 {
    4096
}
fn default_subagent_timeout() -> u64 {
    120
}

impl Default for SubagentConfig {
    fn default() -> Self {
        Self {
            model: String::new(),
            base_url: String::new(),
            api_key: String::new(),
            max_tokens: 4096,
            timeout_secs: 120,
            default_tools: vec!["file".into(), "exec".into()],
        }
    }
}

/// Multimodal (vision) LLM configuration for image understanding.
///
/// Separate from the main LLM provider so users can use a vision-capable
/// model (e.g. MiMo) for image analysis while keeping their primary text
/// provider (e.g. DeepSeek) for general conversation and tool use.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MultimodalConfig {
    /// Whether multimodal image understanding is enabled.
    #[serde(default)]
    pub enabled: bool,
    /// Provider type: "mimo", "ollama", "openai_compat", "lmstudio".
    /// Determines which backend adapter is used.
    #[serde(default = "default_multimodal_provider_type")]
    pub provider_type: String,
    /// Provider ID for multimodal (e.g. "mimo").
    #[serde(default)]
    pub provider_id: String,
    /// API key for multimodal provider. Empty = use main API key.
    #[serde(default)]
    pub api_key: String,
    /// Base URL override for multimodal. Empty = use provider default.
    #[serde(default)]
    pub base_url: String,
    /// Model name for multimodal (e.g. "mimo-v2.5").
    #[serde(default = "default_multimodal_model")]
    pub model: String,
    /// Max output tokens for multimodal requests.
    #[serde(default = "default_multimodal_max_tokens")]
    pub max_tokens: u32,
}

fn default_multimodal_provider_type() -> String { "mimo".into() }
fn default_multimodal_model() -> String { "mimo-v2.5".into() }
fn default_multimodal_max_tokens() -> u32 { 4096 }

impl Default for MultimodalConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider_type: "mimo".into(),
            provider_id: "mimo".into(),
            api_key: String::new(),
            base_url: String::new(),
            model: "mimo-v2.5".into(),
            max_tokens: 4096,
        }
    }
}

/// RAG（检索增强生成）配置
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RagConfig {
    /// 是否启用 RAG（false 时不加载向量引擎）
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// HuggingFace 模型 ID
    #[serde(default = "default_rag_model")]
    pub model: String,
    /// 嵌入向量维度（512 = bge-small, 768 = bge-base）
    #[serde(default = "default_embed_dim")]
    pub embed_dim: usize,
    /// 数据存储目录（None = 自动选择 ~/.deepx/vector/）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store_dir: Option<String>,
    /// 技能语义检索 top-K
    #[serde(default = "default_skill_top_k")]
    pub skill_top_k: usize,
    /// 记忆检索 top-K
    #[serde(default = "default_memory_top_k")]
    pub memory_top_k: usize,
    /// 本地模型目录（设置后跳过 HF Hub 下载）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_model: Option<String>,
}

fn default_true() -> bool { true }
fn default_rag_model() -> String { "BAAI/bge-small-zh-v1.5".into() }
fn default_embed_dim() -> usize { 512 }
fn default_skill_top_k() -> usize { 5 }
fn default_memory_top_k() -> usize { 3 }

impl Default for RagConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            model: "BAAI/bge-small-zh-v1.5".into(),
            embed_dim: 512,
            store_dir: None,
            skill_top_k: 5,
            memory_top_k: 3,
            local_model: None,
        }
    }
}

/// Runtime agent configuration built from PersistentConfig + registry.
///
/// This is the fully-resolved config used by the agent at runtime. It combines
/// user settings from config.toml with provider registry defaults and profile
/// overrides. All fields are concrete (no Option wrapping).
#[derive(Debug, Clone)]
pub struct Config {
    /// API key for the selected provider.
    pub api_key: String,
    /// Base URL for API requests (from provider registry).
    pub base_url: String,
    /// Active model identifier.
    pub model: String,
    /// Max output tokens per turn.
    pub max_tokens: u32,
    /// Maximum context window size in tokens.
    pub context_limit: u32,
    /// Selected provider ID (e.g. "deepseek", "qwen").
    pub provider_id: String,
    /// Selected endpoint within the provider (e.g. "openai").
    pub endpoint: String,
    /// Reasoning effort: "high", "max", or empty.
    pub reasoning_effort: String,
    /// Named profiles for quick config switching.
    pub profiles: HashMap<String, deepx_types::ProfileConfig>,
    /// Currently active profile name.
    pub active_profile: String,
    /// UI language preference.
    pub lang: Option<String>,
    /// UI font family（WinUI 壳全局字体；空 = 跟随系统默认）。
    pub font_family: String,
    /// Default configuration for sub-agent spawning.
    pub subagent: SubagentConfig,
    /// Whether the content filter is active.
    pub compliance_enabled: bool,
    /// Additional banned keywords for the content filter.
    pub compliance_extra_keywords: Vec<String>,
    /// Whitelisted patterns exempt from content filtering.
    pub compliance_allowlist: Vec<String>,
    /// Multimodal (vision) LLM configuration for image understanding.
    pub multimodal: MultimodalConfig,
    /// RAG 向量引擎配置（embedding / 语义搜索 / 跨会话记忆）
    pub rag: RagConfig,
    /// Agent permission level:
    /// 1 = MaxLockdown, 2 = ReadFree, 3 = WorkspaceFree, 4 = Unrestricted.
    pub permission_level: u8,
    /// Path to a HuggingFace tokenizer.json. `None` = use heuristic fallback.
    pub tokenizer_path: Option<String>,
    /// Auto-compact threshold: fraction of context_limit (0.0-1.0).
    /// When total tokens exceed `context_limit * threshold`, compact is
    /// triggered before the next user message is processed. 0.0 disables.
    /// Default: 0.75 (compact at 75% capacity).
    pub auto_compact_threshold: f64,
    /// 工具套件运行环境："local"（默认）| "wsl"（仅 Windows）。
    pub workspace: WorkspaceConfig,
}

/// 工具套件运行环境（daemon 据此拉起 deepx-workspace serve）。
#[derive(Debug, Clone)]
pub struct WorkspaceConfig {
    pub mode: String,
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            mode: "local".into(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        let (provider_id, endpoint) = crate::registry::first_provider_endpoint();
        let base_url = crate::registry::base_url_for(&provider_id, &endpoint);
        let model = crate::registry::default_model_for(&provider_id, &endpoint);

        let mut profiles = HashMap::new();
        profiles.insert(
            "default".into(),
            deepx_types::ProfileConfig {
                model: model.clone(),
                max_tokens: 16384,
                effort: Some("high".into()),
                context_limit: 1_000_000,
                base_url: base_url.clone(),
                endpoint: None,
            },
        );
        Self {
            api_key: String::new(),
            base_url,
            model,
            max_tokens: 16384,
            context_limit: 1_000_000,
            provider_id,
            endpoint,
            reasoning_effort: "high".into(),
            profiles,
            active_profile: "default".into(),
            lang: None,
            font_family: String::new(),
            subagent: SubagentConfig::default(),
            compliance_enabled: true,
            compliance_extra_keywords: Vec::new(),
            compliance_allowlist: Vec::new(),
            multimodal: MultimodalConfig::default(),
            rag: RagConfig::default(),
            permission_level: 4, // Unrestricted — backward compat
            tokenizer_path: None,
            auto_compact_threshold: 0.75,
            workspace: WorkspaceConfig::default(),
        }
    }
}

impl Config {
    /// Load config from disk (TOML primary store).
    pub fn load() -> Result<Self, String> {
        let store = ConfigStore::default_location();
        Self::load_from_paths(store)
    }

    /// Load configuration from a TOML primary store.
    fn load_from_paths(store: ConfigStore) -> Result<Self, String> {
        let mut cfg = Self::default();

        let pc = store.load();

        if let Some(pc) = pc {
            // ── Backward compat: migrate old provider_id → new (provider_id, endpoint) ──
            let raw_pid = pc.provider_id.unwrap_or_default();
            let (provider_id, endpoint) = if raw_pid.is_empty() {
                crate::registry::first_provider_endpoint()
            } else {
                crate::registry::migrate_provider_id(&raw_pid)
            };
            cfg.provider_id = provider_id;
            // New endpoint field takes priority over backward-compat migration
            cfg.endpoint = pc.endpoint.filter(|e| !e.is_empty()).unwrap_or(endpoint);

            // ── Resolve base_url from endpoint (user override takes priority) ──
            let endpoint_base_url = crate::registry::base_url_for(&cfg.provider_id, &cfg.endpoint);
            if !endpoint_base_url.is_empty() {
                cfg.base_url = endpoint_base_url.clone();
            }

            if let Some(profiles) = pc.profiles {
                cfg.profiles = profiles;
            }
            if let Some(ref active) = pc.active_profile {
                cfg.active_profile = active.clone();
                if let Some(profile) = cfg.profiles.get(active) {
                    cfg.model = profile.model.clone();
                    cfg.max_tokens = profile.max_tokens;
                    cfg.reasoning_effort = profile.effort.clone().unwrap_or_else(|| "high".into());
                    cfg.context_limit = profile.context_limit;
                    cfg.base_url = profile.base_url.clone();
                    if let Some(ref ep) = profile.endpoint
                        && !ep.is_empty()
                    {
                        cfg.endpoint = ep.clone();
                        let ep_burl = crate::registry::base_url_for(&cfg.provider_id, ep);
                        if !ep_burl.is_empty() && ep_burl != cfg.base_url {
                            cfg.base_url = ep_burl;
                        }
                    }
                }
            }
            if let Some(k) = pc.api_key
                && !k.is_empty()
            {
                cfg.api_key = k;
            }
            if let Some(m) = pc.model
                && !m.is_empty()
            {
                cfg.model = m;
            }
            // User base_url override: only apply if differs from all known endpoint defaults
            if let Some(ref u) = pc.base_url
                && !u.is_empty()
            {
                let is_ep_default = crate::registry::all_providers()
                    .iter()
                    .flat_map(|p| &p.endpoints)
                    .any(|e| e.base_url == *u || e.models_url.as_deref() == Some(u.as_str()));
                if !is_ep_default {
                    cfg.base_url = u.clone();
                }
            }
            if let Some(mt) = pc.max_tokens {
                cfg.max_tokens = mt;
            }
            if let Some(cl) = pc.context_limit {
                cfg.context_limit = cl;
            }
            if let Some(ref l) = pc.lang
                && !l.is_empty()
            {
                cfg.lang = Some(l.clone());
            }
            // ── UI 字体（空 = 跟随系统默认）──
            if let Some(ref f) = pc.font_family
                && !f.is_empty()
            {
                cfg.font_family = f.clone();
            }
            // ── Subagent defaults ──
            if let Some(ref s) = pc.subagent {
                if let Some(ref m) = s.model
                    && !m.is_empty()
                {
                    cfg.subagent.model = m.clone();
                }
                if let Some(ref u) = s.base_url
                    && !u.is_empty()
                {
                    cfg.subagent.base_url = u.clone();
                }
                if let Some(ref k) = s.api_key
                    && !k.is_empty()
                {
                    cfg.subagent.api_key = k.clone();
                }
                if let Some(mt) = s.max_tokens {
                    cfg.subagent.max_tokens = mt;
                }
                if let Some(ts) = s.timeout_secs {
                    cfg.subagent.timeout_secs = ts;
                }
                if let Some(ref tools) = s.default_tools {
                    cfg.subagent.default_tools = tools.clone();
                }
            }

            // ── Compliance ──
            if let Some(enabled) = pc.compliance_enabled {
                cfg.compliance_enabled = enabled;
            }
            if let Some(ref keywords) = pc.compliance_extra_keywords {
                cfg.compliance_extra_keywords = keywords.clone();
            }
            if let Some(ref allowlist) = pc.compliance_allowlist {
                cfg.compliance_allowlist = allowlist.clone();
            }

            // ── Multimodal (vision) ──
            if let Some(ref mm) = pc.multimodal {
                if let Some(enabled) = mm.enabled {
                    cfg.multimodal.enabled = enabled;
                }
                if let Some(ref pt) = mm.provider_type {
                    cfg.multimodal.provider_type = pt.clone();
                }
                if let Some(ref pid) = mm.provider_id {
                    cfg.multimodal.provider_id = pid.clone();
                }
                if let Some(ref key) = mm.api_key {
                    cfg.multimodal.api_key = key.clone();
                }
                if let Some(ref url) = mm.base_url {
                    cfg.multimodal.base_url = url.clone();
                }
                if let Some(ref model) = mm.model {
                    cfg.multimodal.model = model.clone();
                }
                if let Some(mt) = mm.max_tokens {
                    cfg.multimodal.max_tokens = mt;
                }
            }

            // ── Permission ──
            if let Some(pl) = pc.permission_level {
                cfg.permission_level = pl;
            }

            // ── Tokenizer ──
            if let Some(ref tp) = pc.tokenizer_path {
                cfg.tokenizer_path = Some(tp.clone());
            }

            // ── Auto-compact ──
            if let Some(act) = pc.auto_compact_threshold {
                cfg.auto_compact_threshold = act;
            }

            // ── 工具套件运行环境 ──
            if let Some(ref ws) = pc.workspace {
                if let Some(ref mode) = ws.mode {
                    cfg.workspace.mode = mode.clone();
                }
            }
        }

        if !cfg.profiles.contains_key("default") {
            cfg.profiles.insert(
                "default".into(),
                deepx_types::ProfileConfig {
                    model: cfg.model.clone(),
                    max_tokens: cfg.max_tokens,
                    effort: Some(cfg.reasoning_effort.clone()),
                    context_limit: cfg.context_limit,
                    base_url: cfg.base_url.clone(),
                    endpoint: Some(cfg.endpoint.clone()),
                },
            );
        }

        // Initialize tokenizer if configured
        if let Some(ref path) = cfg.tokenizer_path {
            let _ = deepx_types::token::init_tokenizer(path);
        }

        Ok(cfg)
    }

    pub fn save(&self) -> Result<(), String> {
        let store = ConfigStore::default_location();
        let mut profiles = self.profiles.clone();
        profiles.insert(
            self.active_profile.clone(),
            deepx_types::ProfileConfig {
                model: self.model.clone(),
                max_tokens: self.max_tokens,
                effort: Some(self.reasoning_effort.clone()),
                context_limit: self.context_limit,
                base_url: self.base_url.clone(),
                endpoint: Some(self.endpoint.clone()),
            },
        );
        let pc = PersistentConfig {
            api_key: if self.api_key.is_empty() {
                None
            } else {
                Some(self.api_key.clone())
            },
            model: Some(self.model.clone()),
            base_url: Some(self.base_url.clone()),
            max_tokens: Some(self.max_tokens),
            context_limit: Some(self.context_limit),
            provider_id: Some(self.provider_id.clone()),
            endpoint: Some(self.endpoint.clone()),
            reasoning_effort: Some(self.reasoning_effort.clone()),
            profiles: Some(profiles),
            active_profile: Some(self.active_profile.clone()),
            lang: self.lang.clone(),
            font_family: if self.font_family.is_empty() {
                None
            } else {
                Some(self.font_family.clone())
            },
            subagent: Some(PersistentSubagentConfig {
                model: if self.subagent.model.is_empty() {
                    None
                } else {
                    Some(self.subagent.model.clone())
                },
                base_url: if self.subagent.base_url.is_empty() {
                    None
                } else {
                    Some(self.subagent.base_url.clone())
                },
                api_key: if self.subagent.api_key.is_empty() {
                    None
                } else {
                    Some(self.subagent.api_key.clone())
                },
                max_tokens: Some(self.subagent.max_tokens),
                timeout_secs: Some(self.subagent.timeout_secs),
                default_tools: if self.subagent.default_tools.is_empty() {
                    None
                } else {
                    Some(self.subagent.default_tools.clone())
                },
            }),
            compliance_enabled: Some(self.compliance_enabled),
            compliance_extra_keywords: if self.compliance_extra_keywords.is_empty() {
                None
            } else {
                Some(self.compliance_extra_keywords.clone())
            },
            compliance_allowlist: if self.compliance_allowlist.is_empty() {
                None
            } else {
                Some(self.compliance_allowlist.clone())
            },
            multimodal: Some(PersistentMultimodalConfig {
                enabled: Some(self.multimodal.enabled),
                provider_type: if self.multimodal.provider_type.is_empty() {
                    None
                } else {
                    Some(self.multimodal.provider_type.clone())
                },
                provider_id: if self.multimodal.provider_id.is_empty() {
                    None
                } else {
                    Some(self.multimodal.provider_id.clone())
                },
                api_key: if self.multimodal.api_key.is_empty() {
                    None
                } else {
                    Some(self.multimodal.api_key.clone())
                },
                base_url: if self.multimodal.base_url.is_empty() {
                    None
                } else {
                    Some(self.multimodal.base_url.clone())
                },
                model: Some(self.multimodal.model.clone()),
                max_tokens: Some(self.multimodal.max_tokens),
            }),
            permission_level: Some(self.permission_level),
            tokenizer_path: self.tokenizer_path.clone(),
            auto_compact_threshold: Some(self.auto_compact_threshold),
            workspace: Some(PersistentWorkspaceConfig {
                mode: Some(self.workspace.mode.clone()),
            }),
        };
        log::info!(
            "[Config::save] writing to {}",
            deepx_types::platform::config_path().display()
        );
        if !store.save(&pc) {
            return Err(format!(
                "Failed to save config to {}",
                deepx_types::platform::config_path().display()
            ));
        }

        Ok(())
    }

    pub fn apply_profile(&mut self, name: &str) -> Option<String> {
        let profile = self.profiles.get(name)?.clone();
        self.model = profile.model;
        self.max_tokens = profile.max_tokens;
        self.reasoning_effort = profile.effort.unwrap_or_else(|| "high".into());
        self.context_limit = profile.context_limit;
        self.base_url = profile.base_url;
        if let Some(ref ep) = profile.endpoint {
            self.endpoint = ep.clone();
            let ep_burl = crate::registry::base_url_for(&self.provider_id, ep);
            if !ep_burl.is_empty() && ep_burl != self.base_url {
                self.base_url = ep_burl;
            }
        }
        self.active_profile = name.to_string();
        let _ = self.save();
        Some(name.to_string())
    }

    pub fn save_profile(&mut self, name: &str) {
        self.profiles.insert(
            name.to_string(),
            deepx_types::ProfileConfig {
                model: self.model.clone(),
                max_tokens: self.max_tokens,
                effort: Some(self.reasoning_effort.clone()),
                context_limit: self.context_limit,
                base_url: self.base_url.clone(),
                endpoint: Some(self.endpoint.clone()),
            },
        );
        self.active_profile = name.to_string();
        let _ = self.save();
    }

    pub fn delete_profile(&mut self, name: &str) -> bool {
        if name == "default" {
            return false;
        }
        if self.profiles.remove(name).is_some() {
            let _ = self.save();
            true
        } else {
            false
        }
    }

    pub fn is_ready(&self) -> bool {
        !self.api_key.is_empty()
    }

    /// Protocol derived from (provider_id, endpoint) in the registry.
    pub fn protocol(&self) -> String {
        crate::registry::protocol_for(&self.provider_id, &self.endpoint)
    }






}
