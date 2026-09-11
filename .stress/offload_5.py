import io

def patch(path, pairs):
    with io.open(path, "r", encoding="utf-8") as f:
        src = f.read()
    for old, new, label in pairs:
        count = src.count(old)
        assert count == 1, f"[{label}] expected 1, found {count}\n---\n{old[:200]}"
        src = src.replace(old, new, 1)
        print(f"[ok] {label}")
    with io.open(path, "w", encoding="utf-8", newline="") as f:
        f.write(src)

# ============ timeline.rs 修复 3 处 ============
TL = r"D:\project\QAQ-Harness\crates\qaqh-runtime\src\timeline.rs"
patch(TL, [
# 1. Debug derive：offload 字段手动跳过 Debug —— 把 derive 拆开
(
"""#[derive(Debug, Default)]
struct SeedTimeline {""",
"""#[derive(Default)]
struct SeedTimeline {""",
"SeedTimeline drop Debug derive"),

# 2. seal 路径借用错误重写：不在闭包外再 timeline_mut，直接顺序处理
(
"""        // seal 即时裁剪：sealed turn 的条目在快照内已物化，回放不再需要
        // （与 persist 侧 prune_sealed_timeline_journal 语义一致）。不裁剪则
        // journal 随会话累积（实测单会话 7.3 万条 / 25 MB）。
        prune_turn_journal(timeline, turn_id);
        // turn-seal 卸载：先回调持久化完整文本，再把 blocks 清成壳。
        // 顺序不可反——回调失败（None/panic 边界）时保底不卸载。
        if timeline.offload_enabled {
            if let (Some(offload), Some(turn)) =
                (timeline.offload.clone(), timeline.turns.get(turn_id))
            {
                let snapshot_turn = turn.clone();
                let seed_owned = seed.to_string();
                drop(timeline);
                // 不持 timeline 锁执行磁盘 I/O；回调内部自行处理错误。
                offload(&seed_owned, &snapshot_turn);
                let timeline = timeline_mut(seed)?;
                if let Some(turn) = timeline.turns.get_mut(turn_id) {
                    offload_turn_blocks(turn);
                }
                let _ = timeline; // 借用结束
            }
        }
        Ok(entry)
    }""",
"""        // seal 即时裁剪：sealed turn 的条目在快照内已物化，回放不再需要
        // （与 persist 侧 prune_sealed_timeline_journal 语义一致）。不裁剪则
        // journal 随会话累积（实测单会话 7.3 万条 / 25 MB）。
        prune_turn_journal(timeline, turn_id);
        // turn-seal 卸载：先回调持久化完整文本，再把 blocks 清成壳。
        // 回调在持有 timeline 锁的状态下执行 append-only 追加（O(文本)
        // 一次写，无每秒重写），不做任何可锁 store 状态访问，无死锁面。
        if let Some(offload) = timeline.offload.clone() {
            if let Some(turn) = timeline.turns.get(turn_id) {
                offload(seed, turn);
            }
            if let Some(turn) = timeline.turns.get_mut(turn_id) {
                offload_turn_blocks(turn);
            }
        }
        Ok(entry)
    }""",
"seal offload borrow fix"),

# 3. restore initializer 补字段
(
"""            SeedTimeline {
                next_seq: snapshot.watermark,
                turns: snapshot
                    .turns
                    .into_iter()
                    .map(|turn| (turn.turn_id.clone(), turn))
                    .collect(),
                journal,
                next_fragment,
                journal_bytes,
            },""",
"""            SeedTimeline {
                next_seq: snapshot.watermark,
                turns: snapshot
                    .turns
                    .into_iter()
                    .map(|turn| (turn.turn_id.clone(), turn))
                    .collect(),
                journal,
                next_fragment,
                journal_bytes,
                offload_enabled: false,
                offload: None,
            },""",
"restore fields"),
])

# ============ timeline_hub.rs: E0521 修复（async window 里不引用 self） ============
TH = r"D:\project\QAQ-Harness\crates\qaqh-runtime\src\ringing\timeline_hub.rs"
patch(TH, [
(
"""                        // offload 壳补齐（异步窗口；磁盘文件始终完整）。
                        let snapshot = {
                            let hub: &RingingHub = self;
                            hub.rehydrate_offloaded_turns(&seed, snapshot)
                        };""",
"""                        // offload 壳补齐（异步窗口；磁盘文件始终完整）。
                        let snapshot = rehydrate_offloaded_turns(
                            &timeline_store,
                            &seed,
                            snapshot,
                        );""",
"async window uses free fn"),

# rehydrate_offloaded_turns 从 impl 内移到自由函数 + impl 内方法改为转发
(
"""    /// 用 offload 侧车补齐快照中已卸载（壳化）的 turn 文本。
    /// 侧车缺失/损坏时保留壳（快照仍可恢复，全文降级为预览）。
    fn rehydrate_offloaded_turns(
        &self,
        seed: &str,
        mut snapshot: TimelineSnapshot,
    ) -> TimelineSnapshot {
        let store = self.timeline_store.lock().unwrap_or_else(|e| e.into_inner());
        let Some(store) = store.as_ref() else {
            return snapshot;
        };""",
"""    /// 用 offload 侧车补齐快照中已卸载（壳化）的 turn 文本。
    /// 侧车缺失/损坏时保留壳（快照仍可恢复，全文降级为预览）。
    fn rehydrate_offloaded_turns(
        &self,
        seed: &str,
        snapshot: TimelineSnapshot,
    ) -> TimelineSnapshot {
        rehydrate_offloaded_turns(&self.timeline_store, seed, snapshot)
    }
}

/// 自由函数版本：供异步 persist 线程使用（不借用 &self）。
fn rehydrate_offloaded_turns(
    timeline_store: &std::sync::Arc<
        std::sync::Mutex<Option<crate::timeline_store::TimelineStore>>,
    >,
    seed: &str,
    mut snapshot: TimelineSnapshot,
) -> TimelineSnapshot {
    {
        let store = timeline_store.lock().unwrap_or_else(|e| e.into_inner());
        let Some(store) = store.as_ref() else {
            return snapshot;
        };""",
"rehydrate split free fn"),
])
print("[done] fixes")
