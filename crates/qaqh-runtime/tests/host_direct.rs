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
use qaqh_session::canonical::{CanonicalSessionIdentity, CommittedFactReader};
use qaqh_session::projection::{MailboxProjection, Projection};
use qaqh_session::session_fact_v2::{FactPayload, MailboxMessageState};
use qaqh_subagent::{SendAgentMessageRequest, SpawnSubagentRequest, SubagentHost};

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
    let batch = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("subscribe must deliver the seed's event batch");
    assert_eq!(batch.seed, seed, "batch must carry the sub seed");
    assert_eq!(batch.channel, RingingChannel::Control);
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
}
