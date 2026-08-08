//! 流式输出效果模拟器（控制台版）：
//!
//! 模拟 `deepx-domain` 协议事件流（TurnStarted → RoundDelta×N →
//! RoundCompleted → …），驱动 `Transcript` 状态机，逐事件打印
//! 模型失效摘要与当前 transcript 的可视状态——直观展示：
//! - 未闭合语法（`**bo`）按字面输出，不破损
//! - 跨 delta 闭合（`**bold**`）后 live 预览升级
//! - RoundCompleted 权威终态重建（final 冻结）
//! - 工具调用流 upsert
//! - 多 round / 多 turn 局域化
//!
//! 运行：`cargo run -p markdown-winui --bin stream_demo`

use std::thread::sleep;
use std::time::Duration;

use markdown_core::ast::Inline;
use markdown_winui::{AnswerView, ConversationEvent, LiveSegment, RoundDeltaKind, Transcript};

/// 事件与等待间隔（模拟流式节奏）。
struct Ev(ConversationEvent, Duration);

fn d(id: &str, round: u32, kind: RoundDeltaKind, delta: &str) -> Ev {
    Ev(
        ConversationEvent::RoundDelta {
            turn_id: id.into(),
            round_num: round,
            kind,
            delta: delta.into(),
        },
        Duration::from_millis(60),
    )
}

/// 把一段文本切成字符级 delta（模拟 SSE/token 流）。
fn stream_text(turn: &str, round: u32, kind: RoundDeltaKind, text: &str, chunk: usize) -> Vec<Ev> {
    let mut out = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let take: String = rest.chars().take(chunk).collect();
        out.push(d(turn, round, kind, &take));
        rest = &rest[take.len()..];
    }
    out
}

fn main() {
    println!("╔══════════════════════════════════════════════════════════╗");
    println!("║  ChatView 流式输出模拟（协议驱动 RoundRenderer）           ║");
    println!("╚══════════════════════════════════════════════════════════╝\n");

    // ── 场景：一个 turn，两轮（round0 纯回答，round1 思考+工具+回答）──
    // 全部 token 级流（逐字/小批），贴近 SSE 传输节奏。
    let mut events: Vec<Ev> = vec![Ev(
        ConversationEvent::TurnStarted {
            turn_id: "t1".into(),
            user_text: "讲一下 Rust 所有权，并搜索相关文档".into(),
        },
        Duration::from_millis(300),
    )];
    events.extend(stream_text(
        "t1",
        0,
        RoundDeltaKind::Thinking,
        "用户需要所有权概览，可以顺带搜索官方文档。",
        3,
    ));
    events.extend(stream_text(
        "t1",
        0,
        RoundDeltaKind::ToolCalling,
        r#"{"id":"call_1","name":"web_search","arguments":"rust ownership"}"#,
        4,
    ));
    // round 0：逐字回答（未闭合 markdown 过程真实可见）
    events.extend(stream_text(
        "t1",
        0,
        RoundDeltaKind::Answering,
        "Rust 的所有权系统**很重要**，核心是三个规则：\n\n1. 每个值有且仅有一个所有者\n2. 所有权可以转移（move）\n3. 借用用 `&T` 表示\n\n更多见 [官方书](https://doc.rust-lang.org/book)",
        2,
    ));
    events.push(Ev(
        ConversationEvent::RoundCompleted {
            turn_id: "t1".into(),
            round_num: 0,
            thinking: None,
            answer: Some(
                "Rust 的所有权系统**很重要**，核心是三个规则：\n\n1. 每个值有且仅有一个所有者\n2. 所有权可以转移（move）\n3. 借用用 `&T` 表示\n\n更多见 [官方书](https://doc.rust-lang.org/book)".into(),
            ),
            output_ref: None,
            is_final: false,
        },
        Duration::from_millis(400),
    ));
    // round 1：继续逐字回答
    events.extend(stream_text(
        "t1",
        1,
        RoundDeltaKind::Answering,
        "编译期就能捕获悬垂引用与数据竞争，无需 GC。",
        2,
    ));
    events.push(Ev(
        ConversationEvent::RoundCompleted {
            turn_id: "t1".into(),
            round_num: 1,
            thinking: Some("用户需要所有权概览，可以顺带搜索官方文档。".into()),
            answer: Some("编译期就能捕获悬垂引用与数据竞争，无需 GC。".into()),
            output_ref: None,
            is_final: true,
        },
        Duration::from_millis(400),
    ));
    events.push(Ev(
        ConversationEvent::TurnCompleted {
            turn_id: "t1".into(),
        },
        Duration::from_millis(300),
    ));

    let mut t = Transcript::new();
    for Ev(ev, wait) in events {
        print_event(&ev);
        let change = t.apply(&ev);
        println!(
            "   ↳ model {:?}, extent_changed={}",
            change.invalidation, change.extent_changed
        );
        println!();
        render_transcript(&t);
        println!("──────────────────────────────────────────────────────────");
        sleep(wait);
    }

    println!("\n✓ 播放完毕。要点回顾：");
    println!("  • 全部 delta 为 token 级（2~4 字符/批），模拟 SSE 传输节奏");
    println!("  • round0 流式期间 `**很` 未闭合 → 字面输出，闭合后升级为加粗");
    println!("  • RoundCompleted 后 round 冻结（final），迟到 delta 不再改变模型");
    println!("  • 思考流 → Expander；工具流 → 卡片 upsert");
    println!("  • 前文不被重算；reactor 只 diff 变化 key（内容局域化）");
}

fn print_event(ev: &ConversationEvent) {
    match ev {
        ConversationEvent::TurnStarted { user_text, .. } => {
            println!("▶ TurnStarted  用户: {user_text}")
        }
        ConversationEvent::TurnCompleted { .. } => println!("▶ TurnCompleted"),
        ConversationEvent::TurnFailed { error, .. } => {
            println!("▶ TurnFailed   error: {error}")
        }
        ConversationEvent::Unknown => println!("▶ (忽略的领域事件)"),
        ConversationEvent::RoundDelta {
            kind,
            delta,
            round_num,
            ..
        } => println!(
            "▶ RoundDelta    round{round_num} [{kind:?}] +{}: {delta:?}",
            delta.chars().count()
        ),
        ConversationEvent::BlockCheckpoint {
            kind,
            text,
            round_num,
            ..
        } => println!("▶ Checkpoint    round{round_num} [{kind:?}] = {text:?}"),
        ConversationEvent::RoundCompleted {
            round_num,
            is_final,
            ..
        } => println!("▶ RoundCompleted round{round_num} (is_final={is_final})"),
        ConversationEvent::ProviderToolStatus {
            round_num,
            tool_kind,
            state,
            ..
        } => println!("▶ ProviderTool  round{round_num} [{tool_kind}] {state:?}"),
        ConversationEvent::ToolCallPrepared {
            tool_call_id,
            round_num,
            name,
            args_so_far,
            ..
        } => println!("▶ ToolPrepared round{round_num} [{name}] {tool_call_id}: {args_so_far:?}"),
        ConversationEvent::ToolStarted {
            tool_call_id,
            round_num,
            name,
            ..
        } => println!("▶ ToolStarted  round{round_num} [{name}] {tool_call_id}"),
        ConversationEvent::ToolFinished {
            tool_call_id,
            round_num,
            result,
            ..
        } => println!(
            "▶ ToolFinished round{round_num} {tool_call_id}: {:?}",
            result.get("summary").and_then(|s| s.as_str())
        ),
    }
}

/// 把当前 transcript 渲染为"可视文本"（模拟 XAML 树的内容视图）。
fn render_transcript(t: &Transcript) {
    for (ti, turn) in t.turns().iter().enumerate() {
        if ti > 0 {
            println!();
        }
        println!(
            "┌─ Turn #{ti} [{}] {}",
            turn_status(turn.status),
            turn.user_text
        );
        for round in &turn.rounds {
            // 思考区
            if let Some(thinking) = &round.thinking {
                println!("│   🧠 [思考] {}", one_line(thinking, 48));
            }
            // 工具卡
            for card in &round.tool_calls {
                let done = if card.done { "✓" } else { "…" };
                let name = card.name.as_deref().unwrap_or("<解析中>");
                println!("│   🛠 {done} {name}  {}", one_line(&card.args_display, 40));
            }
            // 答案区
            match &round.answer {
                AnswerView::Streaming {
                    segments, inlines, ..
                } => {
                    for seg in segments {
                        match seg {
                            LiveSegment::Text(t) => {
                                println!("│   ▸ [live] {}", styled_live(t, inlines))
                            }
                            LiveSegment::Table(td) => println!(
                                "│   ▸ [live-table] {} 列 × {} 行（流式中）",
                                td.headers.len(),
                                td.rows.len()
                            ),
                        }
                    }
                }
                AnswerView::Final { blocks, .. } => {
                    let text = blocks
                        .iter()
                        .map(markdown_core::ast::block_plain_text)
                        .collect::<Vec<_>>()
                        .join(" / ");
                    println!("│   ▸ [final] {}", one_line(&text, 96));
                }
            }
        }
        println!("└─");
    }
}

fn turn_status(s: markdown_winui::TurnStatus) -> &'static str {
    match s {
        markdown_winui::TurnStatus::Running => "⏳",
        markdown_winui::TurnStatus::Completed => "✅",
        markdown_winui::TurnStatus::Failed => "❌",
    }
}

/// live 行内预览的可视化：Text 原文；Bold/Italic 用 ** 包裹还原。
fn styled_live(raw: &str, inlines: &[Inline]) -> String {
    let _ = raw;
    let mut out = String::new();
    for inline in inlines {
        match inline {
            Inline::Text(s) => out.push_str(s),
            Inline::Bold(v) => {
                out.push_str("**");
                out.push_str(&markdown_core::ast::concat_inlines(v));
                out.push_str("**");
            }
            Inline::Italic(v) => {
                out.push('*');
                out.push_str(&markdown_core::ast::concat_inlines(v));
                out.push('*');
            }
            Inline::Strikethrough(v) => {
                out.push_str("~~");
                out.push_str(&markdown_core::ast::concat_inlines(v));
                out.push_str("~~");
            }
            Inline::Code(s) => {
                out.push('`');
                out.push_str(s);
                out.push('`');
            }
            Inline::Link { text, url } => {
                out.push('[');
                out.push_str(&markdown_core::ast::concat_inlines(text));
                out.push_str("](");
                out.push_str(url);
                out.push(')');
            }
            Inline::Math { source, display } => {
                let d = if *display { "$$" } else { "$" };
                out.push_str(d);
                out.push_str(source);
                out.push_str(d);
            }
            Inline::Image { alt, .. } => out.push_str(alt),
            Inline::SoftBreak => out.push(' '),
        }
    }
    one_line(&out, 88)
}

fn one_line(s: &str, max: usize) -> String {
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i >= max {
            out.push('…');
            break;
        }
        out.push(c);
    }
    out.replace('\n', " ")
}
