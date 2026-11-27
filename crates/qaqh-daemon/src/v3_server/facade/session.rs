//! Session / Command / ToolCatalog 三域门面（同一份"会话运行时"状态）。
//!
//! ## 骨架期的回合语义（写清楚，免得被当成假数据）
//!
//! `SendPrompt` 会跑一个**真实的回合骨架**：用户块 → `TurnStarted` →
//! 工具调用视图（`ToolCallUpdated` ×2）→ 执行会话视图 + 输出增量 → 答案块
//! （`BlockFinal`）→ `TurnFinished`。它不接模型、不接工具执行器，但**事件形状、
//! 定序、游标、外置规则全是最终形状**，所以前端与守卫拿到的字节流不是玩具。
//!
//! 输出内容就是输入文本本身（"echo"）：这不是偷懒，而是让守卫能**精确对账**
//! （发布字节数 == 收到字节数），任何合帧/分片丢字节都会立刻被抓出来。
//! `v4.7` 把这一段换成真 AgentLoop 时，替换点只有 `run_skeleton_turn` 一个函数。
//!
//! ## 大输出一律外置
//!
//! 超过 [`ServerConfig::inline_budget_bytes`] 的输出**不内联**：正文进内容寻址
//! blob 库，客户端在 `BlobTransfer` 流上按分片收，`ExecOutputDelta` 只发一个
//! `spilled: true` 的短预览。这条规则是 `ADR-0035`/铁律 6 的"大输出外置"在
//! V3 上的落点。

use std::sync::Arc;
use std::sync::atomic::Ordering;

use qaqh_core_vocab::{
    ActivityState, ContentRef, InputId, SessionId, ToolAvailability,
    ToolCapabilityConstraints, ToolCallId, ToolCatalog, ToolCatalogEntry, ToolConcurrency,
    ToolGroup, ToolMode, ToolOrigin, ToolRiskLevel, TurnTerminal, ToolUnavailableReason,
};
use qaqh_ringing_v3::{
    BlockKind, CommandInteractionResolution, CommandRequest, CommandResponse,
    ExecSessionSnapshot, InteractionResolution, RpcError, SessionEvent, SessionRequest,
    SessionResponse, ToolCallStatus, ToolCallView, ToolCatalogRequest, ToolCatalogResponse,
    WorkspaceRequest, WorkspaceResponse,
};

use super::super::error::{denied, not_found, not_wired};
use super::super::session_runtime::{
    SessionRuntime, empty_usage, skeleton_block_id, skeleton_exec_id, skeleton_turn_id,
};
use super::super::state::ServerState;
use super::super::util::now_ms;
use super::{BlobPush, CallContext};

/// Session 门面骨架实现。
pub(crate) struct SkeletonSession {
    state: Arc<ServerState>,
}

impl SkeletonSession {
    /// 构造。
    pub(crate) fn new(state: Arc<ServerState>) -> Self {
        Self { state }
    }

    /// 取会话并做**作用域检查**（越权一律当场拒绝，不泄漏会话是否存在之外的信息）。
    fn session_for(
        &self,
        context: &CallContext,
        session_id: &SessionId,
    ) -> Result<Arc<SessionRuntime>, RpcError> {
        let session = self
            .state
            .session(session_id)
            .ok_or_else(|| not_found(format!("没有会话 `{session_id}`")))?;
        if !session.can_access(&context.identity.device) {
            return Err(denied(format!(
                "设备无权访问会话 `{session_id}`（跨设备越权）"
            )));
        }
        Ok(session)
    }

    /// 处理一条 Session 请求。
    pub(crate) async fn handle_session(
        &self,
        context: &CallContext,
        request: SessionRequest,
    ) -> Result<SessionResponse, RpcError> {
        match request {
            SessionRequest::Create {
                title,
                cwd: _,
                tool_mode,
                model_selection: _,
                custom_tools: _,
                handoff_from: _,
            } => {
                let session = self.state.create_session(&context.identity.device);
                if let Some(title) = title {
                    session.rename(title);
                }
                if let Some(mode) = tool_mode {
                    session.set_tool_mode(mode);
                }
                Ok(SessionResponse::Created(session.summary()))
            }
            SessionRequest::GetSessionSnapshot {
                session_id,
                tail_blocks,
                tail_turns,
            } => {
                let session = self.session_for(context, &session_id)?;
                Ok(SessionResponse::Snapshot(Box::new(
                    session.snapshot(tail_blocks, tail_turns),
                )))
            }
            SessionRequest::TranscriptPage {
                session_id,
                before_ord,
                limit,
                timeline_epoch,
            } => {
                let session = self.session_for(context, &session_id)?;
                Ok(SessionResponse::Page(Box::new(session.page(
                    before_ord,
                    limit,
                    timeline_epoch,
                )?)))
            }
            SessionRequest::List {
                include_archived: _,
            } => {
                let sessions = self
                    .state
                    .sessions()
                    .into_iter()
                    .filter(|session| session.can_access(&context.identity.device))
                    .map(|session| session.summary())
                    .collect();
                Ok(SessionResponse::Sessions(sessions))
            }
            SessionRequest::Rename { session_id, title } => {
                let session = self.session_for(context, &session_id)?;
                session.rename(title);
                Ok(SessionResponse::Ack)
            }
            SessionRequest::Archive { session_id }
            | SessionRequest::Unarchive { session_id }
            | SessionRequest::PatchSettings { session_id, .. }
            | SessionRequest::Truncate { session_id, .. }
            | SessionRequest::SetGoal { session_id, .. }
            | SessionRequest::ClearGoal { session_id }
            | SessionRequest::GetTodos { session_id }
            | SessionRequest::GetTeam { session_id } => {
                let _session = self.session_for(context, &session_id)?;
                Err(not_wired(
                    "该 Session 域子能力随 v4.7 P3-5d 接 QaqhService/Db",
                ))
            }
            SessionRequest::GetSkillsCatalog { cwd: _ } => Err(not_wired(
                "SkillsCatalog 扫描随 v4.7 P3-5d 接 QaqhService",
            )),
            SessionRequest::Delete { session_id } => {
                let session = self.session_for(context, &session_id)?;
                // 先广播再摘表：订阅者要能收到"它没了"，而不是静默消失。
                session.emit(SessionEvent::SessionRemoved {
                    session_id: session_id.clone(),
                });
                self.state.remove_session(&session_id);
                Ok(SessionResponse::Ack)
            }
            SessionRequest::SetToolMode { session_id, mode } => {
                let session = self.session_for(context, &session_id)?;
                session.set_tool_mode(mode);
                session.emit(SessionEvent::CatalogChanged {
                    session_id: session_id.clone(),
                });
                Ok(SessionResponse::Ack)
            }
            SessionRequest::Attach {
                session_id,
                since_seq: _,
            } => {
                let session = self.session_for(context, &session_id)?;
                session.attach(&context.identity);
                super::super::lock(&context.attached_sessions).insert(session_id);
                // 首个附着者原子取得驱动权并按同一锁序发广播。
                session.claim_driver_if_vacant_and_emit(context.identity.driver_key());
                Ok(SessionResponse::Ack)
            }
            SessionRequest::SwitchDriver {
                session_id,
                target_client_instance_id,
            } => {
                let session = self.session_for(context, &session_id)?;
                session.switch_driver_and_emit(
                    &context.identity,
                    target_client_instance_id.as_deref(),
                )?;
                Ok(SessionResponse::Ack)
            }
        }
    }

    /// 处理一条 Command 请求。
    pub(crate) async fn handle_command(
        &self,
        context: &CallContext,
        request: CommandRequest,
    ) -> Result<CommandResponse, RpcError> {
        match request {
            CommandRequest::SendPrompt {
                session_id,
                input_id,
                text,
                attachments,
            } => {
                let session = self.session_for(context, &session_id)?;
                self.run_skeleton_turn(context, &session, input_id, text, attachments)
                    .await
            }
            CommandRequest::CancelTurn { session_id } => {
                let session = self.session_for(context, &session_id)?;
                if session.activity() == ActivityState::Idle {
                    return Ok(CommandResponse::CancelAccepted);
                }
                let turn_id = skeleton_turn_id(session.current_turn().unwrap_or(1));
                session.set_activity(ActivityState::Idle);
                session.finish_turn(
                    session.current_turn().unwrap_or(1),
                    TurnTerminal::Cancelled {
                        code: "user".to_string(),
                    },
                );
                session.emit(SessionEvent::TurnFinished {
                    session_id: session_id.clone(),
                    turn_id,
                    terminal: TurnTerminal::Cancelled {
                        code: "user".to_string(),
                    },
                    usage: empty_usage(),
                });
                session.emit(SessionEvent::ActivityChanged {
                    session_id,
                    activity: ActivityState::Idle,
                    current_turn_id: None,
                });
                Ok(CommandResponse::CancelAccepted)
            }
            CommandRequest::RespondInteraction {
                session_id,
                interaction_id,
                resolution,
            } => {
                let session = self.session_for(context, &session_id)?;
                session.resolve_interaction(&interaction_id);
                let resolution = match resolution {
                    CommandInteractionResolution::Approve { scope } => {
                        InteractionResolution::Approved { scope }
                    }
                    CommandInteractionResolution::Reject { text } => {
                        InteractionResolution::Denied { reason: text }
                    }
                    CommandInteractionResolution::Answer { answers } => {
                        InteractionResolution::Answered { answers }
                    }
                    CommandInteractionResolution::Dismiss => InteractionResolution::Dismissed,
                };
                session.emit(SessionEvent::InteractionResolved {
                    session_id,
                    interaction_id,
                    resolution,
                });
                Ok(CommandResponse::InteractionAccepted)
            }
            CommandRequest::ExecInterrupt { .. } | CommandRequest::ExecTerminate { .. } => {
                Err(not_wired(
                    "ExecSession 控制随 v4.7 P3-5d 接 ExecSessionManager",
                ))
            }
        }
    }

    /// 处理一条 Workspace 请求（骨架期：Owner 权限先验，真实现随 `v4.7 P3-5d` 接盘）。
    pub(crate) async fn handle_workspace(
        &self,
        context: &CallContext,
        request: WorkspaceRequest,
    ) -> Result<WorkspaceResponse, RpcError> {
        if matches!(
            request,
            WorkspaceRequest::Create { .. }
                | WorkspaceRequest::Rename { .. }
                | WorkspaceRequest::Delete { .. }
        ) {
            context.identity.require_owner("Workspace::Mutate")?;
        }
        Err(not_wired(
            "Workspace 域在骨架期如实拒绝；真实现随 v4.7 P3-5d 接 Db",
        ))
    }

    /// 处理一条 ToolCatalog 请求。
    pub(crate) async fn handle_catalog(
        &self,
        context: &CallContext,
        request: ToolCatalogRequest,
    ) -> Result<ToolCatalogResponse, RpcError> {
        match request {
            ToolCatalogRequest::Get { session_id } => {
                let session = self.session_for(context, &session_id)?;
                Ok(ToolCatalogResponse::Catalog(Box::new(skeleton_catalog(
                    session.tool_mode(),
                ))))
            }
        }
    }

    /// 骨架回合（`v4.7` 的唯一替换点）。
    async fn run_skeleton_turn(
        &self,
        context: &CallContext,
        session: &Arc<SessionRuntime>,
        input_id: InputId,
        text: String,
        attachments: Vec<ContentRef>,
    ) -> Result<CommandResponse, RpcError> {
        // ① 驱动权：非 Driver 端发 SendPrompt 当场拒绝（多端联动的物理保证）。
        if !session.is_driver(&context.identity.driver_key()) {
            return Err(denied(
                "只有当前驱动端可以投递用户输入（请先 SwitchDriver）",
            ));
        }
        // 原子检查空闲并切换为 Running，防止并发投递竞争。
        session.try_begin_turn()?;
        let session_id = session.id.clone();
        let attachment_count = attachments.len();

        // ② 用户块 + 回合开始。
        session.append_block(
            BlockKind::User,
            ContentRef::Inline { text: text.clone() },
            None,
        );
        let turn = session.start_turn(None);
        let turn_id = skeleton_turn_id(turn.ordinal);
        session.emit(SessionEvent::ActivityChanged {
            session_id: session_id.clone(),
            activity: ActivityState::Running,
            current_turn_id: Some(turn_id.clone()),
        });
        session.emit(SessionEvent::TurnStarted {
            session_id: session_id.clone(),
            turn_id: turn_id.clone(),
            started_at_ms: now_ms(),
        });

        // ③ 工具调用视图（`Running` → `Succeeded`），跨页归属靠 `tool_call_id`。
        let exec_ordinal = session.metrics.exec_sessions.fetch_add(1, Ordering::SeqCst) + 1;
        let exec_id = skeleton_exec_id(exec_ordinal);
        let tool_call_id = ToolCallId::new(format!("tool-call-{exec_ordinal:04}"))
            .expect("tool call id 形状固定");
        let running_view = ToolCallView {
            tool_call_id: tool_call_id.clone(),
            tool_name: "exec_command".to_string(),
            status: ToolCallStatus::Running,
            started_at_ms: now_ms(),
            finished_at_ms: None,
            summary: None,
        };
        session.upsert_tool_call(running_view.clone());
        session.emit(SessionEvent::ToolCallUpdated {
            session_id: session_id.clone(),
            call: running_view,
        });
        session.begin_tool_call();

        // ④ 执行会话视图 + 输出增量（大输出外置）。
        let output = text.as_bytes().to_vec();
        let inline_budget = self.state.config.inline_budget_bytes;
        let spilled = output.len() > inline_budget;
        let content_ref = if spilled {
            let hash = self.state.blobs.put(output.clone());
            let len = output.len() as u64;
            // 入队推送：连接层会核验权限后开一条 BlobTransfer 流。
            let _ = context.blob_pushes.send(BlobPush {
                session_id: session_id.clone(),
                hash,
                len,
            });
            ContentRef::Blob { hash, len }
        } else {
            ContentRef::Inline { text: text.clone() }
        };
        let exec_snapshot = ExecSessionSnapshot {
            exec_session_id: exec_id.clone(),
            command: format!(
                "skeleton-echo <{} bytes, {attachment_count} attachments>",
                output.len()
            ),
            tty: false,
            finished: true,
            exit_code: Some(0),
            next_offset: output.len() as u64,
            output: Some(content_ref.clone()),
        };
        session.upsert_exec_session(exec_snapshot.clone());
        session.emit(SessionEvent::ExecSessionUpdated {
            session_id: session_id.clone(),
            exec: exec_snapshot,
        });
        if spilled {
            let preview = preview_of(&text, 1024).to_string();
            let end_offset = preview.len() as u64;
            session.emit(SessionEvent::ExecOutputDelta {
                session_id: session_id.clone(),
                exec_session_id: exec_id.clone(),
                start_offset: 0,
                end_offset,
                text: preview,
                spilled: true,
            });
        } else {
            let mut offset = 0u64;
            for chunk in chunk_text(&text, self.state.config.exec_delta_chunk_bytes) {
                let start = offset;
                offset += chunk.len() as u64;
                session.emit(SessionEvent::ExecOutputDelta {
                    session_id: session_id.clone(),
                    exec_session_id: exec_id.clone(),
                    start_offset: start,
                    end_offset: offset,
                    text: chunk.to_string(),
                    spilled: false,
                });
            }
        }

        // ⑤ 工具调用收口 + 工具结果块。
        session.end_tool_call();
        let finished_view = ToolCallView {
            tool_call_id: tool_call_id.clone(),
            tool_name: "exec_command".to_string(),
            status: ToolCallStatus::Succeeded,
            started_at_ms: now_ms(),
            finished_at_ms: Some(now_ms()),
            summary: Some(content_ref.clone()),
        };
        session.upsert_tool_call(finished_view.clone());
        session.emit(SessionEvent::ToolCallUpdated {
            session_id: session_id.clone(),
            call: finished_view,
        });
        session.append_block(
            BlockKind::ToolResult,
            content_ref.clone(),
            Some(tool_call_id),
        );

        // ⑥ 答案块（全文以引用给出）+ 终态。
        let answer_block = session.append_block(BlockKind::Answer, content_ref.clone(), None);
        let block_id = skeleton_block_id(answer_block.ord);
        session.emit(SessionEvent::BlockFinal {
            session_id: session_id.clone(),
            block_id,
            content: content_ref,
        });
        session.finish_turn(turn.ordinal, TurnTerminal::Completed);
        session.emit(SessionEvent::TurnFinished {
            session_id: session_id.clone(),
            turn_id,
            terminal: TurnTerminal::Completed,
            usage: empty_usage(),
        });
        session.set_activity(ActivityState::Idle);
        session.emit(SessionEvent::ActivityChanged {
            session_id,
            activity: ActivityState::Idle,
            current_turn_id: None,
        });
        Ok(CommandResponse::Accepted { input_id })
    }
}

/// 取不超过 `max_bytes` 的前缀（**不切碎 UTF-8 码点**）。
fn preview_of(text: &str, max_bytes: usize) -> &str {
    let mut end = text.len().min(max_bytes);
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.split_at(end).0
}

/// 按字节预算切块（同样不切碎码点；单码点超过预算时优先保证不切碎）。
fn chunk_text(text: &str, chunk_bytes: usize) -> Vec<&str> {
    let budget = chunk_bytes.max(1);
    let mut chunks = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let mut end = rest.len().min(budget);
        while end > 0 && !rest.is_char_boundary(end) {
            end -= 1;
        }
        if end == 0 {
            // 单码点就超过预算：整码点切出去，绝不切碎它（切碎会造出非法 UTF-8）。
            end = rest
                .chars()
                .next()
                .map(char::len_utf8)
                .unwrap_or(rest.len());
        }
        let (head, tail) = rest.split_at(end);
        chunks.push(head);
        rest = tail;
    }
    chunks
}

/// 骨架期工具目录（**不带 schema**；真投影在 `v4.7` 接 `project_tool_catalog`）。
fn skeleton_catalog(mode: ToolMode) -> ToolCatalog {
    let entry = |name: &str,
                 group: ToolGroup,
                 risk: ToolRiskLevel,
                 concurrency: ToolConcurrency,
                 image_output: bool,
                 origin: ToolOrigin,
                 available: bool,
                 reason: ToolUnavailableReason| ToolCatalogEntry {
        name: name.to_string(),
        group,
        summary: format!("{name} 的骨架期摘要（真描述来自声明面 ToolDef）"),
        risk,
        constraints: ToolCapabilityConstraints {
            concurrency,
            streaming: false,
            idempotent: group == ToolGroup::Read,
            workspace_bound: group != ToolGroup::Net,
            interactive: false,
            image_output,
        },
        origin,
        availability: if available {
            ToolAvailability::Available
        } else {
            ToolAvailability::Unavailable { reason }
        },
    };
    let builtin_available = |name: &str| match mode {
        ToolMode::Core => name == "read_file" || name == "exec_command",
        ToolMode::Standard | ToolMode::Custom => true,
    };
    let dynamic_available = mode == ToolMode::Custom;
    let entries = vec![
        entry(
            "exec_command",
            ToolGroup::Exec,
            ToolRiskLevel::Destructive,
            ToolConcurrency::Serial,
            false,
            ToolOrigin::Builtin,
            builtin_available("exec_command"),
            ToolUnavailableReason::ExcludedByToolMode,
        ),
        entry(
            "mcp__demo__search",
            ToolGroup::Net,
            ToolRiskLevel::ReadOnly,
            ToolConcurrency::Parallel,
            false,
            ToolOrigin::DynamicMcp {
                server: "demo".to_string(),
            },
            dynamic_available,
            ToolUnavailableReason::ExcludedByGlobalCeiling,
        ),
        entry(
            "read_file",
            ToolGroup::Read,
            ToolRiskLevel::ReadOnly,
            ToolConcurrency::Parallel,
            false,
            ToolOrigin::Builtin,
            builtin_available("read_file"),
            ToolUnavailableReason::ExcludedByToolMode,
        ),
        entry(
            "read_image",
            ToolGroup::Read,
            ToolRiskLevel::ReadOnly,
            ToolConcurrency::Parallel,
            true,
            ToolOrigin::Builtin,
            builtin_available("read_image"),
            ToolUnavailableReason::ExcludedByToolMode,
        ),
        entry(
            "write_file",
            ToolGroup::Write,
            ToolRiskLevel::Write,
            ToolConcurrency::Parallel,
            false,
            ToolOrigin::Builtin,
            builtin_available("write_file"),
            ToolUnavailableReason::ExcludedByToolMode,
        ),
    ];
    ToolCatalog::new(mode, entries)
}

/// 权限审批视图（骨架期只发审批：`Ask` 需要真模型回合）。
#[cfg(test)]
pub(crate) fn permission_interaction(
    interaction_id: qaqh_core_vocab::InteractionId,
    title: impl Into<String>,
    body: impl Into<String>,
) -> qaqh_ringing_v3::InteractionView {
    qaqh_ringing_v3::InteractionView {
        interaction_id,
        kind: qaqh_ringing_v3::InteractionKind::Permission,
        tool_call_id: None,
        title: title.into(),
        body: body.into(),
        options: Vec::new(),
        questions: Vec::new(),
        expires_at_ms: None,
    }
}

#[cfg(test)]
mod smoke {
    use super::*;
    use crate::v3_server::facade::test_support;
    use qaqh_ringing_v3::{ErrorCode, SessionResponse};

    #[tokio::test]
    async fn snapshot_page_and_catalog_are_reachable() {
        let rig = test_support::rig();
        let context = test_support::owner_context(&rig);
        let session = rig.state.create_session(&context.identity.device);
        let session_id = session.id.clone();

        let snapshot = rig
            .backend
            .session
            .handle_session(
                &context,
                SessionRequest::GetSessionSnapshot {
                    session_id: session_id.clone(),
                    tail_blocks: 8,
                    tail_turns: 32,
                },
            )
            .await
            .expect("快照必须可用");
        match snapshot {
            SessionResponse::Snapshot(snapshot) => {
                assert_eq!(snapshot.session_id, session_id);
                assert_eq!(snapshot.timeline_epoch, 1);
                assert!(snapshot.pinned_compaction.is_none());
            }
            other => panic!("形状不符：{other:?}"),
        }

        let page = rig
            .backend
            .session
            .handle_session(
                &context,
                SessionRequest::TranscriptPage {
                    session_id: session_id.clone(),
                    before_ord: 0,
                    limit: 10,
                    timeline_epoch: 1,
                },
            )
            .await
            .expect("翻页必须可用");
        assert!(matches!(page, SessionResponse::Page(_)));

        // 代际失配必须 StaleCursor（绝不"尽量拼一下"）。
        let stale = rig
            .backend
            .session
            .handle_session(
                &context,
                SessionRequest::TranscriptPage {
                    session_id: session_id.clone(),
                    before_ord: 0,
                    limit: 10,
                    timeline_epoch: 99,
                },
            )
            .await
            .expect_err("陈旧代际必须拒绝");
        assert_eq!(stale.code, ErrorCode::StaleCursor);

        let catalog = rig
            .backend
            .session
            .handle_catalog(&context, ToolCatalogRequest::Get { session_id })
            .await
            .expect("目录必须可用");
        // `ToolCatalogResponse` 只有一个变体：不需要兜底臂，编译器帮忙盯着。
        let ToolCatalogResponse::Catalog(catalog) = catalog;
        assert_eq!(catalog.mode, ToolMode::Standard);
        // 按名字升序，且一个 schema 都没带（目录是选择面，不是声明面）。
        assert!(catalog.entry("exec_command").is_some());
        assert!(catalog.len() >= 3);
        let names: Vec<&str> = catalog.entries.iter().map(|e| e.name.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted, "目录条目必须按名字升序（同一状态产生同一份字节）");
    }

    #[tokio::test]
    async fn cross_device_access_is_denied() {
        let rig = test_support::rig();
        let owner = test_support::owner_context(&rig);
        let device = test_support::device_context(&rig);
        let session = rig.state.create_session(&owner.identity.device);
        let error = rig
            .backend
            .session
            .handle_session(
                &device,
                SessionRequest::GetSessionSnapshot {
                    session_id: session.id.clone(),
                    tail_blocks: 4,
                    tail_turns: 8,
                },
            )
            .await
            .expect_err("跨设备访问必须拒绝");
        assert_eq!(error.code, ErrorCode::PermissionDenied);
    }

    #[tokio::test]
    async fn chunking_never_splits_a_codepoint() {
        let text = "汉字与 emoji 🙂 混排";
        for chunk in chunk_text(text, 3) {
            assert!(std::str::from_utf8(chunk.as_bytes()).is_ok());
        }
        assert_eq!(chunk_text(text, 3).concat(), text);
        assert_eq!(preview_of(text, 4), "汉");
    }
}
