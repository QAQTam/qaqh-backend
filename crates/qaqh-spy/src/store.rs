//! 快照存储（紧急备份核心）。默认放工作区之外：`<data_dir>/spy/<hash>`，
//! 模型脚本把工作区整个删掉也动不到备份，且不污染目标仓库。
//! `Store::open` 的 `override_dir` 可显式指定；`QAQH_SPY_DIR` 覆盖根（仍按工作区
//! 哈希分子目录，保持多工作区隔离），`QAQH_DATA_DIR` 经 `platform::data_dir()`
//! 整体重定向——与 `QAQH_JOURNAL_DIR` 同构，测试 harness 可直接隔离。
//!
//! 布局：
//! - `objects/xx/yyyy...`   内容寻址 blob（SHA-256），未变内容零重复；temp+rename 原子落盘
//! - `journal.jsonl`        append-only 变更流水，每行一个 change
//! - `manifests/<scan>.json 一次扫描的清单：锚点存全量 path→sha，增量相对 base 只存变化条目 + tombstones——"回到时刻 T"不需要重放日志
//! - `state.json`           last_scan / last_report / seq
//!
//! 崩溃安全：先写 blob，后 append journal，再写 manifest/state；恢复前校验 sha。

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// journal 中的一条变更（审计的原子单位）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    /// 变更 id（undo 的定位键），形如 `s0001730000000000_0001#0`
    pub id: String,
    /// 所属扫描（manifest id）
    pub scan: String,
    /// RFC3339 时间戳
    pub ts: String,
    /// 工作区内相对路径（'/' 分隔）
    pub path: String,
    pub status: ChangeStatus,
    /// 变更前内容 sha（新增文件为 None）
    #[serde(default)]
    pub before: Option<String>,
    /// 变更后内容 sha（删除文件为 None）
    #[serde(default)]
    pub after: Option<String>,
    #[serde(default)]
    pub size_before: Option<u64>,
    #[serde(default)]
    pub size_after: Option<u64>,
    /// 触发来源（tool_start / tool_end / periodic / manual）
    pub trigger: String,
}

/// 变更类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeStatus {
    Added,
    Modified,
    Deleted,
}

/// 存储内部状态（state.json）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    pub last_scan: Option<String>,
    pub last_report: Option<String>,
    pub seq: u64,
}

/// 一次 GC 的回收统计。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GcOutcome {
    pub dropped_manifests: usize,
    pub dropped_blobs: usize,
}

/// manifest 的单文件条目：内容 sha + 用于跳过重读的 stat 缓存。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub sha: String,
    /// UNIX 纪元起的纳秒
    pub mtime: u64,
    pub size: u64,
}

/// 一次扫描的清单。锚点（`anchor = true`）存全量；增量（`anchor = false`）
/// 相对 `base` 只存变化条目，删除以 `tombstones` 墓碑表达。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub id: String,
    pub ts: String,
    pub trigger: String,
    pub base: Option<String>,
    /// 兼容存量数据：字段缺失（旧 manifest）视为全量锚点
    #[serde(default = "default_true")]
    pub anchor: bool,
    /// 锚点链深：anchor = 0，delta = base.depth + 1
    #[serde(default)]
    pub depth: u32,
    pub files: BTreeMap<String, FileEntry>,
    /// delta 专用：相对 base 被删除的路径
    #[serde(default)]
    pub tombstones: Vec<String>,
}

fn default_true() -> bool {
    true
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// 内容寻址的快照存储。
pub struct Store {
    dir: PathBuf,
}

impl Store {
    /// 打开（创建）存储。`override_dir` 为 None 时走 `default_dir`：以
    /// `QAQH_SPY_DIR`（或 `platform::data_dir()`）为根，下挂工作区绝对路径
    /// 哈希前 16 位——备份始终在工作区之外。
    pub fn open(workspace: &Path, override_dir: Option<&Path>) -> Result<Store> {
        let dir = match override_dir {
            Some(p) => p.to_path_buf(),
            None => default_dir(workspace)?,
        };
        fs::create_dir_all(&dir)?;
        fs::create_dir_all(dir.join("objects"))?;
        fs::create_dir_all(dir.join("manifests"))?;
        Ok(Store { dir })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    // ---------- blob ----------

    pub fn blob_path(&self, sha: &str) -> PathBuf {
        // sha256 hex 恒为 64 位 ASCII，分片前缀只为摊平目录数。
        let shard = sha.get(..2).unwrap_or(sha);
        self.dir.join("objects").join(shard).join(sha)
    }

    /// 内容寻址写 blob（已存在则跳过），返回 sha。temp+rename 原子落盘。
    pub fn write_blob(&self, bytes: &[u8]) -> Result<String> {
        let sha = sha256_hex(bytes);
        let path = self.blob_path(&sha);
        if path.exists() {
            return Ok(sha);
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, bytes)?;
        fs::rename(&tmp, &path)?;
        Ok(sha)
    }

    /// 读 blob 并校验完整性。
    pub fn read_blob(&self, sha: &str) -> Result<Vec<u8>> {
        if sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("非法 sha: {sha}");
        }
        let bytes =
            fs::read(self.blob_path(sha)).with_context(|| format!("读 blob 失败: {sha}"))?;
        let actual = sha256_hex(&bytes);
        if actual != sha {
            bail!("blob 损坏: {sha} (实际 {actual})");
        }
        Ok(bytes)
    }

    // ---------- journal ----------

    /// append-only 追加变更流水。
    pub fn append_journal(&self, changes: &[Change]) -> Result<()> {
        if changes.is_empty() {
            return Ok(());
        }
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join("journal.jsonl"))?;
        for c in changes {
            f.write_all(serde_json::to_string(c)?.as_bytes())?;
            f.write_all(b"\n")?;
        }
        f.flush()?;
        Ok(())
    }

    pub fn read_journal(&self) -> Result<Vec<Change>> {
        let p = self.dir.join("journal.jsonl");
        if !p.exists() {
            return Ok(Vec::new());
        }
        let text = fs::read_to_string(&p)?;
        let mut out = Vec::new();
        for (i, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let c: Change = serde_json::from_str(line)
                .with_context(|| format!("journal 第 {} 行损坏", i + 1))?;
            out.push(c);
        }
        Ok(out)
    }

    // ---------- manifest / state ----------

    pub fn save_manifest(&self, m: &Manifest) -> Result<()> {
        atomic_write_json(
            &self.dir.join("manifests").join(format!("{}.json", m.id)),
            m,
        )
    }

    /// 物化任意 manifest 为全量清单：沿 base 链上溯到最近锚点，
    /// 自锚点向下逐层应用增量条目与删除墓碑。链深有界（锚点间隔）。
    pub fn materialize_manifest(&self, id: &str) -> Result<BTreeMap<String, FileEntry>> {
        let mut chain = Vec::new();
        let mut cur = id.to_string();
        loop {
            let m = self.load_manifest(&cur)?;
            let (is_anchor, next) = (m.anchor, m.base.clone());
            chain.push(m);
            if is_anchor {
                break;
            }
            cur = next.context("增量 manifest 缺少 base 指针，链无法物化")?;
        }
        let mut files = chain.last().context("manifest 链不应为空")?.files.clone();
        for m in chain.iter().rev().skip(1) {
            for (k, v) in &m.files {
                files.insert(k.clone(), v.clone());
            }
            for t in &m.tombstones {
                files.remove(t);
            }
        }
        Ok(files)
    }

    pub fn load_manifest(&self, id: &str) -> Result<Manifest> {
        // id 只允许安全字符，防路径注入
        if !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'#')
        {
            bail!("非法 scan id: {id}");
        }
        let text = fs::read_to_string(self.dir.join("manifests").join(format!("{id}.json")))
            .with_context(|| format!("manifest 不存在: {id}"))?;
        Ok(serde_json::from_str(&text)?)
    }

    /// 全部 manifest id，按时间序（id 字典序 == 时间序）。
    pub fn all_manifest_ids(&self) -> Result<Vec<String>> {
        let mut ids = Vec::new();
        for e in fs::read_dir(self.dir.join("manifests"))? {
            let name = e?.file_name().to_string_lossy().to_string();
            if let Some(id) = name.strip_suffix(".json") {
                ids.push(id.to_string());
            }
        }
        ids.sort();
        Ok(ids)
    }

    /// 保留窗口 GC：回收窗口外的 manifest、journal 条目与孤儿 blob。
    ///
    /// 保留最近 `keep` 份 manifest 为窗口；journal 压缩到
    /// `scan >= 最老保留 manifest id`（id 字典序即时间序）；blob 只保留被
    /// 保留 manifest 与窗口内 journal 条目（before/after）引用的内容。
    /// 代价：undo/restore 只保证窗口内可回溯，窗口外的历史变更不可恢复。
    ///
    /// `lock_wait` 为存储锁等待上界；争锁超时返回 Err——调用方在高频路径
    /// （工具批边界 / `Session::open`）上应传小值并容忍本次跳过。
    pub fn gc(&self, keep: usize, lock_wait: std::time::Duration) -> Result<GcOutcome> {
        let _guard = self.lock_timeout(lock_wait)?;
        let ids = self.all_manifest_ids()?;
        if ids.len() <= keep {
            return Ok(GcOutcome {
                dropped_manifests: 0,
                dropped_blobs: 0,
            });
        }
        let cut = ids.len() - keep;
        let oldest_kept = ids[cut].clone();

        // 0) 链感知：窗口最老一份若是增量，其 base 链上溯到锚点途经的
        //    manifest 虽在窗口外也必须保留，否则链断、物化失败。
        let mut chain_keep: HashSet<String> = HashSet::new();
        let mut cur = oldest_kept.clone();
        loop {
            let m = self.load_manifest(&cur)?;
            let (is_anchor, next) = (m.anchor, m.base.clone());
            chain_keep.insert(cur);
            if is_anchor {
                break;
            }
            cur = next.context("增量 manifest 缺少 base 指针，GC 无法定位锚点")?;
        }

        // 1) 引用集：保留 manifest 物化为全量后的条目 + 窗口内 journal 的
        //    before/after（不能用 delta 的稀疏条目，否则会误删活跃 blob）
        let mut referenced: HashSet<String> = HashSet::new();
        for id in &ids[cut..] {
            for e in self.materialize_manifest(id)?.values() {
                referenced.insert(e.sha.clone());
            }
        }
        let retained: Vec<Change> = self
            .read_journal()?
            .into_iter()
            .filter(|c| c.scan >= oldest_kept)
            .collect();
        for c in &retained {
            if let Some(b) = &c.before {
                referenced.insert(b.clone());
            }
            if let Some(a) = &c.after {
                referenced.insert(a.clone());
            }
        }

        // 2) journal 压缩（原子重写；崩溃后最坏残留 = 未删的旧 manifest，
        //    下次 GC 再收）
        let mut buf = String::new();
        for c in &retained {
            buf.push_str(&serde_json::to_string(c)?);
            buf.push('\n');
        }
        atomic_write_bytes(&self.dir.join("journal.jsonl"), buf.as_bytes())?;

        // 3) 删窗口外 manifest（链感知：锚点链途经的保留）
        let mut dropped_manifests = 0usize;
        for id in &ids[..cut] {
            if chain_keep.contains(id) {
                continue;
            }
            fs::remove_file(self.dir.join("manifests").join(format!("{id}.json")))?;
            dropped_manifests += 1;
        }

        // 4) 孤儿 blob 回收（内容寻址：不在引用集即不可达）
        let mut dropped_blobs = 0usize;
        for shard in fs::read_dir(self.dir.join("objects"))? {
            let shard = shard?.path();
            if !shard.is_dir() {
                continue;
            }
            for f in fs::read_dir(&shard)? {
                let p = f?.path();
                let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                if !referenced.contains(name) {
                    fs::remove_file(&p)?;
                    dropped_blobs += 1;
                }
            }
        }
        Ok(GcOutcome {
            dropped_manifests,
            dropped_blobs,
        })
    }

    pub fn load_state(&self) -> Result<State> {
        let p = self.dir.join("state.json");
        if !p.exists() {
            return Ok(State::default());
        }
        Ok(serde_json::from_str(&fs::read_to_string(&p)?)?)
    }

    pub fn save_state(&self, s: &State) -> Result<()> {
        atomic_write_json(&self.dir.join("state.json"), s)
    }

    // ---------- 并发 ----------

    /// 跨线程/跨进程的简单文件锁；崩溃残留的锁 60s 后可抢占。
    /// 获取存储锁，最长等待 15 秒（人工 / CLI 路径的默认上界）。
    pub fn lock(&self) -> Result<StoreLock> {
        self.lock_timeout(std::time::Duration::from_secs(15))
    }

    /// 带等待上界的加锁。库接入（每批工具边界）应传**小值**：争用时宁可跳过
    /// 本次扫描，也不把调用方（agent 循环）阻塞到锁超时——超过 `wait` 返回 Err。
    pub fn lock_timeout(&self, wait: std::time::Duration) -> Result<StoreLock> {
        let path = self.dir.join("lock");
        let deadline = std::time::Instant::now() + wait;
        loop {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut f) => {
                    let _ = writeln!(f, "pid={}", std::process::id());
                    return Ok(StoreLock { path });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if let Ok(age) = fs::metadata(&path)?.modified()?.elapsed()
                        && age > std::time::Duration::from_secs(60)
                    {
                        let _ = fs::remove_file(&path);
                        continue;
                    }
                    if std::time::Instant::now() >= deadline {
                        bail!("获取存储锁超时（另一个扫描正在进行？）");
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
}

pub struct StoreLock {
    path: PathBuf,
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

// ---------- 工具函数 ----------

pub(crate) fn atomic_write_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("qaqh-spy-tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

fn atomic_write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_string(value)?)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

fn default_dir(workspace: &Path) -> Result<PathBuf> {
    let base = std::env::var("QAQH_SPY_DIR")
        .ok()
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(qaqh_types::platform::data_dir);
    // 按工作区绝对路径哈希取前 16 位：同一存储根下多工作区互不覆盖。
    let hash = sha256_hex(workspace.to_string_lossy().as_bytes());
    let key = hash.get(..16).unwrap_or(hash.as_str());
    Ok(base.join("spy").join(key))
}

#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(test)]
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
pub(crate) fn tmp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "qaqh-spy-{tag}-{}-{}",
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_roundtrip_and_dedupe() {
        let dir = tmp_dir("blob");
        let store = Store::open(&dir, Some(&dir)).unwrap();
        let sha = store.write_blob(b"hello").unwrap();
        assert_eq!(sha, sha256_hex(b"hello"));
        assert_eq!(store.read_blob(&sha).unwrap(), b"hello");
        // 同内容去重
        assert_eq!(store.write_blob(b"hello").unwrap(), sha);
        // 损坏校验
        fs::write(store.blob_path(&sha), b"tampered").unwrap();
        assert!(store.read_blob(&sha).is_err());
    }

    #[test]
    fn journal_and_manifest_roundtrip() {
        let dir = tmp_dir("journal");
        let store = Store::open(&dir, Some(&dir)).unwrap();
        let c = Change {
            id: "s1#0".into(),
            scan: "s1".into(),
            ts: "2026-10-02T00:00:00Z".into(),
            path: "a.txt".into(),
            status: ChangeStatus::Modified,
            before: Some("a".repeat(64)),
            after: Some("b".repeat(64)),
            size_before: Some(1),
            size_after: Some(2),
            trigger: "manual".into(),
        };
        store.append_journal(std::slice::from_ref(&c)).unwrap();
        assert_eq!(store.read_journal().unwrap(), vec![c.clone()]);

        let m = Manifest {
            id: "s1".into(),
            ts: "t".into(),
            trigger: "manual".into(),
            base: None,
            anchor: true,
            depth: 0,
            tombstones: Vec::new(),
            files: BTreeMap::from([(
                "a.txt".into(),
                FileEntry {
                    sha: "a".repeat(64),
                    mtime: 1,
                    size: 2,
                },
            )]),
        };
        store.save_manifest(&m).unwrap();
        assert_eq!(store.load_manifest("s1").unwrap().files.len(), 1);
        assert_eq!(store.all_manifest_ids().unwrap(), vec!["s1".to_string()]);
        assert!(store.load_manifest("missing").is_err());
    }

    #[test]
    fn gc_prunes_manifests_journal_and_orphan_blobs() {
        let dir = tmp_dir("gc");
        let store = Store::open(&dir, Some(&dir)).unwrap();
        let v1 = store.write_blob(b"v1").unwrap();
        let v2 = store.write_blob(b"v2").unwrap();
        let v3 = store.write_blob(b"v3").unwrap();
        let orphan = store.write_blob(b"orphan").unwrap();

        // 三份 manifest（a.txt: v1→v2→v3）+ 两条 journal 条目
        let mk = |seq: u64, sha: &str| Manifest {
            id: format!("s{seq:016}_0001"),
            ts: "t".into(),
            trigger: "manual".into(),
            base: None,
            anchor: true,
            depth: 0,
            tombstones: Vec::new(),
            files: BTreeMap::from([(
                "a.txt".into(),
                FileEntry {
                    sha: sha.into(),
                    mtime: 1,
                    size: 2,
                },
            )]),
        };
        store.save_manifest(&mk(1, &v1)).unwrap();
        store.save_manifest(&mk(2, &v2)).unwrap();
        store.save_manifest(&mk(3, &v3)).unwrap();
        let ch = |seq: u64, before: &str, after: &str| Change {
            id: format!("s{seq:016}_0001#0"),
            scan: format!("s{seq:016}_0001"),
            ts: "t".into(),
            path: "a.txt".into(),
            status: ChangeStatus::Modified,
            before: Some(before.into()),
            after: Some(after.into()),
            size_before: Some(2),
            size_after: Some(2),
            trigger: "manual".into(),
        };
        store
            .append_journal(&[ch(2, &v1, &v2), ch(3, &v2, &v3)])
            .unwrap();

        // 保留最近 2 份：m1 丢弃；journal 全在窗口内 → v1 经 before 引用保留；
        // 唯一孤儿是手动写入的 orphan blob
        let out = store.gc(2, std::time::Duration::from_millis(100)).unwrap();
        assert_eq!(out.dropped_manifests, 1);
        assert_eq!(out.dropped_blobs, 1);
        assert_eq!(
            store.all_manifest_ids().unwrap(),
            vec![format!("s{:016}_0001", 2), format!("s{:016}_0001", 3)]
        );
        assert_eq!(store.read_journal().unwrap().len(), 2);
        for sha in [&v1, &v2, &v3] {
            assert!(store.read_blob(sha).is_ok());
        }
        assert!(store.read_blob(&orphan).is_err());

        // 未超窗口：空操作
        let again = store.gc(2, std::time::Duration::from_millis(100)).unwrap();
        assert_eq!(again.dropped_manifests + again.dropped_blobs, 0);
    }

    #[test]
    fn manifest_chain_materialize() {
        let dir = tmp_dir("chain");
        let store = Store::open(&dir, Some(&dir)).unwrap();
        let s1 = store.write_blob(b"one").unwrap();
        let s2 = store.write_blob(b"two").unwrap();
        let s3 = store.write_blob(b"three").unwrap();
        let fe = |sha: &str| FileEntry {
            sha: sha.into(),
            mtime: 1,
            size: 3,
        };

        // anchor(m1)：a=one, b=two
        store
            .save_manifest(&Manifest {
                id: format!("s{:016}_0001", 1),
                ts: "t".into(),
                trigger: "manual".into(),
                base: None,
                anchor: true,
                depth: 0,
                files: BTreeMap::from([("a.txt".into(), fe(&s1)), ("b.txt".into(), fe(&s2))]),
                tombstones: Vec::new(),
            })
            .unwrap();
        // delta(m2)：a→three（改），b 删除（墓碑），c 新增 one
        store
            .save_manifest(&Manifest {
                id: format!("s{:016}_0001", 2),
                ts: "t".into(),
                trigger: "tool_end".into(),
                base: Some(format!("s{:016}_0001", 1)),
                anchor: false,
                depth: 1,
                files: BTreeMap::from([("a.txt".into(), fe(&s3)), ("c.txt".into(), fe(&s1))]),
                tombstones: vec!["b.txt".into()],
            })
            .unwrap();

        // 物化 delta：改/增/删三项语义全部生效
        let full = store
            .materialize_manifest(&format!("s{:016}_0001", 2))
            .unwrap();
        assert_eq!(full.get("a.txt").unwrap().sha, s3);
        assert_eq!(full.get("c.txt").unwrap().sha, s1);
        assert!(!full.contains_key("b.txt"));
        assert_eq!(full.len(), 2);

        // 锚点物化 = 原样
        let base = store
            .materialize_manifest(&format!("s{:016}_0001", 1))
            .unwrap();
        assert_eq!(base.len(), 2);
    }

    #[test]
    fn gc_keeps_anchor_chain_outside_window() {
        let dir = tmp_dir("gc-chain");
        let store = Store::open(&dir, Some(&dir)).unwrap();
        let s1 = store.write_blob(b"one").unwrap();
        let fe = |sha: &str| FileEntry {
            sha: sha.into(),
            mtime: 1,
            size: 3,
        };
        // anchor(m1) → delta(m2) → delta(m3)
        store
            .save_manifest(&Manifest {
                id: format!("s{:016}_0001", 1),
                ts: "t".into(),
                trigger: "manual".into(),
                base: None,
                anchor: true,
                depth: 0,
                files: BTreeMap::from([("a.txt".into(), fe(&s1))]),
                tombstones: Vec::new(),
            })
            .unwrap();
        for seq in [2u64, 3] {
            store
                .save_manifest(&Manifest {
                    id: format!("s{:016}_0001", seq),
                    ts: "t".into(),
                    trigger: "tool_end".into(),
                    base: Some(format!("s{:016}_0001", seq - 1)),
                    anchor: false,
                    depth: (seq - 1) as u32,
                    files: BTreeMap::new(),
                    tombstones: Vec::new(),
                })
                .unwrap();
        }

        // keep=1：窗口只有 m3，但 m1/m2 是 m3 的 base 链，必须保留
        let out = store.gc(1, std::time::Duration::from_millis(100)).unwrap();
        assert_eq!(out.dropped_manifests, 0);
        assert_eq!(store.all_manifest_ids().unwrap().len(), 3);
        // 物化仍成功 = 链未断
        assert!(
            store
                .materialize_manifest(&format!("s{:016}_0001", 3))
                .is_ok()
        );
    }
}
