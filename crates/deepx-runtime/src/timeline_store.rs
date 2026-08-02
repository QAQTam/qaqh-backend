//! Durable v3 Timeline state. It intentionally has no dependency on the v2
//! Ringing journal format: one atomically replaced record per session contains
//! the materialized recovery snapshot and its replay tail.

use std::collections::HashMap;
use std::path::PathBuf;

use deepx_domain::{TimelineEntry, TimelineSnapshot};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedTimeline {
    pub seed: String,
    pub snapshot: TimelineSnapshot,
    pub journal: Vec<TimelineEntry>,
}

#[derive(Debug)]
pub struct TimelineStore {
    root: PathBuf,
}

impl TimelineStore {
    pub fn new(root: impl Into<PathBuf>) -> std::io::Result<Self> {
        let root = root.into().join("timeline-v3");
        std::fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    pub fn persist(
        &self,
        seed: &str,
        snapshot: &TimelineSnapshot,
        journal: Vec<TimelineEntry>,
    ) -> std::io::Result<()> {
        let path = self.path_for(seed);
        let tmp = path.with_extension("json.tmp");
        let body = serde_json::to_vec(&PersistedTimeline {
            seed: seed.to_string(),
            snapshot: snapshot.clone(),
            journal,
        })
        .map_err(io_error)?;
        std::fs::write(&tmp, body)?;
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        std::fs::rename(tmp, path)
    }

    pub fn load(&self) -> std::io::Result<HashMap<String, PersistedTimeline>> {
        let mut timelines = HashMap::new();
        for entry in std::fs::read_dir(&self.root)? {
            let path = entry?.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            match std::fs::read(&path)
                .ok()
                .and_then(|body| serde_json::from_slice::<PersistedTimeline>(&body).ok())
            {
                Some(timeline) => {
                    if timeline.seed.is_empty() {
                        log::warn!("[timeline] skip record without seed {}", path.display());
                    } else {
                        timelines.insert(timeline.seed.clone(), timeline);
                    }
                }
                None => log::warn!(
                    "[timeline] skip corrupt persistent record {}",
                    path.display()
                ),
            }
        }
        Ok(timelines)
    }

    fn path_for(&self, seed: &str) -> PathBuf {
        self.root.join(format!("{}.json", sanitize_seed(seed)))
    }
}

fn sanitize_seed(seed: &str) -> String {
    seed.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn io_error(error: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persists_and_loads_a_native_timeline_without_v2_envelopes() {
        let root =
            std::env::temp_dir().join(format!("deepx-timeline-store-{}", std::process::id()));
        let store = TimelineStore::new(&root).unwrap();
        store
            .persist(
                "seed",
                &TimelineSnapshot {
                    watermark: 0,
                    turns: vec![],
                },
                vec![],
            )
            .unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded["seed"].snapshot.watermark, 0);
        let _ = std::fs::remove_dir_all(root);
    }
}
