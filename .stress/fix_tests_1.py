import io

def patch(path, pairs):
    with io.open(path, "r", encoding="utf-8") as f:
        src = f.read()
    for old, new, label in pairs:
        count = src.count(old)
        assert count == 1, f"[{label}] expected 1, found {count} in {path}"
        src = src.replace(old, new, 1)
        print(f"[ok] {label}")
    with io.open(path, "w", encoding="utf-8", newline="") as f:
        f.write(src)

# ============ 1. persistence_policy.rs: 字节上限测试覆写 ============
POL = r"D:\project\QAQ-Harness\crates\qaqh-runtime\src\ringing\persistence_policy.rs"
patch(POL, [
(
"""pub const MAX_TIMELINE_JOURNAL_BYTES: u64 = 256 * 1024 * 1024;""",
"""pub const MAX_TIMELINE_JOURNAL_BYTES: u64 = 256 * 1024 * 1024;

/// 测试用字节上限覆写（OnceLock 一次性；仅测试模块设置，模式同
/// hub.rs 的 JOURNAL_REWRITE_THRESHOLD_OVERRIDE）。
static JOURNAL_BYTE_LIMIT_OVERRIDE: std::sync::OnceLock<u64> = std::sync::OnceLock::new();

/// 生效中的回放尾字节上限（生产为 [`MAX_TIMELINE_JOURNAL_BYTES`]）。
pub fn journal_byte_limit() -> u64 {
    *JOURNAL_BYTE_LIMIT_OVERRIDE
        .get()
        .unwrap_or(&MAX_TIMELINE_JOURNAL_BYTES)
}

#[cfg(test)]
pub(crate) fn set_journal_byte_limit_for_test(limit: u64) {
    let _ = JOURNAL_BYTE_LIMIT_OVERRIDE.set(limit);
}""",
"persistence_policy override"),
])

# ============ 2. timeline.rs: enforce 用 journal_byte_limit() ============
TL = r"D:\project\QAQ-Harness\crates\qaqh-runtime\src\timeline.rs"
patch(TL, [
(
"""fn enforce_journal_budget(timeline: &mut SeedTimeline) {
    let entry_limit = crate::ringing::persistence_policy::MAX_TIMELINE_JOURNAL_ENTRIES;
    let byte_limit = crate::ringing::persistence_policy::MAX_TIMELINE_JOURNAL_BYTES;""",
"""fn enforce_journal_budget(timeline: &mut SeedTimeline) {
    let entry_limit = crate::ringing::persistence_policy::MAX_TIMELINE_JOURNAL_ENTRIES;
    let byte_limit = crate::ringing::persistence_policy::journal_byte_limit();""",
"enforce uses override-able limit"),

# ---- appender_assigns_a_single_order: replay 断言移到 seal 前 ----
(
"""        appender.seal_round("s", "t", 0, true).unwrap();
        appender.seal_turn("s", "t").unwrap();

        let snapshot = appender.snapshot("s").unwrap();
        let blocks = &snapshot.turns[0].rounds[0].blocks;""",
"""        appender.seal_round("s", "t", 0, true).unwrap();
        // seal 裁剪语义：TurnSealed 会清空该 turn 的回放尾，seq 连续性
        // 必须在 seal 前断言；watermark 含 TurnSealed 占用的下一序号。
        let last_seq_before_seal = appender
            .replay_since("s", 0)
            .last()
            .expect("journal holds entries while the turn is active")
            .timeline_seq;
        appender.seal_turn("s", "t").unwrap();

        let snapshot = appender.snapshot("s").unwrap();
        let blocks = &snapshot.turns[0].rounds[0].blocks;""",
"appender_assigns: capture before seal"),
(
"""        assert_eq!(
            appender.replay_since("s", 0).last().unwrap().timeline_seq,
            snapshot.watermark
        );
    }""",
"""        assert_eq!(snapshot.watermark, last_seq_before_seal + 1);
    }""",
"appender_assigns: watermark assertion"),

# ---- replay_tail_covers: seal 移到断言之后 ----
(
"""        appender.seal_round("s", "t1", 0, true).unwrap();
        appender.seal_turn("s", "t1").unwrap();

        let all = appender.replay_since("s", 0);""",
"""        appender.seal_round("s", "t1", 0, true).unwrap();

        let all = appender.replay_since("s", 0);""",
"replay_tail: move seal after"),
(
"""        // 水位到底 = 无条目可回放（客户端已对齐，不产生 gap）。
        assert!(appender.replay_since("s", watermark).is_empty());
    }

    #[test]
    fn sealed_turn_journal_is_pruned_immediately() {""",
"""        // 水位到底 = 无条目可回放（客户端已对齐，不产生 gap）。
        assert!(appender.replay_since("s", watermark).is_empty());

        // seal 后回放尾清空（seal 即时裁剪契约，详见 sealed_turn 测试）。
        appender.seal_turn("s", "t1").unwrap();
        assert!(appender.replay_since("s", 0).is_empty());
    }

    #[test]
    fn sealed_turn_journal_is_pruned_immediately() {""",
"replay_tail: seal at end"),

# ---- 我的字节测试：设置覆写 + 补条数分支 ----
(
"""    #[test]
    fn journal_enforcement_bounds_entries_and_bytes() {
        // 双限驱逐：字节越界后最老 delta 被驱逐，且被驱逐区间是连续前缀。
        let mut appender = TimelineAppender::new();""",
"""    #[test]
    fn journal_enforcement_bounds_entries_and_bytes() {
        // 双限驱逐：字节越界后最老 delta 被驱逐，且被驱逐区间是连续前缀。
        // 测试覆写为 512 KB（生产 256 MB 无法在单测内写满）。
        crate::ringing::persistence_policy::set_journal_byte_limit_for_test(512 * 1024);
        let mut appender = TimelineAppender::new();""",
"journal test: override limit"),
(
"""        let expected: Vec<u64> = (min_kept..600).collect();
        assert_eq!(fragment_seqs, expected, "retained window must be contiguous");
    }""",
"""        let expected: Vec<u64> = (min_kept..600).collect();
        assert_eq!(fragment_seqs, expected, "retained window must be contiguous");

        // 条数上限分支：微小 delta 推过 8192 条后同样头部驱逐。
        let mut many = TimelineAppender::new();
        many.open_turn("s2", "t1", "many deltas").unwrap();
        many.open_block("s2", "t1", 0, "r", TimelineBlockKind::Reasoning, None)
            .unwrap();
        for seq in 0..8500u64 {
            many.append_text("s2", "t1", 0, "r", seq, "x".to_string())
                .unwrap();
        }
        let bounded = many.replay_since("s2", 0);
        assert!(
            bounded.len() <= 8192,
            "entry budget must bound the replay tail, got {}",
            bounded.len()
        );
        let last_seq = bounded.last().unwrap().timeline_seq;
        assert_eq!(
            many.snapshot("s2").unwrap().watermark,
            last_seq + 1,
            "watermark must include the TurnOpened/BlockOpened offset"
        );
    }""",
"journal test: entry-limit branch"),
])
print("[done] phase-1 fixes")
