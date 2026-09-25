use std::collections::{HashMap, HashSet};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use qaqh_domain::RingingChannel;
use qaqh_session::actor::{
    ConnectionId, SessionActor, SessionActorEffect, SessionCommand, SubscriptionCommand,
    SubscriptionEffect,
};
use qaqh_session::canonical::{
    CANONICAL_IDENTITY_FILE, CanonicalSessionIdentity, CommittedFactReader, EVENTS_COMMIT_FILE,
    EVENTS_FILE,
};
use qaqh_session::projection::AgentGraphSnapshot;
use qaqh_session::session_fact_v2::{
    AgentMetadata, AgentPath, FactPayload, SessionFact, SessionId, SubagentSpawnConfig,
    SubagentTerminalStatus,
};
use qaqh_subagent::{ListedAgent, ListedAgentResidency, ListedAgentStatus};

use crate::agent::SubagentSpawnSpec;
use crate::agent_catalog::AgentCatalog;
use crate::quota_ledger::{QuotaKind, QuotaLedger, QuotaLimits, QuotaReservation, ReleaseReason};
use crate::subagent_supervisor::{LifecycleEvent, SubagentSupervisor};
use crate::{RingingHub, SessionActivityTracker};

static SYSTEM_PATH: OnceLock<String> = OnceLock::new();

pub fn cache_system_path() {
    // `mut` 只被下方 Windows 注册表探测分支消费（非 Windows 编译只读）。
    #[cfg_attr(not(target_os = "windows"), allow(unused_mut))]
    let mut path = std::env::var("PATH").unwrap_or_default();
    #[cfg(target_os = "windows")]
    for key in [
        r"HKCU\Environment",
        r"HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Environment",
    ] {
        let mut command = background_command("reg");
        command.args(["query", key, "/v", "Path"]);
        if let Some(output) = probe_output(command, PROBE_TIMEOUT) {
            let text = String::from_utf8_lossy(&output.stdout);
            if let Some(value) = text
                .lines()
                .find(|line| line.contains("REG_"))
                .and_then(|line| {
                    line.split_once("REG_EXPAND_SZ")
                        .or_else(|| line.split_once("REG_SZ"))
                })
                .map(|(_, value)| value.trim())
            {
                for segment in value.split(';').filter(|value| !value.is_empty()) {
                    if !path
                        .split(';')
                        .any(|current| current.eq_ignore_ascii_case(segment))
                    {
                        if !path.is_empty() {
                            path.push(';')
                        }
                        path.push_str(segment)
                    }
                }
            }
        }
    }
    let _ = SYSTEM_PATH.set(path.clone());
    unsafe {
        std::env::set_var("PATH", path);
    }
}

/// 启动期壳探测（exec 的可用性口径与实际派生同源）。幂等，可重复调用。
pub fn detect_shell() {
    let shell = qaqh_workspace::bootstrap_exec_shell();
    log::info!("[runtime] exec shell bootstrap: {shell}");
}

/// 探测型命令的单次超时上界。
///
/// `--version` / `uname` 这类探测本该毫秒级返回，2s 已是极大宽限。
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// 整份工具快照（6 次探测）的总预算；超出即停止探测、用已有结果降级。
///
/// 探测结果只喂给提示词里的工具快照（`crate::agent::prompt::TOOLS_INFO`），
/// **不值得为它拖慢启动**——何况它跑在 daemon 开始服务之前。
const PROBE_BUDGET: Duration = Duration::from_secs(6);

/// 等子进程退出时的轮询间隔。
const PROBE_POLL: Duration = Duration::from_millis(10);

pub fn detect_os_info() {
    #[cfg(target_os = "windows")]
    let info = {
        let mut command = background_command("cmd");
        command.args(["/d", "/c", "ver"]);
        probe_output(command, PROBE_TIMEOUT)
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| format!("windows {}", std::env::consts::ARCH))
    };
    #[cfg(not(target_os = "windows"))]
    let info = {
        let mut command = Command::new("uname");
        command.arg("-a");
        probe_output(command, PROBE_TIMEOUT)
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| format!("{} {}", std::env::consts::OS, std::env::consts::ARCH))
    };
    let _ = crate::agent::prompt::OS_INFO.set(info);
    // Toolchain snapshot. Each probe lists candidate program names tried in
    // order (first success wins): e.g. Windows ships `python` while most
    // Linux distros only provide `python3`.
    let tool_probes: [(&[&str], &[&str]); 6] = [
        (&["git"], &["--version"]),
        (&["cargo"], &["--version"]),
        (&["node"], &["--version"]),
        (&["python", "python3"], &["--version"]),
        (&["rustc"], &["--version"]),
        (&["pnpm"], &["--version"]),
    ];
    let budget = Instant::now() + PROBE_BUDGET;
    let mut tools = Vec::new();
    'probes: for (programs, args) in tool_probes {
        for program in programs {
            let mut command = background_command(program);
            command.args(args);
            if let Some(output) = probe_output(command, PROBE_TIMEOUT) {
                let value = if output.stdout.is_empty() {
                    &output.stderr
                } else {
                    &output.stdout
                };
                let value = String::from_utf8_lossy(value).trim().to_string();
                if !value.is_empty() {
                    tools.push(value);
                    break;
                }
            }
            if Instant::now() >= budget {
                log::warn!(
                    "[runtime] 工具探测超出 {PROBE_BUDGET:?} 总预算，放弃剩余探测（已得 {} 项）",
                    tools.len()
                );
                break 'probes;
            }
        }
    }
    let _ = crate::agent::prompt::TOOLS_INFO.set(tools.join(", "));
}

/// 跑一个探测型命令并取回输出，**带超时**。
///
/// 为什么不能用 `Command::output()`：它的契约是「等子进程退出 **且** 把
/// stdout/stderr 管道读到 EOF」。只要有任何后代进程继承并持有管道的写端，
/// EOF 就永不出现——**即便被探测的程序本身早已退出**。把它放在 daemon 开始
/// 服务**之前**的同步路径上，就得到一个不报错、不退出、无日志的永久挂起
/// （BUG-2026-09-15-02）。
///
/// 这里三重设防：stdin 接空设备（不再有交互等待）、等退出用轮询限时、读管道
/// 另起线程且同样限时。任一环节超时即 `kill` 并回收子进程、返回 `None`——
/// **探测失败必须降级，不得阻塞启动**。
fn probe_output(mut command: Command, timeout: Duration) -> Option<Output> {
    let deadline = Instant::now() + timeout;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let stdout_rx = drain(child.stdout.take());
    let stderr_rx = drain(child.stderr.take());

    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait(); // 回收，避免僵尸
                    log::warn!(
                        "[runtime] 探测命令超时（{timeout:?}），已终止并跳过：{:?}",
                        command.get_program()
                    );
                    return None;
                }
                std::thread::sleep(PROBE_POLL);
            }
            Err(_) => return None,
        }
    };

    // 子进程已退出 **≠** 管道已到 EOF：后代可能仍持有写端（同一失败模式在
    // `qaqh-workspace/src/process_registry.rs:354` 已有认知）。故这里同样限时，
    // 拿多少算多少——绝不回到无限等待。
    let left = deadline.saturating_duration_since(Instant::now());
    Some(Output {
        status,
        stdout: stdout_rx.recv_timeout(left).unwrap_or_default(),
        stderr: stderr_rx.recv_timeout(left).unwrap_or_default(),
    })
}

/// 另起线程把 `reader` 读干，结果经 channel 回传。
///
/// 读取必须离开主线程：`read_to_end` 会一直阻塞到 EOF，而 EOF 正是此处不可信
/// 的东西。读线程可能永远收不到 EOF（后代持有写端），但它只是**有界**地泄漏
/// 一个线程，而主线程的等待是限时的。
fn drain<R: std::io::Read + Send + 'static>(
    reader: Option<R>,
) -> std::sync::mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = std::sync::mpsc::channel();
    if let Some(mut reader) = reader {
        let _ = std::thread::Builder::new()
            .name("qaqh-probe-io".to_string())
            .spawn(move || {
                let mut buf = Vec::new();
                let _ = reader.read_to_end(&mut buf);
                let _ = tx.send(buf);
            });
    }
    rx
}

fn background_command(program: &str) -> Command {
    // `mut` 只被 Windows 的 CREATE_NO_WINDOW 调整消费（非 Windows 编译只读）。
    #[cfg_attr(not(target_os = "windows"), allow(unused_mut))]
    let mut command = Command::new(program);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

/// How an agent worker is attached to this daemon process.
enum AgentTransport {
    /// Knife-1 in-process worker: the Ringing Loop runs on a daemon thread and
    /// communicates through the same WorkerCommand/WriterEvent channel types as
    /// the pipe boundary.
    InProcess {
        cmd_tx: SyncSender<crate::agent::types::WorkerCommand>,
        cancel: crate::agent::types::CancelToken,
    },
}

enum AgentKind {
    Session,
    Subagent(SubagentSpawnSpec),
}

pub struct AgentInstance {
    seed: String,
    transport: AgentTransport,
    kind: AgentKind,
    /// P2-2d-b migration bridge: daemon-side logical subscription mailbox.
    /// The worker `TurnActor` remains authoritative for turn state until the
    /// actors are consolidated.
    subscription_actor: SessionActor,
    /// Idle-unload liveness (shared with the Loop actor). `None` for legacy
    /// process workers — they are not idle-unload candidates.
    liveness: Option<std::sync::Arc<crate::agent::liveness::WorkerLiveness>>,
    /// Event consumer thread (stdout reader for process workers, event channel
    /// reader for in-process actors). daemon 关闭时必须 join：worker 退出 ≠
    /// 尾部 intent（含 seal_turn）已消费——管道/通道里的最后几个事件仍由
    /// 本线程读取并 publish（见 shutdown）。
    reader: Option<std::thread::JoinHandle<()>>,
    /// In-process loop thread. `None` for process workers.
    thread: Option<std::thread::JoinHandle<()>>,
}

/// Logical identity allocated for a newly spawned subagent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnedSubagentInfo {
    pub child_session_id: SessionId,
    pub parent_agent_path: AgentPath,
    pub child_agent_path: AgentPath,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct SubagentSpawnOptions<'a> {
    pub(crate) tools: &'a [String],
    pub(crate) model: Option<&'a str>,
    pub(crate) base_url: Option<&'a str>,
    pub(crate) max_tokens: Option<u32>,
    pub(crate) ephemeral: bool,
}

pub(crate) struct CollectorArmSpec {
    pub(crate) child_session_id: String,
    pub(crate) name: String,
    pub(crate) parent_session_id: String,
    pub(crate) parent_call_id: String,
    pub(crate) timeout_secs: u64,
    pub(crate) root_session_id: String,
    pub(crate) parent_agent_path: String,
    pub(crate) child_agent_path: String,
}

fn listed_terminal_status(status: SubagentTerminalStatus) -> ListedAgentStatus {
    match status {
        SubagentTerminalStatus::Completed => ListedAgentStatus::Completed,
        SubagentTerminalStatus::Failed | SubagentTerminalStatus::TimedOut => {
            ListedAgentStatus::Errored
        }
        SubagentTerminalStatus::Cancelled => ListedAgentStatus::Interrupted,
    }
}

pub struct AgentRegistry {
    instances: HashMap<String, AgentInstance>,
    activity: SessionActivityTracker,
    /// 会话存储句柄（PR-3-1 注入化；spawn 诊断读 meta 用）。
    sessions: Arc<qaqh_session::SessionManager>,
    /// Ringing 运行时；None = 未启用 legacy worker-only 模式。
    hub: Option<Arc<RingingHub>>,
    /// daemon 正在关闭：worker 退出是预期的，禁止自动重生。
    shutting_down: bool,
    /// 最近一次 spawn 时间（防崩溃-重启风暴：同一 seed 1 秒内不重复拉起）。
    last_spawn: HashMap<String, std::time::Instant>,
    /// P2-5：daemon 级 parent/child edge 与 unload 顺序状态机。
    supervisor: SubagentSupervisor,
    /// Subagent V2：逻辑 agent metadata；worker handle 仍由 `instances` 管理。
    agent_catalog: AgentCatalog,
    /// Explicit residency state for logical agents. Kept separate from
    /// `instances` so `list_agents` reads lifecycle state rather than guessing
    /// from the worker table.
    residency: HashMap<String, ListedAgentResidency>,
    /// P2-7：root session tree 的 durable quota owner。
    quota_ledgers: HashMap<String, QuotaLedger>,
    quota_limits: QuotaLimits,
    /// Child result collectors currently armed for a running/reloaded turn.
    armed_collectors: HashSet<String>,
}

impl AgentRegistry {
    pub fn new(sessions: Arc<qaqh_session::SessionManager>) -> Self {
        Self {
            instances: HashMap::new(),
            activity: SessionActivityTracker::default(),
            sessions,
            hub: None,
            shutting_down: false,
            last_spawn: HashMap::new(),
            supervisor: SubagentSupervisor::default(),
            agent_catalog: AgentCatalog::default(),
            residency: HashMap::new(),
            quota_ledgers: HashMap::new(),
            quota_limits: QuotaLimits::unlimited(),
            armed_collectors: HashSet::new(),
        }
    }

    /// 挂载 Ringing 运行时。Ringing worker 事件只进入 native hub。
    pub fn attach_ringing(&mut self, hub: Arc<RingingHub>) {
        self.hub = Some(hub);
    }

    /// Register a root session as `/root`.
    ///
    /// This is idempotent for the same root session. A conflicting agent id or
    /// path is rejected instead of silently replacing logical metadata.
    pub fn register_root_agent(
        &mut self,
        seed: &str,
        created_at_ms: i64,
    ) -> Result<AgentMetadata, String> {
        let session_dir = self.sessions.session_path_dir(seed);
        let identity = CanonicalSessionIdentity::open_or_create(&session_dir)
            .map_err(|error| format!("open canonical identity for root {seed}: {error}"))?;
        self.agent_catalog
            .register_root_with_alias(identity.session_id, Some(seed), created_at_ms)
            .map_err(|error| error.to_string())
    }

    /// Read logical metadata by stable `AgentId = session_id`.
    pub fn agent_metadata(&self, agent_id: &str) -> Option<AgentMetadata> {
        self.agent_catalog.get_by_id(agent_id).cloned()
    }

    /// Read logical metadata by tree-relative path.
    pub fn agent_metadata_by_path(
        &self,
        root_session_id: &str,
        agent_path: &AgentPath,
    ) -> Option<AgentMetadata> {
        self.agent_catalog
            .get_by_path(root_session_id, agent_path)
            .cloned()
    }

    /// List logical metadata at or below a path prefix within one root tree.
    pub fn list_agents_by_path(
        &self,
        root_session_id: &str,
        prefix: &AgentPath,
    ) -> Vec<AgentMetadata> {
        self.agent_catalog.list_prefix(root_session_id, prefix)
    }

    /// List logical agents for a caller, resolving relative prefixes below the
    /// caller's path and constraining absolute prefixes to the same root tree.
    pub fn list_agents_for_caller(
        &mut self,
        caller_session_id: &str,
        path_prefix: &str,
    ) -> Result<Vec<AgentMetadata>, String> {
        self.ensure_root_metadata(caller_session_id)?;
        let caller = self
            .agent_catalog
            .get_by_id(caller_session_id)
            .cloned()
            .ok_or_else(|| format!("caller agent metadata missing for {caller_session_id}"))?;
        let prefix = caller
            .agent_path
            .resolve(path_prefix)
            .map_err(|error| format!("invalid path prefix {path_prefix:?}: {error}"))?;
        if prefix.namespace() != caller.agent_path.namespace() {
            return Err(format!(
                "path prefix {prefix} crosses agent namespaces from {}",
                caller.agent_path
            ));
        }
        Ok(self
            .agent_catalog
            .list_prefix(caller.root_session_id.as_str(), &prefix))
    }

    /// List logical agents with their explicit status and residency snapshot.
    ///
    /// Status is rebuilt from canonical facts and overlaid with the current
    /// activity tracker while loaded. Residency comes from the registry's
    /// explicit lifecycle map; it is never inferred from the worker table.
    pub fn list_agents_with_state_for_caller(
        &mut self,
        caller_session_id: &str,
        path_prefix: &str,
    ) -> Result<Vec<ListedAgent>, String> {
        let agents = self.list_agents_for_caller(caller_session_id, path_prefix)?;
        let mut listed = Vec::with_capacity(agents.len());
        for agent in agents {
            let residency = self.agent_residency(agent.agent_id.as_str());
            let status = self.listed_agent_status(&agent, residency)?;
            listed.push(ListedAgent {
                root_session_id: agent.root_session_id.as_str().to_string(),
                agent_id: agent.agent_id.as_str().to_string(),
                agent_path: agent.agent_path.as_str().to_string(),
                parent_agent_path: agent
                    .parent_agent_path
                    .as_ref()
                    .map(|path| path.as_str().to_string()),
                nickname: agent.nickname.clone(),
                role: agent.role.clone(),
                status,
                residency,
                created_at_ms: agent.created_at_ms,
            });
        }
        Ok(listed)
    }

    fn agent_residency(&self, agent_id: &str) -> ListedAgentResidency {
        self.residency
            .get(agent_id)
            .copied()
            .unwrap_or(ListedAgentResidency::Unloaded)
    }

    fn listed_agent_status(
        &self,
        agent: &AgentMetadata,
        residency: ListedAgentResidency,
    ) -> Result<ListedAgentStatus, String> {
        if residency == ListedAgentResidency::Loaded
            && let Some(activity) = self.activity.get(agent.agent_id.as_str())
        {
            match activity.state {
                qaqh_domain::ActivityState::Starting => {
                    return Ok(ListedAgentStatus::PendingInit);
                }
                qaqh_domain::ActivityState::Working => {
                    return Ok(ListedAgentStatus::Running);
                }
                qaqh_domain::ActivityState::WaitingUser => {
                    return Ok(ListedAgentStatus::WaitingUser);
                }
                qaqh_domain::ActivityState::Disconnected => {
                    return Ok(ListedAgentStatus::Shutdown);
                }
                qaqh_domain::ActivityState::Idle => {}
            }
        }

        // A terminal child fact lives in the parent log. For an unloaded child
        // it is the authoritative final lifecycle state, even if a later
        // shutdown attempt emitted a generic turn interruption in the child log.
        if !agent.agent_path.is_root()
            && residency == ListedAgentResidency::Unloaded
            && let Some(status) = self.parent_terminal_status(agent)?
        {
            return Ok(status);
        }

        self.session_canonical_status(agent.agent_id.as_str())
    }

    fn session_canonical_status(&self, agent_id: &str) -> Result<ListedAgentStatus, String> {
        let mut status = ListedAgentStatus::PendingInit;
        for fact in self.read_session_facts(agent_id)? {
            match &fact.payload {
                FactPayload::TurnStarted(_) => status = ListedAgentStatus::Running,
                FactPayload::TurnInterrupted(_) => status = ListedAgentStatus::Interrupted,
                FactPayload::InteractionRequested(_) => status = ListedAgentStatus::WaitingUser,
                FactPayload::InteractionResolved(_) | FactPayload::InteractionExpired(_)
                    if status == ListedAgentStatus::WaitingUser =>
                {
                    status = ListedAgentStatus::Running;
                }
                FactPayload::TurnFinished(_) if status == ListedAgentStatus::WaitingUser => {
                    status = ListedAgentStatus::Running;
                }
                FactPayload::SubagentFinished(payload) => {
                    status = listed_terminal_status(payload.status);
                }
                FactPayload::SessionDeleted(_) => status = ListedAgentStatus::Shutdown,
                _ => {}
            }
        }
        Ok(status)
    }

    fn parent_terminal_status(
        &self,
        agent: &AgentMetadata,
    ) -> Result<Option<ListedAgentStatus>, String> {
        let parent_path = agent
            .parent_agent_path
            .as_ref()
            .ok_or_else(|| format!("child agent {} has no parent path", agent.agent_id))?;
        let parent = self
            .agent_catalog
            .get_by_path(agent.root_session_id.as_str(), parent_path)
            .cloned()
            .ok_or_else(|| {
                format!(
                    "child agent {} parent metadata missing at {parent_path}",
                    agent.agent_id
                )
            })?;
        let child_id = SessionId::new(agent.agent_id.as_str());
        let status = self
            .read_session_facts(parent.agent_id.as_str())?
            .into_iter()
            .rev()
            .find_map(|fact| match fact.payload {
                FactPayload::SubagentFinished(payload) if payload.child_session_id == child_id => {
                    Some(listed_terminal_status(payload.status))
                }
                _ => None,
            });
        Ok(status)
    }

    fn read_session_facts(&self, session_id: &str) -> Result<Vec<SessionFact>, String> {
        let session_dir = self.sessions.session_path_dir(session_id);
        if !session_dir.join(EVENTS_COMMIT_FILE).exists() {
            return Ok(Vec::new());
        }
        let identity = CanonicalSessionIdentity::open(&session_dir)
            .map_err(|error| format!("open canonical identity for {session_id}: {error}"))?;
        if identity.session_id.as_str() != session_id {
            return Err(format!(
                "canonical identity for {session_id} is {}",
                identity.session_id
            ));
        }
        let reader = CommittedFactReader::open(
            &session_dir,
            identity.session_id.clone(),
            identity.log_id.clone(),
        )
        .map_err(|error| format!("open canonical facts for {session_id}: {error}"))?;
        reader
            .read_all()
            .map_err(|error| format!("read canonical facts for {session_id}: {error}"))
    }

    /// Resolve a caller-relative or absolute target path within the caller's
    /// root tree.
    pub fn resolve_agent_for_caller(
        &mut self,
        caller_session_id: &str,
        target: &str,
    ) -> Result<AgentMetadata, String> {
        self.ensure_root_metadata(caller_session_id)?;
        let caller = self
            .agent_catalog
            .get_by_id(caller_session_id)
            .cloned()
            .ok_or_else(|| format!("caller agent metadata missing for {caller_session_id}"))?;
        let target_path = caller
            .agent_path
            .resolve(target)
            .map_err(|error| format!("invalid target agent path {target:?}: {error}"))?;
        if target_path.namespace() != caller.agent_path.namespace() {
            return Err(format!(
                "target {target_path} crosses agent namespaces from {}",
                caller.agent_path
            ));
        }
        self.agent_catalog
            .get_by_path(caller.root_session_id.as_str(), &target_path)
            .cloned()
            .ok_or_else(|| format!("target agent not found: {target_path}"))
    }

    /// Rebuild the canonical agent graph for one root tree.
    pub fn agent_graph_snapshot(
        &self,
        root_session_id: &str,
    ) -> Result<AgentGraphSnapshot, String> {
        crate::agent_graph::load_agent_graph(&self.sessions, root_session_id)
            .map(|graph| graph.snapshot())
    }

    fn ensure_root_metadata(&mut self, seed: &str) -> Result<(), String> {
        if seed.is_empty()
            || self.agent_catalog.get_by_id(seed).is_some()
            || self.supervisor.parent_of(seed).is_some()
        {
            return Ok(());
        }
        if self.canonical_parent_session_id(seed)?.is_none() {
            self.register_root_agent(seed, unix_ms())?;
        }
        Ok(())
    }

    /// Read the `SessionCreated.parent_session_id` recovery hint from the
    /// canonical log. This is deliberately a hint only: graph ownership is
    /// established by parent-log `SubagentSpawned` facts in SUBV2-03/04.
    fn canonical_parent_session_id(&self, seed: &str) -> Result<Option<String>, String> {
        let session_dir = self.sessions.session_path_dir(seed);
        if !session_dir.exists() {
            return Ok(None);
        }
        // This is a read-only recovery hint. Do not create the identity
        // sidecar here: a session directory can legitimately exist before the
        // canonical baseline has been materialized.
        if !session_dir.join(CANONICAL_IDENTITY_FILE).exists() {
            return Ok(None);
        }
        let has_events = session_dir.join(EVENTS_FILE).exists();
        let has_commit = session_dir.join(EVENTS_COMMIT_FILE).exists();
        if !has_events && !has_commit {
            // `persist_new_session` creates a metadata-only directory before
            // the first canonical fact. That is still a root session.

            return Ok(None);
        }
        let identity = CanonicalSessionIdentity::open_or_create(&session_dir)
            .map_err(|error| format!("open canonical identity for {seed}: {error}"))?;
        let reader = CommittedFactReader::open(
            &session_dir,
            identity.session_id.clone(),
            identity.log_id.clone(),
        )
        .map_err(|error| format!("open committed facts for {seed}: {error}"))?;
        for fact in reader
            .read_all()
            .map_err(|error| format!("read committed facts for {seed}: {error}"))?
        {
            if let FactPayload::SessionCreated(created) = fact.payload {
                return Ok(created
                    .parent_session_id
                    .map(|parent| parent.as_str().to_string()));
            }
        }
        Ok(None)
    }

    pub fn get_or_spawn(&mut self, seed: &str) -> Result<(), String> {
        self.ensure_root_metadata(seed)?;
        if self.instances.contains_key(seed) {
            return Ok(());
        }
        // B9/R2：收尾必须在 spawn 之前——新 worker 线程一启动就可能发布
        // 新 ask/TurnOpened，force 收尾若晚于 spawn 会误杀活交互。
        if let Some(hub) = self.hub.as_ref() {
            hub.seal_orphan_running_turns(seed);
            hub.seal_orphan_channel_state(seed, true);
            hub.mark_worker_live(seed);
        }
        self.spawn(seed, None)?;
        // Diagnostic: the timeline snapshot is a best-effort async checkpoint
        // and a daemon restart can drop its tail. When it lags the message
        // store (meta.turn_count), the resumed transcript misses turns — the
        // frontend now backfills them from the Ringing conversation store, so
        // this is informational but valuable for restart forensics.
        if let Some(hub) = self.hub.as_ref()
            && let Some(meta) = self.sessions.load_meta(seed)
            && let Some(snapshot) = hub.timeline_snapshot(seed)
        {
            let snapshot_turns = snapshot.turns.len();
            if snapshot_turns != meta.turn_count {
                log::warn!(
                    "[timeline] snapshot turns ({snapshot_turns}) != meta.turn_count ({}) for {seed}; transcript backfills from the conversation store",
                    meta.turn_count
                );
            }
        }
        // 新 worker 诞生意味着旧 worker 已死（daemon 重启或进程退出）。
        // timeline 中该 seed 任何未 seal 的 running turn 都是孤儿（如工具
        // 调用未返回 result 时进程被杀），立即收尾为 Cancelled，否则前端
        // 会永远把它投影为 running 并禁止发送新消息。
        Ok(())
    }

    pub fn spawn_new(&mut self, seed: &str) -> Result<(), String> {
        if self.instances.contains_key(seed) {
            return Err(format!("agent already running for {seed}"));
        }
        self.spawn_with(seed, Some(seed), &[])
    }

    /// Spawn an isolated subagent worker **inside the daemon process**.
    ///
    /// Knife-1 step 1 removes `Command::new(current_exe)` and the child-process
    /// leg of the subagent path.
    ///
    /// The subagent is a normal Ringing V1
    /// [`crate::agent::loop_core::Loop`] running on a daemon thread;
    /// its command/event channels are the same typed envelopes as the pipe wire,
    /// so the daemon publishes events and commands unchanged.
    pub fn spawn_subagent(
        &mut self,
        seed: &str,
        tools: &[String],
        model: Option<&str>,
        base_url: Option<&str>,
        max_tokens: Option<u32>,
    ) -> Result<(), String> {
        let persist = std::env::var("QAQH_SUBAGENT_PERSIST")
            .is_ok_and(|value| matches!(value.as_str(), "1" | "true" | "on"));
        let options = SubagentSpawnOptions {
            tools,
            model,
            base_url,
            max_tokens,
            ephemeral: !persist,
        };
        let parent_seed = qaqh_workspace::runtime::context()
            .map(|ctx| ctx.active_session)
            .unwrap_or_default();
        if parent_seed.is_empty() {
            let child_dir = self.sessions.session_path_dir(seed);
            let child_identity = CanonicalSessionIdentity::open_or_create(&child_dir)
                .map_err(|error| format!("open child canonical identity for {seed}: {error}"))?;
            return self.spawn_subagent_internal(
                seed,
                &parent_seed,
                None,
                child_identity.session_id,
                options,
            );
        }
        let requested_name = legacy_child_name(seed);
        self.spawn_subagent_v2(seed, &parent_seed, &requested_name, options)
            .map(|_| ())
    }

    /// V2 spawn path: allocate a tree-relative child path, create the actor,
    /// and register logical metadata before the caller commits the canonical
    /// `SubagentSpawned` edge.
    pub(crate) fn spawn_subagent_v2(
        &mut self,
        seed: &str,
        parent_session_id: &str,
        requested_name: &str,
        options: SubagentSpawnOptions<'_>,
    ) -> Result<SpawnedSubagentInfo, String> {
        if parent_session_id.is_empty() {
            return Err("subagent v2 requires a parent_session_id".to_string());
        }
        self.ensure_root_metadata(parent_session_id)?;
        let parent = self
            .agent_catalog
            .get_by_id(parent_session_id)
            .cloned()
            .ok_or_else(|| format!("subagent parent metadata missing for {parent_session_id}"))?;
        let child_path = parent
            .agent_path
            .child(requested_name)
            .map_err(|error| format!("invalid subagent name {requested_name:?}: {error}"))?;
        if self
            .agent_catalog
            .get_by_path(parent.root_session_id.as_str(), &child_path)
            .is_some()
        {
            return Err(format!(
                "subagent path {child_path} already exists in root {}",
                parent.root_session_id
            ));
        }

        let parent_dir = self.sessions.session_path_dir(parent_session_id);
        let parent_identity =
            CanonicalSessionIdentity::open_or_create(&parent_dir).map_err(|error| {
                format!("open parent canonical identity for {parent_session_id}: {error}")
            })?;
        let child_dir = self.sessions.session_path_dir(seed);
        let child_identity = CanonicalSessionIdentity::open_or_create(&child_dir)
            .map_err(|error| format!("open child canonical identity for {seed}: {error}"))?;
        let cwd = self
            .sessions
            .workspace_cwd(seed)
            .or_else(|| self.sessions.workspace_cwd(parent_session_id))
            .or_else(|| {
                std::env::current_dir()
                    .ok()
                    .map(|path| path.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| "/".to_string());
        crate::service::materialize_canonical_session_in(
            &child_dir,
            &cwd,
            options.model.unwrap_or("unknown"),
            Some(parent_identity.session_id.clone()),
        )?;

        let identity = Some((parent.clone(), child_path.clone()));
        self.spawn_subagent_internal(
            seed,
            parent_session_id,
            identity,
            child_identity.session_id.clone(),
            options,
        )?;
        Ok(SpawnedSubagentInfo {
            child_session_id: child_identity.session_id,
            parent_agent_path: parent.agent_path,
            child_agent_path: child_path,
        })
    }

    /// Roll back a child registration after its canonical spawn edge failed.
    ///
    /// The actor may already exist; closing it is safe, while the durable child
    /// session remains available for recovery because V2 children are
    /// persistent.
    pub fn rollback_subagent(&mut self, seed: &str, child_session_id: &str) {
        self.agent_catalog.remove(child_session_id);
        self.residency.remove(child_session_id);
        self.close(seed);
    }

    fn spawn_subagent_internal(
        &mut self,
        seed: &str,
        parent_seed: &str,
        identity: Option<(AgentMetadata, AgentPath)>,
        child_session_id: SessionId,
        options: SubagentSpawnOptions<'_>,
    ) -> Result<(), String> {
        let SubagentSpawnOptions {
            tools,
            model,
            base_url,
            max_tokens,
            ephemeral,
        } = options;
        if self.instances.contains_key(seed) {
            return Err(format!("agent already running for {seed}"));
        }
        if !ephemeral {
            self.sessions.set_ephemeral(seed, false);
        }
        let root_seed = if parent_seed.is_empty() {
            seed.to_string()
        } else {
            self.supervisor.root_of(parent_seed)
        };
        // Durable reservation must exist before the child actor can perform
        // any side effect.
        let reservation = self.reserve_spawn(&root_seed, seed)?;
        let parent_cancel = self.cancel_for_seed(parent_seed);
        if let Err(error) = self.spawn_subagent_inprocess(
            seed,
            SubagentSpawnSpec {
                tools: tools.to_vec(),
                model: model.map(str::to_string),
                base_url: base_url.map(str::to_string),
                max_tokens,
                ephemeral,
            },
            parent_cancel,
        ) {
            let _ = self.release_spawn(
                &root_seed,
                &reservation.reservation_id,
                ReleaseReason::Cancelled,
            );
            return Err(error);
        }
        if !parent_seed.is_empty()
            && parent_seed != seed
            && let Err(error) = self.link_subagent(parent_seed, seed)
        {
            self.close(seed);
            let _ = self.release_spawn(
                &root_seed,
                &reservation.reservation_id,
                ReleaseReason::Cancelled,
            );
            return Err(error);
        }
        if let Some((parent, child_path)) = identity {
            let metadata = AgentMetadata {
                root_session_id: parent.root_session_id.clone(),
                agent_id: child_session_id.clone(),
                agent_path: child_path,
                parent_agent_path: Some(parent.agent_path),
                nickname: None,
                role: None,
                created_at_ms: unix_ms(),
            };
            if let Err(error) = self.agent_catalog.register_with_alias(metadata, Some(seed)) {
                self.close(seed);
                let _ = self.release_spawn(
                    &root_seed,
                    &reservation.reservation_id,
                    ReleaseReason::Reconciliation,
                );
                return Err(error.to_string());
            }
        }
        if let Err(error) = self.commit_spawn(&root_seed, &reservation.reservation_id) {
            self.agent_catalog.remove(child_session_id.as_str());
            self.close(seed);
            let _ = self.release_spawn(
                &root_seed,
                &reservation.reservation_id,
                ReleaseReason::Reconciliation,
            );
            return Err(error);
        }
        Ok(())
    }

    fn spawn_subagent_inprocess(
        &mut self,
        seed: &str,
        spec: SubagentSpawnSpec,
        parent_cancel: Option<crate::agent::types::CancelToken>,
    ) -> Result<(), String> {
        if self.instances.contains_key(seed) {
            return Err(format!("agent already running for {seed}"));
        }
        self.last_spawn
            .insert(seed.to_string(), std::time::Instant::now());
        let (generation, _) = self.activity.begin(seed);

        let channels = crate::agent::loop_core::LoopChannels::new();
        let crate::agent::loop_core::LoopChannels {
            cmd_tx,
            cmd_rx,
            event_tx,
            event_rx,
            cancel,
            writer_dead,
        } = channels;
        let cancel = parent_cancel.map_or(cancel, |parent| parent.child());
        let cancel_for_sender = cancel.clone();

        let event_seed = seed.to_string();
        let activity = self.activity.clone();
        let hub = self.hub.clone();
        let reader = std::thread::spawn(move || {
            crate::actor::run_inprocess_event_reader(
                event_rx, event_seed, generation, activity, hub,
            );
        });

        let actor_seed = seed.to_string();
        let actor_spec = spec.clone();
        let tools_len = spec.tools.len();
        let liveness = std::sync::Arc::new(crate::agent::liveness::WorkerLiveness::new());
        let liveness_for_registry = std::sync::Arc::clone(&liveness);
        // T-1-5：worker 退出即摘除活表（与 T-1-1 的 spawn 登记成对）。
        // `live_workers` 此前只靠 `forget_seed`（会话关闭）清理，子代理 actor
        // 自然退出后条目永留——bootstrap 的孤儿收尾因此永远跳过该 seed。
        let hub_for_worker = self.hub.clone();
        let dead_seed = seed.to_string();
        let thread = std::thread::Builder::new()
            .name(format!("qaqh-subagent-{actor_seed}"))
            .spawn(move || {
                crate::actor::run_subagent_actor(
                    actor_seed,
                    actor_spec,
                    cmd_rx,
                    event_tx,
                    cancel,
                    writer_dead,
                    liveness,
                );
                if let Some(hub) = hub_for_worker.as_ref() {
                    hub.mark_worker_dead(&dead_seed);
                }
            })
            .map_err(|e| format!("spawn in-process subagent {seed}: {e}"))?;

        self.instances.insert(
            seed.to_string(),
            AgentInstance {
                seed: seed.to_string(),
                transport: AgentTransport::InProcess {
                    cmd_tx,
                    cancel: cancel_for_sender,
                },
                kind: AgentKind::Subagent(spec),
                subscription_actor: SessionActor::new(16),
                liveness: Some(liveness_for_registry),
                reader: Some(reader),
                thread: Some(thread),
            },
        );
        self.residency
            .insert(seed.to_string(), ListedAgentResidency::Loaded);
        // T-1-1：子 seed 必须进活表。否则 bootstrap 的
        // `seal_orphan_channel_state(seed, force=false)` 会把它判为孤儿并封禁
        // 其正在进行的 turn（前端据此显示 cancelled），而子 actor 仍在运行并
        // 继续发布事件——即「已判定 cancel 的子代理复活」。
        if let Some(hub) = self.hub.as_ref() {
            hub.mark_worker_live(seed);
        }
        log::info!(
            "[subagent] spawned in-process actor seed={seed} tools={tools_len} (no child process)"
        );
        Ok(())
    }

    fn spawn(&mut self, seed: &str, new_seed: Option<&str>) -> Result<(), String> {
        self.spawn_with(seed, new_seed, &[])
    }

    fn spawn_with(
        &mut self,
        seed: &str,
        new_seed: Option<&str>,
        extra_args: &[String],
    ) -> Result<(), String> {
        self.spawn_session_inprocess(seed, new_seed, extra_args)
    }

    fn spawn_session_inprocess(
        &mut self,
        seed: &str,
        new_seed: Option<&str>,
        _extra_args: &[String],
    ) -> Result<(), String> {
        if self.instances.contains_key(seed) {
            return Err(format!("agent already running for {seed}"));
        }
        self.last_spawn
            .insert(seed.to_string(), std::time::Instant::now());
        let (generation, _) = self.activity.begin(seed);

        let channels = crate::agent::loop_core::LoopChannels::new();
        let crate::agent::loop_core::LoopChannels {
            cmd_tx,
            cmd_rx,
            event_tx,
            event_rx,
            cancel,
            writer_dead,
        } = channels;
        let cancel_for_sender = cancel.clone();

        let event_seed = seed.to_string();
        let activity = self.activity.clone();
        let hub = self.hub.clone();
        let reader = std::thread::spawn(move || {
            crate::actor::run_inprocess_event_reader(
                event_rx, event_seed, generation, activity, hub,
            );
        });

        // Resume worker: timeline is the authoritative turn ledger. The meta
        // turn_count can lag the timeline after a daemon restart, so the actor
        // must start its turn allocator above any timeline turn already sealed.
        let timeline_turn_count = if new_seed.is_none() {
            if let Some(hub) = self.hub.as_ref()
                && let Some(snapshot) = hub.timeline_snapshot(seed)
            {
                snapshot
                    .turns
                    .iter()
                    .filter_map(|turn| turn.turn_id.strip_prefix('t'))
                    .filter_map(|seq| seq.parse::<u64>().ok())
                    .max()
                    .unwrap_or(0)
            } else {
                0
            }
        } else {
            0
        };

        let actor_seed = seed.to_string();
        let resume_seed = if new_seed.is_none() {
            Some(seed.to_string())
        } else {
            None
        };
        let new_seed_owned = new_seed.map(str::to_string);
        let liveness = std::sync::Arc::new(crate::agent::liveness::WorkerLiveness::new());
        let liveness_for_registry = std::sync::Arc::clone(&liveness);
        let thread = std::thread::Builder::new()
            .name(format!("qaqh-session-{actor_seed}"))
            .spawn(move || {
                crate::actor::run_session_actor(
                    actor_seed,
                    resume_seed,
                    new_seed_owned,
                    timeline_turn_count,
                    cmd_rx,
                    event_tx,
                    cancel,
                    writer_dead,
                    liveness,
                );
            })
            .map_err(|e| format!("spawn in-process session {seed}: {e}"))?;

        self.instances.insert(
            seed.to_string(),
            AgentInstance {
                seed: seed.to_string(),
                transport: AgentTransport::InProcess {
                    cmd_tx,
                    cancel: cancel_for_sender,
                },
                kind: AgentKind::Session,
                subscription_actor: SessionActor::new(16),
                liveness: Some(liveness_for_registry),
                reader: Some(reader),
                thread: Some(thread),
            },
        );
        self.residency
            .insert(seed.to_string(), ListedAgentResidency::Loaded);
        log::info!("[session] spawned in-process actor seed={seed} (no child process)");
        Ok(())
    }

    /// Materialize the delivery target without turning a logical child into a
    /// root session. Roots use the ordinary get-or-spawn path; children reload
    /// only through a loaded immediate parent and reuse the durable spawn
    /// config recorded in the parent's canonical `SubagentSpawned` fact.
    fn ensure_loaded_for_command(&mut self, seed: &str) -> Result<(), String> {
        if self.instances.contains_key(seed) {
            return Ok(());
        }
        let Some(metadata) = self.agent_catalog.get_by_id(seed).cloned() else {
            return self.get_or_spawn(seed);
        };
        if metadata.agent_path.is_root() {
            return self.get_or_spawn(seed);
        }
        let parent_path = metadata
            .parent_agent_path
            .as_ref()
            .ok_or_else(|| format!("child agent {seed} has no parent path"))?;
        let parent = self
            .agent_catalog
            .get_by_path(metadata.root_session_id.as_str(), parent_path)
            .cloned()
            .ok_or_else(|| {
                format!(
                    "child agent {seed} parent metadata missing at {parent_path} in root {}",
                    metadata.root_session_id
                )
            })?;
        if !self.instances.contains_key(parent.agent_id.as_str()) {
            return Err(format!(
                "child agent {seed} cannot reload: immediate parent {} is unloaded",
                parent.agent_path
            ));
        }
        let (config, _) = self.subagent_spawn_config(parent.agent_id.as_str(), seed)?;
        self.reload_subagent_internal(seed, parent.agent_id.as_str(), config)
    }

    pub(crate) fn subagent_spawn_config(
        &self,
        parent_id: &str,
        child_id: &str,
    ) -> Result<(SubagentSpawnConfig, String), String> {
        let parent_dir = self.sessions.session_path_dir(parent_id);
        let identity = CanonicalSessionIdentity::open(&parent_dir)
            .map_err(|error| format!("open parent canonical identity for {parent_id}: {error}"))?;
        let reader = CommittedFactReader::open(
            &parent_dir,
            identity.session_id.clone(),
            identity.log_id.clone(),
        )
        .map_err(|error| format!("open parent canonical facts for {parent_id}: {error}"))?;
        let child_id = SessionId::new(child_id);
        reader
            .read_all()
            .map_err(|error| format!("read parent canonical facts for {parent_id}: {error}"))?
            .into_iter()
            .rev()
            .find_map(|fact| match fact.payload {
                FactPayload::SubagentSpawned(payload) if payload.child_session_id == child_id => {
                    payload
                        .spawn_config
                        .map(|config| (config, payload.parent_call_id.as_str().to_string()))
                }
                _ => None,
            })
            .ok_or_else(|| {
                format!(
                    "child agent {child_id} has no canonical spawn config in parent {parent_id}"
                )
            })
    }

    pub(crate) fn prepare_collector_arm(
        &mut self,
        child_id: &str,
    ) -> Result<Option<CollectorArmSpec>, String> {
        if self.armed_collectors.contains(child_id) {
            return Ok(None);
        }
        let child = self
            .agent_catalog
            .get_by_id(child_id)
            .cloned()
            .ok_or_else(|| format!("child agent metadata missing for {child_id}"))?;
        if child.agent_path.is_root() {
            return Ok(None);
        }
        let parent_path = child
            .parent_agent_path
            .as_ref()
            .ok_or_else(|| format!("child agent {child_id} has no parent path"))?;
        let parent = self
            .agent_catalog
            .get_by_path(child.root_session_id.as_str(), parent_path)
            .cloned()
            .ok_or_else(|| format!("child agent {child_id} parent metadata missing"))?;
        if !self.instances.contains_key(parent.agent_id.as_str()) {
            return Err(format!(
                "child agent {child_id} collector cannot arm: parent {} is unloaded",
                parent.agent_path
            ));
        }
        let (config, parent_call_id) =
            self.subagent_spawn_config(parent.agent_id.as_str(), child_id)?;
        self.armed_collectors.insert(child_id.to_string());
        Ok(Some(CollectorArmSpec {
            child_session_id: child.agent_id.as_str().to_string(),
            name: child.role.clone().unwrap_or_else(|| {
                child
                    .agent_path
                    .as_str()
                    .rsplit('/')
                    .next()
                    .unwrap_or("subagent")
                    .to_string()
            }),
            parent_session_id: parent.agent_id.as_str().to_string(),
            parent_call_id,
            timeout_secs: config.timeout_secs,
            root_session_id: child.root_session_id.as_str().to_string(),
            parent_agent_path: parent.agent_path.as_str().to_string(),
            child_agent_path: child.agent_path.as_str().to_string(),
        }))
    }

    pub(crate) fn mark_collector_armed(&mut self, child_id: &str) {
        self.armed_collectors.insert(child_id.to_string());
    }

    pub(crate) fn unmark_collector_armed(&mut self, child_id: &str) {
        self.armed_collectors.remove(child_id);
    }

    fn reload_subagent_internal(
        &mut self,
        seed: &str,
        parent_id: &str,
        config: SubagentSpawnConfig,
    ) -> Result<(), String> {
        if self.instances.contains_key(seed) {
            return Ok(());
        }
        if !config.ephemeral {
            self.sessions.set_ephemeral(seed, false);
        }
        let parent_cancel = self.cancel_for_seed(parent_id);
        self.spawn_subagent_inprocess(
            seed,
            SubagentSpawnSpec {
                tools: config.tools,
                model: config.model,
                base_url: config.base_url,
                max_tokens: config.max_tokens,
                ephemeral: config.ephemeral,
            },
            parent_cancel,
        )?;
        if let Err(error) = self.link_subagent(parent_id, seed) {
            self.close(seed);
            return Err(format!(
                "reload child {seed} failed to restore parent edge: {error}"
            ));
        }
        log::info!("[registry] reloaded child agent {seed} through loaded parent {parent_id}");
        Ok(())
    }

    /// 发送 Ringing worker 命令帧（携带 `wire` 判别字段；worker reader 按 wire 解析）。
    pub fn send_ringing(
        &mut self,
        seed: &str,
        env: &qaqh_ringing::RingingWorkerCommandEnvelope,
    ) -> Result<(), String> {
        if let Some(metadata) = self.agent_catalog.get_by_id(seed)
            && !metadata.agent_path.is_root()
            && matches!(
                &env.command,
                qaqh_ringing::RingingCommand::Conversation(
                    qaqh_domain::ConversationCommand::ConversationSendMessage {
                        inter_agent: None,
                        ..
                    }
                )
            )
        {
            return Err(format!(
                "parent-owned agent {} rejects direct ConversationSendMessage without inter-agent metadata",
                metadata.agent_path
            ));
        }
        self.ensure_loaded_for_command(seed)?;
        let write = |instance: &AgentInstance| -> Result<(), String> {
            match &instance.transport {
                AgentTransport::InProcess { cmd_tx, cancel } => {
                    // Mirror the pipe reader: interrupt frames set the cancel
                    // token before they enter the command queue so long-running
                    // gate/tool work observes the abort immediately. PR-3-4：
                    // 会话级取消写会话键控表（该会话在途工具即时中止），不再
                    // 置进程级 flag——那会误伤其它会话的在途工具。
                    if crate::agent::loop_core::ringing_command_is_interrupt(env) {
                        cancel.set();
                        qaqh_workspace::set_session_cancel(seed, true);
                    }
                    let cmd = crate::agent::types::WorkerCommand {
                        frame: env.clone(),
                        causation: Some(env.command_id.clone()),
                    };
                    cmd_tx
                        .send(cmd)
                        .map_err(|e| format!("agent command channel send: {e}"))
                }
            }
        };
        if write(self.instances.get(seed).expect("spawned instance")).is_ok() {
            // T-1-4：父会话取消传播到它派生的子 seed。放在投递成功之后——
            // 取消帧确实进入了父 worker 才谈得上「取消已经发生」。registry
            // 是 Ringing 命令的唯一咽喉（daemon RPC、宿主直连、广播都经此），
            // 因此这里也是唯一能同时触达 hub（mark_worker_dead）与子实例
            // cmd_tx 的位置。
            if matches!(
                &env.command,
                qaqh_ringing::RingingCommand::Conversation(
                    qaqh_domain::ConversationCommand::ConversationCancel { .. }
                )
            ) {
                self.cancel_subagent_children(seed);
            }
            return Ok(());
        }
        let kind = self
            .instances
            .get(seed)
            .map(AgentInstance::kind_name)
            .unwrap_or(AgentKind::Session);
        let parent = self.supervisor.parent_of(seed);
        let parent_cancel = parent
            .as_ref()
            .and_then(|parent| self.cancel_for_seed(parent));
        self.close(seed);
        match kind {
            AgentKind::Session => self.get_or_spawn(seed)?,
            AgentKind::Subagent(spec) => {
                self.spawn_subagent_inprocess(seed, spec, parent_cancel)?;
                if let Some(parent) = parent {
                    self.link_subagent(&parent, seed)?;
                }
            }
        }
        write(self.instances.get(seed).expect("respawned instance"))
    }

    /// 向所有活跃 worker（含子代理）广播同一条 Ringing 命令。
    /// 只发给已运行的实例，不触发 spawn。返回失败项列表（seed: error）。
    pub fn broadcast_ringing(&mut self, command: &qaqh_ringing::RingingCommand) -> Vec<String> {
        let seeds: Vec<String> = self.instances.keys().cloned().collect();
        let mut failed = Vec::new();
        for seed in seeds {
            let env = qaqh_ringing::RingingWorkerCommandEnvelope::new(
                &seed,
                broadcast_command_id(),
                command.clone(),
            );
            if let Err(error) = self.send_ringing(&seed, &env) {
                failed.push(format!("{seed}: {error}"));
            }
        }
        failed
    }

    pub fn subscribe_channel(
        &mut self,
        seed: &str,
        connection_id: ConnectionId,
        channel: RingingChannel,
    ) -> Result<bool, String> {
        let instance = self
            .instances
            .get_mut(seed)
            .ok_or_else(|| format!("session {seed} is not running"))?;
        match instance.apply_subscription(SubscriptionCommand::Subscribe {
            connection_id,
            channel,
        })? {
            SubscriptionEffect::Subscribed { changed, .. } => Ok(changed),
            effect => Err(format!("unexpected subscribe effect: {effect:?}")),
        }
    }

    pub fn unsubscribe_channel(
        &mut self,
        seed: &str,
        connection_id: ConnectionId,
        channel: RingingChannel,
    ) -> Result<bool, String> {
        let instance = self
            .instances
            .get_mut(seed)
            .ok_or_else(|| format!("session {seed} is not running"))?;
        match instance.apply_subscription(SubscriptionCommand::Unsubscribe {
            connection_id,
            channel,
        })? {
            SubscriptionEffect::Unsubscribed { changed, .. } => Ok(changed),
            effect => Err(format!("unexpected unsubscribe effect: {effect:?}")),
        }
    }

    pub fn connection_closed(
        &mut self,
        seed: &str,
        connection_id: ConnectionId,
    ) -> Result<usize, String> {
        let Some(instance) = self.instances.get_mut(seed) else {
            return Ok(0);
        };
        match instance
            .apply_subscription(SubscriptionCommand::ConnectionClosed { connection_id })?
        {
            SubscriptionEffect::ConnectionClosed { removed, .. } => Ok(removed),
            effect => Err(format!("unexpected connection-close effect: {effect:?}")),
        }
    }

    pub fn close(&mut self, seed: &str) {
        let descendants = self.supervisor.begin_unload(seed);
        for child in descendants {
            let parent = self.supervisor.parent_of(&child);
            self.finish_for_unload(&child, parent.as_deref());
            self.supervisor.unlink(&child);
            self.armed_collectors.remove(&child);
        }

        let parent = self.supervisor.parent_of(seed);
        self.finish_for_unload(seed, parent.as_deref());
        self.supervisor.unlink(seed);
        self.supervisor.parent_unload_ack(seed);
        self.armed_collectors.remove(seed);
    }

    /// Signal, observe terminal, then join one worker. For a child, the parent
    /// edge is closed before the join, which is the P2-5 ordering contract.
    fn finish_for_unload(&mut self, seed: &str, parent: Option<&str>) {
        self.residency
            .insert(seed.to_string(), ListedAgentResidency::Unloaded);
        if let Some(parent) = parent {
            self.supervisor.cancel_sent(parent, seed);
        }
        let Some(mut instance) = self.instances.remove(seed) else {
            if let Some(parent) = parent {
                self.supervisor.child_terminal(parent, seed);
                self.supervisor.parent_subagent_finished(parent, seed);
                self.supervisor.child_joined(parent, seed);
            }
            qaqh_workspace::remove_session_cancel(seed);
            return;
        };

        instance.signal_shutdown();
        if !instance.wait_until_stopped(seed) {
            log::warn!("[registry] worker {seed} did not stop before join timeout");
        }
        if let Some(parent) = parent {
            self.supervisor.child_terminal(parent, seed);
            self.supervisor.parent_subagent_finished(parent, seed);
        }
        instance.finish_shutdown();
        if let Some(parent) = parent {
            self.supervisor.child_joined(parent, seed);
        }
        qaqh_workspace::remove_session_cancel(seed);
    }

    /// T-1-4：登记父会话 → 子代理的派生关系（幂等）。
    fn link_subagent(&mut self, parent: &str, child: &str) -> Result<(), String> {
        self.supervisor.link(parent, child)
    }

    /// T-1-4：父会话当前登记的子代理 seed（排序后返回，便于日志与测试）。
    fn children_of(&self, parent: &str) -> Vec<String> {
        self.supervisor.children_of(parent)
    }

    fn cancel_for_seed(&self, seed: &str) -> Option<crate::agent::types::CancelToken> {
        if seed.is_empty() {
            return None;
        }
        self.instances.get(seed).map(|instance| {
            let AgentTransport::InProcess { cancel, .. } = &instance.transport;
            cancel.clone()
        })
    }

    /// T-1-4 测试/运维只读视图：父会话登记的子代理 seed。
    #[doc(hidden)]
    pub fn subagent_children(&self, parent: &str) -> Vec<String> {
        self.children_of(parent)
    }

    /// Set root-tree quota limits. Existing in-memory ledgers are dropped;
    /// durable files remain and are replayed on next access.
    pub fn set_quota_limits(&mut self, limits: QuotaLimits) {
        self.quota_limits = limits;
        self.quota_ledgers.clear();
    }

    /// P2-7 test/ops view for one root ledger.
    #[doc(hidden)]
    pub fn quota_snapshot(
        &mut self,
        root: &str,
    ) -> Result<crate::quota_ledger::QuotaSnapshot, String> {
        self.quota_ledger_mut(root)?.snapshot()
    }

    fn quota_ledger_mut(&mut self, root: &str) -> Result<&mut QuotaLedger, String> {
        if !self.quota_ledgers.contains_key(root) {
            let ledger = QuotaLedger::open(root, self.quota_limits)?;
            self.quota_ledgers.insert(root.to_string(), ledger);
        }
        self.quota_ledgers
            .get_mut(root)
            .ok_or_else(|| format!("quota ledger for {root} missing after open"))
    }

    fn reserve_spawn(&mut self, root: &str, child: &str) -> Result<QuotaReservation, String> {
        self.quota_ledger_mut(root)?.reserve(
            QuotaKind::Spawn,
            1,
            format!("subagent-spawn:{child}"),
            Some(child.to_string()),
        )
    }

    fn commit_spawn(&mut self, root: &str, reservation_id: &str) -> Result<(), String> {
        self.quota_ledger_mut(root)?.commit(reservation_id)
    }

    fn release_spawn(
        &mut self,
        root: &str,
        reservation_id: &str,
        reason: ReleaseReason,
    ) -> Result<(), String> {
        self.quota_ledger_mut(root)?.release(reservation_id, reason)
    }

    /// P2-5 测试/运维只读视图：parent/child 生命周期事件顺序。
    #[doc(hidden)]
    pub fn subagent_lifecycle_trace(&self) -> Vec<String> {
        self.supervisor
            .trace()
            .iter()
            .map(|event| match event {
                LifecycleEvent::EdgeLinked { parent, child } => {
                    format!("edge_linked:{parent}:{child}")
                }
                LifecycleEvent::EdgeUnlinked { parent, child } => {
                    format!("edge_unlinked:{parent}:{child}")
                }
                LifecycleEvent::ParentUnloadRequested { parent } => {
                    format!("parent_unload_requested:{parent}")
                }
                LifecycleEvent::ChildCancelSent { parent, child } => {
                    format!("child_cancel_sent:{parent}:{child}")
                }
                LifecycleEvent::ChildTerminal { parent, child } => {
                    format!("child_terminal:{parent}:{child}")
                }
                LifecycleEvent::ParentSubagentFinished { parent, child } => {
                    format!("parent_subagent_finished:{parent}:{child}")
                }
                LifecycleEvent::ChildJoined { parent, child } => {
                    format!("child_joined:{parent}:{child}")
                }
                LifecycleEvent::ParentUnloadAck { parent } => {
                    format!("parent_unload_ack:{parent}")
                }
            })
            .collect()
    }

    /// T-1-4：把父会话的取消传播到它派生的全部子 seed（递归覆盖孙代）。
    ///
    /// 子代理的取消入口与父会话一致：会话键控取消标记（子 seed 在途工具在
    /// 轮询点立即中止）+ `ConversationCancel` 命令（子 Loop 收尾当前回合并
    /// 发布 `ConversationCancelled`）。子代理同时从 `live_workers` 摘除：父
    /// 取消后它不再是活 worker，bootstrap 的孤儿收尾必须能收掉它遗留的
    /// running 状态，否则前端会把它投影为仍在运行。
    ///
    /// 只对**已在册**的子实例投递命令——绝不 `get_or_spawn`：已取消/已退出
    /// 的子代理不得因为一次取消传播而被重新拉起。
    fn cancel_subagent_children(&mut self, parent: &str) {
        let children = self.children_of(parent);
        for child in children {
            self.supervisor.cancel_sent(parent, &child);
            qaqh_workspace::set_session_cancel(&child, true);
            if let Some(hub) = self.hub.as_ref() {
                hub.mark_worker_dead(&child);
            }
            let command_id = format!(
                "parent-cancel-{:x}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            );
            let env = qaqh_ringing::RingingWorkerCommandEnvelope::new(
                child.clone(),
                command_id.clone(),
                qaqh_ringing::RingingCommand::Conversation(
                    qaqh_domain::ConversationCommand::ConversationCancel { turn_id: None },
                ),
            );
            let delivered = match self.instances.get(&child) {
                Some(AgentInstance {
                    transport: AgentTransport::InProcess { cmd_tx, cancel: _ },
                    ..
                }) => {
                    // Token tree propagation already happened when the parent
                    // token was set in `send_ringing`. This command is still
                    // required to drain the child actor and emit its terminal.
                    cmd_tx
                        .send(crate::agent::types::WorkerCommand {
                            frame: env,
                            causation: Some(command_id),
                        })
                        .is_ok()
                }
                None => false,
            };
            log::info!(
                "[subagent] parent cancel {parent} propagated to child {child} (delivered={delivered})"
            );
            // 子 actor 自身分派取消时不经过 registry，孙代在此逐层传播。
            self.cancel_subagent_children(&child);
        }
    }

    /// Test/ops hook: the idle-unload liveness handle of a worker, if it has
    /// one (in-process actors only).
    #[doc(hidden)]
    pub fn worker_liveness(
        &self,
        seed: &str,
    ) -> Option<std::sync::Arc<crate::agent::liveness::WorkerLiveness>> {
        self.instances
            .get(seed)
            .and_then(|instance| instance.liveness.clone())
    }

    /// E: idle-unload workers whose last dispatch is older than `idle_secs`.
    /// Only in-process Session actors participate; subagents are short-lived
    /// and legacy process workers have no liveness handle. The close path is
    /// the regular graceful shutdown (signal → join → drop bundle → final
    /// flush/drain), so the next input resumes from disk via
    /// `load_for_resume`. Returns the seeds that were unloaded.
    pub fn unload_idle_sessions(&mut self, idle_secs: u64) -> Vec<String> {
        if self.shutting_down || idle_secs == 0 {
            return Vec::new();
        }
        let unloadable: Vec<String> = self
            .instances
            .iter()
            .filter_map(|(seed, instance)| {
                let liveness = instance.liveness.as_ref()?;
                (liveness.unloadable() && liveness.idle_secs() >= idle_secs).then(|| seed.clone())
            })
            .collect();
        let mut unloaded = Vec::new();
        for seed in unloadable {
            let idle = self
                .instances
                .get(&seed)
                .and_then(|instance| instance.liveness.as_ref())
                .map(|liveness| liveness.idle_secs())
                .unwrap_or(0);
            log::info!("[registry] idle unload seed={seed} idle={idle}s");
            self.close(&seed);
            unloaded.push(seed);
        }
        unloaded
    }

    pub fn shutdown_all(&mut self) {
        self.shutting_down = true;
        // Close roots through the supervisor so every child tree follows
        // terminal -> edge finish -> join -> parent ack.
        let mut roots: Vec<String> = self
            .instances
            .keys()
            .filter(|seed| {
                self.supervisor
                    .parent_of(seed)
                    .is_none_or(|parent| !self.instances.contains_key(&parent))
            })
            .cloned()
            .collect();
        roots.sort();
        for seed in roots {
            self.close(&seed);
        }
        // Defensive fallback for an instance whose edge state was already
        // removed before shutdown.
        let leftovers: Vec<AgentInstance> = self
            .instances
            .drain()
            .map(|(_, instance)| instance)
            .collect();
        for instance in leftovers {
            instance.shutdown();
        }
    }

    /// F4: 拉起所有已退出且非优雅关闭的 worker。由 daemon 侧周期任务调用；
    /// 带 1 秒退避防止崩溃-重启风暴。优雅关闭（收到 Shutdown 帧后退出、
    /// 或被 `close`/`shutdown_all` 主动结束）的实例不会重启。
    pub fn respawn_dead_agents(&mut self) {
        if self.shutting_down {
            return;
        }
        let dead: Vec<(String, AgentKind, Option<crate::agent::types::CancelToken>)> = self
            .instances
            .iter()
            .filter(|(_, instance)| instance.is_dead())
            .filter(|(seed, _)| {
                self.supervisor.parent_of(seed).is_none_or(|parent| {
                    !self
                        .instances
                        .get(&parent)
                        .is_some_and(AgentInstance::is_dead)
                })
            })
            .map(|(seed, instance)| {
                let parent_cancel = self
                    .supervisor
                    .parent_of(seed)
                    .as_ref()
                    .and_then(|parent| self.cancel_for_seed(parent));
                (seed.clone(), instance.kind_name(), parent_cancel)
            })
            .collect();
        for (seed, kind, parent_cancel) in dead {
            // 退避：同一 seed 最近 1 秒内刚 spawn 过（例如刚拉起又立刻崩溃）
            // 则跳过本轮，避免无意义的重启风暴。
            if self
                .last_spawn
                .get(&seed)
                .is_some_and(|at| at.elapsed() < std::time::Duration::from_secs(1))
            {
                log::warn!("[AGENT:{seed}] worker exited immediately after spawn; backing off");
                continue;
            }
            if !self.children_of(&seed).is_empty() {
                log::warn!(
                    "[AGENT:{seed}] dead parent detected; closing child tree before respawn"
                );
            }
            self.close(&seed);
            log::warn!("[AGENT:{seed}] in-process worker died; respawning");
            // B9/R2：先 seal 后 spawn——新 worker 线程一启动就可能发布
            // 新 ask/TurnOpened，晚于 spawn 的 force 收尾会误杀活交互。
            if let Some(hub) = self.hub.as_ref() {
                hub.seal_orphan_running_turns(&seed);
                // force=true：旧 worker 已死亡，挂起交互必为孤儿。
                hub.seal_orphan_channel_state(&seed, true);
                hub.mark_worker_live(&seed);
            }
            let spawned = match kind {
                AgentKind::Session => self.spawn(&seed, None),
                AgentKind::Subagent(spec) => {
                    self.spawn_subagent_inprocess(&seed, spec, parent_cancel)
                }
            };
            if let Err(error) = spawned {
                log::error!("[AGENT:{seed}] respawn failed: {error}");
            }
        }
    }

    pub fn activities(&self) -> Vec<qaqh_domain::SessionActivity> {
        self.activity.snapshot()
    }

    pub fn activity(&self, seed: &str) -> Option<qaqh_domain::SessionActivity> {
        self.activity.get(seed)
    }

    pub fn is_running(&self, seed: &str) -> bool {
        self.instances.contains_key(seed)
    }

    /// Test/ops hook: whether the worker's loop thread has exited without
    /// having been reaped by `close` or `respawn_dead_agents`.
    #[doc(hidden)]
    pub fn worker_finished(&self, seed: &str) -> bool {
        self.instances.get(seed).is_some_and(AgentInstance::is_dead)
    }

    /// 向所有存活 agent 广播同一 Ringing 命令。
    pub fn send_ringing_all(&mut self, command: qaqh_ringing::RingingCommand) {
        let seeds: Vec<_> = self.instances.keys().cloned().collect();
        for seed in seeds {
            let env = qaqh_ringing::RingingWorkerCommandEnvelope::new(
                seed.clone(),
                "daemon-broadcast",
                command.clone(),
            );
            let _ = self.send_ringing(&seed, &env);
        }
    }
}

impl AgentInstance {
    fn apply_subscription(
        &mut self,
        command: SubscriptionCommand,
    ) -> Result<SubscriptionEffect, String> {
        self.subscription_actor
            .submit(SessionCommand::Subscription(command))
            .map_err(|error| error.to_string())?;
        match self
            .subscription_actor
            .step()
            .map_err(|error| error.to_string())?
        {
            Some(SessionActorEffect::Subscription(effect)) => Ok(effect),
            Some(effect) => Err(format!("unexpected subscription actor effect: {effect:?}")),
            None => Err("subscription actor produced no effect".into()),
        }
    }

    fn is_dead(&self) -> bool {
        match &self.transport {
            AgentTransport::InProcess { .. } => self
                .thread
                .as_ref()
                .is_some_and(std::thread::JoinHandle::is_finished),
        }
    }

    fn is_fully_stopped(&self) -> bool {
        self.thread
            .as_ref()
            .is_none_or(std::thread::JoinHandle::is_finished)
            && self
                .reader
                .as_ref()
                .is_none_or(std::thread::JoinHandle::is_finished)
    }

    fn wait_until_stopped(&self, seed: &str) -> bool {
        const TERMINAL_WAIT: Duration = Duration::from_secs(30);
        let deadline = Instant::now() + TERMINAL_WAIT;
        while !self.is_fully_stopped() {
            if Instant::now() >= deadline {
                log::error!(
                    "[registry] child {seed} terminal observation timed out after {TERMINAL_WAIT:?}"
                );
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        true
    }

    fn kind_name(&self) -> AgentKind {
        match &self.kind {
            AgentKind::Session => AgentKind::Session,
            AgentKind::Subagent(spec) => AgentKind::Subagent(spec.clone()),
        }
    }

    fn signal_shutdown(&mut self) {
        let _ = self.subscription_actor.submit(SessionCommand::Shutdown);
        let _ = self.subscription_actor.step();
        // 优雅关闭：agent 侧只识别 Ringing 帧（legacy Ui2Agent 已拆除）。
        let env = qaqh_ringing::RingingWorkerCommandEnvelope::new(
            self.seed.clone(),
            "daemon-shutdown",
            qaqh_ringing::RingingCommand::Control(qaqh_domain::ControlCommand::SessionShutdown),
        );
        match &self.transport {
            AgentTransport::InProcess { cmd_tx, cancel } => {
                // D-3：取消必须按会话键控，单会话 close 不得触碰进程级全局
                // flag。实例 token 让 turn 循环在下个检查点解卷（engine_turn
                // 轮询 ctx.cancel.is_set()）；SESSION_CANCELS 让该会话的工具
                // 线程在轮询点立即中止。旧的全局 set_cancel(true) 对已绑定
                // 会话的工具线程不可见（is_cancel 先读会话键控表），却会在
                // daemon 侧残留全局脏标记（C2 同源问题），已废弃。
                cancel.set();
                qaqh_workspace::set_session_cancel(&self.seed, true);
                let cmd = crate::agent::types::WorkerCommand {
                    frame: env,
                    causation: Some("daemon-shutdown".into()),
                };
                let _ = cmd_tx.send(cmd);
            }
        }
    }

    /// Blocking join of the worker loop + reader threads. Contract (D-4):
    /// must run in a blocking context (spawn_blocking / dedicated thread /
    /// daemon teardown) — never directly on a tokio worker thread. The
    /// caller holds the registry mutex across the join by design; other
    /// registry RPCs stall until the join completes.
    fn finish_shutdown(&mut self) {
        // Join the loop thread first so it drops `event_tx`; the event reader
        // then drains the channel tail and publishes the last intents
        // (含 seal_turn——terminal intent 同步落盘).
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        log::info!("stopped agent {}", self.seed);
    }

    fn shutdown(mut self) {
        self.signal_shutdown();
        self.finish_shutdown();
    }
}

/// #345：把 pending interaction 的正文写进 content store 并 pin。
///
/// 正文的 content_id 必须与引擎写进 canonical fact 的 `request_ref` 一致：
/// 两处都走 `qaqh_domain::interaction_body` 的同一个构造函数，**不要**在这里
/// 另写一份序列化（哈希漂移不会有编译期错误，只会让客户端取不到 modal 正文）。
///
/// 超配额（[`ContentQuotaExceeded`]）时只记录错误、不写正文：客户端会拿到
/// 404 并收掉 modal（契约要求把 404 当「正文不可用」）。pinned 额度按 seed 计
/// 且交互终结即释放，正常会话不可能触达。
pub(crate) fn stash_interaction_body(
    hub: &RingingHub,
    seed: &str,
    event: &qaqh_domain::DomainEvent,
) {
    use qaqh_domain::{ControlEvent, ToolEvent, interaction_body};
    // (content store 的交互 key, 正文 bytes, 是否 pin)。
    //
    // ask / plan 用 wire interaction_id 作 key 并 pin（resolve 时按同一个 key unpin）。
    // permission 用 canonical interaction_id 作 key 且**不 pin**：纯 v2 的 wire 上
    // 没有 tool 频道快照，详情只能从正文取；而拒绝/过期路径没有可挂 unpin 的域事件，
    // 走 30min TTL 的普通条目反而不会泄漏 pin 配额。
    let (interaction_id, bytes, pinned) = match event {
        qaqh_domain::DomainEvent::Control(ControlEvent::InteractionRequested {
            interaction_id,
            mode,
            questions,
            ..
        }) => (
            interaction_id.clone(),
            interaction_body::ask_body(*mode, questions),
            true,
        ),
        qaqh_domain::DomainEvent::Control(ControlEvent::PlanReviewRequested {
            interaction_id,
            plan_content,
            review_type,
            todo_items,
            ..
        }) => (
            interaction_id.clone(),
            interaction_body::plan_body(plan_content, review_type, todo_items.as_deref()),
            true,
        ),
        qaqh_domain::DomainEvent::Tool(ToolEvent::ToolPermissionRequested {
            tool_call_id,
            tool_name,
            action_summary,
            reason,
            paths,
            category,
            level,
            risk,
            consequence,
            ..
        }) => (
            crate::agent::tool_runtime::canonical_interaction_id(tool_call_id)
                .as_str()
                .to_string(),
            interaction_body::permission_body(
                tool_name,
                action_summary.as_deref(),
                reason,
                paths,
                *category,
                *level,
                *risk,
                consequence,
            ),
            false,
        ),
        _ => return,
    };
    if !pinned {
        hub.put_content(
            seed,
            interaction_body::INTERACTION_BODY_MEDIA_TYPE,
            bytes,
            false,
        );
        return;
    }
    match hub.put_interaction_content(
        seed,
        &interaction_id,
        interaction_body::INTERACTION_BODY_MEDIA_TYPE,
        bytes,
    ) {
        Ok(content_id) => log::debug!(
            "[ringing] interaction body stashed for {seed}/{interaction_id} -> {content_id}"
        ),
        Err(error) => log::error!(
            "[ringing] interaction body rejected for {seed}/{interaction_id}: {error} \
             (client will 404 the request ref)"
        ),
    }
}

pub(crate) fn externalize_large_content(
    hub: &RingingHub,
    seed: &str,
    event: qaqh_domain::DomainEvent,
) -> qaqh_domain::DomainEvent {
    let qaqh_domain::DomainEvent::Tool(qaqh_domain::ToolEvent::ToolFinished {
        tool_call_id,
        turn_id,
        round_num,
        result,
    }) = event
    else {
        return event;
    };
    let full_text = result.model_text();
    if full_text.len() <= crate::ringing::CONTENT_STORE_THRESHOLD_BYTES {
        return qaqh_domain::DomainEvent::Tool(qaqh_domain::ToolEvent::ToolFinished {
            tool_call_id,
            turn_id,
            round_num,
            result,
        });
    }
    let content_id = hub.put_content(seed, "text/plain", full_text.as_bytes().to_vec(), true);
    // 保留尾部（命令输出通常尾部才是结论），但展示行取**全文开头**——
    // 取 tail 的前 512 字符只会得到输出中段，作为 summary 毫无意义。
    let tail = tail_text(full_text, CONTENT_TAIL_BYTES);
    let head: String = full_text
        .chars()
        .take(qaqh_types::TOOL_SUMMARY_MAX_CHARS)
        .collect();
    let mut projected = result;
    projected.externalize_output(
        tail,
        head,
        qaqh_domain::ContentRef {
            content_id: content_id.clone(),
            media_type: "text/plain".into(),
            sha256: content_id.clone(),
            truncated: true,
        },
    );
    qaqh_domain::DomainEvent::Tool(qaqh_domain::ToolEvent::ToolFinished {
        tool_call_id,
        turn_id,
        round_num,
        result: projected,
    })
}

/// 事件内可渲染 tail 上限。
const CONTENT_TAIL_BYTES: usize = 256 * 1024;

/// 按 char 边界截取文本末尾最多 max_bytes（UTF-8 保守按 4 字节/字符）。
fn tail_text(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let max_chars = max_bytes / 4;
    text.chars()
        .rev()
        .take(max_chars)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

/// Stable, grammar-valid name for legacy direct `spawn_subagent` callers.
fn legacy_child_name(seed: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    seed.hash(&mut hasher);
    format!("sub_{:016x}", hasher.finish())
}

/// Millisecond wall clock for logical agent registration.
fn unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or_default()
}

/// Broadcast 命令 id（时间戳十六进制，语义同 service.rs 的 `command_id`）。
fn broadcast_command_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("bcast-{nanos:x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 正常路径：探测命令秒回，输出拿得到。
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

        let listed = registry
            .list_agents_with_state_for_caller(&root, "/root")
            .expect("list agents with state");
        let child_state = listed
            .iter()
            .find(|agent| agent.agent_id == child.as_str())
            .expect("child listing");
        assert_eq!(child_state.status, ListedAgentStatus::Completed);
        assert_eq!(child_state.residency, ListedAgentResidency::Unloaded);
    }

    #[test]
    fn subscription_actor_is_idempotent_and_shutdown_closes_ingress() {
        let (cmd_tx, _cmd_rx) = std::sync::mpsc::sync_channel(4);
        let mut instance = AgentInstance {
            seed: "seed-subscription".into(),
            transport: AgentTransport::InProcess {
                cmd_tx,
                cancel: crate::agent::types::CancelToken::new(),
            },
            kind: AgentKind::Session,
            subscription_actor: SessionActor::new(4),
            liveness: None,
            reader: None,
            thread: None,
        };
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
        let (cmd_tx, _cmd_rx) = std::sync::mpsc::sync_channel(4);
        let instance = AgentInstance {
            seed: "seed-registry".into(),
            transport: AgentTransport::InProcess {
                cmd_tx,
                cancel: crate::agent::types::CancelToken::new(),
            },
            kind: AgentKind::Session,
            subscription_actor: SessionActor::new(4),
            liveness: None,
            reader: None,
            thread: None,
        };
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
            quota_ledgers: HashMap::new(),
            quota_limits: QuotaLimits::unlimited(),
            armed_collectors: HashSet::new(),
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
            registry.subscribe_channel(
                "seed-registry",
                connection_id.clone(),
                RingingChannel::Tool
            ),
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
        let (parent_tx, parent_rx) = std::sync::mpsc::sync_channel(4);
        let (child_tx, child_rx) = std::sync::mpsc::sync_channel(4);
        let parent = AgentInstance {
            seed: "parent-seed".into(),
            transport: AgentTransport::InProcess {
                cmd_tx: parent_tx,
                cancel: parent_cancel.clone(),
            },
            kind: AgentKind::Session,
            subscription_actor: SessionActor::new(4),
            liveness: None,
            reader: None,
            thread: None,
        };
        let child = AgentInstance {
            seed: "child-seed".into(),
            transport: AgentTransport::InProcess {
                cmd_tx: child_tx,
                cancel: child_cancel.clone(),
            },
            kind: AgentKind::Subagent(SubagentSpawnSpec {
                tools: Vec::new(),
                model: None,
                base_url: None,
                max_tokens: None,
                ephemeral: true,
            }),
            subscription_actor: SessionActor::new(4),
            liveness: None,
            reader: None,
            thread: None,
        };
        let mut registry = AgentRegistry {
            instances: HashMap::from([
                ("parent-seed".into(), parent),
                ("child-seed".into(), child),
            ]),
            activity: SessionActivityTracker::default(),
            sessions,
            hub: None,
            shutting_down: false,
            last_spawn: HashMap::new(),
            supervisor: SubagentSupervisor::default(),
            agent_catalog: AgentCatalog::default(),
            residency: HashMap::new(),
            quota_ledgers: HashMap::new(),
            quota_limits: QuotaLimits::unlimited(),
            armed_collectors: HashSet::new(),
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

        assert!(parent_cancel.is_set());
        assert!(
            child_cancel.is_set(),
            "parent cancellation must propagate through the token tree"
        );
        assert!(
            parent_rx.try_recv().is_ok(),
            "parent cancel command must still be delivered"
        );
        let child_command = child_rx
            .try_recv()
            .expect("child terminal command must still be delivered");
        assert!(matches!(
            child_command.frame.command,
            qaqh_ringing::RingingCommand::Conversation(
                qaqh_domain::ConversationCommand::ConversationCancel { turn_id: None }
            )
        ));
        qaqh_workspace::remove_session_cancel("parent-seed");
        qaqh_workspace::remove_session_cancel("child-seed");
    }

    fn tool_finished(summary: String) -> qaqh_domain::DomainEvent {
        // The worker normally sends the bounded model projection. This test
        // helper also covers the pre-projection large-output boundary used by
        // the content store: `limit = None` 即 NoFold 语义，模型文本不截断。
        let result = if summary.len() > qaqh_types::TOOL_MODEL_MAX_CHARS {
            qaqh_domain::ToolResult::ok_with_limit(summary, None)
        } else {
            qaqh_domain::ToolResult::ok(summary)
        };
        qaqh_domain::DomainEvent::Tool(qaqh_domain::ToolEvent::ToolFinished {
            tool_call_id: "t1".into(),
            turn_id: "turn1".into(),
            round_num: 0,
            result,
        })
    }

    #[test]
    fn large_tool_finished_is_externalized() {
        let hub = RingingHub::new("test");
        let big = "x".repeat(crate::ringing::CONTENT_STORE_THRESHOLD_BYTES + 1024);
        let out = externalize_large_content(&hub, "s1", tool_finished(big.clone()));
        match out {
            qaqh_domain::DomainEvent::Tool(qaqh_domain::ToolEvent::ToolFinished {
                result, ..
            }) => {
                assert!(result.model_text().len() <= CONTENT_TAIL_BYTES);
                assert!(result.summary().chars().count() <= qaqh_types::TOOL_SUMMARY_MAX_CHARS);
                let rf = result.output_ref().expect("output_ref set").clone();
                assert!(rf.truncated);
                assert_eq!(rf.media_type, "text/plain");
                // 完整内容可从 ContentStore 读回（会话所有权校验）
                let entry = hub.get_content("s1", &rf.content_id).expect("stored");
                assert_eq!(entry.bytes.len(), big.len());
                assert_eq!(entry.sha256, rf.sha256);
                // 跨会话不可读
                assert!(hub.get_content("other", &rf.content_id).is_none());
            }
            other => panic!("expected ToolFinished, got {other:?}"),
        }
    }

    #[test]
    fn small_tool_finished_is_not_externalized() {
        let hub = RingingHub::new("test");
        let out = externalize_large_content(&hub, "s1", tool_finished("small".into()));
        match out {
            qaqh_domain::DomainEvent::Tool(qaqh_domain::ToolEvent::ToolFinished {
                result, ..
            }) => {
                assert_eq!(result.summary(), "small");
                assert!(result.output_ref().is_none());
            }
            other => panic!("expected ToolFinished, got {other:?}"),
        }
    }

    #[test]
    fn non_tool_event_passes_through() {
        let hub = RingingHub::new("test");
        let ev =
            qaqh_domain::DomainEvent::Conversation(qaqh_domain::ConversationEvent::TurnStarted {
                turn_id: "t1".into(),
                user_text: "hi".into(),
            });
        let out = externalize_large_content(&hub, "s1", ev);
        assert!(matches!(
            out,
            qaqh_domain::DomainEvent::Conversation(
                qaqh_domain::ConversationEvent::TurnStarted {
                    turn_id,
                    user_text,
                }
            ) if turn_id == "t1" && user_text == "hi"
        ));
    }

    #[test]
    fn tail_text_respects_char_boundaries() {
        // 中文 3 字节/字符：按 4 字节/字符保守截取，不得切半个字符
        let text = "汉".repeat(200_000);
        let tail = tail_text(&text, 1024);
        assert!(tail.len() <= 1024);
        assert!(tail.chars().all(|c| c == '汉'));
        assert_eq!(tail, "汉".repeat(tail.chars().count()));
    }
}
