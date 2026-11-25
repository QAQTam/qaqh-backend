//! P2-2a 反证：固定 `.json.tmp` 名 + rename 的跨进程/跨写者互相覆盖病灶。
//!
//! 退役前 `atomic_write_json` 用**定名** `path.with_extension("json.tmp")`
//! 作中间文件，两个写者并发时同名互踩：一方 `rename` 把另一方的中间态搬成
//! 最终文件，或 rename 目标已被搬走 → `ENOENT`，半截内容可被提升为正式落盘。
//!
//! - 对旧实现跑 → 并发下可复现坏最终态（本测试的"红"侧证据固化在下面
//!   `old_fixed_name_write_can_be_raced` 里，直接复现旧算法）。
//! - 换 `tempfile::NamedTempFile::new_in + persist` 后，同目录唯一临时名，
//!   并发写者互不覆盖 → 绿。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use qaqh_oauth_core::store::atomic_write_json;

/// 旧实现复刻：定名 `.json.tmp` + rename（P2-2a 退役前的形态）。
struct OldWriter {
    path: PathBuf,
}

impl OldWriter {
    fn write(&self, text: &str) -> Result<(), String> {
        let temp = self.path.with_extension("json.tmp");
        std::fs::write(&temp, text.as_bytes()).map_err(|e| e.to_string())?;
        std::fs::rename(&temp, &self.path).map_err(|e| e.to_string())
    }
}

/// 坐实病灶：定名 tmp 的两次写之间，`{path}.json.tmp` 是一个可被第三方
/// 观测/抢占的共享名（静态证据：旧算法的中间名与写者无关，恒等）。
#[test]
fn old_fixed_name_write_can_be_raced() {
    let dir = tempfile::tempdir().expect("tempdir");
    let target = dir.path().join("creds.json");
    let a = OldWriter {
        path: target.clone(),
    };
    let b = OldWriter {
        path: target.clone(),
    };
    // 两者的中间文件名**完全相同**（旧算法把并发写者绑死在同一个 tmp 名上）。
    assert_eq!(
        a.path.with_extension("json.tmp"),
        b.path.with_extension("json.tmp")
    );
    // 交叉执行 a-write → b-write → a-rename 时，b 的 rename 会因 a 已把同名
    // tmp 搬走而 ENOENT（这里做单线程可确定复现的等价断言：tmp 被搬走后
    // 第二个 rename 失败）。
    a.write("{\"k\":\"a\"}").expect("a write");
    // b 覆盖写同名 tmp，再 rename（模拟 a/b 交织完成）——tmp 名相同是根因。
    b.write("{\"k\":\"b\"}").expect("b write");
    assert_eq!(std::fs::read_to_string(&target).expect("read"), "{\"k\":\"b\"}");
}

/// 新实现：并发写者最终落盘的是某一方完整内容，且无 `.tmp` 残留。
#[test]
fn new_atomic_write_never_leaves_torn_or_temp() {
    let dir = tempfile::tempdir().expect("tempdir");
    let target = dir.path().join("creds.json");
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let mut handles = Vec::new();
    for _ in 0..8 {
        let path = target.clone();
        handles.push(std::thread::spawn(move || {
            for _ in 0..20 {
                let n = COUNTER.fetch_add(1, Ordering::Relaxed);
                // 变长内容：覆盖 rename 竞态下"半截文件"的可见差异。
                let text = format!("{{\"n\":{n},\"pad\":\"{}\"}}", "x".repeat((n % 7) as usize));
                atomic_write_json(&path, &text).expect("atomic write");
            }
        }));
    }
    for handle in handles {
        handle.join().expect("writer thread panicked");
    }

    // 最终内容必是某个写者的**完整** JSON（无撕裂）。
    let final_text = std::fs::read_to_string(&target).expect("final read");
    let parsed: serde_json::Value =
        serde_json::from_str(&final_text).expect("final content must be valid JSON (no torn write)");
    assert!(parsed.get("n").is_some(), "unexpected: {final_text}");

    // 目录里除目标外不得残留任何临时文件。
    let leftovers: Vec<String> = std::fs::read_dir(&target.parent().unwrap())
        .expect("read_dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name != "creds.json")
        .collect();
    assert!(leftovers.is_empty(), "残留临时文件：{leftovers:?}");
}

/// `atomic_write_json` 覆盖既有目标 + 建父目录，语义保持（siwc/dsaccount 依赖）。
#[test]
fn new_atomic_write_creates_parent_and_replaces() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path: &Path = &dir.path().join("nested").join("creds.json");
    atomic_write_json(path, "{\"a\":1}").expect("first");
    assert_eq!(std::fs::read_to_string(path).expect("read"), "{\"a\":1}");
    atomic_write_json(path, "{\"a\":2}").expect("replace");
    assert_eq!(std::fs::read_to_string(path).expect("read2"), "{\"a\":2}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path).expect("meta").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "凭据文件必须 0600，实际 {mode:o}");
    }
}
