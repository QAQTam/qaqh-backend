//! 端到端验证紧急备份闭环：基线 → 失控脚本（清空/大改/删除/新增）→
//! 报告命中危险启发式 → undo 单文件找回 → restore 整库回基线。

#![allow(clippy::unwrap_used)] // 测试代码豁免（仓库惯例，见 clippy.toml）

use qaqh_spy::{ScanId, Session, Trigger};

const APP_OLD: &str = "\
def main():
    conf = load()
    for i in range(10):
        print(conf, i)
    return 0

# a
# b
# c
# d
# e
";

const UTILS_OLD: &str = "\
def add(a, b):
    return a + b

def sub(a, b):
    return a - b

def mul(a, b):
    return a * b

def div(a, b):
    return a / b
";

fn unique_dir(tag: &str) -> std::path::PathBuf {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let d = std::env::temp_dir().join(format!(
        "qaqh-spy-e2e-{}-{}-{}",
        tag,
        std::process::id(),
        N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn emergency_backup_end_to_end() {
    let base = unique_dir("main");
    let ws = base.join("ws");
    let store = base.join("store");
    std::fs::create_dir_all(&ws).unwrap();

    std::fs::write(ws.join("app.py"), APP_OLD).unwrap();
    std::fs::write(ws.join("utils.py"), UTILS_OLD).unwrap();
    std::fs::write(ws.join("config.toml"), "k = 1\n").unwrap();

    let s = Session::open(&ws, Some(store)).unwrap();

    // 基线
    let baseline = s.scan(Trigger::Manual).unwrap();
    assert!(baseline.baseline);
    assert_eq!(baseline.files, 3);

    // 无变更：稳态扫描不产生任何 journal 记录
    std::thread::sleep(std::time::Duration::from_millis(10));
    let again = s.scan(Trigger::Manual).unwrap();
    assert_eq!(again.added + again.modified + again.deleted, 0);

    // 模拟失控的批量修改脚本：清空 app.py、大改 utils.py、删 config.toml、加 new.py
    std::thread::sleep(std::time::Duration::from_millis(10));
    std::fs::write(ws.join("app.py"), "").unwrap();
    std::fs::write(ws.join("utils.py"), "x = 1\n").unwrap();
    std::fs::remove_file(ws.join("config.toml")).unwrap();
    std::fs::write(ws.join("new.py"), "print('hi')\n").unwrap();

    // 报告：清空被启发式命中，diff 带 @@ 头
    let mark = ScanId(baseline.id.clone());
    let report = s.report_since(&mark, 8192).unwrap();
    assert!(report.contains("⚠"), "应命中危险启发式:\n{report}");
    assert!(report.contains("app.py"), "应提到被清空的文件:\n{report}");
    assert!(report.contains("已变为空文件"), "应指出清空:\n{report}");
    assert!(report.contains("@@"), "应输出 hunk 头:\n{report}");
    assert!(report.contains("-def main()"), "应包含删除行:\n{report}");
    assert!(report.contains("config.toml"), "应提到被删文件:\n{report}");

    // 审计流水：四条净变更
    let journal = s.changes_since(&mark).unwrap();
    assert_eq!(journal.len(), 4);

    // undo：单文件找回被清空的 app.py
    let app_change = journal.iter().find(|c| c.path == "app.py").unwrap().clone();
    let out = s.undo(&app_change.id, false).unwrap();
    assert_eq!(out.action, "reverted");
    assert_eq!(std::fs::read_to_string(ws.join("app.py")).unwrap(), APP_OLD);

    // 冲突保护：把 app.py 改掉再 undo 同一条记录应被拒绝
    std::fs::write(ws.join("app.py"), "tampered\n").unwrap();
    assert!(s.undo(&app_change.id, false).is_err());
    assert!(s.undo(&app_change.id, true).is_ok()); // force 可过
    assert_eq!(std::fs::read_to_string(ws.join("app.py")).unwrap(), APP_OLD);

    // restore：整库回基线（找回 config.toml、prune 删掉 new.py、还原 utils.py）
    let r = s.restore(&ScanId(baseline.id.clone()), true).unwrap();
    assert!(r.written >= 1, "应至少重写 utils.py: {r:?}");
    assert!(r.extras.iter().any(|e| e == "new.py"));
    assert_eq!(r.pruned, 1);
    assert_eq!(
        std::fs::read_to_string(ws.join("config.toml")).unwrap(),
        "k = 1\n"
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("utils.py")).unwrap(),
        UTILS_OLD
    );
    assert!(!ws.join("new.py").exists());

    // cat：能看任意历史版本
    let utils_entry = journal
        .iter()
        .find(|c| c.path == "utils.py")
        .unwrap()
        .clone();
    let old = s.cat(utils_change_before(&utils_entry)).unwrap();
    assert_eq!(String::from_utf8_lossy(&old), UTILS_OLD);

    let _ = std::fs::remove_dir_all(&base);
}

fn utils_change_before(c: &qaqh_spy::Change) -> &str {
    c.before.as_deref().expect("修改记录必有 before")
}
