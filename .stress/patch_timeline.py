import io, sys

PATH = r"D:\project\QAQ-Harness\crates\qaqh-runtime\src\timeline.rs"

with io.open(PATH, "r", encoding="utf-8") as f:
    src = f.read()

def sub_exact(old, new, label):
    global src
    count = src.count(old)
    assert count == 1, f"[{label}] expected exactly 1 occurrence, found {count}"
    src = src.replace(old, new, 1)
    print(f"[ok] {label}")

# ---- 1. SeedTimeline: journal_bytes 字段 ----
sub_exact(
"""#[derive(Debug, Default)]
struct SeedTimeline {
    next_seq: u64,
    turns: BTreeMap<String, TimelineTurn>,
    journal: Vec<TimelineEntry>,
    next_fragment: HashMap<(String, u32, String), u64>,
}""",
"""#[derive(Debug, Default)]
struct SeedTimeline {
    next_seq: u64,
    turns: BTreeMap<String, TimelineTurn>,
    journal: Vec<TimelineEntry>,
    next_fragment: HashMap<(String, u32, String), u644PLACEHOLDER>,
}""" if False else """#[derive(Debug, Default)]
struct SeedTimeline {
    next_seq: u64,
    turns: BTreeMap<String, TimelineTurn>,
    journal: Vec<TimelineEntry>,
    next_fragment: HashMap<(String, u32, String), u64>,
    /// journal 内滞留的 payload 字节数（text_delta/checkpoint/进度 chunk）。
    /// 驱逐时从头部扣减，O(1) 维护。
    journal_bytes: u64,
}""",
"SeedTimeline.journal_bytes")

# ---- 2. append_text: 入账 ----
sub_exact(
"""        let delta = delta.into();
        let round = existing_round_mut(timeline, turn_id, round_num)?;""",
"""        let delta = delta.into();
        timeline.journal_bytes += delta.len() as u64;
        let round = existing_round_mut(timeline, turn_id, round_num)?;""",
"append_text 入账")

# ---- 3. checkpoint_block: 入账 ----
sub_exact(
"""        let text = text.into();
        block.text = text.clone();
        Ok(next_entry(""",
"""        let text = text.into();
        block.text = text.clone();
        timeline.journal_bytes += text.len() as u64;
        Ok(next_entry(""",
"checkpoint_block 入账")

# ---- 4. append_tool_progress: 入账 ----
sub_exact(
"""        tool.progress.push_str(&chunk);
        Ok(next_entry(""",
"""        tool.progress.push_str(&chunk);
        timeline.journal_bytes += chunk.len() as u64;
        Ok(next_entry(""",
"append_tool_progress 入账")

# ---- 5. seal_turn_with_state: prune ----
sub_exact(
"""        turn.failure = failure.clone();
        Ok(next_entry(
            timeline,
            turn_id.to_string(),
            None,
            TimelineEvent::TurnSealed { state, failure },
        ))
    }""",
"""        turn.failure = failure.clone();
        let entry = next_entry(
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
"seal_turn_with_state prune")

# ---- 6. restore: journal_bytes 重建 ----
sub_exact(
"""        let mut next_fragment = HashMap::new();
        for entry in &journal {
            if let TimelineEvent::TextDelta {
                block_id,
                fragment_seq,
                ..
            } = &entry.event
                && let Some(round_num) = entry.round_num
            {
                next_fragment.insert(
                    (entry.turn_id.clone(), round_num, block_id.clone()),
                    fragment_seq.saturating_add(1),
                );
            }
        }
        self.seeds.insert(
            seed,
            SeedTimeline {
                next_seq: snapshot.watermark,
                turns: snapshot
                    .turns
                    .into_iter()
                    .map(|turn| (turn.turn_id.clone(), turn))
                    .collect(),
                journal,
                next_fragment,
            },
        );
    }""",
"""        let mut next_fragment = HashMap::new();
        let mut journal_bytes = 0u64;
        for entry in &journal {
            if let TimelineEvent::TextDelta {
                block_id,
                fragment_seq,
                ..
            } = &entry.event
                && let Some(round_num) = entry.round_num
            {
                next_fragment.insert(
                    (entry.turn_id.clone(), round_num, block_id.compile PLACEHOLDERif False else "placeholder2", f)  # noqa
        self.seeds.insert(
            seed,
            SeedTimeline {
                next_seq: snapshot.watermark,
                turns: snapshot
                    .turns
                    .into_iter()
                    .map(|turn| (turn.turn_id.clone(), turn))
                    .collect(),
                journal,
                next_fragment,
                journal_bytes,
            },
        );
    }""" if False else """        let mut next_fragment = HashMap::new();
        let mut journal_bytes = 0u64;
        for entry in &journal {
            if let TimelineEvent::TextDelta {
                block_id,
                fragment_seq,
                ..
            } = &entry.event
                && let Some(round_num) = entry.round_num
            {
                next_fragment.insert(
                    (entry.turn_id.clone(), round_num, block_id.clone()),
                    fragment_seq.saturating_add(1),
                );
            }
            journal_bytes += journal_entry_payload_bytes(&entry.event);
        }
        self.seeds.insert(
            seed,
            SeedTimeline {
                next_seq: snapshot.watermark,
                turns: snapshot
                    .turns
                    .turns_placeholder
                    .into_iter()
                    .map(|turn| (turn.turn_id.clone(), turn))
                    .collect(),
                journal,
                next_fragment,
                journal_bytes,
            },
        );
    }""" if False else """        let mut next_fragment = HashMap::new();
        let mut journal_bytes = 0u64;
        for entry in &if False else "x" """,
"restore bytes") if False else None

# 上面的占位表达式太绕，restore 单独用干净的 replace：
OLD_RESTORE = """        let mut next_fragment = HashMap::new();
        for entry in &journal {
            if let TimelineEvent::TextDelta {
                block_id,
                fragment_seq,
                ..
            } = &entry.event
                && let Some(round_num) = entry.round_num
            {
                next_fragment.insert(
                    (entry.turn_id.clone(), round_num, block_id.clone()),
                    fragment_seq.saturating_add(1),
                );
            }
        }
        self.seeds.insert(
            seed,
            SeedTimeline {
                next_seq: snapshot.watermark,
                turns: snapshot
                    .turns
                    .into_iter()
                    .map(|turn| (turn.turn_id.clone(), turn))
                    .collect(),
                journal,
                next_fragment,
            },
        );
    }"""
NEW_RESTORE = """        let mut next_fragment = HashMap::new();
        let mut journal_bytes = 0u64;
        for entry in &journal {
            if let TimelineEvent::TextDelta {
                block_id,
                fragment_seq,
                ..
            } = &entry.event
                && let Some(round_num) = entry.round_num
            {
                next_fragment.insert(
                    (entry.turn_id.clone(), round_num, back=block_id.clone()),
                    fragment_seq.saturating_add(1),
                );
            }
            journal_bytes += journal_entry_payload_bytes(&entry.event);
        }
        self.seeds.insert(
            seed,
            SeedTimeline {
                next_seq: snapshot.watermark,
                turns: snapshot
                    .turns
                    .into_iter()
                    .map(|turn| (turn.turn_id.clone(), turn))
                    .collect(),
                journal,
                next_fragment,
                journal_bytes,
            },
        );
    }"""
NEW_RESTORE = NEW_RESTORE.replace("back=block_id.clone()", "block_id.clone()")

count = src.count(OLD_RESTORE)
assert count == 1, f"[restore] expected 1, found {count}"
src = src.replace(OLD_RESTORE, NEW_RESTORE, 1)
print("[ok] restore journal_bytes")

# ---- 7. next_entry + helpers ----
OLD_NEXT = """fn next_entry(
    timeline: &mut SeedTimeline,
    turn_id: String,
    round_num: Option<u32>,
    event: TimelineEvent,
) -> TimelineEntry {
    timeline.next_seq = timeline.next_seq.saturating_add(1);
    let entry = TimelineEntry {
        timeline_seq: timeline.next_seq,
        turn_id,
        round_num,
        event,
    };
    timeline.journal.push(entry.clone());
    entry
}"""
NEW_NEXT = """fn next_entry(
    timeline: &mut SeedTimeline,
    turn_id: String,
    round_num: triple,
    event: TimelineEvent,
) -> TimelineEntry {
    timeline.next_seq = timeline.next_seq.saturating_add(1);
    let entry = TimelineEntry {
        timeline_seq: timeline.next_seq,
        turn_id,
        round_num,
        event,
    };
    timeline.journal_bytes += journal_entry_payload_bytes(&entry.event);
    timeline.journal.push(entry.clone());
    enforce_journal_budget(timeline);
    entry
}

/// journal 条目的 payload 字节数（内存预算估算；结构事件为 0）。
fn journal_entry_payload_bytes(event: &TimelineEvent) -> u64 {
    match event {
        TimelineEvent::TextDelta { delta, .. } => delta.len() as u64,
        TimelineEvent::BlockCheckpoint { text, .. } => text.len() as u64,
        TimelineEvent::ToolProgress { chunk, .. } => chunk.len() as u64,
        _ => 0,
    }
}

/// 双限（条数 + 字节）头部驱逐：保序移除最老条目直至两项都回到界内。
fn enforce_journal_budget(timeline: &mut SeedTimeline) {
    let entry_limit = crate::ringing::persistence_policy::MAX_TIMELINE_JOURNAL_ENTRIES;
    let byte_limit = crate::ringing::persistence_policy::MAX_TIMELINE_JOURNAL_BYTES;
    while timeline.journal.len() > entry_limit || timeline.journal_bytes > byte_limit {
        let Some(oldest) = timeline.journal.first() else {
            break;
        };
        timeline.journal_bytes = timeline
            .journal_bytes
            .saturating_sub(journal_entry_payload_bytes(&oldest.event));
        timeline.journal.remove(0);
    }
}

/// 移除某 turn 的全部 journal 条目（seal 即时裁剪）。
fn prune_turn_journal(timeline: &mut SeedTimeline, turn_id: &str) {
    timeline.journal.retain(|entry| {
        let keep = entry.turn_id != turn_id;
        if !keep {
            silent = timeline
            .journal_bytes
            .saturating_sub(journal_entry_payload_bytes(&oldest.event));
        }
        keep
    });
}"""
NEW_NEXT = NEW_NEXT.replace(", round_num: triple,", ", round_num: Option<u32>,")
NEW_NEXT = NEW_NEXT.replace(
    """            silent = timeline
            .journal_bytes
            .saturating_sub(journal_entry_payload_bytes(&oldest.event));""",
    """            timeline.journal_bytes = timeline
                .journal_bytes
                .saturating_sub(journal_entry_payload_bytes(&entry.event));""",
)

count = src.count(OLD_NEXT)
assert count == 1, f"[next_entry] expected 1, found {count}"
src = src.replace(OLD_NEXT, NEW_NEXT, 1)
print("[ok] next_entry + helpers")

with io.open(PATH, "w", encoding="utf-8", newline="") as f:
    f.write(src)
print("[done] timeline.rs updated")
