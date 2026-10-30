//! canonical/blobs — per-session persistent CAS blob store.
//!
//! Bodies referenced by facts live here: content-addressed by sha256, no TTL,
//! reclaimed only with the session itself. Writes go tmp → fsync → link so a
//! body is published only after file fsync. Directory durability is platform
//! dependent, so callers cannot infer a cross-platform commit barrier from
//! `put` alone. This is the persistent store proper; `ContentStore`
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

    fn path_for(&self, hash: &ContentHash) -> io::Result<PathBuf> {
        let name = hash
            .as_str()
            .strip_prefix("sha256:")
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid blob hash"))?;
        if name.len() != 64
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid blob hash",
            ));
        }
        Ok(self.dir.join(name))
    }

    /// Store `bytes` and return a `ContentRef` that resolves through this
    /// store. Content addressing makes repeated puts of identical bytes
    /// idempotent.
    pub fn put(&self, bytes: &[u8]) -> io::Result<ContentRef> {
        let hash = sha256_content_hash(bytes);
        let final_path = self.path_for(&hash)?;
        if self.verify_existing(&hash, bytes)? {
            return Ok(ContentRef::new(hash));
        }
        let tmp_path = self.dir.join(format!(".tmp-{}", uuid::Uuid::now_v7()));
        let write_result = (|| {
            use std::io::Write;
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp_path)?;
            file.write_all(bytes)?;
            file.sync_all()
        })();
        if let Err(error) = write_result {
            let _ = fs::remove_file(&tmp_path);
            return Err(error);
        }
        // Never overwrite an existing content-addressed object: a corrupt or
        // conflicting object must fail closed rather than being silently healed.
        if let Err(error) = fs::hard_link(&tmp_path, &final_path) {
            let existing = self.verify_existing(&hash, bytes);
            let _ = fs::remove_file(&tmp_path);
            if !existing? {
                return Err(error);
            }
        } else {
            fs::remove_file(&tmp_path)?;
        }
        // Best-effort directory durability; not every platform allows opening
        // a directory handle, and a lost link on crash is recoverable by
        // re-putting the same bytes.
        if let Ok(dir_handle) = fs::File::open(&self.dir) {
            let _ = dir_handle.sync_all();
        }
        Ok(ContentRef::new(hash))
    }

    /// Stronger Unix-only preparation barrier for facts about to be committed.
    /// Other platforms must implement a proven directory durability primitive
    /// before this method can acknowledge successful preparation.
    pub fn put_durable(&self, bytes: &[u8]) -> io::Result<ContentRef> {
        #[cfg(not(unix))]
        {
            let _ = bytes;
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "durable blob publication is unsupported on this platform",
            ));
        }
        #[cfg(unix)]
        {
            let reference = self.put(bytes)?;
            fs::File::open(self.path_for(reference.hash())?)?.sync_all()?;
            fs::File::open(&self.dir)?.sync_all()?;
            let parent = self.dir.parent().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "blob directory has no parent")
            })?;
            fs::File::open(parent)?.sync_all()?;
            Ok(reference)
        }
    }

    fn verify_existing(&self, hash: &ContentHash, expected: &[u8]) -> io::Result<bool> {
        match fs::read(self.path_for(hash)?) {
            Ok(existing) if existing == expected => Ok(true),
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "existing blob hash mismatch",
            )),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Resolve a `ContentRef` produced by this store.
    pub fn get(&self, content_ref: &ContentRef) -> io::Result<Vec<u8>> {
        let bytes = fs::read(self.path_for(content_ref.hash())?)?;
        if sha256_content_hash(&bytes) != *content_ref.hash() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "blob hash mismatch",
            ));
        }
        Ok(bytes)
    }

    /// Resolve a blob without accepting an unbounded body into memory.
    pub fn get_bounded(&self, content_ref: &ContentRef, max_bytes: usize) -> io::Result<Vec<u8>> {
        use std::io::Read;

        let file = fs::File::open(self.path_for(content_ref.hash())?)?;
        if file.metadata()?.len() > max_bytes as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "blob exceeds byte budget",
            ));
        }
        let mut bytes = Vec::new();
        file.take((max_bytes as u64).saturating_add(1))
            .read_to_end(&mut bytes)?;
        if bytes.len() > max_bytes || sha256_content_hash(&bytes) != *content_ref.hash() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "blob exceeds budget or hash mismatch",
            ));
        }
        Ok(bytes)
    }

    pub fn contains(&self, content_ref: &ContentRef) -> bool {
        self.get(content_ref).is_ok()
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
