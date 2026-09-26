//! Durable canonical identity for a seed-keyed session directory.
//!
//! v2 canonical facts require UUIDv7 `SessionId`/`LogId`, while the v1 wire and
//! on-disk session directory remain keyed by the legacy seed. This sidecar is
//! the stable bridge between those two identities: it is created atomically on
//! first use and reused after reopen. It is metadata, never a fact source.

#[cfg(unix)]
use std::fs::File;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

use crate::session_fact_v2::{LogId, SessionId};

pub const CANONICAL_IDENTITY_FILE: &str = "canonical-identity.json";
pub const CANONICAL_IDENTITY_SCHEMA: &str = "qaqh.canonical-identity/v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanonicalSessionIdentity {
    pub schema: String,
    pub session_id: SessionId,
    pub log_id: LogId,
}

#[derive(Debug, Error)]
pub enum CanonicalIdentityError {
    #[error("canonical identity io error: {0}")]
    Io(#[from] io::Error),

    #[error("canonical identity json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("unsupported canonical identity schema: {0}")]
    UnsupportedSchema(String),

    #[error("canonical identity {field} is not UUIDv7: {value}")]
    InvalidId { field: &'static str, value: String },

    #[error("canonical session_id and log_id must differ")]
    DuplicateIds,

    #[error("canonical identity already exists with a different session_id")]
    Conflict,
}

impl CanonicalSessionIdentity {
    pub fn new() -> Self {
        Self {
            schema: CANONICAL_IDENTITY_SCHEMA.to_string(),
            session_id: SessionId::new(Uuid::now_v7().to_string()),
            log_id: LogId::new(Uuid::now_v7().to_string()),
        }
    }

    /// Read an existing identity sidecar without creating one.
    pub fn open(session_dir: impl AsRef<Path>) -> Result<Self, CanonicalIdentityError> {
        let bytes = fs::read(session_dir.as_ref().join(CANONICAL_IDENTITY_FILE))?;
        Self::decode(&bytes)
    }

    /// Read the identity sidecar, creating it if absent.
    ///
    /// Creation uses a unique temp file plus a no-clobber hard link, so two
    /// concurrent creators cannot silently install different identities. A
    /// crash before the link leaves only an unreferenced temp file; a crash
    /// after the link leaves a complete, durable sidecar.
    pub fn open_or_create(session_dir: impl AsRef<Path>) -> Result<Self, CanonicalIdentityError> {
        let session_dir = session_dir.as_ref();
        fs::create_dir_all(session_dir)?;
        let path = session_dir.join(CANONICAL_IDENTITY_FILE);
        match fs::read(&path) {
            Ok(bytes) => return Self::decode(&bytes),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        Self::write_new(session_dir, &Self::new())
    }

    /// Install a preallocated identity into a new session directory.
    ///
    /// This is the beta identity-unification creation path: the caller chooses
    /// `SessionId` first, uses it as the directory name, then installs the
    /// matching sidecar. Reinstalling the same identity is idempotent; a
    /// conflicting sidecar fails closed.
    pub fn install(
        session_dir: impl AsRef<Path>,
        identity: &Self,
    ) -> Result<Self, CanonicalIdentityError> {
        let session_dir = session_dir.as_ref();
        fs::create_dir_all(session_dir)?;
        Self::write_new(session_dir, identity)
    }

    fn write_new(session_dir: &Path, identity: &Self) -> Result<Self, CanonicalIdentityError> {
        let path = session_dir.join(CANONICAL_IDENTITY_FILE);

        match fs::read(&path) {
            Ok(bytes) => {
                let existing = Self::decode(&bytes)?;
                return if existing == *identity {
                    Ok(existing)
                } else {
                    Err(CanonicalIdentityError::Conflict)
                };
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }

        let bytes = serde_json::to_vec_pretty(identity)?;
        let temp_path = session_dir.join(format!(
            ".{CANONICAL_IDENTITY_FILE}.{}.{}.tmp",
            std::process::id(),
            next_temp_ordinal()
        ));

        let write_result = (|| -> Result<(), CanonicalIdentityError> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp_path)?;
            file.write_all(&bytes)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            Ok(())
        })();
        if let Err(error) = write_result {
            let _ = fs::remove_file(&temp_path);
            return Err(error);
        }

        match fs::hard_link(&temp_path, &path) {
            Ok(()) => {
                let _ = fs::remove_file(&temp_path);
                sync_parent_dir(session_dir)?;
                Ok(identity.clone())
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let _ = fs::remove_file(&temp_path);
                let bytes = fs::read(&path)?;
                let existing = Self::decode(&bytes)?;
                if existing == *identity {
                    Ok(existing)
                } else {
                    Err(CanonicalIdentityError::Conflict)
                }
            }
            Err(error) => {
                let _ = fs::remove_file(&temp_path);
                Err(error.into())
            }
        }
    }

    fn decode(bytes: &[u8]) -> Result<Self, CanonicalIdentityError> {
        let identity: Self = serde_json::from_slice(bytes)?;
        if identity.schema != CANONICAL_IDENTITY_SCHEMA {
            return Err(CanonicalIdentityError::UnsupportedSchema(identity.schema));
        }
        validate_uuid_v7("session_id", identity.session_id.as_str())?;
        validate_uuid_v7("log_id", identity.log_id.as_str())?;
        if identity.session_id.as_str() == identity.log_id.as_str() {
            return Err(CanonicalIdentityError::DuplicateIds);
        }
        Ok(identity)
    }
}

impl Default for CanonicalSessionIdentity {
    fn default() -> Self {
        Self::new()
    }
}

/// Generate a canonical UUIDv7 session identifier.
pub fn generate_session_id() -> SessionId {
    SessionId::new(Uuid::now_v7().to_string())
}

/// Generate a Crockford-base32 ULID-shaped identifier from UUIDv7 bytes.
///
/// Canonical event/execution IDs require the ULID alphabet and length. UUIDv7
/// supplies the timestamp-ordered 128-bit payload; this encoding preserves the
/// same bytes in the canonical ULID representation.
pub fn generate_ulid() -> String {
    encode_ulid_bytes(*Uuid::now_v7().as_bytes())
}

/// Deterministically map a legacy/wire identifier to a canonical ULID.
///
/// v1 model providers use arbitrary tool-call IDs (`call_1`, `tc_...`), while
/// canonical facts require the ULID shape. Hashing the wire ID gives a stable
/// migration alias without changing the wire contract.
pub fn ulid_from_text(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    encode_ulid_bytes(bytes)
}

fn encode_ulid_bytes(bytes: [u8; 16]) -> String {
    const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let value = u128::from_be_bytes(bytes);
    let mut out = String::with_capacity(26);
    for position in (0..26).rev() {
        let index = ((value >> (position * 5)) & 0x1f) as usize;
        out.push(ALPHABET[index] as char);
    }
    out
}

fn validate_uuid_v7(field: &'static str, value: &str) -> Result<(), CanonicalIdentityError> {
    let uuid = Uuid::parse_str(value).map_err(|_| CanonicalIdentityError::InvalidId {
        field,
        value: value.to_string(),
    })?;
    if uuid.get_version_num() != 7 {
        return Err(CanonicalIdentityError::InvalidId {
            field,
            value: value.to_string(),
        });
    }
    Ok(())
}

fn next_temp_ordinal() -> u64 {
    static ORDINAL: AtomicU64 = AtomicU64::new(0);
    ORDINAL.fetch_add(1, Ordering::Relaxed)
}

#[cfg(unix)]
fn sync_parent_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_stable_across_reopen() {
        let temp = tempfile::tempdir().expect("tempdir");
        let first = CanonicalSessionIdentity::open_or_create(temp.path()).expect("create");
        let second = CanonicalSessionIdentity::open_or_create(temp.path()).expect("reopen");
        assert_eq!(first, second);
        assert_ne!(first.session_id.as_str(), first.log_id.as_str());
        assert_eq!(first.session_id.as_str().len(), 36);
        assert_eq!(first.log_id.as_str().len(), 36);
    }

    #[test]
    fn preallocated_identity_install_is_idempotent_and_conflict_fails_closed() {
        let temp = tempfile::tempdir().expect("tempdir");
        let identity = CanonicalSessionIdentity::new();
        let session_dir = temp.path().join(identity.session_id.as_str());

        assert_eq!(
            CanonicalSessionIdentity::install(&session_dir, &identity).expect("install"),
            identity
        );
        assert_eq!(
            CanonicalSessionIdentity::install(&session_dir, &identity).expect("reinstall"),
            identity
        );

        let conflicting = CanonicalSessionIdentity::new();
        assert!(matches!(
            CanonicalSessionIdentity::install(&session_dir, &conflicting),
            Err(CanonicalIdentityError::Conflict)
        ));
    }

    #[test]
    fn invalid_identity_fails_closed() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            temp.path().join(CANONICAL_IDENTITY_FILE),
            r#"{"schema":"qaqh.canonical-identity/v1","session_id":"seed-a","log_id":"seed-b"}"#,
        )
        .expect("write invalid identity");
        assert!(CanonicalSessionIdentity::open_or_create(temp.path()).is_err());
    }

    #[test]
    fn generated_ulid_has_canonical_shape() {
        let id = generate_ulid();
        assert_eq!(id.len(), 26);
        assert!(id.bytes().all(|byte| {
            matches!(
                byte,
                b'0'..=b'9' | b'A'..=b'H' | b'J' | b'K' | b'M' | b'N' | b'P'..=b'T' | b'V'..=b'Z'
            )
        }));
    }
}
