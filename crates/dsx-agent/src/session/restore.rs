//! Session restore: scan live sessions for resume.

use dsx_types::{SessionFile, SessionMeta};

pub fn find_live_sessions() -> Vec<SessionMeta> {
    let Some(dir) = super::sessions_dir() else { return vec![] };
    if !dir.exists() { return vec![]; }
    let Ok(entries) = std::fs::read_dir(&dir) else { return vec![] };
    let mut metas = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let fname = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !fname.ends_with(".live.json") { continue; }
        if let Ok(data) = std::fs::read_to_string(&path) {
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
