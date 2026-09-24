//! Timeline 投影重建（BUG-006）。
//!
//! timeline 是前端 transcript 的权威读侧，但 `messages.jsonl` 才是消息写侧
//! 事实。timeline 文件缺失或损坏时，这里从归档重建一条全 sealed 的 timeline，
//! 使 timeline 恢复“可重建投影”的地位；压缩摘要作为归档行存在，但读取时
//! 显式跳过，不能污染人类 transcript。

use qaqh_domain::{
    TimelineBlockKind, TimelineFailure, TimelineIntent, TimelineSnapshot, TimelineTool,
    TimelineToolState, TimelineTurnState,
};
use qaqh_session::SessionManager;

/// 重建只覆盖前端 transcript 恢复窗口（最近 N 轮），而非全历史。与
/// `handle_timeline_snapshot` 的分页语义对齐：客户端翻旧页会触发新的
/// 快照拉取，不需要重建时物化全量。
const REBUILD_RECENT_TURNS: usize = 40;

/// 从持久化 session 消息重建 timeline 快照 + replay journal。
///
/// Phase 2（有界恢复）：本函数是 timeline 文件缺失/损坏时的降级路径，
/// 只服务于前端 transcript 恢复窗口，因此改走 **尾部有界读取**（反向扫描）：
/// GB 级归档的重建从 O(文件) 降到 O(尾部)，不再全量 `read_to_string` +
/// 全量反序列化。模型循环的完整历史装载（`load_for_resume`）不受影响。
///
/// **读的是归档，不是 compact 视图**（`load_archive_tail`）：timeline 是**人类
/// transcript**，而 compact 视图是**模型可见面**——后者刻意 shadow 掉被替换的区间，
/// 拿它当 transcript 的来源会把用户已经看过的对话抹掉。deepseek-harness 把这条写成
/// 了设计条款（`surface.ts`：「the model-visible surface … is the wrong source for a
/// human transcript」），codex 与 grok-build 同款（后者干脆把 `chat_history.jsonl`
/// 与 append-only 的 `updates.jsonl` 分成两个文件）。摘要现在是归档里的真实行，
/// 但 `load_archive_tail` 会过滤 `[Compacted N turns]`，不会把它当成真实回合显示。
pub fn rebuild_timeline_snapshot(
    sessions: Option<&SessionManager>,
    seed: &str,
) -> Option<(TimelineSnapshot, Vec<qaqh_domain::TimelineEntry>)> {
    let manager = sessions?;
    // 200 条消息 ≈ 多轮对话（含工具往返），足够投影出 ~40 turn 窗口。
    let messages = manager.load_archive_tail(seed, 200)?;
    if messages.is_empty() {
        return None;
    }
    let (_, turns) = super::projection::project_recent_turns_from_messages(
        seed,
        &messages,
        REBUILD_RECENT_TURNS,
    );
    // 这个窗口是会话的**最后** N 轮，故全局基址 = 真实回合总数 - 窗口大小。
    // `meta.turn_count` 是权威的真实回合数（timeline_api 的 total_turns 同源）。
    let total = manager.load_meta(seed)?.turn_count;
    let base = total.saturating_sub(turns.len());
    timeline_snapshot_from_turns(seed, &turns, base)
}

/// 把已经投影好的 `TurnData` 序列重放进 [`TimelineAppender`]，得到与原生
/// 写入路径同构的 snapshot + replay journal。
///
/// 重放使用 `BlockCheckpoint` 写入整段文本（不做逐 token delta），因此重建
/// 结果的每个 block 都是最终全文且 sealed；turn 终态统一标记为 `Completed`
/// （messages.jsonl 不持久化 turn 终态，这是恢复时的可接受降级）。
/// `base_index` = 这批 `TurnData` 里**第一个回合在会话内的全局序号**（0-based）。
/// turn_id 由它派生：`t{base_index + i + 1}`。
///
/// **为什么 id 必须全局化**：`projection::build_turns` 合成的 id 是「**已加载消息池**
/// 内的下标」（`projection.rs:554`），而池的大小取决于本次读了多少归档尾部——
/// 同一个回合在「重建只读 200 条消息」时叫 `t1`、在「深翻页读 2000 条」时叫 `t161`。
/// 客户端是**按 turn_id 拼页**的（`prepend_older` 用边界回合的 id 去重），
/// 池内下标会让相邻两页对不上：轻则重复一条，重则错位拼到别的回合上。
/// 派生自全局序号后，任何读取窗口对同一回合都给出同一个 id。
///
/// 传入的 `turn.turn_id` 因此被**忽略**：它携带的信息量为零（永远是池内下标）。
pub fn timeline_snapshot_from_turns(
    seed: &str,
    turns: &[qaqh_domain::TurnData],
    base_index: usize,
) -> Option<(TimelineSnapshot, Vec<qaqh_domain::TimelineEntry>)> {
    if turns.is_empty() {
        return None;
    }

    let mut appender = crate::timeline::TimelineAppender::new();
    for (turn_index, turn) in turns.iter().enumerate() {
        let turn_id = format!("t{}", base_index + turn_index + 1);
        if let Err(error) = appender.apply_intent(
            seed,
            TimelineIntent::TurnOpened {
                turn_id: turn_id.clone(),
                user_text: turn.user_text.clone(),
            },
        ) {
            log::warn!("[timeline-rebuild] open turn {turn_id} failed: {error}");
            return None;
        }

        for (round_index, round) in turn.rounds.iter().enumerate() {
            let mut stream_segment = 0u32;
            let mut round_has_blocks = false;

            for block in &round.blocks {
                match block {
                    qaqh_domain::RoundBlock::Reasoning { content } => {
                        if content.is_empty() {
                            continue;
                        }
                        let block_id =
                            format!("round-{}:reasoning:{stream_segment}", round.round_num);
                        stream_segment = stream_segment.saturating_add(1);
                        rebuild_text_block(
                            &mut appender,
                            seed,
                            &turn_id,
                            round.round_num,
                            &block_id,
                            TimelineBlockKind::Reasoning,
                            content,
                        )?;
                        round_has_blocks = true;
                    }
                    qaqh_domain::RoundBlock::Text { content } => {
                        if content.is_empty() {
                            continue;
                        }
                        let block_id = format!("round-{}:text:{stream_segment}", round.round_num);
                        stream_segment = stream_segment.saturating_add(1);
                        rebuild_text_block(
                            &mut appender,
                            seed,
                            &turn_id,
                            round.round_num,
                            &block_id,
                            TimelineBlockKind::Text,
                            content,
                        )?;
                        round_has_blocks = true;
                    }
                    qaqh_domain::RoundBlock::Tool { card } => {
                        let block_id = format!("tool:{}", card.id);
                        let tool = rebuild_tool(card, &round.tool_results);
                        if let Err(error) = appender.apply_intent(
                            seed,
                            TimelineIntent::BlockOpened {
                                turn_id: turn_id.clone(),
                                round_num: round.round_num,
                                block_id: block_id.clone(),
                                kind: TimelineBlockKind::Tool,
                                tool: Some(tool.clone()),
                            },
                        ) {
                            log::warn!(
                                "[timeline-rebuild] open tool block {block_id} failed: {error}"
                            );
                            return None;
                        }
                        if let Err(error) = appender.apply_intent(
                            seed,
                            TimelineIntent::ToolUpdated {
                                turn_id: turn_id.clone(),
                                round_num: round.round_num,
                                block_id: block_id.clone(),
                                tool,
                            },
                        ) {
                            log::warn!(
                                "[timeline-rebuild] update tool block {block_id} failed: {error}"
                            );
                            return None;
                        }
                        if let Err(error) = appender.apply_intent(
                            seed,
                            TimelineIntent::BlockSealed {
                                turn_id: turn_id.clone(),
                                round_num: round.round_num,
                                block_id,
                            },
                        ) {
                            log::warn!("[timeline-rebuild] seal tool block failed: {error}");
                            return None;
                        }
                        round_has_blocks = true;
                    }
                    // Responses API 的内置 web search 没有原生 timeline block；
                    // 重建为文本记录行，保证它不会破坏后续 round 的轮次序号。
                    qaqh_domain::RoundBlock::WebSearch { action } => {
                        let content = serde_json::to_string(action)
                            .map(|action| format!("web_search: {action}"))
                            .unwrap_or_else(|_| "web_search".to_string());
                        let block_id = format!("round-{}:text:{stream_segment}", round.round_num);
                        stream_segment = stream_segment.saturating_add(1);
                        rebuild_text_block(
                            &mut appender,
                            seed,
                            &turn_id,
                            round.round_num,
                            &block_id,
                            TimelineBlockKind::Text,
                            &content,
                        )?;
                        round_has_blocks = true;
                    }
                }
            }

            // 旧投影可能没有 `blocks` 但有 thinking/answer 字段：兜底补两个
            // 文本块，保证老会话至少有可见内容。
            if !round_has_blocks {
                if let Some(thinking) = round.thinking.as_deref().filter(|text| !text.is_empty()) {
                    let block_id = format!("round-{}:reasoning:{stream_segment}", round.round_num);
                    stream_segment = stream_segment.saturating_add(1);
                    rebuild_text_block(
                        &mut appender,
                        seed,
                        &turn_id,
                        round.round_num,
                        &block_id,
                        TimelineBlockKind::Reasoning,
                        thinking,
                    )?;
                    round_has_blocks = true;
                }
                if let Some(answer) = round.answer.as_deref().filter(|text| !text.is_empty()) {
                    let block_id = format!("round-{}:text:{stream_segment}", round.round_num);
                    rebuild_text_block(
                        &mut appender,
                        seed,
                        &turn_id,
                        round.round_num,
                        &block_id,
                        TimelineBlockKind::Text,
                        answer,
                    )?;
                    round_has_blocks = true;
                }
            }

            if round_has_blocks {
                let is_final = round.is_final || round_index + 1 == turn.rounds.len();
                if let Err(error) = appender.apply_intent(
                    seed,
                    TimelineIntent::RoundSealed {
                        turn_id: turn_id.clone(),
                        round_num: round.round_num,
                        is_final,
                    },
                ) {
                    log::warn!("[timeline-rebuild] seal round failed: {error}");
                    return None;
                }
            }
        }

        if let Err(error) = appender.apply_intent(
            seed,
            TimelineIntent::TurnSealed {
                turn_id: turn_id.clone(),
                state: TimelineTurnState::Completed,
                failure: None,
            },
        ) {
            log::warn!("[timeline-rebuild] seal turn {turn_id} failed: {error}");
            return None;
        }
    }

    let snapshot = appender.snapshot(seed)?;
    let journal = appender.replay_since(seed, 0);
    Some((snapshot, journal))
}

fn rebuild_text_block(
    appender: &mut crate::timeline::TimelineAppender,
    seed: &str,
    turn_id: &str,
    round_num: u32,
    block_id: &str,
    kind: TimelineBlockKind,
    text: &str,
) -> Option<()> {
    if let Err(error) = appender.apply_intent(
        seed,
        TimelineIntent::BlockOpened {
            turn_id: turn_id.to_string(),
            round_num,
            block_id: block_id.to_string(),
            kind,
            tool: None,
        },
    ) {
        log::warn!("[timeline-rebuild] open {kind:?} block failed: {error}");
        return None;
    }
    if let Err(error) = appender.apply_intent(
        seed,
        TimelineIntent::BlockCheckpoint {
            turn_id: turn_id.to_string(),
            round_num,
            block_id: block_id.to_string(),
            text: text.to_string(),
        },
    ) {
        log::warn!("[timeline-rebuild] checkpoint {kind:?} block failed: {error}");
        return None;
    }
    if let Err(error) = appender.apply_intent(
        seed,
        TimelineIntent::BlockSealed {
            turn_id: turn_id.to_string(),
            round_num,
            block_id: block_id.to_string(),
        },
    ) {
        log::warn!("[timeline-rebuild] seal {kind:?} block failed: {error}");
        return None;
    }
    Some(())
}

fn rebuild_tool(
    card: &qaqh_domain::ToolCallDef,
    results: &[qaqh_domain::ToolResultDef],
) -> TimelineTool {
    let result = results.iter().find(|result| result.tool_call_id == card.id);
    let success = result.is_some_and(|result| result.success);
    // 优先用归档里的五态 status（新 journal）；历史记录无 status 时按
    // success 布尔二值回退，保持向后兼容。
    let state = match result.and_then(|result| result.status) {
        Some(status) => TimelineToolState::from(status),
        None if success => TimelineToolState::Succeeded,
        None => TimelineToolState::Failed,
    };
    let failure = match result.and_then(|result| result.status) {
        Some(status) => status.is_failure(),
        None => !success,
    }
    .then(|| TimelineFailure {
        code: "tool_execution_failed".into(),
        message: result
            .map(|result| result.output.clone())
            .filter(|output| !output.is_empty())
            .unwrap_or_else(|| "tool result missing in archived messages".into()),
    });
    // 展示投影由工具作者声明；重建路径用归档 args 重新调用同一投影函数，
    // 保证 live 与 rebuild 两条路径产出同形 display（契约 §7.1）。
    let mut display = result.and_then(|result| {
        result
            .display
            .as_ref()
            .map(qaqh_workspace::runtime::project_tool_display_from_wire)
            .or_else(|| {
                let args = serde_json::from_str::<serde_json::Value>(&card.args_json).ok()?;
                qaqh_workspace::runtime::project_tool_display(&card.name, &args, &result.output)
            })
    });
    if let (Some(display), Some(result)) = (display.as_mut(), result) {
        crate::timeline::apply_result_metrics(display, &result.metrics);
    }
    let summary = Some(crate::timeline::project_tool_summary(
        &card.name,
        state,
        display
            .as_ref()
            .and_then(|display| display.summary.as_deref()),
        result.map(|result| result.output.as_str()),
    ));
    TimelineTool {
        tool_call_id: card.id.clone(),
        name: card.name.clone(),
        state,
        summary,
        args_json: Some(card.args_json.clone()),
        output: result.map(|result| result.output.clone()),
        diff: None,
        progress: String::new(),
        progress_truncated: false,
        progress_stream: None,
        progress_bytes_total: 0,
        display: display.as_ref().map(crate::timeline::wire_display),
        failure,
        permission: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_domain::{RoundBlock, RoundData, ToolCallDef, ToolResultDef, TurnData};

    #[test]
    fn rebuild_prefers_canonical_display_payload_over_text_projection() {
        let card = ToolCallDef {
            id: "call-1".into(),
            name: "read".into(),
            args_display: "read".into(),
            args_json: "{}".into(),
        };
        let result = ToolResultDef {
            tool_call_id: "call-1".into(),
            output: "not-json legacy text".into(),
            success: true,
            status: Some(qaqh_types::ToolStatus::Ok),
            file: None,
            metrics: Default::default(),
            display: Some(qaqh_types::ToolResultDisplay {
                summary: Some("canonical summary".into()),
                diff: None,
                header: Some(qaqh_types::ToolResultDisplayHeader::Other {
                    label: "canonical".into(),
                }),
                body: Some(qaqh_types::ToolResultDisplayBody::Streams {
                    stdout: "canonical stdout".into(),
                    stderr: "canonical stderr".into(),
                    exit_code: Some(0),
                    truncated: false,
                    interleaved: false,
                }),
                outcome: Some(qaqh_types::ToolResultDisplayOutcome {
                    state: qaqh_types::ToolResultDisplayOutcomeState::Succeeded,
                    exit_code: Some(0),
                    duration_ms: Some(9),
                    output_bytes: Some(14),
                    truncated: Some(false),
                }),
            }),
        };

        let tool = rebuild_tool(&card, &[result]);

        assert_eq!(tool.summary.as_deref(), Some("canonical summary"));
        let display = tool.display.expect("canonical display");
        assert_eq!(display.summary.as_deref(), Some("canonical summary"));
        assert!(matches!(
            display.header,
            Some(qaqh_domain::TimelineToolHeader::Other { .. })
        ));
        assert!(matches!(
            display.body,
            Some(qaqh_domain::TimelineToolBody::Streams {
                ref stdout,
                ref stderr,
                ..
            }) if stdout == "canonical stdout" && stderr == "canonical stderr"
        ));
        let outcome = display
            .outcome
            .expect("snapshot preserves structured outcome");
        assert_eq!(
            outcome.state,
            qaqh_types::ToolResultDisplayOutcomeState::Succeeded
        );
        assert_eq!(outcome.exit_code, Some(0));
    }

    fn turn_with_blocks() -> TurnData {
        TurnData {
            turn_id: "t1".into(),
            user_text: "question".into(),
            rounds: vec![RoundData {
                round_num: 0,
                is_final: true,
                thinking: None,
                answer: None,
                tool_calls: vec![ToolCallDef {
                    id: "call-1".into(),
                    name: "read".into(),
                    args_display: "read".into(),
                    args_json: "{\"path\":\"/tmp/a\"}".into(),
                }],
                tool_results: vec![ToolResultDef {
                    tool_call_id: "call-1".into(),
                    output: "line one\nline two".into(),
                    success: true,
                    status: None,
                    file: None,
                    metrics: Default::default(),
                    display: None,
                }],
                blocks: vec![
                    RoundBlock::Reasoning {
                        content: "thinking".into(),
                    },
                    RoundBlock::Tool {
                        card: ToolCallDef {
                            id: "call-1".into(),
                            name: "read".into(),
                            args_display: "read".into(),
                            args_json: "{\"path\":\"/tmp/a\"}".into(),
                        },
                    },
                    RoundBlock::Text {
                        content: "answer".into(),
                    },
                ],
            }],
        }
    }

    #[test]
    fn rebuild_preserves_round_blocks_and_tool_terminal_state() {
        let (snapshot, journal) = timeline_snapshot_from_turns("seed", &[turn_with_blocks()], 0)
            .expect("snapshot rebuilt");
        assert_eq!(snapshot.turns.len(), 1);
        assert!(snapshot.watermark > 0);
        // #314（2026-09-23 契约变更）：seal **不再**裁剪回放尾，重建出的已 seal
        // 历史 turn 的条目同样保留在 journal 里（上界由双限驱逐控制）。
        // watermark 由快照独立持有，条目数断言不再适用。
        assert!(
            !journal.is_empty(),
            "rebuilt sealed turns keep their replay tail (#314)"
        );
        assert!(
            matches!(
                journal.last().map(|entry| &entry.event),
                Some(qaqh_domain::TimelineEvent::TurnSealed { .. })
            ),
            "回放尾必须收在 TurnSealed 上"
        );
        assert!(
            snapshot.watermark >= 12,
            "watermark counts all rebuilt entries"
        );

        let rebuilt = &snapshot.turns[0];
        assert_eq!(rebuilt.turn_id, "t1");
        assert_eq!(rebuilt.user_text, "question");
        assert!(rebuilt.sealed);
        assert_eq!(rebuilt.state, qaqh_domain::TimelineTurnState::Completed);

        let round = &rebuilt.rounds[0];
        assert!(round.sealed);
        assert!(round.is_final);
        assert_eq!(round.blocks.len(), 3);
        assert_eq!(
            round.blocks[0].kind,
            qaqh_domain::TimelineBlockKind::Reasoning
        );
        assert_eq!(round.blocks[0].text, "thinking");
        assert_eq!(
            round.blocks[0].state,
            qaqh_domain::TimelineBlockState::Sealed
        );

        assert_eq!(round.blocks[1].kind, qaqh_domain::TimelineBlockKind::Tool);
        let tool = round.blocks[1].tool.as_ref().expect("tool block");
        assert_eq!(tool.tool_call_id, "call-1");
        assert_eq!(tool.name, "read");
        assert_eq!(tool.state, qaqh_domain::TimelineToolState::Succeeded);
        assert_eq!(tool.output.as_deref(), Some("line one\nline two"));
        assert_eq!(tool.summary.as_deref(), Some("line one"));
        assert!(tool.args_json.is_some());
        assert!(tool.failure.is_none());

        assert_eq!(round.blocks[2].kind, qaqh_domain::TimelineBlockKind::Text);
        assert_eq!(round.blocks[2].text, "answer");
    }

    #[test]
    fn rebuild_marks_failed_tool_as_failed() {
        let mut turn = turn_with_blocks();
        turn.rounds[0].blocks = vec![RoundBlock::Tool {
            card: ToolCallDef {
                id: "call-1".into(),
                name: "bash".into(),
                args_display: "bash".into(),
                args_json: "{}".into(),
            },
        }];
        turn.rounds[0].tool_calls = vec![ToolCallDef {
            id: "call-1".into(),
            name: "bash".into(),
            args_display: "bash".into(),
            args_json: "{}".into(),
        }];
        turn.rounds[0].tool_results = vec![ToolResultDef {
            tool_call_id: "call-1".into(),
            output: "boom".into(),
            success: false,
            status: None,
            file: None,
            metrics: Default::default(),
            display: None,
        }];

        let (snapshot, _) =
            timeline_snapshot_from_turns("seed", &[turn], 0).expect("snapshot rebuilt");
        let tool = snapshot.turns[0].rounds[0].blocks[0]
            .tool
            .as_ref()
            .expect("tool block");
        assert_eq!(tool.state, qaqh_domain::TimelineToolState::Failed);
        assert_eq!(tool.failure.as_ref().expect("failure").message, "boom");
    }

    #[test]
    fn rebuild_maps_archived_cancelled_status_to_cancelled_state() {
        // 新 journal 携带五态 status：cancelled 不再折叠进 failed。
        let mut turn = turn_with_blocks();
        turn.rounds[0].blocks = vec![RoundBlock::Tool {
            card: ToolCallDef {
                id: "call-1".into(),
                name: "exec".into(),
                args_display: "exec".into(),
                args_json: "{}".into(),
            },
        }];
        turn.rounds[0].tool_calls = vec![ToolCallDef {
            id: "call-1".into(),
            name: "exec".into(),
            args_display: "exec".into(),
            args_json: "{}".into(),
        }];
        turn.rounds[0].tool_results = vec![ToolResultDef {
            tool_call_id: "call-1".into(),
            output: "cancelled by user".into(),
            success: false,
            status: Some(qaqh_types::ToolStatus::Cancelled),
            file: None,
            metrics: Default::default(),
            display: None,
        }];

        let (snapshot, _) =
            timeline_snapshot_from_turns("seed", &[turn], 0).expect("snapshot rebuilt");
        let tool = snapshot.turns[0].rounds[0].blocks[0]
            .tool
            .as_ref()
            .expect("tool block");
        assert_eq!(tool.state, qaqh_domain::TimelineToolState::Cancelled);
        assert_eq!(
            tool.failure.as_ref().expect("failure").message,
            "cancelled by user"
        );
    }

    #[test]
    fn rebuild_falls_back_to_legacy_thinking_and_answer_fields() {
        let turn = TurnData {
            turn_id: "t1".into(),
            user_text: "question".into(),
            rounds: vec![RoundData {
                round_num: 0,
                is_final: true,
                thinking: Some("thinking".into()),
                answer: Some("answer".into()),
                tool_calls: vec![],
                tool_results: vec![],
                blocks: vec![],
            }],
        };
        let (snapshot, _) =
            timeline_snapshot_from_turns("seed", &[turn], 0).expect("snapshot rebuilt");
        let blocks = &snapshot.turns[0].rounds[0].blocks;
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].text, "thinking");
        assert_eq!(blocks[1].text, "answer");
    }

    #[test]
    fn rebuild_returns_none_for_empty_history() {
        assert!(timeline_snapshot_from_turns("seed", &[], 0).is_none());
    }
}
