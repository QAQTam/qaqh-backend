//! Knife-1 step-2 收尾 regression：`QaqhService` 提供进程内 `SubagentHost`。
//!
//! `spawn_subagent` 工具此后经宿主句柄直达 daemon 进程内的
//! AgentRegistry + RingingHub，不再建立 daemon HTTP/SSE 回连。本测试用无模型
//! 的确定性路径验证宿主接口：spawn 返回 seed、send_ringing 命令可直达、
//! subscribe 能从 hub 过滤出该 seed 的事件批次、close 幂等；无 hub 时优雅降级。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use qaqh_domain::{ControlEvent, DomainEvent, InterAgentDelivery, RingingChannel, SessionState};
use qaqh_runtime::{QaqhService, RingingHub};
use qaqh_session::canonical::{
    CanonicalLog, CanonicalSessionIdentity, CommittedFactReader, WriterId, generate_ulid,
};
use qaqh_session::projection::{MailboxProjection, Projection};
use qaqh_session::session_fact_v2::{
    AgentPath, EventId, FactPayload, FactSchema, MailboxMessageState, SessionCreated, SessionFact,
    SessionId, SubagentSpawnConfig, SubagentSpawned, ToolCallId,
};
use qaqh_subagent::{
    InterruptAgentRequest, ListedAgentResidency, ListedAgentStatus, SendAgentMessageRequest,
    SpawnSubagentRequest, SubagentHost, TaskBoardHost, TaskClaimAction, TaskClaimRequest,
    TaskCloseAction, TaskCloseRequest, TaskCreateRequest, TaskListRequest, TaskUpdateAction,
    TaskUpdateRequest, WaitAgentOutcome, WaitAgentRequest,
};

static TEST_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn qaqh_service_host_spawn_subscribe_send_close() {
    let _test_lock = TEST_LOCK.lock().expect("test setup must not fail");
    let root = std::env::temp_dir().join(format!(
        "qaqh-host-direct-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let data = root.join("data");
    std::fs::create_dir_all(&data).expect("test setup must not fail");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).expect("test setup must not fail");
    unsafe {
        std::env::set_var("QAQH_DATA_DIR", &data);
    }
    qaqh_workspace::set_workspace(&ws.to_string_lossy());

    // ── 阶段 1：未 attach hub，宿主能力优雅降级（不 panic / 不阻塞）。──
    qaqh_session::SessionManager::init(qaqh_types::platform::data_dir());
    let service = QaqhService::init(qaqh_session::SessionManager::global());
    let host: &dyn SubagentHost = &service;
    let nohub_rx = host.subscribe("no-hub-seed");
    assert!(
        nohub_rx.recv_timeout(Duration::from_millis(200)).is_err(),
        "subscribe without hub must not block forever"
    );

    // ── 阶段 2：attach hub 后走完整宿主直连路径。──
    let hub = Arc::new(RingingHub::new("qaqh-host-direct-test"));
    service.attach_ringing(hub.clone());
    let host: &dyn SubagentHost = &service;

    // 1. spawn：返回合法 seed/path；不经过 daemon HTTP/SSE。
    let parent_identity = qaqh_session::SessionManager::global()
        .allocate_session(None)
        .expect("allocate canonical parent session");
    let parent = parent_identity.session_id.as_str().to_string();
    let spawned = host
        .spawn_subagent(SpawnSubagentRequest {
            parent_session_id: &parent,
            requested_name: "review_code",
            tools: &[],
            model: None,
            base_url: None,
            max_tokens: None,
            workspace: None,
        })
        .expect("host spawn_subagent must succeed");
    let seed = spawned.seed;
    let child_session_id = spawned.child_session_id;
    assert!(!seed.is_empty(), "host spawn must return a non-empty seed");
    assert_eq!(
        child_session_id, seed,
        "beta identity requires seed == child_session_id == directory"
    );
    assert_eq!(spawned.parent_agent_path, "/root");
    assert_eq!(spawned.child_agent_path, "/root/review_code");

    let child_only = host
        .list_agents(&parent, "review_code")
        .expect("relative prefix must resolve below caller");
    assert_eq!(child_only.len(), 1);
    assert_eq!(child_only[0].agent_id, child_session_id);
    assert_eq!(child_only[0].agent_path, "/root/review_code");
    assert_eq!(child_only[0].parent_agent_path.as_deref(), Some("/root"));
    assert_eq!(child_only[0].status, ListedAgentStatus::PendingInit);
    assert_eq!(child_only[0].residency, ListedAgentResidency::Loaded);

    let root_tree = host
        .list_agents(&child_session_id, "/root")
        .expect("child caller can list its root tree");
    assert_eq!(root_tree.len(), 2);
    assert!(root_tree.iter().any(|agent| agent.agent_path == "/root"));
    assert!(
        root_tree
            .iter()
            .any(|agent| agent.agent_path == "/root/review_code")
    );
    assert!(
        host.list_agents(&parent, "/morpheus").is_err(),
        "cross-namespace listing must fail closed"
    );

    let parent_identity = CanonicalSessionIdentity::open(data.join("sessions").join(&parent))
        .expect("parent canonical identity");
    let child_dir = data.join("sessions").join(&seed);
    let child_identity =
        CanonicalSessionIdentity::open_or_create(&child_dir).expect("child canonical identity");
    assert_eq!(child_identity.session_id.as_str(), child_session_id);
    let child_facts = CommittedFactReader::open(
        &child_dir,
        child_identity.session_id.clone(),
        child_identity.log_id.clone(),
    )
    .expect("child reader")
    .read_all()
    .expect("child facts");
    assert!(
        child_facts.iter().any(|fact| matches!(
            &fact.payload,
            FactPayload::SessionCreated(created)
                if created.parent_session_id.as_ref() == Some(&parent_identity.session_id)
        )),
        "child SessionCreated must carry the canonical parent hint"
    );

    let sent = host
        .send_agent_message(SendAgentMessageRequest {
            caller_session_id: &parent,
            target: "/root/review_code",
            text: "queued inter-agent message",
            delivery: InterAgentDelivery::Queue,
        })
        .expect("send_agent_message");
    assert_eq!(sent.recipient, "/root/review_code");
    assert_eq!(sent.delivery, InterAgentDelivery::Queue);

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let delivered_facts = loop {
        let facts = CommittedFactReader::open(
            &child_dir,
            child_identity.session_id.clone(),
            child_identity.log_id.clone(),
        )
        .expect("child reader")
        .read_all()
        .expect("child facts");
        let has_communication = facts
            .iter()
            .any(|fact| matches!(&fact.payload, FactPayload::InterAgentCommunication(_)));
        let has_input = facts.iter().any(|fact| {
            matches!(
                &fact.payload,
                FactPayload::InputAccepted(payload)
                    if payload.client_request_id.as_deref() == Some(sent.message_id.as_str())
            )
        });
        if has_communication && has_input {
            break facts;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for child mailbox delivery"
        );
        std::thread::sleep(Duration::from_millis(25));
    };
    let mut mailbox = MailboxProjection::default();
    for fact in &delivered_facts {
        mailbox.apply(fact);
    }
    assert_eq!(mailbox.pending_count(), 0);
    assert_eq!(
        mailbox.snapshot().messages[0].state,
        MailboxMessageState::Delivered
    );

    // The parent must be a running actor for wait_agent, matching the real
    // tool-call path. SessionAttach only materializes the actor; the daemon
    // lease layer owns its production semantics.
    host.send_ringing(
        &parent,
        qaqh_ringing::RingingCommand::Control(qaqh_domain::ControlCommand::SessionAttach {
            seed: parent.clone(),
        }),
    )
    .expect("start parent actor before wait_agent");

    let no_cancel = || false;
    assert_eq!(
        host.wait_agent(WaitAgentRequest {
            caller_session_id: &parent,
            timeout: Duration::from_millis(50),
            should_cancel: &no_cancel,
        })
        .expect("wait_agent timeout must be stable"),
        WaitAgentOutcome::TimedOut {
            activity_fact_seq: 0
        }
    );

    let waiter_service = service.clone();
    let waiter_parent = parent.clone();
    let waiter = std::thread::spawn(move || {
        let no_cancel = || false;
        waiter_service.wait_agent(WaitAgentRequest {
            caller_session_id: &waiter_parent,
            timeout: Duration::from_secs(2),
            should_cancel: &no_cancel,
        })
    });
    std::thread::sleep(Duration::from_millis(100));
    host.send_agent_message(SendAgentMessageRequest {
        caller_session_id: &child_session_id,
        target: "/root",
        text: "queue-only activity for the waiting parent",
        delivery: InterAgentDelivery::Queue,
    })
    .expect("child sends queue-only mailbox activity to parent");
    assert!(
        matches!(
            waiter.join().expect("wait thread must not panic"),
            Ok(WaitAgentOutcome::Activity { activity_fact_seq })
                if activity_fact_seq > 0
        ),
        "mailbox activity must wake wait_agent"
    );

    let interrupted = host
        .interrupt_agent(InterruptAgentRequest {
            caller_session_id: &parent,
            target: "/root/review_code",
        })
        .expect("root may interrupt a child turn");
    assert_eq!(interrupted.recipient, "/root/review_code");
    assert!(!interrupted.previous_status.is_empty());
    assert!(
        host.list_agents(&parent, "/root")
            .expect("interrupt must retain logical identity")
            .iter()
            .any(|agent| agent.agent_id == child_session_id),
        "interrupt must not unload or delete the child identity"
    );
    assert!(
        host.interrupt_agent(InterruptAgentRequest {
            caller_session_id: &parent,
            target: "/root",
        })
        .is_err(),
        "root interrupt must be rejected"
    );
    assert!(
        host.interrupt_agent(InterruptAgentRequest {
            caller_session_id: &child_session_id,
            target: "/root/review_code",
        })
        .is_err(),
        "self interrupt must be rejected"
    );
    host.send_agent_message(SendAgentMessageRequest {
        caller_session_id: &parent,
        target: "/root/review_code",
        text: "post-interrupt queue delivery",
        delivery: InterAgentDelivery::Queue,
    })
    .expect("interrupted child must remain available for messages");

    // 2. subscribe：从 hub 过滤该 seed 的事件批次（工具 collect 线程消费）。
    let rx = host.subscribe(&seed);
    // 发布一条属于该 seed 的合成事件（等价 actor 事件进入 hub 的路径）。
    hub.publish_with_causation(
        &seed,
        DomainEvent::Control(ControlEvent::SessionStateChanged {
            seed: seed.clone(),
            state: SessionState::Created,
        }),
        None,
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let batch = loop {
        let batch = rx
            .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
            .expect("subscribe must deliver the seed's event batch");
        if batch.channel == RingingChannel::Control {
            break batch;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for the synthetic control batch"
        );
    };
    assert_eq!(batch.seed, seed, "batch must carry the sub seed");
    assert!(
        batch.envelopes.iter().any(|env| env.seed == seed),
        "batch must contain the published envelope"
    );

    // 3. send_ringing：命令直达 actor 队列（SessionShutdown 触发 actor 优雅退出，
    //    无模型也可安全清理；不依赖 provider）。
    host.send_ringing(
        &seed,
        qaqh_ringing::RingingCommand::Control(qaqh_domain::ControlCommand::SessionShutdown),
    )
    .expect("send_ringing must address the in-process actor");

    // 4. close：幂等（已关闭/已退出均返回 Ok，不 panic）。
    host.close(&seed).expect("first close");
    let _ = host.close(&seed); // 第二次幂等
    host.close(&parent).expect("close parent actor");
}

#[test]
fn delivery_reloads_unloaded_child_through_loaded_parent() {
    let _test_lock = TEST_LOCK.lock().expect("test setup must not fail");
    let root = std::env::temp_dir().join(format!(
        "qaqh-host-reload-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let data = root.join("data");
    std::fs::create_dir_all(&data).expect("test setup must not fail");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).expect("test setup must not fail");
    unsafe {
        std::env::set_var("QAQH_DATA_DIR", &data);
    }
    qaqh_workspace::set_workspace(&ws.to_string_lossy());

    let sessions = Arc::new(qaqh_session::SessionManager::new_for_test(
        data.join("sessions"),
        data.join(".active_session"),
    ));
    let service = QaqhService::init(sessions.clone());
    let hub = Arc::new(RingingHub::new("qaqh-host-reload-test"));
    service.attach_ringing(hub.clone());
    let host: &dyn SubagentHost = &service;

    let parent_identity = sessions
        .allocate_session(None)
        .expect("allocate canonical parent session");
    let parent = parent_identity.session_id.as_str().to_string();
    let spawned = host
        .spawn_subagent(SpawnSubagentRequest {
            parent_session_id: &parent,
            requested_name: "reload_task",
            tools: &["read_file".to_string()],
            model: Some("test-model"),
            base_url: None,
            max_tokens: Some(2048),
            workspace: None,
        })
        .expect("spawn child before unload");
    let child = spawned.child_session_id.clone();
    let child_path = spawned.child_agent_path.clone();
    assert_eq!(child, spawned.seed);

    let parent_dir = sessions.session_path_dir(&parent);
    let parent_canonical = CanonicalSessionIdentity::open(&parent_dir).expect("parent identity");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    let mut log = CanonicalLog::open(
        &parent_dir,
        parent_canonical.session_id.clone(),
        parent_canonical.log_id.clone(),
    )
    .expect("open parent canonical log");
    let lease = log
        .acquire_writer(WriterId::new("host-direct-reload"), now, 10_000)
        .expect("acquire parent writer");
    log.append(
        &lease,
        SessionFact {
            schema: FactSchema::v2(),
            session_id: parent_canonical.session_id.clone(),
            log_id: parent_canonical.log_id.clone(),
            fact_seq: 0,
            event_id: EventId::new(generate_ulid()),
            ts_ms: now,
            causation_id: None,
            turn_id: None,
            call_id: None,
            interaction_id: None,
            payload: FactPayload::SessionCreated(SessionCreated {
                created_at_ms: now,
                cwd: "/".to_string(),
                model: "test-model".to_string(),
                parent_session_id: None,
                schema_caps: vec![],
            }),
        },
        now,
    )
    .expect("append parent SessionCreated");
    log.append(
        &lease,
        SessionFact {
            schema: FactSchema::v2(),
            session_id: parent_canonical.session_id.clone(),
            log_id: parent_canonical.log_id.clone(),
            fact_seq: 0,
            event_id: EventId::new(generate_ulid()),
            ts_ms: now,
            causation_id: None,
            turn_id: None,
            call_id: None,
            interaction_id: None,
            payload: FactPayload::SubagentSpawned(SubagentSpawned {
                child_session_id: SessionId::new(child.clone()),
                parent_call_id: ToolCallId::new(format!("call_{}", generate_ulid())),
                parent_agent_path: Some(AgentPath::root()),
                child_agent_path: Some(
                    AgentPath::parse_absolute(&child_path).expect("canonical child path"),
                ),
                role: Some("reload_task".to_string()),
                spawn_config: Some(SubagentSpawnConfig {
                    tools: vec!["read_file".to_string()],
                    model: Some("test-model".to_string()),
                    base_url: None,
                    max_tokens: Some(2048),
                    ephemeral: false,
                    timeout_secs: 120,
                }),
                spawned_at_ms: now,
            }),
        },
        now,
    )
    .expect("append canonical spawn config");
    drop(log);

    host.send_ringing(
        &parent,
        qaqh_ringing::RingingCommand::Control(qaqh_domain::ControlCommand::SessionAttach {
            seed: parent.clone(),
        }),
    )
    .expect("start parent actor");
    host.close(&child).expect("unload child");
    let listed_after_unload = host
        .list_agents(&parent, "/root")
        .expect("list after unload");
    let child_after_unload = listed_after_unload
        .iter()
        .find(|agent| agent.agent_id == child && agent.agent_path == child_path)
        .expect("unloaded child must retain logical metadata");
    assert_eq!(child_after_unload.residency, ListedAgentResidency::Unloaded);
    assert_eq!(child_after_unload.status, ListedAgentStatus::PendingInit);

    let direct_input = host.send_ringing(
        &child,
        qaqh_ringing::RingingCommand::Conversation(
            qaqh_domain::ConversationCommand::ConversationSendMessage {
                text: "direct app-server input must be rejected".to_string(),
                images: vec![],
                attachments: None,
                message_id: Some(format!("msg_{}", generate_ulid())),
                input_purpose: qaqh_domain::ConversationInputPurpose::TriggerTurn,
                as_system: false,
                inter_agent: None,
                subagent_terminal: None,
            },
        ),
    );
    assert!(
        direct_input
            .as_ref()
            .is_err_and(|error| error.contains("rejects direct ConversationSendMessage")),
        "parent-owned child must reject direct input: {direct_input:?}"
    );
    assert_eq!(
        host.list_agents(&parent, "/root")
            .expect("direct input must not reload the child")
            .into_iter()
            .find(|agent| agent.agent_id == child)
            .expect("child remains listable")
            .residency,
        ListedAgentResidency::Unloaded
    );

    host.send_agent_message(SendAgentMessageRequest {
        caller_session_id: &parent,
        target: &child_path,
        text: "reload without starting a turn",
        delivery: InterAgentDelivery::Queue,
    })
    .expect("queue delivery must reload child through loaded parent");
    let reloaded = host
        .list_agents(&parent, "/root")
        .expect("list after queue reload")
        .into_iter()
        .find(|agent| agent.agent_id == child)
        .expect("reloaded child remains listable");
    assert_eq!(reloaded.residency, ListedAgentResidency::Loaded);
    assert_eq!(reloaded.status, ListedAgentStatus::PendingInit);
    std::thread::sleep(Duration::from_millis(1_500));
    let unloaded = service.unload_idle_sessions(1);
    assert!(
        unloaded.iter().any(|seed| seed == &child),
        "idle reloaded child must be an unload candidate: {unloaded:?}"
    );
    assert_eq!(
        host.list_agents(&parent, "/root")
            .expect("list after idle unload")
            .into_iter()
            .find(|agent| agent.agent_id == child)
            .expect("idle-unloaded child remains listable")
            .residency,
        ListedAgentResidency::Unloaded
    );
    host.send_ringing(
        &parent,
        qaqh_ringing::RingingCommand::Control(qaqh_domain::ControlCommand::SessionAttach {
            seed: parent.clone(),
        }),
    )
    .expect("ensure parent is loaded after idle sweep");

    host.send_agent_message(SendAgentMessageRequest {
        caller_session_id: &parent,
        target: &child_path,
        text: "reload and trigger",
        delivery: InterAgentDelivery::Trigger,
    })
    .expect("trigger delivery must reload child again through loaded parent");

    // The original collector ended when the child unloaded. A Trigger delivery
    // must arm a new collector before the turn so terminal activity can route
    // back to the parent mailbox.
    hub.publish_with_causation(
        &child,
        DomainEvent::Conversation(qaqh_domain::ConversationEvent::TurnCompleted {
            turn_id: "t1".to_string(),
            stop_reason: None,
            usage: None,
        }),
        None,
    );

    let child_dir = sessions.session_path_dir(&child);
    let child_canonical = CanonicalSessionIdentity::open(&child_dir).expect("child identity");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let facts = CommittedFactReader::open(
            &child_dir,
            child_canonical.session_id.clone(),
            child_canonical.log_id.clone(),
        )
        .expect("child reader")
        .read_all()
        .expect("child facts");
        if facts.iter().any(|fact| {
            matches!(
                &fact.payload,
                FactPayload::InterAgentCommunication(payload)
                    if payload.recipient.as_str() == child_path
            )
        }) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for reloaded child delivery"
        );
        std::thread::sleep(Duration::from_millis(25));
    }

    host.close(&child).expect("unload reloaded child");
    host.close(&parent).expect("unload parent");
    assert!(
        host.send_agent_message(SendAgentMessageRequest {
            caller_session_id: &parent,
            target: &child_path,
            text: "must not reload through unloaded parent",
            delivery: InterAgentDelivery::Queue,
        })
        .is_err(),
        "child reload must fail closed when immediate parent is unloaded"
    );
}

#[test]
fn broadcast_targets_and_outbound_quota_are_rejected() {
    let _test_lock = TEST_LOCK.lock().expect("test setup must not fail");
    let root = std::env::temp_dir().join(format!(
        "qaqh-host-quota-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let data = root.join("data");
    std::fs::create_dir_all(&data).expect("test setup must not fail");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).expect("test setup must not fail");
    unsafe {
        std::env::set_var("QAQH_DATA_DIR", &data);
    }
    qaqh_workspace::set_workspace(&ws.to_string_lossy());

    let sessions = Arc::new(qaqh_session::SessionManager::new_for_test(
        data.join("sessions"),
        data.join(".active_session"),
    ));
    let service = QaqhService::init(sessions.clone());
    let hub = Arc::new(RingingHub::new("qaqh-host-quota-test"));
    service.attach_ringing(hub);
    let host: &dyn SubagentHost = &service;

    let parent_identity = sessions
        .allocate_session(None)
        .expect("allocate canonical parent session");
    let parent = parent_identity.session_id.as_str().to_string();
    let spawned = host
        .spawn_subagent(SpawnSubagentRequest {
            parent_session_id: &parent,
            requested_name: "quota_child",
            tools: &[],
            model: None,
            base_url: None,
            max_tokens: None,
            workspace: None,
        })
        .expect("spawn child for quota test");
    let child = spawned.child_session_id;

    let broadcast_error = host
        .send_agent_message(SendAgentMessageRequest {
            caller_session_id: &parent,
            target: "@all",
            text: "broadcast",
            delivery: InterAgentDelivery::Queue,
        })
        .expect_err("broadcast target must be rejected");
    assert!(
        broadcast_error.contains("broadcast"),
        "unexpected broadcast error: {broadcast_error}"
    );

    service.set_message_quota_limits(0, 1);
    host.send_agent_message(SendAgentMessageRequest {
        caller_session_id: &parent,
        target: "/root/quota_child",
        text: "first message",
        delivery: InterAgentDelivery::Queue,
    })
    .expect("first send must be within quota");
    let quota_error = host
        .send_agent_message(SendAgentMessageRequest {
            caller_session_id: &parent,
            target: "/root/quota_child",
            text: "second message",
            delivery: InterAgentDelivery::Queue,
        })
        .expect_err("second send must hit the outbound quota");
    assert!(
        quota_error.contains("outbound message limit"),
        "unexpected quota error: {quota_error}"
    );

    host.close(&child).expect("close child");
    host.close(&parent).expect("close parent");
}

#[test]
fn task_board_host_round_trips_task_lifecycle() {
    let _test_lock = TEST_LOCK.lock().expect("test setup must not fail");
    let root = std::env::temp_dir().join(format!(
        "qaqh-host-task-board-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let data = root.join("data");
    std::fs::create_dir_all(&data).expect("test setup must not fail");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).expect("test setup must not fail");
    unsafe {
        std::env::set_var("QAQH_DATA_DIR", &data);
    }
    qaqh_workspace::set_workspace(&ws.to_string_lossy());

    let sessions = Arc::new(qaqh_session::SessionManager::new_for_test(
        data.join("sessions"),
        data.join(".active_session"),
    ));
    let service = QaqhService::init(sessions.clone());
    let host: &dyn SubagentHost = &service;
    let parent_identity = sessions
        .allocate_session(None)
        .expect("allocate canonical parent session");
    let parent = parent_identity.session_id.as_str().to_string();
    // Register root metadata in the agent catalog.
    host.list_agents(&parent, "/root")
        .expect("root metadata registration");

    let task_host: &dyn TaskBoardHost = &service;
    let task = task_host
        .task_create(TaskCreateRequest {
            caller_session_id: &parent,
            title: "wire the task board",
            description_ref: None,
        })
        .expect("create task");
    assert_eq!(task.state, "open");
    assert_eq!(task.claim_epoch, 0);

    let claimed = task_host
        .task_claim(TaskClaimRequest {
            caller_session_id: &parent,
            task_id: &task.task_id,
            action: TaskClaimAction::Claim,
            reason: None,
        })
        .expect("claim task");
    assert_eq!(claimed.state, "claimed");
    assert_eq!(claimed.claim_epoch, 1);
    assert_eq!(claimed.owner.as_deref(), Some("/root"));

    let acceptance = vec!["tests pass".to_string()];
    let updated = task_host
        .task_update(TaskUpdateRequest {
            caller_session_id: &parent,
            task_id: &task.task_id,
            action: TaskUpdateAction::SetAcceptance,
            depends_on: None,
            artifact_ref: None,
            media_type: None,
            acceptance: Some(&acceptance),
        })
        .expect("set acceptance");
    assert_eq!(updated.acceptance, acceptance);

    let completed = task_host
        .task_close(TaskCloseRequest {
            caller_session_id: &parent,
            task_id: &task.task_id,
            action: TaskCloseAction::Complete,
            result_ref: None,
            reason: None,
        })
        .expect("complete task");
    assert_eq!(completed.state, "completed");

    let listed = task_host
        .task_list(TaskListRequest {
            caller_session_id: &parent,
            state: Some("completed"),
        })
        .expect("list completed tasks");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].task_id, task.task_id);

    let closed = task_host
        .task_close(TaskCloseRequest {
            caller_session_id: &parent,
            task_id: &task.task_id,
            action: TaskCloseAction::Close,
            result_ref: None,
            reason: None,
        })
        .expect("close task");
    assert_eq!(closed.state, "closed");

    host.close(&parent).expect("close parent");
}
