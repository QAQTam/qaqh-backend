//! Persistent scan list of seeds that may still hold a driver seat.
//!
//! This is deliberately **not** seat state: the canonical `DriverChanged` fact
//! stays the single source of truth. The list only answers "which seeds are
//! worth re-reading every few seconds", and it is persisted so a daemon restart
//! does not lose track of seats whose holder's lease expired while the daemon
//! was down.
//!
//! Entries are added when a seat is observed (claim request or bootstrap) and
//! removed once the canonical seat is vacant, or when the seed can no longer be
//! scanned (session gone). A stale entry costs one control-projection read.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Minimum gap between two reclaim dispatches for the same seed.
///
/// A dispatch can be accepted by the runtime but rejected inside the actor
/// (e.g. the canonical writer fence of a previous daemon is still live until
/// its lease expires), and the sweep cannot observe that failure. Re-dispatching
/// every tick would spam logs and emit a failure event each time, so a rejected
/// reclaim is retried on this cooldown instead.
pub const RECLAIM_RETRY_COOLDOWN_MS: u64 = 15_000;

#[derive(Debug, Default)]
pub struct RingingDriverWatch {
    sessions: HashSet<String>,
    /// In-memory only: last dispatch time per seed (epoch ms).
    dispatched_at_ms: HashMap<String, u64>,
    persistence_path: Option<PathBuf>,
}

impl RingingDriverWatch {
    pub fn new() -> Self {
        Self::default()
    }

    /// Load `<data_dir>/ringing-driver-watch.json`.
    ///
    /// Unreadable or malformed files start empty: losing the scan list only
    /// delays reclamation until the next claim/bootstrap re-registers the seed.
    pub fn new_persistent() -> Self {
        let path = qaqh_types::platform::data_dir().join("ringing-driver-watch.json");
        let mut watch = Self {
            persistence_path: Some(path.clone()),
            ..Self::new()
        };
        watch.load(&path);
        watch
    }

    fn load(&mut self, path: &Path) {
        let Ok(bytes) = std::fs::read(path) else {
            return;
        };
        match serde_json::from_slice::<Vec<String>>(&bytes) {
            Ok(sessions) => {
                self.sessions = sessions
                    .into_iter()
                    .filter(|session_id| !session_id.trim().is_empty())
                    .collect();
            }
            Err(_) => log::warn!(
                "[ringing] driver watch list is unreadable; starting empty ({})",
                path.display()
            ),
        }
    }

    fn persist(&self) {
        let Some(path) = &self.persistence_path else {
            return;
        };
        let mut sessions: Vec<&String> = self.sessions.iter().collect();
        sessions.sort();
        let Ok(bytes) = serde_json::to_vec(&sessions) else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, bytes).is_ok() && std::fs::rename(&tmp, path).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }

    /// Register a seed for scanning. Returns whether the set changed.
    pub fn insert(&mut self, session_id: &str) -> bool {
        if session_id.trim().is_empty() || !self.sessions.insert(session_id.to_string()) {
            return false;
        }
        self.persist();
        true
    }

    /// Drop a seed from the scan list. Returns whether the set changed.
    pub fn remove(&mut self, session_id: &str) -> bool {
        self.dispatched_at_ms.remove(session_id);
        if !self.sessions.remove(session_id) {
            return false;
        }
        self.persist();
        true
    }

    /// Whether a reclaim may be dispatched for this seed now.
    pub fn reclaim_due(&self, session_id: &str, now_ms: u64) -> bool {
        self.dispatched_at_ms
            .get(session_id)
            .is_none_or(|last| now_ms.saturating_sub(*last) >= RECLAIM_RETRY_COOLDOWN_MS)
    }

    /// Record a reclaim dispatch attempt for this seed.
    pub fn note_reclaim_dispatch(&mut self, session_id: &str, now_ms: u64) {
        self.dispatched_at_ms.insert(session_id.to_string(), now_ms);
    }

    pub fn contains(&self, session_id: &str) -> bool {
        self.sessions.contains(session_id)
    }

    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// Snapshot of the seeds to scan (stable order).
    pub fn sessions(&self) -> Vec<String> {
        let mut sessions: Vec<String> = self.sessions.iter().cloned().collect();
        sessions.sort();
        sessions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn watch_at(path: &Path) -> RingingDriverWatch {
        RingingDriverWatch {
            sessions: HashSet::new(),
            dispatched_at_ms: HashMap::new(),
            persistence_path: Some(path.to_path_buf()),
        }
    }

    #[test]
    fn reclaim_dispatch_is_rate_limited_per_session() {
        let mut watch = RingingDriverWatch::new();
        assert!(watch.reclaim_due("seed-a", 1_000));
        watch.note_reclaim_dispatch("seed-a", 1_000);
        assert!(!watch.reclaim_due("seed-a", 1_000 + RECLAIM_RETRY_COOLDOWN_MS - 1));
        assert!(watch.reclaim_due("seed-a", 1_000 + RECLAIM_RETRY_COOLDOWN_MS));
        assert!(watch.reclaim_due("seed-b", 1_000), "guard is per seed");

        // Dropping the seed clears the guard so a later seat starts fresh.
        watch.insert("seed-a");
        watch.remove("seed-a");
        assert!(watch.reclaim_due("seed-a", 1_001));
    }

    #[test]
    fn watch_round_trips_through_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("driver-watch.json");
        let mut watch = watch_at(&path);
        assert!(watch.insert("seed-a"));
        assert!(watch.insert("seed-b"));
        assert!(!watch.insert("seed-a"), "duplicate insert is a no-op");
        assert!(!watch.insert("  "), "blank seed is rejected");
        assert_eq!(
            watch.sessions(),
            vec!["seed-a".to_string(), "seed-b".to_string()]
        );

        let mut reloaded = watch_at(&path);
        reloaded.load(&path);
        assert_eq!(reloaded.sessions(), watch.sessions());
        assert!(reloaded.contains("seed-a"));

        assert!(reloaded.remove("seed-a"));
        assert!(!reloaded.remove("seed-a"), "second remove is a no-op");
        let mut again = watch_at(&path);
        again.load(&path);
        assert_eq!(again.sessions(), vec!["seed-b".to_string()]);
    }
}
