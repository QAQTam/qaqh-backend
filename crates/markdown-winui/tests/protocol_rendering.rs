//! 协议驱动渲染测试：喂入模拟的 `deepx-domain` 事件序列，断言
//! `Transcript` 产出的渲染命令——验证"事件 → XAML 节点"增量路径。
//!
//! 覆盖（对应设计讨论的验收点）：
//! 1. 流式增量：未闭合语法字面输出（跨 delta 边界闭合）
//! 2. checkpoint 自愈：覆盖语义
//! 3. RoundCompleted 权威终态：以权威 answer 重建（忽略流式差异）
//! 4. 内容局域化：前 round/turn 冻结，零命令
//! 5. 工具卡 upsert
//! 6. output_ref 外置正文加载路径

use markdown_core::ast::Inline;
use markdown_winui::{
    ConversationEvent, LiveSegment, RenderCommand, RoundDeltaKind, Transcript,
};

fn turn_started(id: &str) -> ConversationEvent {
    ConversationEvent::TurnStarted {
        turn_id: id.into(),
        user_text: format!("user asks {id}"),
    }
}

fn delta(id: &str, round: u32, kind: RoundDeltaKind, d: &str) -> ConversationEvent {
    ConversationEvent::RoundDelta {
        turn_id: id.into(),
        round_num: round,
        kind,
        delta: d.into(),
    }
}

fn checkpoint(id: &str, round: u32, kind: RoundDeltaKind, text: &str) -> ConversationEvent {
    ConversationEvent::BlockCheckpoint {
        turn_id: id.into(),
        round_num: round,
        kind,
        text: text.into(),
    }
}

fn completed(id: &str, round: u32, answer: &str) -> ConversationEvent {
    ConversationEvent::RoundCompleted {
        turn_id: id.into(),
        round_num: round,
        thinking: None,
        answer: Some(answer.into()),
        output_ref: None,
        is_final: true,
    }
}

/// 1a. 流式增量：未闭合 `**` 字面输出（REFERENCE §3 语义 1 在协议驱动下的形态）
#[test]
fn answer_delta_unclosed_is_literal() {
    let mut t = Transcript::new();
    t.apply(&turn_started("t1"));
    let cmds = t.apply(&delta("t1", 0, RoundDeltaKind::Answering, "see **bold"));
    assert_eq!(
        cmds,
        vec![RenderCommand::UpdateLiveTail {
            turn: 0,
            round: 0,
            inlines: vec![Inline::Text("see **bold".into())],
            raw: "see **bold".into(),
            segments: vec![LiveSegment::Text("see **bold".into())],
        }],
        "未闭合 ** 必须字面输出"
    );
}

/// 1b. 跨 delta 边界闭合：`**bo` + `ld**` → Bold
#[test]
fn answer_delta_closes_across_deltas() {
    let mut t = Transcript::new();
    t.apply(&turn_started("t1"));
    t.apply(&delta("t1", 0, RoundDeltaKind::Answering, "**bo"));
    let cmds = t.apply(&delta("t1", 0, RoundDeltaKind::Answering, "ld**"));
    let RenderCommand::UpdateLiveTail { inlines, .. } = &cmds[0] else {
        panic!("expect live tail: {cmds:?}");
    };
    assert!(inlines.contains(&Inline::Bold(vec![Inline::Text("bold".into())])));
}

/// 1c. 协议表格流式渐进（P0）：围栏+表头 → 表格网格出现；数据行逐行确认；
/// 残行实时显示在网格末行（逐字生长）；闭合后表格封存、字面不重复。
#[test]
fn answer_delta_table_grows_progressively() {
    use markdown_winui::LiveSegment;
    let mut t = Transcript::new();
    t.apply(&turn_started("t1"));
    // 围栏 + 表头：表格立即出现（表头网格），围栏前文本为 Text 段
    let cmds = t.apply(&delta("t1", 0, RoundDeltaKind::Answering, "速查：\n\n```table\n语言\t类型\n"));
    let RenderCommand::UpdateLiveTail { segments, .. } = &cmds[0] else {
        panic!("expect live tail: {cmds:?}");
    };
    assert_eq!(segments.len(), 2, "Text(前缀) + Table");
    assert_eq!(
        segments[0],
        LiveSegment::Text("速查：\n\n".into()),
        "表格前文本保留在字面"
    );
    let LiveSegment::Table(t0) = &segments[1] else {
        panic!("expect table segment");
    };
    assert_eq!(
        markdown_core::ast::concat_inlines(&t0.headers[0]),
        "语言",
        "表头确认即出表格"
    );
    assert!(t0.rows.is_empty());

    // 数据行确认：逐行追加
    let cmds = t.apply(&delta("t1", 0, RoundDeltaKind::Answering, "Rust\t静态\n"));
    let RenderCommand::UpdateLiveTail { segments, .. } = &cmds[0] else {
        panic!("expect live tail");
    };
    let LiveSegment::Table(t1) = &segments[1] else {
        panic!("expect table segment");
    };
    assert_eq!(t1.rows.len(), 1);

    // 残行：不确认，但实时显示在网格末行（打字机效果延续进表格）
    let cmds = t.apply(&delta("t1", 0, RoundDeltaKind::Answering, "Go\t静"));
    let RenderCommand::UpdateLiveTail { segments, .. } = &cmds[0] else {
        panic!("expect live tail");
    };
    assert_eq!(segments.len(), 2, "残行不产生额外 Text 段");
    let LiveSegment::Table(t2) = &segments[1] else {
        panic!("expect table segment");
    };
    assert_eq!(t2.rows.len(), 2, "残行在网格末行");
    assert_eq!(markdown_core::ast::concat_inlines(&t2.rows[1][0]), "Go");
    assert_eq!(markdown_core::ast::concat_inlines(&t2.rows[1][1]), "静");

    // 残行逐字生长
    let cmds = t.apply(&delta("t1", 0, RoundDeltaKind::Answering, "态"));
    let RenderCommand::UpdateLiveTail { segments, .. } = &cmds[0] else {
        panic!("expect live tail");
    };
    let LiveSegment::Table(t3) = &segments[1] else {
        panic!("expect table segment");
    };
    assert_eq!(markdown_core::ast::concat_inlines(&t3.rows[1][1]), "静态", "残行逐字生长");

    // 残行完成 + 围栏闭合：表格封存（sealed），字面不重复出现表格内容
    let cmds = t.apply(&delta("t1", 0, RoundDeltaKind::Answering, "\n```\n"));
    let RenderCommand::UpdateLiveTail { segments, raw, .. } = &cmds[0] else {
        panic!("expect live tail");
    };
    assert_eq!(segments.len(), 2, "闭合后仍是 Text + Table");
    let LiveSegment::Table(t4) = &segments[1] else {
        panic!("expect table segment");
    };
    assert_eq!(t4.rows.len(), 2, "sealed 表格完整");
    assert!(
        !raw.contains("Rust\t静态"),
        "表格内容不回到字面（不重复显示）：{raw:?}"
    );
}

/// 1d. 表格坏格式回退：```table 后无分隔符 → 整段恢复字面，内容不丢
#[test]
fn answer_delta_table_falls_back_on_bad_header() {
    use markdown_winui::LiveSegment;
    let mut t = Transcript::new();
    t.apply(&turn_started("t1"));
    let cmds = t.apply(&delta("t1", 0, RoundDeltaKind::Answering, "```table\n这不是表格\n"));
    let RenderCommand::UpdateLiveTail { segments, .. } = &cmds[0] else {
        panic!("expect live tail: {cmds:?}");
    };
    assert_eq!(
        *segments,
        vec![LiveSegment::Text("```table\n这不是表格\n".into())],
        "无分隔符 → 围栏与内容全部保留字面"
    );
}

/// 2. checkpoint 自愈：乱序/丢 delta 后完整值覆盖，重解析正确
#[test]
fn checkpoint_overrides_and_self_heals() {
    let mut t = Transcript::new();
    t.apply(&turn_started("t1"));
    t.apply(&delta("t1", 0, RoundDeltaKind::Answering, "hel"));
    // 假设 delta 丢失，checkpoint 给出权威完整值
    let cmds = t.apply(&checkpoint("t1", 0, RoundDeltaKind::Answering, "hello **world**"));
    let RenderCommand::UpdateLiveTail { raw, inlines, .. } = &cmds[0] else {
        panic!("expect live tail");
    };
    assert_eq!(raw, "hello **world**");
    assert!(inlines.contains(&Inline::Bold(vec![Inline::Text("world".into())])));
}

/// 3. RoundCompleted 权威终态：流式累积与权威不一致时以权威为准
#[test]
fn round_completed_rebuilds_authoritative() {
    let mut t = Transcript::new();
    t.apply(&turn_started("t1"));
    // 流式期间看到未闭合（provider 修正前）
    t.apply(&delta("t1", 0, RoundDeltaKind::Answering, "hello **world"));
    let cmds = t.apply(&completed("t1", 0, "hello **world**"));
    let RenderCommand::RebuildRound { rich, .. } = &cmds[0] else {
        panic!("expect rebuild: {cmds:?}");
    };
    // 权威值含已闭合加粗
    let joined: String = rich.paragraphs[0]
        .inlines
        .iter()
        .map(|i| match i {
            windows_reactor::RichTextInline::Run(r) => r.text.clone(),
            windows_reactor::RichTextInline::Hyperlink(h) => h.text.clone(),
            windows_reactor::RichTextInline::LineBreak => "\n".into(),
        })
        .collect();
    assert!(joined.contains("bold") || joined.contains("world"), "{joined}");
    // final 后答案冻结为 Final
    assert!(matches!(
        t.turns()[0].rounds[0].answer,
        markdown_winui::AnswerView::Final { .. }
    ));
    // 终态后到达的 delta 被忽略（协议保证不会发生，防御性验证）
    let cmds = t.apply(&delta("t1", 0, RoundDeltaKind::Answering, "!!"));
    assert!(cmds.is_empty(), "终态后不得再产生 live 命令");
}

/// 4a. 内容局域化：round0 完成后，round1 的流只产生 round1 的命令
#[test]
fn content_locality_across_rounds() {
    let mut t = Transcript::new();
    t.apply(&turn_started("t1"));
    t.apply(&delta("t1", 0, RoundDeltaKind::Answering, "first"));
    t.apply(&completed("t1", 0, "first **done**"));

    let cmds = t.apply(&delta("t1", 1, RoundDeltaKind::Answering, "second"));
    assert_eq!(cmds.len(), 1);
    let RenderCommand::UpdateLiveTail { turn, round, .. } = &cmds[0] else {
        panic!("expect live tail");
    };
    assert_eq!((*turn, *round), (0, 1), "命令必须只指向 round1");
}

/// 4b. 多 turn append-only：turn2 事件不触碰 turn1（零命令泄漏）
#[test]
fn turns_are_append_only() {
    let mut t = Transcript::new();
    t.apply(&turn_started("t1"));
    t.apply(&delta("t1", 0, RoundDeltaKind::Answering, "a"));
    t.apply(&completed("t1", 0, "a"));

    t.apply(&turn_started("t2"));
    let cmds = t.apply(&delta("t2", 0, RoundDeltaKind::Answering, "b"));
    let RenderCommand::UpdateLiveTail { turn, .. } = &cmds[0] else {
        panic!("expect live tail");
    };
    assert_eq!(*turn, 1, "turn1 冻结，命令只能指向 turn2");
    assert_eq!(t.turn_count(), 2);
}

/// 5. 工具卡 upsert：ToolCalling 流累积 → 卡创建/更新（id 稳定）
#[test]
fn tool_card_upsert_by_id() {
    let mut t = Transcript::new();
    t.apply(&turn_started("t1"));
    let cmds = t.apply(&delta(
        "t1",
        0,
        RoundDeltaKind::ToolCalling,
        r#"{"id":"call_1","name":"read_fi"#,
    ));
    let RenderCommand::UpsertToolCard { card, .. } = &cmds[0] else {
        panic!("expect tool card");
    };
    assert_eq!(card.id, "call_1");
    assert_eq!(card.name.as_deref(), None, "名字未完整到达");

    let cmds = t.apply(&delta(
        "t1",
        0,
        RoundDeltaKind::ToolCalling,
        r#"le","arguments":"src/main.rs"}"#,
    ));
    let RenderCommand::UpsertToolCard { card, .. } = &cmds[0] else {
        panic!("expect tool card");
    };
    assert_eq!(card.id, "call_1");
    assert_eq!(card.name.as_deref(), Some("read_file"));

    // RoundCompleted 收尾：卡 done
    t.apply(&completed("t1", 0, "ok"));
    assert!(t.turns()[0].rounds[0].tool_calls[0].done);
}

/// 6. output_ref 外置正文：占位命令 → 拉取完成 → resolve_output 重建
#[test]
fn output_ref_load_path() {
    let mut t = Transcript::new();
    t.apply(&turn_started("t1"));
    let cmds = t.apply(&ConversationEvent::RoundCompleted {
        turn_id: "t1".into(),
        round_num: 0,
        thinking: None,
        answer: None, // 外置
        output_ref: Some(serde_json::json!("journal://big-round-1")),
        is_final: true,
    });
    let RenderCommand::LoadOutput { output_ref, .. } = &cmds[0] else {
        panic!("expect load output: {cmds:?}");
    };
    assert_eq!(output_ref, "journal://big-round-1");

    // 应用层拉取完成后重建
    let cmds = t.resolve_output("t1", 0, "big **content**");
    let RenderCommand::RebuildRound { rich, .. } = &cmds[0] else {
        panic!("expect rebuild");
    };
    assert!(!rich.paragraphs.is_empty());
}

/// 7. 多 round 混合序列（thinking → answering → tool）端到端冒烟
#[test]
fn mixed_round_smoke() {
    let mut t = Transcript::new();
    let mut total = 0;
    for ev in [
        turn_started("t1"),
        delta("t1", 0, RoundDeltaKind::Thinking, "think"),
        delta("t1", 0, RoundDeltaKind::Answering, "let me check **"),
        delta("t1", 0, RoundDeltaKind::ToolCalling, r#"{"id":"c1","name":"grep""#),
        completed("t1", 0, "done **here**"),
        delta("t1", 1, RoundDeltaKind::Answering, "follow up"),
        completed("t1", 1, "follow up"),
        ConversationEvent::TurnCompleted {
            turn_id: "t1".into(),
        },
    ] {
        total += t.apply(&ev).len();
    }
    assert!(total >= 6);
    assert_eq!(t.turns()[0].rounds.len(), 2);
    assert_eq!(
        t.turns()[0].status,
        markdown_winui::TurnStatus::Completed
    );
    // round0 权威终态冻结
    assert!(matches!(
        t.turns()[0].rounds[0].answer,
        markdown_winui::AnswerView::Final { .. }
    ));
}
