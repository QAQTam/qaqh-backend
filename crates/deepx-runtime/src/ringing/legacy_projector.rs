//! LegacyProjector：只接受 DomainEvent，生成 Agent2Ui（影子投影）。
//!
//! PLAN 硬规则：
//! - LegacyProjector **只能**接受 `DomainEvent` 并生成 `Agent2Ui`；
//! - 禁止 `Ringing → Agent2Ui` 桥接（serializer 只接受 DomainEvent 也一样）；
//! - 切流期间同一 DomainEvent 可同时投影 legacy 与 Ringing 用于影子验证，
//!   但只有一个协议能进入可见 store。
//!
//! 本投影器是"domain 事件 → legacy wire"的唯一合法出口。未迁移的生产点
//! 继续直接构造 legacy 事件（EventBus.publish 保留原签名），两者并存。

use deepx_domain::{DomainEvent, RoundDeltaKind};
use deepx_proto::Agent2Ui;

/// 将领域事件投影为 legacy `Agent2Ui`。
///
/// 返回 `None` 表示该事件在 legacy 协议中没有对应表达（影子投影时跳过）。
/// 近似映射（如 OperationFailed → Error）保留语义要点，legacy 无法表达的
/// 结构化字段（error_id 等）不伪造进旧协议。
pub fn project(event: &DomainEvent) -> Option<Agent2Ui> {
    match event {
        DomainEvent::Control(ce) => project_control(ce),
        DomainEvent::Conversation(ce) => project_conversation(ce),
        DomainEvent::Tool(te) => project_tool(te),
    }
}

fn project_control(ce: &deepx_domain::ControlEvent) -> Option<Agent2Ui> {
    use deepx_domain::ControlEvent as CE;
    match ce {
        CE::SessionStateChanged { seed, state } => match state {
            deepx_domain::SessionState::Created | deepx_domain::SessionState::Resumed => {
                Some(Agent2Ui::SessionCreated { seed: seed.clone() })
            }
            deepx_domain::SessionState::Closed => None,
        },
        // SessionActivity 是 ControlServerMessage 独立消息（不经 Agent2Ui）；
        // 该事件在 Agent2Ui 无对应表达，由接入层走 activity 流。
        CE::SessionActivityChanged { .. } => None,
        CE::AgentLifecycleChanged { state } => match state {
            deepx_domain::AgentLifecycleState::Ready => Some(Agent2Ui::Ready),
            deepx_domain::AgentLifecycleState::Stopping
            | deepx_domain::AgentLifecycleState::Stopped => Some(Agent2Ui::ShutdownAck),
            deepx_domain::AgentLifecycleState::Booting => None,
        },
        CE::DashboardUpdated {
            hp_connected,
            session_seed,
            tool_calls_total,
            tool_failures,
            current_phase,
            streaming,
        } => Some(Agent2Ui::Dashboard {
            hp_connected: *hp_connected,
            session_seed: session_seed.clone(),
            tool_calls_total: *tool_calls_total,
            tool_failures: *tool_failures,
            current_phase: current_phase.clone(),
            streaming: *streaming,
            dsml_compat_count: 0,
            documents: vec![],
            recent_edits: vec![],
            tasks: vec![],
            current_todo_id: None,
            session_title: None,
            usage: None,
            context_limit: 0,
            model: None,
        }),
        CE::InteractionRequested {
            interaction_id,
            turn_id,
            mode,
            questions,
        } => Some(Agent2Ui::AskUser {
            turn_id: turn_id.clone(),
            round_num: 0,
            ask_id: interaction_id.clone(),
            mode: match mode {
                deepx_domain::AskMode::Single => deepx_proto::AskMode::Single,
                deepx_domain::AskMode::Batch => deepx_proto::AskMode::Batch,
            },
            questions: questions
                .iter()
                .map(|q| deepx_proto::AskQuestion {
                    id: q.id.clone(),
                    question: q.question.clone(),
                    options: q.options.clone(),
                    allow_custom: q.allow_custom,
                })
                .collect(),
        }),
        CE::InteractionResolved {
            interaction_id,
            resolution,
        } => Some(Agent2Ui::AskResolved {
            ask_id: interaction_id.clone(),
            resolution: match resolution {
                deepx_domain::AskResolution::Answered => deepx_proto::AskResolution::Answered,
                deepx_domain::AskResolution::Dismissed => deepx_proto::AskResolution::Dismissed,
            },
        }),
        CE::PlanReviewRequested {
            interaction_id,
            turn_id: _turn_id,
            plan_content,
            review_type,
            todo_items,
        } => Some(Agent2Ui::PlanSubmitted {
            call_id: interaction_id.clone(),
            plan_content: plan_content.clone(),
            review_type: review_type.clone(),
            todo_items: todo_items.as_ref().map(|items| {
                items
                    .iter()
                    .map(|t| deepx_proto::TodoActivationItem {
                        id: t.id.clone(),
                        title: t.title.clone(),
                        description: t.description.clone(),
                        complexity: t.complexity.clone(),
                    })
                    .collect()
            }),
        }),
        CE::PlanReviewResolved {
            interaction_id,
            approved,
        } => Some(Agent2Ui::PlanResolved {
            call_id: interaction_id.clone(),
            approved: *approved,
        }),
        CE::SkillsUpdated {
            available,
            active,
            catalog_revision,
            operation_revision,
        } => Some(Agent2Ui::SkillsChanged {
            status: deepx_proto::SkillsStatus {
                available: available
                    .iter()
                    .map(|s| deepx_proto::SkillInfo {
                        name: s.name.clone(),
                        description: s.description.clone(),
                        scope: s.scope.clone(),
                        source: s.source.clone(),
                    })
                    .collect(),
                active: active.clone(),
                catalog_revision: catalog_revision.clone().unwrap_or_default(),
                context_epoch: 0,
                operation_revision: operation_revision.unwrap_or(0),
                token_budget: 0,
                token_usage: 0,
                runtime: vec![],
                diagnostics: vec![],
            },
        }),
        CE::SystemNotice {
            level, message, ..
        } => Some(Agent2Ui::ToolNotice {
            message: message.clone(),
            level: match level {
                deepx_domain::NoticeLevel::Info => "info".into(),
                deepx_domain::NoticeLevel::Warn => "warn".into(),
                deepx_domain::NoticeLevel::Error => "error".into(),
            },
        }),
        CE::OperationFailed { error, .. } => {
            // legacy 无结构化错误：投影为 Error（不伪造 error_id）
            Some(Agent2Ui::Error {
                message: error.message.clone(),
            })
        }
    }
}

fn project_conversation(ce: &deepx_domain::ConversationEvent) -> Option<Agent2Ui> {
    use deepx_domain::ConversationEvent as CE;
    match ce {
        CE::TurnStarted { turn_id, user_text } => Some(Agent2Ui::TurnStart {
            turn_id: turn_id.clone(),
            user_text: user_text.clone(),
        }),
        CE::TurnCompleted {
            turn_id,
            stop_reason,
            usage,
        } => Some(Agent2Ui::TurnEnd {
            turn_id: turn_id.clone(),
            stop_reason: stop_reason.clone(),
            usage: usage.clone(),
        }),
        CE::TurnFailed { error, .. } => Some(Agent2Ui::Error {
            message: error.message.clone(),
        }),
        CE::RoundDelta {
            turn_id,
            round_num,
            kind,
            delta,
        } => Some(Agent2Ui::RoundDelta {
            turn_id: turn_id.clone(),
            round_num: *round_num,
            kind: match kind {
                RoundDeltaKind::Thinking => deepx_proto::RoundDeltaKind::Thinking,
                RoundDeltaKind::ToolCalling => deepx_proto::RoundDeltaKind::ToolCalling,
                RoundDeltaKind::Answering => deepx_proto::RoundDeltaKind::Answering,
            },
            delta: delta.clone(),
        }),
        CE::RoundCompleted {
            turn_id,
            round_num,
            thinking,
            answer,
            is_final,
            ..
        } => Some(Agent2Ui::RoundComplete {
            turn_id: turn_id.clone(),
            round_num: *round_num,
            thinking: thinking.clone(),
            answer: answer.clone(),
            tool_calls: vec![],
            blocks: vec![],
            is_final: *is_final,
        }),
        CE::ProviderRetrying {
            turn_id,
            round_num,
            attempt,
            max_retries,
            delay_secs,
            error_message,
        } => Some(Agent2Ui::ProviderRetrying {
            turn_id: turn_id.clone(),
            round_num: *round_num,
            attempt: *attempt,
            max_retries: *max_retries,
            delay_secs: *delay_secs,
            error: error_message.clone(),
        }),
        CE::ProviderToolStatus {
            turn_id,
            round_num,
            state,
            ..
        } => Some(Agent2Ui::SearchStatus {
            turn_id: turn_id.clone(),
            round_num: *round_num,
            status: match state {
                deepx_domain::ProviderToolState::InProgress => "in_progress".into(),
                deepx_domain::ProviderToolState::Searching => "searching".into(),
                deepx_domain::ProviderToolState::Completed => "completed".into(),
            },
        }),
        CE::UsageUpdated {
            turn_id,
            round_num,
            usage,
            context_limit,
            model,
        } => Some(Agent2Ui::UsageUpdated {
            turn_id: turn_id.clone(),
            round_num: *round_num,
            usage: usage.clone(),
            context_limit: *context_limit,
            model: model.clone(),
        }),
        CE::CompactStarted {
            turns_total,
            turns_keeping,
            ..
        } => Some(Agent2Ui::CompactStart {
            turns_total: *turns_total,
            turns_keeping: *turns_keeping,
        }),
        CE::CompactProgress { delta, .. } => Some(Agent2Ui::CompactDelta { delta: delta.clone() }),
        CE::CompactFinished {
            status,
            summary_chars,
            turns_compacted,
            turns_removed,
            ..
        } => match status {
            deepx_domain::CompactStatus::Completed => Some(Agent2Ui::CompactEnd {
                summary_chars: summary_chars.unwrap_or(0),
                turns_compacted: turns_compacted.unwrap_or(0),
                turns_removed: turns_removed.unwrap_or(0),
            }),
            deepx_domain::CompactStatus::Skipped
            | deepx_domain::CompactStatus::Failed
            | deepx_domain::CompactStatus::Cancelled => None,
        },
        CE::ConversationCancelled { .. } => Some(Agent2Ui::Cancelled),
    }
}

fn project_tool(te: &deepx_domain::ToolEvent) -> Option<Agent2Ui> {
    use deepx_domain::ToolEvent as TE;
    match te {
        TE::ToolCallPrepared {
            tool_call_id,
            turn_id,
            round_num,
            name,
            args_so_far,
        } => Some(Agent2Ui::ToolCallPreview {
            turn_id: turn_id.clone(),
            round_num: *round_num,
            index: 0,
            id: tool_call_id.clone(),
            name: name.clone(),
            args_so_far: args_so_far.clone(),
        }),
        // ToolStarted 在 legacy 无对应（ToolCallPreview 已覆盖预览语义）
        TE::ToolStarted { .. } => None,
        TE::ToolProgress {
            tool_call_id,
            stream,
            seq_start,
            chunk,
            ..
        } => Some(Agent2Ui::ExecProgress {
            tool_call_id: tool_call_id.clone(),
            stream: stream.clone(),
            seq: *seq_start,
            chunk: chunk.clone(),
        }),
        TE::ToolFinished {
            tool_call_id,
            turn_id,
            round_num,
            result,
        } => Some(Agent2Ui::ToolResults {
            turn_id: turn_id.clone(),
            round_num: *round_num,
            results: vec![deepx_proto::ToolResultDef {
                tool_call_id: tool_call_id.clone(),
                output: result.summary.clone(),
                success: result.success,
                file: None,
            }],
        }),
        TE::ToolFailed {
            tool_call_id,
            turn_id,
            round_num,
            error,
        } => Some(Agent2Ui::ToolResults {
            turn_id: turn_id.clone(),
            round_num: *round_num,
            results: vec![deepx_proto::ToolResultDef {
                tool_call_id: tool_call_id.clone(),
                output: error.message.clone(),
                success: false,
                file: None,
            }],
        }),
        TE::ToolPermissionRequested {
            tool_call_id,
            tool_name,
            reason,
            paths,
            category,
            level,
            risk,
            consequence,
            ..
        } => Some(Agent2Ui::PermissionRequest {
            tool_call_id: tool_call_id.clone(),
            tool_name: tool_name.clone(),
            reason: reason.clone(),
            paths: paths.clone(),
            category: match category {
                deepx_domain::PermissionCategory::Read => "read".into(),
                deepx_domain::PermissionCategory::Write => "write".into(),
                deepx_domain::PermissionCategory::Exec => "exec".into(),
                deepx_domain::PermissionCategory::Net => "net".into(),
            },
            level: *level,
            risk: match risk {
                deepx_domain::PermissionRisk::Low => deepx_proto::PermissionRisk::Low,
                deepx_domain::PermissionRisk::Medium => deepx_proto::PermissionRisk::Medium,
                deepx_domain::PermissionRisk::High => deepx_proto::PermissionRisk::High,
            },
            consequence: consequence.clone(),
        }),
        TE::ToolNotice {
            level, message, ..
        } => Some(Agent2Ui::ToolNotice {
            message: message.clone(),
            level: match level {
                deepx_domain::NoticeLevel::Info => "info".into(),
                deepx_domain::NoticeLevel::Warn => "warn".into(),
                deepx_domain::NoticeLevel::Error => "error".into(),
            },
        }),
        TE::AuditRecorded {
            tool_name,
            result_summary,
            success,
            time,
            ..
        } => Some(Agent2Ui::AuditRecord {
            tool_name: tool_name.clone(),
            result_summary: result_summary.clone(),
            success: *success,
            time: time.clone(),
            args: String::new(),
        }),
        TE::CodeChanged {
            lines_added,
            lines_removed,
            files_created,
            files_deleted,
            file,
        } => Some(Agent2Ui::CodeDelta {
            lines_added: *lines_added,
            lines_removed: *lines_removed,
            files_created: *files_created,
            files_deleted: *files_deleted,
            file: file.clone(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deepx_domain::{
        AskMode, ConversationEvent, ControlEvent, DomainError, SessionState, ToolEvent, ToolResult,
    };

    #[test]
    fn control_lifecycle_projects_to_legacy() {
        let ev = DomainEvent::Control(ControlEvent::SessionStateChanged {
            seed: "s1".into(),
            state: SessionState::Created,
        });
        match project(&ev) {
            Some(Agent2Ui::SessionCreated { seed }) => assert_eq!(seed, "s1"),
            other => panic!("expected SessionCreated, got {other:?}"),
        }
    }

    #[test]
    fn interaction_projects_to_ask_user() {
        let ev = DomainEvent::Control(ControlEvent::InteractionRequested {
            interaction_id: "i1".into(),
            turn_id: "t1".into(),
            mode: AskMode::Single,
            questions: vec![deepx_domain::AskQuestion {
                id: "q1".into(),
                question: "pick".into(),
                options: vec!["A".into()],
                allow_custom: true,
            }],
        });
        match project(&ev) {
            Some(Agent2Ui::AskUser { ask_id, mode, .. }) => {
                assert_eq!(ask_id, "i1");
                assert!(matches!(mode, deepx_proto::AskMode::Single));
            }
            other => panic!("expected AskUser, got {other:?}"),
        }
    }

    #[test]
    fn conversation_projects_round_delta_with_kind() {
        let ev = DomainEvent::Conversation(ConversationEvent::RoundDelta {
            turn_id: "t1".into(),
            round_num: 1,
            kind: RoundDeltaKind::Thinking,
            delta: "hmm".into(),
        });
        match project(&ev) {
            Some(Agent2Ui::RoundDelta { kind, delta, .. }) => {
                assert!(matches!(kind, deepx_proto::RoundDeltaKind::Thinking));
                assert_eq!(delta, "hmm");
            }
            other => panic!("expected RoundDelta, got {other:?}"),
        }
    }

    #[test]
    fn tool_progress_projects_to_exec_progress() {
        let ev = DomainEvent::Tool(ToolEvent::ToolProgress {
            tool_call_id: "c1".into(),
            turn_id: "t".into(),
            round_num: 0,
            stream: "stderr".into(),
            seq_start: 3,
            seq_end: 5,
            chunk: "err".into(),
            dropped_bytes: 0,
            truncated: false,
        });
        match project(&ev) {
            Some(Agent2Ui::ExecProgress { seq, chunk, .. }) => {
                assert_eq!(seq, 3);
                assert_eq!(chunk, "err");
            }
            other => panic!("expected ExecProgress, got {other:?}"),
        }
    }

    #[test]
    fn tool_finished_projects_to_tool_results() {
        let ev = DomainEvent::Tool(ToolEvent::ToolFinished {
            tool_call_id: "c1".into(),
            turn_id: "t".into(),
            round_num: 0,
            result: ToolResult {
                success: true,
                summary: "ok".into(),
                output_ref: None,
            },
        });
        match project(&ev) {
            Some(Agent2Ui::ToolResults { results, .. }) => {
                assert_eq!(results.len(), 1);
                assert!(results[0].success);
            }
            other => panic!("expected ToolResults, got {other:?}"),
        }
    }

    #[test]
    fn started_and_skipped_compact_have_no_legacy_form() {
        assert!(
            project(&DomainEvent::Tool(ToolEvent::ToolStarted {
                tool_call_id: "c".into(),
                turn_id: "t".into(),
                round_num: 0,
                name: "exec".into(),
            }))
            .is_none()
        );
        assert!(
            project(&DomainEvent::Conversation(ConversationEvent::CompactFinished {
                compact_id: "k".into(),
                status: deepx_domain::CompactStatus::Skipped,
                summary_chars: None,
                turns_compacted: None,
                turns_removed: None,
            }))
            .is_none()
        );
    }

    #[test]
    fn operation_failed_projects_to_legacy_error_without_forging_id() {
        let ev = DomainEvent::Control(ControlEvent::OperationFailed {
            occurrence_id: "occ".into(),
            scope: deepx_domain::ErrorScope::Tool,
            error: DomainError {
                error_id: "e1".into(),
                code: "x".into(),
                message: "boom".into(),
                retryable: false,
                dedupe_key: None,
            },
            operation_id: None,
        });
        match project(&ev) {
            Some(Agent2Ui::Error { message }) => assert_eq!(message, "boom"),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn permission_projects_with_category_mapping() {
        let ev = DomainEvent::Tool(ToolEvent::ToolPermissionRequested {
            tool_call_id: "c".into(),
            turn_id: "t".into(),
            round_num: 0,
            tool_name: "write".into(),
            reason: "r".into(),
            paths: vec!["/a".into()],
            category: deepx_domain::PermissionCategory::Write,
            level: 3,
            risk: deepx_domain::PermissionRisk::High,
            consequence: "write file".into(),
        });
        match project(&ev) {
            Some(Agent2Ui::PermissionRequest {
                category,
                level,
                risk,
                ..
            }) => {
                assert_eq!(category, "write");
                assert_eq!(level, 3);
                assert!(matches!(risk, deepx_proto::PermissionRisk::High));
            }
            other => panic!("expected PermissionRequest, got {other:?}"),
        }
    }
}
