//! 大内容外置存储（PLAN 大内容外置）。
//!
//! - 工具完整输出、compact archive、超大 diff、诊断内容进入 content store；
//! - 事件只携带 `RingingContentRef`（content_id、media_type、bytes、sha256、truncated）；
//! - 客户端通过带鉴权的 HTTP GET 按需读取（range/分页）；
//! - content 设置会话所有权与生命周期；
//! - **API key、provider 原始响应和未脱敏错误禁止进入 content store**。
//!
//! 持久化模式（`RingingHub::with_persistence`）下，条目按
//! `<root>/<content_id>.json` + `<root>/<content_id>.bin` 落盘：
//! body 先落盘、metadata 后落盘，读取时以 sha256 复核 body。崩溃窗口留下的
//! 无 metadata `.bin` 会在下次启动扫描时回收。进程重启后只加载 metadata，
//! body 在第一次读取时懒加载，避免启动时把大输出全量读入内存。content_id
//! 是正文的 sha256，相同正文的多个会话共享同一条目，用 `owners` 做引用计数。

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use qaqh_types::sha256_hex;
use serde::{Deserialize, Serialize};

/// 超过该阈值的内容应外置（10 MiB）。
///
/// ⚠ 这是**传输保护阀**，而非常规路径。内容走向本 store 的唯一入口是
/// `agent::loop_dispatch_conversation::externalize_canonical_content`（最终经
/// `hub.put_content` 落 store），在标准模式下模型文本已被
/// `TOOL_MODEL_MAX_CHARS`（24K 字符，上限约 96 KiB）封顶，永远够不到 10 MiB；
/// 只有 NoFold 极限模式下的超长输出才会触发。别把它当成"大输出的分页通道"。
///
/// 此外本 store 还服务于附件/图片（`attachment.rs`、`host_impl.rs`）与 HTTP
/// content 端点，那部分与工具结果外置无关，不受上述可达性限制。
pub const CONTENT_STORE_THRESHOLD_BYTES: usize = 10 * 1024 * 1024;

/// 默认生命周期（30 分钟）。
pub const DEFAULT_CONTENT_TTL: Duration = Duration::from_secs(30 * 60);

/// 单会话 pinned 条目上限（#345：pending interaction 正文）。
///
/// pinned 条目是「客户端还没有机会读到」的内容——交互正文只有几百字节量级，
/// 这个额度只用于防跑飞，不是常规容量规划。
pub const PINNED_MAX_ENTRIES_PER_SESSION: usize = 64;

/// 单会话 pinned 字节上限（#345）。
pub const PINNED_MAX_BYTES_PER_SESSION: usize = 4 * 1024 * 1024;

/// pinned 准入失败（#345 fail-closed）：调用方**不得**在正文拿不到的情况下
/// 继续把交互呈现给用户。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContentQuotaExceeded;

impl std::fmt::Display for ContentQuotaExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "content_quota_exceeded")
    }
}

#[derive(Debug, Clone)]
pub struct ContentEntry {
    pub content_id: String,
    /// 拥有该条目的会话集合。content_id 是正文哈希，同一正文可被多个会话
    /// 引用；删除时按引用计数释放。
    pub owners: Vec<String>,
    pub media_type: String,
    pub bytes: Vec<u8>,
    pub sha256: String,
    pub truncated: bool,
    /// `true` = 被一个活交互引用：不吃 TTL、不被容量淘汰，直到显式 unpin
    /// 或会话释放（#345）。
    pub pinned: bool,
    pub created_at: Instant,
    pub expires_at: Instant,
    /// pin 的业务键（交互 id）。持久化后仍可在重启后按交互 id unpin。
    pin_key: Option<String>,
    bytes_loaded: bool,
    size_bytes: usize,
}

impl ContentEntry {
    pub fn is_owned_by(&self, session_id: &str) -> bool {
        self.owners.iter().any(|owner| owner == session_id)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ContentMeta {
    content_id: String,
    owners: Vec<String>,
    media_type: String,
    sha256: String,
    truncated: bool,
    pinned: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pin_key: Option<String>,
    created_at_ms: u64,
    expires_at_ms: u64,
    size_bytes: usize,
}

/// 大内容存储（有界、会话所有权、TTL 清理）。
#[derive(Debug)]
pub struct ContentStore {
    entries: HashMap<String, ContentEntry>,
    max_entries: usize,
    /// `None` = 纯内存（测试/未装配持久化）；`Some` = 写穿磁盘。
    root: Option<PathBuf>,
}

impl Default for ContentStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ContentStore {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            max_entries: 256,
            root: None,
        }
    }

    /// 持久化 store：启动时只加载 metadata，body 首次读取时懒加载。
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        if let Err(error) = fs::create_dir_all(&root) {
            log::warn!(
                "[content] persistence disabled for {}: {error}",
                root.display()
            );
            return Self::new();
        }
        let mut store = Self {
            entries: HashMap::new(),
            max_entries: 256,
            root: Some(root),
        };
        store.load_index();
        store
    }

    /// 存入内容。返回 content_id（正文的 SHA-256 hex）。
    pub fn put(
        &mut self,
        session_id: &str,
        media_type: &str,
        bytes: Vec<u8>,
        truncated: bool,
    ) -> String {
        self.sweep_expired();
        let content_id = sha256_hex(&bytes);
        self.upsert(session_id, media_type, bytes, truncated, false, None);
        self.evict_over_capacity();
        content_id
    }

    /// 存入**被活交互引用**的内容（#345）：不吃 TTL、不被容量淘汰。
    ///
    /// 超配额返回 [`ContentQuotaExceeded`]（fail-closed）——调用方必须把交互
    /// 收尾，而不是呈现一个取不到正文的 modal。
    pub fn put_pinned(
        &mut self,
        session_id: &str,
        media_type: &str,
        bytes: Vec<u8>,
    ) -> Result<String, ContentQuotaExceeded> {
        self.put_pinned_for(session_id, media_type, bytes, None)
    }

    /// 与 [`Self::put_pinned`] 相同，但额外记录业务 pin 键（交互 id）。
    /// 重启后 [`Self::unpin_key`] 仍能按同一个键解除 pin。
    pub fn put_pinned_for(
        &mut self,
        session_id: &str,
        media_type: &str,
        bytes: Vec<u8>,
        pin_key: Option<&str>,
    ) -> Result<String, ContentQuotaExceeded> {
        self.sweep_expired();
        let content_id = sha256_hex(&bytes);
        let mut pinned_entries = 0usize;
        let mut pinned_bytes = 0usize;
        for entry in self.entries.values().filter(|entry| {
            entry.pinned && entry.is_owned_by(session_id) && entry.content_id != content_id
        }) {
            pinned_entries += 1;
            pinned_bytes = pinned_bytes.saturating_add(entry.size_bytes);
        }
        if pinned_entries >= PINNED_MAX_ENTRIES_PER_SESSION
            || pinned_bytes.saturating_add(bytes.len()) > PINNED_MAX_BYTES_PER_SESSION
        {
            return Err(ContentQuotaExceeded);
        }
        self.upsert(session_id, media_type, bytes, false, true, pin_key);
        self.evict_over_capacity();
        Ok(content_id)
    }

    /// 解除 pin：条目回到普通 TTL / 容量淘汰语义。
    pub fn unpin(&mut self, content_id: &str) -> bool {
        let snapshot = {
            let Some(entry) = self.entries.get_mut(content_id) else {
                return false;
            };
            if !entry.pinned {
                return false;
            }
            entry.pinned = false;
            entry.pin_key = None;
            entry.expires_at = Instant::now() + DEFAULT_CONTENT_TTL;
            entry.clone()
        };
        self.persist_entry(&snapshot);
        true
    }

    /// 按业务 pin 键解除某会话的 pin，返回解除数量。
    ///
    /// 这是重启恢复路径：`live_interaction_content` 内存表在重启后为空，
    /// 但仍能通过磁盘 metadata 里的 `pin_key` 找到正文并 unpin。
    pub fn unpin_key(&mut self, session_id: &str, pin_key: &str) -> usize {
        let ids: Vec<String> = self
            .entries
            .values()
            .filter(|entry| {
                entry.pinned
                    && entry.is_owned_by(session_id)
                    && entry.pin_key.as_deref() == Some(pin_key)
            })
            .map(|entry| entry.content_id.clone())
            .collect();
        let mut released = 0;
        for content_id in ids {
            if self.unpin(&content_id) {
                released += 1;
            }
        }
        released
    }

    /// 读取（校验所有权）。过期条目惰性清理。
    pub fn get(&mut self, session_id: &str, content_id: &str) -> Option<ContentEntry> {
        let entry = self.get_internal(content_id)?;
        if entry.is_owned_by(session_id) {
            Some(entry)
        } else {
            None
        }
    }

    /// 按 id 读取（**不校验所有权**，调用方负责）。v2 的 content 端点不带 seed，
    /// 由 daemon 拿条目的 `owners` 再校验调用方归属。
    pub fn get_any(&mut self, content_id: &str) -> Option<ContentEntry> {
        self.get_internal(content_id)
    }

    /// 会话关闭/切流时释放该会话内容。仅当最后一个 owner 释放时删除磁盘文件。
    pub fn release_session(&mut self, session_id: &str) -> usize {
        let ids: Vec<String> = self
            .entries
            .values()
            .filter(|entry| entry.is_owned_by(session_id))
            .map(|entry| entry.content_id.clone())
            .collect();
        let mut removed = 0;
        for content_id in ids {
            let owners = {
                let Some(entry) = self.entries.get_mut(&content_id) else {
                    continue;
                };
                entry.owners.retain(|owner| owner != session_id);
                entry.owners.clone()
            };
            if owners.is_empty() {
                self.remove_entry(&content_id);
                removed += 1;
            } else if let Some(entry) = self.entries.get(&content_id).cloned() {
                self.persist_entry(&entry);
            }
        }
        removed
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn memory_components(&self) -> Vec<qaqh_memwatch::ComponentMemory> {
        let mut loaded_entries = 0_u64;
        let mut loaded_bytes = 0_u64;
        let mut loaded_heap = 0_u64;
        let mut pinned_entries = 0_u64;
        let mut metadata_heap = self
            .entries
            .capacity()
            .saturating_mul(std::mem::size_of::<(String, ContentEntry)>());
        for (content_id, entry) in &self.entries {
            metadata_heap = metadata_heap
                .saturating_add(content_id.capacity())
                .saturating_add(std::mem::size_of::<ContentEntry>())
                .saturating_add(entry.content_id.capacity())
                .saturating_add(entry.media_type.capacity())
                .saturating_add(entry.sha256.capacity())
                .saturating_add(
                    entry
                        .owners
                        .capacity()
                        .saturating_mul(std::mem::size_of::<String>()),
                );
            for owner in &entry.owners {
                metadata_heap = metadata_heap.saturating_add(owner.capacity());
            }
            if entry.pinned {
                pinned_entries = pinned_entries.saturating_add(1);
            }
            if entry.bytes_loaded {
                loaded_entries = loaded_entries.saturating_add(1);
                loaded_bytes = loaded_bytes.saturating_add(entry.bytes.len() as u64);
                loaded_heap = loaded_heap.saturating_add(entry.bytes.capacity() as u64);
            }
            if let Some(pin_key) = &entry.pin_key {
                metadata_heap = metadata_heap.saturating_add(pin_key.capacity());
            }
        }
        vec![
            qaqh_memwatch::ComponentMemory {
                name: "ringing.content_store".into(),
                item_count: self.entries.len() as u64,
                payload_bytes: None,
                heap_estimate_bytes: Some(metadata_heap as u64),
                ..Default::default()
            },
            qaqh_memwatch::ComponentMemory {
                name: "ringing.content_store.loaded_bodies".into(),
                item_count: loaded_entries,
                payload_bytes: Some(loaded_bytes),
                heap_estimate_bytes: Some(loaded_heap),
                ..Default::default()
            },
            qaqh_memwatch::ComponentMemory {
                name: "ringing.content_store.pinned".into(),
                item_count: pinned_entries,
                payload_bytes: None,
                heap_estimate_bytes: None,
                ..Default::default()
            },
        ]
    }

    fn upsert(
        &mut self,
        session_id: &str,
        media_type: &str,
        bytes: Vec<u8>,
        truncated: bool,
        pinned: bool,
        pin_key: Option<&str>,
    ) {
        let content_id = sha256_hex(&bytes);
        let size_bytes = bytes.len();
        let now = Instant::now();
        let snapshot = {
            let entry = self
                .entries
                .entry(content_id.clone())
                .or_insert_with(|| ContentEntry {
                    content_id: content_id.clone(),
                    owners: Vec::new(),
                    media_type: media_type.to_string(),
                    bytes: Vec::new(),
                    sha256: content_id.clone(),
                    truncated,
                    pinned,
                    created_at: now,
                    expires_at: now + DEFAULT_CONTENT_TTL,
                    pin_key: pin_key.map(str::to_string),
                    bytes_loaded: false,
                    size_bytes,
                });
            if !entry.is_owned_by(session_id) {
                entry.owners.push(session_id.to_string());
            }
            if pinned {
                entry.pinned = true;
                if pin_key.is_some() {
                    entry.pin_key = pin_key.map(str::to_string);
                }
            }
            entry.media_type = media_type.to_string();
            entry.truncated = entry.truncated || truncated;
            entry.bytes = bytes;
            entry.bytes_loaded = true;
            entry.size_bytes = size_bytes;
            if !entry.pinned {
                entry.expires_at = now + DEFAULT_CONTENT_TTL;
            }
            entry.clone()
        };
        self.persist_entry(&snapshot);
    }

    fn get_internal(&mut self, content_id: &str) -> Option<ContentEntry> {
        let expired = {
            let entry = self.entries.get(content_id)?;
            !entry.pinned && entry.expires_at < Instant::now()
        };
        if expired {
            self.remove_entry(content_id);
            return None;
        }
        if !self.ensure_loaded(content_id) {
            self.remove_entry(content_id);
            return None;
        }
        self.entries.get(content_id).cloned()
    }

    /// 从磁盘懒加载 body；sha256 不符时删除条目（fail closed）。
    fn ensure_loaded(&mut self, content_id: &str) -> bool {
        if self
            .entries
            .get(content_id)
            .is_some_and(|entry| entry.bytes_loaded)
        {
            return true;
        }
        let Some(root) = self.root.clone() else {
            return false;
        };
        let Some(expected) = self
            .entries
            .get(content_id)
            .map(|entry| entry.sha256.clone())
        else {
            return false;
        };
        let path = root.join(format!("{content_id}.bin"));
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) => {
                log::warn!(
                    "[content] body read failed for {content_id} at {}: {error}",
                    path.display()
                );
                return false;
            }
        };
        let digest = sha256_hex(&bytes);
        if digest != expected {
            log::error!(
                "[content] body hash mismatch for {content_id}: expected {expected}, got {digest}"
            );
            return false;
        }
        if let Some(entry) = self.entries.get_mut(content_id) {
            entry.size_bytes = bytes.len();
            entry.bytes = bytes;
            entry.bytes_loaded = true;
            true
        } else {
            false
        }
    }

    fn load_index(&mut self) {
        let Some(root) = self.root.clone() else {
            return;
        };
        let read_dir = match fs::read_dir(&root) {
            Ok(read_dir) => read_dir,
            Err(error) => {
                log::warn!(
                    "[content] index read failed for {}: {error}",
                    root.display()
                );
                return;
            }
        };
        let now = Instant::now();
        let now_ms = now_ms();
        for entry in read_dir.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let meta: ContentMeta = match fs::read(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            {
                Some(meta) => meta,
                None => {
                    log::warn!("[content] skipping unreadable metadata {}", path.display());
                    continue;
                }
            };
            if meta.content_id.is_empty() || meta.sha256.is_empty() {
                log::warn!("[content] skipping incomplete metadata {}", path.display());
                continue;
            }
            if !meta.pinned && meta.expires_at_ms <= now_ms {
                self.delete_files(&meta.content_id);
                continue;
            }
            self.entries.insert(
                meta.content_id.clone(),
                ContentEntry {
                    content_id: meta.content_id.clone(),
                    owners: meta.owners.clone(),
                    media_type: meta.media_type.clone(),
                    bytes: Vec::new(),
                    sha256: meta.sha256.clone(),
                    truncated: meta.truncated,
                    pinned: meta.pinned,
                    created_at: instant_from_ms(meta.created_at_ms, now, now_ms),
                    expires_at: if meta.pinned {
                        now + DEFAULT_CONTENT_TTL
                    } else {
                        instant_from_ms(meta.expires_at_ms, now, now_ms)
                    },
                    pin_key: meta.pin_key.clone(),
                    bytes_loaded: false,
                    size_bytes: meta.size_bytes,
                },
            );
        }
        self.remove_orphan_bodies(&root);
        self.evict_over_capacity();
    }

    /// 删除没有 metadata 的 `.bin`：进程在 body 落盘后、metadata 落盘前
    /// 被杀时会留下这类孤儿；对应 canonical fact 从未写入，可安全回收。
    fn remove_orphan_bodies(&self, root: &Path) {
        let known: HashSet<&str> = self.entries.keys().map(String::as_str).collect();
        let Ok(read_dir) = fs::read_dir(root) else {
            return;
        };
        for entry in read_dir.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("bin") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            if !known.contains(stem) {
                log::warn!("[content] removing orphan body {}", path.display());
                let _ = fs::remove_file(&path);
            }
        }
    }

    fn persist_entry(&self, entry: &ContentEntry) {
        let Some(root) = &self.root else {
            return;
        };
        let meta = ContentMeta {
            content_id: entry.content_id.clone(),
            owners: entry.owners.clone(),
            media_type: entry.media_type.clone(),
            sha256: entry.sha256.clone(),
            truncated: entry.truncated,
            pinned: entry.pinned,
            pin_key: entry.pin_key.clone(),
            created_at_ms: instant_to_ms(entry.created_at),
            expires_at_ms: instant_to_ms(entry.expires_at),
            size_bytes: entry.size_bytes,
        };
        let meta_bytes = match serde_json::to_vec(&meta) {
            Ok(bytes) => bytes,
            Err(error) => {
                log::warn!(
                    "[content] metadata serialize failed for {}: {error}",
                    entry.content_id
                );
                return;
            }
        };
        if entry.bytes_loaded
            && let Err(error) = write_atomic(
                &root.join(format!("{}.bin", entry.content_id)),
                &entry.bytes,
            )
        {
            log::warn!(
                "[content] body persist failed for {}: {error}",
                entry.content_id
            );
            return;
        }
        if let Err(error) = write_atomic(
            &root.join(format!("{}.json", entry.content_id)),
            &meta_bytes,
        ) {
            log::warn!(
                "[content] metadata persist failed for {}: {error}",
                entry.content_id
            );
        }
    }

    fn delete_files(&self, content_id: &str) {
        let Some(root) = &self.root else {
            return;
        };
        let _ = fs::remove_file(root.join(format!("{content_id}.json")));
        let _ = fs::remove_file(root.join(format!("{content_id}.bin")));
    }

    fn remove_entry(&mut self, content_id: &str) -> Option<ContentEntry> {
        let entry = self.entries.remove(content_id)?;
        self.delete_files(content_id);
        Some(entry)
    }

    fn sweep_expired(&mut self) {
        let now = Instant::now();
        let ids: Vec<String> = self
            .entries
            .values()
            .filter(|entry| !entry.pinned && entry.expires_at < now)
            .map(|entry| entry.content_id.clone())
            .collect();
        for content_id in ids {
            self.remove_entry(&content_id);
        }
    }

    /// 淘汰最早的**未 pin** 条目，直到回到容量上限。全被 pin 时提前退出
    /// （pinned 额度由 [`Self::put_pinned`] 单独兜底）。
    fn evict_over_capacity(&mut self) {
        while self.entries.len() > self.max_entries {
            let victim = self
                .entries
                .values()
                .filter(|entry| !entry.pinned)
                .min_by_key(|entry| entry.expires_at)
                .map(|entry| entry.content_id.clone());
            match victim {
                Some(victim) => {
                    self.remove_entry(&victim);
                }
                None => break,
            }
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}

fn instant_from_ms(ms: u64, now: Instant, now_ms: u64) -> Instant {
    if ms <= now_ms {
        now
    } else {
        now + Duration::from_millis(ms - now_ms)
    }
}

fn instant_to_ms(instant: Instant) -> u64 {
    let now = Instant::now();
    if instant <= now {
        now_ms()
    } else {
        now_ms().saturating_add((instant - now).as_millis().min(u64::MAX as u128) as u64)
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(&tmp, bytes)?;
    if path.exists() {
        let _ = fs::remove_file(path);
    }
    fs::rename(&tmp, path)
}

// SHA-256 hex 由 `qaqh_types::sha256_hex` 单源提供（PR-4-2，审计 #6）。
// content_id 需跨进程稳定，两处实现曾逐字节同形；现仅存 types 一份，
// 严禁再复制构造（换实现/换 crate 必须两侧同步评估 content_id 兼容性）。
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_get_round_trip_with_ownership() {
        let mut store = ContentStore::new();
        let id = store.put("s1", "text/plain", vec![1, 2, 3], false);
        let entry = store.get("s1", &id).expect("owner can read");
        assert_eq!(entry.bytes, vec![1, 2, 3]);
        assert_eq!(entry.media_type, "text/plain");
        assert_eq!(entry.owners, vec!["s1".to_string()]);
        // 其他会话无权读取
        assert!(store.get("s2", &id).is_none());
        // 错误 id
        assert!(store.get("s1", "nope").is_none());
    }

    #[test]
    fn sha256_is_deterministic_and_distinct() {
        assert_eq!(sha256_hex(b"abc"), sha256_hex(b"abc"));
        assert_ne!(sha256_hex(b"abc"), sha256_hex(b"abd"));
        assert_eq!(sha256_hex(b"abc").len(), 64);
    }

    #[test]
    fn session_release_frees_content() {
        let mut store = ContentStore::new();
        let id = store.put("s1", "text/plain", vec![1], false);
        store.put("s2", "text/plain", vec![2], false);
        assert_eq!(store.release_session("s1"), 1);
        assert!(store.get("s1", &id).is_none());
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn expired_entry_is_evicted_lazily() {
        let mut store = ContentStore::new();
        let id = store.put("s1", "text/plain", vec![1], false);
        // 直接改过期时间
        store.entries.get_mut(&id).expect("exists").expires_at =
            Instant::now() - Duration::from_secs(1);
        assert!(store.get("s1", &id).is_none());
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn large_content_flagged_for_externalization() {
        // 阈值判定属于调用方策略；此处验证常量（编译期守卫）
        const _: () = assert!(CONTENT_STORE_THRESHOLD_BYTES >= 10 * 1024 * 1024);
        let mut store = ContentStore::new();
        let big = vec![0_u8; CONTENT_STORE_THRESHOLD_BYTES];
        let id = store.put("s1", "application/octet-stream", big, true);
        let entry = store.get("s1", &id).expect("big content readable");
        assert!(entry.truncated);
        assert_eq!(entry.bytes.len(), CONTENT_STORE_THRESHOLD_BYTES);
    }

    // ── #345：pinned 交互正文 ──

    #[test]
    fn pinned_entry_survives_ttl_and_capacity_eviction() {
        let mut store = ContentStore::new();
        let pinned = store
            .put_pinned("s1", "application/json", b"{\"kind\":\"ask\"}".to_vec())
            .expect("pinned admitted");
        // TTL 到期也不清（pin 到交互终态）。
        store.entries.get_mut(&pinned).expect("exists").expires_at =
            Instant::now() - Duration::from_secs(1);
        assert!(store.get("s1", &pinned).is_some(), "pinned ignores TTL");

        // 容量淘汰只吃未 pin 条目。
        for index in 0..(store.max_entries + 8) {
            store.put(
                "s1",
                "text/plain",
                format!("line-{index}").into_bytes(),
                false,
            );
        }
        assert!(
            store.get("s1", &pinned).is_some(),
            "pinned survives eviction"
        );
    }

    #[test]
    fn pinned_quota_is_fail_closed_and_reopens_after_unpin() {
        let mut store = ContentStore::new();
        let mut ids = Vec::new();
        for index in 0..PINNED_MAX_ENTRIES_PER_SESSION {
            let id = store
                .put_pinned(
                    "s1",
                    "application/json",
                    format!("body-{index}").into_bytes(),
                )
                .expect("within entry quota");
            ids.push(id);
        }
        assert_eq!(
            store.put_pinned("s1", "application/json", b"overflow".to_vec()),
            Err(ContentQuotaExceeded)
        );
        // 其他会话不受影响（配额按 seed 计）。
        assert!(
            store
                .put_pinned("s2", "application/json", b"other".to_vec())
                .is_ok()
        );
        // unpin 之后腾出额度。
        assert!(store.unpin(&ids[0]));
        assert!(
            store
                .put_pinned("s1", "application/json", b"after-unpin".to_vec())
                .is_ok()
        );
    }

    #[test]
    fn pinned_byte_quota_is_enforced() {
        let mut store = ContentStore::new();
        store
            .put_pinned(
                "s1",
                "application/octet-stream",
                vec![0_u8; PINNED_MAX_BYTES_PER_SESSION],
            )
            .expect("exactly at quota");
        assert_eq!(
            store.put_pinned("s1", "application/octet-stream", vec![1_u8]),
            Err(ContentQuotaExceeded)
        );
    }

    #[test]
    fn unpin_restores_normal_ttl_and_lookup_by_id_ignores_session() {
        let mut store = ContentStore::new();
        let id = store
            .put_pinned("s1", "application/json", b"body".to_vec())
            .expect("pinned admitted");
        // get_any 不校验 seed（所有权由 daemon 校验条目的 owners）。
        let entry = store.get_any(&id).expect("lookup by id");
        assert_eq!(entry.owners, vec!["s1".to_string()]);
        assert!(entry.pinned);

        assert!(store.unpin(&id));
        assert!(!store.unpin(&id), "second unpin is a no-op");
        let entry = store.get_any(&id).expect("still readable");
        assert!(!entry.pinned);
        assert!(entry.expires_at > Instant::now());
    }

    // ── 持久化：重启后 metadata / body / pin 仍在 ──

    #[test]
    fn durable_store_round_trips_after_restart() {
        let temp = tempfile::tempdir().expect("tempdir");
        let id = {
            let mut store = ContentStore::with_root(temp.path());
            store.put("s1", "text/plain", b"persisted".to_vec(), false)
        };

        let mut reopened = ContentStore::with_root(temp.path());
        let entry = reopened
            .get("s1", &id)
            .expect("owner can read after restart");
        assert_eq!(entry.bytes, b"persisted");
        assert_eq!(entry.media_type, "text/plain");
        assert_eq!(entry.owners, vec!["s1".to_string()]);
        assert!(reopened.get("s2", &id).is_none());
    }

    #[test]
    fn durable_pinned_entry_survives_restart_and_unpins_by_key() {
        let temp = tempfile::tempdir().expect("tempdir");
        let id = {
            let mut store = ContentStore::with_root(temp.path());
            store
                .put_pinned_for(
                    "s1",
                    "application/json",
                    b"pending ask".to_vec(),
                    Some("int_ask_1"),
                )
                .expect("pinned admitted")
        };

        let mut reopened = ContentStore::with_root(temp.path());
        let entry = reopened.get("s1", &id).expect("pinned survives restart");
        assert!(entry.pinned);
        assert_eq!(reopened.unpin_key("s1", "int_ask_1"), 1);
        assert!(!reopened.get("s1", &id).expect("still readable").pinned);
    }

    #[test]
    fn durable_content_is_reference_counted_across_sessions() {
        let temp = tempfile::tempdir().expect("tempdir");
        let id = {
            let mut store = ContentStore::with_root(temp.path());
            store.put("s1", "text/plain", b"shared".to_vec(), false);
            store.put("s2", "text/plain", b"shared".to_vec(), false)
        };

        let mut reopened = ContentStore::with_root(temp.path());
        assert_eq!(reopened.get("s1", &id).expect("s1").bytes, b"shared");
        assert_eq!(reopened.get("s2", &id).expect("s2").bytes, b"shared");
        assert_eq!(reopened.release_session("s1"), 0);
        assert!(reopened.get("s1", &id).is_none());
        assert!(reopened.get("s2", &id).is_some());
        assert_eq!(reopened.release_session("s2"), 1);
        assert!(reopened.get_any(&id).is_none());
    }

    #[test]
    fn durable_expired_unpinned_entry_is_pruned_on_restart() {
        let temp = tempfile::tempdir().expect("tempdir");
        let id = {
            let mut store = ContentStore::with_root(temp.path());
            let id = store.put("s1", "text/plain", b"stale".to_vec(), false);
            store.entries.get_mut(&id).expect("exists").expires_at =
                Instant::now() - Duration::from_secs(1);
            // 模拟进程退出前 metadata 已按过期时间落盘。
            let entry = store.entries.get(&id).cloned().expect("entry");
            store.persist_entry(&entry);
            id
        };

        let mut reopened = ContentStore::with_root(temp.path());
        assert!(reopened.get("s1", &id).is_none());
        assert_eq!(reopened.len(), 0);
    }
}
