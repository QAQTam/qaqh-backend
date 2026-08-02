//! Conversation 频道完整快照：从持久化 session 消息构建领域状态。
//!
//! PLAN：ConversationSnapshot 直接从持久化 session 消息构建，客户端切流后
//! 经 HTTP 读取完整历史。快照 state 使用中立 JSON 形状（非 legacy wire 类型），
//! 包含 turns、total_turns、has_more、usage、usage_totals 及其计数快照。

use deepx_session::SessionManager;
use serde_json::json;

/// 读取持久化消息并构建中立对话状态。无持久化会话时返回 `None`。
pub fn persisted_conversation_state(seed: &str) -> Option<serde_json::Value> {
    let (meta, archive_messages, compact_context) =
        SessionManager::global().load_for_resume(seed)?;
    // 与 legacy resume 投影保持一致：compact 上下文优先（否则 daemon 快照与
    // worker resume 的 transcript 基线不一致）。
    let messages = compact_context
        .as_ref()
        .map(|context| context.messages.as_slice())
        .unwrap_or(archive_messages.as_slice());
    let (total, turns) = deepx_msglp::util::project_turns_from_messages(seed, messages, None, None);
    Some(json!({
        "turns": turns.iter().map(neutral_turn).collect::<Vec<_>>(),
        "total_turns": total,
        "has_more": false,
        "usage": meta.last_usage,
        "usage_totals": meta.usage_totals,
        "usage_requests": meta.usage_requests,
        "cache_reported_requests": meta.effective_cache_reported_requests(),
    }))
}

/// 把 legacy `TurnData` 投影成中立 JSON（Ringing snapshot 不携带 legacy wire 类型）。
fn neutral_turn(turn: &deepx_proto::TurnData) -> serde_json::Value {
    json!({
        "turn_id": turn.turn_id,
        "user_text": turn.user_text,
        "rounds": turn.rounds.iter().map(neutral_round).collect::<Vec<_>>(),
    })
}

fn neutral_round(round: &deepx_proto::RoundData) -> serde_json::Value {
    json!({
        "round_num": round.round_num,
        "is_final": round.is_final,
        "thinking": round.thinking,
        "answer": round.answer,
        "blocks": round.blocks,
        "tool_calls": round.tool_calls,
        "tool_results": round.tool_results,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use deepx_proto::{RoundData, TurnData};

    #[test]
    fn turns_are_projected_to_neutral_json() {
        let turn = TurnData {
            turn_id: "t1".into(),
            user_text: "hello".into(),
            rounds: vec![RoundData {
                round_num: 0,
                is_final: true,
                thinking: Some("plan".into()),
                answer: Some("answer".into()),
                tool_calls: vec![],
                tool_results: vec![],
                blocks: vec![],
            }],
        };
        let value = neutral_turn(&turn);
        assert_eq!(value["turn_id"], "t1");
        assert_eq!(value["user_text"], "hello");
        assert_eq!(value["rounds"][0]["round_num"], 0);
        assert_eq!(value["rounds"][0]["is_final"], true);
        assert_eq!(value["rounds"][0]["thinking"], "plan");
        assert_eq!(value["rounds"][0]["answer"], "answer");
        assert!(!value.to_string().contains("Agent2Ui"));
    }
}
