import io

HUB = r"D:\project\QAQ-Harness\crates\qaqh-runtime\src\ringing\hub.rs"
with io.open(HUB, "r", encoding="utf-8") as f:
    src = f.read()

OLD = """        assert!(snapshot.turns[0].rounds[0].sealed);
        assert_eq!(
            snapshot.turns[0].rounds[0].blocks[0].state,
            qaqh_domain::TimelineBlockState::Sealed
        );
        assert_eq!(hub.timeline_replay_since("s", 1).len(), 5);"""
NEW = """        assert!(snapshot.turns[0].rounds[0].sealed);
        assert_eq!(
            snapshot.turns[0].rounds[0].blocks[0].state,
            qaqh_domain::TimelineBlockState::Sealed
        );
        // seal 裁剪后回放尾只含孤儿收尾的 TurnSealed 条目（1 条）。
        let replayed = hub.timeline_replay_since("s", 1);
        assert_eq!(
            replayed.len(),
            1,
            "sealed turn leaves only its own TurnSealed entry in the tail"
        );
        assert!(matches!(
            replayed[0].event,
            qaqh_domain::TimelineEvent::TurnSealed { .. }
        ));"""

count = src.count(OLD)
assert count == 1, f"expected 1, found {count}"
src = src.replace(OLD, NEW, 1)
with io.open(HUB, "w", encoding="utf-8", newline="") as f:
    f.write(src)
print("[ok] persisted_native replay assertion updated")
