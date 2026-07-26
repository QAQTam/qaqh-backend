//! Turso local database backend for session dual-write.
//!
//! Mirrors JSONL session data to a local Turso (.db) file.
//! All code gated by `#[cfg(feature = "turso-backend")]`.

use std::path::Path;
use std::sync::LazyLock;

use crate::manager::CompactContext;
use crate::mirror::{MirrorManifest, MirrorSnapshot};
use deepx_types::{Message, SessionMeta};

static RT: LazyLock<Option<tokio::runtime::Runtime>> = LazyLock::new(|| {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()
});

/// Per-session local Turso (SQLite) database for dual-write mirroring.
///
/// When enabled via `database.enabled`, session messages are written
/// to both JSONL (primary) and this SQLite database for fast querying.
pub struct TursoBackend {
    _db: turso::Database,
    conn: turso::Connection,
}

/// Helper: get a reference to the shared runtime, or return an error.
fn runtime() -> Result<&'static tokio::runtime::Runtime, String> {
    RT.as_ref()
        .ok_or_else(|| "turso: tokio runtime unavailable".to_string())
}

impl std::fmt::Debug for TursoBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TursoBackend").finish_non_exhaustive()
    }
}

impl TursoBackend {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, String> {
        let path_str = path.as_ref().to_str().ok_or("invalid path")?;
        let db = runtime()?
            .block_on(turso::Builder::new_local(path_str).build())
            .map_err(|e| format!("open turso: {e}"))?;
        let conn = db.connect().map_err(|e| format!("connect turso: {e}"))?;
        Ok(Self { _db: db, conn })
    }

    pub fn init_tables(&self) -> Result<(), String> {
        runtime()?.block_on(async {
            // PRAGMAs that return rows must run via execute(), not execute_batch().
            let _ = self.conn.execute("PRAGMA journal_mode=WAL", ()).await;
            self.conn
                .execute("PRAGMA synchronous=FULL", ())
                .await
                .map_err(|e| format!("enable full synchronous writes: {e}"))?;
            self.conn
                .execute_batch(
                    "CREATE TABLE IF NOT EXISTS sessions (
                        seed TEXT PRIMARY KEY,
                        meta_json TEXT NOT NULL DEFAULT '{}',
                        created_at INTEGER NOT NULL DEFAULT (unixepoch()),
                        updated_at INTEGER NOT NULL DEFAULT (unixepoch())
                    );
                    CREATE TABLE IF NOT EXISTS messages (
                        id INTEGER PRIMARY KEY AUTOINCREMENT,
                        session_seed TEXT NOT NULL,
                        msg_id INTEGER,
                        role TEXT NOT NULL DEFAULT '',
                        content_json TEXT NOT NULL DEFAULT '{}',
                        FOREIGN KEY (session_seed) REFERENCES sessions(seed) ON DELETE CASCADE
                    );
                    CREATE INDEX IF NOT EXISTS idx_msgs ON messages(session_seed, msg_id);
                    CREATE TABLE IF NOT EXISTS session_mirrors (
                        seed TEXT PRIMARY KEY,
                        schema_version INTEGER NOT NULL,
                        revision INTEGER NOT NULL,
                        meta_sha256 TEXT NOT NULL,
                        messages_sha256 TEXT NOT NULL
                    );
                    CREATE TABLE IF NOT EXISTS session_compact_context (
                        seed TEXT PRIMARY KEY,
                        context_json TEXT NOT NULL,
                        created_at INTEGER NOT NULL
                    );",
                )
                .await
                .map_err(|e| format!("init tables: {e}"))?;
            // Checkpoint flushes WAL → main db file.
            let _ = self
                .conn
                .execute("PRAGMA wal_checkpoint(TRUNCATE)", ())
                .await;
            Ok(())
        })
    }

    /// Force-checkpoint the WAL into the main database file.
    pub fn checkpoint(&self) -> Result<(), String> {
        runtime()?.block_on(async {
            self.conn
                .execute("PRAGMA wal_checkpoint(TRUNCATE)", ())
                .await
                .map_err(|e| format!("checkpoint: {e}"))
                .map(|_| ())
        })
    }

    /// Replace a complete session snapshot and its manifest in one transaction.
    /// A manifest is written last, so its presence proves the preceding rows
    /// committed as one revision.
    pub fn replace_snapshot(
        &self,
        snapshot: &MirrorSnapshot,
        manifest: &MirrorManifest,
    ) -> Result<(), String> {
        let seed = snapshot.seed.clone();
        let meta_json = serde_json::to_string(&snapshot.meta)
            .map_err(|error| format!("serialize meta: {error}"))?;
        runtime()?.block_on(async {
            self.conn.execute_batch("BEGIN IMMEDIATE").await
                .map_err(|error| format!("begin snapshot tx: {error}"))?;
            let result: Result<(), String> = async {
                self.conn.execute(
                    "INSERT OR REPLACE INTO sessions (seed, meta_json, updated_at) VALUES (?1, ?2, unixepoch())",
                    turso::params![seed.clone(), meta_json],
                ).await.map_err(|error| format!("snapshot meta: {error}"))?;
                self.conn.execute(
                    "DELETE FROM messages WHERE session_seed = ?1", turso::params![seed.clone()],
                ).await.map_err(|error| format!("snapshot clear messages: {error}"))?;
                for message in &snapshot.messages {
                    let json = serde_json::to_string(message)
                        .map_err(|error| format!("serialize message: {error}"))?;
                    self.conn.execute(
                        "INSERT INTO messages (session_seed, msg_id, role, content_json) VALUES (?1, ?2, ?3, ?4)",
                        turso::params![seed.clone(), message.msg_id.map(|id| id as i64), message.role.clone(), json],
                    ).await.map_err(|error| format!("snapshot message: {error}"))?;
                }
                self.conn.execute(
                    "INSERT OR REPLACE INTO session_mirrors (seed, schema_version, revision, meta_sha256, messages_sha256) VALUES (?1, ?2, ?3, ?4, ?5)",
                    turso::params![seed.clone(), manifest.schema_version as i64, manifest.revision as i64, manifest.meta_sha256.clone(), manifest.messages_sha256.clone()],
                ).await.map_err(|error| format!("snapshot manifest: {error}"))?;
                Ok(())
            }.await;
            if let Err(error) = result {
                let _ = self.conn.execute_batch("ROLLBACK").await;
                return Err(error);
            }
            self.conn.execute_batch("COMMIT").await
                .map_err(|error| format!("commit snapshot tx: {error}"))?;
            let _ = self.conn.execute("PRAGMA wal_checkpoint(PASSIVE)", ()).await;
            Ok(())
        })
    }

    pub fn load_manifest(&self, seed: &str) -> Result<Option<MirrorManifest>, String> {
        let seed = seed.to_string();
        runtime()?.block_on(async {
            let mut rows = self.conn.query(
                "SELECT schema_version, revision, meta_sha256, messages_sha256 FROM session_mirrors WHERE seed = ?1",
                turso::params![seed],
            ).await.map_err(|error| format!("load manifest: {error}"))?;
            let Some(row) = rows.next().await.map_err(|error| format!("manifest rows: {error}"))? else {
                return Ok(None);
            };
            let integer = |index| -> Result<u64, String> {
                row.get_value(index).map_err(|error| format!("manifest value: {error}"))?
                    .as_integer().copied().map(|value| value as u64)
                    .ok_or_else(|| "manifest integer has wrong type".into())
            };
            let text = |index| -> Result<String, String> {
                row.get_value(index).map_err(|error| format!("manifest value: {error}"))?
                    .as_text().cloned().ok_or_else(|| "manifest text has wrong type".into())
            };
            Ok(Some(MirrorManifest {
                schema_version: integer(0)? as u32,
                revision: integer(1)?,
                meta_sha256: text(2)?,
                messages_sha256: text(3)?,
            }))
        })
    }

    pub fn save_compact_context(&self, seed: &str, context: &CompactContext) -> Result<(), String> {
        let seed = seed.to_string();
        let json = serde_json::to_string(context).map_err(|error| format!("serialize compact context: {error}"))?;
        let created_at = context.created_at as i64;
        runtime()?.block_on(async {
            self.conn.execute(
                "INSERT OR REPLACE INTO session_compact_context (seed, context_json, created_at) VALUES (?1, ?2, ?3)",
                turso::params![seed, json, created_at],
            ).await.map_err(|error| format!("save compact context: {error}"))?;
            let _ = self.conn.execute("PRAGMA wal_checkpoint(PASSIVE)", ()).await;
            Ok(())
        })
    }

    pub fn load_compact_context(&self, seed: &str) -> Result<Option<CompactContext>, String> {
        let seed = seed.to_string();
        runtime()?.block_on(async {
            let mut rows = self.conn.query(
                "SELECT context_json FROM session_compact_context WHERE seed = ?1",
                turso::params![seed],
            ).await.map_err(|error| format!("load compact context: {error}"))?;
            let Some(row) = rows.next().await.map_err(|error| format!("compact context rows: {error}"))? else { return Ok(None); };
            let json = row.get_value(0).map_err(|error| format!("compact context value: {error}"))?
                .as_text().cloned().ok_or_else(|| "compact context has wrong type".to_string())?;
            serde_json::from_str(&json).map(Some).map_err(|error| format!("parse compact context: {error}"))
        })
    }

    // ── meta ──────────────────────────────────────────────────────────

    pub fn upsert_meta(&self, seed: &str, meta: &SessionMeta) -> Result<(), String> {
        let json = serde_json::to_string(meta).unwrap_or_default();
        let seed = seed.to_string();
        runtime()?.block_on(async {
            self.conn
                .execute(
                    "INSERT OR REPLACE INTO sessions (seed, meta_json, updated_at)
                     VALUES (?1, ?2, unixepoch())",
                    turso::params![seed, json],
                )
                .await
                .map_err(|e| format!("upsert meta: {e}"))?;
            Ok(())
        })
    }

    pub fn load_meta(&self, seed: &str) -> Result<Option<SessionMeta>, String> {
        let seed = seed.to_string();
        runtime()?.block_on(async {
            let mut rows = self
                .conn
                .query(
                    "SELECT meta_json FROM sessions WHERE seed = ?1",
                    turso::params![seed.clone()],
                )
                .await
                .map_err(|e| format!("load meta: {e}"))?;
            if let Some(row) = rows.next().await.map_err(|e| format!("rows: {e}"))? {
                let s: String = row
                    .get_value(0)
                    .map_err(|e| format!("get: {e}"))?
                    .as_text()
                    .unwrap_or(&String::new())
                    .clone();
                Ok(serde_json::from_str(&s).ok())
            } else {
                Ok(None)
            }
        })
    }

    pub fn list_sessions(&self) -> Result<Vec<SessionMeta>, String> {
        runtime()?.block_on(async {
            let mut rows = self
                .conn
                .query(
                    "SELECT meta_json FROM sessions ORDER BY updated_at DESC",
                    [0i32; 0],
                )
                .await
                .map_err(|e| format!("list: {e}"))?;
            let mut v = Vec::new();
            while let Some(row) = rows.next().await.map_err(|e| format!("rows: {e}"))? {
                let s: String = row
                    .get_value(0)
                    .map_err(|e| format!("get: {e}"))?
                    .as_text()
                    .unwrap_or(&String::new())
                    .clone();
                if let Ok(m) = serde_json::from_str(&s) {
                    v.push(m);
                }
            }
            Ok(v)
        })
    }

    pub fn message_count(&self, seed: &str) -> Result<usize, String> {
        let seed = seed.to_string();
        runtime()?.block_on(async {
            let mut rows = self
                .conn
                .query(
                    "SELECT COUNT(*) FROM messages WHERE session_seed = ?1",
                    turso::params![seed],
                )
                .await
                .map_err(|e| format!("count msgs: {e}"))?;
            if let Some(row) = rows.next().await.map_err(|e| format!("rows: {e}"))? {
                let count: i64 = row
                    .get_value(0)
                    .map_err(|e| format!("get: {e}"))?
                    .as_integer()
                    .copied()
                    .unwrap_or(0);
                Ok(count as usize)
            } else {
                Ok(0)
            }
        })
    }

    // ── messages ──────────────────────────────────────────────────────

    /// Insert a batch of messages in a single transaction (single fsync, 10x+ throughput).
    pub fn insert_messages_batch(&self, seed: &str, messages: &[Message]) -> Result<(), String> {
        if messages.is_empty() {
            return Ok(());
        }
        let seed = seed.to_string();

        runtime()?.block_on(async move {
            self.conn
                .execute_batch("BEGIN IMMEDIATE")
                .await
                .map_err(|e| format!("begin tx: {e}"))?;

            for msg in messages {
                let json = serde_json::to_string(msg).unwrap_or_default();
                let mid = msg.msg_id.map(|v| v as i64);
                let role = msg.role.clone();
                if let Err(e) = self
                    .conn
                    .execute(
                        "INSERT OR REPLACE INTO messages (session_seed, msg_id, role, content_json)
                         VALUES (?1, ?2, ?3, ?4)",
                        turso::params![seed.clone(), mid, role, json],
                    )
                    .await
                {
                    // Rollback on error before returning
                    let _ = self.conn.execute_batch("ROLLBACK").await;
                    return Err(format!("insert msg batch: {e}"));
                }
            }

            self.conn
                .execute_batch("COMMIT")
                .await
                .map_err(|e| format!("commit tx: {e}"))?;

            // Passive checkpoint — flushes committed WAL pages into main db.
            let _ = self
                .conn
                .execute("PRAGMA wal_checkpoint(PASSIVE)", ())
                .await;

            Ok(())
        })
    }

    pub fn insert_message(&self, seed: &str, msg: &Message) -> Result<(), String> {
        let json = serde_json::to_string(msg).unwrap_or_default();
        let seed = seed.to_string();
        let mid = msg.msg_id.map(|v| v as i64);
        let role = msg.role.clone();
        runtime()?.block_on(async {
            self.conn
                .execute(
                    "INSERT INTO messages (session_seed, msg_id, role, content_json)
                     VALUES (?1, ?2, ?3, ?4)",
                    turso::params![seed, mid, role, json],
                )
                .await
                .map_err(|e| format!("insert msg: {e}"))?;
            Ok(())
        })
    }

    pub fn load_messages(&self, seed: &str) -> Result<Vec<Message>, String> {
        let seed = seed.to_string();
        runtime()?.block_on(async {
            let mut rows = self
                .conn
                .query(
                    "SELECT content_json FROM messages WHERE session_seed = ?1 ORDER BY msg_id",
                    turso::params![seed],
                )
                .await
                .map_err(|e| format!("load msgs: {e}"))?;
            let mut v = Vec::new();
            while let Some(row) = rows.next().await.map_err(|e| format!("rows: {e}"))? {
                let s: String = row
                    .get_value(0)
                    .map_err(|e| format!("get: {e}"))?
                    .as_text()
                    .unwrap_or(&String::new())
                    .clone();
                if let Ok(m) = serde_json::from_str(&s) {
                    v.push(m);
                }
            }
            Ok(v)
        })
    }

    // ── delete ────────────────────────────────────────────────────────

    /// Clear all messages for a session (used before rewrite).
    pub fn clear_messages(&self, seed: &str) -> Result<(), String> {
        let seed = seed.to_string();
        runtime()?.block_on(async {
            self.conn
                .execute(
                    "DELETE FROM messages WHERE session_seed = ?1",
                    turso::params![seed],
                )
                .await
                .map_err(|e| format!("clear msgs: {e}"))?;
            Ok(())
        })
    }

    /// Rewrite all messages for a session: clear then batch insert in a transaction.
    pub fn rewrite_messages(&self, seed: &str, messages: &[Message]) -> Result<(), String> {
        let seed = seed.to_string();
        runtime()?.block_on(async {
            self.conn
                .execute_batch("BEGIN IMMEDIATE")
                .await
                .map_err(|e| format!("begin tx: {e}"))?;

            // Delete old messages
            if let Err(e) = self
                .conn
                .execute(
                    "DELETE FROM messages WHERE session_seed = ?1",
                    turso::params![seed.clone()],
                )
                .await
            {
                let _ = self.conn.execute_batch("ROLLBACK").await;
                return Err(format!("rewrite clear msgs: {e}"));
            }

            // Insert new messages
            for msg in messages {
                let json = serde_json::to_string(msg).unwrap_or_default();
                let mid = msg.msg_id.map(|v| v as i64);
                let role = msg.role.clone();
                if let Err(e) = self
                    .conn
                    .execute(
                        "INSERT INTO messages (session_seed, msg_id, role, content_json)
                         VALUES (?1, ?2, ?3, ?4)",
                        turso::params![seed.clone(), mid, role, json],
                    )
                    .await
                {
                    let _ = self.conn.execute_batch("ROLLBACK").await;
                    return Err(format!("rewrite insert: {e}"));
                }
            }

            self.conn
                .execute_batch("COMMIT")
                .await
                .map_err(|e| format!("commit tx: {e}"))?;

            let _ = self
                .conn
                .execute("PRAGMA wal_checkpoint(PASSIVE)", ())
                .await;
            Ok(())
        })
    }

    pub fn delete_session(&self, seed: &str) -> Result<(), String> {
        let seed = seed.to_string();
        runtime()?.block_on(async {
            self.conn
                .execute(
                    "DELETE FROM messages WHERE session_seed = ?1",
                    turso::params![seed.clone()],
                )
                .await
                .map_err(|e| format!("del msgs: {e}"))?;
            self.conn
                .execute("DELETE FROM sessions WHERE seed = ?1", turso::params![seed])
                .await
                .map_err(|e| format!("del session: {e}"))?;
            Ok(())
        })
    }
}
