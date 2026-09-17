use std::collections::HashMap;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use crate::agent::SubagentSpawnSpec;
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
    /// T-1-4：子代理 seed → 派生出它的父会话 seed（`spawn_subagent` 登记，
    /// `close` 清理）。取消传播需要反向查询，故与 `subagent_children` 成对
    /// 维护。
    subagent_parent: HashMap<String, String>,
    /// T-1-4：父会话 seed → 其子代理 seed 集合。父会话收到
    /// `ConversationCancel` 时逐个取消（见 `cancel_subagent_children`）。
    subagent_children: HashMap<String, std::collections::HashSet<String>>,
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
            subagent_parent: HashMap::new(),
            subagent_children: HashMap::new(),
        }
    }

    /// 挂载 Ringing 运行时。Ringing worker 事件只进入 native hub。
    pub fn attach_ringing(&mut self, hub: Arc<RingingHub>) {
        self.hub = Some(hub);
    }

    pub fn get_or_spawn(&mut self, seed: &str) -> Result<(), String> {
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
        if self.instances.contains_key(seed) {
            return Err(format!("agent already running for {seed}"));
        }
        let persist = std::env::var("QAQH_SUBAGENT_PERSIST")
            .is_ok_and(|value| matches!(value.as_str(), "1" | "true" | "on"));
        let ephemeral = !persist;
        // T-1-4：登记「父会话 → 子 seed」。父 seed 取自当前工具线程的运行时
        // 上下文——`spawn_subagent` handler 运行在父 actor 的工具线程上，
        // `ActorToolScope` 已把父会话的 `RUNTIME_CTX` 安装到该线程。daemon
        // RPC `subagent.spawn`（无父上下文）与测试路径读到 None，跳过登记。
        let parent_seed = qaqh_workspace::runtime::context()
            .map(|ctx| ctx.active_session)
            .unwrap_or_default();
        self.spawn_subagent_inprocess(
            seed,
            SubagentSpawnSpec {
                tools: tools.to_vec(),
                model: model.map(str::to_string),
                base_url: base_url.map(str::to_string),
                max_tokens,
                ephemeral,
            },
        )?;
        if !parent_seed.is_empty() && parent_seed != seed {
            self.link_subagent(&parent_seed, seed);
        }
        Ok(())
    }

    fn spawn_subagent_inprocess(
        &mut self,
        seed: &str,
        spec: SubagentSpawnSpec,
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

        let actor_seed = seed.to_string();
        let actor_spec = spec.clone();
        let tools_len = spec.tools.len();
        let liveness = std::sync::Arc::new(crate::agent::liveness::WorkerLiveness::new());
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
                liveness: None,
                reader: Some(reader),
                thread: Some(thread),
            },
        );
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
                liveness: Some(liveness_for_registry),
                reader: Some(reader),
                thread: Some(thread),
            },
        );
        log::info!("[session] spawned in-process actor seed={seed} (no child process)");
        Ok(())
    }

    /// 发送 Ringing worker 命令帧（携带 `wire` 判别字段；worker reader 按 wire 解析）。
    pub fn send_ringing(
        &mut self,
        seed: &str,
        env: &qaqh_ringing::RingingWorkerCommandEnvelope,
    ) -> Result<(), String> {
        self.get_or_spawn(seed)?;
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
        if let Some(dead) = self.instances.remove(seed) {
            dead.shutdown();
        }
        match kind {
            AgentKind::Session => self.get_or_spawn(seed)?,
            AgentKind::Subagent(spec) => self.spawn_subagent_inprocess(seed, spec)?,
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

    pub fn close(&mut self, seed: &str) {
        if let Some(instance) = self.instances.remove(seed) {
            instance.shutdown();
        }
        // T-1-4：会话/子代理关闭即摘除派生登记（等价于 hub 的 `forget_seed`
        // 生命周期点）——父集合与反向指针都不随历史 seed 无界增长。
        self.unlink_subagent(seed);
    }

    /// T-1-4：登记父会话 → 子代理的派生关系（幂等）。
    fn link_subagent(&mut self, parent: &str, child: &str) {
        self.subagent_parent
            .insert(child.to_string(), parent.to_string());
        self.subagent_children
            .entry(parent.to_string())
            .or_default()
            .insert(child.to_string());
    }

    /// T-1-4：摘除某个 seed 的派生登记——作为父会话关闭时丢弃它的子集合；
    /// 作为子代理关闭时从父集合中移除自身。
    fn unlink_subagent(&mut self, seed: &str) {
        if let Some(parent) = self.subagent_parent.remove(seed) {
            let parent_now_empty = match self.subagent_children.get_mut(&parent) {
                Some(children) => {
                    children.remove(seed);
                    children.is_empty()
                }
                None => false,
            };
            if parent_now_empty {
                self.subagent_children.remove(&parent);
            }
        }
        self.subagent_children.remove(seed);
    }

    /// T-1-4：父会话当前登记的子代理 seed（排序后返回，便于日志与测试）。
    fn children_of(&self, parent: &str) -> Vec<String> {
        let mut children: Vec<String> = self
            .subagent_children
            .get(parent)
            .map(|set| set.iter().cloned().collect())
            .unwrap_or_default();
        children.sort();
        children
    }

    /// T-1-4 测试/运维只读视图：父会话登记的子代理 seed。
    #[doc(hidden)]
    pub fn subagent_children(&self, parent: &str) -> Vec<String> {
        self.children_of(parent)
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
                    transport: AgentTransport::InProcess { cmd_tx, cancel },
                    ..
                }) => {
                    // 与 `send_ringing` 的 interrupt 分支一致：先置 token，长
                    // 在途的 gate/tool 工作立即观察到取消。
                    cancel.set();
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
                if !matches!(instance.kind, AgentKind::Session) {
                    return None;
                }
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
        let mut instances: Vec<AgentInstance> = self
            .instances
            .drain()
            .map(|(_, instance)| instance)
            .collect();
        // Signal every worker before waiting on any of them. In-process actors
        // run concurrently (per-actor thread-local state); signal all before
        // joining any, so a busy actor is not left waiting on its channel while
        // shut down.
        for instance in &mut instances {
            instance.signal_shutdown();
        }
        for mut instance in instances {
            instance.finish_shutdown();
        }
    }

    /// F4: 拉起所有已退出且非优雅关闭的 worker。由 daemon 侧周期任务调用；
    /// 带 1 秒退避防止崩溃-重启风暴。优雅关闭（收到 Shutdown 帧后退出、
    /// 或被 `close`/`shutdown_all` 主动结束）的实例不会重启。
    pub fn respawn_dead_agents(&mut self) {
        if self.shutting_down {
            return;
        }
        let dead: Vec<(String, AgentKind)> = self
            .instances
            .iter()
            .filter(|(_, instance)| instance.is_dead())
            .map(|(seed, instance)| (seed.clone(), instance.kind_name()))
            .collect();
        for (seed, kind) in dead {
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
            if let Some(instance) = self.instances.remove(&seed) {
                instance.shutdown();
            }
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
                AgentKind::Subagent(spec) => self.spawn_subagent_inprocess(&seed, spec),
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
    fn is_dead(&self) -> bool {
        match &self.transport {
            AgentTransport::InProcess { .. } => self
                .thread
                .as_ref()
                .is_some_and(std::thread::JoinHandle::is_finished),
        }
    }

    fn kind_name(&self) -> AgentKind {
        match &self.kind {
            AgentKind::Session => AgentKind::Session,
            AgentKind::Subagent(spec) => AgentKind::Subagent(spec.clone()),
        }
    }

    fn signal_shutdown(&mut self) {
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
