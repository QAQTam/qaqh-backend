//! Small synthetic ownership probes; no provider calls, tools, real sessions, or subagents.
//! Run through probes/memory-hotspots/Cargo.toml to avoid runtime dev-dependencies.
//! Counts requested live heap bytes with System (not RSS or allocator usable sizes).
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::time::Instant;

// Compile the exact production metric implementation, not a reimplementation.
#[allow(dead_code)]
#[path = "../src/agent/state/token_calibration.rs"]
mod token_probe;

struct Tracked;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
fn added(n: usize) {
    let live = LIVE.fetch_add(n, Relaxed).saturating_add(n);
    PEAK.fetch_max(live, Relaxed);
}
// SAFETY: every allocation operation is forwarded unchanged to System;
// bookkeeping is allocation-free and observes requested sizes only.
unsafe impl GlobalAlloc for Tracked {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            added(l.size());
        }
        p
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(l) };
        if !p.is_null() {
            added(l.size());
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size(), Relaxed);
        unsafe { System.dealloc(p, l) };
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, l, n) };
        if !q.is_null() {
            if n >= l.size() {
                added(n - l.size());
            } else {
                LIVE.fetch_sub(l.size() - n, Relaxed);
            }
        }
        q
    }
}
#[global_allocator]
static ALLOCATOR: Tracked = Tracked;
struct Scratch(std::path::PathBuf);
impl Scratch {
    fn new() -> std::io::Result<Self> {
        static N: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "qaqh-memory-probe-{}-{}",
            std::process::id(),
            N.fetch_add(1, Relaxed)
        ));
        std::fs::create_dir(&path)?;
        Ok(Self(path))
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
const MIB: usize = 1024 * 1024;
fn start() -> (usize, Instant) {
    let b = LIVE.load(Relaxed);
    PEAK.store(b, Relaxed);
    (b, Instant::now())
}
fn report(label: &str, b: (usize, Instant)) {
    println!(
        "{label}: live_delta={} peak_delta={} elapsed_ms={:.3}",
        LIVE.load(Relaxed) as i64 - b.0 as i64,
        PEAK.load(Relaxed).saturating_sub(b.0),
        b.1.elapsed().as_secs_f64() * 1000.0
    );
}
fn gauge(rows: &[qaqh_memwatch::ComponentMemory], name: &str) -> u64 {
    rows.iter()
        .find(|r| r.name == name)
        .and_then(|r| r.payload_bytes)
        .unwrap_or(0)
}
fn open(a: &mut qaqh_runtime::TimelineAppender) {
    a.open_turn("s", "t1", "probe").unwrap();
    a.open_block(
        "s",
        "t1",
        0,
        "b",
        qaqh_domain::TimelineBlockKind::Text,
        None,
    )
    .unwrap();
}
fn channels() {
    println!(
        "sizes: WriterEvent={} WorkerCommand={} TimelineEntry={} V2Envelope={}",
        std::mem::size_of::<qaqh_runtime::agent::types::WriterEvent>(),
        std::mem::size_of::<qaqh_runtime::agent::types::WorkerCommand>(),
        std::mem::size_of::<qaqh_domain::TimelineEntry>(),
        std::mem::size_of::<qaqh_runtime::ringing::V2Envelope>()
    );
    let b = start();
    let c = qaqh_runtime::agent::loop_core::LoopChannels::new();
    report("one empty worker channel pair", b);
    // B1.1（暂缓）：空通道对固定税 ~15.8MB（4096×WriterEvent + 16384×WorkerCommand
    // ×824/528B）。缩容被 registry 阻塞 send / worker 回调锁环阻塞，见
    // docs/plan-memory-hotspots-remediation-2026-10-22.md §B1.1 决策记录。
    println!(
        "B1.1 baseline (blocked): channel fixed tax = {} bytes",
        LIVE.load(Relaxed).saturating_sub(b.0)
    );
    drop(c);
    report("after channel drop", b);
}
#[cfg(test)]
mod channel_regressions {
    use qaqh_runtime::agent::loop_core::LoopChannels;
    use qaqh_runtime::agent::types::WriterEvent;
    use std::sync::mpsc::{self, TrySendError};
    use std::time::Duration;

    fn event(n: usize) -> WriterEvent {
        WriterEvent::TitleChanged {
            session_id: "synthetic".into(),
            title: n.to_string(),
        }
    }

    #[test]
    #[ignore = "B1.1: target capacity blocked on registry shutdown/backpressure safety"]
    fn burst_is_bounded_and_reader_disconnect_releases_sender() {
        let channels = LoopChannels::new();
        let mut count = 0;
        loop {
            match channels.event_tx.try_send(event(count)) {
                Ok(()) => count += 1,
                Err(TrySendError::Full(_)) => break,
                Err(TrySendError::Disconnected(_)) => panic!("reader disappeared"),
            }
        }
        assert_eq!(count, 1024);
        let tx = channels.event_tx.clone();
        let (done_tx, done_rx) = mpsc::channel();
        let sender = std::thread::spawn(move || {
            done_tx.send(tx.send(event(count)).is_err()).unwrap();
        });
        drop(channels.event_rx);
        assert!(done_rx.recv_timeout(Duration::from_secs(2)).unwrap());
        sender.join().unwrap();
    }

    #[test]
    fn burst_preserves_order_under_backpressure() {
        let channels = LoopChannels::new();
        let tx = channels.event_tx;
        let sender = std::thread::spawn(move || {
            for n in 0..2048 {
                tx.send(event(n)).unwrap();
            }
        });
        for n in 0..2048 {
            match channels
                .event_rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
            {
                WriterEvent::TitleChanged { title, .. } => assert_eq!(title, n.to_string()),
                _ => panic!("unexpected event"),
            }
        }
        sender.join().unwrap();
    }
}

#[cfg(test)]
mod journal_regressions {
    use super::{MIB, gauge, open};
    use qaqh_domain::{TimelineBlockKind, TimelineTool};
    use qaqh_runtime::TimelineAppender;

    #[test]
    fn journal_counts_checkpoint_argument_and_nested_tool_display() {
        let mut appender = TimelineAppender::new();
        open(&mut appender);
        appender
            .checkpoint_block("s", "t1", 0, "b", "x".repeat(MIB))
            .unwrap();
        let after_checkpoint = gauge(
            &appender.memory_components(),
            "ringing.timeline.replay_journal",
        );
        assert!(after_checkpoint >= MIB as u64);
        let tool: TimelineTool = serde_json::from_value(serde_json::json!({
            "tool_call_id":"c", "name":"edit", "state":"running",
            "args_json":"a".repeat(MIB),
            "display":{"body":{"kind":"diff","unified":"d".repeat(MIB),"files":["f"]}}
        }))
        .unwrap();
        appender
            .open_block(
                "s",
                "t1",
                0,
                "tool",
                TimelineBlockKind::Tool,
                Some(tool.clone()),
            )
            .unwrap();
        appender.replace_tool("s", "t1", 0, "tool", tool).unwrap();
        let total = gauge(
            &appender.memory_components(),
            "ringing.timeline.replay_journal",
        );
        assert!(
            total >= after_checkpoint + 4 * MIB as u64,
            "nested tool bodies must be counted for both entries: {total}"
        );
    }
}

fn journal() {
    let mut a = qaqh_runtime::TimelineAppender::new();
    open(&mut a);
    let b = start();
    a.checkpoint_block("s", "t1", 0, "b", "x".repeat(8 * MIB))
        .unwrap();
    report("8MiB incremental checkpoint", b);
    let rows = a.memory_components();
    println!(
        "checkpoint journal_reported_payload={} (expected actual arg=8388608)",
        gauge(&rows, "ringing.timeline.replay_journal")
    );
    let replay = a.replay_since("s", 0);
    let args: usize = replay
        .iter()
        .map(|e| match &e.event {
            qaqh_domain::TimelineEvent::BlockCheckpoint { arg, .. } => {
                arg.as_ref().map_or(0, String::len)
            }
            _ => 0,
        })
        .sum();
    println!("actual checkpoint replay arg bytes={args}");
    assert_eq!(args, 8 * MIB);
    assert!(gauge(&rows, "ringing.timeline.replay_journal") >= 8 * MIB as u64);
    drop(replay);
    a.seal_block("s", "t1", 0, "b").unwrap();
    a.seal_round("s", "t1", 0, true).unwrap();
    a.seal_turn("s", "t1").unwrap();
    a.enable_offload("s");
    let candidate = a.offload_candidate("s", "t1").unwrap();
    let generation = candidate.created_seq;
    drop(candidate);
    a.mark_offloaded_if_current("s", "t1", generation);
    report("after simulated successful turn offload", b);
}
fn forget() {
    let h = qaqh_runtime::RingingHub::new("synthetic");
    let b = start();
    h.publish_timeline(
        "s",
        qaqh_domain::TimelineIntent::TurnOpened {
            turn_id: "t1".into(),
            user_text: "u".repeat(4 * MIB),
        },
    )
    .unwrap();
    report("4MiB user timeline", b);
    h.forget_session("s");
    report("after forget_session", b);
    let survives = h.timeline_snapshot("s").is_some();
    println!("timeline survives forget={survives}");
    assert!(survives);
    drop(h);
    report("after entire hub drop", b);
}
fn content() {
    let root = Scratch::new().unwrap();
    let mut s = qaqh_runtime::ringing::content_store::ContentStore::with_root(root.path());
    let b = start();
    let id = s.put("s", "text/plain", vec![b'a'; 4 * MIB], false);
    report("persisted 4MiB content", b);
    assert_eq!(gauge(&s.memory_components(), "ringing.content_store.loaded_bodies"), 0);
    let body = s.get("s", &id).unwrap();
    report("GET content clone held", b);
    drop(body);
    report("after GET clone drop", b);
}
fn processes() {
    use qaqh_workspace::process_registry::ProcessRegistry as R;
    let chunk = "x".repeat(MIB);
    let b = start();
    let mut last = 0;
    for _ in 0..12 {
        last = R::register("synthetic-only-no-OS-process");
        R::append_output(last, &chunk);
        R::mark_exited(last, 0);
    }
    report("12 completed synthetic exec entries, 1MiB each", b);
    let info = R::get_info(last).unwrap();
    println!(
        "visible tail bytes={}",
        info["output"].as_str().unwrap().len()
    );
    drop(info);
    let full = R::captured_full(last).unwrap();
    println!("retained full stdout bytes={}", full.0.len());
    assert_eq!(full.0.len(), MIB);
}
fn make_log(n: usize) -> (Scratch, qaqh_session::canonical::CanonicalSessionIdentity) {
    use qaqh_session::canonical::{CanonicalSessionIdentity, EventsCommit};
    use std::io::Write;
    let root = Scratch::new().unwrap();
    let identity = CanonicalSessionIdentity::open_or_create(root.path()).unwrap();
    let template: serde_json::Value = serde_json::from_str(include_str!(
        "../../qaqh-session/tests/fixtures/session_fact_v2/envelope.jsonl"
    ))
    .unwrap();
    let mut file =
        std::io::BufWriter::new(std::fs::File::create(root.path().join("events.jsonl")).unwrap());
    let mut offset = 0;
    for i in 1..=n {
        let mut fact = template.clone();
        fact["session_id"] = serde_json::json!(identity.session_id);
        fact["log_id"] = serde_json::json!(identity.log_id);
        fact["fact_seq"] = serde_json::json!(i);
        fact["event_id"] = serde_json::json!(format!("01J{i:023}"));
        // Repeated valid SessionCreated facts exercise reader allocations without
        // assuming a user-history body; the projection test uses unique inputs.
        fact["payload"]["data"]["cwd"] = serde_json::json!(format!("/{}", "w".repeat(4095)));
        let line = serde_json::to_vec(&fact).unwrap();
        file.write_all(&line).unwrap();
        file.write_all(b"\n").unwrap();
        offset += line.len() as u64 + 1;
    }
    file.flush().unwrap();
    let mut marker = EventsCommit::empty(identity.log_id.clone());
    marker.committed_fact_seq = n as u64;
    marker.committed_offset = offset;
    marker.last_barrier_event_id = Some(qaqh_session::session_fact_v2::EventId::new(format!(
        "01J{n:023}"
    )));
    std::fs::write(
        root.path().join("events.commit.json"),
        serde_json::to_vec(&marker).unwrap(),
    )
    .unwrap();
    (root, identity)
}
fn reader() {
    use qaqh_session::canonical::CommittedFactReader as R;
    for n in [256, 1024] {
        let (root, id) = make_log(n);
        let b = start();
        let r = R::open(root.path(), id.session_id, id.log_id).unwrap();
        report(&format!("reader open n={n}"), b);
        let b = start();
        let tail = r.read_range(n as u64, n as u64).unwrap();
        println!(
            "one-fact range vector capacity={} fact_size={}",
            tail.capacity(),
            std::mem::size_of::<qaqh_session::session_fact_v2::SessionFact>()
        );
        assert_eq!(tail.len(), 1);
        assert!(tail.capacity() <= 4);
        report(
            &format!("reader ONE fact range n={n}, returned={}", tail.len()),
            b,
        );
        drop(tail);
        let b = start();
        let empty = R::open(root.path(), r.session_id().clone(), r.log_id().clone())
            .unwrap()
            .read_after(n as u64)
            .unwrap();
        report(
            &format!(
                "idle polling OPEN+read_after n={n}, returned={}",
                empty.len()
            ),
            b,
        );
    }
}
fn v2() {
    use qaqh_session::canonical::{CanonicalSessionIdentity, EventsCommit};
    use std::io::Write;
    let root = Scratch::new().unwrap();
    let id = CanonicalSessionIdentity::open_or_create(root.path()).unwrap();
    let mut writer =
        std::io::BufWriter::new(std::fs::File::create(root.path().join("events.jsonl")).unwrap());
    let mut offset = 0;
    for i in 1..=1024 {
        let value = serde_json::json!({
            "schema": {"name":"qaqh.session-fact","version":2,"payload_version":2},
            "session_id":id.session_id, "log_id":id.log_id, "fact_seq":i,
            "event_id":format!("01J{i:023}"), "ts_ms":1789830000000_i64,
            "payload":{"kind":"input_accepted","data":{
                "input_id":format!("input_01J{i:023}"),"input_kind":"user_text", "input_purpose":"trigger_turn",
                "inline_text":"x".repeat(8192),"attachments":[], "actor":{"kind":"user","id":"local"}
            }}
        });
        let line = serde_json::to_vec(&value).unwrap();
        writer.write_all(&line).unwrap();
        writer.write_all(b"\n").unwrap();
        offset += line.len() as u64 + 1;
    }
    writer.flush().unwrap();
    let mut marker = EventsCommit::empty(id.log_id.clone());
    marker.committed_fact_seq = 1024;
    marker.committed_offset = offset;
    marker.last_barrier_event_id = Some(qaqh_session::session_fact_v2::EventId::new(format!(
        "01J{:023}",
        1024
    )));
    std::fs::write(
        root.path().join("events.commit.json"),
        serde_json::to_vec(&marker).unwrap(),
    )
    .unwrap();
    let h = qaqh_runtime::ringing::V2ProjectionHub::new("synthetic");
    let b = start();
    let snap = h.bootstrap(root.path(), id.session_id.as_str()).unwrap();
    report(
        "V2 8MiB inline input: cached projections + bootstrap held",
        b,
    );
    println!(
        "conversation entries={} timeline entries={}",
        snap.projections.conversation.context.len(),
        snap.projections.timeline.entries.len()
    );
    drop(snap);
    report("V2 after bootstrap drop", b);
    println!(
        "V2 reported heap lower bound={:?}",
        h.memory_components()[0].heap_estimate_bytes
    );
    let b3 = start();
    let store = qaqh_session::canonical::CanonicalSessionStore::open(
        root.path(),
        id.session_id.clone(),
        id.log_id.clone(),
    )
    .unwrap();
    report(
        "additional writer CanonicalSessionStore for SAME session",
        b3,
    );
    drop(store);
    let b2 = start();
    let _sub = h
        .subscribe(root.path(), id.session_id.as_str(), None)
        .unwrap();
    report("V2 subscribe(None) at current cursor", b2);
    assert!(PEAK.load(Relaxed).saturating_sub(b2.0) < MIB, "zero-replay subscription must not rebuild historical projections");
    drop(h);
    report("V2 hub drop (subscription still alive)", b);
}
fn context() {
    let mut store = qaqh_message::MessageStore::new_ephemeral("synthetic");
    let text = "x".repeat(32 * 1024);
    for _ in 0..256 {
        store.push_user(&text);
    }
    drop(text);
    let b = start();
    let messages = store.build_context_for_gate(&[]);
    report("8MiB context: owned request copy", b);
    let b2 = start();
    let metrics = token_probe::prepared_request_metrics(&messages, None, None);
    report("production prepared_request_metrics", b2);
    println!("metrics serialized bytes={}", metrics.2);
    drop(messages);
    report("after request copy drop", b);
}
fn display() {
    use qaqh_workspace::tool_api::{
        ToolDisplay, ToolModelProjection, ToolOutcome, ToolOutputValue, ToolStatus,
    };
    let outcome = ToolOutcome {
        status: ToolStatus::Ok,
        output: ToolOutputValue::Empty,
        error: None,
        model: ToolModelProjection {
            text: "ok".into(),
            truncated: false,
        },
        display: ToolDisplay::default().with_diff("d".repeat(4 * MIB)),
        images: Vec::new(),
        metrics: Default::default(),
        effects: Vec::new(),
    };
    let b = start();
    let wire = outcome.to_tool_result();
    report(
        "4MiB display diff -> ToolResult (source outcome still held)",
        b,
    );
    println!(
        "model bytes={} top diff={} display diff={}",
        wire.model_text().len(),
        wire.diff.as_ref().map_or(0, String::len),
        wire.display()
            .and_then(|d| d.diff.as_ref())
            .map_or(0, String::len)
    );
    assert_eq!(wire.diff.as_ref().map_or(0, String::len), 4 * MIB);
    assert_eq!(
        wire.display()
            .and_then(|d| d.diff.as_ref())
            .map_or(0, String::len),
        4 * MIB
    );
    let b2 = start();
    let cloned = wire.clone();
    report("deep clone ToolResult for request/history", b2);
    std::hint::black_box(&cloned);
}
fn persist() {
    use qaqh_domain::{TimelineBlockKind as K, TimelineIntent as I, TimelineTurnState as S};
    let root = Scratch::new().unwrap();
    let hub = qaqh_runtime::RingingHub::with_persistence("synthetic", root.path());
    for i in 1..=4 {
        let t = format!("t{i}");
        hub.publish_timeline(
            "s",
            I::TurnOpened {
                turn_id: t.clone(),
                user_text: "probe".into(),
            },
        )
        .unwrap();
        hub.publish_timeline(
            "s",
            I::BlockOpened {
                turn_id: t.clone(),
                round_num: 0,
                block_id: "b".into(),
                kind: K::Text,
                tool: None,
                reasoning_kind: None,
            },
        )
        .unwrap();
        hub.publish_timeline(
            "s",
            I::BlockCheckpoint {
                turn_id: t.clone(),
                round_num: 0,
                block_id: "b".into(),
                text: "h".repeat(MIB),
            },
        )
        .unwrap();
        hub.publish_timeline(
            "s",
            I::BlockSealed {
                turn_id: t.clone(),
                round_num: 0,
                block_id: "b".into(),
            },
        )
        .unwrap();
        hub.publish_timeline(
            "s",
            I::RoundSealed {
                turn_id: t.clone(),
                round_num: 0,
                is_final: true,
            },
        )
        .unwrap();
        hub.publish_timeline(
            "s",
            I::TurnSealed {
                turn_id: t,
                state: S::Completed,
                failure: None,
            },
        )
        .unwrap();
    }
    hub.publish_timeline(
        "s",
        I::TurnOpened {
            turn_id: "hot".into(),
            user_text: "probe".into(),
        },
    )
    .unwrap();
    hub.publish_timeline(
        "s",
        I::BlockOpened {
            turn_id: "hot".into(),
            round_num: 0,
            block_id: "b".into(),
            kind: K::Text,
            tool: None,
            reasoning_kind: None,
        },
    )
    .unwrap();
    hub.flush_timeline_persistence();
    std::thread::sleep(std::time::Duration::from_millis(1200));
    let snapshot = hub.timeline_snapshot("s").unwrap();
    println!(
        "sealed offloaded turns={}",
        snapshot.turns.iter().filter(|t| t.offloaded).count()
    );
    drop(snapshot);
    let path = root.path().join("ringing-timeline/s.json");
    let before = std::fs::metadata(&path).unwrap().modified().unwrap();
    let b = start();
    hub.publish_timeline(
        "s",
        I::TextDelta {
            turn_id: "hot".into(),
            round_num: 0,
            block_id: "b".into(),
            delta: "transient-only-marker".into(),
        },
    )
    .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1500));
    report("21-byte transient event with 4MiB sealed history", b);
    let metadata = std::fs::metadata(&path).unwrap();
    let rewritten = metadata.modified().unwrap() != before;
    let disk = std::fs::read_to_string(&path).unwrap();
    println!(
        "transient rewrote snapshot={rewritten}, snapshot_bytes={}, persisted marker={}",
        metadata.len(),
        disk.contains("transient-only-marker")
    );
    // B1.4 契约：瞬态事件不触发旧史全量重写（热路径零持久化）。
    assert!(!rewritten && !disk.contains("transient-only-marker"),
        "transient-only event must not rewrite the snapshot on the hot path");
    // 显式同步边界仍 fail-closed：flush 排空瞬态集合后必须完整落盘。
    hub.flush_timeline_persistence();
    let metadata = std::fs::metadata(&path).unwrap();
    let flushed = metadata.modified().unwrap() != before;
    let disk = std::fs::read_to_string(&path).unwrap();
    println!(
        "after flush rewrote snapshot={flushed}, snapshot_bytes={}, persisted marker={}",
        metadata.len(),
        disk.contains("transient-only-marker")
    );
    assert!(flushed && disk.contains("transient-only-marker"),
        "explicit flush boundary must persist the transient tail");
}
fn main() {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "all".into());
    // Warm stdout before allocation baselines.
    println!("synthetic memory probes; requested bytes, not RSS; mode={mode}");
    for (name, f) in [
        ("channels", channels as fn()),
        ("journal", journal),
        ("forget", forget),
        ("content", content),
        ("processes", processes),
        ("reader", reader),
        ("v2", v2),
        ("context", context),
        ("display", display),
        ("persist", persist),
    ] {
        if mode == "all" || mode == name {
            println!("--- {name} ---");
            f();
        }
    }
}
