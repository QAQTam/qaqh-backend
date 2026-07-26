//! Stable session snapshot metadata shared by JSONL recovery and Turso mirrors.

use deepx_types::{Message, SessionMeta};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const MIRROR_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MirrorSnapshot {
    pub seed: String,
    pub meta: SessionMeta,
    pub messages: Vec<Message>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MirrorManifest {
    pub schema_version: u32,
    pub revision: u64,
    pub meta_sha256: String,
    pub messages_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MirrorOutbox {
    pub manifest: MirrorManifest,
    pub snapshot: MirrorSnapshot,
}

impl MirrorSnapshot {
    pub fn new(meta: SessionMeta, messages: Vec<Message>) -> Self {
        Self {
            seed: meta.seed.clone(),
            meta,
            messages,
        }
    }

    pub fn manifest(&self, revision: u64) -> Result<MirrorManifest, String> {
        Ok(MirrorManifest {
            schema_version: MIRROR_SCHEMA_VERSION,
            revision,
            meta_sha256: sha256_json(&self.meta)?,
            messages_sha256: sha256_json(&self.messages)?,
        })
    }
}

pub fn sha256_json<T: Serialize>(value: &T) -> Result<String, String> {
    let bytes =
        serde_json::to_vec(value).map_err(|error| format!("serialize snapshot: {error}"))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_snapshots_have_equal_manifest_hashes() {
        let mut meta = SessionMeta::default();
        meta.seed = "seed".into();
        let snapshot = MirrorSnapshot::new(meta, vec![Message::user("hi")]);
        assert_eq!(snapshot.manifest(7).unwrap(), snapshot.manifest(7).unwrap());
    }
}
