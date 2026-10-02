//! 快照存储（紧急备份核心）。默认放工作区之外：`<data_dir>/spy/<hash>`，
//! 模型脚本把工作区整个删掉也动不到备份，且不污染目标仓库。
//! `Store::open` 的 `override_dir` 可显式指定；`QAQH_SPY_DIR` 覆盖根（仍按工作区
//! 哈希分子目录，保持多工作区隔离），`QAQH_DATA_DIR` 经 `platform::data_dir()`
//! 整体重定向——与 `QAQH_JOURNAL_DIR` 同构，测试 harness 可直接隔离。
//!
//! 布局：
//! - `objects/xx/yyyy...`   内容寻址 blob（SHA-256），未变内容零重复；temp+rename 原子落盘
//! - `journal.jsonl`        append-only 变更流水，每行一个 change
//! - `manifests/<scan>.json 每次扫描的 path→sha 全量清单——"回到时刻 T"不需要重放日志
//! - `state.json`           last_scan / last_report / seq
//!
//! 崩溃安全：先写 blob，后 append journal，再写 manifest/state；恢复前校验 sha。

use std::collections::BTreeMap;
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

/// manifest 的单文件条目：内容 sha + 用于跳过重读的 stat 缓存。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub sha: String,
    /// UNIX 纪元起的纳秒
    pub mtime: u64,
    pub size: u64,
}

/// 一次扫描的全量清单。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub id: String,
    pub ts: String,
    pub trigger: String,
    #[allow(dead_code)]
    pub base: Option<String>,
    pub files: BTreeMap<String, FileEntry>,
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
    pub fn lock(&self) -> Result<StoreLock> {
        let path = self.dir.join("lock");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
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
    fs::write(&tmp, serde_json::to_string_pretty(value)?)?;
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
}
