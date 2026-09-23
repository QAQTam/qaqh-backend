//! Legacy writer facade serialization contract.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

use qaqh_message::legacy_writer::LegacyWriterFacade;

static ACTIVE_WRITERS: AtomicUsize = AtomicUsize::new(0);

#[test]
fn facade_serializes_concurrent_writers() {
    let barrier = Arc::new(Barrier::new(3));
    let mut handles = Vec::new();

    for _ in 0..2 {
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            barrier.wait();
            let _guard = LegacyWriterFacade::lock();
            let active = ACTIVE_WRITERS.fetch_add(1, Ordering::SeqCst) + 1;
            assert_eq!(active, 1, "legacy writers overlapped");
            thread::sleep(Duration::from_millis(20));
            ACTIVE_WRITERS.fetch_sub(1, Ordering::SeqCst);
        }));
    }

    barrier.wait();
    for handle in handles {
        handle.join().expect("writer thread");
    }
    assert_eq!(ACTIVE_WRITERS.load(Ordering::SeqCst), 0);
}
