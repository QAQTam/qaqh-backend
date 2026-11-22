//! File runtime state — per-session file baselines + read cache.
//!
//! P0-2：进程级 `LEDGER` / `file_cache::CACHE` 单例随「工具执行面没有可注入
//! 的上下文」这一旧前提一并退役。账本与读缓存现在是 [`FileRuntime`] 的实例
//! 状态：`ToolManager` 构造时创建一份、全部文件工具共享同一 `Arc`，会话私有
//! manager（`WorkspaceToolPort`）天然各持一份——跨会话基线互踩（A 的 read
//! 基线被 B 的 write 防漂移校验拿去判 STALE_FILE）与
//! `init_tools → file_state::clear()` 一键洗白全进程基线的污染机理物理消灭。
//!
//! read 缓存策略（原 file_cache 模块，单实例语义不变）：
//! 1. file_read 时算 hash → 与缓存一致且非连续读 → 返回 "unchanged"；
//! 2. 连续读同一文件永远旁路缓存（模型需要重新审视）；
//! 3. write/edit/delete 失效对应路径。容量 64，FIFO 驱逐。

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug)]
struct FileEntry {
    op: &'static str,
    line_count: usize,
    /// 最近一次工具看到的内容指纹（**LF canonical 视图**，与 read 返回的 hash 同视图）。
    /// edit/write/delete 未显式携带 expected_hash 时，用它做自动防漂移校验。
    hash: Option<String>,
    order: u64, // monotonically increasing for recency sort
}

/// 单次编辑的行号偏移（账本行号修正用）。
///
/// 语义：编辑前 1-based 起始行 `before_line` 及之后的所有行，行号增加
/// `delta`（可负）。read 建立新基线（清空偏移链），write/delete/move 的
/// 全覆盖语义同样使行号失效（清空）。
#[derive(Clone, Debug)]
struct LineShift {
    before_line: usize,
    delta: i64,
}

struct CacheEntry {
    path: String,
    hash: String,
    line_count: usize,
}

const MAX_CACHE: usize = 64;
const MAX_SUMMARY_FILES: usize = 20;

/// 一份会话私有的文件运行时状态（账本 + 读缓存），**一把锁**。
///
/// 原 `STATE`/`COUNTER`/`SHIFTS`/`CACHE`/`LAST_READ_PATH` 五个 static、多把锁
/// 的锁序约定问题随实例化一并消失：所有字段在同一个 `&mut self` 下变更，
/// 不再需要跨锁顺序推理。
#[derive(Default)]
struct Inner {
    entries: HashMap<String, FileEntry>,
    shifts: HashMap<String, Vec<LineShift>>,
    cache: Vec<CacheEntry>,
    last_read_path: Option<String>,
    counter: u64,
}

/// 会话私有的文件运行时状态句柄（可克隆，共享同一把锁）。
#[derive(Clone, Default)]
pub struct FileRuntime {
    inner: Arc<Mutex<Inner>>,
}

impl FileRuntime {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn next_order(inner: &mut Inner) -> u64 {
        inner.counter = inner.counter.wrapping_add(1);
        inner.counter
    }

    fn insert(inner: &mut Inner, path: &str, op: &'static str, line_count: usize, hash: Option<String>) {
        let key = ledger_key(path);
        let order = Self::next_order(inner);
        inner.entries.insert(
            key.clone(),
            FileEntry {
                op,
                line_count,
                hash: hash.clone(),
                order,
            },
        );
    }

    fn invalidate_cache(inner: &mut Inner, path: &str) {
        let before = inner.cache.len();
        inner.cache.retain(|e| e.path != path);
        if inner.cache.len() < before {
            log::info!("[CACHE] invalidated: {}", path);
        }
    }

    /// Record a file read (full file or range — both establish the ledger).
    /// 同时建立行号基线：清空该文件的编辑偏移链，并把内容存入读缓存。
    pub fn record_read(&self, path: &str, content: &str, line_count: usize) {
        let hash = hash_content(content);
        let hash_short = short_hash(&hash).to_string();
        let mut inner = self.lock();
        inner.cache.retain(|e| e.path != path);
        if inner.cache.len() >= MAX_CACHE {
            inner.cache.remove(0);
        }
        inner.cache.push(CacheEntry {
            path: path.to_string(),
            hash,
            line_count,
        });
        log::info!(
            "[CACHE] file_read stored: {} (hash={}, {}L)",
            path,
            hash_short,
            line_count
        );
        Self::insert(&mut inner, path, "read", line_count, Some(ledger_hash(content)));
        inner.shifts.remove(&ledger_key(path));
    }

    /// Read 缓存命中检查。返回 `Some(json)` 表示内容自上次 read 后未变。
    /// 连续读同一文件永远旁路缓存（模型需要重新审视）。
    pub fn read_cache_check(&self, path: &str, content: &str) -> Option<String> {
        let hash = hash_content(content);
        let mut inner = self.lock();
        let is_consecutive = inner.last_read_path.as_ref().is_some_and(|p| p == path);
        inner.last_read_path = Some(path.to_string());
        if is_consecutive {
            log::info!("[CACHE] file_read consecutive: {} — passing through", path);
            return None;
        }
        for e in inner.cache.iter() {
            if e.path == path {
                if e.hash == hash {
                    log::info!(
                        "[CACHE] file_read hit: {} (hash={})",
                        path,
                        short_hash(&hash)
                    );
                    return Some(
                        serde_json::json!({
                            "timeis": qaqh_types::platform::now_utc8(),
                            "status": "ok",
                            "path": path,
                            "hash": crate::file_shared::content_hash(content),
                            "cache_hash": short_hash(&hash),
                            "total_lines": e.line_count,
                            "unchanged": true,
                            "content": format!("{} unchanged (hash={})", path, short_hash(&hash)),
                        })
                        .to_string(),
                    );
                }
                break;
            }
        }
        None
    }

    /// Record a file write (create/overwrite/append). 全覆盖语义：行号全失效，
    /// 清空偏移链（模型应重新 read 建立基线）。
    pub fn record_write(&self, path: &str, content: &str) {
        let mut inner = self.lock();
        Self::invalidate_cache(&mut inner, path);
        let op = if inner.entries.contains_key(&ledger_key(path)) {
            "edited"
        } else {
            "created"
        };
        Self::insert(
            &mut inner,
            path,
            op,
            content.lines().count(),
            Some(ledger_hash(content)),
        );
        inner.shifts.remove(&ledger_key(path));
    }

    /// Record a file edit with per-hunk line shifts (账本行号修正的数据源)。
    /// `shifts` = 编辑前 1-based 起始行 → 行号增量（按序应用，均相对同一快照）。
    pub fn record_edit_with_shifts(&self, path: &str, content: &str, shifts: &[(usize, i64)]) {
        let mut inner = self.lock();
        Self::invalidate_cache(&mut inner, path);
        let key = ledger_key(path);
        Self::insert(
            &mut inner,
            path,
            "edited",
            content.lines().count(),
            Some(ledger_hash(content)),
        );
        let list = inner.shifts.entry(key).or_default();
        for (before_line, delta) in shifts {
            if *delta != 0 {
                list.push(LineShift {
                    before_line: *before_line,
                    delta: *delta,
                });
            }
        }
    }

    /// 账本行号修正：把"基于最近一次 read 基线"的行号映射到当前行号。
    ///
    /// 返回 `(修正后行号, 总偏移)`；无偏移链（该文件从未被 read 过或链已清）
    /// 返回 `Some((line, 0))` 原样，无基线返回 `None`。
    pub fn correct_line(&self, path: &str, line: usize) -> Option<(usize, i64)> {
        let key = ledger_key(path);
        let inner = self.lock();
        let list = inner.shifts.get(&key)?;
        if list.is_empty() {
            return Some((line, 0));
        }
        let mut delta: i64 = 0;
        for s in list {
            if line >= s.before_line {
                delta += s.delta;
            }
        }
        Some(((line as i64 + delta).max(1) as usize, delta))
    }

    /// grep 输出的是实时行号：清空偏移链，防止后续 read 用 grep 行号时被误修正。
    pub fn record_grep(&self, path: &str) {
        self.lock().shifts.remove(&ledger_key(path));
    }

    /// Record a file deletion (ledger cleared — the file no longer exists).
    pub fn record_delete(&self, path: &str) {
        let mut inner = self.lock();
        Self::invalidate_cache(&mut inner, path);
        Self::insert(&mut inner, path, "deleted", 0, None);
    }

    /// 工具侧账本：该路径最近一次被工具看到时的 LF 视图 hash。
    /// `None` = 本会话中工具从未见过该文件（尚无校验基线）。
    pub fn last_hash(&self, path: &str) -> Option<String> {
        let inner = self.lock();
        inner
            .entries
            .get(&ledger_key(path))
            .and_then(|e| e.hash.clone())
    }

    /// Generate file state summary. Capped at 20 most recently touched files.
    pub fn summary(&self) -> String {
        let inner = self.lock();
        let s = &inner.entries;
        if s.is_empty() {
            return String::new();
        }
        let mut entries: Vec<(&String, &FileEntry)> = s.iter().collect();
        entries.sort_by_key(|(_, e)| -(e.order as i64)); // most recent first
        let total = entries.len();
        entries.truncate(MAX_SUMMARY_FILES);

        let mut out = String::from("<file_state>\n");
        for (path, e) in &entries {
            let lines = if e.line_count > 0 {
                format!("{}L", e.line_count)
            } else {
                String::new()
            };
            out.push_str(&format!(
                "  {:<50} {:>6}  ({})\n",
                path,
                if lines.is_empty() { "—" } else { &lines },
                e.op,
            ));
        }
        if total > MAX_SUMMARY_FILES {
            out.push_str(&format!(
                "  ... ({} more files)\n",
                total - MAX_SUMMARY_FILES
            ));
        }
        out.push_str("</file_state>");
        out
    }
}

/// Windows 上 `canonicalize` 返回 `\\?\C:\…` verbatim 前缀路径；剥掉以与
/// read/write 既有键形态一致（见 `copy_range.rs` 的 BUG-2026-09-13-16 注）。
fn strip_verbatim(path: &Path) -> String {
    let s = path.to_string_lossy();
    s.strip_prefix(r"\\?\").unwrap_or(&s).to_string()
}

/// 账本键的唯一派生（T-8-3② / 安全审查 P1-3）。
///
/// 绝对路径：词法归一（去 `./`/`..`/冗余分隔符，不引入 Windows `\\?\`
/// verbatim 前缀——BUG-2026-09-13-16 的根因）+ 文件存在时 best-effort
/// `canonicalize`（解析符号链接；不存在时解析父目录后拼回文件名，再不行
/// 退回词法归一形态）。
///
/// 相对路径**原样保留**：生产调用方（read/write/edit/copy_range/apply_patch）
/// 都已先经显式 ctx 解析出绝对路径；账本层再解析会把无锚点的相对键悄悄改成
/// workspace/cwd 绝对键，破坏既有相对键契约。
///
/// 宿主在工具之外回填账本时（批边界扫描发现脚本改的文件）必须走同一口径，
/// 否则与 record_read / record_write 的键错开——注意对**相对路径不做
/// canonicalize**，因此调用方应先把路径解析成绝对路径再传入。
pub fn ledger_key(raw: &str) -> String {
    let raw_path = Path::new(raw);
    if !raw_path.is_absolute() {
        return raw.to_string();
    }
    let normalized =
        qaqh_permission::normalize_lexically(raw_path).to_string_lossy().to_string();
    let path = Path::new(&normalized);
    if let Ok(canon) = std::fs::canonicalize(path) {
        return strip_verbatim(&canon);
    }
    if let (Some(parent), Some(name)) = (path.parent(), path.file_name())
        && let Ok(canon_parent) = std::fs::canonicalize(parent)
    {
        return strip_verbatim(&canon_parent.join(name));
    }
    normalized
}

/// 任意内容 → LF canonical 视图 hash（与 read 返回的 hash 同视图）。
/// 调用方传入的 content 可能是 CRLF 或混合换行（如 write 落盘内容），
/// 账本必须统一归一化后再哈希，否则 CRLF 文件下账本永远失配。
fn ledger_hash(content: &str) -> String {
    let (lf, _) = crate::file_shared::normalize_newlines(content);
    crate::file_shared::content_hash(&lf)
}

/// 16 位小写 hex 的前 8 位（hex 为 ASCII，切片恒为 char boundary）。
fn short_hash(h: &str) -> &str {
    h.get(..8).unwrap_or(h)
}

fn hash_content(s: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    format!("{:016x}", h.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 实例化后的账本天然会话私有：两份 FileRuntime 互不可见——这正是
    /// P0-2 消灭跨会话污染的不变量（8 线程并发探针的单元级前身）。
    #[test]
    fn runtimes_are_isolated_by_construction() {
        let a = FileRuntime::default();
        let b = FileRuntime::default();
        a.record_read("shared.txt", "alpha\n", 1);
        assert_eq!(a.last_hash("shared.txt"), Some(ledger_hash("alpha\n")));
        assert_eq!(
            b.last_hash("shared.txt"), None,
            "另一份 FileRuntime 不得看到本实例的基线"
        );
        // B 建立自己的基线后互不覆盖
        b.record_write("shared.txt", "beta\n");
        assert_eq!(a.last_hash("shared.txt"), Some(ledger_hash("alpha\n")));
        assert_eq!(b.last_hash("shared.txt"), Some(ledger_hash("beta\n")));
    }

    #[test]
    fn read_cache_hits_and_invalidates_per_instance() {
        let rt = FileRuntime::default();
        rt.record_read("c.txt", "v1\n", 1);
        // 同内容再次读（非连续——中间插一个别的路径）→ 命中
        rt.record_read("other.txt", "o\n", 1);
        let hit = rt.read_cache_check("c.txt", "v1\n").expect("cache hit");
        assert!(hit.contains("unchanged"));
        // 内容变了 → 未命中
        assert!(rt.read_cache_check("c.txt", "v2\n").is_none());
        // write 失效 → 未命中
        rt.record_write("c.txt", "v3\n");
        assert!(rt.read_cache_check("c.txt", "v3\n").is_none());
        // 连续读旁路
        assert!(rt.read_cache_check("c.txt", "v3\n").is_none());
    }

    #[test]
    fn file_state_key_matches_resolved_key() {
        // T-8-3②：同一文件的不同书写形态必须派生**同一个**账本键——否则
        // read 建立的基线对 write/edit 不可见（STALE_FILE 误判 / hash 漂移）。
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("ledger.txt");
        std::fs::write(&file, "hello\n").expect("write file");

        // 绝对路径 + 冗余分量（`./` 与 `sub/..` 回折）。旧实现把绝对路径
        // 原样返回，该字符串与归一化后的解析键分叉。
        let dotted = format!("{}/./sub/../ledger.txt", dir.path().display());
        let rt = FileRuntime::default();
        rt.record_read(&dotted, "hello\n", 1);

        let resolved = qaqh_permission::normalize_lexically(Path::new(&file))
            .to_string_lossy()
            .to_string();
        assert!(
            Path::new(&resolved).is_absolute(),
            "fixture must resolve to an absolute key, got {resolved}"
        );
        assert_eq!(
            ledger_key(&dotted),
            ledger_key(&resolved),
            "state key must not depend on the path spelling"
        );
        assert_eq!(
            rt.last_hash(&dotted),
            rt.last_hash(&resolved),
            "file_state key must match the resolved key for the same file"
        );
        assert!(
            rt.last_hash(&resolved).is_some(),
            "the read baseline must be visible through the resolved key"
        );
        assert!(
            !ledger_key(&dotted).contains("/./"),
            "key must be normalized: {}",
            ledger_key(&dotted)
        );
    }
}
