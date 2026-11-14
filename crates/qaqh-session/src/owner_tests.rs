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
        InteractionId, InteractionKind, ToolCallId, ToolCallOutcome, ToolMetrics, TurnId,
    };
    use qaqh_ringing_contract::state::{
        CodeDelta, InteractionView, TimelineEntry, ToolCallState, ToolEffectInfo, TurnOutcome,
    };
    use qaqh_tool_core::{ExecOutputStream, ExecProgressEvent};

    use crate::db::store;
    use crate::owner::{
        OwnerDirectory, ToolSettlement, append_exec_log, exec_output_log_path,
        read_exec_output_log_range,
    };

    #[test]
    fn deleting_a_session_tree_removes_business_rows_without_resource_directories() {
        let (_dir, owners) = test_owner_dir();
        owners.ensure_session("root", None, false).expect("root");
        owners
            .ensure_session("child", Some("root"), true)
            .expect("child");
        owners.ensure_session("other", None, false).expect("other");
        owners
            .db()
            .write_blocking(|conn| {
                store::put_session_content(conn, "child", "content", "text/plain", b"body", false)
            })
            .expect("content");
        let removed = owners.delete_session_tree("root").expect("delete tree");
        assert_eq!(removed.len(), 2);
        owners
            .db()
            .read_blocking(|conn| {
                assert!(!store::session_exists(conn, "root")?);
                assert!(!store::session_exists(conn, "child")?);
                assert!(store::session_exists(conn, "other")?);
                assert!(store::get_session_content(conn, "child", "content")?.is_none());
                Ok::<_, crate::db::DbError>(())
            })
            .expect("business state");
    }

    fn test_owner_dir() -> (tempfile::TempDir, Arc<OwnerDirectory>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let owners =
            Arc::new(OwnerDirectory::open(dir.path().join("qaqh.db")).expect("open owners db"));
        (dir, owners)
    }

    #[test]
    fn exec_output_log_path_derives_from_db_dir_and_appends_in_order() {
        let (dir, owners) = test_owner_dir();
        let session_id = "ses_TESTLOG";
        let exec_id = "execs_01TESTLOGEXECSESSION0000000000";
        let path = exec_output_log_path(owners.db(), session_id, exec_id)
            .expect("data_dir derivable from db path");
        assert_eq!(
            path,
            dir.path()
                .join("sessions")
                .join(session_id)
                .join("exec")
                .join(format!("{exec_id}.log"))
        );
        append_exec_log(&path, "中").expect("append chunk 1");
        append_exec_log(&path, "😀tail").expect("append chunk 2");
        let text = std::fs::read_to_string(&path).expect("read log");
        assert_eq!(text, "中😀tail");
        // 字节口径 = capture 解码字节口径,与 next_offset 同一计量(中=3,😀=4,tail=4)。
        assert_eq!(text.len(), 11);
    }

    #[test]
    fn exec_output_range_read_clamps_watermark_and_respects_char_boundaries() {
        let (_dir, owners) = test_owner_dir();
        let session_id = "ses_TESTLOG";
        let exec_id = "execs_01TESTLOGEXECSESSION0000000000";
        let path = exec_output_log_path(owners.db(), session_id, exec_id)
            .expect("data_dir derivable from db path");
        append_exec_log(&path, "中").expect("append chunk 1");
        append_exec_log(&path, "😀tail").expect("append chunk 2");
        let db = owners.db();

        // 水位钳制:watermark=7 只到 "中😀",文件里更靠后的 tail 不越过水位。
        let (next, text) =
            read_exec_output_log_range(db, session_id, exec_id, 7, 0, 1 << 20)
                .expect("range behind watermark");
        assert_eq!((next, text.as_str()), (7, "中😀"));

        // max_bytes 切进多字节字符:终点向前取整到字符边界(😀 的尾字节补全)。
        let (next, text) = read_exec_output_log_range(db, session_id, exec_id, 11, 0, 4)
            .expect("range across multibyte char");
        assert_eq!((next, text.as_str()), (7, "中😀"));

        // 中段续读到末尾:恰好吐完剩余字符。
        let (next, text) = read_exec_output_log_range(db, session_id, exec_id, 11, 7, 4)
            .expect("range continuation");
        assert_eq!((next, text.as_str()), (11, "tail"));

        // 追平:from == watermark ⇒ 空文本、游标原地。
        let (next, text) = read_exec_output_log_range(db, session_id, exec_id, 11, 11, 256)
            .expect("range caught up");
        assert_eq!((next, text.as_str()), (11, ""));

        // from 切进多字节字符:当场报错,不静默对齐(宪规 5)。
        let error = read_exec_output_log_range(db, session_id, exec_id, 11, 1, 4)
            .expect_err("mid-char from_offset is an error");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);

        // 文件短于水位(append 曾丢字节):按实有字节返回,游标停在实有末尾。
        let (next, text) = read_exec_output_log_range(db, session_id, exec_id, 20, 0, 1 << 20)
            .expect("range with lost bytes");
        assert_eq!((next, text.as_str()), (11, "中😀tail"));

        // 日志缺席:空文本、游标原地(旧会话无日志,如实不假装)。
        let (next, text) = read_exec_output_log_range(
            db,
            session_id,
            "execs_01NOLOG000000000000000000000",
            11,
            0,
            256,
        )
        .expect("range without log");
        assert_eq!((next, text.as_str()), (0, ""));
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
                    tool_mode: None,
                    model_selection: None,
                    custom_tools: None,
                    frozen_annotation: None,
                    ephemeral: false,
                })
                .await
                .expect("create session");
            let mut rx = owner.subscribe();

            // 完整输入:回合 + 输入块。
            owner
                .start_turn("T1", Some("IN1".into()))
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
                    effects: vec![ToolEffectInfo::SubagentSpawned {
                        child_session_id: "child-1".into(),
                        name: "worker".into(),
                        task_text: "task".into(),
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
                        vec![ToolEffectInfo::SubagentSpawned {
                            child_session_id: "child-1".into(),
                            name: "worker".into(),
                            task_text: "task".into(),
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
                    tool_mode: None,
                    model_selection: None,
                    custom_tools: None,
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

    /// 尾窗快照(仅 bootstrap 读口):timeline 裁剪到窗口,`timeline_from_ord`
    /// 标记边界;seq 水位与全量快照一致,事件序号空间不因尾窗改变(B01)。
    #[test]
    fn snapshot_tailed_trims_timeline_without_touching_watermark() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let (_dir, owners) = test_owner_dir();
            let owner = owners
                .create_session(store::NewSession {
                    session_id: "SES-TAIL".into(),
                    parent_session_id: None,
                    title: String::new(),
                    title_source: None,
                    cwd: None,
                    model: None,
                    tool_mode: None,
                    model_selection: None,
                    custom_tools: None,
                    frozen_annotation: None,
                    ephemeral: false,
                })
                .await
                .expect("create session");
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
            for id in ["B1", "B2", "B3"] {
                owner.append_block(input(id)).await.expect("append");
            }
            let full = owner.snapshot().await.expect("full snapshot");
            assert_eq!(full.timeline.len(), 3);
            assert_eq!(full.timeline_from_ord, None);
            assert_eq!(full.seq, 3);

            let tailed = owner.snapshot_tailed(2).await.expect("tailed snapshot");
            assert_eq!(tailed.timeline.len(), 2);
            // 块 ord 从 1 起:窗口 = [B2,B3],B1 在界外;输入块无回合锚点,
            // 边界 = 窗口首块 ord,不做回合回退。
            assert_eq!(tailed.timeline_from_ord, Some(2), "窗口 [B2,B3],B1 在界外");
            assert_eq!(tailed.seq, 3, "水位不因尾窗改变");
            assert_eq!(owner.current_seq(), 3, "空闲读水位不变");
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
                    tool_mode: None,
                    model_selection: None,
                    custom_tools: None,
                    frozen_annotation: None,
                    ephemeral: false,
                })
                .await
                .expect("create session");
            let mut rx = owner.subscribe();
            owner
                .start_turn("T1", None)
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

    /// ord 回归:reasoning/answer 块的权威落库序必须随 live 帧与 bootstrap 快照
    /// 同携,且 reasoning.ord < answer.ord。客户端据此排序而非到达序——重连丢帧后
    /// 迟到的 reasoning seal 不再插到 answer 之下(思考链落到答案之下的根因修复)。
    #[test]
    fn assistant_block_ord_authoritative_in_live_and_snapshot() {
        use qaqh_ringing_contract::fact::AssistantBlockKind;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let (_dir, owners) = test_owner_dir();
            let owner = owners
                .create_session(store::NewSession {
                    session_id: "SES-ORD".into(),
                    parent_session_id: None,
                    title: String::new(),
                    title_source: None,
                    cwd: None,
                    model: None,
                    tool_mode: None,
                    model_selection: None,
                    custom_tools: None,
                    frozen_annotation: None,
                    ephemeral: false,
                })
                .await
                .expect("create session");
            let mut rx = owner.subscribe();
            owner.start_turn("T1", None).await.expect("start turn");
            // 排空 start_turn 伴随帧(turn_started/phase/activity)。
            while rx.try_recv().is_ok() {}

            let block = |id: &str, kind: AssistantBlockKind, text: &str| {
                TimelineEntry::AssistantBlock {
                    block_id: BlockId::new(id),
                    turn_id: TurnId::new("T1"),
                    ord: 0, // 占位:落库时由 append_block 权威回填
                    block_kind: kind,
                    content: ContentValue::Inline { text: text.into() },
                    model: "m".into(),
                    usage: None,
                    generated_ms: None,
                    opaque: vec![],
                }
            };
            owner
                .append_block(block("R1", AssistantBlockKind::Reasoning, "think"))
                .await
                .expect("append reasoning");
            owner
                .append_block(block("A1", AssistantBlockKind::Answer, "answer"))
                .await
                .expect("append answer");

            let reasoning_ord = match recv_event(&mut rx) {
                SessionEvent::TimelineAppended {
                    entry: TimelineEntry::AssistantBlock { ord, block_kind: AssistantBlockKind::Reasoning, .. },
                } => ord,
                other => panic!("expected reasoning timeline_appended, got {other:?}"),
            };
            let answer_ord = match recv_event(&mut rx) {
                SessionEvent::TimelineAppended {
                    entry: TimelineEntry::AssistantBlock { ord, block_kind: AssistantBlockKind::Answer, .. },
                } => ord,
                other => panic!("expected answer timeline_appended, got {other:?}"),
            };
            assert!(reasoning_ord > 0, "live 帧必须回填权威 ord, got {reasoning_ord}");
            assert!(
                reasoning_ord < answer_ord,
                "reasoning.ord 必须 < answer.ord(落库序), got {reasoning_ord} vs {answer_ord}"
            );

            // bootstrap 快照同携 ord,且与 live 帧一致、reasoning 在前。
            let state = owner.snapshot().await.expect("snapshot");
            let ords: Vec<i64> = state
                .timeline
                .iter()
                .filter_map(|entry| match entry {
                    TimelineEntry::AssistantBlock { ord, .. } => Some(*ord),
                    _ => None,
                })
                .collect();
            assert_eq!(
                ords,
                vec![reasoning_ord, answer_ord],
                "快照 ord 必须与 live 帧一致且 reasoning 在前"
            );
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
                    tool_mode: None,
                    model_selection: None,
                    custom_tools: None,
                    frozen_annotation: None,
                    ephemeral: false,
                })
                .await
                .expect("create session");
            let mut rx = owner.subscribe();
            owner
                .update_metadata(store::SessionColumnPatch {
                    title: Some("renamed".into()),
                    ..Default::default()
                })
                .expect("set title");
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
                    tool_mode: None,
                    model_selection: None,
                    custom_tools: None,
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

    /// 撤回的落库面:块按会话 `ord` 硬截断,工具调用与未决交互随行消失,事件流
    /// 只追加**一条**截断事实;目标回合不存在或与活动回合冲突时既不写也不发。
    #[test]
    fn truncate_from_turn_hard_deletes_history_and_publishes_one_fact() {
        fn new_session(id: &str) -> store::NewSession {
            store::NewSession {
                session_id: id.into(),
                parent_session_id: None,
                title: id.into(),
                title_source: Some("user".into()),
                cwd: None,
                model: None,
                tool_mode: None,
                model_selection: None,
                custom_tools: None,
                frozen_annotation: None,
                ephemeral: false,
            }
        }
        fn input_entry(block: &str, turn: &str) -> TimelineEntry {
            TimelineEntry::Input {
                block_id: BlockId::new(block),
                turn_id: Some(TurnId::new(turn)),
                purpose: InputPurpose::TriggerTurn,
                content: ContentValue::Inline {
                    text: format!("第 {turn} 轮的输入"),
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
            }
        }
        fn finished_outcome() -> TurnOutcome {
            TurnOutcome::Finished {
                terminal: qaqh_ringing_contract::fact::TurnTerminal::Completed,
                usage: None,
                error: None,
            }
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let (_dir, owners) = test_owner_dir();
            let owner = owners
                .create_session(new_session("SES-TRUNC"))
                .await
                .expect("create session");

            // 三个已收尾的回合,每回合一个输入块。
            for (turn, block) in [("T1", "B1"), ("T2", "B2"), ("T3", "B3")] {
                owner.start_turn(turn, None).await.expect("start turn");
                owner
                    .append_block(input_entry(block, turn))
                    .await
                    .expect("append block");
                owner
                    .finish_turn(turn, "finished", finished_outcome())
                    .await
                    .expect("finish turn");
            }
            // T2 额外带一个工具调用与一条未决交互:撤回它们所在的回合时必须随行消失,
            // 否则 bootstrap 的 `tools`/`interactions` 会指向一条不存在的历史。
            owners
                .db()
                .write_blocking(|conn| {
                    store::upsert_tool_call(
                        conn,
                        &store::ToolCallUpsert {
                            call_id: "C2".into(),
                            session_id: "SES-TRUNC".into(),
                            turn_id: Some("T2".into()),
                            tool_name: "read".into(),
                            args_json: None,
                            state_json: serde_json::to_string(&ToolCallState::Declared)
                                .expect("state"),
                            state_kind: "declared",
                        },
                    )
                })
                .expect("tool row");
            owners
                .db()
                .write_blocking(|conn| {
                    store::create_interaction(
                        conn,
                        &store::NewInteraction {
                            interaction_id: "I2".into(),
                            session_id: "SES-TRUNC".into(),
                            turn_id: Some("T2".into()),
                            call_id: None,
                            kind: "permission".into(),
                            payload_json: "{}".into(),
                        },
                    )
                })
                .expect("interaction row");

            let mut rx = owner.subscribe();
            let removed = owner.truncate_from_turn("T2").expect("truncate");
            assert_eq!(
                removed
                    .iter()
                    .map(|turn| turn.as_ref().to_string())
                    .collect::<Vec<_>>(),
                vec!["T2".to_string(), "T3".to_string()],
                "撤回含自身:目标回合与其后每个回合都要现身"
            );
            match recv_event(&mut rx) {
                SessionEvent::TimelineTruncated {
                    from_turn_id,
                    removed_turn_ids,
                } => {
                    assert_eq!(from_turn_id.as_ref(), "T2");
                    assert_eq!(removed_turn_ids.len(), 2);
                }
                other => panic!("expected timeline truncated, got {other:?}"),
            }

            // 重连客户端的读模型只剩 T1。
            let state = owner.snapshot().await.expect("snapshot");
            assert_eq!(state.turns.len(), 1);
            assert_eq!(state.timeline.len(), 1);
            assert!(state.tools.is_empty(), "被删回合的工具调用不得留在快照里");
            assert!(
                state.interactions.is_empty(),
                "被删回合的未决交互不得留在快照里"
            );

            // 同一 turn_id 第二次撤回:回合已经不在了,必须当场报错而不是「什么都没删」的假成功。
            let error = owner
                .truncate_from_turn("T2")
                .expect_err("already truncated");
            assert!(
                matches!(error, crate::db::DbError::TurnNotFound { .. }),
                "{error}"
            );

            // 活动回合冲突:先清掉 T4 开场的帧,再证明被拒的撤回一帧都不追加。
            owner.start_turn("T4", None).await.expect("start T4");
            let error = owner
                .truncate_from_turn("T1")
                .expect_err("active turn must block truncate");
            assert!(
                matches!(error, crate::db::DbError::TruncateConflict { .. }),
                "{error}"
            );
            let mut frames = 0;
            while let Ok(frame) = rx.try_recv() {
                assert!(
                    !matches!(frame.event, SessionEvent::TimelineTruncated { .. }),
                    "被拒的撤回不得留下截断帧"
                );
                frames += 1;
            }
            assert_eq!(frames, 3, "只应有 T4 开场三帧");
            let state = owner.snapshot().await.expect("snapshot after reject");
            assert_eq!(state.turns.len(), 2, "T1 仍在,新开的 T4 也在");
            assert_eq!(state.timeline.len(), 1, "拒绝不得动任何块");
        });
    }
}
