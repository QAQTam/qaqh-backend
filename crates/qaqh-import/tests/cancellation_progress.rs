use std::{
    fs,
    sync::{Arc, Mutex},
};

use qaqh_import::{
    CaptureOptions, CaptureRequest, DiscoverRequest, ImportHost, ImportPhase, ImportProgress,
    OperationContext, PrepareRequest, capture_with_context, discover_with_context,
    prepare_with_context,
};

struct CancelNow;

impl ImportHost for CancelNow {
    fn is_cancelled(&self) -> bool {
        true
    }
}

#[derive(Default)]
struct ProgressRecorder(Mutex<Vec<ImportProgress>>);

impl ImportHost for ProgressRecorder {
    fn on_progress(&self, progress: ImportProgress) {
        self.0.lock().expect("progress lock").push(progress);
    }
}

#[test]
fn host_cancellation_stops_discovery_and_progress_is_reported_by_phase() {
    let temp = tempfile::tempdir().expect("tempdir");
    let source = temp.path().join("source");
    fs::create_dir_all(&source).expect("source");
    let transcript = source.join("session.jsonl");
    fs::write(
        &transcript,
        b"{\"type\":\"user\",\"uuid\":\"u1\",\"message\":{\"id\":\"m1\",\"role\":\"user\",\"content\":\"Question\"}}\n",
    )
    .expect("transcript");
    let cancelled = OperationContext::with_host(Arc::new(CancelNow));
    let result = discover_with_context(
        DiscoverRequest {
            input: transcript.clone(),
            allowed_roots: vec![source.clone()],
        },
        &cancelled,
    );
    assert!(matches!(result, Err(qaqh_import::ImportError::Cancelled)));

    let recorder = Arc::new(ProgressRecorder::default());
    let context = OperationContext::with_host(recorder.clone());
    let discovery = discover_with_context(
        DiscoverRequest {
            input: transcript,
            allowed_roots: vec![source],
        },
        &context,
    )
    .expect("discover");
    let captured = capture_with_context(
        CaptureRequest {
            discovery,
            package_dir: temp.path().join("package"),
            options: CaptureOptions::default(),
        },
        &context,
    )
    .expect("capture");
    prepare_with_context(PrepareRequest { captured }, &context).expect("prepare");
    let phases = recorder
        .0
        .lock()
        .expect("progress lock")
        .iter()
        .map(|progress| progress.phase)
        .collect::<Vec<_>>();
    assert!(phases.contains(&ImportPhase::Discover));
    assert!(phases.contains(&ImportPhase::Capture));
    assert!(phases.contains(&ImportPhase::Normalize));
    assert!(phases.contains(&ImportPhase::Verify));
}
