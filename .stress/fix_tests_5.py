import io

REB = r"D:\project\QAQ-Harness\crates\qaqh-runtime\src\ringing\timeline_rebuild.rs"
with io.open(REB, "r", encoding="utf-8") as f:
    src = f.read()

# rebuild 路径重建的是"已 seal turn 的历史"——seal 裁剪后 journal 必然为空。
# 测试断言从 journal 完整性改为快照物化完整性。
OLD = """        assert_eq!(snapshot.turns.len(), 1);
        assert!(snapshot.watermark > 0);
        assert_eq!(journal.len() as u64, snapshot.watermark);
        assert_eq!(journal.first().expect("opened").timeline_seq, 1);
        assert_eq!(
            journal.last().expect("sealed").timeline_seq,
            snapshot.watermark
        );"""
NEW = """        assert_eq!(snapshot.turns.len(), 1);
        assert!(snapshot.watermark > 0);
        // seal 即时裁剪语义：重建的历史 turn 已 seal，回放尾为空；
        // watermark 由快照独立持有，条目数断言不再适用。
        assert!(
            journal.is_empty(),
            "rebuilt sealed turns leave an empty replay tail"
        );
        assert!(snapshot.watermark >= 12, "watermark counts all rebuilt entries");"""

count = src.count(OLD)
assert count == 1, f"expected 1, found {count}"
src = src.replace(OLD, NEW, 1)
with io.open(REB, "w", encoding="utf-8", newline="") as f:
    f.write(src)
print("[ok] rebuild test updated")
