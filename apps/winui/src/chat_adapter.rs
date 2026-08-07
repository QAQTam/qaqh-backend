//! ChatView 原生适配层：真实 wire 事件 → `markdown_winui::ConversationEvent`。
//!
//! 内部协议（`markdown-winui::protocol`）与 `deepx-domain::event::ConversationEvent`
//! **serde tag 同构**（`type` tag + snake_case），且内部枚举带 `Unknown`
//! 兜底——真实事件 JSON 可直接反序列化，零映射胶水：
//!
//! - 渲染关心的 6 个变体（turn_started / turn_completed / turn_failed /
//!   round_delta / block_checkpoint / round_completed）→ 原样消费；
//! - 其余（provider_retrying / provider_tool_status / usage_updated /
//!   compact_* / conversation_cancelled）→ `Unknown` 兜底 → `None` 忽略
//!   （渲染不关心，丢失更新；后续按需补能力）。
//!
//! 数据源：`BridgeCore` 的 `EventBatch`（deepx-client handlers 回调），
//! 每个事件是 `serde_json::Value`（`{"type":"...", ...}`）。

use markdown_winui::{ConversationEvent, RestoredRound, RestoredTurn, ToolCardView, TurnStatus};

/// 反序列化一个 wire 事件为渲染事件；渲染不关心的返回 `None`。
pub fn internal_event(v: &serde_json::Value) -> Option<ConversationEvent> {
    match serde_json::from_value::<ConversationEvent>(v.clone()) {
        Ok(ev) => match ev {
            ConversationEvent::Unknown => None,
            other => Some(other),
        },
        // 防御：字段形状不匹配（如 output_ref 结构变化）→ 丢弃，绝不 panic。
        Err(_) => None,
    }
}

/// timeline 快照（`TimelineSnapshot` JSON）→ 恢复用的 turns。
///
/// 映射（对齐 deepx-domain `timeline.rs`）：
/// - turn：`user_text` / `state`（completed/failed/cancelled → 展示态）；
/// - round：blocks 按 `block_order` 排序，`reasoning` 块拼接为 thinking、
///   `text` 块拼接为 answer（markdown 原文，restore 时 final 渲染）、
///   `tool` 块转为工具卡（succeeded/failed → done）。
pub fn timeline_turns(v: &serde_json::Value) -> Vec<RestoredTurn> {
    let Some(turns) = v.get("turns").and_then(|t| t.as_array()) else {
        return Vec::new();
    };
    turns
        .iter()
        .filter_map(|t| {
            let turn_id = t.get("turn_id")?.as_str()?.to_string();
            let user_text = t
                .get("user_text")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let status = match t.get("state").and_then(|s| s.as_str()) {
                Some("failed") => TurnStatus::Failed,
                // cancelled 与 completed 均展示为完成态（内容已封存）。
                _ => TurnStatus::Completed,
            };
            let rounds = t
                .get("rounds")
                .and_then(|r| r.as_array())
                .map(|rs| {
                    rs.iter()
                        .filter_map(|r| {
                            let round_num = r
                                .get("round_num")
                                .and_then(|n| n.as_u64())
                                .unwrap_or(0) as u32;
                            let mut blocks = r
                                .get("blocks")
                                .and_then(|b| b.as_array())
                                .cloned()
                                .unwrap_or_default();
                            // 快照一般有序；防御性按 block_order 排序。
                            blocks.sort_by_key(|b| {
                                b.get("block_order")
                                    .and_then(|o| o.as_u64())
                                    .unwrap_or(0)
                            });
                            let mut thinking: Vec<String> = Vec::new();
                            let mut answer: Vec<String> = Vec::new();
                            let mut tool_calls: Vec<ToolCardView> = Vec::new();
                            for b in &blocks {
                                match b.get("kind").and_then(|k| k.as_str()) {
                                    Some("reasoning") => {
                                        if let Some(t) = b.get("text").and_then(|x| x.as_str()) {
                                            thinking.push(t.to_string());
                                        }
                                    }
                                    Some("text") => {
                                        if let Some(t) = b.get("text").and_then(|x| x.as_str()) {
                                            answer.push(t.to_string());
                                        }
                                    }
                                    Some("tool") => {
                                        if let Some(tool) = b.get("tool") {
                                            tool_calls.push(parse_tool(tool));
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            Some(RestoredRound {
                                round_num,
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
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            Some(RestoredTurn {
                turn_id,
                user_text,
                status,
                rounds,
            })
        })
        .collect()
}

/// timeline tool 块 → 工具卡视图。
fn parse_tool(tool: &serde_json::Value) -> ToolCardView {
    let id = tool
        .get("tool_call_id")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let name = tool.get("name").and_then(|x| x.as_str()).map(str::to_string);
    let args_display = tool
        .get("args_json")
        .and_then(|x| x.as_str())
        .or_else(|| tool.get("summary").and_then(|x| x.as_str()))
        .unwrap_or("")
        .to_string();
    let done = matches!(
        tool.get("state").and_then(|s| s.as_str()),
        Some("succeeded") | Some("failed")
    );
    ToolCardView {
        id,
        name,
        args_display,
        done,
    }
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
            "output_ref":{"kind":"session_content","key":"k123"}
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
        assert_eq!(output_ref.unwrap()["key"], "k123");
    }

    /// turn_failed：新领域事件映射到渲染协议
    #[test]
    fn turn_failed_roundtrips() {
        let v = serde_json::json!({
            "type":"turn_failed","turn_id":"t1",
            "error":{"scope":"provider","code":"E_TIMEOUT","message":"provider timeout"}
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
            serde_json::json!({"type":"usage_updated","turn_id":"t1","round_num":0,"usage":{},"context_limit":8192,"model":"m"}),
            serde_json::json!({"type":"compact_started","compact_id":"c1","turns_total":5,"turns_keeping":2}),
            serde_json::json!({"type":"conversation_cancelled","turn_id":null}),
        ] {
            assert_eq!(internal_event(&v), None, "unrelated: {v}");
        }
    }

    /// 畸形 JSON：None（防御，绝不 panic）
    #[test]
    fn malformed_events_are_dropped() {
        assert_eq!(internal_event(&serde_json::json!({"type":"round_delta"})), None);
        assert_eq!(internal_event(&serde_json::json!({"foo":1})), None);
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
        assert_eq!(turns[1].status, TurnStatus::Completed, "running 显示为完成态（内容封存）");
        assert!(turns[1].rounds.is_empty());
    }

    /// 无 turns / 非快照：空（防御）
    #[test]
    fn timeline_empty_is_empty() {
        assert!(timeline_turns(&serde_json::json!({})).is_empty());
        assert!(timeline_turns(&serde_json::json!({"watermark":0,"turns":[]})).is_empty());
    }
}
