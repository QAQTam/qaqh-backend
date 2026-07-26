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
use std::sync::{Arc, OnceLock};

use deepx_types::{Message, SessionMeta};

use crate::mirror::{MirrorManifest, MirrorOutbox, MirrorSnapshot};
use crate::store;

#[cfg(feature = "turso-backend")]
use crate::store::turso_backend::TursoBackend;
#[cfg(feature = "turso-backend")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(feature = "turso-backend")]
use std::sync::Mutex;

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

/// Read-only comparison of a session's JSONL primary data and Turso mirror.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MirrorAudit {
    pub seed: String,
    pub consistent: bool,
    pub jsonl_exists: bool,
    pub database_exists: bool,
    pub jsonl_message_count: Option<usize>,
    pub database_message_count: Option<usize>,
    pub metadata_matches: Option<bool>,
    pub manifest_matches: Option<bool>,
    pub file_revision: Option<u64>,
    pub database_revision: Option<u64>,
    pub outbox_exists: bool,
    pub message_mismatch_indices: Vec<usize>,
    pub errors: Vec<String>,
}

/// Explicit safety decision for a future DB-primary rollout.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DbPrimaryReadiness {
    pub ready: bool,
    pub sessions: Vec<MirrorAudit>,
    pub pending_outboxes: Vec<String>,
    pub reasons: Vec<String>,
}

/// One session's result from an explicit JSONL → Turso reconciliation run.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MigrationOutcome {
    pub seed: String,
    pub status: String,
    pub messages: usize,
    pub reason: Option<String>,
}

/// Structured migration result for UI feedback. Per-session failures do not
/// discard successful migrations, and are always surfaced to the caller.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MigrationReport {
    pub sessions: usize,
    pub messages: usize,
    pub failed: usize,
    pub outcomes: Vec<MigrationOutcome>,
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
    #[cfg(feature = "turso-backend")]
    turso_enabled: AtomicBool,
    #[cfg(feature = "turso-backend")]
    dbs: Mutex<HashMap<String, TursoBackend>>,
}

impl SessionManager {
    /// Initialize the global singleton. Must be called once at startup.
    /// Also triggers automatic migration from legacy TOML format if needed.
    /// When `turso_enabled` is true, per-session SQLite databases are created
    /// at `{sessions_dir}/{seed}/sessions.db`.
    pub fn init(data_dir: PathBuf, turso_enabled: bool) {
        let sessions_dir = data_dir.join("sessions");
        let _ = std::fs::create_dir_all(&sessions_dir);

        #[cfg(feature = "turso-backend")]
        log::info!(
            "SessionManager: Turso mirroring {} (per-session at {})",
            if turso_enabled { "ENABLED" } else { "DISABLED" },
            sessions_dir.join("<seed>").join("sessions.db").display()
        );

        let mgr = Self {
            active_path: data_dir.join(".active_session"),
            session_locks: Mutex::new(HashMap::new()),
            sessions_dir,
            #[cfg(feature = "turso-backend")]
            turso_enabled: AtomicBool::new(turso_enabled),
            #[cfg(feature = "turso-backend")]
            dbs: Mutex::new(HashMap::new()),
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

    /// Toggle Turso mirroring at runtime (no restart needed).
    #[cfg(feature = "turso-backend")]
    pub fn set_turso_enabled(&self, enabled: bool) {
        let old = self.turso_enabled.load(Ordering::Relaxed);
        self.turso_enabled.store(enabled, Ordering::Relaxed);
        log::info!(
            "SessionManager: Turso mirroring {} -> {}",
            if old { "ENABLED" } else { "DISABLED" },
            if enabled { "ENABLED" } else { "DISABLED" },
        );
    }

    // ── Session listing ──

    /// List all sessions sorted by updated_at descending.
    /// Index-first; fallback scans JSON metadata, with Turso recovery only when
    /// the JSON metadata is absent.
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
                    let seed = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    let meta = store::read_meta(&path).or_else(|| {
                        #[cfg(feature = "turso-backend")]
                        if self.turso_enabled.load(Ordering::Relaxed) {
                            if let Some(dbs) = self.get_or_open_db(seed) {
                                if let Some(db) = dbs.get(seed) {
                                    if let Ok(Some(m)) = db.load_meta(seed) {
                                        Some(m)
                                    } else {
                                        None
                                    }
                                } else {
                                    None
                                }
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                        #[cfg(not(feature = "turso-backend"))]
                        None
                    });
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

        #[cfg(feature = "turso-backend")]
        {
            self.dbs.lock().unwrap().remove(seed);
        }

        log::info!("SessionManager: deleted session {seed}");
        Ok(())
    }

    // ── Load / Save ──

    /// Read both persistence channels and resume from the newest verified
    /// snapshot. A newer channel repairs the older channel before returning.
    pub fn load(&self, seed: &str) -> Option<(SessionMeta, Vec<Message>)> {
        let file = self.snapshot_from_files(seed).ok();
        #[cfg(feature = "turso-backend")]
        if self.turso_enabled.load(Ordering::Relaxed) {
            let mut candidates: Vec<(u64, u8, MirrorSnapshot)> = Vec::new();
            let database = self.read_database_snapshot(seed);
            if let Some(outbox) = self.read_outbox(seed) {
                candidates.push((outbox.manifest.revision, 3, outbox.snapshot));
            }
            if let Some(snapshot) = file.clone() {
                let revision = self.read_file_manifest(seed).and_then(|manifest| {
                    (snapshot.manifest(manifest.revision).ok().as_ref() == Some(&manifest))
                        .then_some(manifest.revision)
                });
                // A changed JSONL file without a matching manifest is not a
                // verified newer revision. Only use it when DB recovery is
                // unavailable, preserving legacy file-only sessions.
                if let Some(revision) = revision {
                    candidates.push((revision, 2, snapshot));
                } else if database.is_none() {
                    candidates.push((0, 2, snapshot));
                }
            }
            if let Some((snapshot, manifest)) = database {
                candidates.push((manifest.revision, 1, snapshot));
            }
            candidates.sort_by(|left, right| left.0.cmp(&right.0).then(left.1.cmp(&right.1)));
            if let Some((_, source, snapshot)) = candidates.pop() {
                if source != 2 {
                    if let Err(error) = self.restore_file_snapshot(seed, &snapshot) {
                        log::warn!(
                            "SessionManager: restore newer mirror for {seed} failed: {error}"
                        );
                    }
                }
                if source != 1 {
                    self.queue_and_sync_mirror(seed);
                }
                return Some((snapshot.meta, snapshot.messages));
            }
            // Legacy DB-only sessions predate manifests. Preserve the previous
            // recovery behavior; the next successful write upgrades them.
            if let Some(dbs) = self.get_or_open_db(seed) {
                if let Some(db) = dbs.get(seed) {
                    if let (Ok(Some(meta)), Ok(messages)) =
                        (db.load_meta(seed), db.load_messages(seed))
                    {
                        return Some((meta, messages));
                    }
                }
            }
        }
        file.map(|snapshot| (snapshot.meta, snapshot.messages))
    }

    /// Load the immutable archive plus the latest compact context, if one
    /// exists.  Callers must use `active_messages` for the model loop and
    /// retain `archive_messages` for replay/pagination.
    pub fn load_for_resume(
        &self,
        seed: &str,
    ) -> Option<(SessionMeta, Vec<Message>, Option<CompactContext>)> {
        let (meta, archive_messages) = self.load(seed)?;
        let file = self.read_compact_context(seed);
        #[cfg(feature = "turso-backend")]
        let database = if self.turso_enabled.load(Ordering::Relaxed) {
            self.get_or_open_db(seed).and_then(|dbs| {
                dbs.get(seed)
                    .and_then(|db| db.load_compact_context(seed).ok().flatten())
            })
        } else {
            None
        };
        #[cfg(not(feature = "turso-backend"))]
        let database: Option<CompactContext> = None;

        let selected = match (file, database) {
            (Some(file), Some(database)) if database.created_at > file.created_at => {
                let _ = self.write_compact_context(seed, &database);
                Some(database)
            }
            (Some(file), Some(database)) => {
                #[cfg(feature = "turso-backend")]
                if serde_json::to_string(&file).ok() != serde_json::to_string(&database).ok() {
                    if let Some(dbs) = self.get_or_open_db(seed) {
                        if let Some(db) = dbs.get(seed) {
                            let _ = db.save_compact_context(seed, &file);
                        }
                    }
                }
                Some(file)
            }
            (Some(file), None) => {
                #[cfg(feature = "turso-backend")]
                if let Some(dbs) = self.get_or_open_db(seed) {
                    if let Some(db) = dbs.get(seed) {
                        let _ = db.save_compact_context(seed, &file);
                    }
                }
                Some(file)
            }
            (None, Some(database)) => {
                let _ = self.write_compact_context(seed, &database);
                Some(database)
            }
            (None, None) => None,
        };
        let selected =
            selected.filter(|context| context.archive_message_count <= archive_messages.len());
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
        #[cfg(feature = "turso-backend")]
        if self.turso_enabled.load(Ordering::Relaxed) {
            if let Some(dbs) = self.get_or_open_db(seed) {
                if let Some(db) = dbs.get(seed) {
                    if let Err(error) = db.save_compact_context(seed, &context) {
                        log::error!(
                            "SessionManager: mirror compact context failed for {seed}: {error}"
                        );
                    }
                }
            }
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
        #[cfg(feature = "turso-backend")]
        if self.turso_enabled.load(Ordering::Relaxed) {
            if let Some(dbs) = self.get_or_open_db(seed) {
                if let Some(db) = dbs.get(seed) {
                    if let Err(error) = db.save_compact_context(seed, &context) {
                        log::error!("SessionManager: mirror compact context update failed for {seed}: {error}");
                    }
                }
            }
        }
    }

    /// Check whether a session exists (directory on disk or Turso DB).
    pub fn exists(&self, seed: &str) -> bool {
        if self.session_dir(seed).is_some() {
            return true;
        }
        #[cfg(feature = "turso-backend")]
        if self.turso_enabled.load(Ordering::Relaxed) {
            return self.is_turso_backed(seed);
        }
        false
    }

    /// Check whether this session has messages in the Turso SQLite store.
    pub fn is_turso_backed(&self, seed: &str) -> bool {
        #[cfg(feature = "turso-backend")]
        if self.turso_enabled.load(Ordering::Relaxed) {
            if let Some(dbs) = self.get_or_open_db(seed) {
                if let Some(db) = dbs.get(seed) {
                    return db.message_count(seed).unwrap_or(0) > 0;
                }
            }
        }
        let _ = seed;
        false
    }

    /// Compare the JSONL primary data with an existing Turso mirror without
    /// creating, mutating, or repairing either side.
    pub fn audit_mirror(&self, seed: &str) -> MirrorAudit {
        let dir = self.session_path_dir(seed);
        let jsonl_path = dir.join("messages.jsonl");
        let db_path = dir.join("sessions.db");
        let jsonl_exists = jsonl_path.exists();
        let database_exists = db_path.exists();
        let outbox_exists = self.outbox_path(seed).exists();
        let mut errors = Vec::new();
        let file_manifest = self.read_file_manifest(seed);

        let jsonl_meta = store::read_meta(&dir);
        let jsonl_messages = if jsonl_exists {
            match read_messages_without_deduplication(&jsonl_path) {
                Ok(messages) => Some(messages),
                Err(error) => {
                    errors.push(error);
                    None
                }
            }
        } else {
            errors.push("messages.jsonl is missing".into());
            None
        };

        #[cfg(feature = "turso-backend")]
        let (database_meta, database_messages, database_manifest) = if database_exists {
            match TursoBackend::open(&db_path) {
                Ok(db) => {
                    let meta = match db.load_meta(seed) {
                        Ok(meta) => meta,
                        Err(error) => {
                            errors.push(format!("database metadata: {error}"));
                            None
                        }
                    };
                    let messages = match db.load_messages(seed) {
                        Ok(messages) => Some(messages),
                        Err(error) => {
                            errors.push(format!("database messages: {error}"));
                            None
                        }
                    };
                    let manifest = match db.load_manifest(seed) {
                        Ok(manifest) => manifest,
                        Err(error) => {
                            errors.push(format!("database manifest: {error}"));
                            None
                        }
                    };
                    (meta, messages, manifest)
                }
                Err(error) => {
                    errors.push(format!("open database: {error}"));
                    (None, None, None)
                }
            }
        } else {
            errors.push("sessions.db is missing".into());
            (None, None, None)
        };

        #[cfg(not(feature = "turso-backend"))]
        let (database_meta, database_messages, database_manifest): (
            Option<SessionMeta>,
            Option<Vec<Message>>,
            Option<MirrorManifest>,
        ) = {
            errors.push("Turso backend is not compiled".into());
            (None, None, None)
        };

        let metadata_matches = match (&jsonl_meta, &database_meta) {
            (Some(jsonl), Some(database)) => {
                Some(serde_json::to_string(jsonl).ok() == serde_json::to_string(database).ok())
            }
            _ => None,
        };
        let file_manifest_matches_data = match (&jsonl_meta, &jsonl_messages, &file_manifest) {
            (Some(meta), Some(messages), Some(manifest)) => {
                MirrorSnapshot::new(meta.clone(), messages.clone())
                    .manifest(manifest.revision)
                    .map(|computed| computed == *manifest)
                    .unwrap_or(false)
            }
            _ => false,
        };
        let manifest_matches = match (&file_manifest, &database_manifest) {
            (Some(jsonl), Some(database)) => Some(
                jsonl.schema_version == database.schema_version
                    && jsonl.meta_sha256 == database.meta_sha256
                    && jsonl.messages_sha256 == database.messages_sha256,
            ),
            _ => None,
        };
        if jsonl_meta.is_none() {
            errors.push("meta.json is missing or unreadable".into());
        }
        if database_meta.is_none() {
            errors.push("database metadata is missing or unreadable".into());
        }
        if database_manifest.is_none() {
            errors.push("database manifest is missing or unreadable".into());
        }
        if file_manifest.is_none() {
            errors.push("file mirror manifest is missing or unreadable".into());
        } else if !file_manifest_matches_data {
            errors.push("file mirror manifest does not match JSONL snapshot".into());
        }
        if outbox_exists {
            errors.push("durable mirror outbox is pending reconciliation".into());
        }

        let mut message_mismatch_indices = Vec::new();
        if let (Some(jsonl), Some(database)) = (&jsonl_messages, &database_messages) {
            let shared = jsonl.len().min(database.len());
            for index in 0..shared {
                if serde_json::to_string(&jsonl[index]).ok()
                    != serde_json::to_string(&database[index]).ok()
                {
                    message_mismatch_indices.push(index);
                }
            }
            message_mismatch_indices.extend(shared..jsonl.len().max(database.len()));
        }

        let jsonl_message_count = jsonl_messages.as_ref().map(Vec::len);
        let database_message_count = database_messages.as_ref().map(Vec::len);
        let consistent = errors.is_empty()
            && metadata_matches == Some(true)
            && manifest_matches == Some(true)
            && message_mismatch_indices.is_empty()
            && jsonl_message_count == database_message_count;

        MirrorAudit {
            seed: seed.to_string(),
            consistent,
            jsonl_exists,
            database_exists,
            jsonl_message_count,
            database_message_count,
            metadata_matches,
            manifest_matches,
            file_revision: file_manifest.as_ref().map(|manifest| manifest.revision),
            database_revision: database_manifest.as_ref().map(|manifest| manifest.revision),
            outbox_exists,
            message_mismatch_indices,
            errors,
        }
    }

    /// Audit every session directory without mutating either persistence channel.
    pub fn audit_all_mirrors(&self) -> Vec<MirrorAudit> {
        let mut audits = std::fs::read_dir(&self.sessions_dir)
            .ok()
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                path.is_dir()
                    .then(|| entry.file_name().to_str().map(str::to_owned))
                    .flatten()
            })
            .map(|seed| self.audit_mirror(&seed))
            .collect::<Vec<_>>();
        audits.sort_by(|left, right| left.seed.cmp(&right.seed));
        audits
    }

    /// Replay a durable file outbox, or snapshot the current JSONL session.
    /// This is the only path that updates the versioned Turso manifest.
    #[cfg(feature = "turso-backend")]
    pub fn reconcile_mirror(&self, seed: &str) -> Result<(), String> {
        if !self.turso_enabled.load(Ordering::Relaxed) {
            return Err("Turso is disabled in settings".into());
        }
        let outbox_path = self.outbox_path(seed);
        let outbox = if outbox_path.exists() {
            let content = std::fs::read_to_string(&outbox_path)
                .map_err(|error| format!("read mirror outbox: {error}"))?;
            serde_json::from_str::<MirrorOutbox>(&content)
                .map_err(|error| format!("parse mirror outbox: {error}"))?
        } else {
            let snapshot = self.snapshot_from_files(seed)?;
            let file_revision = self
                .read_file_manifest(seed)
                .filter(|manifest| {
                    snapshot.manifest(manifest.revision).ok().as_ref() == Some(manifest)
                })
                .map(|manifest| manifest.revision)
                .unwrap_or(0);
            let revision = file_revision
                .max(self.database_revision(seed).unwrap_or(0))
                .saturating_add(1);
            MirrorOutbox {
                manifest: snapshot.manifest(revision)?,
                snapshot,
            }
        };

        self.write_file_manifest(seed, &outbox.manifest)?;
        self.write_outbox(seed, &outbox)?;
        let dbs = self
            .get_or_open_db(seed)
            .ok_or_else(|| "open Turso mirror".to_string())?;
        let db = dbs
            .get(seed)
            .ok_or_else(|| "missing Turso mirror".to_string())?;
        db.replace_snapshot(&outbox.snapshot, &outbox.manifest)?;
        drop(dbs);
        std::fs::remove_file(&outbox_path)
            .map_err(|error| format!("remove mirror outbox: {error}"))?;
        Ok(())
    }

    #[cfg(feature = "turso-backend")]
    pub fn reconcile_all_mirrors(&self) -> Vec<(String, Result<(), String>)> {
        self.audit_all_mirrors()
            .into_iter()
            .map(|audit| {
                let seed = audit.seed;
                let result = self.reconcile_mirror(&seed);
                (seed, result)
            })
            .collect()
    }

    /// Returns the non-mutating safety gate for a future DB-primary switch.
    pub fn db_primary_readiness(&self) -> DbPrimaryReadiness {
        let sessions = self.audit_all_mirrors();
        let pending_outboxes = sessions
            .iter()
            .filter(|audit| audit.outbox_exists)
            .map(|audit| audit.seed.clone())
            .collect::<Vec<_>>();
        let mut reasons = Vec::new();
        if sessions.is_empty() {
            reasons.push("no sessions were audited".into());
        }
        if !pending_outboxes.is_empty() {
            reasons.push("durable mirror outboxes are pending".into());
        }
        if sessions.iter().any(|audit| !audit.consistent) {
            reasons.push("one or more session mirrors are inconsistent".into());
        }
        DbPrimaryReadiness {
            ready: reasons.is_empty(),
            sessions,
            pending_outboxes,
            reasons,
        }
    }

    /// Count sessions that have JSONL data but not yet migrated to Turso.
    pub fn count_pending_migration(&self) -> usize {
        #[cfg(feature = "turso-backend")]
        if self.turso_enabled.load(Ordering::Relaxed) {
            return self
                .audit_all_mirrors()
                .into_iter()
                .filter(|audit| audit.jsonl_exists && !audit.consistent)
                .count();
        }
        0
    }

    /// Migrate all pending sessions from JSONL to Turso.
    /// Reconcile every JSONL session into Turso and report each result.
    /// A stale, incomplete, or missing database is migration-pending.
    pub fn migrate_all_to_turso(&self) -> Result<MigrationReport, String> {
        #[cfg(not(feature = "turso-backend"))]
        return Err("Turso backend not compiled".into());

        #[cfg(feature = "turso-backend")]
        {
            if !self.turso_enabled.load(Ordering::Relaxed) {
                return Err("Turso is disabled in settings".into());
            }
            let mut outcomes = Vec::new();
            for audit in self.audit_all_mirrors() {
                if !audit.jsonl_exists || audit.consistent {
                    continue;
                }
                let messages = audit.jsonl_message_count.unwrap_or(0);
                let status = if audit.database_exists {
                    "repaired"
                } else {
                    "migrated"
                };
                match self.reconcile_mirror(&audit.seed) {
                    Ok(()) => {
                        let verified = self.audit_mirror(&audit.seed);
                        if verified.consistent {
                            outcomes.push(MigrationOutcome {
                                seed: audit.seed,
                                status: status.into(),
                                messages,
                                reason: None,
                            });
                        } else {
                            outcomes.push(MigrationOutcome {
                                seed: audit.seed,
                                status: "failed".into(),
                                messages,
                                reason: Some(verified.errors.join("; ")),
                            });
                        }
                    }
                    Err(error) => outcomes.push(MigrationOutcome {
                        seed: audit.seed,
                        status: "failed".into(),
                        messages,
                        reason: Some(error),
                    }),
                }
            }
            let sessions = outcomes
                .iter()
                .filter(|outcome| outcome.status != "failed")
                .count();
            let messages = outcomes
                .iter()
                .filter(|outcome| outcome.status != "failed")
                .map(|outcome| outcome.messages)
                .sum();
            let failed = outcomes
                .iter()
                .filter(|outcome| outcome.status == "failed")
                .count();
            Ok(MigrationReport {
                sessions,
                messages,
                failed,
                outcomes,
            })
        }
    }

    /// Load only metadata (fast, no message parsing). JSON remains primary
    /// until the DB-primary readiness gate is explicitly promoted.
    pub fn load_meta(&self, seed: &str) -> Option<SessionMeta> {
        if let Some(dir) = self.session_dir(seed) {
            if let Some(meta) = store::read_meta(&dir) {
                return Some(meta);
            }
        }
        #[cfg(feature = "turso-backend")]
        if self.turso_enabled.load(Ordering::Relaxed) {
            if let Some(dbs) = self.get_or_open_db(seed) {
                if let Some(db) = dbs.get(seed) {
                    match db.load_meta(seed) {
                        Ok(Some(meta)) => return Some(meta),
                        Ok(None) => {} // fall through to JSON
                        Err(e) => log::warn!("Turso load_meta failed for {seed}: {e}"),
                    }
                }
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
        #[cfg(feature = "turso-backend")]
        self.queue_and_sync_mirror(seed);
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
        #[cfg(feature = "turso-backend")]
        self.queue_and_sync_mirror(seed);
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
        #[cfg(feature = "turso-backend")]
        self.prepare_outbox_before_file_write(
            seed,
            MirrorSnapshot::new(meta.clone(), target_messages),
        );
        if let Err(e) = store::append_one(&dir, msg) {
            log::error!("SessionManager: save_one failed: {e}");
            return;
        }
        if let Err(e) = store::write_meta(&dir, &meta) {
            log::error!("SessionManager: save_one metadata write failed: {e}");
            return;
        }
        store::upsert_index(&self.sessions_dir, &meta);
        #[cfg(feature = "turso-backend")]
        self.queue_and_sync_mirror(seed);
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
            ..Default::default()
        };
        #[cfg(feature = "turso-backend")]
        self.prepare_outbox_before_file_write(
            seed,
            MirrorSnapshot::new(meta.clone(), store::read_messages(&dir).unwrap_or_default()),
        );
        if let Err(e) = store::write_meta(&dir, &meta) {
            log::error!("SessionManager: write_meta failed: {e}");
            return;
        }
        store::upsert_index(&self.sessions_dir, &meta);

        #[cfg(feature = "turso-backend")]
        self.queue_and_sync_mirror(seed);
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

        #[cfg(feature = "turso-backend")]
        self.prepare_outbox_before_file_write(
            seed,
            MirrorSnapshot::new(meta.clone(), messages.to_vec()),
        );

        if let Err(e) = store::rewrite_messages(&dir, messages) {
            log::error!("SessionManager: rewrite_messages failed: {e}");
            return;
        }
        if let Err(e) = store::write_meta(&dir, &meta) {
            log::error!("SessionManager: write_meta failed: {e}");
            return;
        }
        store::upsert_index(&self.sessions_dir, &meta);

        #[cfg(feature = "turso-backend")]
        self.queue_and_sync_mirror(seed);
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
        #[cfg(feature = "turso-backend")]
        self.prepare_outbox_before_file_write(
            seed,
            MirrorSnapshot::new(meta.clone(), target_messages),
        );

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

        #[cfg(feature = "turso-backend")]
        self.queue_and_sync_mirror(seed);
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
        #[cfg(feature = "turso-backend")]
        self.queue_and_sync_mirror(seed);
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

    fn outbox_path(&self, seed: &str) -> PathBuf {
        self.session_path_dir(seed).join(".mirror-outbox.json")
    }

    fn file_manifest_path(&self, seed: &str) -> PathBuf {
        self.session_path_dir(seed).join(".mirror-state.json")
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

    fn read_file_manifest(&self, seed: &str) -> Option<MirrorManifest> {
        serde_json::from_str(&std::fs::read_to_string(self.file_manifest_path(seed)).ok()?).ok()
    }

    fn write_file_manifest(&self, seed: &str, manifest: &MirrorManifest) -> Result<(), String> {
        let path = self.file_manifest_path(seed);
        let temporary = path.with_extension("json.tmp");
        let json = serde_json::to_vec_pretty(manifest)
            .map_err(|error| format!("serialize file mirror manifest: {error}"))?;
        std::fs::write(&temporary, json)
            .map_err(|error| format!("write file mirror manifest: {error}"))?;
        std::fs::rename(&temporary, &path)
            .map_err(|error| format!("activate file mirror manifest: {error}"))
    }

    fn read_outbox(&self, seed: &str) -> Option<MirrorOutbox> {
        serde_json::from_str(&std::fs::read_to_string(self.outbox_path(seed)).ok()?).ok()
    }

    fn restore_file_snapshot(&self, seed: &str, snapshot: &MirrorSnapshot) -> Result<(), String> {
        let dir = self.session_path_dir(seed);
        std::fs::create_dir_all(&dir)
            .map_err(|error| format!("create session directory: {error}"))?;
        store::rewrite_messages(&dir, &snapshot.messages)?;
        store::write_meta(&dir, &snapshot.meta)?;
        store::upsert_index(&self.sessions_dir, &snapshot.meta);
        let revision = self
            .read_file_manifest(seed)
            .map(|manifest| manifest.revision)
            .unwrap_or(0)
            .saturating_add(1);
        self.write_file_manifest(seed, &snapshot.manifest(revision)?)
    }

    fn snapshot_from_files(&self, seed: &str) -> Result<MirrorSnapshot, String> {
        let dir = self
            .session_dir(seed)
            .ok_or_else(|| format!("session directory is missing: {seed}"))?;
        let meta = store::read_meta(&dir)
            .ok_or_else(|| format!("meta.json is missing or unreadable: {seed}"))?;
        let messages = read_messages_without_deduplication(&dir.join("messages.jsonl"))?;
        Ok(MirrorSnapshot::new(meta, messages))
    }

    fn write_outbox(&self, seed: &str, outbox: &MirrorOutbox) -> Result<(), String> {
        let path = self.outbox_path(seed);
        let temporary = path.with_extension("json.tmp");
        let json = serde_json::to_vec_pretty(outbox)
            .map_err(|error| format!("serialize mirror outbox: {error}"))?;
        std::fs::write(&temporary, json)
            .map_err(|error| format!("write mirror outbox: {error}"))?;
        std::fs::rename(&temporary, &path)
            .map_err(|error| format!("activate mirror outbox: {error}"))
    }

    #[cfg(feature = "turso-backend")]
    fn prepare_outbox_before_file_write(&self, seed: &str, snapshot: MirrorSnapshot) {
        if !self.turso_enabled.load(Ordering::Relaxed) {
            return;
        }
        let revision = self
            .read_file_manifest(seed)
            .map(|manifest| manifest.revision)
            .unwrap_or(0)
            .max(self.database_revision(seed).unwrap_or(0))
            .saturating_add(1);
        match snapshot.manifest(revision) {
            Ok(manifest) => {
                if let Err(error) = self.write_outbox(seed, &MirrorOutbox { manifest, snapshot }) {
                    log::error!("SessionManager: durable outbox write failed before file write for {seed}: {error}");
                }
            }
            Err(error) => {
                log::error!("SessionManager: create outbox manifest failed for {seed}: {error}")
            }
        }
    }

    #[cfg(feature = "turso-backend")]
    fn database_revision(&self, seed: &str) -> Option<u64> {
        let path = self.session_path_dir(seed).join("sessions.db");
        if !path.exists() {
            return None;
        }
        TursoBackend::open(path)
            .ok()?
            .load_manifest(seed)
            .ok()
            .flatten()
            .map(|manifest| manifest.revision)
    }

    #[cfg(feature = "turso-backend")]
    fn read_database_snapshot(&self, seed: &str) -> Option<(MirrorSnapshot, MirrorManifest)> {
        let path = self.session_path_dir(seed).join("sessions.db");
        let db = TursoBackend::open(path).ok()?;
        let meta = db.load_meta(seed).ok()??;
        let messages = db.load_messages(seed).ok()?;
        let snapshot = MirrorSnapshot::new(meta, messages);
        let manifest = db
            .load_manifest(seed)
            .ok()?
            .unwrap_or(snapshot.manifest(0).ok()?);
        if manifest.revision == 0 || snapshot.manifest(manifest.revision).ok()? == manifest {
            Some((snapshot, manifest))
        } else {
            None
        }
    }

    #[cfg(feature = "turso-backend")]
    fn queue_and_sync_mirror(&self, seed: &str) {
        if !self.turso_enabled.load(Ordering::Relaxed) {
            if let Ok(snapshot) = self.snapshot_from_files(seed) {
                let revision = self
                    .read_file_manifest(seed)
                    .map(|manifest| manifest.revision)
                    .unwrap_or(0)
                    .max(self.database_revision(seed).unwrap_or(0))
                    .saturating_add(1);
                if let Ok(manifest) = snapshot.manifest(revision) {
                    if let Err(error) = self.write_file_manifest(seed, &manifest) {
                        log::warn!(
                            "SessionManager: write file-only manifest for {seed} failed: {error}"
                        );
                    }
                }
            }
            return;
        }
        if let Err(error) = self.reconcile_mirror(seed) {
            log::warn!("SessionManager: queued Turso mirror reconciliation for {seed}: {error}");
        }
    }

    #[cfg(feature = "turso-backend")]
    fn get_or_open_db(
        &self,
        seed: &str,
    ) -> Option<std::sync::MutexGuard<'_, HashMap<String, TursoBackend>>> {
        if !self.turso_enabled.load(Ordering::Relaxed) {
            return None;
        }
        let mut dbs = self.dbs.lock().unwrap();
        if !dbs.contains_key(seed) {
            let dir = self.session_path_dir(seed);
            let _ = std::fs::create_dir_all(&dir);
            let path = dir.join("sessions.db");
            match TursoBackend::open(&path) {
                Ok(db) => {
                    log::info!("SessionManager: Turso backend at {}", path.display());
                    if let Err(e) = db.init_tables() {
                        log::warn!("SessionManager: Turso init_tables failed: {e}");
                    }
                    dbs.insert(seed.to_string(), db);
                }
                Err(e) => {
                    log::warn!("SessionManager: Turso open failed for {seed}: {e}");
                    return None;
                }
            }
        }
        Some(dbs)
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
            #[cfg(feature = "turso-backend")]
            turso_enabled: AtomicBool::new(false),
            #[cfg(feature = "turso-backend")]
            dbs: Mutex::new(HashMap::new()),
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
                lease_remaining: 2,
            }],
        }
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

    #[cfg(feature = "turso-backend")]
    #[test]
    fn compact_context_preserves_archive_and_restores_the_active_view() {
        let (root, manager) = manager();
        manager.turso_enabled.store(true, Ordering::Relaxed);
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
        let dbs = manager.get_or_open_db("compact-seed").expect("database");
        assert!(dbs
            .get("compact-seed")
            .expect("backend")
            .load_compact_context("compact-seed")
            .expect("load context")
            .is_some());
        std::fs::remove_dir_all(root).expect("remove test directory");
    }

    #[cfg(feature = "turso-backend")]
    #[test]
    fn repeated_compact_links_checkpoints_without_losing_archive() {
        let (root, manager) = manager();
        manager.turso_enabled.store(true, Ordering::Relaxed);
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

    #[cfg(feature = "turso-backend")]
    #[test]
    fn load_recovers_a_session_from_turso_when_json_metadata_is_missing() {
        let (root, manager) = manager();
        manager.turso_enabled.store(true, Ordering::Relaxed);

        let meta = SessionMeta {
            seed: "db-only".into(),
            ..Default::default()
        };
        let message = Message::user("persisted only in Turso");

        {
            let dbs = manager
                .get_or_open_db(&meta.seed)
                .expect("open Turso backend");
            let db = dbs.get(&meta.seed).expect("session backend");
            db.upsert_meta(&meta.seed, &meta).expect("write metadata");
            db.insert_message(&meta.seed, &message)
                .expect("write message");
        }

        let (loaded_meta, loaded_messages) = manager.load(&meta.seed).expect("restore from Turso");
        assert_eq!(loaded_meta.seed, meta.seed);
        assert_eq!(loaded_messages.len(), 1);
        std::fs::remove_dir_all(root).expect("remove test directory");
    }

    #[cfg(feature = "turso-backend")]
    #[test]
    fn mirror_audit_reports_a_database_message_mismatch() {
        let (root, manager) = manager();
        manager.turso_enabled.store(true, Ordering::Relaxed);
        manager.save_full(
            "audit-seed",
            &[Message::user("from JSONL")],
            "model",
            None,
            0,
            1,
        );

        {
            let dbs = manager
                .get_or_open_db("audit-seed")
                .expect("open Turso backend");
            dbs.get("audit-seed")
                .expect("session backend")
                .rewrite_messages("audit-seed", &[Message::user("from database")])
                .expect("rewrite database messages");
        }

        let audit = manager.audit_mirror("audit-seed");
        assert!(!audit.consistent);
        assert_eq!(audit.jsonl_message_count, Some(1));
        assert_eq!(audit.database_message_count, Some(1));
        assert_eq!(audit.message_mismatch_indices, vec![0]);
        std::fs::remove_dir_all(root).expect("remove test directory");
    }

    #[cfg(feature = "turso-backend")]
    #[test]
    fn reconciliation_replays_a_durable_outbox_and_unblocks_readiness() {
        let (root, manager) = manager();
        manager.turso_enabled.store(true, Ordering::Relaxed);
        manager.save_full(
            "outbox-seed",
            &[Message::user("authoritative")],
            "model",
            None,
            0,
            1,
        );

        let snapshot = manager
            .snapshot_from_files("outbox-seed")
            .expect("file snapshot");
        let outbox = MirrorOutbox {
            manifest: snapshot.manifest(99).expect("manifest"),
            snapshot,
        };
        manager
            .write_outbox("outbox-seed", &outbox)
            .expect("durable outbox");

        let readiness = manager.db_primary_readiness();
        assert!(!readiness.ready);
        assert_eq!(readiness.pending_outboxes, vec!["outbox-seed"]);

        manager
            .reconcile_mirror("outbox-seed")
            .expect("replay outbox");
        assert!(!manager.outbox_path("outbox-seed").exists());
        assert!(manager.audit_mirror("outbox-seed").consistent);
        assert!(manager.db_primary_readiness().ready);
        std::fs::remove_dir_all(root).expect("remove test directory");
    }

    #[cfg(feature = "turso-backend")]
    #[test]
    fn migration_repairs_a_stale_database_instead_of_skipping_it() {
        let (root, manager) = manager();
        manager.turso_enabled.store(true, Ordering::Relaxed);
        manager.save_full(
            "stale-seed",
            &[Message::user("file version")],
            "model",
            None,
            0,
            1,
        );
        {
            let dbs = manager.get_or_open_db("stale-seed").expect("open database");
            dbs.get("stale-seed")
                .expect("database")
                .rewrite_messages("stale-seed", &[Message::user("stale database")])
                .expect("make database stale");
        }
        assert_eq!(manager.count_pending_migration(), 1);
        let report = manager.migrate_all_to_turso().expect("repair migration");
        assert_eq!(report.sessions, 1);
        assert_eq!(report.failed, 0);
        assert_eq!(report.outcomes[0].status, "repaired");
        assert!(manager.audit_mirror("stale-seed").consistent);
        std::fs::remove_dir_all(root).expect("remove test directory");
    }

    #[cfg(feature = "turso-backend")]
    #[test]
    fn load_uses_a_newer_verified_database_snapshot_and_repairs_jsonl() {
        let (root, manager) = manager();
        manager.turso_enabled.store(true, Ordering::Relaxed);
        manager.save_full(
            "newest-seed",
            &[Message::user("file version")],
            "model",
            None,
            0,
            1,
        );
        let mut meta = manager.load_meta("newest-seed").expect("file metadata");
        meta.last_summary = "database version".into();
        let snapshot = MirrorSnapshot::new(meta, vec![Message::user("database version")]);
        let manifest = snapshot
            .manifest(manager.database_revision("newest-seed").unwrap_or(0) + 1)
            .expect("database manifest");
        {
            let dbs = manager
                .get_or_open_db("newest-seed")
                .expect("open database");
            dbs.get("newest-seed")
                .expect("database")
                .replace_snapshot(&snapshot, &manifest)
                .expect("make database newest");
        }

        let (loaded_meta, loaded_messages) =
            manager.load("newest-seed").expect("load newest snapshot");
        assert_eq!(loaded_meta.last_summary, "database version");
        assert_eq!(loaded_messages.len(), 1);
        let audit = manager.audit_mirror("newest-seed");
        assert!(audit.consistent, "{audit:?}");
        std::fs::remove_dir_all(root).expect("remove test directory");
    }

    #[cfg(feature = "turso-backend")]
    #[test]
    fn simulation_matrix_reports_realistic_recovery_outcomes() {
        let (root, manager) = manager();
        manager.turso_enabled.store(true, Ordering::Relaxed);

        std::thread::scope(|scope| {
            for number in 0..4 {
                let manager = &manager;
                scope.spawn(move || {
                    manager.save_full(
                        &format!("subagent-{number}"),
                        &[Message::user(&format!("message-{number}"))],
                        "model",
                        None,
                        0,
                        1,
                    )
                });
            }
        });
        let parallel_ok = manager
            .audit_all_mirrors()
            .iter()
            .filter(|audit| audit.seed.starts_with("subagent-"))
            .count()
            == 4
            && manager
                .audit_all_mirrors()
                .iter()
                .filter(|audit| audit.seed.starts_with("subagent-"))
                .all(|audit| audit.consistent);

        manager.save_full(
            "panic",
            &[Message::user("before interruption")],
            "model",
            None,
            0,
            1,
        );
        let snapshot = manager.snapshot_from_files("panic").expect("snapshot");
        let outbox = MirrorOutbox {
            manifest: snapshot
                .manifest(manager.database_revision("panic").unwrap_or(0) + 1)
                .expect("manifest"),
            snapshot,
        };
        manager.write_outbox("panic", &outbox).expect("outbox");
        manager.reconcile_mirror("panic").expect("replay");
        let panic_ok = manager.audit_mirror("panic").consistent;

        manager.save_full(
            "missing-jsonl",
            &[Message::user("recover from db")],
            "model",
            None,
            0,
            1,
        );
        let dir = manager.session_path_dir("missing-jsonl");
        std::fs::remove_file(dir.join("messages.jsonl")).expect("delete JSONL");
        let jsonl_recovery_ok = manager
            .load("missing-jsonl")
            .map(|(_, messages)| messages.len())
            == Some(1)
            && dir.join("messages.jsonl").exists();

        manager.save_full(
            "missing-db",
            &[Message::user("recover database")],
            "model",
            None,
            0,
            1,
        );
        manager
            .dbs
            .lock()
            .expect("database lock")
            .remove("missing-db");
        std::fs::remove_file(manager.session_path_dir("missing-db").join("sessions.db"))
            .expect("delete database");
        let db_recovery_ok = manager
            .load("missing-db")
            .map(|(_, messages)| messages.len())
            == Some(1)
            && manager.audit_mirror("missing-db").consistent;

        manager.save_full("toggle", &[Message::user("db on")], "model", None, 0, 1);
        manager.set_turso_enabled(false);
        manager.save_append("toggle", &[Message::user("db off")], "model", None, 0, 2);
        manager.set_turso_enabled(true);
        let toggle_ok = manager.load("toggle").map(|(_, messages)| messages.len()) == Some(2)
            && manager.audit_mirror("toggle").consistent;

        manager.save_full(
            "manual-delete",
            &[Message::user("one"), Message::user("two")],
            "model",
            None,
            0,
            1,
        );
        store::rewrite_messages(
            &manager.session_path_dir("manual-delete"),
            &[Message::user("one")],
        )
        .expect("manual edit");
        let manual_delete_kept_history = manager
            .load("manual-delete")
            .map(|(_, messages)| messages.len())
            == Some(2);

        assert!(manual_delete_kept_history, "a JSONL file whose manifest no longer matches must be restored from the verified DB snapshot");

        println!("SIMULATION parallel={parallel_ok} panic_outbox={panic_ok} missing_jsonl={jsonl_recovery_ok} missing_db={db_recovery_ok} db_toggle={toggle_ok} manual_jsonl_delete_kept_history={manual_delete_kept_history}");
        std::fs::remove_dir_all(root).expect("remove test directory");
    }
}
