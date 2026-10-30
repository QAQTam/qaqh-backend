use super::*;

/// Registry tests spawn real thin-agent tasks, which require a runtime
/// context even when the test itself never awaits.
fn test_runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
    })
}

fn dummy_thin_agent(session_id: &str) -> AgentInstance {
    let (mailbox_handle, mailbox) = qaqh_agentloop_core::mailbox::Mailbox::new(
        qaqh_agentloop_core::mailbox::LaneBudget::COMMANDS,
        qaqh_agentloop_core::mailbox::LaneBudget::CONTROL,
        qaqh_agentloop_core::mailbox::LaneBudget::COMPLETIONS,
    )
    .unwrap();
    // Keep the receiver alive: these tests exercise command routing, not
    // the closed-mailbox respawn path.
    Box::leak(Box::new(mailbox));
    AgentInstance {
        session_id: session_id.into(),
        transport: AgentTransport::ThinAgent {
            handle: crate::session_agent::AgentHandle {
                session_id: session_id.into(),
                mailbox: mailbox_handle,
            },
            task: test_runtime().spawn(async { Ok(()) }),
        },
        kind: AgentKind::Session,
        subscription_actor: SessionActor::new(4),
        liveness: None,
        reader: None,
        thread: None,
    }
}

/// 正常路径：探测命令秒回，输出拿得到。
#[cfg(unix)] // Windows 无 sh：存量环境失败，与探测逻辑无关
#[test]
fn probe_output_returns_fast_command_output() {
    let mut command = Command::new("sh");
    command.args(["-c", "echo hi"]);
    let output = probe_output(command, Duration::from_secs(5)).expect("应拿到输出");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "hi");
}

/// BUG-2026-09-15-02 的直接回归：命令本身不退出时，探测必须限时返回而非
/// 永久阻塞。
#[test]
fn probe_output_times_out_instead_of_blocking() {
    let mut command = Command::new("sleep");
    command.arg("30");
    let started = Instant::now();
    assert!(
        probe_output(command, Duration::from_millis(200)).is_none(),
        "超时应当返回 None"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "必须在超时后立刻返回，实测 {:?}",
        started.elapsed()
    );
}

/// 同一条缺陷的**精确形状**：被探测程序已经退出，但它的后代继承了管道
/// 写端 → `Command::output()` 会一直等到后代退出（这里 30s），daemon 的
/// 启动就此卡死。`probe_output` 必须把总耗时压在超时预算内。
///
/// 变异验证：把 [`probe_output`] 里的 `recv_timeout` 换回阻塞式读取
/// （或改用 `Command::output()`），本测试立刻红。
#[cfg(unix)] // 同上
#[test]
fn probe_output_survives_grandchild_holding_the_pipe() {
    let mut command = Command::new("sh");
    command.args(["-c", "sleep 30 & echo hi"]);
    let started = Instant::now();
    let output = probe_output(command, Duration::from_millis(300));
    assert!(output.is_some(), "子进程已退出，应拿到（可能是空的）输出");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "管道 EOF 不可信，不得为它无限等待；实测 {:?}",
        started.elapsed()
    );
}

/// 程序不存在时降级为 None，而不是 panic 或挂起。
#[test]
fn probe_output_missing_program_is_none() {
    let command = Command::new("qaqh-no-such-program-xyz");
    assert!(probe_output(command, Duration::from_millis(200)).is_none());
}

#[tokio::test]
async fn materialized_batch_session_spawns_and_reopens_without_reset() {
    let root = tempfile::tempdir().expect("tempdir");
    let sessions = Arc::new(qaqh_session::SessionManager::new_for_test(
        root.path().join("sessions"),
        root.path().join(".active_session"),
    ));
    let identity = sessions.allocate_session(None).expect("allocate");
    let session_id = identity.session_id.as_str();
    let dir = sessions.session_path_dir(session_id);
    crate::service::materialize_canonical_session_in(&dir, "/tmp", "test-model", None)
        .expect("materialize");
    let journal_before =
        std::fs::read(dir.join("committed-batches/batches.jsonl")).expect("baseline journal");
    let mut registry = AgentRegistry::new(sessions);
    // session.new/session.create both materialize before calling spawn_new.
    for _ in 0..2 {
        registry
            .spawn_new(session_id)
            .expect("spawn existing baseline");
        let instance = registry.instances.remove(session_id).expect("running");
        let AgentTransport::ThinAgent { handle, task } = instance.transport;
        handle.shutdown().expect("shutdown");
        task.await.expect("join").expect("clean shutdown");
    }
    assert_eq!(
        std::fs::read(dir.join("committed-batches/batches.jsonl")).expect("journal"),
        journal_before,
        "spawning and reopening must preserve the committed baseline"
    );
    assert!(!dir.join(EVENTS_FILE).exists());
    assert!(!dir.join(EVENTS_COMMIT_FILE).exists());
}

#[test]
fn v2_residency_overlay_tracks_worker_lifecycle() {
    let root_dir = std::env::temp_dir().join(format!(
        "qaqh-registry-v2-residency-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let sessions = Arc::new(qaqh_session::SessionManager::new_for_test(
        root_dir.join("sessions"),
        root_dir.join(".active_session"),
    ));
    let identity = sessions
        .allocate_session(None)
        .expect("allocate root session");
    let session_id = identity.session_id.as_str().to_string();
    let session_dir = sessions.session_path_dir(&session_id);
    crate::service::materialize_canonical_session_in(&session_dir, "/tmp", "test-model", None)
        .expect("materialize canonical session");

    let hub = Arc::new(V2ProjectionHub::new("registry-residency-test"));
    let mut registry = AgentRegistry::new(sessions.clone());
    registry
        .register_root_agent(&session_id, unix_ms())
        .expect("register root");
    registry.attach_v2_projection(hub.clone());
    test_runtime().block_on(async {
        registry
            .get_or_spawn(&session_id)
            .expect("spawn root worker");
    });

    let loaded = hub
        .bootstrap(&session_dir, &session_id)
        .expect("bootstrap loaded root");
    assert_eq!(
        loaded
            .projections
            .team
            .agents
            .iter()
            .find(|agent| agent.agent_id.as_str() == session_id)
            .expect("root roster entry")
            .residency,
        TeamAgentResidency::Loaded
    );

    registry.close(&session_id);
    let unloaded = hub
        .bootstrap(&session_dir, &session_id)
        .expect("bootstrap unloaded root");
    assert_eq!(
        unloaded
            .projections
            .team
            .agents
            .iter()
            .find(|agent| agent.agent_id.as_str() == session_id)
            .expect("root roster entry")
            .residency,
        TeamAgentResidency::Unloaded
    );
}

#[test]
fn list_agents_uses_explicit_residency_and_parent_terminal_status() {
    use qaqh_session::canonical::{CanonicalLog, WriterId, generate_ulid};
    use qaqh_session::session_fact_v2::{
        EventId, FactSchema, SessionCreated, SubagentSpawned, ToolCallId,
    };

    let root_dir = std::env::temp_dir().join(format!(
        "qaqh-registry-list-state-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let sessions = Arc::new(qaqh_session::SessionManager::new_for_test(
        root_dir.join("sessions"),
        root_dir.join(".active_session"),
    ));
    let root_identity = sessions
        .allocate_session(None)
        .expect("allocate root session");
    let root = root_identity.session_id.as_str().to_string();
    let child = SessionId::new("0198f1a0-0000-7000-8000-000000000011");
    let child_path = AgentPath::parse_absolute("/root/review").expect("child path");
    let now = unix_ms();

    let mut registry = AgentRegistry::new(sessions.clone());
    registry
        .register_root_agent(&root, now)
        .expect("register root metadata");
    registry
        .agent_catalog
        .register(AgentMetadata {
            root_session_id: SessionId::new(root.clone()),
            agent_id: child.clone(),
            agent_path: child_path.clone(),
            parent_agent_path: Some(AgentPath::root()),
            nickname: None,
            role: Some("review".to_string()),
            created_at_ms: now,
        })
        .expect("register child metadata");
    registry
        .residency
        .insert(child.as_str().to_string(), ListedAgentResidency::Unloaded);

    let parent_dir = sessions.session_path_dir(&root);
    let identity =
        CanonicalSessionIdentity::open(&parent_dir).expect("open root canonical identity");
    let mut log = CanonicalLog::open(
        &parent_dir,
        identity.session_id.clone(),
        identity.log_id.clone(),
    )
    .expect("open root canonical log");
    let lease = log
        .acquire_writer(WriterId::new("registry-list-state"), now, 10_000)
        .expect("acquire writer");
    for (fact_seq, payload) in [
        (
            1,
            FactPayload::SessionCreated(SessionCreated {
                created_at_ms: now,
                cwd: "/".to_string(),
                model: "test-model".to_string(),
                parent_session_id: None,
                schema_caps: vec![],
            }),
        ),
        (
            2,
            FactPayload::SubagentSpawned(SubagentSpawned {
                child_session_id: child.clone(),
                parent_call_id: ToolCallId::new(format!("call_{}", generate_ulid())),
                parent_agent_path: Some(AgentPath::root()),
                child_agent_path: Some(child_path.clone()),
                role: Some("review".to_string()),
                spawn_config: None,
                spawned_at_ms: now,
            }),
        ),
        (
            3,
            FactPayload::SubagentFinished(qaqh_session::session_fact_v2::SubagentFinished {
                child_session_id: child.clone(),
                parent_call_id: ToolCallId::new(format!("call_{}", generate_ulid())),
                status: SubagentTerminalStatus::Completed,
                result_ref: None,
                finished_at_ms: now,
                recovery_ref: None,
            }),
        ),
    ] {
        log.append(
            &lease,
            SessionFact {
                schema: FactSchema::v2(),
                session_id: identity.session_id.clone(),
                log_id: identity.log_id.clone(),
                fact_seq,
                event_id: EventId::new(generate_ulid()),
                ts_ms: now,
                causation_id: None,
                turn_id: None,
                call_id: None,
                interaction_id: None,
                payload,
            },
            now,
        )
        .expect("append canonical agent fact");
    }
    drop(log);

    // 结果面：collector 经 `SubagentHost::finish_subagent` 落到 agent 状态上，
    // `list_agents` 直接返回，不再需要 `process check`。
    registry.record_subagent_outcome(child.as_str(), "final answer body", 0);

    let listed = registry
        .list_agents_with_state_for_caller(&root, "/root")
        .expect("list agents with state");
    let child_state = listed
        .iter()
        .find(|agent| agent.agent_id == child.as_str())
        .expect("child listing");
    assert_eq!(child_state.status, ListedAgentStatus::Completed);
    assert_eq!(child_state.residency, ListedAgentResidency::Unloaded);
    let outcome = child_state
        .outcome
        .as_ref()
        .expect("outcome on the status plane");
    assert_eq!(outcome.answer.as_deref(), Some("final answer body"));
    assert_eq!(outcome.exit_code, 0);
}

#[tokio::test]
async fn subscription_actor_is_idempotent_and_shutdown_closes_ingress() {
    let mut instance = dummy_thin_agent("seed-subscription");
    let connection_id = ConnectionId::new("connection-1");

    assert_eq!(
        instance.apply_subscription(SubscriptionCommand::Subscribe {
            connection_id: connection_id.clone(),
            channel: RingingChannel::Control,
        }),
        Ok(SubscriptionEffect::Subscribed {
            connection_id: connection_id.clone(),
            channel: RingingChannel::Control,
            changed: true,
        })
    );
    assert_eq!(
        instance.apply_subscription(SubscriptionCommand::Subscribe {
            connection_id: connection_id.clone(),
            channel: RingingChannel::Control,
        }),
        Ok(SubscriptionEffect::Subscribed {
            connection_id: connection_id.clone(),
            channel: RingingChannel::Control,
            changed: false,
        })
    );
    assert_eq!(
        instance.apply_subscription(SubscriptionCommand::ConnectionClosed {
            connection_id: connection_id.clone(),
        }),
        Ok(SubscriptionEffect::ConnectionClosed {
            connection_id: connection_id.clone(),
            removed: 1,
        })
    );

    instance.signal_shutdown();
    assert!(
        instance
            .apply_subscription(SubscriptionCommand::Subscribe {
                connection_id,
                channel: RingingChannel::Tool,
            })
            .is_err()
    );
}

#[test]
fn registry_subscription_ingress_is_connection_scoped_and_shutdown_is_terminal() {
    let directory = tempfile::tempdir().expect("tempdir");
    let sessions = Arc::new(qaqh_session::SessionManager::new_for_test(
        directory.path().join("sessions"),
        directory.path().join(".active_session"),
    ));
    let instance = dummy_thin_agent("seed-registry");
    let mut registry = AgentRegistry {
        instances: HashMap::from([("seed-registry".into(), instance)]),
        activity: SessionActivityTracker::default(),
        sessions,
        hub: None,
        shutting_down: false,
        last_spawn: HashMap::new(),
        supervisor: SubagentSupervisor::default(),
        agent_catalog: AgentCatalog::default(),
        residency: HashMap::new(),
        v2_hub: None,
        quota_ledgers: HashMap::new(),
        quota_limits: QuotaLimits::unlimited(),
        max_depth: 1,
        message_in_flight_per_pair: 16,
        message_outbound_per_sender: 1024,
        outbound_attempts: HashMap::new(),
        armed_collectors: HashSet::new(),
        subagent_outcomes: HashMap::new(),
    };
    let connection_id = ConnectionId::new("connection-registry");

    assert_eq!(
        registry.subscribe_channel(
            "seed-registry",
            connection_id.clone(),
            RingingChannel::Control
        ),
        Ok(true)
    );
    assert_eq!(
        registry.subscribe_channel(
            "seed-registry",
            connection_id.clone(),
            RingingChannel::Control
        ),
        Ok(false)
    );
    assert_eq!(
        registry.unsubscribe_channel(
            "seed-registry",
            connection_id.clone(),
            RingingChannel::Control
        ),
        Ok(true)
    );
    assert_eq!(
        registry.subscribe_channel("seed-registry", connection_id.clone(), RingingChannel::Tool),
        Ok(true)
    );
    assert_eq!(
        registry.connection_closed("seed-registry", connection_id.clone()),
        Ok(1)
    );

    registry.close("seed-registry");
    assert_eq!(
        registry.connection_closed("seed-registry", connection_id),
        Ok(0)
    );
}

#[test]
fn parent_cancel_uses_token_tree_and_still_delivers_child_terminal_command() {
    let directory = tempfile::tempdir().expect("tempdir");
    let sessions = Arc::new(qaqh_session::SessionManager::new_for_test(
        directory.path().join("sessions"),
        directory.path().join(".active_session"),
    ));
    let parent_cancel = crate::agent::types::CancelToken::new();
    let child_cancel = parent_cancel.child();
    let parent = dummy_thin_agent("parent-seed");
    let child = dummy_thin_agent("child-seed");
    let mut registry = AgentRegistry {
        instances: HashMap::from([("parent-seed".into(), parent), ("child-seed".into(), child)]),
        activity: SessionActivityTracker::default(),
        sessions,
        hub: None,
        shutting_down: false,
        last_spawn: HashMap::new(),
        supervisor: SubagentSupervisor::default(),
        agent_catalog: AgentCatalog::default(),
        residency: HashMap::new(),
        v2_hub: None,
        quota_ledgers: HashMap::new(),
        quota_limits: QuotaLimits::unlimited(),
        max_depth: 1,
        message_in_flight_per_pair: 16,
        message_outbound_per_sender: 1024,
        outbound_attempts: HashMap::new(),
        armed_collectors: HashSet::new(),
        subagent_outcomes: HashMap::new(),
    };
    registry
        .link_subagent("parent-seed", "child-seed")
        .expect("link subagent");
    let cancel = qaqh_ringing::RingingWorkerCommandEnvelope::new(
        "parent-seed",
        "cancel-parent",
        qaqh_ringing::RingingCommand::Conversation(
            qaqh_domain::ConversationCommand::ConversationCancel { turn_id: None },
        ),
    );

    registry
        .send_ringing("parent-seed", &cancel)
        .expect("cancel parent");

    qaqh_workspace::remove_session_cancel("parent-seed");
    qaqh_workspace::remove_session_cancel("child-seed");
}

#[test]
fn targeted_cancel_does_not_preset_the_cancel_token() {
    let directory = tempfile::tempdir().expect("tempdir");
    let sessions = Arc::new(qaqh_session::SessionManager::new_for_test(
        directory.path().join("sessions"),
        directory.path().join(".active_session"),
    ));
    let cancel_token = crate::agent::types::CancelToken::new();
    let instance = dummy_thin_agent("seed-a");
    let mut registry = AgentRegistry {
        instances: HashMap::from([("seed-a".into(), instance)]),
        activity: SessionActivityTracker::default(),
        sessions,
        hub: None,
        shutting_down: false,
        last_spawn: HashMap::new(),
        supervisor: SubagentSupervisor::default(),
        agent_catalog: AgentCatalog::default(),
        residency: HashMap::new(),
        v2_hub: None,
        quota_ledgers: HashMap::new(),
        quota_limits: QuotaLimits::unlimited(),
        max_depth: 1,
        message_in_flight_per_pair: 16,
        message_outbound_per_sender: 1024,
        outbound_attempts: HashMap::new(),
        armed_collectors: HashSet::new(),
        subagent_outcomes: HashMap::new(),
    };
    // §7：显式 turn_id 的取消必须先经 actor 目标校验——入队不得预置取消
    // 标记，滞留/错目标的旧请求在入队瞬间不得中止在途工具。
    let targeted = qaqh_ringing::RingingWorkerCommandEnvelope::new(
        "seed-a",
        "cancel-targeted",
        qaqh_ringing::RingingCommand::Conversation(
            qaqh_domain::ConversationCommand::ConversationCancel {
                turn_id: Some("turn-stale".into()),
            },
        ),
    );
    registry
        .send_ringing("seed-a", &targeted)
        .expect("targeted cancel delivered");
    assert!(
        !cancel_token.is_set(),
        "targeted cancel must not preset the token before actor-side validation"
    );
    qaqh_workspace::remove_session_cancel("seed-a");
}

#[test]
fn message_quota_rejects_in_flight_and_outbound_over_limit() {
    let temp = tempfile::tempdir().expect("tempdir");
    let sessions = Arc::new(qaqh_session::SessionManager::new_for_test(
        temp.path().join("sessions"),
        temp.path().join("active.json"),
    ));
    let mut registry = AgentRegistry::new(sessions);
    registry.set_message_quota_limits(2, 3);

    assert!(
        registry.admit_outbound_message("root", "/root", 2).is_err(),
        "in-flight at limit must reject"
    );
    assert!(registry.admit_outbound_message("root", "/root", 0).is_ok());
    assert!(registry.admit_outbound_message("root", "/root", 0).is_ok());
    assert!(registry.admit_outbound_message("root", "/root", 0).is_ok());
    assert!(
        registry.admit_outbound_message("root", "/root", 0).is_err(),
        "outbound at limit must reject"
    );
    assert!(
        registry
            .admit_outbound_message("root", "/root/child", 0)
            .is_ok(),
        "each author has its own outbound budget"
    );
    assert!(
        registry
            .admit_outbound_message("root-2", "/root", 0)
            .is_ok(),
        "each root tree has its own outbound budget"
    );
}
