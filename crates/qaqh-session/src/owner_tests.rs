//! owner 往返测试:P2 退出标准的四条流(完整输入/工具输出/结构化结果/effects)
//! 在一个会话 owner 上端到端走通(报告一 §12.5-2)。

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use tokio::sync::broadcast;

    use qaqh_ringing_contract::events::{InteractionEndStatus, OutputStream, SessionEvent, SessionEventFrame};
    use qaqh_ringing_contract::fact::{
        ActorKind, ActorRef, BlockId, ContentValue, InputPurpose, InteractionDecision,
        InteractionId, InteractionKind, ToolCallId, ToolMetrics, TurnId, TurnMode,
    };
    use qaqh_ringing_contract::state::{
        CodeDelta, InteractionView, TimelineEntry, ToolCallStatus, ToolEffectInfo, TurnOutcome,
    };
    use qaqh_tool_core::{ExecOutputStream, ExecProgressEvent};

    use crate::db::store;
    use crate::owner::{OwnerDirectory, ToolTerminal};

    fn test_owner_dir() -> (tempfile::TempDir, Arc<OwnerDirectory>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let owners = Arc::new(
            OwnerDirectory::open(dir.path().join("qaqh.db")).expect("open owners db"),
        );
        (dir, owners)
    }

    /// 泵线程异步落库/广播:轮询等待事件帧。
    fn recv_event(rx: &mut broadcast::Receiver<SessionEventFrame>) -> SessionEvent {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match rx.try_recv() {
                Ok(frame) => return frame.event,
                Err(broadcast::error::TryRecvError::Empty) => {
                    assert!(Instant::now() < deadline, "event not delivered in time");
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("event channel: {error}"),
            }
        }
    }

    #[test]
    fn owner_round_trips_input_tool_output_effects_and_interactions() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let (_dir, owners) = test_owner_dir();
            let owner = owners
                .create_session(store::NewSession {
                    session_id: "SES-TEST".into(),
                    parent_session_id: None,
                    title: "owner 往返".into(),
                    title_source: Some("user".into()),
                    cwd: None,
                    model: None,
                    effort: None,
                    profile: None,
                    mode: None,
                    tool_mode: None,
                    ephemeral: false,
                })
                .await
                .expect("create session");
            let mut rx = owner.subscribe();

            // 完整输入:回合 + 输入块。
            owner
                .start_turn("T1", Some("IN1".into()), Some(TurnMode::Plan))
                .await
                .expect("start turn");
            owner
                .append_block(TimelineEntry::Input {
                    block_id: BlockId::new("B1"),
                    turn_id: Some(TurnId::new("T1")),
                    purpose: InputPurpose::TriggerTurn,
                    content: ContentValue::Inline {
                        text: "清理构建目录".into(),
                    },
                    attachments: vec![],
                    actor: ActorRef {
                        kind: ActorKind::User,
                        id: "user".into(),
                        display_name: None,
                    },
                })
                .await
                .expect("append input");

            assert!(matches!(
                recv_event(&mut rx),
                SessionEvent::TurnStarted { .. }
            ));
            assert!(matches!(
                recv_event(&mut rx),
                SessionEvent::ActivityChanged { .. }
            ));
            assert!(matches!(
                recv_event(&mut rx),
                SessionEvent::TimelineAppended { .. }
            ));

            // 工具生命周期:started → 实时输出 ×2 → 终态(结构化结果 + effects)。
            let lifecycle = owner.lifecycle();
            let run = lifecycle.started("C1", "shell", Some(r#"{"command":"ls"}"#.into()));
            let sender = run.exec_sender();
            for chunk in ["file-a\n", "file-b\n"] {
                sender.try_send(ExecProgressEvent {
                    tool_call_id: "C1".into(),
                    stream: ExecOutputStream::Stdout,
                    seq: 0,
                    chunk: chunk.into(),
                    bytes_total: 0,
                });
            }
            let metrics = ToolMetrics {
                started_at_ms: run.started_at_ms(),
                finished_at_ms: store::now_ms(),
                retry_count: 0,
                output_bytes: 14,
                progress_bytes_total: run.progress_totals().total_bytes(),
            };
            lifecycle.finished(
                run,
                ToolTerminal {
                    status: ToolCallStatus::Finished,
                    output_text: Some("file-a\nfile-b\n".into()),
                    output_truncated: false,
                    error: None,
                    metrics: Some(metrics),
                    code_delta: Some(CodeDelta {
                        lines_added: 3,
                        lines_removed: 1,
                        files_created: 0,
                        files_deleted: 0,
                        file: Some("src/main.rs".into()),
                    }),
                    effects: vec![ToolEffectInfo::SkillActivated {
                        name: "qaqh-core".into(),
                    }],
                },
            );

            assert!(matches!(
                recv_event(&mut rx),
                SessionEvent::ToolStarted { .. }
            ));
            for chunk in ["file-a\n", "file-b\n"] {
                match recv_event(&mut rx) {
                    SessionEvent::ToolOutputDelta { call_id, stream, text } => {
                        assert_eq!(call_id.as_ref(), "C1");
                        assert_eq!(stream, OutputStream::Stdout);
                        assert_eq!(text, chunk);
                    }
                    other => panic!("expected output delta, got {other:?}"),
                }
            }
            match recv_event(&mut rx) {
                SessionEvent::ToolFinished {
                    call_id,
                    status,
                    output,
                    metrics,
                    code_delta,
                    effects,
                    ..
                } => {
                    assert_eq!(call_id.as_ref(), "C1");
                    assert_eq!(status, ToolCallStatus::Finished);
                    assert!(output.is_some());
                    assert_eq!(metrics.expect("metrics").progress_bytes_total, 14);
                    assert_eq!(
                        code_delta.expect("code delta").lines_added,
                        3
                    );
                    assert_eq!(
                        effects,
                        vec![ToolEffectInfo::SkillActivated {
                            name: "qaqh-core".into(),
                        }]
                    );
                }
                other => panic!("expected tool finished, got {other:?}"),
            }

            // 交互:升起 → 应答。
            owner
                .interaction_requested(InteractionView {
                    interaction_id: InteractionId::new("I1"),
                    call_id: Some(ToolCallId::new("C1")),
                    turn_id: Some(TurnId::new("T1")),
                    kind: InteractionKind::Permission,
                    request: ContentValue::Inline {
                        text: "执行 rm -rf build/".into(),
                    },
                    choices: vec![],
                    expires_at_ms: None,
                    resolution: None,
                })
                .await
                .expect("interaction requested");
            assert!(matches!(
                recv_event(&mut rx),
                SessionEvent::InteractionRequested { .. }
            ));
            let resolved = owner
                .interaction_resolved(
                    &InteractionId::new("I1"),
                    Some(InteractionDecision::Approved),
                    InteractionEndStatus::Resolved,
                )
                .await
                .expect("resolve");
            assert!(resolved);
            match recv_event(&mut rx) {
                SessionEvent::InteractionResolved { decision, status, .. } => {
                    assert_eq!(decision, Some(InteractionDecision::Approved));
                    assert_eq!(status, InteractionEndStatus::Resolved);
                }
                other => panic!("expected interaction resolved, got {other:?}"),
            }

            // 回合终态 + 快照:DB 读模型与事件流一致。
            owner
                .finish_turn(
                    "T1",
                    "finished",
                    TurnOutcome::Finished {
                        terminal: qaqh_ringing_contract::fact::TurnTerminal::Completed,
                        usage: None,
                        error: None,
                    },
                )
                .await
                .expect("finish turn");
            assert!(matches!(
                recv_event(&mut rx),
                SessionEvent::TurnEnded { .. }
            ));

            let state = owner.snapshot().await.expect("snapshot");
            assert_eq!(state.turns.len(), 1);
            assert_eq!(state.timeline.len(), 1);
            assert_eq!(state.tools.len(), 1);
            let tool = &state.tools[0];
            assert_eq!(tool.call_id.as_ref(), "C1");
            assert_eq!(tool.status, ToolCallStatus::Finished);
            assert_eq!(
                tool.turn_id.as_ref().map(|turn| turn.as_ref()),
                Some("T1"),
                "工具落库时归属活跃回合"
            );
            assert_eq!(tool.code_delta.as_ref().expect("code delta").lines_added, 3);
            assert_eq!(tool.effects.len(), 1);
            assert!(state.interactions.is_empty(), "已应答交互离开 pending 列表");
            assert_eq!(state.activity, qaqh_ringing_contract::fact::ActivityState::Idle);
        });
    }
}
