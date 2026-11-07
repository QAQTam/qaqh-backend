//! Files belonging to a session. Session identity and existence live in SQLite.

use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct SessionManager {
    data_dir: PathBuf,
}

impl SessionManager {
    pub fn new(data_dir: PathBuf) -> Self {
        Self { data_dir }
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Generate an identity; the caller creates the authoritative business row.
    pub fn allocate_session(&self) -> String {
        format!("ses_{}", qaqh_ringing_contract::ulid::generate_ulid())
    }

    /// Remove resource files after the business session has been deleted.
    pub fn delete(&self, session_id: &str) -> Result<(), String> {
        let mut components = Path::new(session_id).components();
        if !matches!(components.next(), Some(std::path::Component::Normal(_)))
            || components.next().is_some()
            || session_id.contains('\\')
        {
            return Err("session resource id must be one normal path component".into());
        }
        for path in [
            self.data_dir.join("sessions").join(session_id),
            self.data_dir.join("workspace").join(session_id),
        ] {
            match std::fs::remove_dir_all(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(format!(
                        "remove session resources {}: {error}",
                        path.display()
                    ));
                }
            }
        }
        Ok(())
    }
}
