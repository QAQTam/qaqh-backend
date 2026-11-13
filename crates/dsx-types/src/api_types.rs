use serde::{Deserialize, Serialize};

// ── Usage ──

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct UsageInfo {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    #[serde(default)]
    pub prompt_cache_hit_tokens: u32,
    #[serde(default)]
    pub prompt_cache_miss_tokens: u32,
    #[serde(default)]
    pub completion_tokens_details: Option<TokenDetails>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct TokenDetails {
    #[serde(default)]
    pub reasoning_tokens: u32,
}

// ── Model list ──

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub struct ModelList {
    pub data: Vec<ModelInfo>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub owned_by: String,
}

// ── Balance ──

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub struct BalanceInfo {
    pub is_available: bool,
    pub balance_infos: Vec<BalanceEntry>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub struct BalanceEntry {
    pub currency: String,
    pub total_balance: String,
    pub granted_balance: String,
    pub topped_up_balance: String,
}
