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
            resume_seed: None,
            tokens: 0,
            title: None,
            from_resume: false,
            turso_backed: false,
        }
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
    }
}
