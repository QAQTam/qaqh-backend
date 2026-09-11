import io

def patch(path, pairs):
    with io.open(path, "r", encoding="utf-8") as f:
        src = f.read()
    for old, new, label in pairs:
        count = src.count(old)
        assert count == 1, f"[{label}] expected 1, found {count}"
        src = src.replace(old, new, 1)
        print(f"[ok] {label}")
    with io.open(path, "w", encoding="utf-8", newline="") as f:
        f.write(src)

# ============ hub.rs: 新增 prune_superseded_checkpoints ============
HUB = r"D:\project\QAQ-Harness\crates\qaqh-runtime\src\ringing\hub.rs"
patch(HUB, [(
"""    /// 过滤已 seal turn 的 timeline journal 条目。已 seal turn 的 TextDelta/
    /// ToolProgress 已被快照全量覆盖，且 seal 后不会再有新 delta（append_text
    /// 拒绝已 seal block），因此持久化时丢弃这些条目不会破坏恢复：restore 的
    /// next_fragment 只从保留的活跃 turn 条目重建。这消除了每次 persist 都
    /// 全量克隆整个 journal 的写放大（曾实测 13.5MB JSON 每几秒重写一次）。""",
"""    /// 按 block 折叠落盘副本中被后续覆盖的 BlockCheckpoint。
    ///
    /// checkpoint 的物化语义是整块覆盖：快照恢复时每 block 只有最新一条生效。
    /// 旧 checkpoint 在落盘副本中纯属冗余——流式长块每 256ms 产出一条全量
    /// checkpoint（64 token 节流），7.8MB 块累计冗余可达 100 GB 级。
    /// 内存 journal 不折叠（回放 seq 连续性契约，见 handoff 设计定稿）；
    /// 仅折叠持久化副本：恢复后缺失的旧 seq 由 recover_gap 重基线（Phase 0
    /// 已接受的代价）。被折叠条目不扣 journal_bytes——那是内存态预算。
    pub(super) fn prune_superseded_checkpoints(journal: Vec<TimelineEntry>) -> Vec<TimelineEntry> {
        let mut newest_per_block: std::collections::HashSet<&str> =
            std::collections::HashSet::new();
        let mut keep = vec![true; journal.len()];
        for (index, entry) in journal.iter().enumerate().rev() {
            if let TimelineEvent::BlockCheckpoint { block_id, .. } = &entry.event
                && !newest_per_block.insert(block_id.as_str())
            {
                keep[index] = false;
            }
        }
        journal
            .into_iter()
            .zip(keep)
            .filter_map(|(entry, keep)| keep.then_some(entry))
            .collect()
    }

    /// 过滤已 seal turn 的 timeline journal 条目。已 seal turn 的 TextDelta/
    /// ToolProgress 已被快照全量覆盖，且 seal 后不会再有新 delta（append_text
    /// 拒绝已 seal block），因此持久化时丢弃这些条目不会破坏恢复：restore 的
    /// next_fragment 只从保留的活跃 turn 条目重建。这消除了每次 persist 都
    /// 全量克隆整个 journal 的写放大（曾实测 13.5MB JSON 每几秒重写一次）。""",
"prune_superseded_checkpoints added")])

# ============ timeline_hub.rs: 两个 persist 路径接入折叠 ============
TH = r"D:\project\QAQ-Harness\crates\qaqh-runtime\src\ringing\timeline_hub.rs"
patch(TH, [(
"""                        let Some((snapshot, journal)) = ({
                            let timeline = timeline.lock().unwrap_or_else(|e| e.into_inner());
                            timeline.snapshot(&seed).map(|snapshot| {
                                let journal = timeline.replay_since(&seed, 0);
                                let journal =
                                    Self::prune_sealed_timeline_journal(&snapshot, journal);
                                (snapshot, journal)
                            })
                        }) else {
                            continue;
                        };""",
"""                        let Some((snapshot, journal)) = ({
                            let timeline = timeline.lock().unwrap_or_else(|e| e.into_inner());
                            timeline.snapshot(&seed).map(|snapshot| {
                                let journal = timeline.replay_since(&seed, 0);
                                let journal =
                                    Self::prune_sealed_timeline_journal(&snapshot, journal);
                                let journal = Self::prune_superseded_checkpoints(journal);
                                (snapshot, journal)
                            })
                        }) else {
                            continue;
                        };""",
"async window: fold checkpoints")])

with io.open(TH, "r", encoding="utf-8") as f:
    th = f.read()
OLD_SYNC = """        let Some((snapshot, journal)) = (|| {
            let timeline = self.timeline.lock().unwrap_or_else(|e| e.into_inner());
            timeline.snapshot(seed).map(|snapshot| {
                let journal = timeline.replay_since(seed, 0);
                let journal = Self::prune_sealed_timeline_journal(&snapshot, journal);
                (snapshot, journal)
            })
        })() else {
            return;
        };"""
NEW_SYNC = """        let Some((snapshot, journal)) = (|| {
            let timeline = self.timeline.lock().unwrap_or_else(|e| e.into_inner());
            timeline.snapshot(seed).map(|snapshot| {
                let journal = timeline.replay_since(seed, 0);
                let journal = Self::prune_sealed_timeline_journal(&snapshot, journal);
                let journal = Self::prune_superseded_checkpoints(journal);
                (snapshot, journal)
            })
        })() else {
            return;
        };"""
count = th.count(OLD_SYNC)
assert count == 1, f"[sync persist fold] expected 1, found {count}"
th = th.replace(OLD_SYNC, NEW_SYNC, 1)
print("[ok] sync persist: fold checkpoints")
with io.open(TH, "w", encoding="utf-8", newline="") as f:
    f.write(th)

# ============ hub.rs tests: 折叠契约测试 ============
with io.open(HUB, "r", encoding="utf-8") as f:
    hub = f.read()
i = hub.rindex("    #[test]")
# 找 tests mod 的收尾（最后一个测试函数结束后）——改为在 prune_sealed 测试附近插入。
# 直接在文件末尾 tests mod 收尾前插入新测试。
OLD_TAIL_MARKER = "    pub(super) fn prune_superseded_checkpoints"
assert hub.count(OLD_TAIL_MARKER) == 1
print("[done] fold wiring complete")
