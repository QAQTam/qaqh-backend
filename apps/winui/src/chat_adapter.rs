//! Typed domain/timeline → ChatView presentation model.
//!
//! JSON is decoded by `deepx-client`; this adapter only performs an explicit
//! domain-to-view mapping. Compiler errors therefore expose protocol changes
//! before RC builds instead of silently routing them through `Unknown`.

use deepx_client::{
    ConversationEvent as DomainConversationEvent, RingingEvent, TimelineBlockKind,
    TimelineSnapshot, TimelineTool, TimelineToolState, TimelineTurnState, ToolEvent,
};
use markdown_winui::{
    ConversationEvent, ProviderToolState, RestoredRound, RestoredTurn, RoundDeltaKind,
    ToolCardView, TurnStatus,
};

/// Convert one canonical Ringing event into the subset used by the current
/// Transcript view model. Events unrelated to transcript presentation return
/// `None`; they remain available to their typed application stores.
pub fn render_event(event: &RingingEvent) -> Option<ConversationEvent> {
    match event {
        RingingEvent::Conversation(event) => match event {
            DomainConversationEvent::TurnStarted { turn_id, user_text } => {
                Some(ConversationEvent::TurnStarted {
                    turn_id: turn_id.clone(),
                    user_text: user_text.clone(),
                })
            }
            DomainConversationEvent::TurnCompleted { turn_id, .. } => {
                Some(ConversationEvent::TurnCompleted {
                    turn_id: turn_id.clone(),
                })
            }
            DomainConversationEvent::TurnFailed { turn_id, error } => {
                Some(ConversationEvent::TurnFailed {
                    turn_id: turn_id.clone(),
                    error: serde_json::to_value(error).unwrap_or_default(),
                })
            }
            DomainConversationEvent::RoundDelta {
                turn_id,
                round_num,
                kind,
                delta,
            } => Some(ConversationEvent::RoundDelta {
                turn_id: turn_id.clone(),
                round_num: *round_num,
                kind: map_delta_kind(*kind),
                delta: delta.clone(),
            }),
            DomainConversationEvent::BlockCheckpoint {
                turn_id,
                round_num,
                kind,
                text,
                ..
            } => Some(ConversationEvent::BlockCheckpoint {
                turn_id: turn_id.clone(),
                round_num: *round_num,
                kind: map_delta_kind(*kind),
                text: text.clone(),
            }),
            DomainConversationEvent::ProviderToolStatus {
                turn_id,
                round_num,
                call_id,
                tool_kind,
                state,
            } => Some(ConversationEvent::ProviderToolStatus {
                turn_id: turn_id.clone(),
                round_num: *round_num,
                call_id: call_id.clone(),
                tool_kind: tool_kind.clone(),
                state: match state {
                    deepx_client::ProviderToolState::InProgress => ProviderToolState::InProgress,
                    deepx_client::ProviderToolState::Searching => ProviderToolState::Searching,
                    deepx_client::ProviderToolState::Completed => ProviderToolState::Completed,
                },
            }),
            DomainConversationEvent::RoundCompleted {
                turn_id,
                round_num,
                thinking,
                answer,
                output_ref,
                is_final,
            } => Some(ConversationEvent::RoundCompleted {
                turn_id: turn_id.clone(),
                round_num: *round_num,
                thinking: thinking.clone(),
                answer: answer.clone(),
                output_ref: output_ref
                    .as_ref()
                    .and_then(|value| serde_json::to_value(value).ok()),
                is_final: *is_final,
            }),
            DomainConversationEvent::ProviderRetrying { .. }
            | DomainConversationEvent::UsageUpdated { .. }
            | DomainConversationEvent::CompactStarted { .. }
            | DomainConversationEvent::CompactProgress { .. }
            | DomainConversationEvent::CompactFinished { .. }
            | DomainConversationEvent::ConversationCancelled { .. } => None,
        },
        RingingEvent::Tool(event) => match event {
            ToolEvent::ToolCallPrepared {
                tool_call_id,
                turn_id,
                round_num,
                name,
                args_so_far,
            } => Some(ConversationEvent::ToolCallPrepared {
                tool_call_id: tool_call_id.clone(),
                turn_id: turn_id.clone(),
                round_num: *round_num,
                name: name.clone(),
                args_so_far: args_so_far.clone(),
            }),
            ToolEvent::ToolStarted {
                tool_call_id,
                turn_id,
                round_num,
                name,
            } => Some(ConversationEvent::ToolStarted {
                tool_call_id: tool_call_id.clone(),
                turn_id: turn_id.clone(),
                round_num: *round_num,
                name: name.clone(),
            }),
            ToolEvent::ToolFinished {
                tool_call_id,
                turn_id,
                round_num,
                result,
            } => Some(ConversationEvent::ToolFinished {
                tool_call_id: tool_call_id.clone(),
                turn_id: turn_id.clone(),
                round_num: *round_num,
                result: serde_json::to_value(result).unwrap_or_default(),
            }),
            ToolEvent::ToolProgress { .. }
            | ToolEvent::ToolPermissionRequested { .. }
            | ToolEvent::ToolNotice { .. }
            | ToolEvent::AuditRecorded { .. }
            | ToolEvent::CodeChanged { .. } => None,
        },
        RingingEvent::Control(_) => None,
    }
}

fn map_delta_kind(kind: deepx_client::RoundDeltaKind) -> RoundDeltaKind {
    match kind {
        deepx_client::RoundDeltaKind::Thinking => RoundDeltaKind::Thinking,
        deepx_client::RoundDeltaKind::ToolCalling => RoundDeltaKind::ToolCalling,
        deepx_client::RoundDeltaKind::Answering => RoundDeltaKind::Answering,
    }
}

/// timeline 快照（`TimelineSnapshot` JSON）→ 恢复用的 turns。
///
/// 映射（对齐 deepx-domain `timeline.rs`）：
/// - turn：`user_text` / `state`（completed/failed/cancelled → 展示态）；
/// - round：blocks 按 `block_order` 排序，`reasoning` 块拼接为 thinking、
///   `text` 块拼接为 answer（markdown 原文，restore 时 final 渲染）、
///   `tool` 块转为工具卡（succeeded/failed → done）。
pub fn restored_turns(snapshot: &TimelineSnapshot) -> Vec<RestoredTurn> {
    let parsed = snapshot
        .turns
        .iter()
        .map(|turn| {
            let status = match turn.state {
                TimelineTurnState::Failed => TurnStatus::Failed,
                // cancelled 与 completed 均展示为完成态（内容已封存）。
                _ => TurnStatus::Completed,
            };
            let rounds = turn
                .rounds
                .iter()
                .map(|round| {
                    let mut blocks = round.blocks.clone();
                    // 快照一般有序；防御性按 block_order 排序。
                    blocks.sort_by_key(|block| block.block_order);
                    let mut thinking: Vec<String> = Vec::new();
                    let mut answer: Vec<String> = Vec::new();
                    let mut tool_calls: Vec<ToolCardView> = Vec::new();
                    for block in &blocks {
                        match block.kind {
                            TimelineBlockKind::Reasoning => thinking.push(block.text.clone()),
                            TimelineBlockKind::Text => answer.push(block.text.clone()),
                            TimelineBlockKind::Tool => {
                                if let Some(tool) = &block.tool {
                                    tool_calls.push(parse_tool(tool));
                                }
                            }
                            TimelineBlockKind::Notice => {}
                        }
                    }
                    RestoredRound {
                        round_num: round.round_num,
                        thinking: if thinking.is_empty() {
                            None
                        } else {
                            Some(thinking.join("\n"))
                        },
                        answer: if answer.is_empty() {
                            None
                        } else {
                            Some(answer.join("\n\n"))
                        },
                        tool_calls,
                    }
                })
                .collect();
            RestoredTurn {
                turn_id: turn.turn_id.clone(),
                created_seq: turn.created_seq,
                user_text: turn.user_text.clone(),
                status,
                rounds,
            }
        })
        .collect::<Vec<RestoredTurn>>();
    // daemon 快照已按 created_seq 排序（旧数据退化 turn_id 数值序）；前端
    // 防御性再排一次——旧 daemon / 第三方消费者可能给出无序数组，恢复窗口
    // （尾部 N 个）依赖数组序 = 时间序，错乱会恢复出错误的回合集合。
    let mut out: Vec<RestoredTurn> = parsed;
    out.sort_by_key(|t| (t.created_seq, turn_num(&t.turn_id)));
    out
}

/// turn_id → 数值序（t1/t10 → 1/10）；无数字后缀按 0（稳定排序兜底）。
fn turn_num(id: &str) -> u64 {
    id.trim_start_matches(|c: char| !c.is_ascii_digit())
        .parse()
        .unwrap_or(0)
}

/// timeline tool 块 → 工具卡视图。
fn parse_tool(tool: &TimelineTool) -> ToolCardView {
    let args_display = tool
        .args_json
        .as_deref()
        .or(tool.summary.as_deref())
        .unwrap_or("")
        .to_string();
    let done = matches!(
        tool.state,
        TimelineToolState::Succeeded | TimelineToolState::Failed
    );
    ToolCardView {
        id: tool.tool_call_id.clone(),
        name: Some(tool.name.clone()),
        args_display,
        done,
        provider: false,
    }
}

#[cfg(test)]
fn internal_event(value: &serde_json::Value) -> Option<ConversationEvent> {
    let event_type = value.get("type").and_then(serde_json::Value::as_str)?;
    let event = if matches!(
        event_type,
        "tool_call_prepared"
            | "tool_started"
            | "tool_progress"
            | "tool_finished"
            | "tool_permission_requested"
            | "tool_notice"
            | "audit_recorded"
            | "code_changed"
    ) {
        RingingEvent::Tool(serde_json::from_value(value.clone()).ok()?)
    } else {
        RingingEvent::Conversation(serde_json::from_value(value.clone()).ok()?)
    };
    render_event(&event)
}

#[cfg(test)]
fn timeline_turns(value: &serde_json::Value) -> Vec<RestoredTurn> {
    serde_json::from_value::<TimelineSnapshot>(value.clone())
        .map(|snapshot| restored_turns(&snapshot))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use markdown_winui::RoundDeltaKind;

    /// 真实形状：turn_started（与 deepx-domain 字段一致）
    #[test]
    fn turn_started_roundtrips() {
        let v = serde_json::json!({"type":"turn_started","turn_id":"t1","user_text":"hi"});
        let ev = internal_event(&v).expect("parse");
        assert_eq!(
            ev,
            ConversationEvent::TurnStarted {
                turn_id: "t1".into(),
                user_text: "hi".into()
            }
        );
    }

    /// 真实形状：round_delta（含 kind=answering）
    #[test]
    fn round_delta_roundtrips() {
        let v = serde_json::json!({
            "type":"round_delta","turn_id":"t1","round_num":0,
            "kind":"answering","delta":"hel"
        });
        assert_eq!(
            internal_event(&v).expect("parse"),
            ConversationEvent::RoundDelta {
                turn_id: "t1".into(),
                round_num: 0,
                kind: RoundDeltaKind::Answering,
                delta: "hel".into(),
            }
        );
    }

    /// block_checkpoint 携带上游 char_count：未知字段忽略
    #[test]
    fn block_checkpoint_ignores_extra_fields() {
        let v = serde_json::json!({
            "type":"block_checkpoint","turn_id":"t1","round_num":0,
            "kind":"thinking","text":"完整值","char_count":9
        });
        assert_eq!(
            internal_event(&v).expect("parse"),
            ConversationEvent::BlockCheckpoint {
                turn_id: "t1".into(),
                round_num: 0,
                kind: RoundDeltaKind::Thinking,
                text: "完整值".into(),
            }
        );
    }

    /// round_completed 的 output_ref 是对象（ContentRef）：保留任意形状
    #[test]
    fn round_completed_accepts_content_ref_object() {
        let v = serde_json::json!({
            "type":"round_completed","turn_id":"t1","round_num":0,
            "thinking":null,"answer":"done","is_final":true,
            "output_ref":{"content_id":"k123","media_type":"text/markdown","sha256":"abc","truncated":false}
        });
        let ev = internal_event(&v).expect("parse");
        let ConversationEvent::RoundCompleted {
            turn_id,
            round_num,
            answer,
            is_final,
            output_ref,
            ..
        } = ev
        else {
            panic!("expect round_completed");
        };
        assert_eq!(turn_id, "t1");
        assert_eq!(round_num, 0);
        assert_eq!(answer.as_deref(), Some("done"));
        assert!(is_final);
        assert_eq!(output_ref.unwrap()["content_id"], "k123");
    }

    /// turn_failed：新领域事件映射到渲染协议
    #[test]
    fn turn_failed_roundtrips() {
        let v = serde_json::json!({
            "type":"turn_failed","turn_id":"t1",
            "error":{"error_id":"e1","code":"E_TIMEOUT","message":"provider timeout","retryable":true}
        });
        let ev = internal_event(&v).expect("parse");
        assert!(matches!(
            ev,
            ConversationEvent::TurnFailed { turn_id, .. } if turn_id == "t1"
        ));
    }

    /// 渲染不关心的事件：None（忽略，不 panic）
    #[test]
    fn unrelated_events_are_dropped() {
        for v in [
            serde_json::json!({"type":"provider_retrying","turn_id":"t1","round_num":0,"attempt":1,"max_retries":3,"delay_secs":2,"error_message":"boom"}),
            serde_json::json!({"type":"usage_updated","turn_id":"t1","round_num":0,"usage":{"prompt_tokens":0,"completion_tokens":0,"total_tokens":0},"context_limit":8192,"model":"m"}),
            serde_json::json!({"type":"compact_started","compact_id":"c1","turns_total":5,"turns_keeping":2}),
            serde_json::json!({"type":"conversation_cancelled","turn_id":null}),
        ] {
            assert_eq!(internal_event(&v), None, "unrelated: {v}");
        }
    }

    /// 畸形 JSON：None（防御，绝不 panic）
    #[test]
    fn malformed_events_are_dropped() {
        assert_eq!(
            internal_event(&serde_json::json!({"type":"round_delta"})),
            None
        );
        assert_eq!(internal_event(&serde_json::json!({"foo":1})), None);
    }

    /// `provider_tool_status`（daemon 真实事件，replaceable by call_id）——
    /// 此前协议缺该变体被 Unknown 吞掉，tool 消息不显示。回归：必须解析。
    #[test]
    fn provider_tool_status_is_parsed() {
        let v = serde_json::json!({
            "type": "provider_tool_status",
            "turn_id": "t1",
            "round_num": 0,
            "call_id": "call-1",
            "tool_kind": "web_search",
            "state": "in_progress",
        });
        let ev = internal_event(&v).expect("parse provider_tool_status");
        assert!(matches!(
            ev,
            ConversationEvent::ProviderToolStatus {
                turn_id,
                call_id,
                tool_kind,
                state,
                ..
            } if turn_id == "t1" && call_id == "call-1"
                && tool_kind == "web_search"
                && state == markdown_winui::ProviderToolState::InProgress
        ));
    }

    /// timeline 快照 → 恢复 turns：块排序、thinking/answer 拼接、工具卡 done
    #[test]
    fn timeline_snapshot_restores_turns() {
        let v = serde_json::json!({
            "watermark": 7,
            "turns": [
                {
                    "turn_id": "t1",
                    "user_text": "hi",
                    "sealed": true,
                    "state": "completed",
                    "rounds": [
                        {
                            "round_num": 0,
                            "sealed": true,
                            "is_final": true,
                            "blocks": [
                                {"block_id":"b3","block_order":2,"kind":"tool","state":"sealed",
                                 "tool":{"tool_call_id":"c1","name":"web_search","state":"succeeded",
                                         "args_json":"{\"q\":\"rust\"}","summary":"searched"}},
                                {"block_id":"b2","block_order":1,"kind":"text","state":"sealed",
                                 "text":"答案 **加粗**"},
                                {"block_id":"b1","block_order":0,"kind":"reasoning","state":"sealed",
                                 "text":"思考中"}
                            ]
                        }
                    ]
                },
                {
                    "turn_id": "t2",
                    "user_text": "再来",
                    "sealed": false,
                    "state": "running",
                    "rounds": []
                }
            ]
        });
        let turns = timeline_turns(&v);
        assert_eq!(turns.len(), 2);
        // t1：completed；块按 block_order 重排；reasoning→thinking、text→answer、tool→卡
        let t1 = &turns[0];
        assert_eq!(t1.turn_id, "t1");
        assert_eq!(t1.status, TurnStatus::Completed);
        assert_eq!(t1.rounds.len(), 1);
        let r0 = &t1.rounds[0];
        assert_eq!(r0.thinking.as_deref(), Some("思考中"));
        assert_eq!(r0.answer.as_deref(), Some("答案 **加粗**"));
        assert_eq!(r0.tool_calls.len(), 1);
        assert_eq!(r0.tool_calls[0].id, "c1");
        assert_eq!(r0.tool_calls[0].name.as_deref(), Some("web_search"));
        assert!(r0.tool_calls[0].done);
        // t2：running（未 sealed）
        assert_eq!(
            turns[1].status,
            TurnStatus::Completed,
            "running 显示为完成态（内容封存）"
        );
        assert!(turns[1].rounds.is_empty());
    }

    /// 无 turns / 非快照：空（防御）
    #[test]
    fn timeline_empty_is_empty() {
        assert!(timeline_turns(&serde_json::json!({})).is_empty());
        assert!(timeline_turns(&serde_json::json!({"watermark":0,"turns":[]})).is_empty());
    }
}
