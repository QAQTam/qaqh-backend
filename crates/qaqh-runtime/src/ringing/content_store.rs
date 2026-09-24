//! 大内容外置存储（PLAN 大内容外置）。
//!
//! - 工具完整输出、compact archive、超大 diff、诊断内容进入 content store；
//! - 事件只携带 `RingingContentRef`（content_id、media_type、bytes、sha256、truncated）；
//! - 客户端通过带鉴权的 HTTP GET 按需读取（range/分页）；
//! - content 设置会话所有权与生命周期；
//! - **API key、provider 原始响应和未脱敏错误禁止进入 content store**。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use qaqh_types::sha256_hex;

/// 超过该阈值的内容应外置（10 MiB）。
///
/// ⚠ 这是**传输保护阀**，而非常规路径。工具结果走向本 store 的唯一入口是
/// `registry::externalize_large_content`，在标准模式下模型文本已被
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
pub const PINNED_MAX_ENTRIES_PER_SEED: usize = 64;

/// 单会话 pinned 字节上限（#345）。
pub const PINNED_MAX_BYTES_PER_SEED: usize = 4 * 1024 * 1024;

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
    pub seed: String,
    pub media_type: String,
    pub bytes: Vec<u8>,
    pub sha256: String,
    pub truncated: bool,
    /// `true` = 被一个活交互引用：不吃 TTL、不被容量淘汰，直到显式 unpin
    /// 或会话释放（#345）。
    pub pinned: bool,
    pub created_at: Instant,
    pub expires_at: Instant,
}

/// 大内容存储（有界、会话所有权、TTL 清理）。
#[derive(Debug, Default)]
pub struct ContentStore {
    entries: HashMap<String, ContentEntry>,
    max_entries: usize,
}

impl ContentStore {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            max_entries: 256,
        }
    }

    /// 存入内容。返回 content_id（SHA-256 前 32 hex 或随机）。
    pub fn put(&mut self, seed: &str, media_type: &str, bytes: Vec<u8>, truncated: bool) -> String {
        let content_id = sha256_hex(&bytes);
        let now = Instant::now();
        self.entries.insert(
            content_id.clone(),
            ContentEntry {
                content_id: content_id.clone(),
                seed: seed.to_string(),
                media_type: media_type.to_string(),
                sha256: content_id.clone(),
                bytes,
                truncated,
                pinned: false,
                created_at: now,
                expires_at: now + DEFAULT_CONTENT_TTL,
            },
        );
        self.evict_over_capacity();
        content_id
    }

    /// 存入**被活交互引用**的内容（#345）：不吃 TTL、不被容量淘汰。
    ///
    /// 超配额返回 [`ContentQuotaExceeded`]（fail-closed）——调用方必须把交互
    /// 收尾，而不是呈现一个取不到正文的 modal。
    pub fn put_pinned(
        &mut self,
        seed: &str,
        media_type: &str,
        bytes: Vec<u8>,
    ) -> Result<String, ContentQuotaExceeded> {
        let mut pinned_entries = 0usize;
        let mut pinned_bytes = 0usize;
        for entry in self.entries.values().filter(|e| e.pinned && e.seed == seed) {
            pinned_entries += 1;
            pinned_bytes = pinned_bytes.saturating_add(entry.bytes.len());
        }
        if pinned_entries >= PINNED_MAX_ENTRIES_PER_SEED
            || pinned_bytes.saturating_add(bytes.len()) > PINNED_MAX_BYTES_PER_SEED
        {
            return Err(ContentQuotaExceeded);
        }
        let content_id = sha256_hex(&bytes);
        let now = Instant::now();
        self.entries.insert(
            content_id.clone(),
            ContentEntry {
                content_id: content_id.clone(),
                seed: seed.to_string(),
                media_type: media_type.to_string(),
                sha256: content_id.clone(),
                bytes,
                truncated: false,
                pinned: true,
                created_at: now,
                expires_at: now + DEFAULT_CONTENT_TTL,
            },
        );
        self.evict_over_capacity();
        Ok(content_id)
    }

    /// 解除 pin：条目回到普通 TTL / 容量淘汰语义。
    pub fn unpin(&mut self, content_id: &str) -> bool {
        let Some(entry) = self.entries.get_mut(content_id) else {
            return false;
        };
        if !entry.pinned {
            return false;
        }
        entry.pinned = false;
        entry.expires_at = Instant::now() + DEFAULT_CONTENT_TTL;
        true
    }

    /// 淘汰最早的**未 pin** 条目，直到回到容量上限。全被 pin 时提前退出
    /// （pinned 额度由 [`put_pinned`] 单独兜底）。
    fn evict_over_capacity(&mut self) {
        while self.entries.len() > self.max_entries {
            let victim = self
                .entries
                .values()
                .filter(|e| !e.pinned)
                .min_by_key(|e| e.expires_at)
                .map(|e| e.content_id.clone());
            match victim {
                Some(victim) => {
                    self.entries.remove(&victim);
                }
                None => break,
            }
        }
    }

    /// 读取（校验所有权）。过期条目惰性清理。
    pub fn get(&mut self, seed: &str, content_id: &str) -> Option<ContentEntry> {
        let entry = self.entries.get(content_id)?;
        if entry.seed != seed || (!entry.pinned && entry.expires_at < Instant::now()) {
            self.entries.remove(content_id);
            return None;
        }
        Some(entry.clone())
    }

    /// 按 id 读取（**不校验所有权**，调用方负责）。v2 的 content 端点不带 seed，
    /// 由 daemon 拿条目的 `seed` 再校验调用方归属。
    pub fn get_any(&mut self, content_id: &str) -> Option<ContentEntry> {
        let entry = self.entries.get(content_id)?;
        if !entry.pinned && entry.expires_at < Instant::now() {
            self.entries.remove(content_id);
            return None;
        }
        Some(entry.clone())
    }

    /// 会话关闭/切流时释放该会话内容。
    pub fn release_session(&mut self, seed: &str) -> usize {
        let before = self.entries.len();
        self.entries.retain(|_, e| e.seed != seed);
        before - self.entries.len()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
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
        for index in 0..PINNED_MAX_ENTRIES_PER_SEED {
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
                vec![0_u8; PINNED_MAX_BYTES_PER_SEED],
            )
            .expect("exactly at quota");
        assert_eq!(
            store.put_pinned("s1", "application/octet-stream", vec![1_u8]),
            Err(ContentQuotaExceeded)
        );
    }

    #[test]
    fn unpin_restores_normal_ttl_and_lookup_by_id_ignores_seed() {
        let mut store = ContentStore::new();
        let id = store
            .put_pinned("s1", "application/json", b"body".to_vec())
            .expect("pinned admitted");
        // get_any 不校验 seed（所有权由 daemon 校验条目的 seed）。
        let entry = store.get_any(&id).expect("lookup by id");
        assert_eq!(entry.seed, "s1");
        assert!(entry.pinned);

        assert!(store.unpin(&id));
        assert!(!store.unpin(&id), "second unpin is a no-op");
        let entry = store.get_any(&id).expect("still readable");
        assert!(!entry.pinned);
        assert!(entry.expires_at > Instant::now());
    }
}
