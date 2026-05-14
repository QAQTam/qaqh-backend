//! dsx-hp — Health Platform daemon.
//!
//! A lightweight daemon that tracks process liveness, runs the health
//! judgment pipeline, and exposes a `HealthProbe` service over TCP.
//!
//! ## IPC protocol
//!
//! JSON-LP frames over TCP `localhost`. Port written to
//! `$XDG_RUNTIME_DIR/dsx/hp.port`.
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
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use crate::dsx_hp::ipc_traits::HealthProbe;
use crate::dsx_hp::pipeline::judge;
use crate::dsx_hp::registry::ProcessRegistry;
use crate::dsx_hp::types::{HpError, ProcessKind, Verdict};
use crate::dsx_hp::{
    circuit_breaker, emotion, liveness, monitor, pipeline, registry, types,
};

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
        // monitor_state is placeholder — in production the Agent
        // pushes MonitorState updates via IPC frames.
        let monitor_state = monitor::MonitorState::new();
        judge(&self.registry, &monitor_state)
    }

    fn query(&self, pid: u32) -> Result<crate::dsx_hp::types::ProcessHealth, HpError> {
        self.registry.health(pid)
    }

    fn list_processes(&self) -> Vec<crate::dsx_hp::types::ProcessSummary> {
        self.registry.summaries()
    }
}

// ── Main ──

fn main() {
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
    let service = std::sync::Mutex::new(HealthService::new(timeout_secs));

    // 5. Accept loop
    let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>();

    // Register self as the HP process
    {
        let mut svc = service.lock().unwrap();
        svc.register(ProcessKind::Tui, "dsx-hp", std::process::id())
            .ok();
    }

    // Heartbeat ticker — self-heartbeat every 15s
    let svc_heartbeat = service.clone();
    let hb_ticker = thread::spawn(move || loop {
        thread::sleep(Duration::from_secs(15));
        if shutdown_rx.try_recv().is_ok() {
            break;
        }
        if let Ok(mut svc) = svc_heartbeat.lock() {
            svc.heartbeat(std::process::id()).ok();
        }
    });

    // Signal handler (SIGINT/SIGTERM) — write empty port file, then exit
    let svc_shutdown = service.clone();
    let sig_handler = thread::spawn(move || {
        // Read from stdin or use a simpler signal handling mechanism
        // In production, use a signal library or tokio signal.
        // For now, Ctrl+C will kill the process and port cleanup happens
        // via the drop handler below.
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

/// Process a single TCP connection.
///
/// Reads JSON-LP frames from the stream, dispatches to `HealthProbe`,
/// writes JSON-LP response frames.
fn handle_connection(
    mut stream: TcpStream,
    service: std::sync::Mutex<HealthService>,
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
                let response = dispatch_frame(trimmed, &service);
                if let Ok(mut w) = stream.try_clone() {
                    let _ = writeln!(w, "{response}");
                    let _ = w.flush();
                }
            }
            Err(_) => break,
        }
    }

    if let Some(addr) = peer {
        eprintln!("dsx-hp: connection closed: {addr}");
    }
}

/// Parse a JSON-LP frame and dispatch to the health service.
///
/// Frame format (JSON one-line):
/// ```json
/// {"type": "register", "kind": "Agent", "name": "...", "pid": 1234}
/// {"type": "heartbeat", "pid": 1234}
/// {"type": "unregister", "pid": 1234}
/// {"type": "judge"}
/// {"type": "query", "pid": 1234}
/// {"type": "list"}
/// ```
fn dispatch_frame(
    line: &str,
    service: &std::sync::Mutex<HealthService>,
) -> String {
    // Parse the type field from the JSON line
    let frame_type = line
        .split('"')
        .skip(1)
        .next()
        .unwrap_or("")
        .to_string();

    let mut svc = service.lock().unwrap();

    match frame_type.as_str() {
        "register" => {
            // Minimal parsing — production should use serde
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
    format!("{{\"type\":\"{status}\",\"message\":\"{message}\"}}")
}

/// Extract `pid` field from a JSON-LP line (simple parser, no serde).
fn extract_pid(line: &str) -> Option<u32> {
    // Find `"pid":<digits>` or `"pid": <digits>`
    let after = line.split("\"pid\"").nth(1)?;
    let after_colon = after.split(':').nth(1)?;
    let trimmed = after_colon.trim_start();
    let digits: String = trimmed.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// Extract `name` field from a JSON-LP line (simple parser).
fn extract_name(line: &str) -> Option<&str> {
    // Find `"name":"<text>"` or `"name": "<text>"`
    let after = line.split("\"name\"") .nth(1)?;
    let after_colon = after.split(':').nth(1)?;
    let trimmed = after_colon.trim();
    let start = trimmed.find('"')? + 1;
    let end = trimmed[start..].find('"')?;
    Some(&trimmed[start..start + end])
}

// ── Port file management ──

fn port_file_path() -> std::path::PathBuf {
    let base = std::env::var("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("/tmp"));
    base.join("dsx").join("hp.port")
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
