import io

HUB = r"D:\project\QAQ-Harness\crates\qaqh-runtime\src\ringing\hub.rs"
with io.open(HUB, "r", encoding="utf-8") as f:
    hub = f.read()

def sub(src, old, new, label):
    count = src.count(old)
    assert count == 1, f"[{label}] expected 1, found {count}"
    src = src.replace(old, new, 1)
    print(f"[ok] {label}")
    return src

# 在 prune_sealed_timeline_journal 函数后面插入折叠测试。
# 锚点：persist_checkpoint 函数定义前。
OLD_ANCHOR = """    fn persist_checkpoint(
        &self,
        channel: RingingChannel,
        seed: &str,
        identity: &str,
        stream_seq: u64,
    ) {"""
NEW_ANCHOR = """    #[cfg(test)]
    fn fold_checkpoints_for_test(entries: Vec<TimelineEntry>) -> Vec<TimelineEntry> {
        Self::prune_superseded_checkpoints(entries)
    }

    fn persist_checkpoint(
        &self,
        channel: RingingChannel,
        seed: &str,
        identity: &str,
        stream_seq: u64,
    ) {"""
hub = sub(hub, OLD_ANCHOR, NEW_ANCHOR, "test hook")

# tests mod 末尾追加折叠测试（锚点：最后一个 #[test] 前面的内容不可靠，改用
# mod tests 内已知测试 native_timeline_intents_bypass 的结尾追加）。
OLD_TEST_TAIL = """    #[test]
    fn native_timeline_intents_bypass_the_ringing_v1_channel_sequencer() {"""
NEW_TEST_TAIL = """    #[test]
    fn fold_checkpoints_keeps_only_the_newest_per_block() {
        // 落盘副本折叠契约：同一 block 的旧 checkpoint 全部丢弃，只留最新
        // 一条；非 checkpoint 条目与其它 block 的条目不受影响；顺序保持。
        use qaqh_domain::TimelineEvent;
        let mk = |seq: u64, turn: &str, block: &str, text: &str| TimelineEntry {
            timeline_seq: seq,
            turn_id: turn.into(),
            round_num: Some(0),
            event: TimelineEvent::BlockCheckpoint {
                block_id: block.into(),
                text: text.into(),
            },
        };
        let delta = |seq: u64| TimelineEntry {
            timeline_seq: seq,
            turn_id: "t".into(),
            round_num: Some(0),
            event: TimelineEvent::TextDelta {
                block_id: "b".into(),
                fragment_seq: seq,
                delta: "d".into(),
            },
        };
        let entries = vec![
            mk(1, "t", "b", "v1"),
            delta(2),
            mk(3, "t", "b", "v2"),
            mk(4, "t", "c", "c1"),
            mk(5, "t", "b", "v3-final"),
            delta(6),
            mk(7, "t", "c", "c2-final"),
        ];
        let folded = RingingHub::fold_checkpoints_for_test(entries);
        let seqs: Vec<u64> = folded.iter().map(|e| e.timeline_seq).collect();
        assert_eq!(seqs, vec![2, 5, 6, 7], "only newest checkpoint per block survives");
        assert!(
            folded
                .iter()
                .all(|e| !matches!(&e.event,
                    TimelineEvent::BlockCheckpoint { block_id, text }
                        if block_id == "b" && text == "v1"))
        );
    }

    #[test]
    fn native_timeline_intents_bypass_the_ringing_v1_channel_sequencer() {"""
hub = sub(hub, OLD_TEST_TAIL, NEW_TEST_TAIL, "fold test added")

with io.open(HUB, "w", encoding="utf-8", newline="") as f:
    f.write(hub)
print("[done] fold test inserted")
