#[cfg(test)]
mod tests {
    use super::super::{Db, store, view};
    use qaqh_ringing_contract::fact::{ContentValue, ToolCallOutcome};
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
                    skills: None,
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
                        skills: None,
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
}
