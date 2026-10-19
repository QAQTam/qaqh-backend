use std::fs;

use qaqh_import::{
    CaptureOptions, CaptureRequest, DiscoverRequest, PrepareRequest, capture, discover,
    open_package, prepare, verify,
};

#[test]
fn package_identity_is_stable_across_capture_chunking_and_reopen() {
    let temp = tempfile::tempdir().expect("tempdir");
    let source = temp.path().join("source");
    fs::create_dir_all(&source).expect("source");
    let transcript = source.join("session.jsonl");
    fs::write(
        &transcript,
        concat!(
            "{\"type\":\"user\",\"uuid\":\"u1\",\"message\":{\"id\":\"m1\",\"role\":\"user\",\"content\":\"Question\"}}\n",
            "{\"type\":\"assistant\",\"uuid\":\"a1\",\"parentUuid\":\"u1\",\"message\":{\"id\":\"m2\",\"role\":\"assistant\",\"content\":\"Answer\"}}\n"
        ),
    )
    .expect("transcript");

    let first = capture(CaptureRequest {
        discovery: discover(DiscoverRequest {
            input: transcript.clone(),
            allowed_roots: vec![source.clone()],
        })
        .expect("discover one"),
        package_dir: temp.path().join("package-one"),
        options: CaptureOptions {
            chunk_size: 7,
            max_total_bytes: None,
            max_file_bytes: None,
        },
    })
    .expect("capture one");
    let second = capture(CaptureRequest {
        discovery: discover(DiscoverRequest {
            input: transcript,
            allowed_roots: vec![source],
        })
        .expect("discover two"),
        package_dir: temp.path().join("package-two"),
        options: CaptureOptions::default(),
    })
    .expect("capture two");
    assert_eq!(first.manifest.package_hash, second.manifest.package_hash);
    assert!(!first.manifest.artifacts[0].capture.file_identity.is_empty());
    assert_eq!(
        first.manifest.artifacts[0].capture.second_pass_sha256,
        first.manifest.artifacts[0].sha256
    );

    let prepared = prepare(PrepareRequest {
        captured: first.clone(),
    })
    .expect("prepare");
    assert_eq!(prepared.status, qaqh_import::ImportStatus::Complete);
    let report = verify(&first).expect("verify prepared");
    assert!(report.valid && report.normalization_verified, "{report:?}");
    let reopened = open_package(first.package_dir.clone()).expect("reopen package");
    assert!(
        verify(&reopened)
            .expect("verify reopened")
            .normalization_verified
    );
}

#[test]
fn tampered_chunk_or_normalized_contract_fails_verification() {
    let temp = tempfile::tempdir().expect("tempdir");
    let source = temp.path().join("source");
    fs::create_dir_all(&source).expect("source");
    let transcript = source.join("session.jsonl");
    fs::write(
        &transcript,
        b"{\"type\":\"user\",\"uuid\":\"u1\",\"message\":{\"id\":\"m1\",\"role\":\"user\",\"content\":\"Question\"}}\n",
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
    prepare(PrepareRequest {
        captured: captured.clone(),
    })
    .expect("prepare");
    let prepared_path = captured.package_dir.join("prepared.json");
    let mut prepared: serde_json::Value =
        serde_json::from_slice(&fs::read(&prepared_path).expect("read prepared"))
            .expect("parse prepared");
    prepared["normalization_hash"] = serde_json::Value::String("0".repeat(64));
    fs::write(
        &prepared_path,
        serde_json::to_vec(&prepared).expect("serialize"),
    )
    .expect("tamper normalized");
    let report = verify(&captured).expect("verify tamper");
    assert!(!report.valid);
    assert!(!report.normalization_verified);

    let chunk = captured
        .package_dir
        .join("objects")
        .join(&captured.manifest.artifacts[0].chunks[0].sha256);
    fs::write(&chunk, b"tampered object").expect("tamper chunk");
    let report = verify(&captured).expect("verify chunk");
    assert!(!report.valid);
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.contains("chunk object failed"))
    );
}
