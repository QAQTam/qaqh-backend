//! Canonical Takeover Mock & Isolation Test
//!
//! 目标：
//! 验证在【完全不依赖旧版 Timeline】（零 TimelineStore、零 TimelineSnapshot、零 LegacyWriterFacade）
//! 的纯净环境下，Canonical 架构（CanonicalSessionStore + ConversationProjection）
//! 是否足以独立接管前端所需的全生命周期对话、工具调用、回合控制与断线恢复。

use qaqh_session::canonical::{CanonicalSessionStore, WriterId};
use qaqh_session::projection::ConversationSnapshot;
use qaqh_session::session_fact_v2::{
    ActorKind, ActorRef, AssistantBlockKind, AssistantBlockSealed, BlockId, ContentHash,
    ContentRef, EventId, ExecutionId, FactPayload, InputAccepted, InputId, InputKind, InputPurpose,
    LogId, ProjectionSlot, SessionFact, SessionId, ToolCallDeclared, ToolCallId, ToolFinished,
    ToolMetrics, ToolTerminalStatus, TurnFinished, TurnId, TurnMode, TurnStarted, TurnTerminal,
};

const ENVELOPE_FIXTURE: &str = include_str!("fixtures/session_fact_v2/envelope.jsonl");
const NOW_MS: i64 = 1_789_830_000_000;
const LEASE_MS: i64 = 10_000;

fn session_id() -> SessionId {
    SessionId::new("0198f1a0-0000-7000-8000-000000000001")
}

fn log_id() -> LogId {
    LogId::new("0198f1a0-0000-7000-8000-000000000002")
}

fn fact(
    payload: FactPayload,
    event_ordinal: u64,
    turn_id: Option<TurnId>,
    call_id: Option<ToolCallId>,
) -> SessionFact {
    let mut fact: SessionFact =
        serde_json::from_str(ENVELOPE_FIXTURE.trim()).expect("parse envelope fixture");
    fact.session_id = session_id();
    fact.log_id = log_id();
    fact.fact_seq = 0;
    fact.event_id = EventId::new(format!("01J{event_ordinal:023}"));
    fact.payload = payload;
    fact.turn_id = turn_id;
    fact.call_id = call_id;
    fact
}

fn content_ref(byte: u8) -> ContentRef {
    ContentRef::new(ContentHash::new(format!("sha256:{byte:064x}")))
}

#[test]
fn test_canonical_architecture_fully_takes_over_client_lifecycle() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut store = CanonicalSessionStore::open(temp.path(), session_id(), log_id())
        .expect("open canonical session store");
    let lease = store
        .acquire_writer(WriterId::new("writer-mock-backend"), NOW_MS, LEASE_MS)
        .expect("acquire writer");

    // ── 1. 模拟客户端连接 (Bootstrap)：拿到初始纯空 ConversationSnapshot ──
    let initial_bootstrap: ConversationSnapshot = store.snapshot().conversation;
    assert_eq!(initial_bootstrap.revision, 0);
    assert_eq!(initial_bootstrap.last_fact_seq, 0);
    assert!(initial_bootstrap.turns.is_empty());
    assert!(initial_bootstrap.context.is_empty());

    // ── 2. 用户发送消息 (InputAccepted) ──
    let input_id = InputId::new("input_01J00000000000000000000001");
    let input_fact = fact(
        FactPayload::InputAccepted(InputAccepted {
            input_id: input_id.clone(),
            input_kind: InputKind::UserText,
            input_purpose: InputPurpose::TriggerTurn,
            content_ref: None,
            inline_text: Some("帮我分析项目架构并执行测试".into()),
            attachments: vec![],
            actor: ActorRef {
                kind: ActorKind::User,
                id: "user-alice".into(),
                display_name: Some("Alice".into()),
            },
            client_request_id: None,
        }),
        1,
        None,
        None,
    );

    let append1 = store.append(&lease, input_fact, NOW_MS + 10).expect("append input");
    assert_eq!(append1.fact.fact_seq, 1);
    
    // 验证事件流下发了标准的 Conversation 投影事件
    let conv_event1 = append1
        .events
        .iter()
        .find(|e| e.projection_slot == Some(ProjectionSlot::Conversation))
        .expect("conversation projection event exists");
    assert_eq!(conv_event1.source_fact_seq, 1);

    // ── 3. 回合开启 (TurnStarted) ──
    let turn_id = TurnId::new("turn_01J00000000000000000000002");
    let turn_started_fact = fact(
        FactPayload::TurnStarted(TurnStarted {
            turn_id: turn_id.clone(),
            input_id: input_id.clone(),
            mode: TurnMode::Normal,
            recovery_ref: None,
        }),
        2,
        Some(turn_id.clone()),
        None,
    );
    let append2 = store.append(&lease, turn_started_fact, NOW_MS + 20).expect("append turn started");
    assert_eq!(append2.fact.fact_seq, 2);

    // ── 4. 工具调用声明 (ToolCallDeclared) ──
    let call_id = ToolCallId::new("call_01J00000000000000000000003");
    let tool_call_fact = fact(
        FactPayload::ToolCallDeclared(ToolCallDeclared {
            turn_id: turn_id.clone(),
            call_id: call_id.clone(),
            tool_name: "exec".into(),
            args_ref: content_ref(0xaa),
            args_hash: ContentHash::new(format!("sha256:{:064x}", 0xaa)),
        }),
        3,
        Some(turn_id.clone()),
        Some(call_id.clone()),
    );
    let append3 = store.append(&lease, tool_call_fact, NOW_MS + 30).expect("append tool call");
    assert_eq!(append3.fact.fact_seq, 3);

    // ── 5. 工具执行结果落地 (ToolFinished) ──
    let exec_id = ExecutionId::new("exec_01J00000000000000000000005");
    let tool_finished_fact = fact(
        FactPayload::ToolFinished(ToolFinished {
            call_id: call_id.clone(),
            execution_id: Some(exec_id.clone()),
            terminal_status: ToolTerminalStatus::Succeeded,
            output_ref: Some(content_ref(0xbb)),
            error: None,
            metrics: ToolMetrics {
                started_at_ms: NOW_MS + 31,
                finished_at_ms: NOW_MS + 39,
                retry_count: 0,
                output_bytes: 512,
                progress_bytes_total: 512,
            },
            reconciled: false,
            evidence_ref: None,
            evidence_fact_seq: None,
            evidence_event_id: None,
            recovery_ref: None,
            finished_at_ms: NOW_MS + 40,
        }),
        4,
        Some(turn_id.clone()),
        Some(call_id.clone()),
    );
    let append4 = store.append(&lease, tool_finished_fact, NOW_MS + 40).expect("append tool finished");
    assert_eq!(append4.fact.fact_seq, 4);

    // ── 6. 助手回复密封 (AssistantBlockSealed) ──
    let block_id = BlockId::new("block_01J00000000000000000000004");
    let assistant_block_fact = fact(
        FactPayload::AssistantBlockSealed(AssistantBlockSealed {
            turn_id: turn_id.clone(),
            block_id: block_id.clone(),
            kind: AssistantBlockKind::Answer,
            content_ref: content_ref(0xcc),
            model: "claude-3-7-sonnet".into(),
            usage: None,
        }),
        5,
        Some(turn_id.clone()),
        None,
    );
    let append5 = store.append(&lease, assistant_block_fact, NOW_MS + 50).expect("append assistant block");
    assert_eq!(append5.fact.fact_seq, 5);

    // ── 7. 回合终结 (TurnFinished) ──
    let turn_finished_fact = fact(
        FactPayload::TurnFinished(TurnFinished {
            turn_id: turn_id.clone(),
            terminal: TurnTerminal::Completed,
            usage: None,
            error: None,
            finished_at_ms: NOW_MS + 60,
        }),
        6,
        Some(turn_id.clone()),
        None,
    );
    let append6 = store.append(&lease, turn_finished_fact, NOW_MS + 60).expect("append turn finished");
    assert_eq!(append6.fact.fact_seq, 6);

    // ── 8. 检查在线状态机 (Online Projection Snapshot) ──
    let live_snapshot = store.snapshot().conversation;
    assert_eq!(live_snapshot.last_fact_seq, 6);
    assert_eq!(live_snapshot.revision, 6); // 6 个推进 revision 的 delta (Input + TurnStarted + ToolDeclared + ToolFinished + BlockSealed + TurnFinished)
    assert_eq!(live_snapshot.turns.len(), 1);
    assert_eq!(live_snapshot.turns[0].turn_id, turn_id);
    assert_eq!(live_snapshot.context.len(), 3); // Input + Tool + BlockSealed

    // ── 9. 断电 / 重启 / 客户端断线恢复测试 ──
    // 强制丢弃内存中的 store（模拟 daemon 重启或客户端重新从磁盘 bootstrap）
    drop(store);

    let recovered_store = CanonicalSessionStore::open(temp.path(), session_id(), log_id())
        .expect("reopen canonical store from pure events.jsonl");

    let recovered_snapshot = recovered_store.snapshot().conversation;

    // ── 10. 核心验证：重启后从事实日志重建的快照与在线增量快照 100% 一致 ──
    assert_eq!(
        recovered_snapshot, live_snapshot,
        "Canonical 架构事实重放重建结果必须与在线增量投影完全一致"
    );
    assert_eq!(recovered_snapshot.turns.len(), 1);
    assert_eq!(recovered_snapshot.turns[0].turn_id, turn_id);
    assert_eq!(recovered_snapshot.context.len(), 3);

    // 验证全生命周期事实单调性
    assert_eq!(recovered_store.committed().committed_fact_seq, 6);
    assert_eq!(recovered_store.last_fact_seq(), 6);
}
