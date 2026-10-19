use std::{fs, path::Path};

use qaqh_import::{
    CaptureOptions, CaptureRequest, DiscoverRequest, PrepareRequest, capture, discover, prepare,
    restore_artifact, verify,
};
use sha2::{Digest, Sha256};

#[test]
fn snapshot_roundtrips_bytes_across_chunk_boundaries() {
    let temp = tempfile::tempdir().expect("tempdir");
    let source_root = temp.path().join("claude-project");
    fs::create_dir_all(&source_root).expect("source root");
    let transcript = source_root.join("session.jsonl");
    let original = b"\xef\xbb\xbf{\"k\":1,\"k\":2}\r\n\xffbroken\n{\"last\":true}";
    fs::write(&transcript, original).expect("write source");

    let discovery = discover(DiscoverRequest {
        input: transcript,
        allowed_roots: vec![source_root],
    })
    .expect("discover");
    let captured = capture(CaptureRequest {
        discovery,
        package_dir: temp.path().join("package"),
        options: CaptureOptions {
            chunk_size: 3,
            max_total_bytes: None,
            max_file_bytes: None,
        },
    })
    .expect("capture");
    let artifact = captured
        .manifest
        .artifacts
        .iter()
        .find(|artifact| artifact.display_path == "session.jsonl")
        .expect("transcript artifact");
    let restored = temp.path().join("restored.jsonl");
    restore_artifact(&captured, &artifact.artifact_id, &restored).expect("restore");
    assert_eq!(fs::read(&restored).expect("restored bytes"), original);
    assert_eq!(sha256_file(&restored), artifact.sha256);
    let report = verify(&captured).expect("verify");
    assert!(report.valid, "{report:?}");
    assert_eq!(report.verified_artifacts, 1);
}

#[test]
fn directory_discovery_freezes_main_and_session_subagent_files() {
    let temp = tempfile::tempdir().expect("tempdir");
    let project = temp.path().join("project");
    fs::create_dir_all(project.join("session-1/subagents/deep")).expect("session dirs");
    fs::write(project.join("session-1.jsonl"), b"{}\n").expect("main");
    fs::write(project.join("session-1/subagents/agent.jsonl"), b"{}\n").expect("agent");
    fs::write(
        project.join("session-1/subagents/deep/metadata.json"),
        b"{}\n",
    )
    .expect("metadata");
    fs::write(project.join("sessions-index.json"), b"[]").expect("index");
    fs::write(project.join("settings.json"), b"{\"secret\":true}").expect("ambient setting");

    let discovery = discover(DiscoverRequest {
        input: project.clone(),
        allowed_roots: vec![project.clone()],
    })
    .expect("discover project");
    let paths = discovery
        .artifacts
        .iter()
        .map(|artifact| artifact.display_path.as_str())
        .collect::<Vec<_>>();
    assert!(paths.contains(&"session-1.jsonl"));
    assert!(paths.contains(&"session-1/subagents/agent.jsonl"));
    assert!(paths.contains(&"session-1/subagents/deep/metadata.json"));
    assert!(paths.contains(&"sessions-index.json"));
    assert!(!paths.contains(&"settings.json"));

    // The selection is frozen: a file added after discovery is not captured.
    fs::write(project.join("session-2.jsonl"), b"late\n").expect("late file");
    let captured = capture(CaptureRequest {
        discovery,
        package_dir: temp.path().join("package"),
        options: CaptureOptions::default(),
    })
    .expect("capture");
    assert!(
        !captured
            .manifest
            .artifacts
            .iter()
            .any(|artifact| artifact.display_path == "session-2.jsonl")
    );
}

#[test]
fn outside_paths_and_symlink_files_are_not_added_to_snapshot() {
    let temp = tempfile::tempdir().expect("tempdir");
    let project = temp.path().join("project");
    let outside = temp.path().join("outside.jsonl");
    fs::create_dir_all(&project).expect("project");
    fs::write(project.join("session.jsonl"), b"{}\n").expect("main");
    fs::write(&outside, b"outside\n").expect("outside");
    let link = project.join("escape.jsonl");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, &link).expect("symlink");
    #[cfg(windows)]
    std::os::windows::fs::symlink_file(&outside, &link).expect("symlink");
    let discovery = discover(DiscoverRequest {
        input: project.clone(),
        allowed_roots: vec![project.clone()],
    })
    .expect("discover");
    assert!(
        !discovery
            .artifacts
            .iter()
            .any(|artifact| artifact.display_path == "escape.jsonl")
    );
    assert!(
        discovery
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "symbolic_link_excluded")
    );
}

#[test]
fn referenced_local_media_is_captured_only_when_inside_an_allowed_root() {
    let temp = tempfile::tempdir().expect("tempdir");
    let project = temp.path().join("project");
    fs::create_dir_all(&project).expect("project");
    let attachment = project.join("attached image.png");
    fs::write(&attachment, b"image-bytes").expect("attachment");
    let transcript = project.join("session.jsonl");
    let record = serde_json::json!({
        "type": "user",
        "uuid": "user-1",
        "message": {
            "id": "message-1",
            "role": "user",
            "content": [{
                "type": "image",
                "source": {"type": "path", "path": attachment.to_string_lossy()}
            }]
        }
    });
    fs::write(&transcript, format!("{}\n", record)).expect("transcript");
    let discovery = discover(DiscoverRequest {
        input: project.clone(),
        allowed_roots: vec![project.clone()],
    })
    .expect("discover");
    assert!(discovery.artifacts.iter().any(|artifact| {
        artifact.kind == qaqh_import::ArtifactKind::Attachment
            && artifact.referenced_paths == vec![attachment.to_string_lossy().into_owned()]
    }));
    let captured = capture(CaptureRequest {
        discovery,
        package_dir: temp.path().join("package"),
        options: CaptureOptions::default(),
    })
    .expect("capture");
    let prepared = prepare(PrepareRequest { captured }).expect("prepare");
    assert!(prepared.completeness.messages_complete);
    assert!(prepared.completeness.missing_items.is_empty());
}

#[test]
fn local_attachment_outside_allowed_roots_is_reported_incomplete() {
    let temp = tempfile::tempdir().expect("tempdir");
    let project = temp.path().join("project");
    fs::create_dir_all(&project).expect("project");
    let outside = temp.path().join("private.png");
    fs::write(&outside, b"private attachment").expect("outside attachment");
    let transcript = project.join("session.jsonl");
    let record = serde_json::json!({
        "type": "user",
        "uuid": "user-1",
        "message": {
            "id": "message-1",
            "role": "user",
            "content": [{
                "type": "image",
                "source": {"type": "path", "path": outside.to_string_lossy()}
            }]
        }
    });
    fs::write(&transcript, format!("{}\n", record)).expect("transcript");
    let captured = capture(CaptureRequest {
        discovery: discover(DiscoverRequest {
            input: project.clone(),
            allowed_roots: vec![project],
        })
        .expect("discover"),
        package_dir: temp.path().join("package"),
        options: CaptureOptions::default(),
    })
    .expect("capture");
    let prepared = prepare(PrepareRequest { captured }).expect("prepare");
    assert_eq!(prepared.status, qaqh_import::ImportStatus::Incomplete);
    assert!(
        prepared
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "unresolved_attachment_reference")
    );
    assert!(
        prepared
            .completeness
            .missing_items
            .iter()
            .any(|item| item.contains("private.png"))
    );
}

#[test]
fn selected_session_metadata_is_extracted_from_project_index() {
    let temp = tempfile::tempdir().expect("tempdir");
    let project = temp.path().join("project");
    fs::create_dir_all(&project).expect("project");
    let transcript = project.join("session-1.jsonl");
    fs::write(
        &transcript,
        b"{\"type\":\"user\",\"uuid\":\"u1\",\"message\":{\"id\":\"m1\",\"role\":\"user\",\"content\":\"First question\"}}\n",
    )
    .expect("transcript");
    fs::write(
        project.join("sessions-index.json"),
        br#"{"sessions":[{"sessionId":"another-session","customTitle":"Wrong title"},{"sessionId":"session-1","customTitle":"Imported title","cwd":"C:/repo","gitBranch":"main","version":"2.4.0","mode":"plan"}]}"#,
    )
    .expect("index");
    let captured = capture(CaptureRequest {
        discovery: discover(DiscoverRequest {
            input: transcript,
            allowed_roots: vec![project],
        })
        .expect("discover"),
        package_dir: temp.path().join("package"),
        options: CaptureOptions::default(),
    })
    .expect("capture");
    let prepared = prepare(PrepareRequest { captured }).expect("prepare");
    assert_eq!(
        prepared.metadata.user_title.as_deref(),
        Some("Imported title")
    );
    assert_eq!(
        prepared.metadata.display_title.as_deref(),
        Some("Imported title")
    );
    assert_eq!(
        prepared.metadata.last_question.as_deref(),
        Some("First question")
    );
    assert_eq!(prepared.metadata.cwd.as_deref(), Some("C:/repo"));
    assert_eq!(prepared.metadata.git_branch.as_deref(), Some("main"));
    assert_eq!(
        prepared.metadata.source_program_version.as_deref(),
        Some("2.4.0")
    );
    assert_eq!(
        prepared
            .metadata
            .mode
            .as_ref()
            .and_then(serde_json::Value::as_str),
        Some("plan")
    );
}

fn sha256_file(path: &Path) -> String {
    let digest = Sha256::digest(fs::read(path).expect("read file"));
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}
