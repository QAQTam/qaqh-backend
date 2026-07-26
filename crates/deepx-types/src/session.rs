use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Activation state of a single skill within a session.
///
/// Tracks whether a skill is currently loaded and available in the
/// agent's context window.
#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum SkillSessionEntryState {
    /// Skill is loaded and active in the current session.
    Active,
    /// Skill was previously available but is now unavailable
    /// (e.g. file deleted, scope changed).
    Unavailable,
}

/// Runtime tracking for one skill in a session.
#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
pub struct SkillSessionEntry {
    /// Skill name matching SKILL.md metadata.
    pub name: String,
    /// Monotonic counter for determining activation order across sessions.
    pub activation_order: u64,
    /// Path or identifier of the skill source directory (project/user scope).
    pub source: String,
    /// Current activation state.
    pub state: SkillSessionEntryState,
    /// Number of turns remaining before the skill lease expires and
    /// must be re-validated or released.
    pub lease_remaining: u8,
}

/// Snapshot of skill activation state for a session, persisted in meta.json.
///
/// Version 2 adds `context_epoch` and `operation_revision` for tracking
/// skill activation/deactivation across context compaction cycles.
#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq, Eq)]
#[ts(export)]
pub struct SkillSessionStateV2 {
    /// Schema version (always 2).
    pub version: u8,
    /// Epoch counter incremented on context compaction. Used to detect
    /// whether stale skill contexts need refresh.
    pub context_epoch: u64,
    /// Monotonic revision counter for operation ordering across restarts.
    pub operation_revision: u64,
    /// Active skill entries in activation order.
    pub entries: Vec<SkillSessionEntry>,
}

impl Default for SkillSessionStateV2 {
    fn default() -> Self {
        Self {
            version: 2,
            context_epoch: 0,
            operation_revision: 0,
            entries: Vec::new(),
        }
    }
}

/// Session metadata — unified persistence + runtime state.
///
/// Fields marked `#[serde(skip)]` are runtime-only and not persisted to meta.json.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct SessionMeta {
    // ── Persisted fields ──
    pub seed: String,
    pub created_at: u64,
    pub updated_at: u64,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub effort: Option<String>,
    pub message_count: usize,
    /// Number of conversation turns (one user query + its assistant/tool chain).
    #[serde(default)]
    pub turn_count: usize,
    #[serde(default)]
    pub last_summary: String,
    /// Number of earliest turns compacted (skipped in LLM context).
    #[serde(default)]
    pub compact_skip: usize,
    /// Agent operating mode: 0=Normal, 1=Plan, 2=Code.
    /// Persisted so PLAN/CODE mode survives agent restart within the same session.
    #[serde(default)]
    pub mode: u8,
    #[serde(default)]
    pub skills: SkillSessionStateV2,
    /// Provider-confirmed usage accumulated across model requests in this session.
    #[serde(default)]
    #[ts(skip)]
    pub usage_totals: crate::UsageInfo,
    /// Last provider-confirmed request usage, used to restore the live Info panel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(skip)]
    pub last_usage: Option<crate::UsageInfo>,
    /// Number of model requests included in `usage_totals`.
    #[serde(default)]
    #[ts(skip)]
    pub usage_requests: u32,
    /// Number of requests whose provider explicitly returned cache usage.
    #[serde(default)]
    #[ts(skip)]
    pub cache_reported_requests: u32,

    // ── Runtime fields (not persisted) ──
    /// If set, this seed is passed as a CLI argument to the agent subprocess for auto-restore on startup.
    #[serde(skip)]
    #[ts(skip)]
    pub resume_seed: Option<String>,
    /// Cumulative tokens consumed across all turns.
    #[serde(skip)]
    #[ts(skip)]
    pub tokens: u64,
    /// Display title extracted from first user message.
    #[serde(skip)]
    #[ts(skip)]
    pub title: Option<String>,
    /// True if session was restored from disk — system prompt preserved.
    #[serde(skip)]
    #[ts(skip)]
    pub from_resume: bool,

    /// True if this session has messages in the Turso SQLite store.
    #[serde(skip)]
    pub turso_backed: bool,
}
impl Default for SessionMeta {
    fn default() -> Self {
        Self {
            seed: String::new(),
            created_at: 0,
            updated_at: 0,
            model: String::new(),
            effort: None,
            message_count: 0,
            turn_count: 0,
            last_summary: String::new(),
            compact_skip: 0,
            mode: 0,
            skills: SkillSessionStateV2::default(),
            usage_totals: crate::UsageInfo::default(),
            last_usage: None,
            usage_requests: 0,
            cache_reported_requests: 0,
            resume_seed: None,
            tokens: 0,
            title: None,
            from_resume: false,
            turso_backed: false,
        }
    }
}

impl SessionMeta {
    pub fn effective_cache_reported_requests(&self) -> u32 {
        if self.cache_reported_requests == 0
            && self.usage_requests > 0
            && self.usage_totals.prompt_cache_hit_tokens
                .saturating_add(self.usage_totals.prompt_cache_miss_tokens)
                > 0
        {
            self.usage_requests
        } else {
            self.cache_reported_requests
        }
    }

    pub fn record_usage(&mut self, usage: &crate::UsageInfo) {
        self.cache_reported_requests = self.effective_cache_reported_requests();
        if self.cache_reported_requests > 0 {
            self.usage_totals.cache_usage_reported = Some(true);
        }
        self.usage_totals.prompt_tokens = self
            .usage_totals
            .prompt_tokens
            .saturating_add(usage.prompt_tokens);
        self.usage_totals.completion_tokens = self
            .usage_totals
            .completion_tokens
            .saturating_add(usage.completion_tokens);
        self.usage_totals.total_tokens = self
            .usage_totals
            .total_tokens
            .saturating_add(usage.total_tokens);
        self.usage_totals.prompt_cache_hit_tokens = self
            .usage_totals
            .prompt_cache_hit_tokens
            .saturating_add(usage.prompt_cache_hit_tokens);
        self.usage_totals.prompt_cache_miss_tokens = self
            .usage_totals
            .prompt_cache_miss_tokens
            .saturating_add(usage.prompt_cache_miss_tokens);
        self.usage_totals.reasoning_tokens = self
            .usage_totals
            .reasoning_tokens
            .saturating_add(usage.reasoning_tokens);
        if usage.cache_usage_reported == Some(true) {
            self.usage_totals.cache_usage_reported = Some(true);
        }
        self.usage_requests = self.usage_requests.saturating_add(1);
        if usage.cache_usage_reported == Some(true) {
            self.cache_reported_requests = self.cache_reported_requests.saturating_add(1);
        }
        self.last_usage = Some(usage.clone());
        self.tokens = self.usage_totals.total_tokens.into();
    }

    pub fn reset_usage(&mut self) {
        self.tokens = 0;
        self.usage_totals = crate::UsageInfo::default();
        self.last_usage = None;
        self.usage_requests = 0;
        self.cache_reported_requests = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_session_metadata_defaults_to_empty_skill_state_v2() {
        let meta: SessionMeta = serde_json::from_str(
            r#"{
            "seed":"s","created_at":0,"updated_at":0,"model":"m",
            "message_count":0,"turn_count":0,"last_summary":"","compact_skip":0,"mode":0
        }"#,
        )
        .unwrap();
        assert_eq!(meta.skills.version, 2);
        assert!(meta.skills.entries.is_empty());
        assert_eq!(meta.cache_reported_requests, 0);
    }

    #[test]
    fn usage_tracks_cache_reporting_separately_from_hit_rate() {
        let mut meta = SessionMeta::default();
        meta.record_usage(&crate::UsageInfo {
            prompt_tokens: 100,
            prompt_cache_miss_tokens: 100,
            cache_usage_reported: Some(true),
            ..Default::default()
        });
        meta.record_usage(&crate::UsageInfo {
            prompt_tokens: 50,
            ..Default::default()
        });

        assert_eq!(meta.usage_requests, 2);
        assert_eq!(meta.cache_reported_requests, 1);
        assert_eq!(meta.usage_totals.cache_usage_reported, Some(true));
        assert_eq!(meta.usage_totals.prompt_cache_hit_tokens, 0);
        assert_eq!(meta.usage_totals.prompt_cache_miss_tokens, 100);
    }

    #[test]
    fn legacy_cache_totals_infer_full_request_coverage() {
        let meta = SessionMeta {
            usage_requests: 3,
            usage_totals: crate::UsageInfo {
                prompt_cache_hit_tokens: 60,
                prompt_cache_miss_tokens: 40,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(meta.effective_cache_reported_requests(), 3);
    }
}
