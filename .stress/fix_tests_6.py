import io

# ============ timeline.rs: 消除双重入账 ============
TL = r"D:\project\QAQ-Harness\crates\qaqh-runtime\src\timeline.rs"
with io.open(TL, "r", encoding="utf-8") as f:
    tl = f.read()

def sub(src, old, new, label):
    count = src.count(old)
    assert count == 1, f"[{label}] expected 1, found {count}"
    src = src.replace(old, new, 1)
    print(f"[ok] {label}")
    return src

# 入账只在 next_entry 单点做（previous 分散入账 x2 + next_entry = 双重计数）
tl = sub(tl,
"""        let delta = delta.into();
        timeline.journal_bytes += delta.len() as u64;
        let round = existing_round_mut(timeline, turn_id, round_num)?;""",
"""        let delta = delta.into();
        let round = existing_round_mut(timeline, turn_id, round_num)?;""",
"remove dup: append_text")

tl = sub(tl,
"""        let text = text.into();
        block.text = text.clone();
        timeline.journal_bytes += text.len() as u64;
        Ok(next_entry(""",
"""        let text = text.into();
        block.text = text.clone();
        Ok(next_entry(""",
"remove dup: checkpoint_block")

tl = sub(tl,
"""        tool.progress.push_str(&chunk);
        timeline.journal_bytes += chunk.len() as u64;
        Ok(next_entry(""",
"""        tool.progress.push_str(&chunk);
        Ok(next_entry(""",
"remove dup: append_tool_progress")

with io.open(TL, "w", encoding="utf-8", newline="") as f:
    f.write(tl)

# ============ hub.rs: terminal 测试改写（TurnSealed 同步 + journal 空） ============
HUB = r"D:\project\QAQ-Harness\crates\qaqh-runtime\src\ringing\hub.rs"
with io.open(HUB, "r", encoding="utf-8") as f:
    hub = f.read()

hub = sub(hub,
"""        hub.publish_timeline(
            "s",
            TimelineIntent::BlockSealed {
                turn_id: "t".into(),
                round_num: 0,
                block_id: "text".into(),
            },
        )
        .unwrap();

        let persisted = TimelineStore::new(&root)
            .unwrap()
            .load_seed("s")
            .expect("terminal snapshot persisted synchronously");
        assert_eq!(persisted.snapshot.watermark, 4);
        assert_eq!(
            persisted.snapshot.turns[0].rounds[0].blocks[0].text,
            "hello"
        );
        assert_eq!(
            persisted.snapshot.turns[0].rounds[0].blocks[0].state,
            qaqh_domain::TimelineBlockState::Sealed
        );
        drop(hub);
        let _ = std::fs::remove_dir_all(root);
    }""",
"""        hub.publish_timeline(
            "s",
            TimelineIntent::BlockSealed {
                turn_id: "t".into(),
                round_num: 0,
                block_id: "text".into(),
            },
        )
        .unwrap();
        // BlockSealed 已降级为异步 checkpoint（终端事件写放大收口）。
        // TurnSealed 保持同步落盘：publish 返回即 load_seed 可见。
        hub.publish_timeline(
            "s",
            TimelineIntent::TurnSealed {
                turn_id: "t".into(),
                state: qaqh_domain::TimelineTurnState::Completed,
                failure: None,
            },
        )
        .unwrap();

        let persisted = TimelineStore::new(&root)
            .unwrap()
            .load_seed("s")
            .expect("turn-sealed snapshot persisted synchronously");
        assert_eq!(persisted.snapshot.watermark, 5);
        assert_eq!(
            persisted.snapshot.turns[0].rounds[0].blocks[0].text,
            "hello"
        );
        assert_eq!(
            persisted.snapshot.turns[0].rounds[0].blocks[0].state,
            qaqh_domain::TimelineBlockState::Sealed
        );
        assert!(persisted.snapshot.turns[0].sealed);
        assert!(
            persisted.journal.is_empty(),
            "sealed turn leaves no replay tail on disk"
        );
        drop(hub);
        let _ = std::fs::remove_dir_all(root);
    }""",
"terminal test: TurnSealed boundary")

with io.open(HUB, "w", encoding="utf-8", newline="") as f:
    f.write(hub)
print("[done] final semantic fixes")
