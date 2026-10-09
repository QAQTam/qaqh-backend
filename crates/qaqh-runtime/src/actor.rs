//! In-process agent actor runner (Knife-1).
//!
//! Both main session loops and subagent loops run on daemon threads using the
//! typed `WorkerCommand` / `WriterEvent` channels. Each actor owns its thread
//! and its per-actor workspace state (`qaqh-workspace` thread-locals), so
//! session and subagent actors can run concurrently without a process-wide
//! serialization lock.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, SyncSender};

use crate::agent::{ActorKind, SubagentSpawnSpec};
use crate::ringing::v2::V2ProjectionHub;
use crate::{RingingHub, SessionActivityTracker};

fn short_session(session_id: &str) -> String {
    session_id.chars().take(8).collect()
}

/// Channel-side Ringing event consumer for an in-process actor.
pub(crate) fn run_inprocess_event_reader(
    event_rx: Receiver<crate::agent::types::WriterEvent>,
    session_id: String,
    generation: u64,
    activity: SessionActivityTracker,
    hub: Option<Arc<RingingHub>>,
    v2_hub: Option<Arc<V2ProjectionHub>>,
    sessions: Arc<qaqh_session::SessionManager>,
) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        for event in event_rx {
            publish_worker_event(
                hub.as_deref(),
                v2_hub.as_deref(),
                sessions.as_ref(),
                &activity,
                &session_id,
                generation,
                event,
            );
        }
    }));
    if let Err(panic) = result {
        log::error!(
            "[AGENT:{}] in-process event reader panicked: {:?}",
            short_session(&session_id),
            panic
        );
    }
    // tracker 断连状态更新保留（/activity 查询权威）。
    let _ = activity.disconnect(&session_id, generation);
}

fn publish_worker_event(
    hub: Option<&RingingHub>,
    v2_hub: Option<&V2ProjectionHub>,
    sessions: &qaqh_session::SessionManager,
    activity: &SessionActivityTracker,
    session_id: &str,
    generation: u64,
    event: crate::agent::types::WriterEvent,
) {
    match event {
        crate::agent::types::WriterEvent::Timeline(env) => {
            let Some(hub) = hub else { return; };
            if let Err(error) = hub.publish_timeline(&env.session_id, env.intent) {
                log::error!("[timeline] rejected intent for {}: {error}", env.session_id);
            }
        }
        crate::agent::types::WriterEvent::Ringing(env) => {
            let Some(hub) = hub else { return; };
            let domain: qaqh_domain::DomainEvent = env.event.into();
            // #345：交互正文（ask/plan）在发布前入 content store 并 pin——canonical
            // fact 里只有 ref，正文走展示面旁路。
            crate::registry::stash_interaction_body(hub, &env.session_id, &domain);
            // §4.0.5 迁移完成：live_interactions 登记与交互 pin 释放现在随事件
            // 产生侧执行（worker 桥）——迁移在阶段 3d 删 publish 前即已完成，
            // 故删除广播面未再需要迁移。
            crate::registry::apply_interaction_side_effects(hub, &env.session_id, &domain);
            // 阶段 3d：v1 广播面已删除——worker 事件只保留副作用迁移后的
            // 产生侧动作；事件本体由 canonical fact 面（timeline/journal）外化。
            if let Some(observe) = crate::activity::domain_activity_observe(&domain) {
                // tracker 状态机保留（/activity 查询权威）；v1 广播面已删除，观察只驱动状态机。
                let _ = activity.observe(session_id, generation, &observe);
            }
        }
        crate::agent::types::WriterEvent::TitleChanged { session_id, title } => {
            let Some(v2_hub) = v2_hub else { return; };
            let session_dir = sessions.session_path_dir(&session_id);
            if let Err(error) = v2_hub.publish_title_changed(&session_dir, &session_id, title) {
                log::warn!("[title] v2 publish failed for {session_id}: {error}");
            }
        }
    }
}

/// Shared actor body for main session loops and subagent loops.
///
/// Process-level concerns only: per-actor workspace thread-locals, backend
/// isolation, panic isolation and cleanup. Agent construction and the loop
/// itself live behind [`crate::agent::spawn_agent`] (PR-2-3).
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_actor(
    session_id: String,
    kind: ActorKind,
    cmd_rx: Receiver<crate::agent::types::WorkerCommand>,
    event_tx: SyncSender<crate::agent::types::WriterEvent>,
    cancel: crate::agent::types::CancelToken,
    writer_dead: Arc<std::sync::atomic::AtomicBool>,
    liveness: std::sync::Arc<crate::agent::liveness::WorkerLiveness>,
    hub: Option<Arc<RingingHub>>,
) {
    let is_subagent = matches!(&kind, ActorKind::Subagent(_));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // Per-actor state is thread-local in qaqh-workspace, so actors no
        // longer need a process-wide serialization lock to run concurrently.
        qaqh_workspace::set_actor_context("", &session_id);

        if is_subagent {
            qaqh_workspace::authorization::set_subagent_sandbox(true);
        }

        crate::agent::spawn_agent(
            &session_id,
            kind,
            cmd_rx,
            event_tx,
            cancel,
            writer_dead,
            liveness,
            hub,
        );

        qaqh_workspace::clear_actor_context();
        cleanup_actor_state(is_subagent);
        log::info!("[ACTOR] in-process agent {session_id} exited");
    }));

    if let Err(panic) = result {
        // Failure must not leak actor tooling/sandbox state to the daemon.
        qaqh_workspace::clear_actor_context();
        cleanup_actor_state(is_subagent);
        log::error!(
            "[ACTOR] in-process agent {} panicked: {:?}",
            session_id,
            panic
        );
    }
}

/// Knife-1 step-1 subagent actor wrapper.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_subagent_actor(
    session_id: String,
    spec: SubagentSpawnSpec,
    cmd_rx: Receiver<crate::agent::types::WorkerCommand>,
    event_tx: SyncSender<crate::agent::types::WriterEvent>,
    cancel: crate::agent::types::CancelToken,
    writer_dead: Arc<std::sync::atomic::AtomicBool>,
    liveness: std::sync::Arc<crate::agent::liveness::WorkerLiveness>,
    hub: Option<Arc<RingingHub>>,
) {
    run_actor(
        session_id,
        ActorKind::Subagent(spec),
        cmd_rx,
        event_tx,
        cancel,
        writer_dead,
        liveness,
        hub,
    );
}

/// Knife-1 step-2a session actor wrapper.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_session_actor(
    session_id: String,
    resume_session: Option<String>,
    new_session: Option<String>,
    timeline_turn_count: u64,
    cmd_rx: Receiver<crate::agent::types::WorkerCommand>,
    event_tx: SyncSender<crate::agent::types::WriterEvent>,
    cancel: crate::agent::types::CancelToken,
    writer_dead: Arc<std::sync::atomic::AtomicBool>,
    liveness: std::sync::Arc<crate::agent::liveness::WorkerLiveness>,
    hub: Option<Arc<RingingHub>>,
) {
    run_actor(
        session_id,
        ActorKind::Session {
            resume_session,
            new_session,
            timeline_turn_count,
        },
        cmd_rx,
        event_tx,
        cancel,
        writer_dead,
        liveness,
        hub,
    );
}

fn cleanup_actor_state(is_subagent: bool) {
    qaqh_workspace::runtime::clear_actor_tool_manager();
    if is_subagent {
        qaqh_workspace::authorization::set_subagent_sandbox(false);
    }
    qaqh_workspace::set_cancel(false);
}
