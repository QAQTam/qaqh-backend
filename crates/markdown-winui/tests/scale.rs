//! 规模粗测（`#[ignore]`，显式运行：`cargo test -p markdown-winui --test scale -- --ignored --nocapture`）。
//!
//! 验证两个命题（虚拟化决策的数据依据）：
//! 1. **追加成本与历史规模无关（O(1)）**：灌入 1k / 10k turn 后，
//!    追加单个 turn 的耗时基本不变——协议局域化 + append-only 的实证；
//! 2. **全量事件回放可线性扩展**：万级 turn 事件序列整体回放耗时线性。
//!
//! 注意：这是 debug 构建的粗测（非 benchmark），绝对值无意义，
//! 看的是**相对关系**（t_10k ≈ t_1k）。

use std::time::Instant;

use markdown_winui::{ConversationEvent, RenderCommand, RoundDeltaKind, Transcript};

/// 构造一个 turn 的完整事件序列（2 rounds × 若干 delta + 终态）。
fn turn_events(turn_id: &str) -> Vec<ConversationEvent> {
    let mut evs = Vec::new();
    evs.push(ConversationEvent::TurnStarted {
        turn_id: turn_id.into(),
        user_text: format!("question to {turn_id}"),
    });
    for round in 0..2u32 {
        for i in 0..8 {
            evs.push(ConversationEvent::RoundDelta {
                turn_id: turn_id.into(),
                round_num: round,
                kind: if i % 3 == 0 {
                    RoundDeltaKind::Thinking
                } else {
                    RoundDeltaKind::Answering
                },
                delta: format!("token {i} with **markdown** and `code` "),
            });
        }
        evs.push(ConversationEvent::RoundCompleted {
            turn_id: turn_id.into(),
            round_num: round,
            thinking: Some("reasoning".into()),
            answer: Some("final answer **bold** with ```rs\nfn main() {}\n```".into()),
            output_ref: None,
            is_final: round == 1,
        });
    }
    evs.push(ConversationEvent::TurnCompleted {
        turn_id: turn_id.into(),
    });
    evs
}

fn replay(n_turns: usize) -> (Transcript, u128) {
    let mut t = Transcript::new();
    let start = Instant::now();
    let mut cmd_count = 0usize;
    for i in 0..n_turns {
        for ev in turn_events(&format!("turn{i:08x}")) {
            cmd_count += t.apply(&ev).len();
        }
    }
    let elapsed = start.elapsed().as_millis();
    println!(
        "replay {n_turns} turns: {elapsed} ms total, {} ms/turn, {cmd_count} render commands",
        elapsed as f64 / n_turns as f64
    );
    (t, elapsed)
}

#[test]
#[ignore]
fn append_cost_is_independent_of_history_size() {
    // 灌入 1k turn，测追加单 turn 耗时
    let (mut t1k, _) = replay(1_000);
    let evs = turn_events("append_target");
    let start = Instant::now();
    for ev in &evs {
        t1k.apply(ev);
    }
    let t_1k = start.elapsed().as_micros();

    // 灌入 10k turn，测追加单 turn 耗时
    let (mut t10k, _) = replay(10_000);
    let start = Instant::now();
    for ev in &evs {
        t10k.apply(ev);
    }
    let t_10k = start.elapsed().as_micros();

    println!("append 1 turn after 1k history: {t_1k} µs");
    println!("append 1 turn after 10k history: {t_10k} µs");
    println!("ratio: {:.2}x", t_10k as f64 / t_1k.max(1) as f64);

    // O(1) 断言：历史规模 10 倍，追加成本不应同比例增长（允许 3x 抖动）
    assert!(
        t_10k <= t_1k.max(1) * 3,
        "追加成本随历史规模增长：t_1k={t_1k}µs t_10k={t_10k}µs"
    );

    // 追加只产生该 turn 的命令（局域化不随规模失效）
    let cmds = t10k.apply(&evs[0]);
    assert!(
        cmds.iter().all(|c| matches!(
            c,
            RenderCommand::MountTurn { .. }
                | RenderCommand::UpdateLiveTail { turn: 10000, .. }
                | RenderCommand::UpdateThinking { turn: 10000, .. }
                | RenderCommand::UpsertToolCard { turn: 10000, .. }
                | RenderCommand::RebuildRound { turn: 10000, .. }
                | RenderCommand::UpdateTurnStatus { index: 10000, .. }
        )),
        "追加命令必须只指向新 turn: {cmds:?}"
    );
}

#[test]
#[ignore]
fn replay_scales_linearly() {
    let (_, t_1k) = replay(1_000);
    let (_, t_5k) = replay(5_000);
    println!("5k/1k time ratio: {:.2}x (expect ≈5x linear)", t_5k as f64 / t_1k.max(1) as f64);
    assert!(
        t_5k <= t_1k.max(1) * 8,
        "回放应线性扩展（允许抖动）"
    );
}
