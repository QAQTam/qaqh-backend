//! 有界内容寻址 Blob 存储（大输出与附件外置端）。

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use qaqh_core_vocab::ContentHash;
use sha2::{Digest, Sha256};

use super::lock;

/// 骨架期内存 Blob 库总容量上限（256 MiB，按 FIFO 淘汰最老条目）。
const MAX_BLOB_STORE_BYTES: usize = 256 * 1024 * 1024;

struct StoreInner {
    entries: BTreeMap<ContentHash, Arc<Vec<u8>>>,
    order: VecDeque<ContentHash>,
    total_bytes: usize,
}

/// 内容寻址 blob 库（大输出/图片的外置端）。
pub(crate) struct BlobStore {
    inner: Mutex<StoreInner>,
}

impl BlobStore {
    /// 建库。
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(StoreInner {
                entries: BTreeMap::new(),
                order: VecDeque::new(),
                total_bytes: 0,
            }),
        }
    }

    /// 存入并返回内容哈希（同内容去重；总字节超限按 FIFO 淘汰）。
    pub(crate) fn put(&self, bytes: Vec<u8>) -> ContentHash {
        let digest: [u8; 32] = Sha256::digest(&bytes).into();
        let hash = ContentHash::from_bytes(digest);
        let len = bytes.len();
        let mut inner = lock(&self.inner);
        if inner.entries.contains_key(&hash) {
            return hash;
        }
        while inner.total_bytes.saturating_add(len) > MAX_BLOB_STORE_BYTES {
            let Some(oldest) = inner.order.pop_front() else {
                break;
            };
            if let Some(removed) = inner.entries.remove(&oldest) {
                inner.total_bytes = inner.total_bytes.saturating_sub(removed.len());
            }
        }
        inner.total_bytes = inner.total_bytes.saturating_add(len);
        inner.entries.insert(hash, Arc::new(bytes));
        inner.order.push_back(hash);
        hash
    }

    /// 取回。
    pub(crate) fn get(&self, hash: &ContentHash) -> Option<Arc<Vec<u8>>> {
        lock(&self.inner).entries.get(hash).cloned()
    }
}
