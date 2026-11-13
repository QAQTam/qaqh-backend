//! Session helpers: paths, pitfall guide loading.
//!
//! Provides the same path resolution as dsx-agent::session so that HP can
//! load pitfalls without depending on dsx-agent.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

// ── Time helpers ──

pub fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn chrono_date() -> String {
    let secs = now_epoch();
    let days = (secs / 86400) as u32;
    let y = 1970 + days / 365;
    let rem = days % 365;
    let m = (rem / 30).min(11) + 1;
    let d = (rem % 30).min(30) + 1;
    format!("{:04}-{:02}-{:02}", y, m, d)
}

// ── Path resolution ──

pub fn sessions_dir() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let base = std::env::var("DSC_CONFIG_DIR").ok()
        .map(PathBuf::from)
        .or_else(|| std::env::var("XDG_CONFIG_HOME").ok().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(&home).join(".config"));
    Some(base.join("dsx").join("sessions"))
}

pub fn pitfalls_path() -> Option<PathBuf> {
    sessions_dir().map(|d| d.join("pitfalls.json"))
}

// ── Pitfall types (for assessment.rs find_matches) ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PitfallEntry {
    pub emotion: String,
    pub error_kind: String,
    pub tool: String,
    pub description: String,
    pub lesson: String,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub frequency: u32,
    #[serde(default)]
    pub first_seen: u64,
    #[serde(default)]
    pub last_seen: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PitfallGuide {
    pub entries: Vec<PitfallEntry>,
    #[serde(default)]
    pub total_updates: u64,
}

impl Default for PitfallGuide {
    fn default() -> Self {
        Self { entries: Vec::new(), total_updates: 0 }
    }
}

/// Public result struct returned from `find_matches`.
pub struct PitfallMatch {
    pub error_kind: String,
    pub lesson: String,
}

impl PitfallGuide {
    /// Find entries matching a given ErrorKind variant name.
    pub fn find_matches(
        &self,
        kind: crate::assessment::ErrorKind,
        _extra: &str,
    ) -> Vec<PitfallMatch> {
        let kind_name = format!("{:?}", kind);
        self.entries
            .iter()
            .filter(|e| e.error_kind == kind_name)
            .map(|e| PitfallMatch {
                error_kind: e.error_kind.clone(),
                lesson: e.lesson.clone(),
            })
            .collect()
    }
}

// ── I/O ──

pub fn load_pitfalls() -> PitfallGuide {
    let Some(path) = pitfalls_path() else { return PitfallGuide::default(); };
    if !path.exists() { return PitfallGuide::default(); }
    let Ok(data) = std::fs::read_to_string(&path) else { return PitfallGuide::default(); };
    serde_json::from_str(&data).unwrap_or_default()
}
