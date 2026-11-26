//! ADR-0035 端到端反证：**真实**高产 exec 进程 → 真实 `ExecSessionManager` →
//! 真实权威日志，闸门在模型消费面把洪峰收成"一条通告 + 若干水位行"。
//!
//! 反证锚：不加闸时这一串轮询会把每一帧原始输出（默认 64 KiB/帧）全部灌进模型
//! 上下文；带闸后模型面字节被钉在预算上，而客户端面（发布者/SSE）与权威日志
//! **一个字节都没少**，进程也照常跑完（泄洪不杀进程）。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use qaqh_process_tools::exec_session::capture::ExecOutputWatermark;
use qaqh_process_tools::exec_session::{
    ExecSessionManager, ExecSessionPoll, ExecSessionPublisher, ExecSessionStart,
};
use qaqh_ringing_contract::events::ExecOutputSource;
use qaqh_ringing_contract::fact::ToolCallOutcome;
use qaqh_ringing_contract::state::ExecSessionView;
use qaqh_types::ExecSessionId;

use crate::spill_gate::{ExecSpillGate, MODEL_FACE_BUDGET_BYTES, SpillVerdict};

const FRAME_BYTES: usize = 64 * 1024;
/// 分 32 批 × 64 KiB = 2 MiB ≈ 8× 模型面预算。
///
/// 刻意**分批 + 抖动**：一次写完的 2 MiB 会在首个 yield 窗口内跑完，只产生一帧，
/// 反证装置就废了；持续产的进程才会逼出"逐帧直灌"这个病灶（也正是 ADR-0035
/// 描述的真实场景：长构建、持续打日志的合法高产进程）。
const FLOOD_BATCHES: usize = 32;
const FLOOD_BATCH_BYTES: usize = 64 * 1024;
const FLOOD_BYTES: usize = FLOOD_BATCHES * FLOOD_BATCH_BYTES;

/// 记录型发布者：把每一条输出块 append 到测试自己的"权威日志"，并记账。
///
/// 生产侧这一角色是 `OwnerExecPublisher`（owner 泵写日志 + 发 SSE），本测试只替换
/// 落盘目的地，**发布语义（每条 delta 都到）**保持一致。
struct RecordingPublisher {
    log_path: std::path::PathBuf,
    appended_bytes: AtomicU64,
    frames: AtomicUsize,
    finished: AtomicBool,
}

impl RecordingPublisher {
    fn new(log_path: std::path::PathBuf) -> Self {
        Self {
            log_path,
            appended_bytes: AtomicU64::new(0),
            frames: AtomicUsize::new(0),
            finished: AtomicBool::new(false),
        }
    }
}

impl std::fmt::Debug for RecordingPublisher {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("RecordingPublisher")
    }
}

impl ExecSessionPublisher for RecordingPublisher {
    fn upsert(&self, _view: ExecSessionView) {}

    fn output(
        &self,
        _exec_session_id: &ExecSessionId,
        _start_offset: u64,
        _end_offset: u64,
        _source: ExecOutputSource,
        text: String,
        _watermark: &ExecOutputWatermark,
        _stream_dropped_bytes: u64,
    ) {
        if let Some(parent) = self.log_path.parent() {
            std::fs::create_dir_all(parent).expect("create log dir");
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)
            .expect("open log");
        std::io::Write::write_all(&mut file, text.as_bytes()).expect("append log");
        self.appended_bytes
            .fetch_add(text.len() as u64, Ordering::SeqCst);
        self.frames.fetch_add(1, Ordering::SeqCst);
    }

    fn finish(&self, _view: ExecSessionView, _transcript: String, _truncated: bool) {
        self.finished.store(true, Ordering::SeqCst);
    }
}

#[cfg(unix)]
#[test]
fn real_high_yield_process_collapses_into_one_notice_without_losing_bytes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let log_path = dir.path().join("execs_01FLOODREAL.log");
    let publisher = Arc::new(RecordingPublisher::new(log_path.clone()));
    let manager = ExecSessionManager::new();
    let cancellation = Arc::new(AtomicBool::new(false));
    let command = format!(
        "i=0; while [ $i -lt {FLOOD_BATCHES} ]; do yes x | head -c {FLOOD_BATCH_BYTES}; sleep 0.05; i=$((i+1)); done"
    );

    let mut result = manager
        .start(
            ExecSessionStart {
                owner_session_id: "SES-SPILL-REAL".into(),
                origin_turn_id: "TURN-SPILL".into(),
                origin_call_id: "CALL-SPILL".into(),
                command: command.clone(),
                cwd: None,
                argv: vec!["sh".into(), "-c".into(), command],
                env: Vec::new(),
                tty: false,
                timeout_ms: Some(60_000),
                yield_time_ms: 50,
                max_output_bytes: FRAME_BYTES,
                cancellation: Arc::clone(&cancellation),
                sandbox_spec: None,
            },
            publisher.clone(),
        )
        .expect("start real exec session");
    let exec_id = result.exec_session_id.clone().expect("exec session id");

    let gate = ExecSpillGate::new();
    let mut raw_frames = 0usize;
    let mut ungated_bytes = 0u64;
    let mut gated_bytes = 0u64;
    let mut notices = 0usize;
    let mut suppressed = 0usize;
    let mut notice_text = String::new();
    let mut terminal: Option<ToolCallOutcome> = None;

    for _ in 0..512 {
        let produced = result.output.len();
        if produced > 0 {
            raw_frames += 1;
            ungated_bytes += produced as u64;
        }
        match gate.apply(exec_id.as_str(), &mut result, Some(&log_path)) {
            SpillVerdict::Direct => {}
            SpillVerdict::Spilled { .. } => {
                notices += 1;
                notice_text = result.output.clone();
            }
            SpillVerdict::Suppressed { .. } => suppressed += 1,
        }
        gated_bytes += result.output.len() as u64;
        if !matches!(result.outcome, ToolCallOutcome::Yielded { .. }) {
            terminal = Some(result.outcome.clone());
            break;
        }
        result = manager
            .poll(ExecSessionPoll {
                exec_session_id: exec_id.clone(),
                owner_session_id: "SES-SPILL-REAL".into(),
                chars: String::new(),
                yield_time_ms: 50,
                max_output_bytes: FRAME_BYTES,
                cancellation: Arc::clone(&cancellation),
            })
            .expect("poll real exec session");
    }
    assert_eq!(
        terminal,
        Some(ToolCallOutcome::Completed),
        "洪峰进程必须正常跑完（泄洪不杀进程）"
    );
    assert!(
        raw_frames >= 6,
        "反证装置失效：真实洪峰应产生多帧原始输出（实得 {raw_frames} 帧）"
    );
    assert!(
        ungated_bytes > MODEL_FACE_BUDGET_BYTES,
        "反证装置失效：不带闸的模型面必然越预算（实得 {ungated_bytes} 字节）"
    );

    assert_eq!(notices, 1, "开闸必须只发生一次");
    assert!(suppressed >= 1, "开闸后余下帧必须是水位行");
    assert!(
        gated_bytes < MODEL_FACE_BUDGET_BYTES + FRAME_BYTES as u64,
        "模型面 {gated_bytes} 字节越界（预算 {MODEL_FACE_BUDGET_BYTES}）"
    );
    assert!(
        gated_bytes * 4 < ungated_bytes,
        "闸门未生效：模型面 {gated_bytes} vs 原始 {ungated_bytes}"
    );
    assert!(notice_text.contains(&log_path.display().to_string()));
    assert!(notice_text.contains("永不恢复直灌"));

    // 客户端面不设闸：发布者一个字节不少地收到了全量输出。
    assert_eq!(
        publisher.appended_bytes.load(Ordering::SeqCst),
        FLOOD_BYTES as u64,
        "客户端/日志面被削了——泄洪只允许削模型上下文"
    );
    assert!(
        publisher.finished.load(Ordering::SeqCst),
        "settle 必须送达发布者"
    );
    // 权威日志自证：通告指向的文件就是全量真相。
    assert_eq!(
        std::fs::metadata(&log_path).expect("log metadata").len(),
        FLOOD_BYTES as u64,
        "权威日志必须等于全量输出"
    );
}
