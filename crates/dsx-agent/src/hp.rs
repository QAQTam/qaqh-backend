use std::net::TcpStream;
use std::time::Duration;

use dsx_proto::{self, AgentToHp};

/// Read HP port from port file.
fn hp_port() -> u16 {
    let path = dsx_types::platform::hp_port_path();
    std::fs::read_to_string(&path).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0)
}

/// Connect to HP, register, and return the TCP stream.
pub fn connect() -> Option<TcpStream> {
    let port = hp_port();
    if port == 0 {
        eprintln!("dsx-agent: HP not running (no hp.port) — continuing without AI");
        return None;
    }

    let mut stream = match TcpStream::connect(format!("127.0.0.1:{port}")) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("dsx-agent: cannot connect HP: {e}");
            return None;
        }
    };

    let pid = std::process::id();
    let reg = AgentToHp::Register {
        kind: "Agent".into(),
        name: "dsx-agent".into(),
        pid,
    };
    let _ = dsx_proto::write_frame(&mut stream, &reg);

    eprintln!("dsx-agent: connected to HP on port {port}");
    Some(stream)
}

/// Try to (re)connect to HP daemon. If port file is stale, spawn a new HP.
pub fn try_reconnect() -> Option<TcpStream> {
    let port_path = dsx_types::platform::hp_port_path();

    let port = std::fs::read_to_string(&port_path).ok()
        .and_then(|s| s.trim().parse::<u16>().ok());

    if let Some(p) = port {
        if let Ok(stream) = TcpStream::connect(format!("127.0.0.1:{p}")) {
            return Some(stream);
        }
        let _ = std::fs::write(&port_path, "");
    }

    for dsx_path in &[
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent().and_then(|p| p.parent())
            .map(|p| p.join("target").join("release").join("dsx"))
            .unwrap_or_default(),
        std::env::current_exe().ok()
            .and_then(|e| e.parent().map(|d| d.join("dsx")))
            .unwrap_or_default(),
    ] {
        if !dsx_path.exists() { continue; }
        if let Ok(mut child) = std::process::Command::new(dsx_path).arg("hp")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            for _ in 0..10 {
                std::thread::sleep(Duration::from_millis(500));
                if let Ok(s) = std::fs::read_to_string(&port_path) {
                    if let Ok(p) = s.trim().parse::<u16>() {
                        if let Ok(stream) = TcpStream::connect(format!("127.0.0.1:{p}")) {
                            return Some(stream);
                        }
                    }
                }
                if let Ok(Some(_)) = child.try_wait() { break; }
            }
        }
    }
    None
}
