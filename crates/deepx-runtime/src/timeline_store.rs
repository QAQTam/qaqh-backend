//! Durable Ringing V1 timeline state. One atomically replaced record per session contains
//! the materialized recovery snapshot and its replay tail.

#[cfg(test)]
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
        let parent = root.into();
        let root = parent.join("ringing-timeline");
        // Preserve replay recovery across the one-time pre-V1 → Ringing V1 rename.
        // The legacy name is migration-only; all new reads and writes use the
        // versionless Ringing timeline directory.
        let legacy = parent.join("timeline-v3");
        if !root.exists() && legacy.is_dir() {
            std::fs::rename(&legacy, &root)?;
        }
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

    /// 全量装载（仅测试用；生产走 `list_seeds` + `load_seed` 懒加载）。
    #[cfg(test)]
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

    /// 磁盘上的 timeline seed 清单（懒加载索引；不读取文件内容）。
    pub fn list_seeds(&self) -> std::io::Result<Vec<String>> {
        let mut seeds = Vec::new();
        for entry in std::fs::read_dir(&self.root)? {
            let path = entry?.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            if let Some(seed) = path.file_stem().and_then(|s| s.to_str()) {
                if !seed.is_empty() {
                    seeds.push(seed.to_string());
                }
            }
        }
        Ok(seeds)
    }

    /// 装载单个 seed 的持久化 timeline（懒加载按需恢复用）。
    pub fn load_seed(&self, seed: &str) -> Option<PersistedTimeline> {
        let path = self.path_for(seed);
        std::fs::read(&path)
            .ok()
            .and_then(|body| serde_json::from_slice::<PersistedTimeline>(&body).ok())
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
    fn persists_and_loads_a_native_timeline_without_channel_envelopes() {
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

    #[test]
    fn migrates_legacy_timeline_storage_into_the_ringing_v1_root() {
        let root = std::env::temp_dir().join(format!(
            "deepx-timeline-store-migration-{}",
            std::process::id()
        ));
        let legacy = root.join("timeline-v3");
        std::fs::create_dir_all(&legacy).expect("create legacy directory");
        let record = PersistedTimeline {
            seed: "seed".into(),
            snapshot: TimelineSnapshot {
                watermark: 7,
                turns: vec![],
            },
            journal: vec![],
        };
        std::fs::write(
            legacy.join("seed.json"),
            serde_json::to_vec(&record).expect("serialize legacy record"),
        )
        .expect("write legacy record");

        let store = TimelineStore::new(&root).expect("migrate legacy directory");
        let loaded = store.load().expect("load migrated record");
        assert_eq!(loaded["seed"].snapshot.watermark, 7);
        assert!(root.join("ringing-timeline").is_dir());
        assert!(!legacy.exists());
        let _ = std::fs::remove_dir_all(root);
    }
}
