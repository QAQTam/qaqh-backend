//! v4.5.1 任务4:exec 进度 SQLite 写频节流(修复单 §二.4.2,宪法 #8 独立子模块)。
//!
//! ADR-0030 前端实时性不变:泵仍按 `20ms / 256KiB` 合帧推送
//! `SessionEvent::ExecSessionOutputDelta`,`append_exec_log` 完整日志文件追加仍按
//! 原频率及时落盘。只有 `store::update_exec_session_progress` 的落库按
//! `exec_session_id` 维度节流到 ≥500ms,把全局唯一写线程 `qaqh-db-writer` 从高频
//! `fsync` 的队头阻塞里解放出来(`synchronous = FULL` 下每条进度写都是一次 fsync)。
//!
//! 三条硬性冲刷不变量(修复单 §二.4.2,严禁违背):
//! 1. 距上次该 `exec_session_id` 进度落库已达 500ms 时立即写入;
//! 2. flush 屏障([`crate::owner::SessionOwner::flush`])先把**所有**待落库的
//!    最新进度同步写入 SQLite,再响应屏障完成;
//! 3. settle 清理该 `exec_session_id` 的挂起节流快照(权威终态由 settle 事务
//!    写入);中间 `update_exec_session_progress` 失败必须 `log::error!` 并
//!    **保留**待冲刷标记,供下次窗口/屏障/终态重试。
//!
//! 并发模型:全部方法只被泵任务串行调用(`run_pump_loop` → `flush_exec_output`
//! / `process`),`Mutex` 只为满足 `Send + Sync`,无实际竞争。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::db::{Db, DbError, store};

/// DB 进度落库最小间隔(修复单 §二.4.2:≥ 500ms)。
pub(crate) const EXEC_PROGRESS_DB_MIN_INTERVAL: Duration = Duration::from_millis(500);

/// 单批 exec 输出的进度快照:`update_exec_session_progress` 所需字段。
#[derive(Clone)]
pub(crate) struct ExecProgressSnapshot {
    next_offset: u64,
    output_preview: Arc<str>,
    preview_start_offset: u64,
    capture_omitted_bytes: u64,
    stream_dropped_bytes: u64,
}

impl ExecProgressSnapshot {
    pub(crate) fn new(
        next_offset: u64,
        output_preview: Arc<str>,
        preview_start_offset: u64,
        capture_omitted_bytes: u64,
        stream_dropped_bytes: u64,
    ) -> Self {
        Self {
            next_offset,
            output_preview,
            preview_start_offset,
            capture_omitted_bytes,
            stream_dropped_bytes,
        }
    }
}

/// 单次进度推进的处置结果,驱动 `flush_exec_output` 的 emit 行为与旧实现对齐。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ProgressOutcome {
    /// 已落库:照常 emit 前端增量。
    Written,
    /// 命中节流窗口被延后:前端增量照常 emit(实时性不变),SQLite 落库由
    /// 下次窗口 / flush 屏障冲刷。
    Deferred,
    /// 行缺失 / 已终态 / offset 回退:与旧实现同语义,抑制本批 emit。
    Skipped,
    /// 落库失败:已 `log::error!` 并保留待冲刷标记(不变量3);本批 emit 抑制
    /// (与旧实现错误路径同语义)。
    Failed,
}

/// exec 进度节流闸:每 `exec_session_id` 一份节流状态 + 待冲刷快照。
#[derive(Default)]
pub(crate) struct ExecProgressGate {
    states: Mutex<HashMap<String, GateState>>,
}

#[derive(Default)]
struct GateState {
    last_write: Option<Instant>,
    pending: Option<ExecProgressSnapshot>,
}

impl ExecProgressGate {
    /// 单批进度推进(不变量1):距上次落库 ≥500ms 立即写;否则把快照挂起。
    /// 两种情况前端增量都照常 emit,只有 SQLite 写频被解耦。
    pub(crate) async fn observe(
        &self,
        db: &Db,
        exec_session_id: &str,
        snapshot: ExecProgressSnapshot,
    ) -> ProgressOutcome {
        let due = match self.lock() {
            Ok(mut states) => {
                let state = states
                    .entry(exec_session_id.to_string())
                    .or_default();
                let due = state
                    .last_write
                    .is_none_or(|last| last.elapsed() >= EXEC_PROGRESS_DB_MIN_INTERVAL);
                state.pending = Some(snapshot);
                due
            }
            Err(error) => {
                error.log("observe");
                return ProgressOutcome::Failed;
            }
        };
        if !due {
            return ProgressOutcome::Deferred;
        }
        self.flush_one(db, exec_session_id).await
    }

    /// flush 屏障(不变量2):冲刷**所有**待落库快照后才返回。
    /// 失败的条目保留待冲刷标记并响亮记日志(不变量3),不假成功。
    pub(crate) async fn drain(&self, db: &Db) {
        let pending: Vec<(String, ExecProgressSnapshot)> = match self.lock() {
            Ok(states) => states
                .iter()
                .filter_map(|(id, state)| {
                    state
                        .pending
                        .clone()
                        .map(|snapshot| (id.clone(), snapshot))
                })
                .collect(),
            Err(error) => {
                error.log("barrier enumerate");
                return;
            }
        };
        for (exec_session_id, _snapshot) in pending {
            self.flush_one(db, &exec_session_id).await;
        }
    }

    /// settle(不变量3):先把该 exec_session_id 的**待落库快照冲刷进行**——
    /// 终态进度的权威来源就是行内水位(`finish_exec_session` 不重写 next_offset,
    /// 只在行状态上收口),被节流延后的尾部批次必须在终态前进库;随后清理
    /// 节流状态,终态之后不再有「下次重试」。
    pub(crate) async fn drain_one(&self, db: &Db, exec_session_id: &str) {
        self.flush_one(db, exec_session_id).await;
        self.clear(exec_session_id);
    }

    /// settle(不变量3):清理该 exec_session_id 的挂起快照;权威终态进度由
    /// settle 事务写入,节流快照不得晚于终态覆盖它。
    pub(crate) fn clear(&self, exec_session_id: &str) {
        match self.lock() {
            Ok(mut states) => {
                states.remove(exec_session_id);
            }
            Err(error) => error.log("settle clear"),
        }
    }

    /// 冲刷单个 `exec_session_id` 的待落库快照(不变量1/2/3 的共同写路径)。
    async fn flush_one(&self, db: &Db, exec_session_id: &str) -> ProgressOutcome {
        let snapshot = match self.lock() {
            Ok(mut states) => {
                let Some(state) = states.get_mut(exec_session_id) else {
                    return ProgressOutcome::Skipped;
                };
                match state.pending.clone() {
                    Some(snapshot) => snapshot,
                    None => return ProgressOutcome::Skipped,
                }
            }
            Err(error) => {
                error.log("flush");
                return ProgressOutcome::Failed;
            }
        };
        match write_progress(db, exec_session_id, &snapshot).await {
            Ok(true) => {
                if let Ok(mut states) = self.lock()
                    && let Some(state) = states.get_mut(exec_session_id)
                {
                    state.last_write = Some(Instant::now());
                    state.pending = None;
                }
                ProgressOutcome::Written
            }
            Ok(false) => {
                // 行缺失 / 已终态 / offset 回退:该会话不再接受进度写,清掉挂起
                // 快照与节流状态(与旧实现 Ok(false) → 不 emit 同语义)。
                self.clear(exec_session_id);
                ProgressOutcome::Skipped
            }
            Err(error) => {
                // 不变量3:响亮记日志并保留待冲刷标记,供下次窗口/屏障/终态重试。
                log::error!(
                    "[owner] exec session {exec_session_id} progress write failed \
                     (pending retained for retry): {error}"
                );
                ProgressOutcome::Failed
            }
        }
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, HashMap<String, GateState>>, GatePoisoned> {
        self.states.lock().map_err(|_| GatePoisoned)
    }
}

/// 闸内互斥锁中毒:fail-closed,按 `Failed` 处置并响亮记日志。
struct GatePoisoned;

impl GatePoisoned {
    fn log(&self, where_: &str) {
        log::error!("[owner] exec progress gate poisoned at {where_}");
    }
}

async fn write_progress(
    db: &Db,
    exec_session_id: &str,
    snapshot: &ExecProgressSnapshot,
) -> Result<bool, DbError> {
    let row_id = exec_session_id.to_string();
    let preview = Arc::clone(&snapshot.output_preview);
    let next_offset = snapshot.next_offset;
    let preview_start_offset = snapshot.preview_start_offset;
    let capture_omitted_bytes = snapshot.capture_omitted_bytes;
    let stream_dropped_bytes = snapshot.stream_dropped_bytes;
    db.write(move |conn| {
        store::update_exec_session_progress(
            conn,
            &row_id,
            next_offset,
            &preview,
            preview_start_offset,
            capture_omitted_bytes,
            stream_dropped_bytes,
        )
    })
    .await
}
