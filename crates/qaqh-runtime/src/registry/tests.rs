use super::*;

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

/// A missing model selection cannot silently start the global/default model.
#[tokio::test]
async fn spawn_failure_preserves_the_session_baseline() {
    let owners = test_owner_dir();
    owners
        .ensure_session("strict-model", None, false)
        .expect("business row");
    owners
        .get_or_create("strict-model")
        .update_metadata(qaqh_session::db::store::SessionColumnPatch {
            model_selection: Some(qaqh_types::SettingValue::Use {
                value: qaqh_types::ModelSelection {
                    provider_id: "missing-provider-for-regression".into(),
                    model_id: "missing-model".into(),
                    options: Default::default(),
                },
            }),
            ..Default::default()
        })
        .expect("invalid selection fixture");
    let mut registry = AgentRegistry::new(owners);
    for _ in 0..2 {
        assert!(registry.spawn_new("strict-model").is_err());
        assert!(!registry.is_running("strict-model"));
    }
    assert_eq!(
        registry.owners.session_parent("strict-model").expect("row"),
        None
    );
}

/// 结果面:collector 经 `SubagentHost::finish_subagent` 落到 agent 状态上,
/// `list_agents` 直接返回终态,unloaded 与 completed 正交。
#[test]
fn list_agents_reports_recorded_outcome_and_residency() {
    let root_dir = std::env::temp_dir().join(format!(
        "qaqh-registry-list-state-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let sessions = Arc::new(qaqh_session::SessionManager::new(root_dir.clone()));
    let root = sessions.allocate_session();
    let child = SessionId::new("0198f1a0-0000-7000-8000-000000000011");
    let child_path = AgentPath::parse_absolute("/root/review").expect("child path");
    let now = unix_ms();

    let mut registry = AgentRegistry::new(test_owner_dir());
    registry
        .register_root_agent(&root, now)
        .expect("register root metadata");
    // 子会话业务行(血缘 = 根)。
    registry
        .owners
        .ensure_session(child.as_str(), Some(&root), true)
        .expect("ensure child business row");
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

/// 消息配额:in-flight 与累计 outbound 双闸,按 root/author 隔离。
#[test]
fn message_quota_rejects_in_flight_and_outbound_over_limit() {
    let mut registry = AgentRegistry::new(test_owner_dir());
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

/// 测试 owner 目录:tempdir 业务库(dir 泄漏至进程结束,测试进程可接受)。
fn test_owner_dir() -> std::sync::Arc<qaqh_session::owner::OwnerDirectory> {
    let dir = Box::leak(Box::new(tempfile::tempdir().expect("tempdir")));
    std::sync::Arc::new(
        qaqh_session::owner::OwnerDirectory::open(dir.path().join("qaqh.db"))
            .expect("open owners db"),
    )
}
