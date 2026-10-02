//! 状态扫描（整个系统的事实来源）：
//! 1. `ignore` crate 走一遍工作区（自带目录黑名单，不吃 .gitignore——备份宁可多，
//!    隐藏文件如 .env 也要进快照）；
//! 2. 与上一份 manifest 逐文件比对 (mtime, size)，只对变化文件做 SHA-256——
//!    qaqh-backend 728 个源文件稳态扫描预计 <100ms；
//! 3. 差分按固定顺序落盘：blob → journal → manifest → state，任何一步中断
//!    都不会产生悬空引用。
//!
//! 为什么不用 notify 事件流当事实来源：事件会漏（rename 链、短于防抖间隔的连续写、
//! 原子写让观察者读到中间态），而全量扫描结果确定、可重放、与 git CLI 完全无关。

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use ignore::WalkBuilder;

use crate::store::{Change, ChangeStatus, FileEntry, Manifest, Store};

/// 扫描参数。
#[derive(Debug, Clone)]
pub struct ScanOpts {
    /// 单文件大小上限，超过则不纳入快照（也无法为其回滚）
    pub max_file_bytes: u64,
    /// 存储锁最长等待。挂在工具批边界时用短值：并发会话争锁则跳过本次
    /// 扫描（调用方退化为“本批无报告”），不阻塞 agent 循环。
    pub lock_wait: std::time::Duration,
    /// 目录名黑名单（任何层级下同名目录整棵剪掉）
    pub exclude_dirs: Vec<String>,
}

impl Default for ScanOpts {
    fn default() -> Self {
        ScanOpts {
            max_file_bytes: 2 * 1024 * 1024,
            lock_wait: std::time::Duration::from_millis(1_000),
            exclude_dirs: [
                ".git",
                "node_modules",
                "target",
                "dist",
                "build",
                "__pycache__",
                ".venv",
                "venv",
                ".codegraph",
                ".idea",
            ]
            .into_iter()
            .map(String::from)
            .collect(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ScanOutcome {
    pub id: String,
    pub files: usize,
    /// 首次扫描（只有 manifest，不刷 journal）
    pub baseline: bool,
    pub added: usize,
    pub modified: usize,
    pub deleted: usize,
}

pub(crate) fn run_scan(
    root: &Path,
    store: &Store,
    trigger: &str,
    opts: &ScanOpts,
) -> Result<ScanOutcome> {
    let _guard = store.lock_timeout(opts.lock_wait)?;
    let state = store.load_state()?;
    let prev = match &state.last_scan {
        Some(id) => Some(store.load_manifest(id)?),
        None => None,
    };
    let is_baseline = prev.is_none();
    let prev_files = prev.as_ref().map(|m| m.files.clone()).unwrap_or_default();

    // 走库 + 哈希（mtime/size 命中缓存则不重读）
    let extra = [store.dir().to_path_buf()];
    let cx = WalkCx {
        root,
        opts,
        extra_exclude: &extra,
    };
    let mut files: BTreeMap<String, FileEntry> = BTreeMap::new();
    for (path, rel, meta) in walk(&cx)? {
        if meta.len() > opts.max_file_bytes {
            continue; // TODO: 记 skipped 事件
        }
        let mtime = mtime_nanos(&meta)?;
        let size = meta.len();
        // (mtime, size) 缓存命中即视为未变：不重读、不重算 sha。
        if let Some(p) = prev_files.get(&rel)
            && p.mtime == mtime
            && p.size == size
        {
            files.insert(rel, p.clone());
            continue;
        }
        let bytes = fs::read(&path).with_context(|| format!("读文件失败: {rel}"))?;
        let sha = store.write_blob(&bytes)?; // 先落 blob（崩溃安全顺序的第一步）
        files.insert(rel, FileEntry { sha, mtime, size });
    }
    let n_files = files.len();

    // 差分 → journal（基线扫描不刷屏）
    // 展示用时间戳；排序键是 scan id（`s{millis:016}_{seq:04}`），不依赖本串。
    let now = chrono::Utc::now().to_rfc3339();
    let mut changes: Vec<Change> = Vec::new();
    let (mut n_add, mut n_mod, mut n_del) = (0usize, 0usize, 0usize);
    if !is_baseline {
        let push = |status: ChangeStatus,
                    rel: &str,
                    before: Option<String>,
                    after: Option<String>,
                    sb: Option<u64>,
                    sa: Option<u64>,
                    out: &mut Vec<Change>| {
            out.push(Change {
                id: String::new(),
                scan: String::new(),
                ts: now.clone(),
                path: rel.to_string(),
                status,
                before,
                after,
                size_before: sb,
                size_after: sa,
                trigger: trigger.to_string(),
            });
        };
        for (rel, e) in &files {
            match prev_files.get(rel) {
                None => {
                    n_add += 1;
                    push(
                        ChangeStatus::Added,
                        rel,
                        None,
                        Some(e.sha.clone()),
                        None,
                        Some(e.size),
                        &mut changes,
                    );
                }
                Some(p) if p.sha != e.sha => {
                    n_mod += 1;
                    push(
                        ChangeStatus::Modified,
                        rel,
                        Some(p.sha.clone()),
                        Some(e.sha.clone()),
                        Some(p.size),
                        Some(e.size),
                        &mut changes,
                    );
                }
                _ => {}
            }
        }
        for (rel, p) in &prev_files {
            if !files.contains_key(rel) {
                n_del += 1;
                push(
                    ChangeStatus::Deleted,
                    rel,
                    Some(p.sha.clone()),
                    None,
                    Some(p.size),
                    None,
                    &mut changes,
                );
            }
        }
    }

    let seq = state.seq + 1;
    let id = format!("s{:016}_{:04}", millis_now(), seq);
    for (i, c) in changes.iter_mut().enumerate() {
        c.id = format!("{id}#{i}");
        c.scan = id.clone();
    }
    store.append_journal(&changes)?;

    let manifest = Manifest {
        id: id.clone(),
        ts: now,
        trigger: trigger.to_string(),
        base: state.last_scan.clone(),
        files,
    };
    store.save_manifest(&manifest)?;
    store.save_state(&crate::store::State {
        last_scan: Some(id.clone()),
        last_report: state.last_report,
        seq,
    })?;

    Ok(ScanOutcome {
        id,
        files: n_files,
        baseline: is_baseline,
        added: n_add,
        modified: n_mod,
        deleted: n_del,
    })
}

/// 只列当前存在的文件（restore 找“多余文件”用）。
pub(crate) fn collect_current(cx: &WalkCx<'_>) -> Result<BTreeMap<String, PathBuf>> {
    let mut out = BTreeMap::new();
    for (path, rel, _) in walk(cx)? {
        out.insert(rel, path);
    }
    Ok(out)
}

pub(crate) struct WalkCx<'a> {
    pub root: &'a Path,
    pub opts: &'a ScanOpts,
    /// 额外整棵剪掉的绝对路径（如存储目录自身，防止自快照递归）
    pub extra_exclude: &'a [PathBuf],
}

fn walk(cx: &WalkCx<'_>) -> Result<Vec<(PathBuf, String, fs::Metadata)>> {
    let mut out = Vec::new();
    let root_c = cx.root.to_path_buf();
    let excl = cx.opts.exclude_dirs.clone();
    let extra = cx.extra_exclude.to_vec();
    let mut builder = WalkBuilder::new(cx.root);
    builder
        .hidden(false) // 隐藏文件也备份（.env 等）
        .ignore(false)
        .git_ignore(false) // 不吃 .gitignore——备份宁可多
        .git_global(false)
        .git_exclude(false)
        .parents(false)
        .require_git(false);
    builder.filter_entry(move |e| {
        let p = e.path();
        if extra.iter().any(|x| p.starts_with(x)) {
            return false;
        }
        match p.strip_prefix(&root_c) {
            Ok(rel) => !rel
                .components()
                .any(|c| excl.iter().any(|d| c.as_os_str() == d.as_str())),
            Err(_) => true, // 根目录本身
        }
    });
    for entry in builder.build() {
        let e = entry.with_context(|| "遍历工作区失败")?;
        if !e.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let path = e.path().to_path_buf();
        let rel = rel_key(cx.root, &path)?;
        let meta = e.metadata().with_context(|| format!("stat 失败: {rel}"))?;
        out.push((path, rel, meta));
    }
    Ok(out)
}

fn rel_key(root: &Path, path: &Path) -> Result<String> {
    let rel = path.strip_prefix(root).context("路径不在工作区内")?;
    Ok(rel.to_string_lossy().replace('\\', "/"))
}

fn mtime_nanos(meta: &fs::Metadata) -> Result<u64> {
    let d = meta
        .modified()?
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    Ok(d.as_nanos() as u64)
}

fn millis_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tmp_dir;

    #[test]
    fn baseline_then_changes() {
        let base = tmp_dir("scan");
        let ws = base.join("ws");
        fs::create_dir_all(ws.join("sub")).unwrap();
        fs::write(ws.join("a.txt"), "aaa\n").unwrap();
        fs::write(ws.join("sub/b.txt"), "bbb\n").unwrap();
        let store_dir = base.join("store");
        let store = Store::open(&ws, Some(&store_dir)).unwrap();
        let opts = ScanOpts::default();

        let o1 = run_scan(&ws, &store, "manual", &opts).unwrap();
        assert!(o1.baseline);
        assert_eq!(o1.files, 2);

        // 无变更（mtime/size 缓存命中，不产生 journal 行）
        std::thread::sleep(std::time::Duration::from_millis(10));
        let o2 = run_scan(&ws, &store, "manual", &opts).unwrap();
        assert_eq!(o2.added + o2.modified + o2.deleted, 0);
        assert!(store.read_journal().unwrap().is_empty());

        // 改、删、增
        std::thread::sleep(std::time::Duration::from_millis(10));
        fs::write(ws.join("a.txt"), "AAAA\n").unwrap();
        fs::remove_file(ws.join("sub/b.txt")).unwrap();
        fs::write(ws.join("c.txt"), "ccc\n").unwrap();
        let o3 = run_scan(&ws, &store, "tool_end", &opts).unwrap();
        assert_eq!((o3.modified, o3.deleted, o3.added), (1, 1, 1));

        let journal = store.read_journal().unwrap();
        assert!(
            journal
                .iter()
                .any(|c| c.path == "a.txt" && c.status == ChangeStatus::Modified)
        );
        assert!(
            journal
                .iter()
                .any(|c| c.path == "sub/b.txt" && c.status == ChangeStatus::Deleted)
        );
        assert!(
            journal
                .iter()
                .any(|c| c.path == "c.txt" && c.status == ChangeStatus::Added)
        );
        // 修改条目的 before 指向基线内容
        let m = journal.iter().find(|c| c.path == "a.txt").unwrap();
        assert_eq!(
            store.read_blob(m.before.as_deref().unwrap()).unwrap(),
            b"aaa\n"
        );
        // 黑名单目录被剪掉
        fs::create_dir_all(ws.join("node_modules/x")).unwrap();
        fs::write(ws.join("node_modules/x/junk.js"), "junk\n").unwrap();
        let o4 = run_scan(&ws, &store, "manual", &opts).unwrap();
        assert_eq!(o4.added, 0);
    }
}
