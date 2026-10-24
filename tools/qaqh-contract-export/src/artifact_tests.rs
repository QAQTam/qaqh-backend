use super::*;

#[test]
fn generated_artifacts_are_deterministic_and_import_closed() {
    let root = tempfile::tempdir().unwrap();
    let first = root.path().join("first");
    let second = root.path().join("second");
    fs::create_dir(&first).unwrap();
    fs::create_dir(&second).unwrap();
    generate(&first).unwrap();
    generate(&second).unwrap();
    validate_layout(&first).unwrap();
    validate_layout(&second).unwrap();
    compare_trees(&first, &second).unwrap();
    let public = fs::read_to_string(first.join("index.ts")).unwrap();
    assert!(!public.contains("DaemonDiscovery"));
    assert!(
        fs::read_to_string(first.join("node.ts"))
            .unwrap()
            .contains("DaemonDiscovery")
    );
    for name in [
        "RingingV2PairRequest",
        "RingingV2DevicesResponse",
        "RingingV2CommandStatus",
    ] {
        assert!(public.contains(name), "missing public root {name}");
    }
    fs::remove_file(second.join("fixtures/open-response.json")).unwrap();
    assert!(compare_trees(&first, &second).is_err());
}

#[test]
fn manifest_validation_rejects_tampering_and_import_gaps() {
    let root = tempfile::tempdir().unwrap();
    generate(root.path()).unwrap();
    validation::validate(root.path()).unwrap();
    fs::write(root.path().join("constants.ts"), "corrupt").unwrap();
    assert!(validation::validate(root.path()).is_err());
    generate(root.path()).unwrap();
    fs::write(
        root.path().join("types/qaqh/Gap.ts"),
        "import type { Missing } from \"./Missing\";\n",
    )
    .unwrap();
    write_manifest(root.path()).unwrap();
    assert!(
        validation::validate(root.path())
            .unwrap_err()
            .to_string()
            .contains("unresolved TypeScript import")
    );
}

#[test]
fn export_preserves_non_exporter_directories() {
    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("user-data");
    fs::create_dir(&target).unwrap();
    fs::write(target.join("keep.txt"), "do not delete").unwrap();
    assert!(export(&target, false).is_err());
    assert_eq!(
        fs::read_to_string(target.join("keep.txt")).unwrap(),
        "do not delete"
    );
}

#[cfg(unix)]
#[test]
fn artifact_file_walk_rejects_symlinks_instead_of_following_them() {
    let root = tempfile::tempdir().unwrap();
    let external = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(external.path(), root.path().join("outside")).unwrap();
    assert!(list_files(root.path()).is_err());
}

#[test]
fn wire_decoders_reject_invalid_discriminants_and_unsafe_cursors() {
    assert!(
        serde_json::from_value::<RingingV2CommandStatus>(serde_json::json!({
            "command_id":"synthetic", "state":"finished", "payload_fingerprint":"synthetic"
        }))
        .is_err()
    );
    assert!(
        serde_json::from_value::<RingingV2CommandAck>(serde_json::json!({
            "status":"accepted"
        }))
        .is_err()
    );
    assert!(
        qaqh_ringing::CanonicalCursor::new("synthetic", qaqh_ringing::MAX_SAFE_INTEGER + 1, 0)
            .validate_reliable()
            .is_err()
    );
    let mut open = RingingV2OpenRequest::new("synthetic");
    open.version = 99;
    assert!(open.validate().is_err());
    let invalid = serde_json::json!({"schema":"qaqh.Ringing","version":2,
        "channel":"tool","command_id":"synthetic","client_instance_id":"synthetic",
        "client_session_id":"synthetic","session_id":"session-fixture",
        "command":{"channel":"control","type":"session_attach","session_id":"session-fixture"}});
    let command: RingingV2CommandEnvelope = serde_json::from_value(invalid).unwrap();
    assert!(command.validate().is_err());
}
