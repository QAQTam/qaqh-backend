//! 会话标题生成的 loop 侧 glue。
//!
//! 纯文本逻辑与后台 LLM 总结已外移至 `qaqh-title` crate；本模块只保留
//! 与 `RingContext`/`AgentState` 状态耦合的部分：冻结守卫、截断降级的
//! 落盘广播、以及把 summary 回调桥接回 Ringing 事件通道。

use super::types::{RingContext, WriterEvent};

/// 首 turn 完成挂点（engine `seal_timeline_terminal_round` 的 Completed 分支调用）。
///
/// 幂等：title 已存在（冻结）或 ephemeral 或无可总结的用户消息时零副作用。
pub fn maybe_generate_title(ctx: &mut RingContext) {
    // 冻结守卫：已有标题（含本次会话早前生成）不再生成。
    if ctx.agent.session.title.is_some() {
        return;
    }
    // subagent 一次性模式：不落盘、无标题语义。
    if ctx.agent.ephemeral {
        return;
    }
    let session_id = ctx.agent.session.session_id.clone();
    if session_id.is_empty() {
        return;
    }
    // 首条用户消息（title 的语义锚点 = 用户首次表达的需求）。
    let Some(first_user) = qaqh_title::first_user_text(&ctx.agent.msg) else {
        return;
    };
    let first_user = first_user.trim();
    if first_user.is_empty() {
        return;
    }

    // ── ① 立即：截断标题（instant 可见）──
    let _ = apply_fallback_title(ctx, &session_id, first_user);

    // ── ② 异步：LLM 总结覆盖（失败/超时保持截断版）──
    let provider = super::turn_lap::gate::provider_for(ctx, "title");
    let event_tx = ctx.emitter.event_tx();
    let user_msg = first_user.to_string();
    // 后台线程拿注入句柄写盘（&'static 可跨线程；不入 dispatch 线程的
    // MetaOp 队列——标题覆盖发生在任意时刻，本就不参与写序，PR-1-5）。
    let session_manager = ctx.agent.session_manager_handle();
    qaqh_title::spawn_summary(qaqh_title::SummaryTask {
        provider,
        session_id: session_id.clone(),
        user_msg,
        session_manager,
        on_title: Some(Box::new(move |title| {
            emit_title_changed(&session_id, event_tx, &title);
        })),
    });
}

/// 把 LLM 总结出的标题经 Ringing 事件通道广播为 `SessionMetaChanged`。
fn emit_title_changed(
    session_id: &str,
    event_tx: Option<std::sync::mpsc::SyncSender<WriterEvent>>,
    title: &str,
) {
    let Some(tx) = event_tx else {
        return;
    };
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let env = qaqh_ringing::RingingWorkerEventEnvelope::new(
        session_id,
        format!("w-title-{seq}"),
        qaqh_domain::DomainEvent::Control(qaqh_domain::ControlEvent::SessionMetaChanged {
            session_id: session_id.to_string(),
            title: Some(title.to_string()),
        })
        .into(),
    );
    let _ = tx.send(WriterEvent::Ringing(env));
}

/// Apply the synchronous fallback title. Returns the title applied to the
/// session, or `None` when the session was already frozen.
fn apply_fallback_title(
    ctx: &mut RingContext<'_>,
    session_id: &str,
    first_user: &str,
) -> Option<String> {
    if ctx.agent.session.title.is_some() {
        return None;
    }
    let fallback = qaqh_title::truncate_title(first_user);
    ctx.agent
        .enqueue_meta_op(crate::agent::state::agent::MetaOp::UpdateTitle {
            session_id: session_id.to_string(),
            title: fallback.clone(),
        });
    ctx.agent.session.title = Some(fallback.clone());
    ctx.emitter.emit_domain(qaqh_domain::DomainEvent::Control(
        qaqh_domain::ControlEvent::SessionMetaChanged {
            session_id: session_id.to_string(),
            title: Some(fallback.clone()),
        },
    ));
    Some(fallback)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    struct RecordingEmitter {
        titles: RefCell<Vec<String>>,
    }

    impl crate::agent::types::Emitter for RecordingEmitter {
        fn emit_domain(&self, event: qaqh_domain::DomainEvent) {
            if let qaqh_domain::DomainEvent::Control(
                qaqh_domain::ControlEvent::SessionMetaChanged {
                    title: Some(title), ..
                },
            ) = event
            {
                self.titles.borrow_mut().push(title);
            }
        }
    }

    #[test]
    fn fallback_title_is_applied_once_and_frozen() {
        let mut agent = crate::agent::state::agent::AgentState::new(qaqh_config::Config::default());
        agent.session.session_id = "seed-title".to_string();
        agent.msg.push_user("## 修复标题生成链路");
        let emitter = RecordingEmitter::default();
        let cancel = crate::agent::types::CancelToken::new();
        let writer_dead = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut phase = crate::agent::types::LoopPhase::Idle;
        let mut pending = crate::agent::types::PendingState::default();
        let mut stats = crate::agent::types::StatsCollector::new();
        let mut flow = qaqh_message::ContextFlow::new();
        let mut ctx = crate::agent::types::RingContext {
            agent: &mut agent,
            emitter: &emitter,
            cancel: &cancel,
            phase: &mut phase,
            pending: &mut pending,
            writer_dead: &writer_dead,
            stats: &mut stats,
            flow: &mut flow,
        };

        let first = apply_fallback_title(&mut ctx, "seed-title", "## 修复标题生成链路");
        assert_eq!(first.as_deref(), Some("修复标题生成链路"));
        assert_eq!(ctx.agent.session.title.as_deref(), Some("修复标题生成链路"));

        let second = apply_fallback_title(&mut ctx, "seed-title", "另一个标题");
        assert_eq!(second, None, "title must freeze after the first write");
        assert_eq!(
            emitter.titles.into_inner(),
            vec!["修复标题生成链路".to_string()]
        );
    }
}
