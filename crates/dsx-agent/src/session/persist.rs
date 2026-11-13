//! Session file persistence: save and load conversation sessions.

use crate::router;
use dsx_types::{Message, SessionFile};

// ── Session loading ──

pub fn load_session(seed: &str) -> Option<SessionFile> {
    // Try live first, then completed
    if let Some((file, _)) = load_session_or_live(seed) {
        return Some(file);
    }
    None
}

pub fn load_session_or_live(seed: &str) -> Option<(SessionFile, bool)> {
    // Try .live first (interrupted session)
    if let Some(lp) = super::live_path(seed) {
        if lp.exists() {
            if let Ok(data) = std::fs::read_to_string(&lp) {
                if let Ok(file) = serde_json::from_str::<SessionFile>(&data) {
                    return Some((file, true));
                }
            }
        }
    }
    // Fall back to completed .json
    let path = super::session_path(seed)?;
    if !path.exists() { return None; }
    let data = std::fs::read_to_string(&path).ok()?;
    let file = serde_json::from_str::<SessionFile>(&data).ok()?;
    Some((file, false))
}

// ── Session saving ──

fn save_session(
    seed: &str,
    messages: &[Message],
    model: &str,
    effort: Option<&str>,
) {
    let Some(sfile_path) = super::session_path(seed) else { return };
    let _ = std::fs::create_dir_all(sfile_path.parent().unwrap());

    let now = super::now_epoch();
    // Preserve created_at from existing meta if available
    let created_at = super::index::load_index().iter()
        .find(|m| m.seed == seed)
        .map(|m| m.created_at)
        .unwrap_or(now);

    let last_summary = super::extract_last_summary(messages);

    let file = SessionFile {
        seed: seed.to_string(),
        created_at,
        updated_at: now,
        model: model.to_string(),
        effort: effort.map(|s| s.to_string()),
        messages: messages.to_vec(),
        last_summary,
        stream_state: None,
        semantic_memory: None,
        task_phase: Some(router::read_phase()),
    };

    let _ = std::fs::write(&sfile_path, serde_json::to_string_pretty(&file).unwrap_or_default());
    super::index::update_index_entry(&file);
}

pub fn finalize_session(
    seed: &str,
    messages: &[Message],
    model: &str,
    effort: Option<&str>,
) {
    // Save completed .json
    save_session(seed, messages, model, effort);
    // Clean up .live file
    super::snapshot::cleanup_live(seed);
}
