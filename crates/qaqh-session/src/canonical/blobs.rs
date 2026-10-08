//! canonical/blobs — per-session persistent CAS blob store.
//!
//! Bodies referenced by facts live here: content-addressed by sha256, no TTL,
//! reclaimed only with the session itself. Writes go tmp → fsync → rename so a
//! fact appended after a successful `put` can always resolve the body it
//! references (I2/I5). This is the persistent store proper; `ContentStore`
//! remains a wire-transport cache and facts must never point at it.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};

use super::recovery::sha256_content_hash;
use crate::session_fact_v2::{ContentHash, ContentRef, ResourceId, ResourceKind};

/// ULID (Crockford base32) alphabet — `ResourceId` is validated as
/// `res_` + 26 of these characters.
const ULID_CHARSET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

pub const BLOBS_DIR: &str = "blobs";

#[derive(Debug)]
pub struct SessionBlobStore {
    dir: PathBuf,
}

impl SessionBlobStore {
    pub fn open(session_dir: impl AsRef<Path>) -> io::Result<Self> {
        let dir = session_dir.as_ref().join(BLOBS_DIR);
        fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    fn path_for(&self, hash: &ContentHash) -> PathBuf {
        // The hash is `sha256:<64 hex>`; strip the scheme for the file name so
        // every path component stays a plain hex string.
        self.dir.join(hash.as_str().trim_start_matches("sha256:"))
    }

    /// Store `bytes` and return a `ContentRef` that resolves through this
    /// store. Content addressing makes repeated puts of identical bytes
    /// idempotent.
    pub fn put(&self, bytes: &[u8]) -> io::Result<ContentRef> {
        let hash = sha256_content_hash(bytes);
        let final_path = self.path_for(&hash);
        if final_path.exists() {
            return Ok(ContentRef::new(hash));
        }
        let tmp_path = self.dir.join(format!(
            ".tmp-{}-{}",
            std::process::id(),
            hash.as_str().trim_start_matches("sha256:")
        ));
        {
            use std::io::Write;
            let mut file = fs::File::create(&tmp_path)?;
            file.write_all(bytes)?;
            file.sync_all()?;
        }
        fs::rename(&tmp_path, &final_path)?;
        // Best-effort directory durability; not every platform allows opening
        // a directory handle, and a lost rename on crash is recoverable by
        // re-putting the same bytes.
        if let Ok(dir_handle) = fs::File::open(&self.dir) {
            let _ = dir_handle.sync_all();
        }
        Ok(ContentRef::new(hash))
    }

    /// Resolve a `ContentRef` produced by this store.
    pub fn get(&self, content_ref: &ContentRef) -> io::Result<Vec<u8>> {
        fs::read(self.path_for(content_ref.hash()))
    }

    pub fn contains(&self, content_ref: &ContentRef) -> bool {
        self.path_for(content_ref.hash()).exists()
    }
}

/// Deterministic per-(session, kind) resource id.
///
/// Resource facts upsert by `(resource_kind, resource_id)`, so a resource that
/// exists once per session needs a stable id. Session ids are not ULIDs, so the
/// id is derived as `res_` + 26 Crockford-base32 characters of a session-keyed
/// hash — shape-valid (see fact validation) and stable across restarts.
pub fn stable_workspace_resource_id(session_id: &str, kind: ResourceKind) -> ResourceId {
    let digest = Sha256::digest(format!("qaqh-resource:{kind:?}:{session_id}").as_bytes());
    let bytes = digest.as_slice();
    let mut ulid = String::with_capacity(26);
    for index in 0..26 {
        let bit_index = index * 5;
        let high = bytes[bit_index / 8] as u16;
        let low = bytes[bit_index / 8 + 1] as u16;
        let value = ((high << 8) | low) >> (11 - (bit_index % 8)) & 0x1f;
        ulid.push(ULID_CHARSET[value as usize] as char);
    }
    ResourceId::new(format!("res_{ulid}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_is_content_addressed_and_resolvable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = SessionBlobStore::open(dir.path()).expect("open");
        let reference = store.put(b"todo-body").expect("put");
        assert!(store.contains(&reference));
        assert_eq!(store.get(&reference).expect("get"), b"todo-body");
        let again = store.put(b"todo-body").expect("put");
        assert_eq!(reference, again);
        let other = store.put(b"other").expect("put");
        assert_ne!(reference, other);
        assert_eq!(store.get(&other).expect("get"), b"other");
    }

    #[test]
    fn stable_resource_id_is_shape_valid_and_deterministic() {
        let first = stable_workspace_resource_id("seed-1", ResourceKind::Todo);
        let second = stable_workspace_resource_id("seed-1", ResourceKind::Todo);
        assert_eq!(first, second);
        let text = first.as_str();
        assert!(text.starts_with("res_"));
        let ulid = &text["res_".len()..];
        assert_eq!(ulid.len(), 26);
        assert!(crate::session_fact_v2::is_ulid_text(ulid));
        assert_ne!(
            stable_workspace_resource_id("seed-1", ResourceKind::Plan),
            first
        );
        assert_ne!(
            stable_workspace_resource_id("seed-2", ResourceKind::Todo),
            first
        );
    }
}
