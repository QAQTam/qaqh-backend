#[cfg(test)]
mod tests {
    use super::super::{Db, store, view};
    use qaqh_ringing_contract::fact::{ContentValue, ToolCallId, ToolCallOutcome, TurnId};
    use qaqh_ringing_contract::state::{TimelineEntry, ToolCallState};

    fn test_db() -> Db {
        let dir = tempfile::tempdir().expect("tempdir").keep();
        Db::open(dir.join("qaqh.db")).expect("open db")
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
    }

    #[test]
    fn settings_revision_is_checked_with_the_session_write() {
        let db = test_db();
        db.write_blocking(|conn| {
            store::create_session(
                conn,
                &store::NewSession {
                    session_id: "SES-CAS".into(),
                    parent_session_id: None,
                    title: "CAS".into(),
                    title_source: None,
                    cwd: None,
                    model: None,
                    tool_mode: None,
                    model_selection: None,
                    custom_tools: None,
                    frozen_annotation: None,
                    ephemeral: false,
                },
            )
        })
        .expect("create session");

        let changed = db
            .write_blocking(|conn| {
                store::update_session_columns(
                    conn,
                    "SES-CAS",
                    &store::SessionColumnPatch {
                        tool_mode: Some("minimal".into()),
                        archived: Some(true),
                        expected_settings_revision: Some(0),
                        increment_settings_revision: true,
                        ..Default::default()
                    },
                )
            })
            .expect("first settings write");
        assert_eq!(changed, 1);

        let stale = db
            .write_blocking(|conn| {
                store::update_session_columns(
                    conn,
                    "SES-CAS",
                    &store::SessionColumnPatch {
                        tool_mode: Some("standard".into()),
                        expected_settings_revision: Some(0),
                        increment_settings_revision: true,
                        ..Default::default()
                    },
                )
            })
            .expect("stale write is a conflict, not a database error");
        assert_eq!(stale, 0);

        let row = db
            .read_blocking(|conn| store::session_row(conn, "SES-CAS"))
            .expect("read session")
            .expect("session exists");
        assert_eq!(row.tool_mode.as_deref(), Some("minimal"));
        assert!(row.archived);
        assert_eq!(row.settings_revision, 1);
    }

    #[test]
    fn write_then_read_round_trips_session_state() {
        let rt = runtime();
        rt.block_on(async {
            let db = test_db();

            // 建会话 + 回合 + 块 + 工具 + 交互,一个写闭包内完成(短事务)。
            db.write(|conn| {
                store::create_session(
                    conn,
                    &store::NewSession {
                        session_id: "SES1".into(),
                        parent_session_id: None,
                        title: "测试会话".into(),
                        title_source: Some("user".into()),
                        cwd: None,
                        model: None,
                        tool_mode: None,
                        model_selection: None,
                        custom_tools: None,
                        frozen_annotation: None,
                        ephemeral: false,
                    },
                )?;
                store::start_turn(conn, "T1", "SES1", Some("IN1"))?;
                store::append_block(
                    conn,
                    &store::NewBlock {
                        block_id: "B1".into(),
                        session_id: "SES1".into(),
                        turn_id: Some("T1".into()),
                        kind: "input",
                        text: Some("你好".into()),
                        call_id: None,
                        payload_json: serde_json::to_string(&TimelineEntry::Input {
                            block_id: qaqh_ringing_contract::fact::BlockId::new("B1"),
                            turn_id: Some(qaqh_ringing_contract::fact::TurnId::new("T1")),
                            purpose: Default::default(),
                            content: ContentValue::Inline {
                                text: "你好".into(),
                            },
                            attachments: vec![],
                            images: vec![],
                            inter_agent: None,
                            subagent_terminal: None,
                            actor: qaqh_ringing_contract::fact::ActorRef {
                                kind: qaqh_ringing_contract::fact::ActorKind::User,
                                id: "user".into(),
                                display_name: None,
                            },
                        })
                        .expect("serialize"),
                    },
                )?;
                store::upsert_tool_call(
                    conn,
                    &store::ToolCallUpsert {
                        call_id: "C1".into(),
                        session_id: "SES1".into(),
                        turn_id: Some("T1".into()),
                        tool_name: "shell".into(),
                        args_json: Some(r#"{"kind":"inline","data":{"text":"ls"}}"#.into()),
                        state_json: serde_json::to_string(&ToolCallState::Running)
                            .expect("serialize running state"),
                        state_kind: "running",
                    },
                )?;
                store::create_interaction(
                    conn,
                    &store::NewInteraction {
                        interaction_id: "I1".into(),
                        session_id: "SES1".into(),
                        turn_id: Some("T1".into()),
                        call_id: Some("C1".into()),
                        kind: r#""permission""#.into(),
                        payload_json: serde_json::to_string(
                            &qaqh_ringing_contract::state::InteractionView {
                                interaction_id: qaqh_ringing_contract::fact::InteractionId::new(
                                    "I1",
                                ),
                                call_id: Some(qaqh_ringing_contract::fact::ToolCallId::new("C1")),
                                turn_id: Some(qaqh_ringing_contract::fact::TurnId::new("T1")),
                                kind: qaqh_ringing_contract::fact::InteractionKind::Permission,
                                request: ContentValue::Inline {
                                    text: "rm -rf /".into(),
                                },
                                choices: vec![],
                                expires_at_ms: None,
                                resolution: None,
                            },
                        )
                        .expect("serialize interaction"),
                    },
                )?;
                Ok::<(), rusqlite::Error>(())
            })
            .await
            .expect("write transaction");

            // 读回:活动状态、timeline、运行中工具、待决交互全部就位。
            let state = db
                .read(|conn| view::load_session_state(conn, "SES1"))
                .await
                .expect("load state");
            assert_eq!(
                state.session_id.as_ref().map(|s| s.0.as_str()),
                Some("SES1")
            );
            assert_eq!(state.turns.len(), 1);
            assert!(state.turns[0].outcome.is_none());
            assert_eq!(state.timeline.len(), 1);
            assert!(matches!(&state.timeline[0], TimelineEntry::Input { .. }));
            assert_eq!(state.tools.len(), 1);
            assert_eq!(state.tools[0].state, ToolCallState::Running);
            assert_eq!(state.interactions.len(), 1);

            // 工具完成 + 交互应答:状态迁移反映到下一次读取。
            db.write(|conn| {
                store::finish_tool_call(
                    conn,
                    &store::ToolCallFinish {
                        call_id: "C1".into(),
                        session_id: "SES1".into(),
                        turn_id: Some("T1".into()),
                        tool_name: "shell".into(),
                        state_json: serde_json::to_string(&ToolCallState::Finished {
                            outcome: ToolCallOutcome::Completed,
                        })
                        .expect("serialize finished state"),
                        exec_session_id: None,
                        output_text: Some("file-a\nfile-b".into()),
                        output_ref: None,
                        output_truncated: false,
                        meta_json: None,
                        effects_json: None,
                        code_delta_json: None,
                    },
                )?;
                assert!(store::resolve_interaction(
                    conn,
                    "I1",
                    "resolved",
                    r#""approved""#
                )?);
                Ok::<(), rusqlite::Error>(())
            })
            .await
            .expect("settle");

            let state = db
                .read(|conn| view::load_session_state(conn, "SES1"))
                .await
                .expect("reload");
            assert_eq!(
                state.tools[0].state,
                ToolCallState::Finished {
                    outcome: ToolCallOutcome::Completed
                }
            );
            assert!(
                state.interactions.is_empty(),
                "resolved interaction leaves pending list"
            );

            let source = db
                .read(|conn| view::load_session_turn(conn, "SES1", "T1"))
                .await
                .expect("load source turn")
                .expect("source turn exists");
            assert_eq!(
                source.turn.turn_id,
                qaqh_ringing_contract::fact::TurnId::new("T1")
            );
            assert_eq!(source.timeline.len(), 1);
            assert_eq!(source.tools.len(), 1);
            assert_eq!(
                source.tools[0].output,
                Some(ContentValue::Inline {
                    text: "file-a\nfile-b".into()
                })
            );
            let missing = db
                .read(|conn| view::load_session_turn(conn, "SES1", "T-MISSING"))
                .await
                .expect("query missing turn");
            assert!(missing.is_none());

            // 列表分页直查业务表。
            let rows = db
                .read(|conn| store::list_sessions(conn, false, 10))
                .await
                .expect("list");
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].title, "测试会话");
        });
    }

    /// ADR-0018 反证闸门：子代理管理面终态必须读回合 outcome 的 terminal，
    /// 不得把 status='finished' 一律投影成 Completed——provider 失败/超时取消
    /// 的回合同样是 'finished'，旧投影让失败子代理在管理面显示"已完成"。
    #[test]
    fn subagent_terminal_status_reflects_turn_outcome_not_just_status() {
        use qaqh_ringing_contract::fact::{SubagentTerminalStatus, TurnError, TurnTerminal};
        use qaqh_ringing_contract::state::{ToolEffectInfo, TurnOutcome};

        let db = test_db();
        db.write_blocking(|conn| {
            let mk_session = |conn: &mut rusqlite::Connection, id: &str, parent: Option<&str>| {
                store::create_session(
                    conn,
                    &store::NewSession {
                        session_id: id.into(),
                        parent_session_id: parent.map(str::to_string),
                        title: id.into(),
                        title_source: None,
                        cwd: None,
                        model: None,
                        tool_mode: None,
                        model_selection: None,
                        custom_tools: None,
                        frozen_annotation: None,
                        ephemeral: false,
                    },
                )
            };
            mk_session(conn, "SES-P", None)?;
            for child in ["SES-FAILED", "SES-CANCELLED", "SES-OK", "SES-ACTIVE", "SES-INTERRUPTED"] {
                mk_session(conn, child, Some("SES-P"))?;
            }
            // 父会话为每个子代理留一条 spawn 工具调用（effects 是血缘权威）。
            for (i, child) in ["SES-FAILED", "SES-CANCELLED", "SES-OK", "SES-ACTIVE", "SES-INTERRUPTED"]
                .iter()
                .enumerate()
            {
                let call_id = format!("C-SPAWN-{i}");
                store::upsert_tool_call(
                    conn,
                    &store::ToolCallUpsert {
                        call_id: call_id.clone(),
                        session_id: "SES-P".into(),
                        turn_id: None,
                        tool_name: "spawn_subagent".into(),
                        args_json: None,
                        state_json: serde_json::to_string(&ToolCallState::Running).unwrap(),
                        state_kind: "running",
                    },
                )?;
                store::finish_tool_call(
                    conn,
                    &store::ToolCallFinish {
                        call_id,
                        session_id: "SES-P".into(),
                        turn_id: None,
                        tool_name: "spawn_subagent".into(),
                        state_json: serde_json::to_string(&ToolCallState::Finished {
                            outcome: ToolCallOutcome::Completed,
                        })
                        .unwrap(),
                        exec_session_id: None,
                        output_text: None,
                        output_ref: None,
                        output_truncated: false,
                        meta_json: None,
                        effects_json: Some(
                            serde_json::to_string(&vec![ToolEffectInfo::SubagentSpawned {
                                child_session_id: child.to_string(),
                                name: format!("child_{i}"),
                                task_text: "task".into(),
                            }])
                            .unwrap(),
                        ),
                        code_delta_json: None,
                    },
                )?;
            }
            // 子会话回合终态：三种 finished + 一种 active + 一种 interrupted。
            let settle = |conn: &mut rusqlite::Connection,
                          session: &str,
                          turn: &str,
                          status: &str,
                          outcome: Option<TurnOutcome>|
             -> rusqlite::Result<()> {
                store::start_turn(conn, turn, session, None)?;
                store::finish_turn(
                    conn,
                    turn,
                    status,
                    outcome
                        .map(|o| serde_json::to_string(&o).unwrap())
                        .as_deref(),
                )
            };
            settle(
                conn,
                "SES-FAILED",
                "T-F",
                "finished",
                Some(TurnOutcome::Finished {
                    terminal: TurnTerminal::Failed,
                    usage: None,
                    error: Some(TurnError {
                        code: "provider_failed".into(),
                        message: "upstream closed stream mid-content".into(),
                        retryable: false,
                        details_ref: None,
                    }),
                }),
            )?;
            settle(
                conn,
                "SES-CANCELLED",
                "T-C",
                "finished",
                Some(TurnOutcome::Finished {
                    terminal: TurnTerminal::Cancelled,
                    usage: None,
                    error: None,
                }),
            )?;
            settle(
                conn,
                "SES-OK",
                "T-OK",
                "finished",
                Some(TurnOutcome::Finished {
                    terminal: TurnTerminal::Completed,
                    usage: None,
                    error: None,
                }),
            )?;
            store::start_turn(conn, "T-A", "SES-ACTIVE", None)?;
            settle(
                conn,
                "SES-INTERRUPTED",
                "T-I",
                "interrupted",
                Some(TurnOutcome::Interrupted {
                    reason: qaqh_ringing_contract::fact::InterruptReason::Crash,
                }),
            )?;
            Ok::<(), crate::db::DbError>(())
        })
        .expect("seed parent/child sessions");

        let state = db
            .read_blocking(|conn| view::load_session_state(conn, "SES-P"))
            .expect("load parent state");
        let status_of = |child: &str| {
            state
                .subagents
                .iter()
                .find(|s| s.child_session_id.0 == child)
                .unwrap_or_else(|| panic!("subagent {child} must be listed"))
                .status
        };
        assert_eq!(
            status_of("SES-FAILED"),
            Some(SubagentTerminalStatus::Failed),
            "provider 失败的子代理不得显示已完成"
        );
        assert_eq!(
            status_of("SES-CANCELLED"),
            Some(SubagentTerminalStatus::Cancelled),
            "被取消的子代理不得显示已完成"
        );
        assert_eq!(status_of("SES-OK"), Some(SubagentTerminalStatus::Completed));
        assert_eq!(status_of("SES-ACTIVE"), None, "运行中不是终态");
        assert_eq!(
            status_of("SES-INTERRUPTED"),
            Some(SubagentTerminalStatus::Cancelled)
        );
    }

    /// 64-hex 占位摘要(裸形态)。
    const BARE_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const BARE_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn exec_settle_stores_bare_content_id_and_view_ref_is_prefixed() {
        let db = test_db();
        let expected = {
            use sha2::{Digest, Sha256};
            hex::encode(Sha256::digest(b"body"))
        };
        let expected_ref = format!("sha256:{expected}");
        db.write_blocking(move |conn| {
            store::create_session(
                conn,
                &store::NewSession {
                    session_id: "SES-CONTENT".into(),
                    parent_session_id: None,
                    title: "content".into(),
                    title_source: None,
                    cwd: None,
                    model: None,
                    tool_mode: None,
                    model_selection: None,
                    custom_tools: None,
                    frozen_annotation: None,
                    ephemeral: false,
                },
            )?;
            store::upsert_exec_session(
                conn,
                &store::ExecSessionRow {
                    exec_session_id: "execs-1".into(),
                    session_id: "SES-CONTENT".into(),
                    origin_turn_id: Some("turn-1".into()),
                    origin_call_id: Some("call-1".into()),
                    tty: false,
                    command: "echo body".into(),
                    cwd: None,
                    state_json: "{\"kind\":\"running\"}".into(),
                    state_kind: "running".into(),
                    started_at_ms: 10,
                    finished_at_ms: None,
                    next_offset: 0,
                    output_complete: true,
                    capture_omitted_bytes: 0,
                    stream_dropped_bytes: 0,
                    output_content_id: None,
                    output_preview: String::new(),
                    preview_start_offset: 0,
                },
            )?;
            store::finish_exec_session(
                conn,
                "execs-1",
                "{\"kind\":\"finished\",\"outcome\":{\"kind\":\"exited\",\"exit_code\":0}}",
                20,
                true,
                None,
                Some("body"),
                false,
                "preview",
                0,
            )?;
            // 库内裸 hex:owners/get 按裸键命中(daemon GET 入口剥前缀后的形态)。
            assert_eq!(
                store::session_content_owners(conn, &expected)?,
                ["SES-CONTENT"]
            );
            assert!(store::get_session_content(conn, "SES-CONTENT", &expected)?.is_some());
            let row = store::exec_session_by_id(conn, "execs-1")?.expect("settled row");
            assert_eq!(row.output_content_id.as_deref(), Some(expected.as_str()));
            Ok::<(), crate::db::DbError>(())
        })
        .expect("exec settle round trip");

        let row = db
            .read_blocking(|conn| {
                store::exec_session_by_id(conn, "execs-1")
                    .map(|row| row.expect("settled row"))
                    .map_err(crate::db::DbError::from)
            })
            .expect("read row");
        let view = view::exec_session_view(row).expect("view");
        let Some(ContentValue::Ref { content_ref }) = view.output else {
            panic!("settled exec session must expose a ref output");
        };
        // wire ref 在视图边界补 `sha256:` 前缀(前端/SDK 校验的 canonical 形态)。
        assert_eq!(content_ref.hash().as_str(), expected_ref);
    }

    #[test]
    fn content_ids_normalize_on_write_and_legacy_prefixed_rows_still_resolve() {
        let db = test_db();
        db.write_blocking(|conn| {
            store::create_session(
                conn,
                &store::NewSession {
                    session_id: "SES-CID".into(),
                    parent_session_id: None,
                    title: "content ids".into(),
                    title_source: None,
                    cwd: None,
                    model: None,
                    tool_mode: None,
                    model_selection: None,
                    custom_tools: None,
                    frozen_annotation: None,
                    ephemeral: false,
                },
            )?;
            // wire 形态(`sha256:<hex>`)写入 → 库内归一为裸 hex,
            // 不产生第二种主键形态。
            store::put_session_content(
                conn,
                "SES-CID",
                &format!("sha256:{BARE_A}"),
                "text/plain",
                b"x",
                false,
            )?;
            let prefixed_rows: i64 = conn.query_row(
                "SELECT COUNT(*) FROM session_contents WHERE content_id LIKE 'sha256:%'",
                [],
                |row| row.get(0),
            )?;
            assert_eq!(prefixed_rows, 0);
            assert!(store::get_session_content(conn, "SES-CID", BARE_A)?.is_some());
            assert_eq!(store::session_content_owners(conn, BARE_A)?, ["SES-CID"]);

            // 模拟绕过写入归一的历史遗留前缀行(如外部工具直写库)→
            // 裸键读取按双形态兜底命中。
            conn.execute(
                "INSERT INTO session_contents (session_id, content_id, media_type, bytes, truncated)
                 VALUES ('SES-CID', ?1, 'text/plain', x'79', 0)",
                [&format!("sha256:{BARE_B}")],
            )?;
            assert!(store::get_session_content(conn, "SES-CID", BARE_B)?.is_some());
            assert_eq!(store::session_content_owners(conn, BARE_B)?, ["SES-CID"]);
            Ok::<(), crate::db::DbError>(())
        })
        .expect("content id normalization");
    }

    #[test]
    fn team_inbox_projection_extracts_inter_agent_envelopes_only() {
        let db = test_db();
        let envelope = qaqh_ringing_contract::commands::InterAgentEnvelope {
            message_id: "msg_1".into(),
            root_session_id: "SES-ROOT".into(),
            author: "/root/explore_task".into(),
            recipient: "/root".into(),
            other_recipients: vec![],
            task_id: Some("task_1".into()),
            reply_to: None,
            causation_id: None,
            delivery: qaqh_ringing_contract::fact::InterAgentDelivery::Queue,
            created_at_ms: 1,
        };
        let input = |id: &str, inter: Option<qaqh_ringing_contract::commands::InterAgentEnvelope>| {
            TimelineEntry::Input {
                block_id: qaqh_ringing_contract::fact::BlockId::new(id),
                turn_id: None,
                purpose: Default::default(),
                content: ContentValue::Inline { text: "hi".into() },
                attachments: vec![],
                images: vec![],
                inter_agent: inter,
                subagent_terminal: None,
                actor: qaqh_ringing_contract::fact::ActorRef {
                    kind: qaqh_ringing_contract::fact::ActorKind::User,
                    id: "user".into(),
                    display_name: None,
                },
            }
        };
        db.write_blocking(move |conn| {
            store::create_session(
                conn,
                &store::NewSession {
                    session_id: "SES-ROOT".into(),
                    parent_session_id: None,
                    title: "root".into(),
                    title_source: None,
                    cwd: None,
                    model: None,
                    tool_mode: None,
                    model_selection: None,
                    custom_tools: None,
                    frozen_annotation: None,
                    ephemeral: false,
                },
            )?;
            for (id, inter) in [("B-IA", Some(envelope.clone())), ("B-PLAIN", None)] {
                store::append_block(
                    conn,
                    &store::NewBlock {
                        block_id: id.into(),
                        session_id: "SES-ROOT".into(),
                        turn_id: None,
                        kind: "input",
                        text: Some("hi".into()),
                        call_id: None,
                        payload_json: serde_json::to_string(&input(id, inter)).expect("serialize"),
                    },
                )?;
            }
            Ok::<(), crate::db::DbError>(())
        })
        .expect("seed");

        let hits = db
            .read_blocking(|conn| {
                view::inter_agent_deliveries(
                    conn,
                    &["SES-ROOT".to_string()],
                    10,
                )
            })
            .expect("projection");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].message_id, "msg_1");
        assert_eq!(hits[0].recipient, "/root");
    }

    // ── 尾窗(bootstrap timeline tail;仅 bootstrap 读口接尾窗变体)──────

    fn seed_window_session(db: &Db, session_id: &str) {
        let session_id = session_id.to_string();
        db.write_blocking(move |conn| {
            store::create_session(
                conn,
                &store::NewSession {
                    session_id: session_id.clone().into(),
                    parent_session_id: None,
                    title: session_id.into(),
                    title_source: None,
                    cwd: None,
                    model: None,
                    tool_mode: None,
                    model_selection: None,
                    custom_tools: None,
                    frozen_annotation: None,
                    ephemeral: false,
                },
            )
        })
        .expect("seed session");
    }

    /// 落一个回合 2 个 ToolCall 块;返回逐块 `(turn_id, ord)`。
    fn seed_window_turn(db: &Db, session_id: &str, turn_no: usize) -> Vec<(String, i64)> {
        let session_id = session_id.to_string();
        db.write_blocking(move |conn| {
            let turn_id = format!("T{turn_no}");
            store::start_turn(conn, &turn_id, &session_id, None)?;
            let mut blocks = Vec::new();
            for b in 0..2 {
                let call_id = format!("C{turn_no}_{b}");
                let ord = store::append_block(
                    conn,
                    &store::NewBlock {
                        block_id: format!("B_{call_id}"),
                        session_id: session_id.clone().into(),
                        turn_id: Some(turn_id.clone()),
                        kind: "tool_call",
                        text: None,
                        call_id: Some(call_id.clone()),
                        payload_json: serde_json::to_string(&TimelineEntry::ToolCall {
                            call_id: ToolCallId::new(call_id),
                            turn_id: Some(TurnId::new(turn_id.clone())),
                        })
                        .expect("serialize tool call entry"),
                    },
                )?;
                blocks.push((turn_id.clone(), ord));
            }
            Ok::<Vec<(String, i64)>, rusqlite::Error>(blocks)
        })
        .expect("seed turn")
    }

    /// 压缩块无回合锚点(turn_id NULL),与 owner 写口同形态。
    fn seed_window_compaction(db: &Db, session_id: &str, summary: &str) -> i64 {
        let (session_id, summary) = (session_id.to_string(), summary.to_string());
        db.write_blocking(move |conn| {
            store::append_block(
                conn,
                &store::NewBlock {
                    block_id: "B-COMPACT".into(),
                    session_id: session_id.into(),
                    turn_id: None,
                    kind: "compaction",
                    text: None,
                    call_id: None,
                    payload_json: serde_json::to_string(&TimelineEntry::Compaction {
                        summary: ContentValue::Inline { text: summary.into() },
                    })
                    .expect("serialize compaction"),
                },
            )
        })
        .expect("seed compaction")
    }

    fn turn_of(entry: &TimelineEntry) -> &str {
        let TimelineEntry::ToolCall { turn_id, .. } = entry else {
            panic!("tool call entry expected");
        };
        turn_id.as_ref().map(|turn| turn.as_ref()).unwrap_or("?")
    }

    #[test]
    fn tailed_bootstrap_matches_full_when_window_covers_session() {
        let db = test_db();
        seed_window_session(&db, "SES-TW1");
        for n in 1..=3 {
            seed_window_turn(&db, "SES-TW1", n);
        }
        let full = db
            .read_blocking(|conn| view::load_session_state(conn, "SES-TW1"))
            .expect("full");
        let tailed = db
            .read_blocking(|conn| view::load_session_state_tailed(conn, "SES-TW1", 600))
            .expect("tailed");
        assert_eq!(full, tailed, "窗口覆盖全会话时尾窗变体与全量逐字段一致");
        assert_eq!(tailed.timeline_from_ord, None);
        assert_eq!(tailed.timeline.len(), 6);
    }

    #[test]
    fn tailed_bootstrap_marks_turn_boundary_without_headless_turn() {
        let db = test_db();
        seed_window_session(&db, "SES-TW2");
        let blocks: Vec<(String, i64)> = (1..=3)
            .flat_map(|n| seed_window_turn(&db, "SES-TW2", n))
            .collect();
        // 每回合 2 块,ord 按落库序递增;窗口 2 = T3 全回合。
        let state = db
            .read_blocking(|conn| view::load_session_state_tailed(conn, "SES-TW2", 2))
            .expect("tailed");
        let t3_first = blocks[4].1;
        assert_eq!(state.timeline_from_ord, Some(t3_first));
        assert_eq!(state.timeline.len(), 2);
        assert_eq!(turn_of(&state.timeline[0]), "T3", "不得出现无头回合");

        // 窗口 3 从 T2 尾块开始:向前回退到 T2 首块,整回合入窗。
        let state = db
            .read_blocking(|conn| view::load_session_state_tailed(conn, "SES-TW2", 3))
            .expect("tailed");
        let t2_first = blocks[2].1;
        assert_eq!(state.timeline_from_ord, Some(t2_first));
        assert_eq!(state.timeline.len(), 4, "T2 + T3 整回合入窗");
        assert_eq!(turn_of(&state.timeline[0]), "T2");
    }

    #[test]
    fn tailed_bootstrap_reports_none_when_rollback_covers_session() {
        let db = test_db();
        seed_window_session(&db, "SES-TW3");
        for n in 1..=2 {
            seed_window_turn(&db, "SES-TW3", n);
        }
        // 取满窗口但会话恰好只有 4 块:本来完整。
        let state = db
            .read_blocking(|conn| view::load_session_state_tailed(conn, "SES-TW3", 4))
            .expect("tailed");
        assert_eq!(state.timeline_from_ord, None);
        // 窗口 3 从 T1 尾块开始,回退到 T1 首块后已覆盖全会话:按完整快照标记。
        let state = db
            .read_blocking(|conn| view::load_session_state_tailed(conn, "SES-TW3", 3))
            .expect("tailed");
        assert_eq!(state.timeline_from_ord, None);
        assert_eq!(state.timeline.len(), 4);
    }

    #[test]
    fn tailed_bootstrap_prepends_latest_truncated_compaction() {
        let db = test_db();
        seed_window_session(&db, "SES-TW4");
        let blocks: Vec<(String, i64)> = {
            let mut blocks = seed_window_turn(&db, "SES-TW4", 1);
            seed_window_compaction(&db, "SES-TW4", "早期内容已压缩");
            blocks.extend((2..=3).flat_map(|n| seed_window_turn(&db, "SES-TW4", n)));
            blocks
        };
        // 块序:T1 两块、压缩、T2 两块、T3 两块;压缩块无回合锚点,不在 blocks 里。
        let state = db
            .read_blocking(|conn| view::load_session_state_tailed(conn, "SES-TW4", 2))
            .expect("tailed");
        let t3_first = blocks[4].1;
        assert_eq!(state.timeline_from_ord, Some(t3_first));
        let TimelineEntry::Compaction { summary } = &state.timeline[0] else {
            panic!("compaction head insert expected");
        };
        assert!(matches!(
            summary,
            ContentValue::Inline { text } if text == "早期内容已压缩"
        ));
        assert_eq!(state.timeline.len(), 3);
        assert_eq!(turn_of(&state.timeline[1]), "T3");
    }

    #[test]
    fn session_turn_returns_full_slice_for_truncated_history() {
        let db = test_db();
        seed_window_session(&db, "SES-TW5");
        for n in 1..=3 {
            seed_window_turn(&db, "SES-TW5", n);
        }
        let full = db
            .read_blocking(|conn| view::load_session_state(conn, "SES-TW5"))
            .expect("full");
        let tailed = db
            .read_blocking(|conn| view::load_session_state_tailed(conn, "SES-TW5", 2))
            .expect("tailed");
        assert!(tailed.timeline_from_ord.is_some());
        // 尾窗外的 T1:按回合取回完整切片,与全量快照一致。
        let slice = db
            .read_blocking(|conn| view::load_session_turn(conn, "SES-TW5", "T1"))
            .expect("slice")
            .expect("turn exists");
        assert_eq!(slice.timeline.len(), 2);
        assert_eq!(slice.timeline, full.timeline[..2]);
    }
}
