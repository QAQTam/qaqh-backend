import io

TH = r"D:\project\QAQ-Harness\crates\qaqh-runtime\src\ringing\timeline_hub.rs"
with io.open(TH, "r", encoding="utf-8") as f:
    src = f.read()

OLD = """        for turn in &mut snapshot.turns {
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
    }

    /// 同步落盘所有待写 seed（daemon 优雅关闭收尾；Drop 只 join 异步线程，
    /// 而 Arc 引用可能仍在 tokio task 中存活，必须显式 flush）。
    pub fn flush_timeline_persistence(&self) {"""
NEW = """        for turn in &mut snapshot.turns {
            // 壳判定（启发式）：sealed turn 的任一 block 文本/进度 ≤ 512 字符
            // 预览上限且侧车有更新版本，则补齐（侧车没有时保持现状，无害）。
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
}

impl RingingHub {
    /// 同步落盘所有待写 seed（daemon 优雅关闭收尾；Drop 只 join 异步线程，
    /// 而 Arc 引用可能仍在 tokio task 中存活，必须显式 flush）。
    pub fn flush_timeline_persistence(&self) {"""

count = src.count(OLD)
assert count == 1, f"expected 1, found {count}"
src = src.replace(OLD, NEW, 1)
with io.open(TH, "w", encoding="utf-8", newline="") as f:
    f.write(src)
print("[ok] impl boundary restored")
