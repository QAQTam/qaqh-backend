use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};

use deepx_proto::{Agent2Ui, Ui2Agent};

use crate::{EventBus, RingingHub, SessionActivityTracker};

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
}

pub struct AgentRegistry {
    instances: HashMap<String, AgentInstance>,
    events: EventBus,
    activity: SessionActivityTracker,
    /// Ringing 运行时；None = 未启用 legacy worker-only 模式。
    hub: Option<Arc<RingingHub>>,
    /// daemon 拉起的 workspace serve endpoint + token（注入每个 worker env）。
    workspace_env: Option<(String, String)>,
}

impl AgentRegistry {
    pub fn new(events: EventBus) -> Self {
        Self {
            instances: HashMap::new(),
            events,
            activity: SessionActivityTracker::default(),
            hub: None,
            workspace_env: None,
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
        self.spawn(seed, None)
    }

    pub fn spawn_new(&mut self, seed: &str) -> Result<(), String> {
        if self.instances.contains_key(seed) {
            return Err(format!("agent already running for {seed}"));
        }
        self.spawn(seed, Some(seed))
    }

    fn spawn(&mut self, seed: &str, new_seed: Option<&str>) -> Result<(), String> {
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
            let reader = BufReader::new(stderr);
            for line in reader.lines().map_while(Result::ok) {
                log::warn!(
                    "[AGENT:{}] {line}",
                    &debug_seed[..debug_seed.floor_char_boundary(debug_seed.len().min(8))]
                );
            }
        });

        let event_seed = seed.to_string();
        let events = self.events.clone();
        let activity = self.activity.clone();
        let hub = self.hub.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if line.trim().is_empty() {
                    continue;
                }
                // wire 判别：默认 legacy；Ringing 事件行在 ChannelRouter 接入前跳过
                let event = match deepx_msglp::ringing_v1::wire::read_worker_event_line(&line) {
                    Ok(Some(event)) => event,
                    Ok(None) => {
                        // Ringing V1 timeline intent is a native producer path: it is
                        // intentionally not projected through Agent2Ui or a legacy wire.
                        if let Some(hub) = &hub {
                            if let Ok(env) = serde_json::from_str::<
                                deepx_ringing::RingingTimelineIntentEnvelope,
                            >(&line)
                            {
                                if let Err(error) = hub.publish_timeline(&env.seed, env.intent) {
                                    log::warn!(
                                        "rejected timeline intent for {}: {error}",
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
                                    // Ringing channel 前外置，兼容投影不再参与这条路径。
                                    let domain = externalize_large_content(&hub, &env.seed, domain);
                                    let _ = hub.publish_with_causation(
                                        &env.seed,
                                        domain.clone(),
                                        env.causation_id.as_deref(),
                                    );
                                }
                                Err(e) => log::warn!("invalid ringing worker envelope: {e}"),
                            }
                        }
                        continue;
                    }
                    Err(e) => {
                        log::warn!("invalid agent event for {event_seed}: {e}");
                        continue;
                    }
                };
                if let Ok(value) = serde_json::to_value(&event)
                    && let Some(update) = activity.observe(&event_seed, generation, &value)
                {
                    crate::activity::publish_activity_dual(&events, hub.as_deref(), &update);
                }
                events.publish(&event_seed, event);
            }
            let event = Agent2Ui::Error {
                message: format!(
                    "Agent process for session {} exited",
                    &event_seed[..event_seed.floor_char_boundary(event_seed.len().min(8))]
                ),
            };
            events.publish(&event_seed, event);
            if let Some(update) = activity.disconnect(&event_seed, generation) {
                crate::activity::publish_activity_dual(&events, hub.as_deref(), &update);
            }
        });

        self.instances.insert(
            seed.to_string(),
            AgentInstance {
                seed: seed.to_string(),
                stdin: Arc::new(Mutex::new(Box::new(stdin))),
                child,
            },
        );
        Ok(())
    }

    pub fn send(&mut self, seed: &str, frame: Ui2Agent) -> Result<(), String> {
        self.get_or_spawn(seed)?;
        let json = serde_json::to_string(&frame).map_err(|e| format!("serialize: {e}"))?;
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
        for (_, instance) in self.instances.drain() {
            instance.shutdown();
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

    pub fn send_all(&mut self, frame: Ui2Agent) {
        let seeds: Vec<_> = self.instances.keys().cloned().collect();
        for seed in seeds {
            let _ = self.send(&seed, frame.clone());
        }
    }
}

impl AgentInstance {
    fn shutdown(self) {
        if let Ok(json) = serde_json::to_string(&Ui2Agent::Shutdown)
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
fn agent2ui_channel(event: &Agent2Ui) -> Option<deepx_domain::RingingChannel> {
    use deepx_domain::RingingChannel::{Control, Conversation, Tool};
    Some(match event {
        Agent2Ui::TurnStart { .. }
        | Agent2Ui::TurnEnd { .. }
        | Agent2Ui::RoundDelta { .. }
        | Agent2Ui::RoundComplete { .. }
        | Agent2Ui::SessionRestored { .. }
        | Agent2Ui::MoreTurns { .. }
        | Agent2Ui::ProviderRetrying { .. }
        | Agent2Ui::UsageUpdated { .. }
        | Agent2Ui::CacheDiagnostics { .. }
        | Agent2Ui::CompactStart { .. }
        | Agent2Ui::CompactEnd { .. }
        | Agent2Ui::CompactDelta { .. }
        | Agent2Ui::Cancelled => Conversation,
        Agent2Ui::ToolResults { .. }
        | Agent2Ui::ToolExecDelta { .. }
        | Agent2Ui::ExecProgress { .. }
        | Agent2Ui::ToolCallPreview { .. }
        | Agent2Ui::ToolNotice { .. }
        | Agent2Ui::AuditRecord { .. }
        | Agent2Ui::CodeDelta { .. }
        | Agent2Ui::PermissionRequest { .. } => Tool,
        Agent2Ui::SessionCreated { .. }
        | Agent2Ui::Error { .. }
        | Agent2Ui::PlanSubmitted { .. }
        | Agent2Ui::PlanResolved { .. }
        | Agent2Ui::Dashboard { .. }
        | Agent2Ui::Done
        | Agent2Ui::ShutdownAck
        | Agent2Ui::Ready
        | Agent2Ui::SkillsChanged { .. }
        | Agent2Ui::SkillOperationResolved { .. }
        | Agent2Ui::AskUser { .. }
        | Agent2Ui::AskResolved { .. }
        | Agent2Ui::AskRejected { .. } => Control,
        _ => return None,
    })
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

    /// 构造 Agent2Ui 最小变体（36 变体全覆盖，字段按 agent_protocol.rs 定义）。
    fn sample_agent2ui(variant: &str) -> Agent2Ui {
        use deepx_proto::{AskMode, AskResolution, PermissionRisk};
        match variant {
            "TurnStart" => Agent2Ui::TurnStart {
                turn_id: "t".into(),
                user_text: "u".into(),
            },
            "TurnEnd" => Agent2Ui::TurnEnd {
                turn_id: "t".into(),
                stop_reason: None,
                usage: None,
            },
            "RoundDelta" => Agent2Ui::RoundDelta {
                turn_id: "t".into(),
                round_num: 0,
                kind: deepx_proto::RoundDeltaKind::Answering,
                delta: "d".into(),
            },
            "RoundComplete" => Agent2Ui::RoundComplete {
                turn_id: "t".into(),
                round_num: 0,
                thinking: None,
                answer: None,
                tool_calls: vec![],
                blocks: vec![],
                is_final: true,
            },
            "ToolResults" => Agent2Ui::ToolResults {
                turn_id: "t".into(),
                round_num: 0,
                results: vec![],
            },
            "ToolExecDelta" => Agent2Ui::ToolExecDelta {
                tool_call_id: "c".into(),
                delta: "d".into(),
            },
            "SessionRestored" => Agent2Ui::SessionRestored {
                seed: "s".into(),
                turns: vec![],
                tokens_used: 0,
                cache_hit_pct: 0.0,
                usage: None,
                usage_totals: deepx_types::UsageInfo::default(),
                usage_requests: 0,
                cache_reported_requests: 0,
                total_turns: 0,
                has_more: false,
            },
            "MoreTurns" => Agent2Ui::MoreTurns {
                turns: vec![],
                has_more: false,
            },
            "SessionCreated" => Agent2Ui::SessionCreated { seed: "s".into() },
            "Error" => Agent2Ui::Error {
                message: "e".into(),
            },
            "ProviderRetrying" => Agent2Ui::ProviderRetrying {
                turn_id: "t".into(),
                round_num: 0,
                attempt: 1,
                max_retries: 3,
                delay_secs: 1,
                error: "e".into(),
            },
            "ToolNotice" => Agent2Ui::ToolNotice {
                message: "m".into(),
                level: "info".into(),
            },
            "PlanSubmitted" => Agent2Ui::PlanSubmitted {
                call_id: "c".into(),
                plan_content: "p".into(),
                review_type: "r".into(),
                todo_items: None,
            },
            "PlanResolved" => Agent2Ui::PlanResolved {
                call_id: "c".into(),
                approved: true,
            },
            "Dashboard" => Agent2Ui::Dashboard {
                hp_connected: false,
                session_seed: "s".into(),
                tool_calls_total: 0,
                tool_failures: 0,
                current_phase: "p".into(),
                streaming: false,
                dsml_compat_count: 0,
                documents: vec![],
                recent_edits: vec![],
                tasks: vec![],
                current_todo_id: None,
                session_title: None,
                usage: None,
                context_limit: 0,
                model: None,
            },
            "UsageUpdated" => Agent2Ui::UsageUpdated {
                turn_id: "t".into(),
                round_num: 0,
                usage: deepx_types::UsageInfo::default(),
                context_limit: 1,
                model: "m".into(),
            },
            "CacheDiagnostics" => Agent2Ui::CacheDiagnostics {
                prefix_hash: "h".into(),
                prefix_changed: false,
                change_reasons: vec![],
            },
            "Done" => Agent2Ui::Done,
            "CompactStart" => Agent2Ui::CompactStart {
                turns_total: 1,
                turns_keeping: 1,
            },
            "CompactEnd" => Agent2Ui::CompactEnd {
                summary_chars: 0,
                turns_compacted: 0,
                turns_removed: 0,
            },
            "CompactDelta" => Agent2Ui::CompactDelta { delta: "d".into() },
            "Cancelled" => Agent2Ui::Cancelled,
            "ShutdownAck" => Agent2Ui::ShutdownAck,
            "Ready" => Agent2Ui::Ready,
            "AuditRecord" => Agent2Ui::AuditRecord {
                tool_name: "n".into(),
                result_summary: "r".into(),
                success: true,
                time: "t".into(),
                args: "a".into(),
            },
            "ExecProgress" => Agent2Ui::ExecProgress {
                tool_call_id: "c".into(),
                stream: "stdout".into(),
                seq: 0,
                chunk: "c".into(),
            },
            "ToolCallPreview" => Agent2Ui::ToolCallPreview {
                turn_id: "t".into(),
                round_num: 0,
                index: 0,
                id: "i".into(),
                name: "n".into(),
                args_so_far: "a".into(),
            },
            "CodeDelta" => Agent2Ui::CodeDelta {
                lines_added: 1,
                lines_removed: 0,
                files_created: 0,
                files_deleted: 0,
                file: None,
            },
            "SkillsChanged" => Agent2Ui::SkillsChanged {
                status: deepx_proto::SkillsStatus {
                    available: vec![],
                    active: vec![],
                    catalog_revision: String::new(),
                    context_epoch: 0,
                    operation_revision: 0,
                    token_budget: 0,
                    token_usage: 0,
                    runtime: vec![],
                    diagnostics: vec![],
                },
            },
            "SkillOperationResolved" => Agent2Ui::SkillOperationResolved {
                operation_id: "o".into(),
                success: true,
                revision: 1,
                error: None,
            },
            "PermissionRequest" => Agent2Ui::PermissionRequest {
                tool_call_id: "c".into(),
                tool_name: "n".into(),
                reason: "r".into(),
                paths: vec![],
                category: "c".into(),
                level: 1,
                risk: PermissionRisk::Low,
                consequence: "c".into(),
            },
            "AskUser" => Agent2Ui::AskUser {
                turn_id: "t".into(),
                round_num: 0,
                ask_id: "a".into(),
                mode: AskMode::Single,
                questions: vec![],
            },
            "AskResolved" => Agent2Ui::AskResolved {
                ask_id: "a".into(),
                resolution: AskResolution::Answered,
            },
            "AskRejected" => Agent2Ui::AskRejected {
                ask_id: "a".into(),
                message: "m".into(),
            },
            other => panic!("unhandled variant in sample_agent2ui: {other}"),
        }
    }

    #[test]
    fn agent2ui_channel_covers_all_variants() {
        use deepx_domain::RingingChannel::{Control, Conversation, Tool};
        let expect = [
            ("TurnStart", Conversation),
            ("TurnEnd", Conversation),
            ("RoundDelta", Conversation),
            ("RoundComplete", Conversation),
            ("SessionRestored", Conversation),
            ("MoreTurns", Conversation),
            ("ProviderRetrying", Conversation),
            ("UsageUpdated", Conversation),
            ("CacheDiagnostics", Conversation),
            ("CompactStart", Conversation),
            ("CompactEnd", Conversation),
            ("CompactDelta", Conversation),
            ("Cancelled", Conversation),
            ("ToolResults", Tool),
            ("ToolExecDelta", Tool),
            ("ExecProgress", Tool),
            ("ToolCallPreview", Tool),
            ("ToolNotice", Tool),
            ("AuditRecord", Tool),
            ("CodeDelta", Tool),
            ("PermissionRequest", Tool),
            ("SessionCreated", Control),
            ("Error", Control),
            ("PlanSubmitted", Control),
            ("PlanResolved", Control),
            ("Dashboard", Control),
            ("Done", Control),
            ("ShutdownAck", Control),
            ("Ready", Control),
            ("SkillsChanged", Control),
            ("SkillOperationResolved", Control),
            ("AskUser", Control),
            ("AskResolved", Control),
            ("AskRejected", Control),
        ];
        for (name, expected) in expect {
            let event = sample_agent2ui(name);
            assert_eq!(
                agent2ui_channel(&event),
                Some(expected),
                "variant {name} must belong to {expected:?}",
            );
        }
    }
}
