use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ── Config persistence ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistentConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_limit: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_lang: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profiles: Option<HashMap<String, ProfileConfig>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preferences: Option<UserPreferences>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_mode: Option<bool>,
}

// ── Profile / Preferences ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileConfig {
    pub model: String,
    pub max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    pub context_limit: u32,
    #[serde(default = "default_base_url")]
    pub base_url: String,
    #[serde(default = "default_lang")]
    pub prompt_lang: String,
}

fn default_base_url() -> String { "https://api.deepseek.com".into() }
fn default_lang() -> String { "en".into() }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserPreferences {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_compact: Option<bool>,
}
