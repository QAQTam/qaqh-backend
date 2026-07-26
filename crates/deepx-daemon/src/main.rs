mod server;

use std::io::{Read, Write};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("agent") => {
            deepx_runtime::cache_system_path();
            deepx_runtime::detect_os_info();
            if let Err(error) = deepx_runtime::run_agent_worker(&args[1..]) {
                eprintln!("agent failed: {error}");
                std::process::exit(1);
            }
        }
        Some("status") => status(),
        Some("stop") => stop(),
        Some("run") | None => {
            // Preserve the complete interactive PATH for workers, but defer
            // prompt-only OS/tool probing to each worker process.
            deepx_runtime::cache_system_path();
            let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
            if let Err(error) = runtime.block_on(server::run()) {
                eprintln!("deepx-daemon: {error}");
                std::process::exit(1);
            }
        }
        Some(command) => {
            eprintln!("unknown command: {command}; expected run, status, stop, or agent");
            std::process::exit(2);
        }
    }
}

fn status() {
    match read_discovery() {
        Ok(discovery) if discovery_reachable(&discovery) => println!(
            "running pid={} endpoint={}",
            discovery.pid, discovery.endpoint
        ),
        Ok(_) => {
            println!("stopped (stale discovery record)");
            std::process::exit(1);
        }
        Err(error) => {
            println!("stopped ({error})");
            std::process::exit(1);
        }
    }
}

fn discovery_reachable(discovery: &deepx_proto::DaemonDiscovery) -> bool {
    if !deepx_types::platform::process_is_running(discovery.pid) {
        return false;
    }
    let address = discovery
        .endpoint
        .trim_start_matches("ws://")
        .split('/')
        .next()
        .unwrap_or_default();
    address.parse().ok().is_some_and(|address| {
        std::net::TcpStream::connect_timeout(&address, std::time::Duration::from_millis(300))
            .is_ok()
    })
}

fn stop() {
    let discovery = match read_discovery() {
        Ok(value) => value,
        Err(error) => {
            eprintln!("daemon is not running: {error}");
            return;
        }
    };
    let endpoint = discovery.endpoint.trim_start_matches("ws://");
    let address = endpoint.split('/').next().unwrap_or(endpoint);
    let Ok(socket_address) = address.parse() else {
        eprintln!("invalid daemon address");
        return;
    };
    match std::net::TcpStream::connect_timeout(&socket_address, std::time::Duration::from_secs(2)) {
        Ok(mut stream) => {
            let request = format!(
                "POST /control/v1/stop HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                discovery.token
            );
            if stream.write_all(request.as_bytes()).is_ok() {
                let mut response = String::new();
                let _ = stream.read_to_string(&mut response);
                if response.starts_with("HTTP/1.1 200") {
                    println!("daemon stopping");
                    return;
                }
            }
            eprintln!("daemon rejected stop request");
        }
        Err(error) => eprintln!("cannot connect to daemon: {error}"),
    }
}

fn read_discovery() -> Result<deepx_proto::DaemonDiscovery, String> {
    let content = std::fs::read_to_string(deepx_types::platform::daemon_discovery_path())
        .map_err(|e| e.to_string())?;
    serde_json::from_str(&content).map_err(|e| e.to_string())
}
