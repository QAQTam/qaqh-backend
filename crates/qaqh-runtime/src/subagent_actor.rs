//! Knife-1 step-1 in-process subagent actor.
//!
//! The actor runs the same Ringing V1 `Loop` as a process worker, but on a
//! daemon thread and over the typed `WorkerCommand`/`WriterEvent` channels.
//! Until qaqh-workspace globals become per-actor, all actors share one
//! process-wide serialization mutex.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, SyncSender};

use crate::{RingingHub, SessionActivityTracker};

#[derive(Clone)]
pub(crate) struct SubagentSpawnSpec {
    pub(crate) tools: Vec<String>,
    pub(crate) model: Option<String>,
    pub(crate) base_url: Option<String>,
    pub(crate) max_tokens: Option<u32>,
    pub(crate) ephemeral: bool,
}

pub(crate) static SUBAGENT_ACTOR_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Apply subagent config defaults (`cfg.subagent.*`) and explicit overrides.
///
/// Shared by the process worker (`worker.rs`) and the in-process actor so the
/// two paths cannot drift. Explicit overrides win over settings defaults.
pub(crate) fn apply_subagent_config(
    agent: &mut qaqh_msglp::state::agent::AgentState,
    model: Option<&str>,
    base_url: Option<&str>,
    max_tokens: Option<u32>,
) {
    let sub = agent.config.subagent.clone();
    if !sub.model.is_empty() && model.is_none() {
        agent.config.model = sub.model;
    }
    if !sub.base_url.is_empty() && base_url.is_none() {
        agent.config.base_url = sub.base_url;
    }
    if !sub.api_key.is_empty() {
        agent.config.api_key = sub.api_key;
    }
    if sub.max_tokens > 0 && max_tokens.is_none() {
        agent.config.max_tokens = sub.max_tokens;
    }
    if let Some(model) = model {
        agent.config.model = model.to_string();
    }
    if let Some(base_url) = base_url {
        agent.config.base_url = base_url.to_string();
    }
    if let Some(max_tokens) = max_tokens {
        agent.config.max_tokens = max_tokens;
    }
}

fn short_seed(seed: &str) -> String {
    seed.chars().take(8).collect()
}

/// Channel-side Ringing event consumer for an in-process actor.
pub(crate) fn run_inprocess_event_reader(
    event_rx: Receiver<qaqh_msglp::ringing_v1::types::WriterEvent>,
    seed: String,
    generation: u64,
    activity: SessionActivityTracker,
    hub: Option<Arc<RingingHub>>,
) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        for event in event_rx {
            publish_worker_event(hub.as_deref(), &activity, &seed, generation, event);
        }
    }));
    if let Err(panic) = result {
        log::error!(
            "[AGENT:{}] in-process event reader panicked: {:?}",
            short_seed(&seed),
            panic
        );
    }
    if let Some(update) = activity.disconnect(&seed, generation) {
        crate::activity::publish_activity(hub.as_deref(), &update);
    }
}

fn publish_worker_event(
    hub: Option<&RingingHub>,
    activity: &SessionActivityTracker,
    seed: &str,
    generation: u64,
    event: qaqh_msglp::ringing_v1::types::WriterEvent,
) {
    let Some(hub) = hub else {
        return;
    };
    match event {
        qaqh_msglp::ringing_v1::types::WriterEvent::Timeline(env) => {
            if let Err(error) = hub.publish_timeline(&env.seed, env.intent) {
                log::error!("[timeline] rejected intent for {}: {error}", env.seed);
            }
        }
        qaqh_msglp::ringing_v1::types::WriterEvent::Ringing(env) => {
            let domain: qaqh_domain::DomainEvent = env.event.into();
            let domain = crate::registry::externalize_large_content(hub, &env.seed, domain);
            let _ =
                hub.publish_with_causation(&env.seed, domain.clone(), env.causation_id.as_deref());
            if let Some(observe) = crate::activity::domain_activity_observe(&domain)
                && let Some(activity) = activity.observe(seed, generation, &observe)
            {
                crate::activity::publish_activity(Some(hub), &activity);
            }
        }
    }
}

/// Knife-1 step-1 subagent actor body: one Ringing Loop per subagent seed,
/// running on a daemon thread instead of a `qaqh agent --seed` child process.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_subagent_actor(
    seed: String,
    spec: SubagentSpawnSpec,
    cmd_rx: Receiver<qaqh_msglp::ringing_v1::types::WorkerCommand>,
    event_tx: SyncSender<qaqh_msglp::ringing_v1::types::WriterEvent>,
    cancel: qaqh_msglp::ringing_v1::types::CancelToken,
    writer_dead: Arc<std::sync::atomic::AtomicBool>,
    workspace_mode: String,
    workspace_env: Option<(String, String)>,
) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // Step-1 isolation boundary: serialize in-process subagent loops until
        // qaqh-workspace globals become per-actor state. Main session workers
        // remain separate processes and are unaffected.
        let _serial = SUBAGENT_ACTOR_SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        if workspace_mode.eq_ignore_ascii_case("wsl") {
            if let Some((endpoint, token)) = workspace_env.as_ref()
                && !endpoint.is_empty()
                && !token.is_empty()
            {
                qaqh_workspace::install_workspace_backend(Arc::new(
                    qaqh_workspace::HttpToolExecutionBackend::new(endpoint.clone(), token.clone()),
                ));
            }
        } else {
            qaqh_workspace::use_local_workspace_backend();
        }

        qaqh_workspace::authorization::set_subagent_sandbox(true);
        let mut agent = qaqh_msglp::state::agent::AgentState::new(
            qaqh_config::Config::load().unwrap_or_default(),
        );
        agent.ephemeral = spec.ephemeral;

        apply_subagent_config(
            &mut agent,
            spec.model.as_deref(),
            spec.base_url.as_deref(),
            spec.max_tokens,
        );

        agent.session.seed = seed.clone();
        agent.session.created_at = qaqh_session::SessionManager::now_epoch();

        // 受限工具白名单 + 全量模型侧工具清单（与 init_subagent 相同）；
        // 使用 actor-private ToolManager，不污染 daemon 的 skills.list_tools 快照。
        let mut allowed_tools = spec.tools.clone();
        if !allowed_tools.iter().any(|tool| tool == "skills") {
            allowed_tools.push("skills".to_string());
        }
        let registrars = qaqh_msglp::state::agent::agent_tool_registrars();
        let mut manager = qaqh_workspace::registration::build_tool_manager(&registrars);
        manager.apply_init(allowed_tools, &seed);
        qaqh_workspace::runtime::install_actor_tool_manager(manager);
        agent.tool_defs = qaqh_workspace::runtime::all_tools();

        log::info!(
            "[SUBAGENT-ACTOR] starting in-process subagent seed={} tools={:?} ephemeral={}",
            seed,
            spec.tools,
            spec.ephemeral
        );
        let mut loop_ = qaqh_msglp::ringing_v1::loop_core::Loop::from_channels(
            agent,
            cmd_rx,
            event_tx,
            cancel,
            writer_dead,
        );
        loop_.run();

        qaqh_workspace::runtime::clear_actor_tool_manager();
        qaqh_workspace::authorization::set_subagent_sandbox(false);
        qaqh_workspace::CANCEL.store(false, std::sync::atomic::Ordering::SeqCst);
        log::info!("[SUBAGENT-ACTOR] in-process subagent {seed} exited");
    }));

    if let Err(panic) = result {
        // 失败也绝不能把子代理沙箱/actor 工具集泄漏给 daemon 后续调用。
        qaqh_workspace::runtime::clear_actor_tool_manager();
        qaqh_workspace::authorization::set_subagent_sandbox(false);
        qaqh_workspace::CANCEL.store(false, std::sync::atomic::Ordering::SeqCst);
        log::error!(
            "[SUBAGENT-ACTOR] in-process subagent {} panicked: {:?}",
            seed,
            panic
        );
    }
}
