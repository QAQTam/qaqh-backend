//! P2-1b 反证：同一测试进程内并发构造**两套**不同 `Arc<dyn *Host>` 实例
//! 互不串台，且无需进程级 `OnceLock` 串行锁。
//!
//! 旧实现（daemon 装配期 `install_host(Arc<service>)` 全局槽）在本文件必红：
//! `OnceLock::set` 首次安装生效，第二套宿主被静默忽略，所有工具实例都打到
//! 宿主 A——下面的 B 侧断言（task/goal/spawn 的 tag 与计数）逐一失败。
//! 新实现：宿主经 `register*`/构造器显式进入工具实例，两套 manager 各持各的
//! 宿主，并发 8+8 次调用计数精确归位。

#![allow(clippy::unwrap_used)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use qaqh_ringing_contract::state::{GoalStatus, GoalView};
use qaqh_tool_core::tool_api::{
    SandboxMode, SandboxSpec, ToolCallContext, ToolCallSource, ToolContentBlock, ToolProjection,
    TypedTool,
};
use qaqh_workspace::tool_api::CancellationToken;
use qaqh_workspace::{PermissionLevel, ToolManager};

use crate::goal::{CreateGoalArgs, CreateGoalTool, GoalHost};
use crate::host::{
    InterruptAgentRequest, InterruptedAgent, ListedAgent, SentAgentMessage, SpawnSubagentRequest,
    SpawnedSubagent, SubagentHost, TaskBoardHost, TaskBoardTask, TaskCreateRequest, TaskListPage,
};
use crate::task_tools::{TaskCreateArgs, TaskCreateTool};
use crate::{SpawnSubagentArgs, SpawnSubagentTool};

fn ctx(session: &str) -> ToolCallContext {
    ToolCallContext {
        call_id: "call-iso".to_string(),
        session_id: session.to_string(),
        workspace_root: PathBuf::from("/tmp/qaqh-host-isolation"),
        permission_level: PermissionLevel::WorkspaceWrite,
        sandbox: SandboxMode::Main,
        sandbox_spec: SandboxSpec::disabled(),
        exec_default_shell: None,
        exec_max_output_chars: None,
        timeout: Duration::from_secs(5),
        cancellation: CancellationToken::new(),
        progress: None,
        exec_progress: None,
        exec_host: None,
        official_web: None,
        source: ToolCallSource::Model,
    }
}

// ── 打tag宿主（每套实例自带计数）──────────────────────────────────────────

struct TaggedSubagentHost {
    tag: &'static str,
    calls: AtomicUsize,
}

impl TaggedSubagentHost {
    fn new(tag: &'static str) -> Arc<Self> {
        Arc::new(Self {
            tag,
            calls: AtomicUsize::new(0),
        })
    }
    fn take(&self) -> usize {
        self.calls.swap(0, Ordering::SeqCst)
    }
}

impl SubagentHost for TaggedSubagentHost {
    fn spawn_subagent(&self, _request: SpawnSubagentRequest<'_>) -> Result<SpawnedSubagent, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(SpawnedSubagent {
            session_id: format!("child-{}", self.tag),
            child_session_id: format!("child-{}", self.tag),
            parent_agent_path: "/root".to_string(),
            child_agent_path: format!("/root/worker-{}", self.tag),
        })
    }
    fn list_agents(&self, _caller: &str, _prefix: &str) -> Result<Vec<ListedAgent>, String> {
        Ok(Vec::new())
    }
    fn send_agent_message(
        &self,
        request: crate::host::SendAgentMessageRequest<'_>,
    ) -> Result<SentAgentMessage, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(SentAgentMessage {
            message_id: format!("msg-{}", self.tag),
            recipient: request.target.to_string(),
            delivery: request.delivery,
        })
    }
    fn wait_agent(
        &self,
        _request: crate::host::WaitAgentRequest<'_>,
    ) -> Result<crate::host::WaitAgentOutcome, String> {
        Ok(crate::host::WaitAgentOutcome::Settled)
    }
    fn interrupt_agent(
        &self,
        _request: InterruptAgentRequest<'_>,
    ) -> Result<InterruptedAgent, String> {
        Ok(InterruptedAgent {
            recipient: "n/a".to_string(),
            previous_status: "idle".to_string(),
        })
    }
    fn send_ringing(
        &self,
        _session_id: &str,
        _command: qaqh_ringing::RingingCommand,
    ) -> Result<(), String> {
        Ok(())
    }
}

struct TaggedTaskHost {
    tag: &'static str,
    calls: AtomicUsize,
}

impl TaggedTaskHost {
    fn new(tag: &'static str) -> Arc<Self> {
        Arc::new(Self {
            tag,
            calls: AtomicUsize::new(0),
        })
    }
    fn take(&self) -> usize {
        self.calls.swap(0, Ordering::SeqCst)
    }
}

impl TaskBoardHost for TaggedTaskHost {
    fn task_create(&self, request: TaskCreateRequest<'_>) -> Result<TaskBoardTask, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(TaskBoardTask {
            task_id: format!("task-{}", self.tag),
            title: request.title.to_string(),
            state: "open".to_string(),
            owner: None,
            claim_epoch: 0,
            depends_on: Vec::new(),
            artifacts: Vec::new(),
            acceptance: Vec::new(),
            write_scopes: Vec::new(),
            ready: true,
            write_scope_warnings: Vec::new(),
            owner_session_id: None,
            result_ref: None,
            created_at_ms: 0,
            updated_at_ms: 0,
        })
    }
    fn task_claim(
        &self,
        _r: crate::host::TaskClaimRequest<'_>,
    ) -> Result<TaskBoardTask, String> {
        Err("stub".to_string())
    }
    fn task_update(
        &self,
        _r: crate::host::TaskUpdateRequest<'_>,
    ) -> Result<TaskBoardTask, String> {
        Err("stub".to_string())
    }
    fn task_close(
        &self,
        _r: crate::host::TaskCloseRequest<'_>,
    ) -> Result<TaskBoardTask, String> {
        Err("stub".to_string())
    }
    fn task_list(&self, _r: crate::host::TaskListRequest<'_>) -> Result<TaskListPage, String> {
        Err("stub".to_string())
    }
}

struct TaggedGoalHost {
    tag: &'static str,
    calls: AtomicUsize,
}

impl TaggedGoalHost {
    fn new(tag: &'static str) -> Arc<Self> {
        Arc::new(Self {
            tag,
            calls: AtomicUsize::new(0),
        })
    }
    fn take(&self) -> usize {
        self.calls.swap(0, Ordering::SeqCst)
    }
}

impl GoalHost for TaggedGoalHost {
    fn create_goal(
        &self,
        session_id: &str,
        _objective: &str,
        _token_budget: Option<u64>,
    ) -> Result<GoalView, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(GoalView {
            goal_id: format!("goal-{}", self.tag),
            session_id: session_id.to_string(),
            objective: format!("objective-{}", self.tag),
            status: GoalStatus::Active,
            token_budget: None,
            tokens_used: 0,
            time_used_ms: 0,
            created_at_ms: 0,
            updated_at_ms: 0,
        })
    }
    fn update_goal_status(
        &self,
        _s: &str,
        _status: GoalStatus,
    ) -> Result<GoalView, String> {
        Err("stub".to_string())
    }
    fn get_goal(&self, _s: &str) -> Result<Option<GoalView>, String> {
        Ok(None)
    }
}

fn task_create_args() -> TaskCreateArgs {
    // task_tools 私有字段不越模块——按生产入口的 serde 形态构造。
    serde_json::from_value(serde_json::json!({ "title": "slice" })).expect("args")
}

fn task_text(out: &crate::task_tools::TaskOutput) -> String {
    out.model_blocks()
        .into_iter()
        .map(|block| match block {
            ToolContentBlock::Text { text } => text,
            _ => String::new(),
        })
        .collect()
}

// ── 反证主体 ───────────────────────────────────────────────────────────────

#[test]
fn two_host_sets_route_concurrently_without_cross_contamination() {
    let a = TaggedSubagentHost::new("A");
    let b = TaggedSubagentHost::new("B");
    // 两套 ToolManager 各注册各的宿主——旧全局槽时代第二次 install 被忽略，
    // B 的 manager 实际会打到 A（spawn 输出 child-A、计数全归 A），必红。
    let mut mgr_a = ToolManager::new();
    crate::register(&mut mgr_a, Arc::clone(&a) as Arc<dyn SubagentHost>);
    let mut mgr_b = ToolManager::new();
    crate::register(&mut mgr_b, Arc::clone(&b) as Arc<dyn SubagentHost>);
    for name in ["spawn_subagent", "list_agents", "send_message", "wait_agent", "interrupt_agent"] {
        assert!(mgr_a.builtin(name).is_some(), "A manager missing {name}");
        assert!(mgr_b.builtin(name).is_some(), "B manager missing {name}");
    }

    let workers: Vec<_> = ["A", "B"]
        .iter()
        .map(|tag| {
            let host: Arc<dyn SubagentHost> = if *tag == "A" {
                Arc::clone(&a) as Arc<dyn SubagentHost>
            } else {
                Arc::clone(&b) as Arc<dyn SubagentHost>
            };
            std::thread::spawn(move || {
                let tool = SpawnSubagentTool { host };
                for i in 0..8 {
                    let out = tool
                        .run(
                            &ctx(&format!("ses-{tag}-{i}")),
                            SpawnSubagentArgs {
                                task_description: "focused task".to_string(),
                                agent_name: None,
                                context: None,
                                mode: None,
                            },
                        )
                        .expect("tagged host spawn must succeed");
                    assert_eq!(
                        out.session_id,
                        format!("child-{tag}"),
                        "工具实例必须只回自己宿主的数据"
                    );
                }
            })
        })
        .collect();
    for worker in workers {
        worker.join().expect("worker");
    }
    // spawn 8 次 + 任务投递 send 8 次 = 每套宿主恰好 16 次调用，零串台。
    assert_eq!(a.take(), 16, "host A must serve only its own tool instances");
    assert_eq!(b.take(), 16, "host B must serve only its own tool instances");
}

#[test]
fn task_and_goal_tools_carry_their_own_hosts() {
    let task_a = TaggedTaskHost::new("A");
    let task_b = TaggedTaskHost::new("B");
    let goal_a = TaggedGoalHost::new("A");
    let goal_b = TaggedGoalHost::new("B");

    let mut mgr_team = ToolManager::new();
    crate::register_collaboration_tools(&mut mgr_team, Arc::clone(&task_a) as Arc<dyn TaskBoardHost>);
    assert!(mgr_team.builtin("task_create").is_some());

    let out_a = TaskCreateTool {
        host: Arc::clone(&task_a) as Arc<dyn TaskBoardHost>,
    }
    .run(
        &ctx("ses-a"),
        task_create_args(),
    )
    .expect("task host A");
    assert!(task_text(&out_a).contains("task-A"), "host A tag must ride its own output");
    let out_b = TaskCreateTool {
        host: Arc::clone(&task_b) as Arc<dyn TaskBoardHost>,
    }
    .run(
        &ctx("ses-b"),
        task_create_args(),
    )
    .expect("task host B");
    assert!(task_text(&out_b).contains("task-B"), "host B tag must ride its own output");
    assert_eq!(task_a.take(), 1);
    assert_eq!(task_b.take(), 1);

    let goal_out_a = CreateGoalTool {
        host: Arc::clone(&goal_a) as Arc<dyn GoalHost>,
    }
    .run(
        &ctx("ses-a"),
        CreateGoalArgs {
            objective: "ship p2-1b".to_string(),
            token_budget: None,
        },
    )
    .expect("goal host A");
    assert_eq!(
        goal_out_a.goal.as_ref().expect("goal").objective,
        "objective-A"
    );
    let goal_out_b = CreateGoalTool {
        host: Arc::clone(&goal_b) as Arc<dyn GoalHost>,
    }
    .run(
        &ctx("ses-b"),
        CreateGoalArgs {
            objective: "ship p2-1b".to_string(),
            token_budget: None,
        },
    )
    .expect("goal host B");
    assert_eq!(
        goal_out_b.goal.as_ref().expect("goal").objective,
        "objective-B"
    );
    assert_eq!(goal_a.take(), 1);
    assert_eq!(goal_b.take(), 1);
}
