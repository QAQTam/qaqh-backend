//! dsx-hp — Health Platform daemon.
//!
//! Security boundary process that holds API keys, proxies LLM requests,
//! tracks process liveness, and exposes a `HealthProbe` service over TCP.
//!
//! ## IPC protocol
//!
//! JSON-LP frames over TCP `localhost`. Port written to
//! `~/.dsx/hp.port`.
//!
//! ## Build
//!
//! This is a separate binary target. Add to `Cargo.toml`:
//!
//! ```toml
//! [[bin]]
//! name = "dsx-hp"
//! path = "src/dsx-hp/main.rs"
//! ```
//!
//! ## Startup sequence
//!
//! 1. Parse CLI args (port, log level).
//! 2. Bind TCP listener on `127.0.0.1:{port}`.
//! 3. Write port to `$XDG_RUNTIME_DIR/dsx/hp.port`.
//! 4. Enter accept loop — one thread per connection.
//! 5. On SIGTERM/SIGINT: write empty port file, drain connections, exit.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use crate::config::Config;
use crate::ipc_traits::HealthProbe;
use crate::liveness::LivenessResult;
use crate::registry::ProcessRegistry;
use crate::types::{HpError, ProcessKind, Verdict};
use crate::StreamEvent;
use dsx_gateway::GatewayConfig;

static HP_CONFIG: OnceLock<Config> = OnceLock::new();
static HP_RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

// ── Health service implementation ──

/// Concrete implementation of the `HealthProbe` trait.
///
/// Owns the process registry and delegates pipeline judgment.
/// Runs synchronously; the IPC layer wraps calls in async tasks.
struct HealthService {
    registry: ProcessRegistry,
}

impl HealthService {
    fn new(timeout_secs: u64) -> Self {
        Self {
            registry: ProcessRegistry::new(timeout_secs),
        }
    }
}

impl HealthProbe for HealthService {
    fn register(
        &mut self,
        kind: ProcessKind,
        name: &str,
        pid: u32,
    ) -> Result<(), HpError> {
        self.registry.register(kind, name, pid)
    }

    fn heartbeat(&mut self, pid: u32) -> Result<(), HpError> {
        self.registry.heartbeat(pid)
    }

    fn unregister(&mut self, pid: u32) -> Result<(), HpError> {
        self.registry.unregister(pid)
    }

    fn judge(&self) -> Vec<Verdict> {
        let mut verdicts = Vec::new();
        for (pid, result) in self.registry.check_all() {
            if let LivenessResult::Dead { reason } = &result {
                if let Some(reg) = self.registry.query(pid) {
                    let since = reg.liveness.last_activity.elapsed().as_secs();
                    verdicts.push(Verdict::Dead {
                        pid,
                        name: reg.name.clone(),
                        reason: reason.clone(),
                        since_secs: since,
                    });
                }
            }
        }
        verdicts
    }

    fn query(&self, pid: u32) -> Result<crate::types::ProcessHealth, HpError> {
        self.registry.health(pid)
    }

    fn list_processes(&self) -> Vec<crate::types::ProcessSummary> {
        self.registry.summaries()
    }
}

// ── Main ──

/// Load API config: config.json (priority) then env vars, then defaults.
fn load_hp_config() -> Config {
    let mut protocol = std::env::var("DSX_PROTOCOL").unwrap_or_default();
    if protocol.is_empty() { protocol = "openai".into(); }

    // Config file first (highest priority)
    let cfg_path = dsx_types::platform::config_path();

    if let Ok(data) = std::fs::read_to_string(&cfg_path) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&data) {
            let api_key = v.get("api_key").and_then(|k| k.as_str()).unwrap_or("").to_string();
            let base_url = v.get("base_url").and_then(|b| b.as_str()).unwrap_or("https://api.deepseek.com").to_string();
            let proto = v.get("protocol").and_then(|p| p.as_str()).unwrap_or(&protocol).to_string();
            return Config { base_url, api_key, protocol: proto };
        }
    }

    // Fallback: env vars (for Claude Code / legacy setups)
    let env_key = std::env::var("ANTHROPIC_AUTH_TOKEN").unwrap_or_default();
    let env_url = std::env::var("ANTHROPIC_BASE_URL").unwrap_or_default();

    if !env_key.is_empty() {
        let default_url = if protocol == "anthropic" {
            "https://api.anthropic.com"
        } else {
            "https://api.deepseek.com"
        };
        return Config {
            base_url: if env_url.is_empty() { default_url.into() } else { env_url },
            api_key: env_key,
            protocol,
        };
    }

    Config { base_url: "https://api.deepseek.com".into(), api_key: String::new(), protocol }
}

pub fn run() {
    // 0. Load API config
    let hp_cfg = load_hp_config();
    let _ = HP_CONFIG.set(hp_cfg);
    let _ = HP_RUNTIME.set(tokio::runtime::Runtime::new().expect("create tokio runtime"));
    if HP_CONFIG.get().map_or(true, |c| c.api_key.is_empty()) {
        eprintln!("dsx-hp: WARNING — no API key configured, run 'dsx config' to set up");
    } else {
        eprintln!("dsx-hp: API proxy configured");
    }

    // 1. CLI defaults
    let port = std::env::var("DSX_HP_PORT")
        .ok()
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or(0); // 0 = OS-assigned

    let timeout_secs = std::env::var("DSX_HP_TIMEOUT")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(30);

    let addr = format!("127.0.0.1:{port}");

    // 2. Bind TCP listener
    let listener = TcpListener::bind(&addr).expect("dsx-hp: failed to bind TCP listener");
    let actual_port = listener.local_addr().unwrap().port();

    // 3. Write port file
    write_port_file(actual_port);
    eprintln!("dsx-hp: listening on 127.0.0.1:{actual_port} (timeout={timeout_secs}s)");

    // 4. Create health service
    let service = Arc::new(Mutex::new(HealthService::new(timeout_secs)));

    // 5. Accept loop
    let (_shutdown_tx, shutdown_rx) = mpsc::channel::<()>();

    // Register self as the HP process
    {
        let mut svc = service.lock().unwrap();
        svc.register(ProcessKind::Tui, "dsx-hp", std::process::id())
            .ok();
    }

    // Heartbeat ticker — self-heartbeat every 15s
    let svc_heartbeat = service.clone();
    let _hb_ticker = thread::spawn(move || loop {
        thread::sleep(Duration::from_secs(15));
        if shutdown_rx.try_recv().is_ok() {
            break;
        }
        if let Ok(mut svc) = svc_heartbeat.lock() {
            svc.heartbeat(std::process::id()).ok();
        }
    });

    // Signal handler (SIGINT/SIGTERM)
    let svc_shutdown = service.clone();
    let _sig_handler = thread::spawn(move || {
        let _ = svc_shutdown;
        thread::park();
    });

    // Accept connections
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let svc = service.clone();
                thread::spawn(|| handle_connection(stream, svc));
            }
            Err(e) => {
                eprintln!("dsx-hp: accept error: {e}");
            }
        }
    }

    // Cleanup on exit
    clear_port_file();
}

// ── Connection handler ──

fn handle_connection(
    stream: TcpStream,
    service: Arc<Mutex<HealthService>>,
) {
    let peer = stream.peer_addr().ok();
    let mut reader = BufReader::new(stream.try_clone().unwrap());

    let mut buf = String::new();
    loop {
        buf.clear();
        match reader.read_line(&mut buf) {
            Ok(0) => break, // EOF
            Ok(_) => {
                let trimmed = buf.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if trimmed.contains("\"api_chat\"") {
                    if let Ok(mut w) = stream.try_clone() {
                        handle_api_chat_streaming(trimmed, &mut w);
                    }
                } else {
                    let response = dispatch_frame(trimmed, &service);
                    if let Ok(mut w) = stream.try_clone() {
                        let _ = writeln!(w, "{response}");
                        let _ = w.flush();
                    }
                }
            }
            Err(_) => break,
        }
    }

    if let Some(addr) = peer {
        eprintln!("dsx-hp: connection closed: {addr}");
    }
}

fn dispatch_frame(
    line: &str,
    service: &Mutex<HealthService>,
) -> String {
    let frame_type: String = serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .and_then(|v| v.get("type").and_then(|v| v.as_str().map(String::from)))
        .unwrap_or_default();

    let mut svc = service.lock().unwrap();

    match frame_type.as_str() {
        "register" => {
            let kind = if line.contains("Agent") {
                ProcessKind::Agent
            } else if line.contains("Tools") {
                ProcessKind::Tools
            } else {
                ProcessKind::Tui
            };
            let pid = extract_pid(line).unwrap_or(0);
            let name = extract_name(line).unwrap_or("unknown");

            match svc.register(kind, name, pid) {
                Ok(()) => json_response("ok", "registered"),
                Err(e) => json_response("error", &e.to_string()),
            }
        }
        "heartbeat" => {
            let pid = extract_pid(line).unwrap_or(0);
            match svc.heartbeat(pid) {
                Ok(()) => json_response("ok", "heartbeat recorded"),
                Err(e) => json_response("error", &e.to_string()),
            }
        }
        "unregister" => {
            let pid = extract_pid(line).unwrap_or(0);
            match svc.unregister(pid) {
                Ok(()) => json_response("ok", "unregistered"),
                Err(e) => json_response("error", &e.to_string()),
            }
        }
        "judge" => {
            let verdicts = svc.judge();
            let json = serde_json::to_string(&verdicts).unwrap_or_else(|_| "[]".into());
            format!("{{\"type\":\"verdicts\",\"data\":{json}}}")
        }
        "query" => {
            let pid = extract_pid(line).unwrap_or(0);
            match svc.query(pid) {
                Ok(health) => {
                    let data = serde_json::to_string(&health).unwrap_or_default();
                    format!("{{\"type\":\"health\",\"data\":{data}}}")
                }
                Err(e) => json_response("error", &e.to_string()),
            }
        }
        "list" => {
            let summaries = svc.list_processes();
            let json = serde_json::to_string(&summaries).unwrap_or_else(|_| "[]".into());
            format!("{{\"type\":\"process_list\",\"data\":{json}}}")
        }
        _ => json_response("error", &format!("unknown frame type: {frame_type}")),
    }
}

// ── Helpers ──

fn json_response(status: &str, message: &str) -> String {
    serde_json::json!({"type": status, "message": message}).to_string()
}

/// Streaming variant of `handle_api_chat` — writes content deltas to `writer`
/// as they arrive from the LLM, then a final `api_response` frame.
fn handle_api_chat_streaming(line: &str, writer: &mut impl Write) {
    let config = match HP_CONFIG.get() {
        Some(c) => c.clone(),
        None => {
            let _ = writeln!(writer, "{}", json_response("error", "HP not configured (no API key)"));
            let _ = writer.flush();
            return;
        }
    };

    let v: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            let _ = writeln!(writer, "{}", json_response("error", &format!("invalid api_chat frame: {e}")));
            let _ = writer.flush();
            return;
        }
    };

    let model = v.get("model").and_then(|v| v.as_str()).unwrap_or("deepseek-v4-flash");
    let system = v.get("system").and_then(|v| v.as_str()).map(String::from);
    let effort = v.get("effort").and_then(|v| v.as_str()).map(String::from);
    let max_tokens = v.get("max_tokens").and_then(|v| v.as_u64()).unwrap_or(8192) as u32;

    let messages_val: Vec<dsx_types::Message> = match v.get("messages")
        .and_then(|m| serde_json::from_value::<Vec<dsx_types::Message>>(m.clone()).ok())
    {
        Some(msgs) => msgs,
        None => {
            let _ = writeln!(writer, "{}", json_response("error", "api_chat: invalid messages"));
            let _ = writer.flush();
            return;
        }
    };

    let rt = match HP_RUNTIME.get() {
        Some(r) => r,
        None => {
            let _ = writeln!(writer, "{}", json_response("error", "HP runtime not ready"));
            let _ = writer.flush();
            return;
        }
    };

    let tools: Option<Vec<dsx_types::ToolDef>> = v.get("tools")
        .and_then(|t| serde_json::from_value(t.clone()).ok());

    let gateway_cfg = GatewayConfig {
        base_url: config.base_url.clone(),
        api_key: config.api_key.clone(),
    };

    rt.block_on(async {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<StreamEvent>(64);

        let msgs = messages_val.clone();
        let model_o = model.to_string();
        let sys = system;
        let gw = gateway_cfg;
        let effort_o = effort.clone();
        let proto = config.protocol.clone();
        tokio::spawn(async move {
            let result = if proto == "anthropic" {
                // Map DeepSeek effort levels to Anthropic thinking budgets
                let thinking_budget = effort_o.as_deref().and_then(|e| match e {
                    "low" => Some(2048u32),
                    "medium" => Some(4096),
                    "high" => Some(8192),
                    _ => None,
                });
                dsx_gateway::chat_stream_anthropic(
                    &gw, &model_o, sys, msgs, tools, max_tokens, thinking_budget, tx,
                )
                .await
            } else {
                dsx_gateway::chat_stream_openai(
                    &gw, &model_o, sys, msgs, tools, None, max_tokens, effort_o.as_deref(), tx,
                )
                .await
            };
            if let Err(e) = result {
                eprintln!("dsx-hp: gateway error: {e:?}");
            }
        });

        let mut full_content = String::new();
        let mut reasoning = String::new();
        let mut last_delta: String = String::new();
        let mut repeat_count: u32 = 0;
        while let Some(event) = rx.recv().await {
            match event {
                StreamEvent::ContentDelta(delta) => {
                    // Repetition guard: if the same delta repeats many times, cut off
                    if delta == last_delta {
                        repeat_count += 1;
                        if repeat_count > 20 {
                            log::warn!("hp: repetition detected ({}x same delta), cutting off", repeat_count);
                            let _ = writeln!(writer, "{}", serde_json::json!({
                                "type": "api_response",
                                "content": full_content,
                                "stop_reason": "repetition",
                            }));
                            let _ = writer.flush();
                            return;
                        }
                    } else {
                        repeat_count = 0;
                    }
                    last_delta = delta.clone();

                    full_content.push_str(&delta);
                    // Also check overall content for degenerate patterns
                    if full_content.chars().count() > 100 {
                        let tail = &full_content[full_content.char_indices().map(|(i,_)| i).nth(full_content.chars().count().saturating_sub(100)).unwrap_or(0)..];
                        // If the last 100 chars have >60% same character, it's degenerate
                        if let Some(most_common) = tail.chars().max_by_key(|c| tail.matches(*c).count()) {
                            let ratio = tail.matches(most_common).count() as f64 / tail.len() as f64;
                            if ratio > 0.60 && most_common != ' ' {
                                log::warn!("hp: degenerate output detected ({:.0}% '{:?}'), cutting off", ratio * 100.0, most_common);
                                let _ = writeln!(writer, "{}", serde_json::json!({
                                    "type": "api_response",
                                    "content": full_content,
                                    "stop_reason": "degenerate",
                                }));
                                let _ = writer.flush();
                                return;
                            }
                        }
                    }

                    let frame = serde_json::json!({
                        "type": "content_delta",
                        "delta": delta,
                    });
                    if let Ok(s) = serde_json::to_string(&frame) {
                        let _ = writeln!(writer, "{s}");
                        let _ = writer.flush();
                    }
                }
                StreamEvent::ReasoningDelta(delta) => {
                    reasoning.push_str(&delta);
                    let frame = serde_json::json!({
                        "type": "content_delta",
                        "delta": "",
                        "reasoning": delta,
                    });
                    if let Ok(s) = serde_json::to_string(&frame) {
                        let _ = writeln!(writer, "{s}");
                        let _ = writer.flush();
                    }
                }
                StreamEvent::ToolCallProgress { ref name, ref args_so_far } => {
                    let frame = serde_json::json!({
                        "type": "tool_progress",
                        "id": name,
                        "content": args_so_far,
                        "stream_type": "progress",
                    });
                    if let Ok(s) = serde_json::to_string(&frame) {
                        let _ = writeln!(writer, "{s}");
                        let _ = writer.flush();
                    }
                }
                StreamEvent::Done { raw_message, stop_reason: sr, usage } => {
                    if let Some(text) = raw_message.content {
                        full_content = text;
                    }
                    let mut tool_calls: Vec<serde_json::Value> = Vec::new();
                    if let Some(tcs) = raw_message.tool_calls {
                        for tc in tcs {
                            tool_calls.push(serde_json::json!({
                                "id": tc.id,
                                "name": tc.function.name,
                                "arguments": tc.function.arguments,
                            }));
                        }
                    }
                    let mut resp = serde_json::json!({
                        "type": "api_response",
                        "content": full_content,
                    });
                    if !reasoning.is_empty() {
                        resp["reasoning_content"] = serde_json::json!(reasoning);
                    }
                    if let Some(ref sig) = raw_message.thinking_signature {
                        resp["thinking_signature"] = serde_json::json!(sig);
                    }
                    if !tool_calls.is_empty() {
                        resp["tool_calls"] = serde_json::Value::Array(tool_calls);
                    }
                    if let Some(ref s) = sr {
                        resp["stop_reason"] = serde_json::json!(s);
                    }
                    if let Some(ref u) = usage {
                        resp["usage"] = serde_json::json!({
                            "prompt_tokens": u.prompt_tokens,
                            "completion_tokens": u.completion_tokens,
                            "total_tokens": u.total_tokens,
                            "prompt_cache_hit_tokens": u.prompt_cache_hit_tokens,
                            "prompt_cache_miss_tokens": u.prompt_cache_miss_tokens,
                        });
                    }
                    if let Ok(s) = serde_json::to_string(&resp) {
                        let _ = writeln!(writer, "{s}");
                        let _ = writer.flush();
                    }
                    return;
                }
                StreamEvent::Error(e) => {
                    log::error!("hp: API error — {}", e);
                    let _ = writeln!(writer, "{}", json_response("error", &e));
                    let _ = writer.flush();
                    return;
                }
            }
        }
    });
}

/// Extract `pid` field from a JSON-LP line (simple parser, no serde).
fn extract_pid(line: &str) -> Option<u32> {
    let after = line.split("\"pid\"").nth(1)?;
    let after_colon = after.split(':').nth(1)?;
    let trimmed = after_colon.trim_start();
    let digits: String = trimmed.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// Extract `name` field from a JSON-LP line (simple parser).
fn extract_name(line: &str) -> Option<&str> {
    let after = line.split("\"name\"").nth(1)?;
    let after_colon = after.split(':').nth(1)?;
    let trimmed = after_colon.trim();
    let start = trimmed.find('"')? + 1;
    let end = trimmed[start..].find('"')?;
    Some(&trimmed[start..start + end])
}

// ── Port file management ──

fn port_file_path() -> std::path::PathBuf {
    dsx_types::platform::hp_port_path()
}

fn write_port_file(port: u16) {
    let path = port_file_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, port.to_string());
}

fn clear_port_file() {
    let _ = std::fs::write(port_file_path(), "");
}
