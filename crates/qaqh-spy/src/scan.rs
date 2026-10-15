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

/// 增量链最大深度：每 N 份 manifest 强制拍一份全量锚点。
const ANCHOR_EVERY: u32 = 16;

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
                // 宿主自己的运行时目录（delete 的 trash、todo/journal 缓存等）就落在工作区里。
                // 快照它 = 把备份当"工作区净变更"上报，曾在一次目录删除里报出 9337 个文件。
                ".qaqh",
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
    /// 本扫描的 manifest id。空扫描（无变更）复用上一份 manifest id，
    /// 不落新文件——manifest 是全量文件清单，是存储增长的主项。
    pub id: String,
    pub files: usize,
    /// 首次扫描（只有 manifest，不刷 journal）
    pub baseline: bool,
    pub added: usize,
    pub modified: usize,
    pub deleted: usize,
    /// **本 owner** 上次看到这个 store 时的扫描 id；`None` = 该 owner 首次出现
    /// （或已被容量闸挤掉）。宿主用它算"你离开期间攒了多少变更"（PR-D）——
    /// 注意它与 `id` 不是一回事：`id` 是全局最新边界，别人的批会不停推进它。
    pub prior_mark: Option<String>,
}

pub(crate) fn run_scan(
    root: &Path,
    store: &Store,
    trigger: &str,
    owner: &str,
    opts: &ScanOpts,
) -> Result<ScanOutcome> {
    let _guard = store.lock_timeout(opts.lock_wait)?;
    let state = store.load_state()?;
    let is_baseline = state.last_scan.is_none();
    // 本 owner 上次看到这个 store 的位置（可能没有：首次出现，或被容量闸挤掉）。
    let prior_mark = state.owner_marks.get(owner).cloned();
    // 增量链：prev 的链深用于锚点判定；文件集必须物化为全量（prev 可能是 delta）
    let prev_depth = match &state.last_scan {
        Some(id) => store.load_manifest(id)?.depth,
        None => 0,
    };
    let prev_files = match &state.last_scan {
        Some(id) => store.materialize_manifest(id)?,
        None => BTreeMap::new(),
    };

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
                owner: owner.to_string(),
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

    // 空扫描热补丁：无变更时不落新 manifest、不动 state。工具批边界每批
    // 固定两次扫描，稳态下绝大多数是空扫描，此前每次都全量落一份 manifest
    // （数百 KB/份），是存储 O(批次数 × 文件数) 增长的主因。复用上一份
    // manifest id 后 id 仍单调递增（有变更的扫描才会生成新 id），
    // `changes_since` 的 `c.scan > mark` 过滤与 restore 的按 id 定位均不受影响。
    if changes.is_empty() && !is_baseline {
        let prev_id = state
            .last_scan
            .clone()
            .context("非基线扫描但 state.last_scan 缺失")?;
        // 空扫描不落新 manifest，但**首次见到这个 owner** 时仍要登记指针（一次小 JSON 写），
        // 否则它的 prior_mark 永远是 None，恢复批就说不出漂移摘要。已有指针则零额外写。
        if !owner.is_empty() && prior_mark.is_none() {
            let mut marks = state.owner_marks.clone();
            note_owner(&mut marks, owner, &prev_id);
            store.save_state(&crate::store::State {
                last_scan: Some(prev_id.clone()),
                last_report: state.last_report.clone(),
                seq: state.seq,
                owner_marks: prune_owner_marks(marks),
            })?;
        }
        return Ok(ScanOutcome {
            id: prev_id,
            files: n_files,
            baseline: false,
            added: 0,
            modified: 0,
            deleted: 0,
            prior_mark,
        });
    }

    let seq = state.seq + 1;
    let id = format!("s{:016}_{:04}", millis_now(), seq);
    for (i, c) in changes.iter_mut().enumerate() {
        c.id = format!("{id}#{i}");
        c.scan = id.clone();
    }
    store.append_journal(&changes)?;

    // 锚点判定（补丁 B）：基线 / 链深到顶 / 大变更（>25% 条目变化）直接拍
    // 全量锚点，其余落增量——相对 base 仅变化条目，删除以墓碑表达。
    let changed = changes.len();
    let anchor = is_baseline || prev_depth + 1 >= ANCHOR_EVERY || changed * 4 > n_files;
    let manifest = if anchor {
        Manifest {
            id: id.clone(),
            ts: now,
            trigger: trigger.to_string(),
            base: None,
            anchor: true,
            depth: 0,
            files,
            tombstones: Vec::new(),
        }
    } else {
        let mut delta_files = BTreeMap::new();
        let mut tombstones = Vec::new();
        for c in &changes {
            if c.status == ChangeStatus::Deleted {
                tombstones.push(c.path.clone());
                continue;
            }
            let e = files.get(&c.path).context("变更条目缺失于本次扫描结果")?;
            delta_files.insert(c.path.clone(), e.clone());
        }
        Manifest {
            id: id.clone(),
            ts: now,
            trigger: trigger.to_string(),
            base: state.last_scan.clone(),
            anchor: false,
            depth: prev_depth + 1,
            files: delta_files,
            tombstones,
        }
    };
    store.save_manifest(&manifest)?;
    let mut owner_marks = state.owner_marks;
    note_owner(&mut owner_marks, owner, &id);
    store.save_state(&crate::store::State {
        last_scan: Some(id.clone()),
        last_report: state.last_report,
        seq,
        owner_marks: prune_owner_marks(owner_marks),
    })?;

    Ok(ScanOutcome {
        id,
        files: n_files,
        baseline: is_baseline,
        added: n_add,
        modified: n_mod,
        deleted: n_del,
        prior_mark,
    })
}

/// 归属指针的容量闸：只保留最近 `OWNER_MARKS_KEEP` 个 owner 的边界。超出的最老
/// owner 掉出后，它回来时 `prior_mark` 为 None——只是不再说漂移摘要，不影响正确性。
/// （没有这道闸，state.json 会随历史会话数无界增长。）
const OWNER_MARKS_KEEP: usize = 512;

fn prune_owner_marks(marks: BTreeMap<String, String>) -> BTreeMap<String, String> {
    if marks.len() <= OWNER_MARKS_KEEP {
        return marks;
    }
    // mark 字典序 == 时间序（`s{millis:016}_{seq:04}`），按它降序保留。
    let mut by_mark: Vec<(String, String)> = marks.into_iter().map(|(o, m)| (m, o)).collect();
    by_mark.sort_unstable_by(|a, b| b.0.cmp(&a.0));
    by_mark
        .into_iter()
        .take(OWNER_MARKS_KEEP)
        .map(|(m, o)| (o, m))
        .collect()
}

/// 登记「owner 刚看到这个扫描点」。空 owner（未接归属的调用方，如 `spy` 工具本体
/// 或外部 hook）不登记，免得往指针里塞一个万能键、把所有会话都判成外部。
fn note_owner(marks: &mut BTreeMap<String, String>, owner: &str, scan: &str) {
    if !owner.is_empty() {
        marks.insert(owner.to_string(), scan.to_string());
    }
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
            // 全等**或** `{name}.` 前缀命中即剪。前缀分支专为 `{name}.{ts}` 这类改名留的：
            // `delete` 把目录移成 `.qaqh/trash/target.1791993394`，原件因名为 `target` 被排除，
            // 副本却因名字带了时间戳而整棵进了快照——一次删除报出 9337 个净变更（含 hints 约
            // 1.2–3.5 MB 的一条注入）。手写 starts_with 而不引入 globset：本 crate 依赖表里没有。
            Ok(rel) => !rel.components().any(|c| {
                let name = c.as_os_str().to_string_lossy();
                excl.iter()
                    .any(|d| name == *d || name.starts_with(&format!("{d}.")))
            }),
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

        let o1 = run_scan(&ws, &store, "manual", "", &opts).unwrap();
        assert!(o1.baseline);
        assert_eq!(o1.files, 2);

        // 无变更（mtime/size 缓存命中，不产生 journal 行）
        std::thread::sleep(std::time::Duration::from_millis(10));
        let o2 = run_scan(&ws, &store, "manual", "", &opts).unwrap();
        assert_eq!(o2.added + o2.modified + o2.deleted, 0);
        assert!(store.read_journal().unwrap().is_empty());

        // 改、删、增
        std::thread::sleep(std::time::Duration::from_millis(10));
        fs::write(ws.join("a.txt"), "AAAA\n").unwrap();
        fs::remove_file(ws.join("sub/b.txt")).unwrap();
        fs::write(ws.join("c.txt"), "ccc\n").unwrap();
        let o3 = run_scan(&ws, &store, "tool_end", "", &opts).unwrap();
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
        let o4 = run_scan(&ws, &store, "manual", "", &opts).unwrap();
        assert_eq!(o4.added, 0);
    }

    #[test]
    fn empty_scan_reuses_previous_manifest() {
        let base = tmp_dir("scan-reuse");
        let ws = base.join("ws");
        fs::create_dir_all(&ws).unwrap();
        fs::write(ws.join("a.txt"), "aaa\n").unwrap();
        let store_dir = base.join("store");
        let store = Store::open(&ws, Some(&store_dir)).unwrap();
        let opts = ScanOpts::default();

        // 基线：1 份 manifest
        let o1 = run_scan(&ws, &store, "manual", "", &opts).unwrap();
        assert_eq!(store.all_manifest_ids().unwrap().len(), 1);

        // 空扫描：复用 manifest id，不新增文件
        std::thread::sleep(std::time::Duration::from_millis(10));
        let o2 = run_scan(&ws, &store, "manual", "", &opts).unwrap();
        assert_eq!(o2.id, o1.id);
        assert_eq!(store.all_manifest_ids().unwrap().len(), 1);

        // 有变更的扫描：拿新 id、落新 manifest，journal 照常记账
        std::thread::sleep(std::time::Duration::from_millis(10));
        fs::write(ws.join("a.txt"), "bbb\n").unwrap();
        let o3 = run_scan(&ws, &store, "tool_end", "", &opts).unwrap();
        assert_ne!(o3.id, o2.id);
        assert_eq!(store.all_manifest_ids().unwrap().len(), 2);
        assert!(
            store
                .read_journal()
                .unwrap()
                .iter()
                .any(|c| c.path == "a.txt" && c.status == ChangeStatus::Modified)
        );

        let _ = fs::remove_dir_all(&base);
    }

    /// B1 回归：`delete` 把目录改名成 `{name}.{ts}` 移进 `.qaqh/trash/` 之后，原件因名为
    /// `target` 被 exclude 剪掉，副本却因名字带了时间戳整棵进快照——实测一次删除报出
    /// 9337 个净变更、灌入 3275 条假 SMJ 记录、单条注入 ≈1.2–3.5 MB。
    /// 两层都必须挡住：`.qaqh` 在名单里，且 `{name}.` 前缀命中 `target.1791993394`。
    #[test]
    fn trashed_directory_copy_is_excluded() {
        let base = tmp_dir("scan-trash");
        let ws = base.join("ws");
        fs::create_dir_all(ws.join("target")).unwrap();
        fs::write(ws.join("target/dep.o"), "obj\n").unwrap();
        let store = Store::open(&ws, Some(&base.join("store-trash"))).unwrap();
        let opts = ScanOpts::default();

        // 基线：target/ 被排除 → 工作区内 0 个受快照文件
        let o1 = run_scan(&ws, &store, "tool_start", "", &opts).unwrap();
        assert_eq!(o1.files, 0, "exclude_dirs 应剪掉 target/");

        // 模拟 delete 的 rename：带时间戳的兄弟名字 + 落在 .qaqh/trash 下
        let trash = ws.join(".qaqh/trash");
        fs::create_dir_all(&trash).unwrap();
        fs::rename(&ws.join("target"), trash.join("target.1791993394")).unwrap();
        fs::write(trash.join("target.1791993394/dep.o"), "obj2\n").unwrap();

        let o2 = run_scan(&ws, &store, "tool_end", "", &opts).unwrap();
        assert_eq!(
            o2.added, 0,
            "trash 副本不得进快照（.qaqh 排除 + {{name}}. 前缀匹配）"
        );
        assert_eq!(o2.files, 0);
        assert!(
            store.read_journal().unwrap().is_empty(),
            "无净变更不得写 journal"
        );
        let _ = fs::remove_dir_all(&base);
    }

    /// PR-C：扫描落的每一行都带归属；`split_by_owner` 只在"明确属于另一个会话"时
    /// 才判外部；缺 owner 的存量行读回来必须是空串（`#[serde(default)]` 的兼容性
    /// ——老 journal.jsonl 不能被新二进制读崩）。
    #[test]
    fn owner_is_recorded_and_legacy_rows_default_to_unowned() {
        let base = tmp_dir("scan-owner");
        let ws = base.join("ws");
        fs::create_dir_all(&ws).unwrap();
        let store = Store::open(&ws, Some(&base.join("store-owner"))).unwrap();
        let opts = ScanOpts::default();

        run_scan(&ws, &store, "tool_start", "session-A", &opts).unwrap(); // 基线，不落 journal
        fs::write(ws.join("a.txt"), "a\n").unwrap();
        run_scan(&ws, &store, "tool_end", "session-A", &opts).unwrap();

        let journal = store.read_journal().unwrap();
        assert_eq!(journal.len(), 1, "基线不刷屏，只应有 A 自己那一行");
        assert_eq!(
            journal[0].owner, "session-A",
            "扫描必须记下是谁的边界观测到它"
        );

        // 另一个会话的扫描 → 对 A 而言是外部变更
        fs::write(ws.join("b.txt"), "b\n").unwrap();
        run_scan(&ws, &store, "tool_end", "session-B", &opts).unwrap();
        let journal = store.read_journal().unwrap();
        let (mine, external) = crate::split_by_owner(&journal, "session-A");
        assert_eq!(mine.len(), 1, "A 只该认领自己那一行");
        assert_eq!(external.len(), 1);
        assert_eq!(external[0].path, "b.txt");

        // 宿主自己没有 session id（owner="") 时一律全认领：宁可多报，不静默吞。
        let (all, none) = crate::split_by_owner(&journal, "");
        assert_eq!((all.len(), none.len()), (journal.len(), 0));

        // 向后兼容：没有 owner 字段的旧行必须仍能解析，且判为"无主"而非"外部"。
        let legacy = r#"{"id":"s1#0","scan":"s2","ts":"t","path":"old.txt","status":"modified","before":"0000000000000000000000000000000000000000000000000000000000000000","after":"0000000000000000000000000000000000000000000000000000000000000000","size_before":1,"size_after":1,"trigger":"manual"}"#;
        let parsed: Change = serde_json::from_str(legacy).unwrap();
        assert!(parsed.owner.is_empty(), "缺 owner 的存量行应回落空串");
        let (mine, external) = crate::split_by_owner(std::slice::from_ref(&parsed), "session-A");
        assert_eq!(
            (mine.len(), external.len()),
            (1, 0),
            "无主行不得被当成别人的变更"
        );
        let _ = fs::remove_dir_all(&base);
    }
}
