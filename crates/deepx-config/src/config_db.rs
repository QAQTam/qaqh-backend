//! SQLite backend for config dual-write.
//!
//! Stores the entire config as a single JSON blob keyed "main" in a
//! `config` table.  When `database.enabled` is true, Config::save()
//! writes to both config.toml and this db; Config::load() reads from
//! this db first, falling back to config.toml.

use std::path::Path;
use std::sync::LazyLock;

static RT: LazyLock<Option<tokio::runtime::Runtime>> = LazyLock::new(|| {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()
});

fn runtime() -> Result<&'static tokio::runtime::Runtime, String> {
    RT.as_ref()
        .ok_or_else(|| "config_db: tokio runtime unavailable".to_string())
}

pub struct ConfigDb {
    _db: turso::Database,
    conn: turso::Connection,
}

impl ConfigDb {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, String> {
        let path_str = path.as_ref().to_str().ok_or("invalid path")?;
        let db = runtime()?
            .block_on(turso::Builder::new_local(path_str).build())
            .map_err(|e| format!("open config db: {e}"))?;
        let conn = db
            .connect()
            .map_err(|e| format!("connect config db: {e}"))?;
        Ok(Self { _db: db, conn })
    }

    pub fn init_table(&self) -> Result<(), String> {
        runtime()?.block_on(async {
            let _ = self.conn.execute("PRAGMA journal_mode=WAL", ()).await;
            self.conn
                .execute("PRAGMA synchronous=FULL", ())
                .await
                .map_err(|e| format!("enable full synchronous writes: {e}"))?;
            self.conn
                .execute_batch(
                    "CREATE TABLE IF NOT EXISTS config (
                        key TEXT PRIMARY KEY,
                        value TEXT NOT NULL,
                        updated_at INTEGER NOT NULL DEFAULT (unixepoch())
                    )",
                )
                .await
                .map_err(|e| format!("init config table: {e}"))?;
            let _ = self
                .conn
                .execute("PRAGMA wal_checkpoint(TRUNCATE)", ())
                .await;
            Ok(())
        })
    }

    /// Save config JSON blob to db.
    pub fn save_config(&self, json: &str) -> Result<(), String> {
        let json = json.to_string();
        runtime()?.block_on(async {
            self.conn
                .execute(
                    "INSERT OR REPLACE INTO config (key, value, updated_at)
                     VALUES ('main', ?1, unixepoch())",
                    turso::params![json],
                )
                .await
                .map_err(|e| format!("save config: {e}"))?;
            let _ = self
                .conn
                .execute("PRAGMA wal_checkpoint(PASSIVE)", ())
                .await;
            Ok(())
        })
    }

    /// Load config JSON blob from db. Returns None if empty or not found.
    pub fn load_config(&self) -> Result<Option<String>, String> {
        runtime()?.block_on(async {
            let mut rows = self
                .conn
                .query("SELECT value FROM config WHERE key = 'main'", [0i32; 0])
                .await
                .map_err(|e| format!("load config: {e}"))?;
            if let Some(row) = rows.next().await.map_err(|e| format!("rows: {e}"))? {
                let s: String = row
                    .get_value(0)
                    .map_err(|e| format!("get: {e}"))?
                    .as_text()
                    .unwrap_or(&String::new())
                    .clone();
                Ok(Some(s))
            } else {
                Ok(None)
            }
        })
    }
}
