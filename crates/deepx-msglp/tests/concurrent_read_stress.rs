//! Stress test: 10 parallel file read tool calls on the same file.
//! Designed to trigger any deadlock, panic, or lock poisoning in the
//! multi-tool parallel execution path.

use std::io::{BufRead, BufReader};
use std::sync::mpsc;
use std::time::Duration;

use deepx_msglp::state::agent::AgentState;
use deepx_msglp::ring::loop_core::Loop;
use deepx_proto::{Agent2Ui, Ui2Agent};

#[test]
fn ten_parallel_reads_same_file() {
    // ── Setup workspace with a small test file ──
    let tmp = tempfile::tempdir().unwrap();
    let file_path = tmp.path().join("test.txt");
    std::fs::write(&file_path, "0123456789").unwrap();
    deepx_tools::set_workspace(&tmp.path().to_string_lossy());

    // ── Init agent ──
    deepx_session::SessionManager::init(deepx_types::platform::data_dir(), false);
    let mut agent = AgentState::init("test");
    // Make the session ephemeral to avoid disk I/O interference
    agent.ephemeral = true;

    // ── Create IPC loop with pipe channels ──
    let (event_tx_from_agent, event_rx_to_test) = mpsc::channel::<Agent2Ui>();

    // We need to create a Loop, but Loop::new_ipc uses stdin/stdout.
    // Instead, we construct the loop manually via the same channels.
    // Use a pipe pair for input/output.
    let (input_reader, mut input_writer) = os_pipe::pipe().unwrap();
    let (output_reader, output_writer) = os_pipe::pipe().unwrap();

    let mut loop_ = Loop::new_ipc(agent, BufReader::new(input_reader), output_writer);

    // ── Spawn a thread that feeds commands and collects events ──
    let event_rx = event_rx_to_test;
    let event_tx = event_tx_from_agent;

    // Background thread: forward agent output pipe → event channel
    std::thread::spawn(move || {
        let reader = BufReader::new(output_reader);
        for line in reader.lines() {
            match line {
                Ok(line) => match serde_json::from_str::<Agent2Ui>(&line) {
                    Ok(event) => {
                        if event_tx.send(event).is_err() {
                            break;
                        }
                    }
                    Err(_) => {}
                },
                Err(_) => break,
            }
        }
    });

    let handle = std::thread::spawn(move || {
        // Feed CreateSession first
        use std::io::Write;
        let create = Ui2Agent::CreateSession;
        let json = serde_json::to_string(&create).unwrap();
        writeln!(input_writer, "{}", json).unwrap();
        input_writer.flush().unwrap();

        // Wait for Ready then SessionCreated
        let mut ready = false;
        let mut seed = String::new();
        loop {
            match event_rx.recv_timeout(Duration::from_secs(5)) {
                Ok(Agent2Ui::Ready) => ready = true,
                Ok(Agent2Ui::SessionCreated { seed: s }) => seed = s,
                Ok(_) => {}
                Err(_) => break,
            }
            if ready && !seed.is_empty() {
                break;
            }
        }
        assert!(!seed.is_empty(), "SessionCreated not received");

        // Now send a UserInput that triggers 10 parallel file reads.
        // We send a single ToolCall frame for each read (simulating
        // what the agent does after parsing LLM output).
        // Actually, we simulate the agent's internal flow: send a
        // UserInput that the gate would normally process. But we
        // bypass the gate and directly inject tool calls.

        // Send 10 ToolCall frames with incrementing IDs
        for i in 0..10 {
            let tc = Ui2Agent::ToolCall {
                id: format!("tc_{}", i),
                name: "read".into(),
                action: String::new(),
                args: serde_json::json!({
                    "path": file_path.to_string_lossy(),
                }),
            };
            let json = serde_json::to_string(&tc).unwrap();
            writeln!(input_writer, "{}", json).unwrap();
            input_writer.flush().unwrap();
        }

        // Drain events and check for errors
        let mut error_count = 0;
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match event_rx.recv_timeout(remaining) {
                Ok(Agent2Ui::Error { message }) => {
                    eprintln!("Error event: {}", message);
                    error_count += 1;
                }
                Ok(Agent2Ui::Done) => break,
                Ok(_) => {}
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }

        error_count
    });

    // Run the agent loop in this thread
    loop_.run();

    let error_count = handle.join().unwrap();
    assert_eq!(error_count, 0, "Agent emitted {} error events", error_count);
}
