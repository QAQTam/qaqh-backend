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

# ============ timeline_hub.rs: set_offload 包装 + turn_needs_rehydrate ============
TH = r"D:\project\QAQ-Harness\crates\qaqh-runtime\src\ringing\timeline_hub.rs"
patch(TH, [
# set_offload 包装方法 + 启用点
(
"""    /// 用 offload 侧车补齐快照中已卸载（壳化）的 turn 文本。""",
"""    /// 启用 turn-seal 卸载：seal 后该 turn 的 reasoning/text 全文移出内存，
    /// 经 offload 侧车（`ringing-offload/{seed}.jsonl`）持久化；落盘快照时
    /// 由 rehydrate 补齐。见 `timeline_store::append_offloaded_turn`。
    pub fn enable_turn_offload(&self, seed: &str) {
        let store = self.timeline_store.lock().unwrap_or_else(|e| e.into_inner());
        let Some(store) = store.as_ref() else {
            return;
        };
        let store_seed = seed.to_string();
        let append_store = std::sync::Arc::new(());
        let _ = append_store;
        // 捕获 store 的方法引用：store 在 Arc<Mutex<Option<TimelineStore>>> 里，
        // 回调里再锁。为避免回调内死锁（persist_timeline_sync 持 store 锁时
        // seal 路径不会再触发），offload 回调只做 append（append-only 文件，
        // 不需要 store 可变状态），因此回调内短暂拿锁即可。
        let timeline = self.timeline.clone();
        let timeline_store = self.timeline_store.clone();
        let offload: crate::timeline::OffloadFn = std::sync::Arc::new(
            move |seed: &str, turn: &qaqh_domain::TimelineTurn| {
                let store_guard = timeline_store.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(store) = store_guard.as_ref() {
                    store.append_offloaded_turn(seed, turn);
                }
                drop(store_guard);
                let _ = &timeline;
                let _ = &store_seed;
            },
        );
        drop(store);
        self.timeline
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .set_offload(seed, Some(offload));
    }

    /// 用 offload 侧车补齐快照中已卸载（壳化）的 turn 文本。""",
"enable_turn_offload wrapper"),

# turn_needs_rehydrate helper（rehydrate 条件里引用了未定义函数，改为内联判定）
(
"""        for turn in &mut snapshot.turns {
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
"""        for turn in &mut snapshot.turns {
            // 壳判定：任意 block 文本 ≤ 512+补差（offload_turn_blocks 的预览上限）。
            // 精确判定用启发式：sealed turn 的任一 block 文本 < 预览上限且侧车
            // 有更新版本，则补齐（侧车没有时保持现状，无害）。
            if turn.sealed
                && turn.rounds.iter().any(|round| {
                    round.blocks.iter().any(|block| {
                        block.text.chars().count() <= 512
                            || block
                                .tool
                                .as_ref()
                                .is_some_and(|tool| tool.progress.chars().count() <= 512)
                    })
                })
            {
                if let Some(full) = store.load_offloaded_turn(seed, &turn.turn_id) {
                    *turn = full;
                }
            }
        }
        snapshot
    }""",
"rehydrate inline heuristic"),
])
print("[done] hub offload wrapper")
