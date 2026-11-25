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
        // P1-2:`update_metadata` 改 async。本用例是 `#[tokio::test]`
        // (current_thread)——泵任务与调用方共用这一条线程,同步等回执必死锁,
        // await 才是正确形态;断言与 fixture 意图不变。
        .await
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

/// ADR-0014：独立结果面退役——listing 的 residency 与状态从 canonical
/// 会话态派生；outcome 不再承载终答（终答经回传消息进父会话 mailbox）。
#[test]
fn list_agents_reports_residency_and_canonical_status() {
    let root = test_owner_dir().allocate_session();
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

    let listed = registry
        .list_agents_with_state_for_caller(&root, "/root")
        .expect("list agents with state");
    let child_state = listed
        .iter()
        .find(|agent| agent.agent_id == child.as_str())
        .expect("child listing");
    // 从未开回合 → canonical NeverStarted → PendingInit。
    assert_eq!(child_state.status, ListedAgentStatus::PendingInit);
    assert_eq!(child_state.residency, ListedAgentResidency::Unloaded);
    assert!(
        child_state.outcome.is_none(),
        "result plane retired: terminal answer rides the canonical return message"
    );
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

/// 2026-11-17 事故回归：客户端连发 ConversationCancel 杀掉 9 个 NotStarted
/// 续跑轮，旧语义把它们全部按闸 3 计成"空回复"，3 连即把 Active goal 误封。
/// Cancelled 是明确的停止意图——不记空、不立即重触发，续跑交周期扫描兜底。
///
/// 反证（非空洞）：回合被播成真实的 goal 续跑轮（input 块带 marker、
/// 终态 Cancelled、无答案无工具）。移除 `route_goal_terminal` 的 Cancelled
/// 短路后旧路径照跑：`automatic=true && empty=true` → `empty_turns` 记成 1，
/// 本测试变红。
#[tokio::test]
async fn cancelled_goal_turn_defers_audit_and_retrigger() {
    use qaqh_ringing_contract::fact::{
        ActorKind, ActorRef, BlockId, ContentValue, InputPurpose, TurnId, TurnTerminal,
    };
    use qaqh_ringing_contract::state::{TimelineEntry, TurnOutcome};

    let owners = test_owner_dir();
    let session_id = "ses-cancel-audit";
    let turn_id = "turn-cancelled-cont";
    owners
        .ensure_session(session_id, None, false)
        .expect("session row");
    owners
        .db()
        .write_blocking(|conn| {
            qaqh_session::db::goals::create_goal(
                conn,
                session_id,
                "goal-cancel-1",
                "keep the goal alive",
                None,
            )
            .map(|row| row.goal_id)
        })
        .expect("goal row");

    let owner = owners.get_or_create(session_id);
    owner
        .start_turn(turn_id, Some("input-goal-cont-1".to_string()))
        .await
        .expect("turn row");
    owner
        .append_block(TimelineEntry::Input {
            block_id: BlockId::new("block-goal-cont-1".to_string()),
            turn_id: Some(TurnId::new(turn_id.to_string())),
            purpose: InputPurpose::TriggerTurn,
            content: ContentValue::Inline {
                text: "<qaqh_goal_continuation goal_id=\"goal-cancel-1\">Continue working toward the active goal</qaqh_goal_continuation>".to_string(),
            },
            attachments: vec![],
            images: vec![],
            inter_agent: None,
            subagent_terminal: None,
            actor: ActorRef {
                kind: ActorKind::System,
                id: "system".into(),
                display_name: None,
            },
        })
        .await
        .expect("goal-cont input block");
    owner
        .finish_turn(
            turn_id,
            "finished",
            TurnOutcome::Finished {
                terminal: TurnTerminal::Cancelled,
                usage: None,
                error: None,
            },
        )
        .await
        .expect("finish cancelled turn");

    let mut registry = AgentRegistry::new(owners.clone());
    registry.route_goal_terminal(&qaqh_agent_runtime::session_loop::TurnTerminalNotice {
        session_id: session_id.to_string(),
        turn_id: turn_id.to_string(),
        terminal: TurnTerminal::Cancelled,
        error: None,
        trigger: None,
    });

    let row = owners
        .db()
        .read_blocking(|conn| qaqh_session::db::goals::get_goal(conn, session_id))
        .expect("read goal")
        .expect("goal row");
    assert_eq!(
        row.status,
        qaqh_ringing_contract::state::GoalStatus::Active,
        "单次取消不得封 goal"
    );
    assert_eq!(row.empty_turns, 0, "Cancelled 不计入空回复");
}

/// P2-0 补漏反证：闲置卸载 **seed 之外**、被 `close` 顺带卸载的子代 `SessionOwner`。
///
/// 现场构造生产泄漏形状：父会话先装配（`last_activity_ms` 更早，达到闲置阈值），
/// 子循环在其后 1.2s 装配（自身未达阈值，不进 seed 快照，模拟父 Idle/子忙
/// 的等待窗口）。旧实现只对 seed 根 `take_session`，子代 owner 滞留
/// `OwnerDirectory`——`Arc::strong_count` 停在 2（目录+本地句柄）必红；
/// 新实现对整棵被卸载树回收，目录放手后只剩本地句柄 == 1 变绿。
#[tokio::test]
async fn idle_unload_reaps_descendant_session_owners() {
    use qaqh_agent_runtime::provider_job::ProviderRequestSpec;
    use qaqh_agent_runtime::session_loop::SessionLoop;
    use qaqh_agent_runtime::tool_job::{ToolPolicySnapshot, WorkspaceToolPort};

    fn assemble_test_loop(
        owners: &std::sync::Arc<qaqh_session::owner::OwnerDirectory>,
        dir: &std::path::Path,
        session_id: &str,
    ) -> std::sync::Arc<SessionLoop> {
        let owner = owners.get_or_create(session_id);
        let manager = std::sync::Arc::new(std::sync::Mutex::new(
            qaqh_workspace::registration::build_tool_manager(&[]),
        ));
        let port = std::sync::Arc::new(WorkspaceToolPort::new(
            session_id,
            manager,
            ToolPolicySnapshot::workspace_write(dir.join("ws")),
            owner.lifecycle(),
            std::sync::Arc::new(qaqh_process_tools::exec_session::ExecSessionManager::new()),
        ));
        let provider = qaqh_gate::ProviderConfig::chat(
            "http://127.0.0.1:1",
            "sk-test",
            "test-model",
            None,
            None,
            qaqh_types::ThinkingParamMode::OpenAi,
            qaqh_types::CacheTokenField::PromptCacheHitTokens,
            true,
            None,
        );
        let spec = ProviderRequestSpec::new(provider, 1024);
        // SessionLoop::new 消费一个 owner Arc——这正好复现生产形状：
        // loop 不 drop，目录外引用就永远存在。
        SessionLoop::new(session_id, owner, port, spec, dir.to_path_buf())
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let owners = std::sync::Arc::new(
        qaqh_session::owner::OwnerDirectory::open(dir.path().join("qaqh.db"))
            .expect("open owners db"),
    );
    owners.ensure_session("ses-idle-parent", None, false).expect("parent row");
    owners
        .ensure_session("ses-idle-child", Some("ses-idle-parent"), false)
        .expect("child row");
    // 本地观察句柄（不计入目录）：断言 strong_count 用。
    let parent_handle = owners.get_or_create("ses-idle-parent");
    let child_handle = owners.get_or_create("ses-idle-child");

    let mut registry = AgentRegistry::new(owners.clone());
    let parent_loop = assemble_test_loop(&owners, dir.path(), "ses-idle-parent");
    // 父循环装配后等过 1s 阈值再装子循环：子代不进 seed 快照，只能被父
    // `close` 顺带卸载——正是被旧实现漏掉的回收对象。
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    let child_loop = assemble_test_loop(&owners, dir.path(), "ses-idle-child");
    registry.instances.insert(
        "ses-idle-parent".to_string(),
        AgentInstance {
            session_id: "ses-idle-parent".to_string(),
            loop_: parent_loop,
        },
    );
    registry.instances.insert(
        "ses-idle-child".to_string(),
        AgentInstance {
            session_id: "ses-idle-child".to_string(),
            loop_: child_loop,
        },
    );
    registry
        .link_subagent("ses-idle-parent", "ses-idle-child")
        .expect("link lineage");

    let unloaded = registry.unload_idle_sessions(1);
    assert_eq!(
        unloaded,
        vec!["ses-idle-parent".to_string()],
        "seed 只应包含达到阈值的父会话"
    );
    // 反证断言：目录已放手，唯一存活引用是本地句柄。旧实现此处子代
    // 目录引用滞留，strong_count == 2 变红。
    assert_eq!(
        std::sync::Arc::strong_count(&parent_handle),
        1,
        "seed 根 owner 目录引用必须摘除（v4.4 P1-2 既有行为）"
    );
    assert_eq!(
        std::sync::Arc::strong_count(&child_handle),
        1,
        "被顺带卸载的子代 owner 目录引用必须一并摘除（P2-0 补漏）"
    );
    assert!(
        owners.take_session("ses-idle-child").is_none(),
        "子代目录项已被回收，重复 take 应落空"
    );
}
