//! 会话标题生成的 loop 侧 glue。
//!
//! 纯文本逻辑与后台 LLM 总结已外移至 `qaqh-title` crate；本模块只保留
//! 与 `RingContext`/`AgentState` 状态耦合的部分：首轮开始时冻结标题输入，
//! 立即写入可见回退标题，并把异步 LLM 标题结果桥接到 v2 Meta SSE。

use super::types::{RingContext, WriterEvent};

/// 用户首条消息接收后、首个模型请求开始前调用；summary 在线程中与模型请求并行。
///
/// 幂等：title 已存在（冻结）或 ephemeral 或无可总结的用户消息时零副作用。
pub fn maybe_start_title(ctx: &mut RingContext) {
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

    // ── ① 立即：截断标题（模型请求开始前可见）──
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
            emit_title_changed(event_tx, session_id, title);
        })),
    });
}

/// 把标题变化送到 worker event reader，由它通过 v2 Meta projection 推送。
fn emit_title_changed(
    event_tx: Option<std::sync::mpsc::SyncSender<WriterEvent>>,
    session_id: String,
    title: String,
) {
    let Some(tx) = event_tx else {
        return;
    };
    let _ = tx.send(WriterEvent::TitleChanged { session_id, title });
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
    if let Some(session_manager) = ctx.agent.session_manager_handle() {
        session_manager.update_title(session_id, &fallback);
    } else {
        ctx.agent
            .enqueue_meta_op(crate::agent::state::agent::MetaOp::UpdateTitle {
                session_id: session_id.to_string(),
                title: fallback.clone(),
            });
    }
    ctx.agent.session.title = Some(fallback.clone());
    emit_title_changed(
        ctx.emitter.event_tx(),
        session_id.to_string(),
        fallback.clone(),
    );
    Some(fallback)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_title_is_applied_once_and_frozen() {
        let mut agent = crate::agent::state::agent::AgentState::new(qaqh_config::Config::default());
        agent.session.session_id = "seed-title".to_string();
        agent.msg.push_user("## 修复标题生成链路");
        let writer_dead = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (event_tx, event_rx) = std::sync::mpsc::sync_channel(8);
        let emitter = crate::agent::paced_emitter::PacedEmitter::new(
            "seed-title",
            event_tx,
            writer_dead.clone(),
        );
        let cancel = crate::agent::types::CancelToken::new();
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
        assert!(matches!(
            event_rx.try_recv(),
            Ok(WriterEvent::TitleChanged { title, .. }) if title == "修复标题生成链路"
        ));
        assert!(event_rx.try_recv().is_err(), "frozen title emits no second event");
    }
}
