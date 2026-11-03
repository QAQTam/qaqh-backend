//! Ringing runtime state beside the session owner.
//!
//! Binary session content is stored in the same SQLite database as the session
//! rows. This hub keeps only process-local worker and interaction bookkeeping.

use std::sync::{Arc, Mutex};

use qaqh_session::db::{Db, DbError, store};

/// Ringing daemon runtime state.
pub struct RingingHub {
    epoch: String,
    db: Arc<Db>,
    /// Workers currently owned by the registry.
    live_workers: Mutex<std::collections::HashSet<String>>,
}

#[derive(Debug, Clone)]
pub struct ContentEntry {
    pub session_id: String,
    pub content_id: String,
    pub media_type: String,
    pub bytes: Vec<u8>,
    pub sha256: String,
    pub truncated: bool,
}

impl RingingHub {
    pub fn with_database(epoch: impl Into<String>, db: Arc<Db>) -> Self {
        Self {
            epoch: epoch.into(),
            db,
            live_workers: Mutex::new(std::collections::HashSet::new()),
        }
    }

    pub fn mark_worker_live(&self, session_id: &str) {
        self.live_workers
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(session_id.to_string());
    }

    pub fn mark_worker_dead(&self, session_id: &str) {
        self.live_workers
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(session_id);
    }

    pub fn memory_components(&self) -> Vec<qaqh_memwatch::ComponentMemory> {
        let live_workers = self
            .live_workers
            .lock()
            .map(|workers| workers.len() as u64)
            .unwrap_or_default();
        vec![qaqh_memwatch::ComponentMemory {
            name: "ringing.live_workers".into(),
            item_count: live_workers,
            payload_bytes: None,
            heap_estimate_bytes: None,
            ..Default::default()
        }]
    }

    pub fn epoch(&self) -> &str {
        &self.epoch
    }

    pub fn put_content(
        &self,
        session_id: &str,
        media_type: &str,
        bytes: Vec<u8>,
        truncated: bool,
    ) -> Result<String, DbError> {
        let content_id = qaqh_types::sha256_hex(&bytes);
        let owned_session_id = session_id.to_string();
        let owned_content_id = content_id.clone();
        let owned_media_type = media_type.to_string();
        self.db.write_blocking(move |connection| {
            store::put_session_content(
                connection,
                &owned_session_id,
                &owned_content_id,
                &owned_media_type,
                &bytes,
                truncated,
            )
        })?;
        Ok(content_id)
    }

    pub fn get_content(
        &self,
        session_id: &str,
        content_id: &str,
    ) -> Result<Option<ContentEntry>, DbError> {
        let owned_session_id = session_id.to_string();
        let owned_content_id = content_id.to_string();
        self.db.read_blocking(move |connection| {
            store::get_session_content(connection, &owned_session_id, &owned_content_id).map(
                |row| {
                    row.map(|(media_type, bytes, truncated)| ContentEntry {
                        session_id: owned_session_id,
                        content_id: owned_content_id.clone(),
                        media_type,
                        bytes,
                        sha256: owned_content_id,
                        truncated,
                    })
                },
            )
        })
    }

    pub fn content_sessions(&self, content_id: &str) -> Result<Vec<String>, DbError> {
        let owned_content_id = content_id.to_string();
        self.db.read_blocking(move |connection| {
            store::session_content_owners(connection, &owned_content_id)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_is_session_scoped_and_survives_hub_restart() {
        let temp = tempfile::tempdir().expect("tempdir");
        let database_path = temp.path().join("qaqh.db");
        let db = Arc::new(Db::open(&database_path).expect("database"));
        db.write_blocking(|connection| store::ensure_session(connection, "s1", None, false))
            .expect("session row");

        let content_id = {
            let hub = RingingHub::with_database("epoch-1", Arc::clone(&db));
            let content_id = hub
                .put_content("s1", "text/plain", b"persisted".to_vec(), false)
                .expect("store content");
            assert!(hub.get_content("s2", &content_id).expect("query").is_none());
            assert_eq!(hub.content_sessions(&content_id).expect("owners"), ["s1"]);
            content_id
        };
        drop(db);

        let db = Arc::new(Db::open(&database_path).expect("reopen database"));
        let hub = RingingHub::with_database("epoch-2", db);
        let entry = hub
            .get_content("s1", &content_id)
            .expect("query")
            .expect("content survives restart");
        assert_eq!(entry.bytes, b"persisted");
        assert_eq!(entry.media_type, "text/plain");
        assert_eq!(entry.sha256, content_id);
    }
}
