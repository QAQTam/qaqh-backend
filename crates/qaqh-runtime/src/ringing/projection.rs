//! 领域 snapshot projection。
//!
//! PLAN 硬规则：**Snapshot 必须表达领域状态，禁止用事件数组模拟状态**；
//! snapshot 只能从领域状态/领域事件生成，不从 legacy wire 反推。
//!
//! 本模块维护每 seed+channel 的领域状态（JSON 视图），由 `apply` 事件驱动。
//! 完整强类型投影在频道迁移（T9/T10）时按需扩展；基础骨架在此。

use std::collections::HashMap;

use qaqh_domain::{DomainEvent, RingingChannel};
use qaqh_ringing::RingingChannelSnapshot;

/// 频道快照投影。
#[derive(Debug, Default)]
pub struct SnapshotProjector {
    state: HashMap<(RingingChannel, String), serde_json::Value>,
    revisions: HashMap<(RingingChannel, String), u64>,
}

impl SnapshotProjector {
    pub fn new() -> Self {
        Self::default()
    }

    /// 应用领域事件并更新该 seed+channel 的领域状态。
    /// 返回是否发生状态变更（用于决定是否 bump state_revision）。
    pub fn apply(&mut self, channel: RingingChannel, seed: &str, event: &DomainEvent) -> bool {
        let key = (channel, seed.to_string());
        let entry = self.state.entry(key.clone()).or_insert_with(|| {
            serde_json::json!({
                "seed": seed,
                "channel": channel.as_str(),
                "revision": 0,
            })
        });
        let changed = Self::fold(channel, entry, event);
        if changed {
            let rev = self.revisions.entry(key).or_default();
            *rev = rev.saturating_add(1);
            entry["revision"] = serde_json::Value::from(*rev);
        }
        changed
    }

    /// 生成领域快照（wire 层 `RingingChannelSnapshot`）。
    pub fn snapshot_for(
        &self,
        channel: RingingChannel,
        seed: &str,
        baseline_stream_seq: u64,
    ) -> RingingChannelSnapshot {
        let state = self
            .state
            .get(&(channel, seed.to_string()))
            .cloned()
            .unwrap_or_else(|| {
                serde_json::json!({
                    "seed": seed,
                    "channel": channel.as_str(),
                    "revision": 0,
                })
            });
        let revision = self
            .revisions
            .get(&(channel, seed.to_string()))
            .copied()
            .unwrap_or(0);
        RingingChannelSnapshot::new(channel, seed, baseline_stream_seq, revision, state)
    }

    pub fn revision(&self, channel: RingingChannel, seed: &str) -> u64 {
        self.revisions
            .get(&(channel, seed.to_string()))
            .copied()
            .unwrap_or(0)
    }

    /// 领域状态折叠（按频道）。仅实现基础状态演化；
    /// 强类型字段（turns/tools/control）在 T9/T10 频道迁移时补齐。
    fn fold(channel: RingingChannel, state: &mut serde_json::Value, event: &DomainEvent) -> bool {
        match (channel, event) {
            (RingingChannel::Control, DomainEvent::Control(ce)) => {
                use qaqh_domain::ControlEvent as CE;
                match ce {
                    CE::SessionStateChanged { state: s, .. } => {
                        state["session_state"] = serde_json::json!(s);
                        true
                    }
                    CE::SessionActivityChanged { state: s, .. } => {
                        state["activity"] = serde_json::json!(s);
                        true
                    }
                    CE::SessionMetaChanged { title, .. } => {
                        // 元数据变更（标题）：快照只记录"已变更"事实，
                        // 具体值由前端 session.list 重拉（全量权威）。
                        state["meta_changed"] = serde_json::json!(title);
                        true
                    }
                    CE::ConfigChanged { rev } => {
                        // 配置变更通知：快照只记"已变更+rev"，值由消费者
                        // config.load 重拉（P2-D2）。
                        state["config_rev"] = serde_json::json!(rev);
                        true
                    }
                    CE::AgentLifecycleChanged { state: s } => {
                        state["agent_lifecycle"] = serde_json::json!(s);
                        true
                    }
                    CE::InteractionRequested {
                        interaction_id,
                        questions,
                        ..
                    } => {
                        state["pending_interaction"] = serde_json::json!({
                            "id": interaction_id,
                            "kind": "ask",
                            "details": { "questions": questions },
                        });
                        true
                    }
                    CE::InteractionResolved { .. } => {
                        state["pending_interaction"] = serde_json::Value::Null;
                        true
                    }
                    CE::PlanReviewRequested {
                        interaction_id,
                        plan_content,
                        review_type,
                        todo_items,
                        ..
                    } => {
                        state["pending_interaction"] = serde_json::json!({
                            "id": interaction_id,
                            "kind": "plan",
                            "details": {
                                "plan_content": plan_content,
                                "review_type": review_type,
                                "todo_items": todo_items,
                            },
                        });
                        true
                    }
                    CE::PlanReviewResolved { .. } => {
                        state["pending_interaction"] = serde_json::Value::Null;
                        true
                    }
                    CE::OperationFailed { .. } => {
                        state["last_failure"] = serde_json::json!({ "occurred": true });
                        true
                    }
                    CE::OperationCompleted { .. } => {
                        // R6：成功完成即清 last_failure，避免陈旧失败横幅
                        // 在后续所有 bootstrap 快照中阴魂不散。
                        state["last_failure"] = serde_json::Value::Null;
                        true
                    }
                    CE::SystemNotice { notice_id, .. } => {
                        state["last_notice"] = serde_json::json!(notice_id);
                        true
                    }
                    CE::DashboardSnapshot { snapshot } => {
                        state["dashboard_snapshot"] = serde_json::json!(snapshot);
                        true
                    }
                    CE::SkillsUpdated { .. }
                    | CE::DashboardUpdated { .. }
                    // 瞬态终态推送：不折叠进快照（tracker 收敛走实时事件）。
                    | CE::SubagentStatus { .. } => false,
                }
            }
            (RingingChannel::Conversation, DomainEvent::Conversation(ce)) => {
                use qaqh_domain::ConversationEvent as CE;
                match ce {
                    CE::TurnStarted { turn_id, .. } => {
                        state["active_turn"] = serde_json::json!(turn_id);
                        // R6：新回合开始即清除历史取消标记，否则快照在
                        // 会话余生持续误报 cancelled。
                        state["cancelled"] = serde_json::Value::Null;
                        true
                    }
                    CE::TurnCompleted { turn_id, .. } => {
                        state["last_completed_turn"] = serde_json::json!(turn_id);
                        state["active_turn"] = serde_json::Value::Null;
                        state["cancelled"] = serde_json::Value::Null;
                        true
                    }
                    CE::TurnFailed { turn_id, .. } => {
                        state["last_failed_turn"] = serde_json::json!(turn_id);
                        state["active_turn"] = serde_json::Value::Null;
                        true
                    }
                    CE::RoundCompleted {
                        turn_id,
                        round_num,
                        is_final,
                        ..
                    } => {
                        state["last_round"] = serde_json::json!({
                            "turn_id": turn_id,
                            "round_num": round_num,
                            "final": is_final,
                        });
                        true
                    }
                    CE::CompactStarted { compact_id, .. } => {
                        state["compact_status"] = serde_json::json!("running");
                        state["compact_id"] = serde_json::json!(compact_id);
                        true
                    }
                    CE::CompactFinished {
                        compact_id, status, ..
                    } => {
                        state["compact_status"] = serde_json::json!(status);
                        state["compact_id"] = serde_json::json!(compact_id);
                        true
                    }
                    CE::ConversationCancelled { .. } => {
                        state["active_turn"] = serde_json::Value::Null;
                        state["cancelled"] = serde_json::Value::Bool(true);
                        true
                    }
                    _ => false, // delta/usage/progress 不进快照
                }
            }
            (RingingChannel::Tool, DomainEvent::Tool(te)) => {
                use qaqh_domain::ToolEvent as TE;
                match te {
                    TE::ToolPermissionRequested {
                        tool_call_id,
                        tool_name,
                        action_summary,
                        reason,
                        paths,
                        category,
                        level,
                        risk,
                        consequence,
                        ..
                    } => {
                        state["pending_permission"] = serde_json::json!(tool_call_id);
                        state["pending_permission_details"] = serde_json::json!({
                            "tool_call_id": tool_call_id,
                            "tool_name": tool_name,
                            "action_summary": action_summary,
                            "reason": reason,
                            "paths": paths,
                            "category": category,
                            "level": level,
                            "risk": risk,
                            "consequence": consequence,
                        });
                        true
                    }
                    TE::ToolFinished { tool_call_id, .. } => {
                        state["last_finished"] = serde_json::json!(tool_call_id);
                        state["pending_permission"] = serde_json::Value::Null;
                        state["pending_permission_details"] = serde_json::Value::Null;
                        // ToolStarted 写入的 running 必须在终态清除，否则 daemon
                        // 重启重放 journal 后 tool 快照永远携带陈旧 running 列表
                        // （异常中断时 ToolFinished 从未到达，孤儿 turn 的收尾
                        // 也会依赖本分支清空）。
                        state["running"] = serde_json::Value::Null;
                        true
                    }
                    TE::ToolStarted {
                        tool_call_id,
                        turn_id,
                        round_num,
                        ..
                    } => {
                        // 一旦工具开始执行，审批请求已经被消费；bootstrap
                        // 不得继续暴露可重放的 challenge 事实。
                        state["pending_permission"] = serde_json::Value::Null;
                        state["pending_permission_details"] = serde_json::Value::Null;
                        // 对象化：孤儿收尾（seal_orphan_channel_state）需要
                        // turn_id/round_num 才能发布完整 ToolFinished 终态。
                        state["running"] = serde_json::json!([{
                            "tool_call_id": tool_call_id,
                            "turn_id": turn_id,
                            "round_num": round_num,
                        }]);
                        true
                    }
                    _ => false, // progress/prepared/notice/audit/code 不进快照
                }
            }
            // 频道与事件不匹配：拒绝投影（不变量：事件必须进入正确频道）
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_domain::{
        ActivityState, AgentLifecycleState, ControlEvent, ConversationEvent, SessionState,
        ToolEvent,
    };

    #[test]
    fn control_snapshot_tracks_interaction_pending() {
        let mut p = SnapshotProjector::new();
        let ev = |ce: ControlEvent| DomainEvent::Control(ce);
        p.apply(
            RingingChannel::Control,
            "s",
            &ev(ControlEvent::SessionStateChanged {
                seed: "s".into(),
                state: SessionState::Created,
            }),
        );
        assert!(p.apply(
            RingingChannel::Control,
            "s",
            &ev(ControlEvent::InteractionRequested {
                interaction_id: "i1".into(),
                turn_id: "t1".into(),
                mode: qaqh_domain::AskMode::Single,
                questions: vec![],
            }),
        ));
        let snap = p.snapshot_for(RingingChannel::Control, "s", 42);
        assert_eq!(snap.state["session_state"], "created");
        assert_eq!(snap.state["pending_interaction"]["id"], "i1");
        assert!(snap.state["pending_interaction"]["details"]["questions"].is_array());
        assert_eq!(snap.state_revision, 2);
    }

    #[test]
    fn conversation_snapshot_tracks_turn_lifecycle() {
        let mut p = SnapshotProjector::new();
        let ev = |ce: ConversationEvent| DomainEvent::Conversation(ce);
        p.apply(
            RingingChannel::Conversation,
            "s",
            &ev(ConversationEvent::TurnStarted {
                turn_id: "t1".into(),
                user_text: "hi".into(),
            }),
        );
        p.apply(
            RingingChannel::Conversation,
            "s",
            &ev(ConversationEvent::TurnCompleted {
                turn_id: "t1".into(),
                stop_reason: None,
                usage: None,
            }),
        );
        let snap = p.snapshot_for(RingingChannel::Conversation, "s", 10);
        assert_eq!(snap.state["active_turn"], serde_json::Value::Null);
        assert_eq!(snap.state["last_completed_turn"], "t1");
    }

    #[test]
    fn tool_snapshot_tracks_permission_then_finish() {
        let mut p = SnapshotProjector::new();
        let ev = |te: ToolEvent| DomainEvent::Tool(te);
        p.apply(
            RingingChannel::Tool,
            "s",
            &ev(ToolEvent::ToolPermissionRequested {
                tool_call_id: "c1".into(),
                turn_id: "t".into(),
                round_num: 0,
                tool_name: "exec".into(),
                action_summary: Some(r#"command: "cargo test""#.into()),
                reason: "r".into(),
                paths: vec![],
                category: qaqh_domain::PermissionCategory::Exec,
                level: 3,
                risk: qaqh_domain::PermissionRisk::High,
                consequence: "run".into(),
            }),
        );
        let pending = p.snapshot_for(RingingChannel::Tool, "s", 0);
        assert_eq!(pending.state["pending_permission"], "c1");
        assert_eq!(
            pending.state["pending_permission_details"]["tool_call_id"],
            "c1"
        );
        assert_eq!(pending.state["pending_permission_details"]["risk"], "high");
        p.apply(
            RingingChannel::Tool,
            "s",
            &ev(ToolEvent::ToolFinished {
                tool_call_id: "c1".into(),
                turn_id: "t".into(),
                round_num: 0,
                result: qaqh_domain::ToolResult::ok("ok"),
            }),
        );
        let snap = p.snapshot_for(RingingChannel::Tool, "s", 0);
        assert_eq!(snap.state["pending_permission"], serde_json::Value::Null);
        assert_eq!(
            snap.state["pending_permission_details"],
            serde_json::Value::Null
        );
        assert_eq!(snap.state["last_finished"], "c1");
    }

    #[test]
    fn activity_and_lifecycle_fold() {
        let mut p = SnapshotProjector::new();
        p.apply(
            RingingChannel::Control,
            "s",
            &DomainEvent::Control(ControlEvent::SessionActivityChanged {
                seed: "s".into(),
                state: ActivityState::WaitingUser,
                turn_id: Some("t".into()),
                seq: 1,
                updated_at: 0,
            }),
        );
        p.apply(
            RingingChannel::Control,
            "s",
            &DomainEvent::Control(ControlEvent::AgentLifecycleChanged {
                state: AgentLifecycleState::Ready,
            }),
        );
        let snap = p.snapshot_for(RingingChannel::Control, "s", 0);
        assert_eq!(snap.state["activity"], "waiting_user");
        assert_eq!(snap.state["agent_lifecycle"], "ready");
    }
}

// ── Persisted-history UI projection (moved verbatim from the former msgloop crate's util, PR-1-4) ──

/// Build the same UI turn projection directly from persisted messages.
///
/// The daemon uses this before an Agent worker exists, so a cold attach can
/// return a canonical transcript without depending on a later live event.
///
/// **回放过滤（BUG-2026-09-13-13 / #13）**：中断流遗留的悬挂 `ToolUse`
/// （缺 id 或 name，见 [`qaqh_types::ContentBlock::is_hanging_tool_use`]）
/// 不投影。写侧已在持久化前清洗新历史（`MessageStore::push_assistant`），
/// 这里额外过滤是为了**既有归档**：已落盘的旧会话不会因写侧修复而自愈，
/// 必须让每次 reload 的投影面也看不见它们。
pub fn build_turns_from_messages(
    seed: &str,
    messages: &[qaqh_types::Message],
    start: Option<usize>,
    max_count: Option<usize>,
) -> Vec<qaqh_domain::TurnData> {
    project_turns_from_messages(seed, messages, start, max_count).1
}

/// Return both the total persisted turn count and the requested UI window.
pub fn project_turns_from_messages(
    seed: &str,
    messages: &[qaqh_types::Message],
    start: Option<usize>,
    max_count: Option<usize>,
) -> (usize, Vec<qaqh_domain::TurnData>) {
    let (store, _) = qaqh_message::MessageStore::from_messages(seed, messages, 0);
    let total = store.turns().len();
    (total, build_turns(store.turns(), start, max_count))
}

/// Build only the tail window used by a cold daemon Snapshot.
pub fn project_recent_turns_from_messages(
    seed: &str,
    messages: &[qaqh_types::Message],
    max_count: usize,
) -> (usize, Vec<qaqh_domain::TurnData>) {
    let (store, _) = qaqh_message::MessageStore::from_messages(seed, messages, 0);
    let total = store.turns().len();
    let start = total.saturating_sub(max_count);
    (
        total,
        build_turns(store.turns(), Some(start), Some(max_count)),
    )
}

fn build_turns(
    all_turns: &[qaqh_message::Turn],
    start: Option<usize>,
    max_count: Option<usize>,
) -> Vec<qaqh_domain::TurnData> {
    use qaqh_types::ContentBlock;
    let range_start = start.unwrap_or(0).min(all_turns.len());
    let range_end = match max_count {
        Some(n) => (range_start + n).min(all_turns.len()),
        None => all_turns.len(),
    };

    let mut turns = Vec::new();
    for (ti, turn) in all_turns
        .iter()
        .enumerate()
        .skip(range_start)
        .take(range_end - range_start)
    {
        let mut rounds = Vec::new();
        for (ri, step) in turn.steps.iter().enumerate() {
            let thinking = step.assistant.content.iter().find_map(|b| {
                if let ContentBlock::Reasoning { reasoning } = b {
                    Some(reasoning.clone())
                } else {
                    None
                }
            });
            let answer = step.assistant.content.iter().find_map(|b| {
                if let ContentBlock::Text { text } = b {
                    Some(text.clone())
                } else {
                    None
                }
            });
            let tcs: Vec<qaqh_domain::ToolCallDef> = step
                .assistant
                .content
                .iter()
                .filter_map(|b| {
                    if let ContentBlock::ToolUse { id, name, input } = b {
                        (!b.is_hanging_tool_use()).then(|| qaqh_domain::ToolCallDef {
                            id: id.clone(),
                            name: name.clone(),
                            args_display: name.clone(),
                            args_json: input.to_string(),
                        })
                    } else {
                        None
                    }
                })
                .collect();
            let blocks: Vec<qaqh_domain::RoundBlock> = step
                .assistant
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Reasoning { reasoning } if !reasoning.is_empty() => {
                        Some(qaqh_domain::RoundBlock::Reasoning {
                            content: reasoning.clone(),
                        })
                    }
                    ContentBlock::Text { text } if !text.is_empty() => {
                        Some(qaqh_domain::RoundBlock::Text {
                            content: text.clone(),
                        })
                    }
                    ContentBlock::ToolUse { id, name, input } => {
                        // 悬挂 ToolUse 不进回放面（见模块文档）：前端会把它渲染成
                        // 永远 running 的幽灵卡，且不可执行。
                        if b.is_hanging_tool_use() {
                            return None;
                        }
                        Some(qaqh_domain::RoundBlock::Tool {
                            card: qaqh_domain::ToolCallDef {
                                id: id.clone(),
                                name: name.clone(),
                                args_display: name.clone(),
                                args_json: input.to_string(),
                            },
                        })
                    }
                    _ => None,
                })
                .collect();
            let trs: Vec<qaqh_domain::ToolResultDef> = step
                .tool_results
                .iter()
                .flat_map(|msg| {
                    msg.content.iter().filter_map(|b| {
                        if let ContentBlock::ToolResult {
                            tool_use_id,
                            result,
                        } = b
                        {
                            Some(qaqh_domain::ToolResultDef {
                                tool_call_id: tool_use_id.clone(),
                                output: result.model_text().to_string(),
                                success: result.is_success(),
                                status: Some(result.status),
                                file: None,
                                metrics: result.metrics.clone(),
                                display: result.display().cloned(),
                            })
                        } else {
                            None
                        }
                    })
                })
                .collect();
            rounds.push(qaqh_domain::RoundData {
                round_num: ri as u32,
                is_final: ri + 1 == turn.steps.len(),
                thinking,
                answer,
                tool_calls: tcs,
                tool_results: trs,
                blocks,
            });
        }
        let user_text = turn
            .user
            .content
            .iter()
            .find_map(|b| {
                if let ContentBlock::Text { text } = b {
                    Some(text.clone())
                } else {
                    None
                }
            })
            .unwrap_or_default();
        turns.push(qaqh_domain::TurnData {
            turn_id: format!("t{}", ti + 1),
            user_text,
            rounds,
        });
    }
    turns
}

#[cfg(test)]
mod persisted_projection_tests {
    use super::project_recent_turns_from_messages;
    use qaqh_types::{ContentBlock, Message};

    fn assistant(text: &str) -> Message {
        Message {
            msg_id: None,
            role: "assistant".into(),
            name: None,
            content: vec![ContentBlock::text(text)],
        }
    }

    #[test]
    fn cold_snapshot_projects_the_recent_persisted_turns() {
        let messages = vec![
            Message::system("system"),
            Message::user("first"),
            assistant("answer one"),
            Message::user("second"),
            assistant("answer two"),
        ];
        let (total, turns) = project_recent_turns_from_messages("seed", &messages, 1);
        assert_eq!(total, 2);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].turn_id, "t2");
        assert_eq!(turns[0].user_text, "second");
        assert_eq!(turns[0].rounds[0].answer.as_deref(), Some("answer two"));
    }

    /// #13：中断流抢救路径落盘的悬挂 ToolUse（`id:""` / `name:""` / `input:null`）
    /// 必须被回放投影过滤——它不可执行，投影出去只会变成前端幽灵工具卡。
    #[test]
    fn hanging_tool_use_never_enters_the_projection() {
        let hanging = Message {
            msg_id: None,
            role: "assistant".into(),
            name: None,
            content: vec![
                ContentBlock::text("partial answer"),
                ContentBlock::ToolUse {
                    id: String::new(),
                    name: String::new(),
                    input: serde_json::Value::Null,
                },
            ],
        };
        let messages = vec![Message::user("interrupted"), hanging];
        let (_, turns) = project_recent_turns_from_messages("seed", &messages, 1);
        let round = &turns[0].rounds[0];
        assert!(
            round.tool_calls.is_empty(),
            "hanging tool_use must not surface as a tool_call: {:?}",
            round.tool_calls
        );
        assert!(
            !round
                .blocks
                .iter()
                .any(|b| matches!(b, qaqh_domain::RoundBlock::Tool { .. })),
            "hanging tool_use must not surface as a tool block: {:?}",
            round.blocks
        );
        // 同一回合里的正文不受影响（只丢悬挂工具，不丢回答）。
        assert_eq!(round.answer.as_deref(), Some("partial answer"));
    }

    /// 反例对照：完整的 tool_use ↔ tool_result 对仍必须投影出来。
    #[test]
    fn well_formed_tool_use_still_projects() {
        let assistant = Message {
            msg_id: None,
            role: "assistant".into(),
            name: None,
            content: vec![ContentBlock::ToolUse {
                id: "call-1".into(),
                name: "read".into(),
                input: serde_json::json!({ "path": "/tmp/a" }),
            }],
        };
        let messages = vec![
            Message::user("go"),
            assistant,
            Message::tool("call-1", "ok", true),
        ];
        let (_, turns) = project_recent_turns_from_messages("seed", &messages, 1);
        let round = &turns[0].rounds[0];
        assert_eq!(round.tool_calls.len(), 1);
        assert_eq!(round.tool_calls[0].name, "read");
        assert_eq!(round.tool_results.len(), 1);
    }
}
