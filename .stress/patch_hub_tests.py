import io

# ---- timeline_hub.rs ----
HUB = r"D:\project\QAQ-Harness\crates\qaqh-runtime\src\ringing\timeline_hub.rs"
with io.open(HUB, "r", encoding="utf-8") as f:
    hub = f.read()

def sub_exact(src, old, new, label):
    count = src.count(old)
    assert count == 1, f"[{label}] expected 1, found {count}"
    src = src.replace(old, new, 1)
    print(f"[ok] {label}")
    return src

OLD_PUBLISH = """        // Terminal intents (block/round/turn sealed) are the recovery boundary
        // for a restarting client: persisting them synchronously shrinks the
        // window in which a crash can lose the transcript tail from "the whole
        // turn" to "the current open blocks". Everything else keeps the
        // coalesced async checkpoint to stay off the streaming hot path.
        let terminal = Self::timeline_intent_is_terminal(&intent);
        let entry = {
            let mut timeline = self.timeline.lock().unwrap_or_else(|e| e.into_inner());
            timeline.apply_intent(seed, intent)?
        };
        if terminal {
            self.persist_timeline_sync(seed);
        } else {
            self.request_timeline_persistence(seed);
        }"""
NEW_PUBLISH = """        // Terminal intents are the recovery boundary for a restarting client.
        // TurnSealed 保持同步落盘（turn 级边界，一个 turn 一次，成本可控）；
        // BlockSealed/RoundSealed 降级为 1s 合并窗口异步 checkpoint：长上下文
        // 会话每 turn 可产生几十个 round（实测 79 个），每次同步全量重写快照
        // 使终端事件成为主要写放大源，而 crash 窗口差异只有毫秒级（合并窗口
        // 本身就是周期性 crash checkpoint）。
        let entry = {
            let mut timeline = self.timeline.lock().unwrap_or_else(|e| e.into_inner());
            timeline.apply_intent(seed, intent)?
        };
        if matches!(intent, TimelineIntent::TurnSealed { .. }) {
            self.persist_timeline_sync(seed);
        } else {
            self.request_timeline_persistence(seed);
        }"""
hub = sub_exact(hub, OLD_PUBLISH, NEW_PUBLISH, "publish_timeline 收口")

OLD_HELPER = """    /// Terminal intents seal a block/round/turn — the client's recovery
    /// boundary. They are persisted synchronously so a crash between the seal
    /// and the next async checkpoint cannot drop a completed unit of work.
    pub(super) fn timeline_intent_is_terminal(intent: &TimelineIntent) -> bool {
        matches!(
            intent,
            TimelineIntent::BlockSealed { .. }
                | TimelineIntent::RoundSealed { .. }
                | TimelineIntent::TurnSealed { .. }
        )
    }"""
NEW_HELPER = """    /// TurnSealed 是 turn 级恢复边界，同步落盘（见 publish_timeline 注释）。
    /// BlockSealed/RoundSealed 已降级为异步 checkpoint，不再视作同步终端。
    pub(super) fn timeline_intent_is_terminal(intent: &TimelineIntent) -> bool {
        matches!(intent, TimelineIntent::TurnSealed { .. })
    }"""
hub = sub_exact(hub, OLD_HELPER, NEW_HELPER, "timeline_intent_is_terminal 收窄")

with io.open(HUB, "w", encoding="utf-8", newline="") as f:
    f.write(hub)

# ---- timeline.rs: append 3 tests before final closing brace of tests mod ----
TL = r"D:\project\QAQ-Harness\crates\qaqh-runtime\src\timeline.rs"
with io.open(TL, "r", encoding="utf-8") as f:
    tl = f.read()

OLD_TAIL = """        // 水位到底 = 无条目可回放（客户端已对齐，不产生 gap）。
        assert!(appender.replay_since("s", watermark).is_empty());
    }
}"""
NEW_TAIL = """        // 水位到底 = 无条目可回放（客户端已对齐，不产生 gap）。
        assert!(appender.replay_since("s", watermark).is_empty());
    }

    #[test]
    fn sealed_turn_journal_is_pruned_immediately() {
        // Phase 4 seal 即时裁剪：turn seal 后其全部条目必须立即离开内存
        // journal（快照已物化）；后续 turn 的条目照常进入回放窗口。
        let mut appender = TimelineAppender::new();
        appender.open_turn("s", "t1", "q1").unwrap();
        appender
            .open_block("s", "t1", 0, "r", TimelineBlockKind::Reasoning, None)
            .unwrap();
        appender
            .append_text("s", "t1", 0, "r", 0, "long reasoning text")
            .unwrap();
        appender.seal_block("s", "t1", 0, "r").unwrap();
        appender.seal_round("s", "t1", 0, false).unwrap();
        assert!(!appender.replay_since("s", 0).is_empty());

        appender.seal_turn("s", "t1").unwrap();
        assert!(
            appender.replay_since("s", 0).is_empty(),
            "sealed turn entries must leave the replay tail immediately"
        );
        let snapshot = appender.snapshot("s").unwrap();
        assert_eq!(snapshot.turns.len(), 1, "snapshot keeps the materialized turn");
        assert!(snapshot.turns[0].sealed);
        assert_eq!(
            snapshot.turns[0].rounds[0].blocks[0].text,
            "long reasoning text"
        );

        appender.open_turn("s", "t2", "q2").unwrap();
        assert_eq!(appender.replay_since("s", 0).len(), 1);
    }

    #[test]
    fn journal_enforcement_bounds_entries_and_bytes() {
        // 双限驱逐：字节越界后最老 delta 被驱逐，且被驱逐区间是连续前缀。
        let mut appender = TimelineAppender::new();
        appender.open_turn("s", "t1", "long stream").unwrap();
        appender
            .open_block("s", "t1", 0, "r", TimelineBlockKind::Reasoning, None)
            .unwrap();
        // 600 条 x 1 KB = 600 KB payload > 512 KB 测试字节界。
        let chunk = "x".repeat(1024);
        for seq in 0..600u64 {
            appender
                .append_text("s", "t1", 0, "r", seq, chunk.clone())
                .unwrap();
        }
        let tail = appender.replay_since("s", 0);
        assert!(
            tail.len() < 600,
            "byte budget must evict old deltas, got {} entries",
            tail.len()
        );
        let fragment_seqs: Vec<u64> = tail
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
        assert_eq!(fragment_seqs, expected, "retained window must be contiguous");
    }

    #[test]
    fn restore_rebuilds_journal_byte_budget() {
        // restore 后字节预算必须与 journal 实际 payload 一致。
        let mut appender = TimelineAppender::new();
        appender.open_turn("s", "t1", "q").unwrap();
        appender
            .open_block("s", "t1", 0, "r", TimelineBlockKind::Reasoning, None)
            .unwrap();
        appender.append_text("s", "t1", 0, "r", 0, "payload-123").unwrap();
        let journal = appender.replay_since("s", 0);
        let snapshot = appender.snapshot("s").unwrap();

        let mut restored = TimelineAppender::new();
        restored.restore("s".into(), snapshot, journal);
        let expected: u64 = restored
            .replay_since("s", 0)
            .iter()
            .map(|e| journal_entry_payload_bytes(&e.event))
            .sum();
        let seed = restored.seeds.get("s").unwrap();
        assert_eq!(seed.journal_bytes, expected);
    }
}"""
tl = sub_exact(tl, OLD_TAIL, NEW_TAIL, "timeline tests")

with io.open(TL, "w", encoding="utf-8", newline="") as f:
    f.write(tl)

print("[done] hub + tests patched")
