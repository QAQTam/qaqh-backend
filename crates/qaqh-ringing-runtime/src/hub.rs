//! Ringing runtime state beside the session owner.
//!
//! Binary session content is stored in the same SQLite database as the session
//! rows. Loop residency belongs to the registry; this hub only stores content.

use std::sync::Arc;

use qaqh_session::db::{Db, DbError, store};

/// Ringing daemon runtime state.
pub struct RingingHub {
    epoch: String,
    db: Arc<Db>,
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
        }
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

    /// Store content without blocking the calling async runtime worker.
    pub async fn put_content_async(
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
        self.db
            .write(move |connection| {
                store::put_session_content(
                    connection,
                    &owned_session_id,
                    &owned_content_id,
                    &owned_media_type,
                    &bytes,
                    truncated,
                )
            })
            .await?;
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

    /// Read session-scoped content without blocking the calling async runtime worker.
    pub async fn get_content_async(
        &self,
        session_id: &str,
        content_id: &str,
    ) -> Result<Option<ContentEntry>, DbError> {
        let owned_session_id = session_id.to_string();
        let owned_content_id = content_id.to_string();
        self.db
            .read(move |connection| {
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
            .await
    }

    pub fn content_sessions(&self, content_id: &str) -> Result<Vec<String>, DbError> {
        let owned_content_id = content_id.to_string();
        self.db.read_blocking(move |connection| {
            store::session_content_owners(connection, &owned_content_id)
        })
    }

    /// List content owners without blocking the calling async runtime worker.
    pub async fn content_sessions_async(&self, content_id: &str) -> Result<Vec<String>, DbError> {
        let owned_content_id = content_id.to_string();
        self.db
            .read(move |connection| store::session_content_owners(connection, &owned_content_id))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn async_content_write_yields_while_the_write_executor_is_busy() {
        let temp = tempfile::tempdir().expect("tempdir");
        let db = Arc::new(Db::open(temp.path().join("qaqh.db")).expect("database"));
        db.write(|connection| store::ensure_session(connection, "s1", None, false))
            .await
            .expect("session row");
        let hub = RingingHub::with_database("epoch-1", Arc::clone(&db));

        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let blocking_db = Arc::clone(&db);
        let blocker = tokio::spawn(async move {
            blocking_db
                .write(move |_| {
                    let _ = entered_tx.send(());
                    let _ = release_rx.recv_timeout(std::time::Duration::from_millis(500));
                    Ok::<(), qaqh_session::db::DbError>(())
                })
                .await
        });
        entered_rx.await.expect("write executor entered");

        let pending = tokio::time::timeout(
            std::time::Duration::from_millis(10),
            hub.put_content_async("s1", "text/plain", b"large".to_vec(), false),
        )
        .await;
        release_tx.send(()).expect("release blocking write");
        blocker.await.expect("blocker").expect("blocking write");
        assert!(pending.is_err(), "async write blocked the runtime worker");

        let content_id = hub
            .put_content_async("s1", "text/plain", b"large".to_vec(), false)
            .await
            .expect("store content");
        let entry = hub
            .get_content_async("s1", &content_id)
            .await
            .expect("query")
            .expect("stored content");
        assert_eq!(entry.bytes, b"large");
        assert!(
            hub.get_content_async("s2", &content_id)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            hub.content_sessions_async(&content_id).await.unwrap(),
            ["s1"]
        );
    }

    #[tokio::test]
    async fn async_content_read_yields_while_the_read_pool_is_busy() {
        let temp = tempfile::tempdir().expect("tempdir");
        let db = Arc::new(Db::open(temp.path().join("qaqh.db")).expect("database"));
        db.write(|connection| store::ensure_session(connection, "s1", None, false))
            .await
            .expect("session row");
        let content_id = qaqh_types::sha256_hex(b"large");
        db.write({
            let content_id = content_id.clone();
            move |connection| {
                store::put_session_content(
                    connection,
                    "s1",
                    &content_id,
                    "text/plain",
                    b"large",
                    false,
                )
            }
        })
        .await
        .expect("content row");
        let hub = RingingHub::with_database("epoch-1", Arc::clone(&db));

        let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release_rx = std::sync::Arc::new(std::sync::Mutex::new(release_rx));
        let mut blockers = Vec::new();
        for _ in 0..4 {
            let entered_tx = entered_tx.clone();
            let release_rx = std::sync::Arc::clone(&release_rx);
            let blocking_db = Arc::clone(&db);
            blockers.push(tokio::spawn(async move {
                blocking_db
                    .read(move |_| {
                        let _ = entered_tx.send(());
                        let _ = release_rx
                            .lock()
                            .expect("release receiver")
                            .recv_timeout(std::time::Duration::from_millis(500));
                        Ok::<(), qaqh_session::db::DbError>(())
                    })
                    .await
            }));
        }
        for _ in 0..4 {
            entered_rx.recv().await.expect("read pool worker entered");
        }

        let pending = tokio::time::timeout(
            std::time::Duration::from_millis(10),
            hub.get_content_async("s1", &content_id),
        )
        .await;
        for _ in 0..4 {
            release_tx.send(()).expect("release blocking read");
        }
        for blocker in blockers {
            blocker.await.expect("blocker").expect("blocking read");
        }
        assert!(pending.is_err(), "async read blocked the runtime worker");
    }

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
