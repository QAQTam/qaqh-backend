use deepx_types::{
    ConfigStore, PersistentConfig, PersistentDatabaseConfig, PersistentMultimodalConfig,
    PersistentSubagentConfig, PersistentWorkspaceConfig,
};
use std::collections::HashMap; // still used by profiles

#[cfg(feature = "turso-backend")]
#[derive(serde::Serialize, serde::Deserialize)]
struct ConfigMirrorOutbox {
    version: u32,
    config_json: String,
}

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

/// Database mirror configuration (Turso local SQLite database).
///
/// When enabled, session messages are dual-written to both JSONL and a
/// local SQLite database for fast querying from external tools.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DatabaseConfig {
    /// Whether the database mirror is enabled.
    #[serde(default)]
    pub enabled: bool,
    /// Path to the local Turso database file. `None` = default location.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            enabled: cfg!(feature = "turso-backend"),
            url: None,
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
    /// Default configuration for sub-agent spawning.
    pub subagent: SubagentConfig,
    /// Whether the content filter is active.
    pub compliance_enabled: bool,
    /// Additional banned keywords for the content filter.
    pub compliance_extra_keywords: Vec<String>,
    /// Whitelisted patterns exempt from content filtering.
    pub compliance_allowlist: Vec<String>,
    /// Local Turso SQLite database mirror configuration.
    pub database: DatabaseConfig,
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
            subagent: SubagentConfig::default(),
            compliance_enabled: true,
            compliance_extra_keywords: Vec::new(),
            compliance_allowlist: Vec::new(),
            database: DatabaseConfig::default(),
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
    /// Load config from disk (TOML, with optional Turso DB dual-read).
    ///
    /// # Loading order
    /// 1. Read config.toml (always)
    /// 2. If database.enabled, try reading config.db (SQLite mirror, may be newer)
    /// 3. Apply provider registry defaults for missing fields
    /// 4. Apply active profile overrides
    pub fn load() -> Result<Self, String> {
        let store = ConfigStore::default_location();
        let db_path = deepx_types::platform::data_dir().join("config.db");
        Self::load_from_paths(store, db_path)
    }

    /// Load configuration from a TOML primary store, with a Turso fallback.
    ///
    /// The TOML file is authoritative whenever present because `save()` writes
    /// it first. The database is a recovery mirror used when the TOML file is
    /// missing or unreadable.
    fn load_from_paths(store: ConfigStore, db_path: std::path::PathBuf) -> Result<Self, String> {
        let mut cfg = Self::default();

        // Step 1: always read TOML to get database.enabled flag (bootstrap)
        let pc_toml = store.load();

        // Step 2: if database mirror is enabled, try ConfigDb as a recovery fallback.
        let db_enabled = pc_toml
            .as_ref()
            .and_then(|pc| pc.database.as_ref())
            .and_then(|db| db.enabled)
            .unwrap_or(cfg!(feature = "turso-backend"));

        // A durable outbox is replayed before normal DB reads. TOML remains
        // the bootstrap authority; the outbox only completes its DB mirror.
        if db_enabled {
            let _ = Self::replay_outbox_at(&db_path);
        }

        // Clone TOML data before potential move
        let pc_toml_for_override = pc_toml.clone();

        let pc = if db_enabled {
            #[cfg(feature = "turso-backend")]
            {
                match Self::try_load_from_db_at(&db_path) {
                    Ok(Some(db_pc)) => {
                        if pc_toml.is_some() {
                            pc_toml
                        } else {
                            log::info!("[Config] restored from config.db");
                            Some(db_pc)
                        }
                    }
                    Ok(None) => {
                        // ConfigDb has no data yet (first boot after enabling)
                        pc_toml
                    }
                    Err(e) => {
                        log::warn!("[Config] config.db load failed: {e}, falling back to TOML");
                        pc_toml
                    }
                }
            }
            #[cfg(not(feature = "turso-backend"))]
            pc_toml
        } else {
            pc_toml
        };

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
            if let Some(ref l) = pc.lang
                && !l.is_empty()
            {
                cfg.lang = Some(l.clone());
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

            // ── Database (Turso mirror) ──
            if let Some(ref db) = pc.database {
                if let Some(enabled) = db.enabled {
                    cfg.database.enabled = enabled;
                }
                cfg.database.url = db.url.clone();
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

        // TOML is authoritative for database.enabled (prevents stale ConfigDb value)
        if let Some(ref pc_toml) = pc_toml_for_override {
            if let Some(ref db) = pc_toml.database {
                if let Some(enabled) = db.enabled {
                    cfg.database.enabled = enabled;
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
            database: Some(PersistentDatabaseConfig {
                enabled: Some(self.database.enabled),
                url: self.database.url.clone(),
            }),
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
        let json = serde_json::to_string(&pc).map_err(|e| format!("serialize config mirror: {e}"))?;
        let db_path = deepx_types::platform::data_dir().join("config.db");
        #[cfg(feature = "turso-backend")]
        if self.database.enabled {
            Self::write_outbox_at(&db_path, &json)?;
        }
        #[cfg(not(feature = "turso-backend"))]
        let _ = (&db_path, &json);
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

        // Dual-write: mirror to SQLite when database is enabled
        if self.database.enabled {
            #[cfg(feature = "turso-backend")]
            {
                Self::save_to_db(&json)
                    .map_err(|e| format!("config.toml was saved but config.db mirror failed: {e}"))?;
                Self::remove_outbox_at(&db_path)?;
            }
            #[cfg(not(feature = "turso-backend"))]
            let _ = ();
        }

        Ok(())
    }

    /// Try loading config from a config.db mirror. Returns Ok(None) if db is empty/unavailable.
    #[cfg(feature = "turso-backend")]
    fn try_load_from_db_at(db_path: &std::path::Path) -> Result<Option<PersistentConfig>, String> {
        if !db_path.exists() {
            return Ok(None);
        }
        let db = crate::config_db::ConfigDb::open(db_path)?;
        if let Err(e) = db.init_table() {
            log::warn!("[Config] config.db init failed: {e}");
            return Ok(None);
        }
        let json_str = match db.load_config() {
            Ok(Some(s)) => s,
            Ok(None) => return Ok(None),
            Err(e) => return Err(e),
        };
        let pc: PersistentConfig = serde_json::from_str(&json_str)
            .map_err(|e| format!("deserialize config from db: {e}"))?;
        Ok(Some(pc))
    }

    /// Write config JSON to config.db.
    #[cfg(feature = "turso-backend")]
    fn save_to_db(json: &str) -> Result<(), String> {
        let db_path = deepx_types::platform::data_dir().join("config.db");
        let db = crate::config_db::ConfigDb::open(&db_path)?;
        db.init_table()?;
        db.save_config(json)
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

    /// Whether per-session Turso SQLite mirroring is enabled.
    pub fn turso_enabled(&self) -> bool {
        self.database.enabled
    }

    /// Protocol derived from (provider_id, endpoint) in the registry.
    pub fn protocol(&self) -> String {
        crate::registry::protocol_for(&self.provider_id, &self.endpoint)
    }

    #[cfg(any(feature = "turso-backend", test))]
    fn outbox_path(db_path: &std::path::Path) -> std::path::PathBuf {
        db_path.with_file_name("config-mirror-outbox.json")
    }

    #[cfg(feature = "turso-backend")]
    fn write_outbox_at(db_path: &std::path::Path, json: &str) -> Result<(), String> {
        let path = Self::outbox_path(db_path);
        let tmp = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(&ConfigMirrorOutbox { version: 1, config_json: json.into() })
            .map_err(|e| format!("serialize config outbox: {e}"))?;
        std::fs::write(&tmp, bytes).map_err(|e| format!("write config outbox: {e}"))?;
        std::fs::rename(&tmp, &path).map_err(|e| format!("activate config outbox: {e}"))
    }

    #[cfg(feature = "turso-backend")]
    fn remove_outbox_at(db_path: &std::path::Path) -> Result<(), String> {
        let path = Self::outbox_path(db_path);
        if path.exists() { std::fs::remove_file(path).map_err(|e| format!("remove config outbox: {e}"))?; }
        Ok(())
    }

    fn replay_outbox_at(db_path: &std::path::Path) -> Result<(), String> {
        #[cfg(not(feature = "turso-backend"))]
        {
            let _ = db_path;
            return Ok(());
        }
        #[cfg(feature = "turso-backend")]
        {
            let path = Self::outbox_path(db_path);
            if !path.exists() { return Ok(()); }
            let outbox: ConfigMirrorOutbox = serde_json::from_slice(&std::fs::read(&path)
                .map_err(|e| format!("read config outbox: {e}"))?)
                .map_err(|e| format!("parse config outbox: {e}"))?;
            let db = crate::config_db::ConfigDb::open(db_path)?;
            db.init_table()?;
            db.save_config(&outbox.config_json)?;
            Self::remove_outbox_at(db_path)?;
            Ok(())
        }
    }
}

#[cfg(all(test, not(feature = "turso-backend")))]
mod file_only_tests {
    use super::Config;

    #[test]
    fn disabled_backend_ignores_existing_mirror_outbox() {
        let root = std::env::temp_dir().join(format!(
            "deepx-config-file-only-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).expect("create test directory");
        let db_path = root.join("config.db");
        std::fs::write(Config::outbox_path(&db_path), b"not-json")
            .expect("write stale outbox");

        Config::replay_outbox_at(&db_path).expect("disabled backend must ignore stale outbox");
        assert!(Config::outbox_path(&db_path).exists());

        std::fs::remove_dir_all(root).expect("remove test directory");
    }
}

#[cfg(all(test, feature = "turso-backend"))]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static TEST_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

    fn temp_dir() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "deepx-config-dual-store-{}-{}-{}",
            std::process::id(),
            TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ))
    }

    #[test]
    fn toml_remains_authoritative_when_database_snapshot_is_stale() {
        let root = temp_dir();
        std::fs::create_dir_all(&root).expect("create test directory");
        let toml_path = root.join("config.toml");
        let db_path = root.join("config.db");
        let store = ConfigStore::new(toml_path);

        let toml = PersistentConfig {
            api_key: Some("toml-new-key".into()),
            database: Some(PersistentDatabaseConfig {
                enabled: Some(true),
                url: None,
            }),
            ..Default::default()
        };
        assert!(store.save(&toml));

        let db = crate::config_db::ConfigDb::open(&db_path).expect("open database");
        db.init_table().expect("initialize database");
        db.save_config(
            &serde_json::to_string(&PersistentConfig {
                api_key: Some("stale-db-key".into()),
                database: Some(PersistentDatabaseConfig {
                    enabled: Some(true),
                    url: None,
                }),
                ..Default::default()
            })
            .expect("serialize database snapshot"),
        )
        .expect("write database snapshot");

        let cfg = Config::load_from_paths(store, db_path).expect("load config");
        assert_eq!(cfg.api_key, "toml-new-key");
        std::fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn database_restores_configuration_when_toml_is_missing() {
        let root = temp_dir();
        std::fs::create_dir_all(&root).expect("create test directory");
        let store = ConfigStore::new(root.join("config.toml"));
        let db_path = root.join("config.db");
        let db = crate::config_db::ConfigDb::open(&db_path).expect("open database");
        db.init_table().expect("initialize database");
        db.save_config(
            &serde_json::to_string(&PersistentConfig {
                api_key: Some("database-only-key".into()),
                database: Some(PersistentDatabaseConfig {
                    enabled: Some(true),
                    url: None,
                }),
                ..Default::default()
            })
            .expect("serialize database snapshot"),
        )
        .expect("write database snapshot");

        let cfg = Config::load_from_paths(store, db_path).expect("restore config");
        assert_eq!(cfg.api_key, "database-only-key");
        std::fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn durable_outbox_replays_the_pending_database_config() {
        let root = temp_dir();
        std::fs::create_dir_all(&root).expect("create test directory");
        let db_path = root.join("config.db");
        let json = serde_json::to_string(&PersistentConfig {
            api_key: Some("outbox-key".into()),
            database: Some(PersistentDatabaseConfig {
                enabled: Some(true),
                url: None,
            }),
            ..Default::default()
        })
        .expect("serialize config");
        Config::write_outbox_at(&db_path, &json).expect("write outbox");
        Config::replay_outbox_at(&db_path).expect("replay outbox");
        assert!(!Config::outbox_path(&db_path).exists());
        let db = crate::config_db::ConfigDb::open(&db_path).expect("open database");
        let saved = db
            .load_config()
            .expect("read database")
            .expect("database config");
        assert_eq!(saved, json);
        std::fs::remove_dir_all(root).expect("remove test directory");
    }
}
