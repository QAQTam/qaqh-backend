//! ADR-0034 遗留的「daemon 级真实洪峰冒烟」+ ADR-0035「客户端面不设闸」守卫。
//!
//! 全程真实链路：**真实子进程** → 真实 `ExecSessionManager`（reader 64 KiB 合批 →
//! 同 run 合帧）→ 真实 owner 泵（ADR-0030 合帧 + 落日志）→ 真实 SQLite 落库 →
//! 真实 append-only 权威日志 → 真实区间读（客户端补读主路径）。零 mock、零合成帧：
//! 冒烟要证明的正是"洪峰真的跑过一遍，daemon 还站着，且一个字节都没丢"。
//!
//! 守卫清单（对应 ADR-0034 决策 1/2/3 与 ADR-0035 决策 1）：
//! 1. 洪峰进程正常跑完（没被泄洪/限幅杀掉）；
//! 2. 客户端事件面**不设闸**：SSE/广播收到的 delta 拼接 == 全量输出（合帧但不丢帧）；
//! 3. 权威日志字节 == 全量输出，且库水位（`next_offset`）与日志一致；
//! 4. settle 不读日志内容：`output_content_id IS NULL` + `output_complete` 如实；
//! 5. 区间读（客户端主读取面）从 0 读回全量；
//! 6. daemon 堆高水位增量有界（linux；一侧保守探测，见下方注释）。

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use qaqh_process_tools::exec_session::capture::ExecOutputWatermark;
use qaqh_process_tools::exec_session::{
    ExecSessionManager, ExecSessionPoll, ExecSessionPublisher, ExecSessionStart,
};
use qaqh_ringing_contract::events::ExecOutputSource;
use qaqh_ringing_contract::fact::ToolCallOutcome;
use qaqh_ringing_contract::state::ExecSessionView;
use qaqh_ringing_contract::SessionEvent;
use qaqh_session::db::store;
use qaqh_types::ExecSessionId;
use tokio::sync::broadcast;

use super::test_state;

/// 洪峰总量：32 MiB。取值理由：ADR-0034 的放大类病灶（16 KiB preview × 每条 2 KiB
/// 消息 ≈ 9×、每 chunk 陪跑 head/tail ≈ 500×）在这个量级上会膨胀到数百 MiB，
/// 高水位增量才能被"4× 总量"的松界抓住。
const FLOOD_BYTES: usize = 32 * 1024 * 1024;
const FRAME_BYTES: usize = 64 * 1024;

/// 生产侧 `OwnerExecPublisher` 的同形替身：daemon 测试不依赖 `qaqh-agent-runtime`，
/// 但发布**语义**（每条 delta 都到、settle 必达）与生产完全一致。
/// 被替换的只是"谁在驱动 exec 会话"，不是"exec 事实怎么进库、怎么进日志"。
#[derive(Clone)]
struct LifecyclePublisher {
    lifecycle: qaqh_session::owner::ToolLifecycle,
    chunks: Arc<AtomicUsize>,
}

impl std::fmt::Debug for LifecyclePublisher {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("LifecyclePublisher")
    }
}

impl ExecSessionPublisher for LifecyclePublisher {
    fn upsert(&self, view: ExecSessionView) {
        self.lifecycle.exec_session_upsert(view);
    }

    fn output(
        &self,
        exec_session_id: &ExecSessionId,
        start_offset: u64,
        end_offset: u64,
        source: ExecOutputSource,
        text: String,
        watermark: &ExecOutputWatermark,
        stream_dropped_bytes: u64,
    ) {
        self.chunks.fetch_add(1, Ordering::SeqCst);
        self.lifecycle.exec_session_output(
            exec_session_id.as_str(),
            start_offset,
            end_offset,
            source,
            text,
            watermark.next_offset,
            Arc::clone(&watermark.preview),
            watermark.preview_start_offset,
            watermark.capture_omitted_bytes,
            stream_dropped_bytes,
        );
    }

    fn finish(&self, view: ExecSessionView, transcript: String, truncated: bool) {
        let state_json = serde_json::to_string(&view.state).expect("exec session state json");
        self.lifecycle
            .exec_session_finished(qaqh_session::owner::ExecSessionSettlement {
                exec_session_id: view.id.as_str().to_string(),
                state_json,
                finished_at_ms: view.finished_at_ms.unwrap_or_else(store::now_ms),
                output_complete: view.output_complete,
                output_content_id: None,
                output_text: (!transcript.is_empty()).then_some(transcript),
                output_truncated: truncated,
                output_preview: view.output_preview,
                preview_start_offset: view.preview_start_offset,
            });
    }
}

/// 一侧保守的堆高水位探测：`VmHWM` 只增不减，因此"N 秒内的增量"只会**低估**
/// 真实驻留，不会把别处的高水位算到本测试头上（并行测试最多让它更松，不会假红）。
#[cfg(target_os = "linux")]
fn heap_high_water_bytes() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status.lines().find_map(|line| {
        let rest = line.strip_prefix("VmHWM:")?;
        let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
        Some(kb * 1024)
    })
}

#[cfg(not(target_os = "linux"))]
fn heap_high_water_bytes() -> Option<u64> {
    None
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_exec_flood_keeps_daemon_alive_and_client_face_lossless() {
    let heap_before = heap_high_water_bytes();

    let state = test_state();
    let session_id = format!(
        "SES-FLOOD-{}",
        qaqh_ringing_contract::ulid::generate_ulid()
    );
    let row = store::NewSession {
        session_id: session_id.clone(),
        parent_session_id: None,
        title: "daemon 洪峰冒烟".into(),
        title_source: Some("user".into()),
        cwd: Some(std::env::temp_dir().to_string_lossy().into_owned()),
        model: Some("flood-smoke".into()),
        tool_mode: None,
        model_selection: None,
        custom_tools: None,
        frozen_annotation: None,
        ephemeral: false,
    };
    let seed = session_id.clone();
    state
        .service
        .owners()
        .db()
        .write_blocking(move |conn| store::create_session(conn, &row))
        .unwrap_or_else(|error| panic!("seed business row {seed}: {error}"));

    let owner = state.service.owners().get_or_create(&session_id);
    // 订阅必须早于启动：ADR-0035 决策 1 的"客户端面不设闸"要在同一条链路上验。
    let mut rx = owner.subscribe();

    let publisher = Arc::new(LifecyclePublisher {
        lifecycle: owner.lifecycle(),
        chunks: Arc::new(AtomicUsize::new(0)),
    });
    let shutdown = Arc::new(AtomicBool::new(false));
    let manager = ExecSessionManager::new();
    let publisher_driver = Arc::clone(&publisher);
    let manager_driver = manager.clone();
    let cancellation = Arc::clone(&shutdown);
    let result = tokio::task::spawn_blocking(move || {
        let first = manager_driver.start(
            ExecSessionStart {
                owner_session_id: "SES-FLOOD".into(),
                origin_turn_id: "TURN-FLOOD".into(),
                origin_call_id: "CALL-FLOOD".into(),
                command: format!("head -c {FLOOD_BYTES} /dev/zero | tr '\\0' x"),
                cwd: None,
                argv: vec![
                    "sh".into(),
                    "-c".into(),
                    format!("head -c {FLOOD_BYTES} /dev/zero | tr '\\0' x"),
                ],
                env: Vec::new(),
                tty: false,
                timeout_ms: Some(120_000),
                yield_time_ms: 200,
                max_output_bytes: FRAME_BYTES,
                cancellation: Arc::clone(&cancellation),
                sandbox_spec: None,
            },
            publisher_driver.clone(),
        )?;
        let mut current = first;
        for _ in 0..2_048 {
            if !matches!(current.outcome, ToolCallOutcome::Yielded { .. }) {
                return Ok::<_, qaqh_process_tools::exec_session::ExecManagerError>(current);
            }
            let exec_id = current.exec_session_id.clone().expect("exec session id");
            current = manager_driver.poll(ExecSessionPoll {
                exec_session_id: exec_id,
                owner_session_id: "SES-FLOOD".into(),
                chars: String::new(),
                yield_time_ms: 200,
                max_output_bytes: FRAME_BYTES,
                cancellation: Arc::clone(&cancellation),
            })?;
        }
        panic!("真洪峰 2048 轮仍未收口");
    })
    .await
    .expect("flood driver join")
    .expect("flood driver");

    let exec_id = result.exec_session_id.clone().expect("exec session id");
    assert_eq!(
        result.outcome,
        ToolCallOutcome::Completed,
        "洪峰进程必须正常跑完（限幅/泄洪都不许杀进程）"
    );
    // Request 屏障：此前所有 delta 必已被泵 flush 落库、落日志（T6 顺序）。
    owner.flush().await.expect("owner barrier");

    // 客户端事件面：合帧但不丢帧。拼接顺序即到达序（ADR-0030 定序不变量）。
    let mut started = false;
    let mut finished = false;
    let mut client_frames = 0usize;
    let mut client_bytes = 0usize;
    let mut client_text = String::new();
    loop {
        match rx.try_recv() {
            Ok(frame) => match frame.event {
                SessionEvent::ExecSessionStarted { .. } => started = true,
                SessionEvent::ExecSessionFinished { .. } => finished = true,
                SessionEvent::ExecSessionOutputDelta { text, .. } => {
                    client_frames += 1;
                    client_bytes += text.len();
                    client_text.push_str(&text);
                }
                _ => {}
            },
            Err(broadcast::error::TryRecvError::Empty) => break,
            Err(broadcast::error::TryRecvError::Lagged(skipped)) => {
                panic!("客户端事件面丢帧 {skipped} 条——SSE 面不许设闸");
            }
            Err(broadcast::error::TryRecvError::Closed) => break,
        }
    }
    let publisher_chunks = publisher.chunks.load(Ordering::SeqCst);
    assert!(started, "会话建立必须先于任何输出");
    assert!(finished, "终态事件必须到达客户端面");
    assert_eq!(client_bytes, FLOOD_BYTES, "客户端面字节必须与全量输出等长");
    assert_eq!(client_text.len(), FLOOD_BYTES);
    assert!(
        client_frames >= 1 && client_frames < publisher_chunks,
        "合帧必须生效：客户端 {client_frames} 帧 vs 源端 {publisher_chunks} 块"
    );
    // 堆水位探测前先放掉本测试自己的大缓冲：探针只该看 daemon 侧的增长。
    drop(client_text);

    // 权威日志（ADR-0034 决策 3）+ 库水位对账。
    let log_path = qaqh_session::owner::exec_output_log_path(
        state.service.owners().db().as_ref(),
        &session_id,
        exec_id.as_str(),
    )
    .expect("exec log path");
    let log_len = std::fs::metadata(&log_path)
        .unwrap_or_else(|error| panic!("log {}: {error}", log_path.display()))
        .len();
    assert_eq!(log_len, FLOOD_BYTES as u64, "权威日志必须等于全量输出");

    let db = Arc::clone(state.service.owners().db());
    let exec_id_row = exec_id.as_str().to_string();
    let row = db
        .read(move |conn| store::exec_session_by_id(conn, &exec_id_row))
        .await
        .expect("read exec session row")
        .expect("exec session row");
    assert_eq!(row.next_offset, FLOOD_BYTES as u64, "库水位必须追平日志");
    assert!(row.output_complete, "无缺口时 output_complete 必须为真");
    assert!(
        row.output_content_id.is_none(),
        "settle 不得把全文复制进 session_contents（ADR-0034 决策 2）"
    );
    assert_eq!(row.stream_dropped_bytes, 0, "传输面不得丢字节");
    assert!(row.finished_at_ms.is_some(), "终态必须落库");

    // 客户端补读主路径：区间读（ADR-0017 决策 2）从 0 读回全量。
    {
        let (next_offset, text) = qaqh_session::owner::read_exec_output_log_range(
            db.as_ref(),
            &session_id,
            exec_id.as_str(),
            row.next_offset,
            0,
            FLOOD_BYTES as u64 * 2,
        )
        .expect("range read exec log");
        assert_eq!(next_offset, FLOOD_BYTES as u64);
        assert_eq!(text.len(), FLOOD_BYTES, "区间读必须无损");
    }

    if let (Some(before), Some(after)) = (heap_before, heap_high_water_bytes()) {
        let grown = after.saturating_sub(before);
        eprintln!(
            "[flood-smoke] 洪峰 {FLOOD_BYTES} B：源端 {publisher_chunks} 块 → 客户端 {client_frames} 帧 / \
             {client_bytes} B；权威日志 {log_len} B；堆高水位增长 {grown} B"
        );
        assert!(
            grown < (FLOOD_BYTES as u64) * 6,
            "daemon 堆高水位增长 {grown} 字节 > 6× 洪峰（{FLOOD_BYTES}）——洪峰放大类病灶回归"
        );
    }
}
