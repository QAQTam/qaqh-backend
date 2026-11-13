//! Live snapshot: crash recovery — save/load/delete real-time state.

use crate::router;
use dsx_types::{Message, SessionFile, StreamState};

pub fn save_live_snapshot(
    seed: &str,
    messages: &[Message],
    model: &str,
    effort: Option<&str>,
    stream_state: Option<StreamState>,
) {
    let Some(lp) = super::live_path(seed) else { return };
    let _ = std::fs::create_dir_all(lp.parent().unwrap());

    let now = super::now_epoch();
    let created_at = super::load_index().iter()
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
        stream_state,
        semantic_memory: None,
        task_phase: Some(router::read_phase()),
    };

    // Atomic write: tmp -> rename
    let tmp_path = lp.with_extension("live.tmp");
    let _ = std::fs::write(&tmp_path, serde_json::to_string_pretty(&file).unwrap_or_default());
    let _ = std::fs::rename(&tmp_path, &lp);
}

pub fn load_live_snapshot(seed: &str) -> Option<SessionFile> {
    let path = super::live_path(seed)?;
    if !path.exists() {
        return None;
    }
    let data = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str::<SessionFile>(&data).ok()
}

pub fn delete_live_snapshot(seed: &str) {
    cleanup_live(seed);
}

pub(crate) fn cleanup_live(seed: &str) {
    if let Some(lp) = super::live_path(seed) {
        let _ = std::fs::remove_file(&lp);
    }
}
