//! `SessionToolHosts`——会话工具面的宿主束（v4.5 P2-1b）。
//!
//! 取代此前的 5 个进程级 `OnceLock` 动态宿主反向槽
//! （SubagentHost / TaskBoardHost / GoalHost / GitxHost / TodoHost）：
//! `QaqhService::init` 在自身构造完成后把同一 `QaqhService` 的多态视图
//! 打包成一份不可变宿主束注入 `AgentRegistry`，`spawn_session_inprocess`
//! 在**注册期**把宿主经闭包构造器交给工具实例。同进程可并存多套宿主束
//! （不同 service/测试实例），互不串台——反证见
//! `crates/qaqh-subagent/src/host_isolation_tests.rs` 与
//! `crates/qaqh-gitx/tests/host_isolation.rs`。

use std::sync::Arc;

use crate::service::QaqhService;

pub(crate) struct SessionToolHosts {
    pub subagent: Arc<dyn qaqh_subagent::SubagentHost>,
    pub task: Arc<dyn qaqh_subagent::TaskBoardHost>,
    pub goal: Arc<dyn qaqh_subagent::goal::GoalHost>,
    pub gitx: Arc<dyn qaqh_gitx::GitxHost>,
    pub todo: Arc<dyn qaqh_workspace::todo::TodoHost>,
}

impl SessionToolHosts {
    /// 由 service 值装配（`QaqhService: Clone` 是 Arc 野心的浅拷贝，
    /// 与旧 daemon 装配 `Arc::new(service.clone())` 五个宿主同源同律）。
    pub(crate) fn from_service(service: &QaqhService) -> Arc<Self> {
        Arc::new(Self {
            subagent: Arc::new(service.clone()),
            task: Arc::new(service.clone()),
            goal: Arc::new(service.clone()),
            gitx: Arc::new(service.clone()),
            todo: Arc::new(service.clone()),
        })
    }

    /// 按 ADR-0033 会话画像产出注册器（原 `AGENT/TEAM/ROOT/ROOT_TEAM_TOOL_
    /// REGISTRARS` 四张 const 表的注册期注入替身，词表组合逐条不变）：
    /// - 子会话·非 team：agent 五件 + gitx；
    /// - 子会话·team：task 五件 + gitx；
    /// - 根会话·非 team：agent 五件 + gitx + goal 三件；
    /// - 根会话·team：agent 五件 + task 五件 + gitx + goal 三件。
    pub(crate) fn registrars(
        &self,
        is_child: bool,
        team_mode: bool,
    ) -> Vec<qaqh_workspace::registration::ToolRegistrar> {
        let mut registrars: Vec<qaqh_workspace::registration::ToolRegistrar> = Vec::new();
        // agent 面：根恒有；子仅在非 team 时有（TEAM 子会话只上协作面，
        // 原四表逐条保序复刻，不得顺手扩面）。
        if !is_child || !team_mode {
            let host = Arc::clone(&self.subagent);
            registrars.push(Arc::new(move |mgr: &mut qaqh_workspace::ToolManager| {
                qaqh_subagent::register(mgr, Arc::clone(&host));
            }));
        }
        // 协作面（task_* 五件）：team_mode 时进入（根/子同律）。
        if team_mode {
            let host = Arc::clone(&self.task);
            registrars.push(Arc::new(move |mgr: &mut qaqh_workspace::ToolManager| {
                qaqh_subagent::register_collaboration_tools(mgr, Arc::clone(&host));
            }));
        }
        // worktree 面：所有会话。
        {
            let host = Arc::clone(&self.gitx);
            registrars.push(Arc::new(move |mgr: &mut qaqh_workspace::ToolManager| {
                qaqh_gitx::register(mgr, Arc::clone(&host));
            }));
        }
        // goal 面：只挂根会话（ADR-0015）。
        if !is_child {
            let host = Arc::clone(&self.goal);
            registrars.push(Arc::new(move |mgr: &mut qaqh_workspace::ToolManager| {
                qaqh_subagent::goal::register(mgr, Arc::clone(&host));
            }));
        }
        registrars
    }
}
