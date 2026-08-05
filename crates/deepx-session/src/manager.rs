//! SessionManager — unified singleton for session persistence and lifecycle.
//!
//! Stores each session as:
//!   {sessions_dir}/{seed}/
//!     meta.json       — SessionMeta (atomic replace-write)
//!     messages.jsonl  — one JSON line per Message (append-only)
//!
//! A central `index.json` enables fast listing.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use deepx_types::{Message, SessionMeta};

use crate::store;


static INSTANCE: OnceLock<SessionManager> = OnceLock::new();

/// The LLM-facing view after a compact operation.  Raw messages remain in the
/// normal session archive; this is deliberately a separate, replaceable view.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CompactContext {
    pub version: u32,
    pub checkpoint_id: String,
    pub parent_checkpoint_id: Option<String>,
    pub created_at: u64,
    pub archive_message_count: usize,
    pub messages: Vec<Message>,
}

fn read_messages_without_deduplication(path: &std::path::Path) -> Result<Vec<Message>, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    content
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(index, line)| {
            serde_json::from_str(line)
                .map_err(|error| format!("parse {} line {}: {error}", path.display(), index + 1))
        })
        .collect()
}

#[derive(Debug)]
pub struct SessionManager {
    sessions_dir: PathBuf,
    active_path: PathBuf,
    session_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl SessionManager {
    /// Initialize the global singleton. Must be called once at startup.
    /// Also triggers automatic migration from legacy TOML format if needed.
    pub fn init(data_dir: PathBuf) {
        let sessions_dir = data_dir.join("sessions");
        let _ = std::fs::create_dir_all(&sessions_dir);

        let mgr = Self {
            active_path: data_dir.join(".active_session"),
            session_locks: Mutex::new(HashMap::new()),
            sessions_dir,
        };
        // Migrate old TOML sessions on first startup of v0.4.0
        crate::migrate::run(&mgr.sessions_dir);
        INSTANCE
            .set(mgr)
            .expect("SessionManager already initialized");
    }

    /// Access the global instance.
    pub fn global() -> &'static Self {
        INSTANCE
            .get()
            .expect("SessionManager not initialized — call init() first")
    }

    // ── Session listing ──

    /// List all sessions sorted by updated_at descending.
    pub fn list(&self) -> Vec<SessionMeta> {
        let mut metas = store::read_index(&self.sessions_dir);

        // Fallback: scan directories if index is empty
        if metas.is_empty() {
            if let Ok(entries) = std::fs::read_dir(&self.sessions_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if !path.is_dir() {
                        continue;
                    }
                    let meta = store::read_meta(&path);
                    if let Some(meta) = meta {
                        metas.push(meta);
                    }
                }
            }
        }

        metas.sort_by_key(|m| std::cmp::Reverse(m.updated_at));
        metas
    }

    /// Delete a session: removes the session directory and its index entry.
    pub fn delete(&self, seed: &str) -> Result<(), String> {
        let dir = self
            .session_dir(seed)
            .ok_or_else(|| format!("Session not found: {seed}"))?;

        std::fs::remove_dir_all(&dir).map_err(|e| format!("Failed to delete session: {e}"))?;

        store::remove_from_index(&self.sessions_dir, seed);

        
        log::info!("SessionManager: deleted session {seed}");
        Ok(())
    }

    // ── Load / Save ──

    /// Read the persisted JSONL files for a session.
    pub fn load(&self, seed: &str) -> Option<(SessionMeta, Vec<Message>)> {
        self.snapshot_from_files(seed).ok()
    }

    /// Load the immutable archive plus the latest compact context, if one
    /// exists.  Callers must use `active_messages` for the model loop and
    /// retain `archive_messages` for replay/pagination.
    pub fn load_for_resume(
        &self,
        seed: &str,
    ) -> Option<(SessionMeta, Vec<Message>, Option<CompactContext>)> {
        let (meta, archive_messages) = self.load(seed)?;
        let selected = self
            .read_compact_context(seed)
            .filter(|context| context.archive_message_count <= archive_messages.len());
        Some((meta, archive_messages, selected))
    }

    /// Persist a new checkpoint without rewriting the raw history archive.
    pub fn save_compact_context(&self, seed: &str, messages: &[Message]) {
        let lock = self.session_lock(seed);
        let _guard = lock.lock().unwrap();
        let archive_count = store::read_messages(&self.session_path_dir(seed))
            .map(|m| m.len())
            .unwrap_or(0);
        let parent_checkpoint_id = self
            .read_compact_context(seed)
            .map(|context| context.checkpoint_id);
        let now = Self::now_epoch();
        let context = CompactContext {
            version: 1,
            checkpoint_id: format!("compact-{now}-{archive_count}"),
            parent_checkpoint_id,
            created_at: now,
            archive_message_count: archive_count,
            messages: messages.to_vec(),
        };
        if let Err(error) = self.write_compact_context(seed, &context) {
            log::error!("SessionManager: write compact context failed for {seed}: {error}");
            return;
        }
    }

    /// Refresh the active view after later raw messages were appended.
    pub fn update_compact_context(&self, seed: &str, messages: &[Message]) {
        let Some(mut context) = self.read_compact_context(seed) else {
            return;
        };
        let lock = self.session_lock(seed);
        let _guard = lock.lock().unwrap();
        context.archive_message_count = store::read_messages(&self.session_path_dir(seed))
            .map(|m| m.len())
            .unwrap_or(0);
        context.messages = messages.to_vec();
        if let Err(error) = self.write_compact_context(seed, &context) {
            log::error!("SessionManager: update compact context failed for {seed}: {error}");
            return;
        }
    }

    /// Check whether a session exists on disk.
    pub fn exists(&self, seed: &str) -> bool {
        if self.session_dir(seed).is_some() {
            return true;
        }
        false
    }

    /// Load only metadata (fast, no message parsing). JSON remains primary
    /// until the DB-primary readiness gate is explicitly promoted.
    pub fn load_meta(&self, seed: &str) -> Option<SessionMeta> {
        if let Some(dir) = self.session_dir(seed) {
            if let Some(meta) = store::read_meta(&dir) {
                return Some(meta);
            }
        }
        None
    }

    /// Persist agent mode to meta.json without rewriting messages.
    /// Called when the user switches PLAN/CODE mode so it survives agent restart.
    pub fn persist_mode(&self, seed: &str, mode: u8) {
        let lock = self.session_lock(seed);
        let _guard = lock.lock().unwrap();
        let dir = self.session_path_dir(seed);
        let mut meta = self.load_meta(seed).unwrap_or_default();
        meta.mode = mode;
        let _ = store::write_meta(&dir, &meta);
            }

    pub fn persist_skills(&self, seed: &str, skills: deepx_types::SkillSessionStateV2) {
        let lock = self.session_lock(seed);
        let _guard = lock.lock().unwrap();
        let dir = self.session_path_dir(seed);
        let _ = std::fs::create_dir_all(&dir);
        let mut meta = self.load_meta(seed).unwrap_or_default();
        let now = Self::now_epoch();
        meta.seed = seed.to_string();
        if meta.created_at == 0 {
            meta.created_at = now;
        }
        meta.updated_at = now;
        meta.skills = skills;
        let _ = store::write_meta(&dir, &meta);
        store::upsert_index(&self.sessions_dir, &meta);
            }

    /// Synchronously create a new session directory and initial meta.json
    /// on disk, so that the session exists before the agent process starts.
    /// This prevents the race where the frontend receives a seed from
    /// `session.new` but the session directory isn't created until the
    /// agent writes it asynchronously during boot.
    pub fn persist_new_session(&self, seed: &str) {
        let lock = self.session_lock(seed);
        let _guard = lock.lock().unwrap();
        let dir = self.session_path_dir(seed);
        let _ = std::fs::create_dir_all(&dir);
        let mut meta = self.load_meta(seed).unwrap_or_default();
        let now = Self::now_epoch();
        meta.seed = seed.to_string();
        meta.created_at = now;
        meta.updated_at = now;
        if !dir.join("messages.jsonl").exists() {
            let _ = store::append_messages(&dir, &[]);
        }
        let _ = store::write_meta(&dir, &meta);
        store::upsert_index(&self.sessions_dir, &meta);
            }

    pub fn persist_usage(
        &self,
        seed: &str,
        totals: deepx_types::UsageInfo,
        last_usage: Option<deepx_types::UsageInfo>,
        requests: u32,
        cache_reported_requests: u32,
    ) {
        let lock = self.session_lock(seed);
        let _guard = lock.lock().unwrap();
        let dir = self.session_path_dir(seed);
        let _ = std::fs::create_dir_all(&dir);
        let mut meta = self.load_meta(seed).unwrap_or_default();
        meta.seed = seed.to_string();
        meta.updated_at = Self::now_epoch();
        meta.usage_totals = totals;
        meta.last_usage = last_usage;
        meta.usage_requests = requests;
        meta.cache_reported_requests = cache_reported_requests;
        let _ = store::write_meta(&dir, &meta);
        store::upsert_index(&self.sessions_dir, &meta);
            }

    /// Append a single message to JSONL immediately (per-message persistence).
    /// Writes a complete target snapshot to the durable outbox before appending.
    pub fn save_one(&self, seed: &str, msg: &Message) {
        let lock = self.session_lock(seed);
        let _guard = lock.lock().unwrap();
        let dir = self.session_path_dir(seed);
        let _ = std::fs::create_dir_all(&dir);
        let mut meta = self.load_meta(seed).unwrap_or_default();
        let now = Self::now_epoch();
        meta.seed = seed.to_string();
        if meta.created_at == 0 {
            meta.created_at = now;
        }
        meta.updated_at = now;
        let mut target_messages = store::read_messages(&dir).unwrap_or_default();
        target_messages.push(msg.clone());
        meta.message_count = target_messages.len();
                if let Err(e) = store::append_one(&dir, msg) {
            log::error!("SessionManager: save_one failed: {e}");
            return;
        }
        if let Err(e) = store::write_meta(&dir, &meta) {
            log::error!("SessionManager: save_one metadata write failed: {e}");
            return;
        }
        store::upsert_index(&self.sessions_dir, &meta);
            }

    /// Update session metadata and index after messages have been appended.
    pub fn update_meta(
        &self,
        seed: &str,
        model: &str,
        effort: Option<&str>,
        compact_skip: usize,
        turn_count: usize,
    ) {
        let lock = self.session_lock(seed);
        let _guard = lock.lock().unwrap();
        let now = Self::now_epoch();
        let dir = self.session_path_dir(seed);
        let created_at = self.load_meta(seed).map(|m| m.created_at).unwrap_or(now);
        let total = store::count_message_lines(&dir).unwrap_or(0);

        // Extract summary: read last few messages for title
        let last_summary = match store::read_messages(&dir) {
            Ok(msgs) => Self::extract_summary(&msgs),
            Err(_) => String::new(),
        };

        let existing = self.load_meta(seed).unwrap_or_default();

        let meta = SessionMeta {
            seed: seed.to_string(),
            created_at,
            updated_at: now,
            model: model.to_string(),
            effort: effort.map(String::from),
            message_count: total,
            turn_count,
            last_summary,
            compact_skip,
            mode: existing.mode,
            skills: existing.skills,
            usage_totals: existing.usage_totals,
            last_usage: existing.last_usage,
            usage_requests: existing.usage_requests,
            cache_reported_requests: existing.cache_reported_requests,
            ..Default::default()
        };
                if let Err(e) = store::write_meta(&dir, &meta) {
            log::error!("SessionManager: write_meta failed: {e}");
            return;
        }
        store::upsert_index(&self.sessions_dir, &meta);

            }

    /// Save session: write meta + rewrite all messages.
    /// Used for initial save or after undo/compact.
    pub fn save_full(
        &self,
        seed: &str,
        messages: &[Message],
        model: &str,
        effort: Option<&str>,
        compact_skip: usize,
        turn_count: usize,
    ) {
        let lock = self.session_lock(seed);
        let _guard = lock.lock().unwrap();
        let now = Self::now_epoch();
        let dir = self.session_path_dir(seed);
        let _ = std::fs::create_dir_all(&dir);

        let created_at = self.load_meta(seed).map(|m| m.created_at).unwrap_or(now);

        let existing = self.load_meta(seed).unwrap_or_default();
        let last_summary = Self::extract_summary(messages);

        let meta = SessionMeta {
            seed: seed.to_string(),
            created_at,
            updated_at: now,
            model: model.to_string(),
            effort: effort.map(String::from),
            message_count: messages.len(),
            turn_count,
            last_summary,
            compact_skip,
            mode: existing.mode,
            skills: existing.skills,
            ..Default::default()
        };

        
        if let Err(e) = store::rewrite_messages(&dir, messages) {
            log::error!("SessionManager: rewrite_messages failed: {e}");
            return;
        }
        if let Err(e) = store::write_meta(&dir, &meta) {
            log::error!("SessionManager: write_meta failed: {e}");
            return;
        }
        store::upsert_index(&self.sessions_dir, &meta);

            }

    /// Append new messages (since last save) to the session JSONL.
    /// Updates meta and index.
    pub fn save_append(
        &self,
        seed: &str,
        new_messages: &[Message],
        model: &str,
        effort: Option<&str>,
        compact_skip: usize,
        turn_count: usize,
    ) {
        let lock = self.session_lock(seed);
        let _guard = lock.lock().unwrap();
        if new_messages.is_empty() {
            return;
        }

        let now = Self::now_epoch();
        let dir = self.session_path_dir(seed);
        let _ = std::fs::create_dir_all(&dir);

        let existing = self.load_meta(seed).unwrap_or_default();
        let created_at = if existing.created_at == 0 {
            now
        } else {
            existing.created_at
        };

        let existing_messages = store::read_messages(&dir).unwrap_or_default();
        let last_summary = Self::extract_summary(new_messages);
        let meta = SessionMeta {
            seed: seed.to_string(),
            created_at,
            updated_at: now,
            model: model.to_string(),
            effort: effort.map(String::from),
            message_count: existing_messages.len() + new_messages.len(),
            turn_count,
            last_summary,
            compact_skip,
            mode: existing.mode,
            skills: existing.skills,
            ..Default::default()
        };
        let mut target_messages = existing_messages;
        target_messages.extend_from_slice(new_messages);
        
        // Append messages
        if let Err(e) = store::append_messages(&dir, new_messages) {
            log::error!("SessionManager: append_messages failed: {e}");
            return;
        }

        if let Err(e) = store::write_meta(&dir, &meta) {
            log::error!("SessionManager: write_meta failed: {e}");
            return;
        }
        store::upsert_index(&self.sessions_dir, &meta);

            }

    /// Truncate messages.jsonl to `keep_lines` lines.
    /// Returns the truncated messages.
    pub fn truncate_messages(&self, seed: &str, keep_lines: usize) -> Result<Vec<Message>, String> {
        let lock = self.session_lock(seed);
        let _guard = lock.lock().unwrap();
        let dir = self
            .session_dir(seed)
            .ok_or_else(|| format!("Session not found: {seed}"))?;
        let truncated = store::truncate_messages(&dir, keep_lines)?;
                Ok(truncated)
    }

    // ── Active session ──

    /// Read the currently active session seed.
    pub fn active_seed(&self) -> Option<String> {
        std::fs::read_to_string(&self.active_path)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    /// Set the active session seed (persisted to disk).
    pub fn set_active_seed(&self, seed: &str) {
        if let Some(parent) = self.active_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if std::fs::write(&self.active_path, seed).is_err() {
            log::error!("SessionManager: failed to write active session file");
        }
    }

    /// Clear the active session marker.
    pub fn clear_active(&self) {
        let _ = std::fs::remove_file(&self.active_path);
    }

    // ── Helpers ──

    /// Generate a new session seed (8 hex chars from hashed time + PID).
    pub fn generate_seed() -> String {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut h = DefaultHasher::new();
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .hash(&mut h);
        std::process::id().hash(&mut h);
        let v = h.finish();
        let mixed = (v as u32) ^ ((v >> 32) as u32);
        format!("{:08x}", mixed)
    }

    /// Current UNIX epoch.
    pub fn now_epoch() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    // ── Private ──

    fn session_lock(&self, seed: &str) -> Arc<Mutex<()>> {
        let mut locks = self.session_locks.lock().unwrap();
        locks
            .entry(seed.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    fn compact_context_path(&self, seed: &str) -> PathBuf {
        self.session_path_dir(seed).join("compact-context.json")
    }

    fn read_compact_context(&self, seed: &str) -> Option<CompactContext> {
        serde_json::from_str(&std::fs::read_to_string(self.compact_context_path(seed)).ok()?).ok()
    }

    fn write_compact_context(&self, seed: &str, context: &CompactContext) -> Result<(), String> {
        let path = self.compact_context_path(seed);
        std::fs::create_dir_all(self.session_path_dir(seed))
            .map_err(|error| format!("create compact context directory: {error}"))?;
        let temporary = path.with_extension("json.tmp");
        let data = serde_json::to_vec_pretty(context)
            .map_err(|error| format!("serialize compact context: {error}"))?;
        std::fs::write(&temporary, data)
            .map_err(|error| format!("write compact context: {error}"))?;
        std::fs::rename(&temporary, &path)
            .map_err(|error| format!("activate compact context: {error}"))
    }

    fn snapshot_from_files(&self, seed: &str) -> Result<(SessionMeta, Vec<Message>), String> {
        let dir = self
            .session_dir(seed)
            .ok_or_else(|| format!("session directory is missing: {seed}"))?;
        let meta = store::read_meta(&dir)
            .ok_or_else(|| format!("meta.json is missing or unreadable: {seed}"))?;
        let messages = read_messages_without_deduplication(&dir.join("messages.jsonl"))?;
        Ok((meta, messages))
    }

    fn session_path_dir(&self, seed: &str) -> PathBuf {
        self.sessions_dir.join(seed)
    }

    fn session_dir(&self, seed: &str) -> Option<PathBuf> {
        let dir = self.session_path_dir(seed);
        if dir.exists() && dir.is_dir() {
            Some(dir)
        } else {
            None
        }
    }

    fn extract_summary(messages: &[Message]) -> String {
        messages
            .iter()
            .rev()
            .find(|m| m.role == "assistant" && !m.content.is_empty())
            .and_then(|m| {
                m.content.iter().find_map(|b| {
                    if let deepx_types::ContentBlock::Text { text } = b {
                        Some(text.lines().next().unwrap_or(text))
                    } else {
                        None
                    }
                })
            })
            .map(|s| {
                if s.len() <= 80 {
                    return s.to_string();
                }
                let mut end = 80;
                while !s.is_char_boundary(end) {
                    end -= 1;
                }
                format!("{}..", &s[..end])
            })
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod skill_persistence_tests {
    use super::*;
    use deepx_types::{SkillSessionEntry, SkillSessionEntryState, SkillSessionStateV2};
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    static TEST_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

    fn manager() -> (PathBuf, SessionManager) {
        let root = std::env::temp_dir().join(format!(
            "deepx-session-skills-{}-{}-{}",
            std::process::id(),
            TEST_SEQUENCE.fetch_add(1, AtomicOrdering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos(),
        ));
        let sessions_dir = root.join("sessions");
        std::fs::create_dir_all(&sessions_dir).expect("create test sessions");
        let manager = SessionManager {
            sessions_dir,
            active_path: root.join(".active_session"),
            session_locks: Mutex::new(HashMap::new()),
        };
        (root, manager)
    }

    fn state() -> SkillSessionStateV2 {
        SkillSessionStateV2 {
            version: 2,
            context_epoch: 7,
            operation_revision: 9,
            entries: vec![SkillSessionEntry {
                name: "alpha".into(),
                activation_order: 1,
                source: "model".into(),
                state: SkillSessionEntryState::Active,
            }],
        }
    }

    #[test]
    fn file_only_new_session_is_immediately_listable_and_loadable() {
        let (root, manager) = manager();
        manager.persist_new_session("file-only");

        let listed = manager.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].seed, "file-only");
        let (meta, messages) = manager.load("file-only").expect("file snapshot");
        assert_eq!(meta.seed, "file-only");
        assert!(messages.is_empty());

        std::fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn metadata_rewrites_preserve_skill_session_state_v2() {
        let (root, manager) = manager();
        manager.persist_skills("seed", state());
        manager.update_meta("seed", "model", None, 0, 1);
        manager.save_full("seed", &[Message::user("hello")], "model", None, 0, 1);
        let meta = manager.load_meta("seed").expect("metadata");
        assert_eq!(meta.seed, "seed");
        assert_eq!(meta.skills, state());
        std::fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn compact_context_preserves_archive_and_restores_the_active_view() {
        let (root, manager) = manager();
        let archive = vec![
            Message::user("one"),
            Message::user("two"),
            Message::user("three"),
        ];
        manager.save_full("compact-seed", &archive, "model", None, 0, 2);
        let active = vec![
            Message::user("[Compacted 1 turns]\nsummary"),
            Message::user("three"),
        ];
        manager.save_compact_context("compact-seed", &active);

        let (_, restored_archive, context) =
            manager.load_for_resume("compact-seed").expect("resume");
        assert_eq!(
            restored_archive.len(),
            archive.len(),
            "raw archive must not be rewritten"
        );
        let context = context.expect("compact checkpoint");
        assert_eq!(context.messages.len(), active.len());
        assert_eq!(context.parent_checkpoint_id, None);
        std::fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn repeated_compact_links_checkpoints_without_losing_archive() {
        let (root, manager) = manager();
        let archive = vec![
            Message::user("one"),
            Message::user("two"),
            Message::user("three"),
        ];
        manager.save_full("multi-compact", &archive, "model", None, 0, 3);
        manager.save_compact_context("multi-compact", &[Message::user("[Compacted]\nfirst")]);
        let first = manager
            .read_compact_context("multi-compact")
            .expect("first checkpoint");
        manager.save_compact_context("multi-compact", &[Message::user("[Compacted]\nsecond")]);
        let second = manager
            .read_compact_context("multi-compact")
            .expect("second checkpoint");
        assert_eq!(
            second.parent_checkpoint_id.as_deref(),
            Some(first.checkpoint_id.as_str())
        );
        assert_eq!(
            manager.load("multi-compact").expect("archive").1.len(),
            archive.len()
        );
        std::fs::remove_dir_all(root).expect("remove test directory");
    }

}

