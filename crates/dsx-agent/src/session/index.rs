//! Session index: list of saved sessions.

use dsx_types::{SessionFile, SessionMeta};

fn save_index(metas: &[SessionMeta]) {
    let Some(path) = super::index_path() else { return };
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    let _ = std::fs::write(&path, serde_json::to_string_pretty(metas).unwrap_or_default());
}

pub fn load_index() -> Vec<SessionMeta> {
    let Some(path) = super::index_path() else { return vec![] };
    if !path.exists() { return vec![]; }
    let Ok(data) = std::fs::read_to_string(&path) else { return vec![] };
    serde_json::from_str::<Vec<SessionMeta>>(&data).unwrap_or_default()
}

pub(super) fn update_index_entry(file: &SessionFile) {
    let mut metas = load_index();
    let meta = SessionMeta {
        seed: file.seed.clone(),
        created_at: file.created_at,
        updated_at: file.updated_at,
        model: file.model.clone(),
        effort: file.effort.clone(),
        message_count: file.messages.len(),
        last_summary: file.last_summary.clone(),
    };
    if let Some(existing) = metas.iter_mut().find(|m| m.seed == meta.seed) {
        *existing = meta;
    } else {
        metas.push(meta);
    }
    save_index(&metas);
}
