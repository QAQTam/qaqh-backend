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

# ============ 1. timeline.rs: offload 模式 ============
TL = r"D:\project\QAQ-Harness\crates\qaqh-runtime\src\timeline.rs"
patch(TL, [
(
"""#[derive(Debug, Default)]
struct SeedTimeline {
    next_seq: u64,
    turns: BTreeMap<String, TimelineTurn>,
    journal: Vec<TimelineEntry>,
    next_fragment: HashMap<(String, u32, String), u64>,
    /// journal 内滞留的 payload 字节数（text_delta/checkpoint/进度 chunk）。
    /// 驱逐时从头部扣减，O(1) 维护。
    journal_bytes: u64,
}""",
"""#[derive(Debug, Default)]
struct SeedTimeline {
    next_seq: u64,
    turns: BTreeMap<String, TimelineTurn>,
    journal: Vec<TimelineEntry>,
    next_fragment: HashMap<(String, u32, String), u64>,
    /// journal 内滞留的 payload 字节数（text_delta/checkpoint/进度 chunk）。
    /// 驱逐时从头部扣减，O(1) 维护。
    journal_bytes: u64,
    /// True = 已 seal turn 的 blocks 文本被卸载出内存（壳模式），由
    /// `offload` 回调在持久化时补齐快照全文。见 `set_offload`。
    offload_enabled: bool,
}""",
"SeedTimeline offload flag"),

(
"""    pub fn replay_since(&self, seed: &str, watermark: u64) -> Vec<TimelineEntry> {""",
"""    /// 开启 turn-seal 卸载：seal 后该 turn 的 blocks 文本移出内存，
    /// `offload` 回调负责持久化完整文本（offload 侧车）。reasoning 链路
    /// 常驻内存是长会话内存增长的主因之一；文本的持久权威由侧车承担，
    /// 内存只保留壳（turn 元数据 + 首块预览）。
    pub fn set_offload(
        &mut self,
        seed: &str,
        offload: Option<OffloadFn>,
    ) {
        if let Some(timeline) = self.seeds.get_mut(seed) {
            timeline.offload_enabled = offload.is_some();
            timeline.offload = offload;
        }
    }

    pub fn replay_since(&self, seed: &str, watermark: u64) -> Vec<TimelineEntry> {""",
"set_offload method"),

# SealTimeline 字段 + offload 类型定义（放 SeedTimeline 上方）
(
"""#[derive(Debug, Default)]
struct SeedTimeline {""",
"""/// turn seal 卸载回调：(seed, turn) 由调用方持久化完整文本。
pub type OffloadFn = std::sync::Arc<dyn Fn(&str, &TimelineTurn) + Send + Sync>;

#[derive(Debug, Default)]
struct SeedTimeline {""",
"OffloadFn type"),

# SeedTimeline 加 offload 字段
(
"""    /// True = 已 seal turn 的 blocks 文本被卸载出内存（壳模式），由
    /// `offload` 回调在持久化时补齐快照全文。见 `set_offload`。
    offload_enabled: bool,
}""",
"""    /// True = 已 seal turn 的 blocks 文本被卸载出内存（壳模式），由
    /// `offload` 回调在持久化时补齐快照全文。见 `set_offload`。
    offload_enabled: bool,
    /// 卸载回调（set_offload 注入；Default 的 None = 不卸载）。
    #[allow(clippy::type_complexity)]
    offload: Option<OffloadFn>,
}""",
"SeedTimeline offload field"),

# seal_turn_with_state: prune 前先 offload
(
"""        let entry = next_entry(
            timeline,
            turn_id.to_string(),
            None,
            TimelineEvent::TurnSealed { state, failure },
        );
        // seal 即时裁剪：sealed turn 的条目在快照内已物化，回放不再需要
        // （与 persist 侧 prune_sealed_timeline_journal 语义一致）。不裁剪则
        // journal 随会话累积（实测单会话 7.3 万条 / 25 MB）。
        prune_turn_journal(timeline, turn_id);
        Ok(entry)
    }""",
"""        let entry = next_entry(
            timeline,
            turn_id.to_string(),
            None,
            TimelineEvent::TurnSealed { state, failure },
        );
        // seal 即时裁剪：sealed turn 的条目在快照内已物化，回放不再需要
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
"seal: offload then shell"),

# offload_turn_blocks helper + restore 保持
(
"""/// 移除某 turn 的全部 journal 条目（seal 即时裁剪）。
fn prune_turn_journal(timeline: &mut SeedTimeline, turn_id: &str) {""",
"""/// 把 turn 卸载成壳：清空各 block 文本/进度，保留身份与元数据。
/// 首块保留 512 字符预览，前端列表仍可显示摘要；全文从侧车恢复。
fn offload_turn_blocks(turn: &mut TimelineTurn) {
    for round in &mut turn.rounds {
        for block in &mut round.blocks {
            if block.text.chars().count() > 512 {
                let preview: String = block.text.chars().take(512).collect();
                block.text = preview;
            }
            if let Some(tool) = &mut block.tool {
                if tool.progress.chars().count() > 512 {
                    let preview: String = tool.progress.chars().take(512).collect();
                    tool.progress = preview;
                }
                tool.output = None;
                tool.diff = None;
            }
        }
    }
}

/// 移除某 turn 的全部 journal 条目（seal 即时裁剪）。
fn prune_turn_journal(timeline: &mut SeedTimeline, turn_id: &str) {""",
"offload_turn_blocks helper"),
])
print("[done] timeline.rs offload core")
