//! owner 往返测试:P2 退出标准的四条流(完整输入/工具输出/结构化结果/effects)
//! 在一个会话 owner 上端到端走通(报告一 §12.5-2);契约 state/outcome 单一权威。

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use tokio::sync::broadcast;

    use qaqh_ringing_contract::events::{
        InteractionEndStatus, OutputStream, SessionEvent, SessionEventFrame,
    };
    use qaqh_ringing_contract::fact::{
        ActorKind, ActorRef, BlockId, ContentValue, InputPurpose, InteractionDecision,
        InteractionId, InteractionKind, ToolCallId, ToolCallOutcome, ToolMetrics, TurnId, TurnMode,
    };
    use qaqh_ringing_contract::state::{
        CodeDelta, InteractionView, TimelineEntry, ToolCallState, ToolEffectInfo, TurnOutcome,
    };
    use qaqh_tool_core::{ExecOutputStream, ExecProgressEvent};

    use crate::db::store;
    use crate::owner::{OwnerDirectory, ToolSettlement};

    fn test_owner_dir() -> (tempfile::TempDir, Arc<OwnerDirectory>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let owners =
            Arc::new(OwnerDirectory::open(dir.path().join("qaqh.db")).expect("open owners db"));
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
                    model_selection: None,
                    custom_tools: None,
                    skills: None,
                    frozen_annotation: None,
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
                    images: vec![],
                    inter_agent: None,
                    subagent_terminal: None,
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
                SessionEvent::TurnPhaseChanged {
                    phase: qaqh_ringing_contract::state::TurnPhase::Running,
                    ..
                }
            ));
            assert!(matches!(
                recv_event(&mut rx),
                SessionEvent::ActivityChanged { .. }
            ));
            assert!(matches!(
                recv_event(&mut rx),
                SessionEvent::TimelineAppended { .. }
            ));

            // 工具生命周期:running → 实时输出 ×2 → 终态(结构化结果 + effects)。
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
            // running 的首次迁移就是执行开始;不再有独立 tool_started 事件。
            match recv_event(&mut rx) {
                SessionEvent::ToolStateChanged {
                    call_id,
                    state: ToolCallState::Running,
                    ..
                } => assert_eq!(call_id.as_ref(), "C1"),
                other => panic!("expected tool state changed running, got {other:?}"),
            }
            for chunk in ["file-a\n", "file-b\n"] {
                match recv_event(&mut rx) {
                    SessionEvent::ToolOutputDelta {
                        call_id,
                        stream,
                        text,
                    } => {
                        assert_eq!(call_id.as_ref(), "C1");
                        assert_eq!(stream, OutputStream::Stdout);
                        assert_eq!(text, chunk);
                    }
                    other => panic!("expected live output delta, got {other:?}"),
                }
            }
            // 紧邻终态上报的输出也必须先于 ToolFinished。
            sender.try_send(ExecProgressEvent {
                tool_call_id: "C1".into(),
                stream: ExecOutputStream::Stderr,
                seq: 2,
                chunk: "warning\n".into(),
                bytes_total: 0,
            });
            let metrics = ToolMetrics {
                started_at_ms: run.started_at_ms(),
                finished_at_ms: store::now_ms(),
                retry_count: 0,
                output_bytes: 14,
                progress_bytes_total: run.progress_totals().total_bytes(),
            };
            lifecycle.finished(
                run,
                ToolSettlement {
                    call_id: "C1".into(),
                    tool_name: "shell".into(),
                    outcome: ToolCallOutcome::Completed,
                    output_text: Some("file-a\nfile-b\n".into()),
                    output_truncated: false,
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

            assert!(
                matches!(recv_event(&mut rx), SessionEvent::ToolOutputDelta {
                stream: OutputStream::Stderr, text, ..
            } if text == "warning\n")
            );
            match recv_event(&mut rx) {
                SessionEvent::ToolFinished {
                    call_id,
                    outcome,
                    output,
                    metrics,
                    code_delta,
                    effects,
                    ..
                } => {
                    assert_eq!(call_id.as_ref(), "C1");
                    assert_eq!(outcome, ToolCallOutcome::Completed);
                    assert!(output.is_some());
                    assert_eq!(metrics.expect("metrics").progress_bytes_total, 22);
                    assert_eq!(code_delta.expect("code delta").lines_added, 3);
                    assert_eq!(
                        effects,
                        vec![ToolEffectInfo::SkillActivated {
                            name: "qaqh-core".into(),
                        }]
                    );
                }
                other => panic!("expected tool finished, got {other:?}"),
            }
            // 后台进程仍持有 sender 时,终态不等待它退出,后续输出继续可见。
            sender.try_send(ExecProgressEvent {
                tool_call_id: "C1".into(),
                stream: ExecOutputStream::Stdout,
                seq: 3,
                chunk: "background\n".into(),
                bytes_total: 0,
            });
            assert!(
                matches!(recv_event(&mut rx), SessionEvent::ToolOutputDelta {
                stream: OutputStream::Stdout, text, ..
            } if text == "background\n")
            );
            drop(sender);

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
                SessionEvent::InteractionResolved {
                    decision, status, ..
                } => {
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
            assert_eq!(
                tool.state,
                ToolCallState::Finished {
                    outcome: ToolCallOutcome::Completed
                }
            );
            assert_eq!(
                tool.turn_id.as_ref().map(|turn| turn.as_ref()),
                Some("T1"),
                "工具落库时归属活跃回合"
            );
            assert_eq!(tool.code_delta.as_ref().expect("code delta").lines_added, 3);
            assert_eq!(tool.effects.len(), 1);
            assert!(state.interactions.is_empty(), "已应答交互离开 pending 列表");
            assert_eq!(
                state.activity,
                qaqh_ringing_contract::fact::ActivityState::Idle
            );
        });
    }

    /// B01 边界:首帧 seq = 0;快照水位 = "下一个将分配的 seq";水位读取后的
    /// 下一帧 seq **等于**水位(相等边界必须被客户端处理,不是跳过)。
    /// 提交回执语义:submit_request 完成时"落库 + emit"已在泵内完成。
    #[test]
    fn watermark_is_exclusive_and_first_frame_seq_is_zero() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let (_dir, owners) = test_owner_dir();
            let owner = owners
                .create_session(store::NewSession {
                    session_id: "SES-WM".into(),
                    parent_session_id: None,
                    title: String::new(),
                    title_source: None,
                    cwd: None,
                    model: None,
                    effort: None,
                    profile: None,
                    mode: None,
                    tool_mode: None,
                    model_selection: None,
                    custom_tools: None,
                    skills: None,
                    frozen_annotation: None,
                    ephemeral: false,
                })
                .await
                .expect("create session");
            let mut rx = owner.subscribe();
            let input = |id: &str| TimelineEntry::Input {
                block_id: BlockId::new(id),
                turn_id: None,
                purpose: InputPurpose::TriggerTurn,
                content: ContentValue::Inline { text: id.into() },
                attachments: vec![],
                images: vec![],
                inter_agent: None,
                subagent_terminal: None,
                actor: ActorRef {
                    kind: ActorKind::User,
                    id: "user".into(),
                    display_name: None,
                },
            };

            // 首帧 seq = 0(0 基)。
            owner.append_block(input("B1")).await.expect("append B1");
            let first = rx.try_recv().expect("first frame");
            assert_eq!(first.seq, 0, "首帧 seq 必须 = 0(exclusive 水位语义)");
            assert_eq!(owner.current_seq(), 1);

            // 快照:水位 = 已发出事件数,且 DB 状态包含全部已提交效果。
            for id in ["B2", "B3"] {
                owner.append_block(input(id)).await.expect("append");
            }
            // 排空订阅缓冲(B2/B3 的帧),再取快照。
            assert_eq!(rx.try_recv().expect("B2 frame").seq, 1);
            assert_eq!(rx.try_recv().expect("B3 frame").seq, 2);
            let state = owner.snapshot().await.expect("snapshot");
            assert_eq!(state.seq, 3);
            assert_eq!(state.timeline.len(), 3, "快照覆盖全部已提交事件");
            // 空闲读水位不变:事件不因快照而分配 seq。
            assert_eq!(owner.current_seq(), 3);

            // 相等边界:快照后发出的下一帧 seq == 快照水位。
            owner.append_block(input("B4")).await.expect("append B4");
            let boundary = rx.try_recv().expect("boundary frame");
            assert_eq!(
                boundary.seq, state.seq,
                "水位后的首帧 seq 必须等于水位(客户端按 >= 处理)"
            );
        });
    }

    /// B03:declare 落库 + ToolDeclared 事件(工具身份与最终参数),与
    /// bootstrap `tools` 视图同形状。
    #[test]
    fn declare_tool_call_emits_identity_and_lands_in_snapshot() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let (_dir, owners) = test_owner_dir();
            let owner = owners
                .create_session(store::NewSession {
                    session_id: "SES-DECLARE".into(),
                    parent_session_id: None,
                    title: String::new(),
                    title_source: None,
                    cwd: None,
                    model: None,
                    effort: None,
                    profile: None,
                    mode: None,
                    tool_mode: None,
                    model_selection: None,
                    custom_tools: None,
                    skills: None,
                    frozen_annotation: None,
                    ephemeral: false,
                })
                .await
                .expect("create session");
            let mut rx = owner.subscribe();
            owner
                .start_turn("T1", None, None)
                .await
                .expect("start turn");
            owner
                .declare_tool_call(
                    &ToolCallId::new("C1"),
                    &TurnId::new("T1"),
                    "shell",
                    Some(serde_json::json!({ "command": "ls" })),
                )
                .await
                .expect("declare");
            match recv_event(&mut rx) {
                SessionEvent::TurnStarted { .. } => {}
                other => panic!("expected turn started, got {other:?}"),
            }
            match recv_event(&mut rx) {
                SessionEvent::TurnPhaseChanged { .. } => {}
                other => panic!("expected turn phase changed, got {other:?}"),
            }
            match recv_event(&mut rx) {
                SessionEvent::ActivityChanged { .. } => {}
                other => panic!("expected activity changed, got {other:?}"),
            }
            match recv_event(&mut rx) {
                SessionEvent::ToolDeclared {
                    call_id,
                    tool_name,
                    args,
                    ..
                } => {
                    assert_eq!(call_id.as_ref(), "C1");
                    assert_eq!(tool_name, "shell");
                    assert_eq!(args, Some(serde_json::json!({ "command": "ls" })));
                }
                other => panic!("expected tool declared, got {other:?}"),
            }
            let state = owner.snapshot().await.expect("snapshot");
            let tool = state.tools.iter().find(|t| t.call_id.as_ref() == "C1");
            let tool = tool.expect("declared call in snapshot");
            assert_eq!(tool.tool_name.as_deref(), Some("shell"));
            assert_eq!(tool.state, ToolCallState::Declared);
            assert_eq!(tool.args, Some(serde_json::json!({ "command": "ls" })));
        });
    }

    /// B04:rename 的唯一写口 = owner.set_title → 业务行 + SessionMetaChanged。
    #[test]
    fn set_title_updates_row_and_emits_meta_changed() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let (_dir, owners) = test_owner_dir();
            let owner = owners
                .create_session(store::NewSession {
                    session_id: "SES-TITLE".into(),
                    parent_session_id: None,
                    title: "old".into(),
                    title_source: None,
                    cwd: None,
                    model: None,
                    effort: None,
                    profile: None,
                    mode: None,
                    tool_mode: None,
                    model_selection: None,
                    custom_tools: None,
                    skills: None,
                    frozen_annotation: None,
                    ephemeral: false,
                })
                .await
                .expect("create session");
            let mut rx = owner.subscribe();
            owner.set_title("renamed").await.expect("set title");
            match recv_event(&mut rx) {
                SessionEvent::SessionMetaChanged { title } => {
                    assert_eq!(title.as_deref(), Some("renamed"));
                }
                other => panic!("expected session meta changed, got {other:?}"),
            }
            let state = owner.snapshot().await.expect("snapshot");
            assert_eq!(state.title.as_deref(), Some("renamed"));
        });
    }

    /// D7:外部生产者经 owner.publish 进入同一单流定序(driver 席位事件)。
    /// publish 经泵队列;submit 回执语义同样覆盖(发送完成前事件已定序)。
    #[test]
    fn published_events_share_the_single_stream_sequence() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let (_dir, owners) = test_owner_dir();
            let owner = owners
                .create_session(store::NewSession {
                    session_id: "SES-DRIVER".into(),
                    parent_session_id: None,
                    title: String::new(),
                    title_source: None,
                    cwd: None,
                    model: None,
                    effort: None,
                    profile: None,
                    mode: None,
                    tool_mode: None,
                    model_selection: None,
                    custom_tools: None,
                    skills: None,
                    frozen_annotation: None,
                    ephemeral: false,
                })
                .await
                .expect("create session");
            let mut rx = owner.subscribe();
            owner.publish(SessionEvent::DriverChanged {
                driver: Some(qaqh_ringing_contract::state::DriverState {
                    holder: Some("lease-a".into()),
                    epoch: 1,
                }),
            });
            let frame = {
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    match rx.try_recv() {
                        Ok(frame) => break frame,
                        Err(broadcast::error::TryRecvError::Empty) => {
                            assert!(Instant::now() < deadline, "event not delivered in time");
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("event channel: {error}"),
                    }
                }
            };
            assert_eq!(frame.seq, 0);
            match frame.event {
                SessionEvent::DriverChanged { driver } => {
                    assert_eq!(driver.map(|d| d.epoch), Some(1));
                }
                other => panic!("expected driver changed, got {other:?}"),
            }
        });
    }
}
