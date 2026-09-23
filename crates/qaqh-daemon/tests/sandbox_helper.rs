#![cfg(target_os = "linux")]

//! Production daemon binary must expose the Linux sandbox helper and enforce
//! the workspace-write boundary before execing the target.

use std::io::Write;
use std::process::{Command, Stdio};

use serde_json::json;

#[test]
fn daemon_helper_denies_write_outside_workspace() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let inside = workspace.join("inside.txt");
    let outside = tmp.path().join("outside.txt");

    let request = json!({
        "spec": {
            "enabled": true,
            "backend": "linux_landlock_seccomp",
            "writable_roots": [workspace],
            "network": "deny",
            "max_open_files": 1024,
        },
        "argv": [
            "sh",
            "-c",
            "printf inside > \"$1\"; printf outside > \"$2\"",
            "qaqh-daemon-sandbox-test",
            inside,
            outside,
        ],
    });

    let mut child = Command::new(env!("CARGO_BIN_EXE_qaqh-daemon"))
        .arg("__qaqh-sandbox-exec")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn daemon helper");
    child
        .stdin
        .take()
        .expect("helper stdin")
        .write_all(&serde_json::to_vec(&request).expect("serialize request"))
        .expect("write request");
    let output = child.wait_with_output().expect("wait helper");

    assert!(
        inside.is_file(),
        "workspace write should succeed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !outside.exists(),
        "outside write must be denied: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !output.status.success(),
        "helper should return the target shell's denied-write status"
    );
}
