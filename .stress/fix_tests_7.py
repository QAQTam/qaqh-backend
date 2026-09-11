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

# 1. timeline.rs: watermark 断言语义修正（最新条目在 journal 内 → watermark == last_seq）
TL = r"D:\project\QAQ-Harness\crates\qaqh-runtime\src\timeline.rs"
patch(TL, [(
"""        let last_seq = bounded.last().unwrap().timeline_seq;
        assert_eq!(
            many.snapshot("s2").unwrap().watermark,
            last_seq + 1,
            "watermark must include the TurnOpened/BlockOpened offset"
        );""",
"""        let last_seq = bounded.last().unwrap().timeline_seq;
        // 最新条目仍在窗口内 → watermark 与其 seq 一致；
        // watermark 与窗口头部的差值 = 被驱逐的前缀长度。
        assert_eq!(
            many.snapshot("s2").unwrap().watermark,
            last_seq,
            "newest allocated entry must remain in the bounded tail"
        );""",
"watermark assertion fixed")])

# 2. hub.rs: TurnSealed 前补 RoundSealed，watermark 6
HUB = r"D:\project\QAQ-Harness\crates\qaqh-runtime\src\ringing\hub.rs"
patch(HUB, [(
"""        // BlockSealed 已降级为异步 checkpoint（终端事件写放大收口）。
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
        assert_eq!(persisted.snapshot.watermark, 5);""",
"""        // BlockSealed 已降级为异步 checkpoint（终端事件写放大收口）。
        // TurnSealed 保持同步落盘：publish 返回即 load_seed 可见。
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
        assert_eq!(persisted.snapshot.watermark, 6);""",
"hub test: RoundSealed added")])
print("[done] final two fixes")
