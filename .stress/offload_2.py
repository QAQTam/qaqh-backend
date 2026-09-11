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

# ============ timeline_hub.rs: offload 侧车 + 接线 ============
TH = r"D:\project\QAQ-Harness\crates\qaqh-runtime\src\ringing\timeline_hub.rs"
patch(TH, [
# 1) persist_timeline_sync: 持久化前用侧车补齐全文
(
"""        // 轻量审计（seq/ts/type）：与 terminal 幂等，水位去重后只追加新条目。
        {
            let timeline = self.timeline.lock().unwrap_or_else(|e| e.into_inner());
            let audit_entries = timeline.replay_since(seed, 0);
            drop(timeline);
            store.append_audit(seed, &audit_entries);
        }
        if let Err(error) = store.persist(seed, &snapshot, journal) {
            log::warn!("[timeline] sync persist failed for {seed}: {error}");
        }
    }""",
"""        // 轻量审计（seq/ts/type）：与 terminal 幂等，水位去重后只追加新条目。
        {
            let timeline = self.timeline.lock().unwrap_or_else(|e| e.into_inner());
            let audit_entries = timeline.replay_since(seed, 0);
            drop(timeline);
            store.append_audit(seed, &audit_entries);
        }
        // offload 壳补齐：内存中已卸载的 sealed turn 在落盘前恢复全文，
        // 保证快照文件始终是「无侧车也能独立恢复」的完整权威。
        let snapshot = self.rehydrate_offloaded_turns(seed, snapshot);
        if let Err(error) = store.persist(seed, &snapshot, journal) {
            log::warn!("[timeline] sync persist failed for {seed}: {error}");
        }
    }

    /// 用 offload 侧车补齐快照中已卸载（壳化）的 turn 文本。
    /// 侧车缺失/损坏时保留壳（快照仍可恢复，全文降级为预览）。
    fn rehydrate_offloaded_turns(
        &self,
        seed: &str,
        mut snapshot: TimelineSnapshot,
    ) -> TimelineSnapshot {
        let store = self.timeline_store.lock().unwrap_or_else(|e| e.into_inner());
        let Some(store) = store.as_ref() else {
            return snapshot;
        };
        for turn in &mut snapshot.turns {
            if turn.sealed && turn.rounds.iter().any(|round| round.blocks.is_empty())
                || turn.sealed && turn_needs_rehydrate(turn)
            {
                if let Some(full) = store.load_offloaded_turn(seed, &turn.turn_id) {
                    *turn = full;
                }
            }
        }
        snapshot
    }""",
"sync persist rehydrate"),

# 2) start_timeline_persistence 的异步窗口同样补齐（快照文件唯一权威）
(
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
                        };
                        // offload 壳补齐（异步窗口；磁盘文件始终完整）。
                        let snapshot = {
                            let hub: &RingingHub = self;
                            hub.rehydrate_offloaded_turns(&seed, snapshot)
                        };""",
"async window rehydrate"),
])
print("[done] timeline_hub rehydrate wiring")
