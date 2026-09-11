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

# ============ hub.rs: 两个测试改写 ============
HUB = r"D:\project\QAQ-Harness\crates\qaqh-runtime\src\ringing\hub.rs"
patch(HUB, [
# --- terminal_timeline_intent: BlockSealed 不再同步，改验证 persist 前的同步写 ----
(
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
        assert_eq!(persisted.snapshot.watermark, 4);""",
"""        hub.publish_timeline(
            "s",
            TimelineIntent::BlockSealed {
                turn_id: "t".into(),
                round_num: 0,
                block_id: "text".into(),
            },
        )
        .unwrap();
        // BlockSealed 已降级为异步 checkpoint（写放大收口）：同步落盘契约
        // 只对 TurnSealed 成立。用 TextDelta（非终端）验证异步路径排队后，
        // 由 TurnSealed 同步写覆盖到最新水位。
        hub.publish_timeline(
            "s",
            TimelineIntent::TextDelta {
                turn_id: "t".into(),
                round_num: 0,
                block_id: "text".into(),
                delta: "more".into(),
            },
        )
        .unwrap();
        hub.publish_timeline(
            "s",
            TimelineIntent::RoundSealed {
                turn_id: "t".into(),
                round_num: 0,
                is_final: true,
            },
        )
        .unwrap();
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
        assert_eq!(persisted.snapshot.watermark, 7);""",
"terminal test: TurnSealed boundary"),
# 断言补齐 sealed 状态
(
"""        assert_eq!(
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
"""        assert_eq!(
            persisted.snapshot.turns[0].rounds[0].blocks[0].text,
            "hellomore"
        );
        assert_eq!(
            persisted.snapshot.turns[0].rounds[0].blocks[0].state,
            qaqh_domain::TimelineBlockState::Sealed
        );
        assert!(persisted.snapshot.turns[0].sealed, "turn sealed synchronously");
        drop(hub);
        let _ = std::fs::remove_dir_all(root);
    }""",
"terminal test: text+turn assertions"),

# --- persisted_native_timeline_recovers: 断言改为新契约 ----
(
"""        // 恢复时遗留的 running turn 必须收尾为 Cancelled（孤儿 turn seal
        // 契约），否则前端会永远把它投影为 running 并禁止发送新消息。
        assert_eq!(snapshot.watermark, 6);
        assert_eq!(snapshot.turns[0].rounds[0].blocks[0].text, "hello");
        assert_eq!(
            snapshot.turns[0].state,
            TimelineTurnState::Cancelled
        );
        assert!(snapshot.turns[0].rounds[0].sealed);
        assert_eq!(
            snapshot.turns[0].rounds[0].blocks[0].state,
            TimelineBlockState::Sealed
        );
        assert_eq!(hub.timeline_replay_since("s", 1).len(), 5);""",
"""        // 恢复时遗留的 running turn 必须收尾为 Cancelled（孤儿 turn seal
        // 契约），否则前端会永远把它投影为 running 并禁止发送新消息。
        // seal 裁剪后 replay 尾只含本次 seal 产生的 TurnSealed 条目（1 条）。
        assert_eq!(snapshot.watermark, 6);
        assert_eq!(snapshot.turns[0].rounds[0].blocks[0].text, "hello");
        assert_eq!(
            snapshot.turns[0].state,
            TimelineTurnState::Cancelled
        );
        assert!(snapshot.turns[0].rounds[0].sealed);
        assert_eq!(
            snapshot.turns[0].rounds[0].blocks[0].state,
            TimelineBlockState::Sealed
        );
        let replayed = hub.timeline_replay_since("s", 1);
        assert_eq!(
            replayed.len(),
            1,
            "sealed turn leaves only its own TurnSealed entry in the tail"
        );
        assert!(matches!(
            replayed[0].event,
            qaqh_domain::TimelineEvent::TurnSealed { .. }
        ));""",
"persist test: new replay contract"),
])
print("[done] hub fixes")
