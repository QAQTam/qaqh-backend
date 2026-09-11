import io

# ============ timeline.rs: 修 journal_enforcement 测试 ============
TL = r"D:\project\QAQ-Harness\crates\qaqh-runtime\src\timeline.rs"
with io.open(TL, "r", encoding="utf-8") as f:
    tl = f.read()

OLD = """        let fragment_seqs: Vec<u64> = tail
            .iter()
            .filter_map(|e| match &e.event {
                TimelineEvent::TextDelta { fragment_seq, .. } => Some(*fragment_seq),
                _ => None,
            })
            .collect();
        // 窗口必须覆盖最新条目，且保留区连续。
        assert_eq!(*fragment_seqs.last().unwrap(), 599);
        let min_kept = *fragment_seqs.iter().min().unwrap();
        let expected: Vec<u64> = (min_kept..600).collect();
        assert_eq!(fragment_seqs, expected, "retained window must be contiguous");"""
NEW = """        let fragment_seqs: Vec<u64> = tail
            .iter()
            .filter_map(|e| match &e.event {
                TimelineEvent::TextDelta { fragment_seq, .. } => Some(*fragment_seq),
                _ => None,
            })
            .collect();
        // 窗口必须覆盖最新 delta，且保留区连续。
        assert!(
            fragment_seqs.contains(&599),
            "newest delta must survive eviction, got tail len {}",
            tail.len()
        );
        let min_kept = *fragment_seqs.iter().min().unwrap();
        let expected: Vec<u64> = (min_kept..600).collect();
        assert_eq!(fragment_seqs, expected, "retained window must be contiguous");"""
count = tl.count(OLD)
assert count == 1, f"journal test A: {count}"
tl = tl.replace(OLD, NEW, 1)
print("[ok] journal test: robust newest-delta assertion")
with io.open(TL, "w", encoding="utf-8", newline="") as f:
    f.write(tl)

# ============ hub.rs: 两个测试按最终语义改写 ============
HUB = r"D:\project\QAQ-Harness\crates\qaqh-runtime\src\ringing\hub.rs"
with io.open(HUB, "r", encoding="utf-8") as f:
    hub = f.read()

def sub(src, old, new, label):
    count = src.count(old)
    assert count == 1, f"[{label}] expected 1, found {count}"
    src = src.replace(old, new, 1)
    print(f"[ok] {label}")
    return src

# --- terminal test: TurnSealed 同步落盘 + 裁剪后 replay 尾为空 ---
hub = sub(hub,
"""        hub.publish_timeline(
            "s",
            TimelineIntent::BlockSealed {
                turn_id: "t".into(),
                round_num: 0,
                block_id: "text".into(),
            },
        )
        .unwrap();

        let persisted = TimelineStore::new(&root)
            .unwrap()
            .load_seed("s")
            .expect("terminal snapshot persisted synchronously");
        assert_eq!(persisted.snapshot.watermark, 4);
        assert_eq!(
            persisted.snapshot.turns[0].rounds[0].blocks[0].text,
            "hello"
        );
        assert_eq!(
            persisted.snapshot.turns[0].rounds[0].blocks[0].state,
            qaqh_domain::TimelineBlockState::Sealed
        );
        drop(hub);
        let _ = std::fs::remove_dir_all(root);
    }""",
"""        hub.publish_timeline(
            "s",
            TimelineIntent::BlockSealed {
                turn_id: "t".into(),
                round_num: 0,
                block_id: "text".into(),
            },
        )
        .unwrap();
        // BlockSealed 已降级为异步 checkpoint（终端事件写放大收口）。
        // TurnSealed 保持同步落盘：load_seed 立即可见全部物化文本。
        hub.publish_timeline(
            "s",
            TimelineIntent::TurnSealed {
                turn_id: "t".into(),
                state: qaqh_domain::TimelineTurnState::Completed,
                failure: None,
            },
        )
        .unwrap();

        let persisted = TimelineStore::new(&root)
            .unwrap()
            .load_seed("s")
            .expect("turn-sealed snapshot persisted synchronously");
        assert_eq!(persisted.snapshot.watermark, 5);
        assert_eq!(
            persisted.snapshot.turns[0].rounds[0].blocks[0].text,
            "hello"
        );
        assert_eq!(
            persisted.snapshot.turns[0].rounds[0].blocks[0].state,
            qaqh_domain::TimelineBlockState::Sealed
        );
        assert!(persisted.snapshot.turns[0].sealed, "turn sealed synchronously");
        assert!(
            persisted.journal.is_empty(),
            "sealed turn leaves no replay tail on disk"
        );
        drop(hub);
        let _ = std::fs::remove_dir_all(root);
    }""",
"terminal test rewritten"),

# --- persisted_native test: replay 尾为空（含 TurnSealed 也被裁剪） ---
hub = sub(hub,
"""        // seal 裁剪后回放尾只含孤儿收尾的 TurnSealed 条目（1 条）。
        let replayed = hub.timeline_replay_since("s", 1);
        assert_eq!(
            replayed.len(),
            1,
            "sealed turn leaves only its own TurnSealed entry in the tail"
        );
        assert!(matches!(
            replayed[0].event,
            qaqh_domain::TimelineEvent::TurnSealed { .. }
        ));""",
"""        // seal 裁剪语义：turn seal 后回放尾清空（TurnSealed 条目自身也被
        // 裁剪）；重连客户端由快照 watermark 重基线（recover_gap 契约）。
        assert!(
            hub.timeline_replay_since("s", 1).is_empty(),
            "sealed turn must leave an empty replay tail"
        );""",
"persisted test: empty tail"),

with io.open(HUB, "w", encoding="utf-8", newline="") as f:
    f.write(hub)
print("[done] fixes phase 2")
