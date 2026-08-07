use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};


use crate::{RingingHub, SessionActivityTracker};

static SYSTEM_PATH: OnceLock<String> = OnceLock::new();

pub fn cache_system_path() {
    let mut path = std::env::var("PATH").unwrap_or_default();
    #[cfg(target_os = "windows")]
    for key in [
        r"HKCU\Environment",
        r"HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Environment",
    ] {
        let mut command = background_command("reg");
        if let Ok(output) = command.args(["query", key, "/v", "Path"]).output() {
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

pub fn detect_os_info() {
    #[cfg(target_os = "windows")]
    let info = background_command("cmd")
        .args(["/d", "/c", "ver"])
        .output()
        .ok()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| format!("windows {}", std::env::consts::ARCH));
    #[cfg(not(target_os = "windows"))]
    let info = Command::new("uname")
        .arg("-a")
        .output()
        .ok()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| format!("{} {}", std::env::consts::OS, std::env::consts::ARCH));
    let _ = deepx_config::prompt::OS_INFO.set(info);
    let mut tools = Vec::new();
    for (program, args) in [
        ("git", vec!["--version"]),
        ("cargo", vec!["--version"]),
        ("node", vec!["--version"]),
        ("python", vec!["--version"]),
    ] {
        if let Ok(output) = background_command(program).args(args).output() {
            let value = if output.stdout.is_empty() {
                &output.stderr
            } else {
                &output.stdout
            };
            let value = String::from_utf8_lossy(value).trim().to_string();
            if !value.is_empty() {
                tools.push(value)
            }
        }
    }
    let _ = deepx_config::prompt::TOOLS_INFO.set(tools.join(", "));
}

fn background_command(program: &str) -> Command {
    let mut command = Command::new(program);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

pub struct AgentInstance {
    seed: String,
    stdin: Arc<Mutex<Box<dyn Write + Send>>>,
    child: Arc<Mutex<Option<Child>>>,
    /// stdout 消费线程（registry 侧读取 worker 事件行）。daemon 关闭时
    /// 必须 join：worker 进程退出 ≠ 尾部 intent（含 seal_turn）已消费——
    /// 管道里的最后几行仍由本线程读取并 publish（见 shutdown）。
    reader: Option<std::thread::JoinHandle<()>>,
}

pub struct AgentRegistry {
    instances: HashMap<String, AgentInstance>,
    activity: SessionActivityTracker,
    /// Ringing 运行时；None = 未启用 legacy worker-only 模式。
    hub: Option<Arc<RingingHub>>,
    /// daemon 拉起的 workspace serve endpoint + token（注入每个 worker env）。
    workspace_env: Option<(String, String)>,
    /// daemon 正在关闭：worker 退出是预期的，禁止自动重生。
    shutting_down: bool,
    /// 最近一次 spawn 时间（防崩溃-重启风暴：同一 seed 1 秒内不重复拉起）。
    last_spawn: HashMap<String, std::time::Instant>,
}

impl AgentRegistry {
    pub fn new() -> Self {
        Self {
            instances: HashMap::new(),
            activity: SessionActivityTracker::default(),
            hub: None,
            workspace_env: None,
            shutting_down: false,
            last_spawn: HashMap::new(),
        }
    }

    /// 注入 workspace serve 连接信息；worker spawn 时写入其环境变量。
    pub fn attach_workspace(&mut self, endpoint: String, token: String) {
        self.workspace_env = Some((endpoint, token));
    }

    /// 挂载 Ringing 运行时。Ringing worker 事件只进入 native hub。
    pub fn attach_ringing(&mut self, hub: Arc<RingingHub>) {
        self.hub = Some(hub);
    }

    pub fn get_or_spawn(&mut self, seed: &str) -> Result<(), String> {
        if self.instances.contains_key(seed) {
            return Ok(());
        }
        self.spawn(seed, None)?;
        // Diagnostic: the timeline snapshot is a best-effort async checkpoint
        // and a daemon restart can drop its tail. When it lags the message
        // store (meta.turn_count), the resumed transcript misses turns — the
        // frontend now backfills them from the Ringing conversation store, so
        // this is informational but valuable for restart forensics.
        if let Some(hub) = self.hub.as_ref()
            && let Some(meta) = deepx_session::SessionManager::global().load_meta(seed)
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
        if let Some(hub) = self.hub.as_ref() {
            hub.seal_orphan_running_turns(seed);
            // Ringing 三频道投影的等价收尾：重放的无终态 TurnStarted/
            // ToolStarted/InteractionRequested 同样会污染 bootstrap 快照，
            // 使前端显示陈旧 running turn 与无法批准的幽灵交互面板。
            hub.seal_orphan_channel_state(seed);
        }
        Ok(())
    }

    pub fn spawn_new(&mut self, seed: &str) -> Result<(), String> {
        if self.instances.contains_key(seed) {
            return Err(format!("agent already running for {seed}"));
        }
        self.spawn(seed, Some(seed))
    }

    fn spawn(&mut self, seed: &str, new_seed: Option<&str>) -> Result<(), String> {
        self.last_spawn
            .insert(seed.to_string(), std::time::Instant::now());
        let (generation, _) = self.activity.begin(seed);
        let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
        let mut command = Command::new(exe);
        command.arg("agent");
        if let Some(seed) = new_seed {
            command.arg("--seed").arg(seed);
        } else {
            command.arg("--resume-seed").arg(seed);
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(path) = SYSTEM_PATH.get() {
            command.env("PATH", path);
        }
        if let Some((endpoint, token)) = &self.workspace_env {
            command.env("DEEPX_WORKSPACE_URL", endpoint);
            command.env("DEEPX_WORKSPACE_TOKEN", token);
        }
        // Resume worker：timeline 是 turn 账本的权威源。meta.turn_count 只在
        // turn 完成时持久化（compact 也会缩小消息视图），daemon 重启后它可能
        // 远小于 timeline 已记录的 turn 数——worker 按它恢复分配器就会复用
        // 已 sealed 的 turn id，timeline 侧所有 intent 被拒，transcript 空白。
        // 把 timeline 最大 turn 序号注入环境变量，worker 恢复计数以它为下限。
        // （hub.timeline_snapshot 顺带触发懒加载 + 孤儿收尾，spawn 前完成。）
        if new_seed.is_none()
            && let Some(hub) = self.hub.as_ref()
            && let Some(snapshot) = hub.timeline_snapshot(seed)
        {
            let max_seq = snapshot
                .turns
                .iter()
                .filter_map(|turn| turn.turn_id.strip_prefix('t'))
                .filter_map(|seq| seq.parse::<u64>().ok())
                .max()
                .unwrap_or(0);
            if max_seq > 0 {
                command.env("DEEPX_TIMELINE_TURN_COUNT", max_seq.to_string());
            }
        }
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let mut child = command
            .spawn()
            .map_err(|e| format!("spawn agent for {seed}: {e}"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "agent stdin unavailable".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "agent stdout unavailable".to_string())?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| "agent stderr unavailable".to_string())?;
        let child = Arc::new(Mutex::new(Some(child)));

        let debug_seed = seed.to_string();
        std::thread::spawn(move || {
            // F4: panic 防护——reader 线程若在解析/分发中 panic，整条 stderr
            // 流会静默消失且无法被 respawn 检测到（进程还活着）。catch_unwind
            // 至少把现场记录到日志。
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let reader = BufReader::new(stderr);
                for line in reader.lines().map_while(Result::ok) {
                    log::warn!(
                        "[AGENT:{}] {line}",
                        &debug_seed[..debug_seed.floor_char_boundary(debug_seed.len().min(8))]
                    );
                }
            }));
            log::info!(
                "[AGENT:{}] stderr reader exited",
                &debug_seed[..debug_seed.floor_char_boundary(debug_seed.len().min(8))]
            );
        });

            let event_seed = seed.to_string();
            let activity = self.activity.clone();
            let hub = self.hub.clone();
        let reader = std::thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                for line in BufReader::new(stdout).lines() {
                    let Ok(line) = line else { break };
                    if line.trim().is_empty() {
                        continue;
                    }
                    // wire 判别：M3 后仅 Ringing 线格式（legacy Agent2Ui 帧已拆除）。
                    match deepx_msglp::ringing_v1::wire::read_worker_event_line(&line) {
                        Ok(()) => {
                            if let Some(hub) = &hub {
                                // Ringing V1 timeline intent is a native producer path.
                                if let Ok(env) = serde_json::from_str::<
                                    deepx_ringing::RingingTimelineIntentEnvelope,
                                >(&line)
                                {
                                    if let Err(error) = hub.publish_timeline(&env.seed, env.intent) {
                                        // A rejected intent silently starves the frontend
                                        // transcript (the Ringing conversation store keeps
                                        // delivering, so the session-list title still
                                        // refreshes while the main pane stays blank).
                                        // Surface it at error level with the full error so
                                        // restart-related turn_id collisions are diagnosable.
                                        log::error!(
                                            "[timeline] rejected intent for {}: {error}",
                                            env.seed
                                        );
                                    }
                                    continue;
                                }
                                match serde_json::from_str::<deepx_ringing::RingingWorkerEventEnvelope>(
                                    &line,
                                ) {
                                    Ok(env) => {
                                        let domain: deepx_domain::DomainEvent = env.event.into();
                                        // Ringing 是唯一的 native consumer；大内容在进入
                                        // Ringing channel 前外置。
                                        let domain =
                                            externalize_large_content(&hub, &env.seed, domain);
                                        let _ = hub.publish_with_causation(
                                            &env.seed,
                                            domain.clone(),
                                            env.causation_id.as_deref(),
                                        );
                                        // Activity tracker 生产接线：领域事件驱动
                                        // Working→Idle / →WaitingUser 迁移（此前
                                        // observe 只在测试中调用，daemon 侧活动状态
                                        // 在回合结束后永远停留在 Working）。
                                        if let Some(observe) =
                                            crate::activity::domain_activity_observe(&domain)
                                            && let Some(activity) = activity.observe(
                                                &event_seed,
                                                generation,
                                                &observe,
                                            )
                                        {
                                            crate::activity::publish_activity(
                                                Some(hub.as_ref()),
                                                &activity,
                                            );
                                        }
                                    }
                                    Err(e) => {
                                        log::warn!("invalid ringing worker envelope: {e}")
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            log::warn!("invalid agent event for {event_seed}: {e}");
                        }
                    }
                }
            }));
            if let Err(panic) = result {
                log::error!(
                    "[AGENT:{}] stdout reader panicked: {:?}",
                    &event_seed[..event_seed.floor_char_boundary(event_seed.len().min(8))],
                    panic
                );
            }
            if let Some(update) = activity.disconnect(&event_seed, generation) {
                crate::activity::publish_activity(hub.as_deref(), &update);
            }
        });

        self.instances.insert(
            seed.to_string(),
            AgentInstance {
                seed: seed.to_string(),
                stdin: Arc::new(Mutex::new(Box::new(stdin))),
                child,
                reader: Some(reader),
            },
        );
        Ok(())
    }



    /// 发送 Ringing worker 命令帧（携带 `wire` 判别字段；worker reader 按 wire 解析）。
    pub fn send_ringing(
        &mut self,
        seed: &str,
        env: &deepx_ringing::RingingWorkerCommandEnvelope,
    ) -> Result<(), String> {
        self.get_or_spawn(seed)?;
        let json = serde_json::to_string(env).map_err(|e| format!("serialize: {e}"))?;
        let write = |instance: &AgentInstance| -> Result<(), String> {
            let mut stdin = instance
                .stdin
                .lock()
                .map_err(|e| format!("agent stdin lock: {e}"))?;
            writeln!(*stdin, "{json}").map_err(|e| format!("agent write: {e}"))?;
            stdin.flush().map_err(|e| format!("agent flush: {e}"))
        };
        if write(self.instances.get(seed).expect("spawned instance")).is_ok() {
            return Ok(());
        }
        if let Some(dead) = self.instances.remove(seed) {
            dead.shutdown();
        }
        self.get_or_spawn(seed)?;
        write(self.instances.get(seed).expect("respawned instance"))
    }

    pub fn close(&mut self, seed: &str) {
        if let Some(instance) = self.instances.remove(seed) {
            instance.shutdown();
        }
    }

    pub fn shutdown_all(&mut self) {
        self.shutting_down = true;
        for (_, instance) in self.instances.drain() {
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
        let dead: Vec<String> = self
            .instances
            .iter()
            .filter_map(|(seed, instance)| {
                let exited = instance
                    .child
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_mut()
                    .is_some_and(|child| child.try_wait().ok().flatten().is_some());
                exited.then(|| seed.clone())
            })
            .collect();
        for seed in dead {
            // 退避：同一 seed 最近 1 秒内刚 spawn 过（例如刚拉起又立刻崩溃）
            // 则跳过本轮，避免无意义的重启风暴。
            if self
                .last_spawn
                .get(&seed)
                .is_some_and(|at| at.elapsed() < std::time::Duration::from_secs(1))
            {
                log::warn!(
                    "[AGENT:{seed}] worker exited immediately after spawn; backing off"
                );
                continue;
            }
            if let Some(instance) = self.instances.remove(&seed) {
                instance.shutdown();
            }
            log::warn!("[AGENT:{seed}] worker process died; respawning");
            if let Err(error) = self.spawn(&seed, None) {
                log::error!("[AGENT:{seed}] respawn failed: {error}");
            } else if let Some(hub) = self.hub.as_ref() {
                // 与 get_or_spawn 一致：新 worker 接管前，把 timeline 中任何
                // 未 seal 的 running turn 收尾为 Cancelled。
                hub.seal_orphan_running_turns(&seed);
                hub.seal_orphan_channel_state(&seed);
            }
        }
    }

    pub fn activities(&self) -> Vec<deepx_proto::SessionActivity> {
        self.activity.snapshot()
    }

    pub fn activity(&self, seed: &str) -> Option<deepx_proto::SessionActivity> {
        self.activity.get(seed)
    }

    pub fn reserve_idle(&self, seed: &str) -> Option<deepx_proto::SessionActivity> {
        self.activity.mark_working_if_idle(seed)
    }

    pub fn reserve_for_input(
        &self,
        seed: &str,
    ) -> Option<(
        deepx_proto::SessionActivity,
        deepx_proto::SessionActivityState,
    )> {
        self.activity.mark_working_for_input(seed)
    }

    pub fn rollback_idle_reservation(
        &self,
        seed: &str,
        expected_seq: u64,
    ) -> Option<deepx_proto::SessionActivity> {
        self.activity.restore_idle_if_unchanged(seed, expected_seq)
    }

    pub fn rollback_input_reservation(
        &self,
        seed: &str,
        expected_seq: u64,
        previous: deepx_proto::SessionActivityState,
    ) -> Option<deepx_proto::SessionActivity> {
        self.activity
            .restore_state_if_unchanged(seed, expected_seq, previous)
    }

    pub fn is_running(&self, seed: &str) -> bool {
        self.instances.contains_key(seed)
    }

    /// 向所有存活 agent 广播同一 Ringing 命令。
    pub fn send_ringing_all(&mut self, command: deepx_ringing::RingingCommand) {
        let seeds: Vec<_> = self.instances.keys().cloned().collect();
        for seed in seeds {
            let env = deepx_ringing::RingingWorkerCommandEnvelope::new(
                seed.clone(),
                "daemon-broadcast",
                command.clone(),
            );
            let _ = self.send_ringing(&seed, &env);
        }
    }
}

impl AgentInstance {
    fn shutdown(mut self) {
        // 优雅关闭：agent 侧只识别 Ringing 帧（legacy Ui2Agent 已拆除）。
        let env = deepx_ringing::RingingWorkerCommandEnvelope::new(
            self.seed.clone(),
            "daemon-shutdown",
            deepx_ringing::RingingCommand::Control(
                deepx_domain::ControlCommand::SessionShutdown,
            ),
        );
        if let Ok(json) = serde_json::to_string(&env)
            && let Ok(mut stdin) = self.stdin.lock()
        {
            let _ = writeln!(*stdin, "{json}");
            let _ = stdin.flush();
        }
        if let Ok(mut child) = self.child.lock()
            && let Some(mut child) = child.take()
        {
            let started = std::time::Instant::now();
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if started.elapsed() < std::time::Duration::from_secs(5) => {
                        std::thread::sleep(std::time::Duration::from_millis(50))
                    }
                    _ => {
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                }
            }
        }
        // 进程退出后 stdout 已 EOF，join 消费线程把管道尾部排空：worker 的
        // 最后几个 intent（含 seal_turn——terminal intent 同步落盘）必须被
        // 读取并 publish 后才算收尾完成；否则退出后 flush 缺该 seal，重启
        // 时被孤儿收尾误标 daemon_restart_interrupted（每次安装更新的
        // 必现根因）。线程在 EOF 后自然退出，join 通常毫秒级返回。
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        log::info!("stopped agent {}", self.seed);
    }
}

/// ToolFinished 大内容外置（PLAN 大内容外置）：summary 超过 10 MiB 时
/// 存入 ContentStore（会话所有权 + TTL），事件替换为 tail（256 KiB）+
/// `output_ref`。Ringing 与 legacy 客户端都只收到 tail，完整内容经
/// `GET /ringing/v1/content/{id}?seed=` 按需读取。
///
/// 非 ToolFinished 事件和未超阈值的 ToolFinished 事件原样返回。
fn externalize_large_content(
    hub: &RingingHub,
    seed: &str,
    event: deepx_domain::DomainEvent,
) -> deepx_domain::DomainEvent {
    let deepx_domain::DomainEvent::Tool(deepx_domain::ToolEvent::ToolFinished {
        tool_call_id,
        turn_id,
        round_num,
        result,
    }) = event
    else {
        return event;
    };
    let full_text = result.model.text.as_str();
    if full_text.len() <= crate::ringing::content_store::CONTENT_STORE_THRESHOLD_BYTES {
        return deepx_domain::DomainEvent::Tool(deepx_domain::ToolEvent::ToolFinished {
            tool_call_id,
            turn_id,
            round_num,
            result,
        });
    }
    let content_id = hub.put_content(seed, "text/plain", full_text.as_bytes().to_vec(), true);
    let tail = tail_text(full_text, CONTENT_TAIL_BYTES);
    let mut projected = result;
    projected.summary = tail
        .chars()
        .take(deepx_types::TOOL_SUMMARY_MAX_CHARS)
        .collect();
    projected.model.text = tail;
    projected.model.truncated = true;
    projected.output_ref = Some(deepx_domain::ContentRef {
        content_id: content_id.clone(),
        media_type: "text/plain".into(),
        sha256: content_id.clone(),
        truncated: true,
    });
    deepx_domain::DomainEvent::Tool(deepx_domain::ToolEvent::ToolFinished {
        tool_call_id,
        turn_id,
        round_num,
        result: projected,
    })
}

/// 事件内可渲染 tail 上限（与 ToolProgress tail 对齐）。
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_finished(summary: String) -> deepx_domain::DomainEvent {
        let mut result = deepx_domain::ToolResult::ok(summary.clone());
        // The worker normally sends the bounded model projection. This test
        // helper also covers the pre-projection large-output boundary used by
        // the content store.
        if summary.len() > deepx_types::TOOL_MODEL_MAX_CHARS {
            result.model.text = summary;
            result.model.truncated = false;
        }
        deepx_domain::DomainEvent::Tool(deepx_domain::ToolEvent::ToolFinished {
            tool_call_id: "t1".into(),
            turn_id: "turn1".into(),
            round_num: 0,
            result,
        })
    }

    #[test]
    fn large_tool_finished_is_externalized() {
        let hub = RingingHub::new("test");
        let big = "x".repeat(crate::ringing::content_store::CONTENT_STORE_THRESHOLD_BYTES + 1024);
        let out = externalize_large_content(&hub, "s1", tool_finished(big.clone()));
        match out {
            deepx_domain::DomainEvent::Tool(deepx_domain::ToolEvent::ToolFinished {
                result,
                ..
            }) => {
                assert!(result.model.text.len() <= CONTENT_TAIL_BYTES);
                assert!(result.summary.chars().count() <= deepx_types::TOOL_SUMMARY_MAX_CHARS);
                let rf = result.output_ref.expect("output_ref set");
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
            deepx_domain::DomainEvent::Tool(deepx_domain::ToolEvent::ToolFinished {
                result,
                ..
            }) => {
                assert_eq!(result.summary, "small");
                assert!(result.output_ref.is_none());
            }
            other => panic!("expected ToolFinished, got {other:?}"),
        }
    }

    #[test]
    fn non_tool_event_passes_through() {
        let hub = RingingHub::new("test");
        let ev =
            deepx_domain::DomainEvent::Conversation(deepx_domain::ConversationEvent::TurnStarted {
                turn_id: "t1".into(),
                user_text: "hi".into(),
            });
        let out = externalize_large_content(&hub, "s1", ev);
        assert!(matches!(
            out,
            deepx_domain::DomainEvent::Conversation(
                deepx_domain::ConversationEvent::TurnStarted {
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
