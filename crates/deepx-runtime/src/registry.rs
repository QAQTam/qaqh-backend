use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};

use deepx_proto::{Agent2Ui, Ui2Agent};

use crate::{EventBus, SessionActivityTracker};

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
}

impl AgentRegistry {
    pub fn new(events: EventBus) -> Self {
        Self {
            instances: HashMap::new(),
            events,
            activity: SessionActivityTracker::default(),
        }
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
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if line.trim().is_empty() {
                    continue;
                }
                let Ok(event) = serde_json::from_str::<Agent2Ui>(&line) else {
                    log::warn!("invalid agent event for {event_seed}");
                    continue;
                };
                if let Ok(value) = serde_json::to_value(&event)
                    && let Some(update) = activity.observe(&event_seed, generation, &value)
                {
                    events.publish_activity(update);
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
                events.publish_activity(update);
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
