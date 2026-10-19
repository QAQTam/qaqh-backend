use std::fs;

use qaqh_import::{
    CaptureOptions, CaptureRequest, DiscoverRequest, ImportStatus, PrepareRequest, capture,
    discover, prepare,
};

#[test]
fn known_content_branches_and_tool_pairings_are_normalized_with_coverage() {
    let temp = tempfile::tempdir().expect("tempdir");
    let source_root = temp.path().join("source");
    fs::create_dir_all(&source_root).expect("source root");
    let transcript = source_root.join("transcript.jsonl");
    fs::copy(fixture("transcript.jsonl"), &transcript).expect("copy fixture");
    let discovery = discover(DiscoverRequest {
        input: transcript,
        allowed_roots: vec![source_root],
    })
    .expect("discover");
    let captured = capture(CaptureRequest {
        discovery,
        package_dir: temp.path().join("package"),
        options: CaptureOptions {
            chunk_size: 17,
            max_total_bytes: None,
            max_file_bytes: None,
        },
    })
    .expect("capture");
    let prepared = prepare(PrepareRequest { captured }).expect("prepare");

    assert_eq!(prepared.status, ImportStatus::Incomplete);
    assert_eq!(prepared.messages.len(), 5);
    assert!(
        prepared.messages[0]
            .blocks
            .iter()
            .any(|block| block.kind == qaqh_import::ImportedBlockKind::Image)
    );
    let thinking = prepared.messages[1]
        .blocks
        .iter()
        .find(|block| block.kind == qaqh_import::ImportedBlockKind::Thinking)
        .expect("thinking block");
    assert_eq!(thinking.text.as_deref(), Some("Need inspect the image."));
    assert_eq!(thinking.signature.as_deref(), Some("sig-test"));
    let tool = prepared
        .graph
        .tool_pairs
        .iter()
        .find(|pair| pair.tool_id == "tool-1")
        .expect("tool pair");
    assert_eq!(tool.result_record_ids.len(), 1);
    assert!(
        prepared
            .graph
            .assistant_groups
            .iter()
            .any(
                |group| group.source_message_id == "assistant-msg-1" && group.record_ids.len() == 2
            )
    );
    assert!(
        prepared
            .graph
            .nodes
            .iter()
            .any(|node| node.is_sidechain && node.participant.as_deref() == Some("agent-7"))
    );
    assert!(
        prepared
            .graph
            .nodes
            .iter()
            .any(|node| node.is_progress && node.physical_parent.is_some())
    );
    assert_eq!(
        prepared.graph.main_branch_leaf.as_deref(),
        Some(prepared.messages[3].record_id.as_str())
    );
    assert!(
        prepared
            .records
            .iter()
            .any(|record| record.raw_type.as_deref() == Some("summary"))
    );
    assert_eq!(
        prepared.metadata.user_title.as_deref(),
        Some("User title stays primary")
    );
    assert_eq!(
        prepared.metadata.ai_title.as_deref(),
        Some("Suggested AI title")
    );
    assert_eq!(
        prepared.metadata.display_title.as_deref(),
        Some("User title stays primary")
    );
    assert_eq!(
        prepared.metadata.last_question.as_deref(),
        Some("Please inspect this result.")
    );
    let tool_result = prepared.messages[2]
        .blocks
        .iter()
        .find(|block| block.kind == qaqh_import::ImportedBlockKind::ToolResult)
        .expect("tool result");
    assert_eq!(tool_result.children.len(), 1);
    assert_eq!(
        tool_result.children[0].text.as_deref(),
        Some("line one\nline two")
    );
    assert!(
        prepared
            .records
            .iter()
            .any(|record| record.raw_type.as_deref() == Some("progress"))
    );
    assert!(
        prepared
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "unknown_content_block")
    );
    assert_eq!(
        prepared.completeness.context_complete,
        qaqh_import::CompletenessState::Unknown
    );
}

#[test]
fn malformed_lines_are_located_and_still_present_in_the_raw_artifact() {
    let temp = tempfile::tempdir().expect("tempdir");
    let source_root = temp.path().join("source");
    fs::create_dir_all(&source_root).expect("source root");
    let transcript = source_root.join("transcript.jsonl");
    fs::write(
        &transcript,
        b"{\"type\":\"unknown-future\",\"uuid\":\"x\"}\nnot json\n",
    )
    .expect("transcript");
    let discovery = discover(DiscoverRequest {
        input: transcript,
        allowed_roots: vec![source_root],
    })
    .expect("discover");
    let captured = capture(CaptureRequest {
        discovery,
        package_dir: temp.path().join("package"),
        options: CaptureOptions::default(),
    })
    .expect("capture");
    let prepared = prepare(PrepareRequest { captured }).expect("prepare");
    assert_eq!(prepared.records.len(), 2);
    assert_eq!(
        prepared.records[0].disposition,
        qaqh_import::RecordDisposition::Opaque
    );
    assert_eq!(
        prepared.records[1].disposition,
        qaqh_import::RecordDisposition::Damaged
    );
    assert_eq!(prepared.records[1].origin.byte_offset, 37);
    assert!(
        prepared
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "malformed_jsonl_record"
                && diagnostic.byte_offset == Some(37))
    );
    assert!(!prepared.completeness.messages_complete);
}

#[test]
fn long_jsonl_records_are_spooled_and_never_truncated() {
    let temp = tempfile::tempdir().expect("tempdir");
    let source = temp.path().join("source");
    fs::create_dir_all(&source).expect("source");
    let transcript = source.join("large.jsonl");
    let text = "x".repeat(2 * 1024 * 1024);
    let record = serde_json::json!({
        "type": "user",
        "uuid": "large-user",
        "message": {"id": "large-message", "role": "user", "content": text}
    });
    fs::write(
        &transcript,
        serde_json::to_vec(&record).expect("serialize record"),
    )
    .expect("write transcript");
    let captured = capture(CaptureRequest {
        discovery: discover(DiscoverRequest {
            input: transcript,
            allowed_roots: vec![source],
        })
        .expect("discover"),
        package_dir: temp.path().join("package"),
        options: CaptureOptions {
            chunk_size: 64 * 1024,
            max_total_bytes: None,
            max_file_bytes: None,
        },
    })
    .expect("capture");
    let prepared = prepare(PrepareRequest { captured }).expect("prepare");
    assert_eq!(
        prepared.messages[0].blocks[0]
            .text
            .as_ref()
            .map(String::len),
        Some(2 * 1024 * 1024)
    );
}

#[test]
fn duplicate_uuids_are_kept_as_conflicts_and_parent_choice_is_reported() {
    let temp = tempfile::tempdir().expect("tempdir");
    let source = temp.path().join("source");
    fs::create_dir_all(&source).expect("source");
    let transcript = source.join("duplicates.jsonl");
    fs::write(
        &transcript,
        concat!(
            "{\"type\":\"user\",\"uuid\":\"same\",\"message\":{\"id\":\"one\",\"role\":\"user\",\"content\":\"first\"}}\n",
            "{\"type\":\"user\",\"uuid\":\"same\",\"message\":{\"id\":\"two\",\"role\":\"user\",\"content\":\"second\"}}\n",
            "{\"type\":\"assistant\",\"uuid\":\"child\",\"parentUuid\":\"same\",\"message\":{\"id\":\"three\",\"role\":\"assistant\",\"content\":\"reply\"}}\n"
        ),
    )
    .expect("transcript");
    let captured = capture(CaptureRequest {
        discovery: discover(DiscoverRequest {
            input: transcript,
            allowed_roots: vec![source],
        })
        .expect("discover"),
        package_dir: temp.path().join("package"),
        options: CaptureOptions::default(),
    })
    .expect("capture");
    let prepared = prepare(PrepareRequest { captured }).expect("prepare");
    assert_eq!(
        prepared.records[0].disposition,
        qaqh_import::RecordDisposition::Conflict
    );
    assert_eq!(
        prepared.records[1].disposition,
        qaqh_import::RecordDisposition::Conflict
    );
    assert!(
        prepared
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "duplicate_parent_uuid")
    );
    let child = prepared
        .graph
        .nodes
        .iter()
        .find(|node| node.record_id == prepared.messages[2].record_id)
        .expect("child node");
    assert_eq!(
        child.physical_parent.as_deref(),
        Some(prepared.messages[1].record_id.as_str())
    );
    assert!(!prepared.completeness.messages_complete);
}

#[test]
fn parent_cycles_are_reported_without_dropping_physical_records() {
    let temp = tempfile::tempdir().expect("tempdir");
    let source = temp.path().join("source");
    fs::create_dir_all(&source).expect("source");
    let transcript = source.join("cycle.jsonl");
    fs::write(
        &transcript,
        concat!(
            "{\"type\":\"assistant\",\"uuid\":\"a\",\"parentUuid\":\"b\",\"message\":{\"id\":\"a\",\"role\":\"assistant\",\"content\":\"A\"}}\n",
            "{\"type\":\"assistant\",\"uuid\":\"b\",\"parentUuid\":\"a\",\"message\":{\"id\":\"b\",\"role\":\"assistant\",\"content\":\"B\"}}\n"
        ),
    )
    .expect("transcript");
    let captured = capture(CaptureRequest {
        discovery: discover(DiscoverRequest {
            input: transcript,
            allowed_roots: vec![source],
        })
        .expect("discover"),
        package_dir: temp.path().join("package"),
        options: CaptureOptions::default(),
    })
    .expect("capture");
    let prepared = prepare(PrepareRequest { captured }).expect("prepare");
    assert_eq!(prepared.records.len(), 2);
    assert!(
        prepared
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "parent_cycle")
    );
    assert!(!prepared.completeness.messages_complete);
}

fn fixture(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/claude_code")
        .join(name)
}
