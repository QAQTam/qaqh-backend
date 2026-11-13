//! Session restore: scan live sessions for resume.

use dsx_types::{SessionFile, SessionMeta};

pub fn find_live_sessions() -> Vec<SessionMeta> {
    let Some(dir) = super::sessions_dir() else { return vec![] };
    if !dir.exists() { return vec![]; }
    let Ok(entries) = std::fs::read_dir(&dir) else { return vec![] };
    let mut metas = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        // Old format: sessions/{seed}.live.json
        // New format: sessions/{seed}-{date}/session.live.json
        let live_path = if path.is_dir() {
            path.join("session.live.json")
        } else if path.extension().map(|e| e == "json").unwrap_or(false) && path.file_name().and_then(|n| n.to_str()).map_or(false, |n| n.ends_with(".live.json")) {
            path
        } else {
            continue;
        };
        if !live_path.exists() { continue; }
        if let Ok(data) = std::fs::read_to_string(&live_path) {
            if let Ok(file) = serde_json::from_str::<SessionFile>(&data) {
                let is_live = file.stream_state.is_some();
                metas.push(SessionMeta {
                    seed: file.seed,
                    created_at: file.created_at,
                    updated_at: file.updated_at,
                    model: file.model,
                    effort: file.effort,
                    message_count: file.messages.len(),
                    last_summary: if is_live {
                        format!("[INTERRUPTED] {}", file.last_summary)
                    } else {
                        file.last_summary
                    },
                });
            }
        }
    }
    metas
}
