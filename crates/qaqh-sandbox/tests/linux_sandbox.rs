#![cfg(target_os = "linux")]

use std::io::Write;
use std::process::{Command, Stdio};

use qaqh_sandbox::{NetworkPolicy, SandboxBackend, SandboxRequest, SandboxSpec};

fn run_helper(request: &SandboxRequest) -> std::process::Output {
    let helper = env!("CARGO_BIN_EXE_qaqh-sandbox-exec");
    let mut child = Command::new(helper)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn sandbox helper");
    let payload = serde_json::to_vec(request).expect("serialize request");
    child
        .stdin
        .take()
        .expect("helper stdin")
        .write_all(&payload)
        .expect("write helper request");
    child.wait_with_output().expect("wait helper")
}

fn spec(workspace: &std::path::Path) -> SandboxSpec {
    SandboxSpec {
        enabled: true,
        backend: SandboxBackend::LinuxLandlockSeccomp,
        writable_roots: vec![workspace.to_path_buf()],
        network: NetworkPolicy::Deny,
        max_open_files: Some(1024),
    }
}

#[test]
fn workspace_write_is_allowed_and_outside_write_is_denied() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let inside = workspace.join("inside.txt");
    let outside = tmp.path().join("outside.txt");
    let request = SandboxRequest {
        spec: spec(&workspace),
        argv: vec![
            "sh".into(),
            "-c".into(),
            "printf inside > \"$1\"; printf outside > \"$2\"".into(),
            "qaqh-sandbox-test".into(),
            inside.to_string_lossy().into_owned(),
            outside.to_string_lossy().into_owned(),
        ],
    };

    let output = run_helper(&request);
    assert!(
        inside.is_file(),
        "workspace write should succeed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !outside.exists(),
        "outside workspace write must be denied: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !output.status.success(),
        "shell should report the denied redirection"
    );
}

#[test]
fn network_denial_blocks_tcp_socket_creation() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let helper = env!("CARGO_BIN_EXE_qaqh-sandbox-exec");
    let request = SandboxRequest {
        spec: spec(&workspace),
        argv: vec![helper.into(), "--probe-network".into()],
    };

    let output = run_helper(&request);
    assert!(
        !output.status.success(),
        "network probe must fail under sandbox: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("network denied"),
        "network probe must fail at socket creation: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
