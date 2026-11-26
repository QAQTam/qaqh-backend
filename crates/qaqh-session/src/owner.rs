//! 会话 owner:单个会话的业务顺序与生命周期唯一决定者(架构报告一 §12.5-2)。
//!
//! - **所有业务写与事件广播只在 owner 泵发生**(spec §6.3):状态变更
//!   先提交、再发布,seq 分配单点化,不存在多线程 fetch_add 后分别广播;
//! - 异步入口把"提交+发布"打包成请求闭包投递进泵,oneshot 等待回执;
//! - 执行路径经 [`ToolLifecycle`] 非阻塞上报工具生命周期与结构化效果;
//!   实时输出直接进入同一业务队列,SSE 广播有界,绝不让工具执行等慢 UI。
//!
//! owner 自身是**宿主 tokio runtime 上的一个任务**,不是每会话一条 OS 线程
//! (v4.4 P1-2 / L-31)。定序纪律不变:泵仍然单线程串行消费信箱,一条消息处理
//! 完才取下一条,"先落库、后 emit"仍是原子的;变的是载体——N 个会话不再等于
//! N 条线程(闸门用例 `tests/owner_thread_budget.rs`)。
//!
//! 载体换成任务带来的口径变化(L-03 随本切片退场):**等回执的入口一律
//! `async fn`**(`flush`/`truncate_from_turn`/`update_metadata`)。同步入口若
//! 继续 `rx.recv()`,在 current_thread runtime 上会把唯一能驱动泵的调用方
//! 自己钉死——那不是"慢一点",是死锁。只投递不等回执的方法(`publish`、
//! `queue_block`、各 delta、`ToolLifecycle::*`)保持同步签名:
//! `UnboundedSender::send` 非阻塞,泵在别处被驱动。确实处在同步上下文里的
//! 生产调用方(`spawn_blocking` 内的会话删除/命令投递)经
//! [`qaqh_tool_core::async_rt::block_on_host`] 桥接,在 worker 上误用会当场
//! panic 而不是静默僵死。

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use tokio::sync::mpsc;

use qaqh_ringing_contract::events::{
    ExecOutputSource, HookRunStatus, InteractionEndStatus, OutputStream, SessionEvent,
    SessionEventFrame,
};
use qaqh_ringing_contract::fact::{
    BlockId, ContentHash, ContentRef, ContentValue, InputId, InteractionDecision, InteractionId,
    ToolCallId, ToolCallOutcome, ToolMetrics, TurnId,
};
use qaqh_ringing_contract::state::{
    CodeDelta, ExecSessionView, InteractionView, SessionState, TimelineEntry, ToolCallState,
    ToolEffectInfo, TurnOutcome, TurnPhase,
};
use qaqh_tool_core::{
    EXEC_PROGRESS_CHANNEL_CAPACITY, ExecOutputStream, ExecProgressSender, ExecProgressTotals,
};

use qaqh_tool_core::async_rt;

use crate::db::{Db, DbError, maintenance, store, view};
use crate::exec_progress::{ExecProgressGate, ExecProgressSnapshot, ProgressOutcome};
pub use crate::replay_ring::{OwnerAttach, ReplayRing, ReplaySlice};

/// 每会话 SSE 事件缓冲;落后客户端经快照 + `seq` 水位重新同步(报告一 §194)。
pub(crate) const EVENT_CHANNEL_CAPACITY: usize = 1024;

/// ADR-0030：泵侧合帧窗口与批字节帽。常量，不设配置面（ADR-0029 决策 6 同律）。
/// 窗口自批内首条起算；洪峰下先被字节帽切断，窗口只兜低频尾批。
const EXEC_OUTPUT_COALESCE_WINDOW: Duration = Duration::from_millis(20);
const EXEC_OUTPUT_COALESCE_MAX_BYTES: usize = 256 * 1024;

/// 工具终态输出直存 inline 的字节上限;超过即外置进 `session_contents`,
/// 行与事件只带 ref(见 `OwnerMsg::Settle`)。阈值取 32 KiB:edit/write 的
/// 回执与小 diff 保持 inline 即得,只有真正的大正文才走按需拉取。
const TOOL_OUTPUT_INLINE_MAX_BYTES: usize = 32 * 1024;

/// 泵请求的 future 形态。`'static` + `Send`:请求闭包拿到的是
/// `Arc<OwnerPump>`(泵字段本就全是 `Arc`/`Sender`),因此 future 不借调用方
/// 栈帧,直接进信箱由泵任务串行 `.await`——不存在 `for<'a>` HRTB 地狱。
type OwnerRequestFuture = Pin<Box<dyn Future<Output = Result<(), DbError>> + Send>>;

/// 泵请求:提交 + 发布打包在异步闭包里,由泵任务串行执行。
/// 返回 `Err` 时由泵记 error log;闭包内部必须保证"先落库、后 emit"。
type OwnerRequest = Box<dyn FnOnce(Arc<OwnerPump>) -> OwnerRequestFuture + Send>;

enum OwnerMsg {
    Request(OwnerRequest),
    Output {
        call_id: String,
        stream: OutputStream,
        text: String,
        queued: Arc<std::sync::atomic::AtomicUsize>,
    },
    Settle(Box<ToolSettlement>),
    /// 执行会话建立/迁移(spec §6.1:exec_session_started/state_changed)。
    ExecSessionUpsert(Box<ExecSessionView>),
    /// 执行会话输出增量(采集端已定序;owner 只做事件分配)。
    ExecSessionOutput {
        exec_session_id: String,
        start_offset: u64,
        end_offset: u64,
        source: ExecOutputSource,
        text: String,
        next_offset: u64,
        /// 采集端每批克隆一次的 Arc 共享水位(ADR-0030 补充决策):
        /// 信箱积压时 preview 只有一份物理拷贝,泵合帧只取批内末条。
        output_preview: Arc<str>,
        preview_start_offset: u64,
        capture_omitted_bytes: u64,
        stream_dropped_bytes: u64,
    },
    /// 执行会话终态:一次性落库 + exec_session_finished(spec §8.2)。
    ExecSessionSettle(Box<ExecSessionSettlement>),
}

/// Metadata writes share the session pump and revision boundary.
#[derive(Debug, thiserror::Error)]
pub enum MetadataPatchError {
    #[error("session {session_id} not found")]
    NotFound { session_id: String },
    #[error("settings revision conflict: expected {expected}, current {actual}")]
    RevisionConflict { expected: u64, actual: u64 },
    #[error("{0}")]
    Storage(String),
}

/// 会话 owner:回合、timeline、工具生命周期、执行会话与交互的业务顺序决定者。
pub struct SessionOwner {
    session_id: String,
    db: Arc<Db>,
    events: tokio::sync::broadcast::Sender<SessionEventFrame>,
    next_seq: Arc<AtomicU64>,
    lifecycle_tx: mpsc::UnboundedSender<OwnerMsg>,
    replay: Arc<ReplayRing>,
    /// 泵任务的句柄:`Drop` 时 abort,会话回收即时生效(旧实现靠 sender 全
    /// drop 后 `rx.recv()` 返回 Err 才退线程,还得等信箱排空)。
    pump_task: tokio::task::JoinHandle<()>,
}

/// owner 掉光引用即回收泵任务。`abort` 之后 future 被丢弃,已在飞行的 DB 写
/// 由 `qaqh-db-writer` 线程跑完(事务边界不变),只是不再 emit。
impl Drop for SessionOwner {
    fn drop(&mut self) {
        self.pump_task.abort();
    }
}

/// 泵侧合帧批条目（ADR-0030 决策 2）：`OwnerMsg::ExecSessionOutput` 拆出的载荷。
struct ExecOutputEntry {
    start_offset: u64,
    end_offset: u64,
    source: ExecOutputSource,
    text: String,
    next_offset: u64,
    output_preview: Arc<str>,
    preview_start_offset: u64,
    capture_omitted_bytes: u64,
    stream_dropped_bytes: u64,
}

/// 单一 exec 会话的连续 delta 合帧批；deadline 自首条起算。
///
/// deadline 用 `tokio::time::Instant`(而非 `std::time::Instant`):泵是任务,
/// 窗口到期靠 `sleep_until` 让出 worker,而不是把线程钉在 `recv_timeout` 上。
/// 20ms 窗口本身是 ADR-0030 决策 2 的冻结语义,换时钟类型不改语义。
struct ExecOutputBatch {
    exec_session_id: String,
    entries: Vec<ExecOutputEntry>,
    bytes: usize,
    deadline: tokio::time::Instant,
}

impl SessionOwner {
    /// 创建 owner 并在**宿主 runtime** 上启动业务顺序泵任务(不派生 OS 线程)。
    pub fn new(session_id: impl Into<String>, db: Arc<Db>) -> SessionOwner {
        let session_id = session_id.into();
        let (events, _) = tokio::sync::broadcast::channel(EVENT_CHANNEL_CAPACITY);
        // unbounded:投递侧(`publish`/`queue_block`/各 delta/`ToolLifecycle::*`)
        // 必须是同步非阻塞的,那些签名一字不改;定序与积压上限由泵自己承担
        //(与旧实现同构:`std::sync::mpsc::channel` 同样无界)。
        let (lifecycle_tx, lifecycle_rx) = mpsc::unbounded_channel();
        let next_seq = Arc::new(AtomicU64::new(0));
        let replay = Arc::new(ReplayRing::default());
        let pump = Arc::new(OwnerPump {
            session_id: session_id.clone(),
            db: Arc::clone(&db),
            events: events.clone(),
            next_seq: Arc::clone(&next_seq),
            replay: Arc::clone(&replay),
            exec_progress: ExecProgressGate::default(),
        });
        let pump_task = async_rt::runtime_handle().spawn(run_pump_loop(
            Arc::clone(&pump),
            lifecycle_rx,
        ));
        SessionOwner {
            session_id,
            db,
            events,
            next_seq,
            lifecycle_tx,
            replay,
            pump_task,
        }
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// 读 `sessions` 的标题现态 `(title, title_source)`:空串/缺行归一为 `None`
    /// (与 `view::session_title` 同口径)。读取失败同样返回 `(None, None)`:
    /// 调用方只用它决定控制面工具是否声明,最坏后果是多声明一个工具,不是业务错误。
    pub fn title_state(&self) -> (Option<String>, Option<String>) {
        let session_id = self.session_id.clone();
        self.db
            .read_blocking(move |conn| {
                conn.query_row(
                    "SELECT title, title_source FROM sessions WHERE session_id = ?1",
                    [session_id],
                    |row| {
                        Ok((
                            row.get::<_, Option<String>>(0)?,
                            row.get::<_, Option<String>>(1)?,
                        ))
                    },
                )
            })
            .ok()
            .map(|(title, source)| (title.filter(|title| !title.is_empty()), source))
            .unwrap_or((None, None))
    }

    /// 执行路径侧句柄:工具生命周期上报从这里进入 owner。
    pub fn lifecycle(&self) -> ToolLifecycle {
        ToolLifecycle {
            session_id: self.session_id.clone(),
            tx: self.lifecycle_tx.clone(),
        }
    }

    /// 订阅单流事件(P4 SSE 的来源;落后经快照重同步)。
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<SessionEventFrame> {
        self.events.subscribe()
    }

    /// 读面原子 attach(H1',P0-2026-11-10 报告 §6):**先 grab receiver,再切
    /// 回放尾**。`gap=true` 时游标已跌出尾覆盖范围,调用方走快照 resync;
    /// 否则 `replay.frames` 补发后由 receiver 无缝续 live。
    pub fn attach_after(&self, after_seq: Option<u64>) -> OwnerAttach {
        let receiver = self.events.subscribe();
        let replay = self.replay.slice_after(after_seq);
        OwnerAttach { replay, receiver }
    }

    /// Lagged 后的追赶补发:同 `attach_after` 的环切片,但不新建 receiver。
    pub fn replay_after(&self, after_seq: Option<u64>) -> ReplaySlice {
        self.replay.slice_after(after_seq)
    }

    /// 当前事件水位:即下一个将分配的 seq(exclusive);快照必须携带,客户端
    /// 据此衔接 SSE(`seq < 水位` 跳过,`seq >= 水位` 处理)。
    pub fn current_seq(&self) -> u64 {
        self.next_seq.load(Ordering::Relaxed)
    }

    /// SessionState 快照:业务表直查 + 内存水位。
    ///
    /// 定序纪律:**先取水位,再读库**。业务写路径一律"先落库、后 emit"且二者
    /// 在泵任务内原子完成,因此快照读到的效果要么已提交且水位已含其事件,
    /// 要么事件 seq ≥ 水位由客户端处理,不丢。残余重叠由客户端按
    /// block_id/id 合并吸收。反序(先读库后取水位)会在"落库与 emit 之间"
    /// 留出静默丢帧窗口——B01。
    pub async fn snapshot(&self) -> Result<SessionState, DbError> {
        let watermark = self.current_seq();
        let session_id = self.session_id.clone();
        let mut state = self
            .db
            .read(move |conn| view::load_session_state(conn, &session_id))
            .await?;
        state.seq = watermark;
        Ok(state)
    }

    /// SessionState 快照(尾窗变体):仅 bootstrap HTTP 读口使用。
    ///
    /// 定序纪律与 [`Self::snapshot`] 一致:先取水位,再读库。尾窗只裁剪
    /// timeline 载荷(最近 `tail_blocks` 块,回合边界回退,`timeline_from_ord`
    /// 标记边界),不改 `seq` 覆盖的事件序号空间——含尾窗外历史块产生的事件,
    /// SSE 续传不受影响(B01)。调用方负责 clamp tail 值域(daemon 侧执行)。
    pub async fn snapshot_tailed(&self, tail_blocks: u32) -> Result<SessionState, DbError> {
        let watermark = self.current_seq();
        let session_id = self.session_id.clone();
        let mut state = self
            .db
            .read(move |conn| view::load_session_state_tailed(conn, &session_id, tail_blocks))
            .await?;
        state.seq = watermark;
        Ok(state)
    }

    /// 外部生产者的公共事件出口:非 owner 内部状态的变化
    /// 经此进入唯一单流定序,seq 与业务事件同源单调。
    pub fn publish(&self, event: SessionEvent) {
        self.send_request(move |pump| {
            Box::pin(async move {
                pump.emit(event);
                Ok(())
            })
        });
    }

    /// ADR-0015 切片F：handoff 封箱标记(成功时刻的权威写口)。
    /// 续跑引擎据此跳过已封箱会话,不再向死线投递续跑。
    pub async fn mark_handoff_sealed(&self) -> Result<(), DbError> {
        let session_id = self.session_id.clone();
        self.submit_request(move |pump| {
            Box::pin(async move {
                pump.db
                    .write(move |conn| store::mark_handoff_sealed(conn, &session_id))
                    .await?;
                Ok(())
            })
        })
        .await
    }

    /// 开回合:turn 行(phase=running)+ TurnStarted/TurnPhaseChanged/ActivityChanged。
    pub async fn start_turn(&self, turn_id: &str, input_id: Option<String>) -> Result<(), DbError> {
        let turn_id = turn_id.to_string();
        let session_id = self.session_id.clone();
        let input_id_for_row = input_id.clone();
        self.submit_request(move |pump| {
            Box::pin(async move {
                let row_turn_id = turn_id.clone();
                pump.db
                    .write(move |conn| {
                        store::start_turn(
                            conn,
                            &row_turn_id,
                            &session_id,
                            input_id_for_row.as_deref(),
                        )
                    })
                    .await?;
                let turn = TurnId::new(turn_id.clone());
                pump.emit(SessionEvent::TurnStarted {
                    turn_id: turn.clone(),
                    input_id: input_id.map(InputId::new),
                });
                pump.emit(SessionEvent::TurnPhaseChanged {
                    turn_id: turn.clone(),
                    phase: TurnPhase::Running,
                });
                pump.emit(SessionEvent::ActivityChanged {
                    activity: qaqh_ringing_contract::fact::ActivityState::Running,
                    current_turn_id: Some(turn),
                });
                Ok(())
            })
        })
        .await
    }

    /// 回合非终态 phase 迁移:turns.phase 落库 + TurnPhaseChanged/ActivityChanged。
    pub async fn set_turn_phase(&self, turn_id: &str, phase: TurnPhase) -> Result<(), DbError> {
        let turn_id = turn_id.to_string();
        self.submit_request(move |pump| {
            Box::pin(async move { set_turn_phase_in_pump(&pump, turn_id, phase).await })
        })
            .await
    }

    /// 同步入队回合 phase 迁移;用于 cancel 先于唤醒工具/交互等待排入 owner 泵。
    pub fn queue_turn_phase(&self, turn_id: &str, phase: TurnPhase) {
        let turn_id = turn_id.to_string();
        self.send_request(move |pump| {
            Box::pin(async move { set_turn_phase_in_pump(&pump, turn_id, phase).await })
        });
    }

    /// 回合终态:TurnEnded + ActivityChanged(Idle);phase 列由 finish_turn 清空。
    /// 回合收尾必须等其仍持有的工具/交互结算完成(session_loop 责任),这里
    /// 只发布真实终态,不 accepted 即宣布 cancelled。
    pub async fn finish_turn(
        &self,
        turn_id: &str,
        status: &'static str,
        outcome: TurnOutcome,
    ) -> Result<(), DbError> {
        let outcome_json = serde_json::to_string(&outcome)
            .map_err(|error| DbError::Corrupt(format!("turn {turn_id} outcome: {error}")))?;
        let turn_id_owned = turn_id.to_string();
        self.submit_request(move |pump| {
            Box::pin(async move {
                let row_turn_id = turn_id_owned.clone();
                pump.db
                    .write(move |conn| {
                        store::finish_turn(conn, &row_turn_id, status, Some(&outcome_json))
                    })
                    .await?;
                emit_turn_ended(&pump, turn_id_owned, outcome);
                Ok(())
            })
        })
        .await
    }

    /// 持久化终态写库失败时的显式内存广播口:按泵内单流序发布
    /// `TurnEnded(Failed)` 与 `ActivityChanged(Idle)`,确保前端立即解锁并展示错误。
    pub async fn emit_turn_persist_failed(
        &self,
        turn_id: &str,
        usage: Option<qaqh_types::UsageInfo>,
        code: &str,
        message: String,
    ) {
        let turn_id_owned = turn_id.to_string();
        let outcome = TurnOutcome::Finished {
            terminal: qaqh_ringing_contract::fact::TurnTerminal::Failed,
            usage,
            error: Some(qaqh_ringing_contract::fact::TurnError {
                code: code.to_string(),
                message,
                retryable: false,
                details_ref: None,
            }),
        };
        if let Err(error) = self
            .submit_request(move |pump| {
                Box::pin(async move {
                    emit_turn_ended(&pump, turn_id_owned, outcome);
                    Ok(())
                })
            })
            .await
        {
            log::error!("[owner] emit_turn_persist_failed dropped for {turn_id}: {error}");
        }
    }

    fn block_request(&self, entry: TimelineEntry) -> Result<OwnerRequest, DbError> {
        let block = timeline_block(&self.session_id, &entry)?;
        Ok(Box::new(move |pump| {
            Box::pin(async move {
                let ord = pump
                    .db
                    .write(move |conn| store::append_block(conn, &block))
                    .await?;
                // 落库序号回填进 wire entry:live timeline_appended 与 bootstrap 同携 ord,
                // 客户端按 ord 排序而非到达序(重连丢帧后迟到的 seal 不再插到答案之后)。
                let mut entry = entry;
                if let TimelineEntry::AssistantBlock { ord: slot, .. } = &mut entry {
                    *slot = ord;
                }
                pump.emit(SessionEvent::TimelineAppended { entry });
                Ok(())
            })
        }))
    }

    /// Commit and publish a timeline block before returning.
    pub async fn append_block(&self, entry: TimelineEntry) -> Result<(), DbError> {
        self.submit_request(self.block_request(entry)?).await
    }

    /// Enqueue a block before releasing synchronous input admission.
    pub fn queue_block(&self, entry: TimelineEntry) -> Result<(), DbError> {
        self.send_request(self.block_request(entry)?);
        Ok(())
    }

    /// 撤回(硬截断):进泵串行执行,删除 `turn_id` **含自身**及其后的全部历史事实,
    /// 落库成功后追加一条 [`SessionEvent::TimelineTruncated`]。
    ///
    /// 泵串行化是这条链的定序纪律:`truncate` 与 `start_turn`/`append_block` 同队,
    /// 所以「活动回合冲突」的判据在写点判定,不需要调用方再加一道锁。冲突/找不到
    /// 时**既不写也不发**,回执原样带出错误给命令面(不静默降级成「什么都没删」)。
    ///
    /// `async`(L-03 退场):旧实现在调用线程 `rx.recv()` 等泵 + fsync;泵成了
    /// 任务之后,同步等就是把能驱动泵的那条线程钉死。命令面侧
    /// (`session_loop::truncate_turn`)处在 registry 锁持有的同步段里,经
    /// `async_rt::block_on_host` 桥接——在阻塞池上合法,在 worker 上当场 panic。
    pub async fn truncate_from_turn(&self, turn_id: &str) -> Result<Vec<TurnId>, DbError> {
        let session_id = self.session_id.clone();
        let row_turn_id = turn_id.to_string();
        let from_turn_id = TurnId::new(turn_id.to_string());
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.send_request(move |pump| {
            Box::pin(async move {
                let owned = session_id.clone();
                let row = row_turn_id.clone();
                let result = pump
                    .db
                    .write(move |conn| store::truncate_from_turn(conn, &owned, &row))
                    .await;
                match result {
                    Ok(removed) => {
                        let removed: Vec<TurnId> = removed.into_iter().map(TurnId::new).collect();
                        pump.emit(SessionEvent::TimelineTruncated {
                            from_turn_id: from_turn_id.clone(),
                            removed_turn_ids: removed.clone(),
                        });
                        let _ = tx.send(Ok(removed));
                    }
                    Err(error) => {
                        let _ = tx.send(Err(error));
                    }
                }
                Ok(())
            })
        });
        rx.await.map_err(|_| DbError::ExecutorStopped)?
    }

    /// 交互升起:interaction 行 + InteractionRequested 事件。
    pub async fn interaction_requested(&self, interaction: InteractionView) -> Result<(), DbError> {
        let interaction_id = interaction.interaction_id.as_ref().to_string();
        let payload_json = serde_json::to_string(&interaction)
            .map_err(|error| DbError::Corrupt(format!("interaction {interaction_id}: {error}")))?;
        let kind = serde_json::to_string(&interaction.kind).map_err(|error| {
            DbError::Corrupt(format!("interaction {interaction_id} kind: {error}"))
        })?;
        let row = store::NewInteraction {
            interaction_id: interaction_id.clone(),
            session_id: self.session_id.clone(),
            turn_id: interaction
                .turn_id
                .as_ref()
                .map(|turn| turn.as_ref().to_string()),
            call_id: interaction
                .call_id
                .as_ref()
                .map(|call| call.as_ref().to_string()),
            kind,
            payload_json,
        };
        self.submit_request(move |pump| {
            Box::pin(async move {
                pump.db
                    .write(move |conn| store::create_interaction(conn, &row))
                    .await?;
                pump.emit(SessionEvent::InteractionRequested { interaction });
                Ok(())
            })
        })
        .await
    }

    /// 交互应答:pending → 终态,InteractionResolved 事件(未命中 pending 返回 false)。
    pub async fn interaction_resolved(
        &self,
        interaction_id: &InteractionId,
        decision: Option<InteractionDecision>,
        status: InteractionEndStatus,
    ) -> Result<bool, DbError> {
        let decision_json = serde_json::to_string(&decision)
            .map_err(|error| DbError::Corrupt(format!("interaction {interaction_id}: {error}")))?;
        let status_db = match status {
            InteractionEndStatus::Resolved => "resolved",
            InteractionEndStatus::Expired => "expired",
            InteractionEndStatus::Cancelled => "cancelled",
        };
        let id = interaction_id.as_ref().to_string();
        let event_interaction_id = interaction_id.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.send_request(move |pump| {
            Box::pin(async move {
                let row_id = id.clone();
                let changed = pump
                    .db
                    .write(move |conn| {
                        store::resolve_interaction(conn, &row_id, status_db, &decision_json)
                    })
                    .await;
                if let Ok(true) = &changed {
                    pump.emit(SessionEvent::InteractionResolved {
                        interaction_id: event_interaction_id,
                        decision,
                        status,
                    });
                }
                let _ = tx.send(changed);
                Ok(())
            })
        });
        rx.await
            .map_err(|_| DbError::Corrupt("owner pump dropped request".into()))?
    }

    // ---- 流式展示(同步 API:provider 阻塞线程直接调用;不落库) ----

    /// 模型文本流式片段:同一 block_id 的后续事件替换已展示内容。
    pub fn message_delta(
        &self,
        turn_id: &TurnId,
        block_id: &BlockId,
        block_kind: qaqh_ringing_contract::fact::AssistantBlockKind,
        text: &str,
    ) {
        let turn_id = turn_id.clone();
        let block_id = block_id.clone();
        let text = text.to_string();
        self.send_request(move |pump| {
            Box::pin(async move {
                pump.emit(SessionEvent::MessageDelta {
                    turn_id,
                    block_id,
                    block_kind,
                    text,
                });
                Ok(())
            })
        });
    }

    /// 工具参数流式片段(仅展示;不代表参数已合法或工具已执行)。
    /// 行差估算由 provider 侧随片段一起算好带上来,这里只做投影,不再数一遍。
    pub fn tool_arguments_delta(
        &self,
        turn_id: &TurnId,
        call_id: &ToolCallId,
        tool_name: &str,
        text: &str,
        lines_added: Option<u32>,
        lines_removed: Option<u32>,
        target_files: Vec<String>,
        args_complete: bool,
    ) {
        let turn_id = turn_id.clone();
        let call_id = call_id.clone();
        let tool_name = tool_name.to_string();
        let text = text.to_string();
        self.send_request(move |pump| {
            Box::pin(async move {
                pump.emit(SessionEvent::ToolArgumentsDelta {
                    turn_id,
                    call_id,
                    tool_name,
                    text,
                    lines_added,
                    lines_removed,
                    target_files,
                    args_complete,
                });
                Ok(())
            })
        });
    }

    /// provider 请求正在重试(仅展示;不落库)。attempt/delay 原样透传 gate
    /// 侧策略,客户端渲染"第 n/m 次重试,退避 s 秒"。
    pub fn provider_retrying(
        &self,
        turn_id: &TurnId,
        attempt: u32,
        max_attempts: u32,
        delay_secs: u64,
        error: &str,
    ) {
        let turn_id = turn_id.clone();
        let error = error.to_string();
        self.send_request(move |pump| {
            Box::pin(async move {
                pump.emit(SessionEvent::ProviderRetrying {
                    turn_id,
                    attempt,
                    max_attempts,
                    delay_secs,
                    error,
                });
                Ok(())
            })
        });
    }

    // ---- 生命周期 hooks 事件面(ADR-0038;全部仅展示,不落库) ----

    /// hook 开始执行;HookFinished 必随其后,客户端按 run_id 关联。
    pub fn hook_started(
        &self,
        run_id: &str,
        event: &str,
        hook_name: &str,
        turn_id: Option<&TurnId>,
        call_id: Option<&ToolCallId>,
    ) {
        let run_id = run_id.to_string();
        let event = event.to_string();
        let hook_name = hook_name.to_string();
        let turn_id = turn_id.cloned();
        let call_id = call_id.cloned();
        self.send_request(move |pump| {
            Box::pin(async move {
                pump.emit(SessionEvent::HookStarted {
                    run_id,
                    event,
                    hook_name,
                    turn_id,
                    call_id,
                });
                Ok(())
            })
        });
    }

    /// hook 终态;summary = deny/block 理由或失败摘要(只给人看)。
    pub fn hook_finished(
        &self,
        run_id: &str,
        status: HookRunStatus,
        summary: Option<&str>,
        duration_ms: u64,
    ) {
        let run_id = run_id.to_string();
        let summary = summary.map(str::to_string);
        self.send_request(move |pump| {
            Box::pin(async move {
                pump.emit(SessionEvent::HookFinished {
                    run_id,
                    status,
                    summary,
                    duration_ms,
                });
                Ok(())
            })
        });
    }

    /// UserPromptSubmit hook 拒绝输入:prompt 不入库,reason 只给人看。
    pub fn prompt_blocked(&self, reason: &str) {
        let reason = reason.to_string();
        self.send_request(move |pump| {
            Box::pin(async move {
                pump.emit(SessionEvent::PromptBlocked { reason });
                Ok(())
            })
        });
    }

    // ---- 工具声明与状态迁移(回合循环侧;port 侧 lifecycle 接管 running 之后) ----

    /// 模型声明工具调用:declared 行落库 + ToolDeclared 事件(实时订阅者
    /// 据此获得工具身份与最终参数,与 bootstrap `tools` 视图同形状)。
    pub async fn declare_tool_call(
        &self,
        call_id: &ToolCallId,
        turn_id: &TurnId,
        tool_name: &str,
        args: Option<serde_json::Value>,
    ) -> Result<(), DbError> {
        let args_json = args
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|error| DbError::Corrupt(format!("tool call {call_id} args: {error}")))?;
        let state_json = serde_json::to_string(&ToolCallState::Declared)
            .map_err(|error| DbError::Corrupt(format!("tool call {call_id} state: {error}")))?;
        let call_row = call_id.as_ref().to_string();
        let turn_row = turn_id.as_ref().to_string();
        let session_id = self.session_id.clone();
        let tool_name_owned = tool_name.to_string();
        let event_call = call_id.clone();
        let event_turn = turn_id.clone();
        let event_tool_name = tool_name.to_string();
        self.submit_request(move |pump| {
            Box::pin(async move {
                let upsert = store::ToolCallUpsert {
                    call_id: call_row,
                    session_id,
                    turn_id: Some(turn_row),
                    tool_name: tool_name_owned,
                    args_json,
                    state_json,
                    state_kind: "declared",
                };
                let event_args = upsert.args_json.clone();
                let row = upsert.clone();
                pump.db
                    .write(move |conn| store::upsert_tool_call(conn, &row))
                    .await?;
                pump.emit(SessionEvent::ToolDeclared {
                    call_id: event_call,
                    turn_id: event_turn,
                    tool_name: event_tool_name,
                    args: event_args.and_then(|text| serde_json::from_str(&text).ok()),
                });
                Ok(())
            })
        })
        .await
    }

    /// 权限/Ask/plan 交互挂起:declared → awaiting_interaction(统一覆盖
    /// permission、Ask、plan;具体种类由关联 InteractionView 表达)。
    pub async fn tool_awaiting_interaction(
        &self,
        call_id: &ToolCallId,
        interaction_id: &InteractionId,
    ) -> Result<(), DbError> {
        let call = call_id.as_ref().to_string();
        let state = ToolCallState::AwaitingInteraction {
            interaction_id: interaction_id.as_ref().to_string(),
        };
        let state_json = serde_json::to_string(&state)
            .map_err(|error| DbError::Corrupt(format!("tool call {call}: state: {error}")))?;
        self.submit_request(move |pump| {
            Box::pin(async move {
                pump.db
                    .write(move |conn| {
                        store::set_tool_call_state(conn, &call, &state_json, "awaiting_interaction")
                    })
                    .await
            })
        })
        .await
    }

    /// 权限允许后回到 declared 待实际执行(spec §4.2 迁移表);
    /// 普通权限通过不提前宣称 running,必须等真正进入执行。
    pub async fn tool_back_to_declared(&self, call_id: &ToolCallId) -> Result<(), DbError> {
        let call = call_id.as_ref().to_string();
        let state_json = serde_json::to_string(&ToolCallState::Declared)
            .map_err(|error| DbError::Corrupt(format!("tool call {call}: state: {error}")))?;
        self.submit_request(move |pump| {
            Box::pin(async move {
                pump.db
                    .write(move |conn| {
                        store::set_tool_call_state(conn, &call, &state_json, "declared")
                    })
                    .await
            })
        })
        .await
    }

    /// Wait until preceding lifecycle reports have been committed and published.
    ///
    /// 屏障语义逐字不变(L-03 只改投递方式,不改 ADR-0030/0017 的定序):信箱
    /// FIFO,这条空请求被处理完时,它之前的所有消息都已「落库 + emit」完毕。
    /// 泵成任务后 `async` 是硬要求——同步 `rx.recv()` 在 current_thread runtime
    /// 上就是把泵钉死。
    pub async fn flush(&self) -> Result<(), DbError> {
        self.submit_request(move |pump| {
            Box::pin(async move {
                // v4.5.1 任务4 不变量2:屏障先同步冲刷**所有**待落库的 exec 进度,
                // 再响应 Flush 完成——与既有 owner_tests 语义零漂移。
                pump.exec_progress.drain(&pump.db).await;
                Ok(())
            })
        })
        .await
    }

    /// Commit a metadata patch on the same pump as turn and tool facts.
    ///
    /// `async`:见 [`Self::flush`] 的同一条 L-03 理由。CAS 判据
    /// (`expected_settings_revision` → `RevisionConflict`)在泵内写点执行,
    /// 与 turn/tool 事实同队,定序不变。
    pub async fn update_metadata(
        &self,
        patch: store::SessionColumnPatch,
    ) -> Result<u64, MetadataPatchError> {
        let session_id = self.session_id.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.send_request(move |pump| {
            Box::pin(async move {
                let expected = patch.expected_settings_revision;
                let title = patch.title.clone();
                let owned = session_id.clone();
                let result = pump
                    .db
                    .write(move |conn| {
                        let changed = store::update_session_columns(conn, &owned, &patch)?;
                        let revision = store::session_settings_revision(conn, &owned)?;
                        Ok::<_, DbError>((changed, revision))
                    })
                    .await
                    .map_err(|error| MetadataPatchError::Storage(error.to_string()))
                    .and_then(|(changed, revision)| {
                        let actual = revision.ok_or(MetadataPatchError::NotFound { session_id })?;
                        if changed == 0
                            && let Some(expected) = expected
                        {
                            return Err(MetadataPatchError::RevisionConflict { expected, actual });
                        }
                        Ok(actual)
                    });
                if result.is_ok() && title.is_some() {
                    pump.emit(SessionEvent::SessionMetaChanged { title });
                }
                let _ = tx.send(result);
                Ok(())
            })
        });
        rx.await
            .map_err(|_| MetadataPatchError::Storage("owner pump stopped".into()))?
    }

    fn send_request(
        &self,
        request: impl FnOnce(Arc<OwnerPump>) -> OwnerRequestFuture + Send + 'static,
    ) {
        // 信箱无界且泵任务随会话存活;发送失败意味着 owner 正在回收。
        if let Err(error) = self.lifecycle_tx.send(OwnerMsg::Request(Box::new(request))) {
            log::error!("[owner] request dropped: {error}");
        }
    }

    /// 提交请求并等待回执(异步入口的唯一完成通道)。
    async fn submit_request(
        &self,
        request: impl FnOnce(Arc<OwnerPump>) -> OwnerRequestFuture + Send + 'static,
    ) -> Result<(), DbError> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if self
            .lifecycle_tx
            .send(OwnerMsg::Request(Box::new(move |pump| {
                Box::pin(async move {
                    let result = request(pump).await;
                    let _ = tx.send(result);
                    Ok(())
                })
            })))
            .is_err()
        {
            return Err(DbError::Corrupt("owner pump stopped".into()));
        }
        rx.await
            .map_err(|_| DbError::Corrupt("owner pump dropped request".into()))?
    }
}

/// 序号 + 广播:单流事件出口的唯一写入点;只允许 owner 泵任务调用。
fn emit(
    events: &tokio::sync::broadcast::Sender<SessionEventFrame>,
    next_seq: &AtomicU64,
    replay: &ReplayRing,
    event: SessionEvent,
) {
    // 无订阅者 = 无 SSE 连接;持久化数据不回滚,客户端经快照重同步。
    let frame = SessionEventFrame {
        seq: next_seq.fetch_add(1, Ordering::Relaxed),
        event,
    };
    // 定序纪律(H1'):**先入环、后广播**。与 `attach_after` 的「先 grab receiver、
    // 后读环」交错时,任一帧至少落在 replay 或 receiver 一处;两处重合同一帧时
    // 由转发侧水位去重。
    replay.push(frame.clone());
    let _ = events.send(frame);
}

fn emit_turn_ended(pump: &OwnerPump, turn_id: String, outcome: TurnOutcome) {
    pump.emit(SessionEvent::TurnEnded {
        turn_id: TurnId::new(turn_id),
        outcome,
    });
    pump.emit(SessionEvent::ActivityChanged {
        activity: qaqh_ringing_contract::fact::ActivityState::Idle,
        current_turn_id: None,
    });
}

/// typed 列的 serde 形式;序列化失败按代码错误响(不静默降级为 NULL)。
fn serialize_typed<T: serde::Serialize>(who: &str, label: &str, value: &T) -> Option<String> {
    serde_json::to_string(value)
        .map_err(|error| log::error!("[owner] {who} {label} not serializable: {error}"))
        .ok()
}

/// TimelineEntry → 业务行:kind/call_id 是查询用冗余列,与 wire 形式同写。
/// v4.5.1 任务2:**停写 `text` 冗余正文副本**(统一置 `None`)——生产读取
/// 全走 `payload_json`(view 层 `parse_block_entry` 唯一权威),整段正文再抄一份
/// 进 `text` 只是让 `qaqh.db` 无止境翻倍。`text` 列保留可空以维持 schema 13
/// 零迁移,严禁在此回填任何正文。
fn timeline_block(session_id: &str, entry: &TimelineEntry) -> Result<store::NewBlock, DbError> {
    let (block_id, turn_id, kind, call_id): (
        String,
        Option<String>,
        &'static str,
        Option<String>,
    ) = match entry {
        TimelineEntry::Input {
            block_id,
            turn_id,
            ..
        } => (
            block_id.as_ref().to_string(),
            turn_id.as_ref().map(|turn| turn.as_ref().to_string()),
            "input",
            None,
        ),
        TimelineEntry::AssistantBlock {
            block_id,
            turn_id,
            ..
        } => (
            block_id.as_ref().to_string(),
            Some(turn_id.as_ref().to_string()),
            "assistant",
            None,
        ),
        TimelineEntry::ToolCall { call_id, turn_id } => (
            // 块表主键全库唯一;调用身份本身仍只保存在 call_id 字段。
            format!("tool_call:{}:{}", session_id, call_id.as_ref()),
            turn_id.as_ref().map(|turn| turn.as_ref().to_string()),
            "tool_call",
            Some(call_id.as_ref().to_string()),
        ),
        TimelineEntry::ToolResult {
            call_id,
            turn_id,
            ..
        } => (
            // ToolCall 与 ToolResult 是两条独立的 RingingV2 timeline 事实,
            // 不能共用 block_id;blocks.block_id 是 PRIMARY KEY。
            format!("tool_result:{}:{}", session_id, call_id.as_ref()),
            turn_id.as_ref().map(|turn| turn.as_ref().to_string()),
            "tool_result",
            Some(call_id.as_ref().to_string()),
        ),
        TimelineEntry::Compaction { .. } => (
            // 压缩块没有独立 block_id,按时间派生保证多次压缩不冲突。
            format!("compaction-{}", store::now_ms()),
            None,
            "compaction",
            None,
        ),
    };
    let payload_json = serde_json::to_string(entry)
        .map_err(|error| DbError::Corrupt(format!("block {block_id}: {error}")))?;
    Ok(store::NewBlock {
        block_id,
        session_id: session_id.to_string(),
        turn_id,
        kind,
        // v4.5.1 任务2:text 冗余副本停写(schema 13 的可空列,零迁移保留)。
        text: None,
        call_id,
        payload_json,
    })
}

/// 工具终态:落库行与 ToolFinished 事件的共同来源。契约 outcome 是唯一
/// 终态权威;status/error 的分离权威字段已删除(spec §4.2)。
pub struct ToolSettlement {
    pub call_id: String,
    pub tool_name: String,
    pub outcome: ToolCallOutcome,
    pub output_text: Option<String>,
    pub output_truncated: bool,
    pub diff_text: Option<String>,
    pub metrics: Option<ToolMetrics>,
    pub code_delta: Option<CodeDelta>,
    pub effects: Vec<ToolEffectInfo>,
}

/// 执行会话终态载荷:状态、完整性信息与最终输出引用(spec §8.2/§9.1)。
pub struct ExecSessionSettlement {
    pub exec_session_id: String,
    /// 契约 `ExecSessionState::Finished{outcome}` 的 serde 形式。
    pub state_json: String,
    pub finished_at_ms: i64,
    pub output_complete: bool,
    /// 终态权威输出的 session_contents 引用(可空:无输出或外部化失败)。
    pub output_content_id: Option<String>,
    /// Captured head/tail transcript persisted atomically with the terminal row.
    pub output_text: Option<String>,
    pub output_truncated: bool,
    /// 有界尾窗摘要(≤16 KiB,UTF-8 边界)。
    pub output_preview: String,
    pub preview_start_offset: u64,
}

/// 一次工具执行的生命周期凭据:执行路径持有,终态时交还。
pub struct ToolRun {
    call_id: String,
    tool_name: String,
    exec_sender: ExecProgressSender,
    totals: ExecProgressTotals,
    started_at_ms: i64,
}

impl ToolRun {
    /// exec 处理器的直通进度 sender(E09/E10 断点的生产注入点)。
    pub fn exec_sender(&self) -> ExecProgressSender {
        self.exec_sender.clone()
    }

    /// 进度字节总量句柄:终态 metrics 里读 `progress_bytes_total`。
    pub fn progress_totals(&self) -> ExecProgressTotals {
        self.totals.clone()
    }

    pub fn started_at_ms(&self) -> i64 {
        self.started_at_ms
    }
}

async fn set_turn_phase_in_pump(
    pump: &OwnerPump,
    turn_id: String,
    phase: TurnPhase,
) -> Result<(), DbError> {
    let phase_text = match phase {
        TurnPhase::Running => "running",
        TurnPhase::WaitingUser => "waiting_user",
        TurnPhase::Cancelling => "cancelling",
    };
    let row_turn_id = turn_id.clone();
    let changed = pump
        .db
        .write(move |conn| store::set_turn_phase(conn, &row_turn_id, phase_text))
        .await?;
    if !changed {
        return Ok(());
    }
    let turn = TurnId::new(turn_id);
    pump.emit(SessionEvent::TurnPhaseChanged {
        turn_id: turn.clone(),
        phase,
    });
    let activity = match phase {
        TurnPhase::Running => qaqh_ringing_contract::fact::ActivityState::Running,
        TurnPhase::WaitingUser => qaqh_ringing_contract::fact::ActivityState::WaitingUser,
        TurnPhase::Cancelling => qaqh_ringing_contract::fact::ActivityState::Cancelling,
    };
    pump.emit(SessionEvent::ActivityChanged {
        activity,
        current_turn_id: Some(turn),
    });
    Ok(())
}

/// 执行路径侧同步句柄:非阻塞上报,owner 泵消费。
#[derive(Clone)]
pub struct ToolLifecycle {
    session_id: String,
    tx: mpsc::UnboundedSender<OwnerMsg>,
}

impl ToolLifecycle {
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// 工具进入实际执行:登记 running 行(即 tool_state_changed(running)),
    /// 返回终态凭据(含输出通道)。
    pub fn started(&self, call_id: &str, tool_name: &str, args_json: Option<String>) -> ToolRun {
        let msg_call_id = call_id.to_string();
        let run_call_id = call_id.to_string();
        let output_call_id = call_id.to_string();
        let msg_tool_name = tool_name.to_string();
        let tx = self.tx.clone();
        if tx
            .send(OwnerMsg::Request(Box::new(move |pump| {
                Box::pin(async move {
                    let turn_id = pump.active_turn_id().await;
                    let state = ToolCallState::Running;
                    let state_json = serde_json::to_string(&state)
                        .map_err(|error| DbError::Corrupt(format!("tool state: {error}")))?;
                    let upsert = store::ToolCallUpsert {
                        call_id: msg_call_id.clone(),
                        session_id: pump.session_id.clone(),
                        turn_id,
                        tool_name: msg_tool_name,
                        args_json,
                        state_json,
                        state_kind: "running",
                    };
                    pump.db
                        .write(move |conn| store::upsert_tool_call(conn, &upsert))
                        .await?;
                    let turn_id = pump.active_turn_id().await.map(TurnId::new);
                    if let Some(turn_id) = turn_id {
                        pump.emit(SessionEvent::ToolStateChanged {
                            call_id: ToolCallId::new(msg_call_id),
                            turn_id,
                            state: ToolCallState::Running,
                        });
                    }
                    Ok(())
                })
            })))
            .is_err()
        {
            log::error!("[owner] tool start dropped: pump stopped");
        }
        // 每次输出立即入 owner 队列,与 Started/Settle 共用顺序。
        // 无需第二个缓冲通道或等工具结束后才启动的转发线程。
        let queued = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let sender = ExecProgressSender::with_sink(move |event| {
            // 保留每个工具的有界有损进度预算,生命周期消息不受此预算影响。
            if queued
                .try_update(
                    std::sync::atomic::Ordering::Relaxed,
                    std::sync::atomic::Ordering::Relaxed,
                    |count| (count < EXEC_PROGRESS_CHANNEL_CAPACITY).then_some(count + 1),
                )
                .is_err()
            {
                return false;
            }
            let stream = match event.stream {
                ExecOutputStream::Stdout => OutputStream::Stdout,
                ExecOutputStream::Stderr => OutputStream::Stderr,
            };
            let accepted = tx
                .send(OwnerMsg::Output {
                    call_id: output_call_id.clone(),
                    stream,
                    text: event.chunk,
                    queued: Arc::clone(&queued),
                })
                .is_ok();
            if !accepted {
                queued.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            }
            accepted
        });
        let totals = sender.totals();
        ToolRun {
            call_id: run_call_id,
            tool_name: tool_name.to_string(),
            exec_sender: sender,
            totals,
            started_at_ms: store::now_ms(),
        }
    }

    /// 终态上报(工具已 Started)。
    ///
    /// 已上报输出先于终态进入同一队列;后台进程持有的 sender 仍可在终态后
    /// 上报输出,不必等待后台进程退出。
    pub fn finished(&self, run: ToolRun, settlement: ToolSettlement) {
        drop(run.exec_sender);
        self.send(OwnerMsg::Settle(Box::new(ToolSettlement {
            call_id: run.call_id,
            tool_name: run.tool_name,
            outcome: settlement.outcome,
            output_text: settlement.output_text,
            output_truncated: settlement.output_truncated,
            diff_text: settlement.diff_text,
            metrics: settlement.metrics,
            code_delta: settlement.code_delta,
            effects: settlement.effects,
        })));
    }

    /// 调度前终态(没有 Started 行):终态直落,upsert 建行。
    pub fn settled(
        &self,
        call_id: &str,
        tool_name: &str,
        args_json: Option<String>,
        settlement: ToolSettlement,
    ) {
        let _ = args_json;
        self.send(OwnerMsg::Settle(Box::new(ToolSettlement {
            call_id: call_id.to_string(),
            tool_name: tool_name.to_string(),
            outcome: settlement.outcome,
            output_text: settlement.output_text,
            output_truncated: settlement.output_truncated,
            diff_text: settlement.diff_text,
            metrics: settlement.metrics,
            code_delta: settlement.code_delta,
            effects: settlement.effects,
        })));
    }

    /// 执行会话建立或非终态迁移(exec_session_started/state_changed)。
    pub fn exec_session_upsert(&self, session: ExecSessionView) {
        self.send(OwnerMsg::ExecSessionUpsert(Box::new(session)));
    }

    /// 执行会话输出增量(采集端同锁定序后投递)。
    pub fn exec_session_output(
        &self,
        exec_session_id: &str,
        start_offset: u64,
        end_offset: u64,
        source: ExecOutputSource,
        text: String,
        next_offset: u64,
        output_preview: Arc<str>,
        preview_start_offset: u64,
        capture_omitted_bytes: u64,
        stream_dropped_bytes: u64,
    ) {
        self.send(OwnerMsg::ExecSessionOutput {
            exec_session_id: exec_session_id.to_string(),
            start_offset,
            end_offset,
            source,
            text,
            next_offset,
            output_preview,
            preview_start_offset,
            capture_omitted_bytes,
            stream_dropped_bytes,
        });
    }

    /// 执行会话终态:一次性落库 + exec_session_finished。
    pub fn exec_session_finished(&self, settlement: ExecSessionSettlement) {
        self.send(OwnerMsg::ExecSessionSettle(Box::new(settlement)));
    }

    fn send(&self, msg: OwnerMsg) {
        // 信箱无界且泵任务随会话存活;发送失败意味着进程正在退出。
        if let Err(error) = self.tx.send(msg) {
            log::error!("[owner] lifecycle msg dropped: {error}");
        }
    }
}

/// 业务顺序泵:单线程串行处理,DB 写与事件广播都在这里定序。
struct OwnerPump {
    session_id: String,
    db: Arc<Db>,
    events: tokio::sync::broadcast::Sender<SessionEventFrame>,
    next_seq: Arc<AtomicU64>,
    replay: Arc<ReplayRing>,
    /// v4.5.1 任务4:exec 进度 SQLite 落库节流闸(前端 20ms 合帧不变)。
    exec_progress: ExecProgressGate,
}

impl OwnerPump {
    fn emit(&self, event: SessionEvent) {
        emit(&self.events, &self.next_seq, &self.replay, event);
    }

    /// ADR-0030 决策 2：flush = 现单条语义原样放大到批，**三步顺序不变**
    /// （先 append 日志 → 再 update progress → 后 emit）。append 一次拼接写入；
    /// DB 写取批内末条的累计水位（单调状态，末条即全批末态）；emit 按 source
    /// run 切分，跨 run offset 连续。失败语义不变：append 失败响亮报错不阻断
    /// 实时流，DB 写不进去则整批不 emit（缺口由 settle 对账如实暴露）。
    ///
    /// 异步化只换载体,不改定序:日志 append 仍在 `spawn_blocking` 里**先**完成
    /// 并被 `.await` 等回来,才轮到 DB 水位,再 emit——ADR-0017 决策 1 的
    /// 「客户端见过 offset X 的 delta ⇒ 日志必可读到 X」逐步仍成立。
    async fn flush_exec_output(&self, batch: ExecOutputBatch) {
        let ExecOutputBatch {
            exec_session_id,
            entries,
            ..
        } = batch;
        if let Some(path) = exec_output_log_path(&self.db, &self.session_id, &exec_session_id) {
            let combined = entries
                .iter()
                .map(|entry| entry.text.as_str())
                .collect::<String>();
            let log_id = exec_session_id.clone();
            let appended = tokio::task::spawn_blocking(move || append_exec_log(&path, &combined))
                .await
                .map_err(|error| std::io::Error::other(format!("exec log task join: {error}")))
                .and_then(|result| result);
            if let Err(error) = appended {
                log::error!("[owner] exec log append {log_id}: {error}");
            }
        }
        let last = entries
            .last()
            .expect("批由首条 entry 建立，批内不可能为空");
        // v4.5.1 任务4:DB 进度落库按 exec_session_id 节流(≥500ms),日志追加与
        // 前端 emit 节奏不变(ADR-0030);节流/冲刷不变量收口在 exec_progress 子模块。
        let outcome = self
            .exec_progress
            .observe(
                &self.db,
                &exec_session_id,
                ExecProgressSnapshot::new(
                    last.next_offset,
                    Arc::clone(&last.output_preview),
                    last.preview_start_offset,
                    last.capture_omitted_bytes,
                    last.stream_dropped_bytes,
                ),
            )
            .await;
        if matches!(outcome, ProgressOutcome::Skipped | ProgressOutcome::Failed) {
            return;
        }
        let mut runs: Vec<(ExecOutputSource, u64, u64, String)> = Vec::new();
        for entry in entries {
            match runs.last_mut() {
                Some(run) if run.0 == entry.source && run.2 == entry.start_offset => {
                    run.2 = entry.end_offset;
                    run.3.push_str(&entry.text);
                }
                _ => runs.push((entry.source, entry.start_offset, entry.end_offset, entry.text)),
            }
        }
        for (source, start_offset, end_offset, text) in runs {
            self.emit(SessionEvent::ExecSessionOutputDelta {
                exec_session_id: qaqh_ringing_contract::fact::ExecSessionId::new(
                    exec_session_id.clone(),
                ),
                start_offset,
                end_offset,
                source,
                text,
            });
        }
    }

    /// 活跃回合:工具行落库时的回合归属。
    async fn active_turn_id(&self) -> Option<String> {
        let session_id = self.session_id.clone();
        match self
            .db
            .read(move |conn| store::active_turn_id(conn, &session_id))
            .await
        {
            Ok(turn_id) => turn_id,
            Err(error) => {
                log::error!("[owner] active turn lookup failed: {error}");
                None
            }
        }
    }

    /// 泵的单条消息处理。`self: &Arc<Self>` 而不是 `&self`:
    /// `OwnerMsg::Request` 要把 `Arc<OwnerPump>` 交给请求闭包,让它的 future
    /// 是 `'static`(见 [`OwnerRequest`])。其余分支照旧走 `&self` 解引用。
    async fn process(self: &Arc<Self>, msg: OwnerMsg) {
        match msg {
            OwnerMsg::Request(request) => {
                if let Err(error) = request(Arc::clone(self)).await {
                    log::error!("[owner] request failed: {error}");
                }
            }
            OwnerMsg::Output {
                call_id,
                stream,
                text,
                queued,
            } => {
                queued.fetch_sub(1, Ordering::Relaxed);
                // 无执行会话的普通工具输出;exec 输出经 ExecSessionOutput。
                self.emit(SessionEvent::ToolOutputDelta {
                    call_id: ToolCallId::new(call_id),
                    stream,
                    text,
                });
            }
            OwnerMsg::Settle(settlement) => {
                let turn_id = self.active_turn_id().await;
                let who = format!("tool {}", settlement.call_id);
                let state = ToolCallState::Finished {
                    outcome: settlement.outcome.clone(),
                };
                let state_json = match serde_json::to_string(&state) {
                    Ok(text) => text,
                    Err(error) => {
                        log::error!("[owner] {who} terminal state: {error}");
                        return;
                    }
                };
                // UI 投影外置(阈值见 TOOL_OUTPUT_INLINE_MAX_BYTES):大工具输出
                // 全文进 session_contents(sha256 为 id),行与事件只带 ref。
                // 这不影响模型环——模型回放读 messages 表的 ToolResult 块,
                // model.text 已被 tool_side_fold 裁剪;`tool_calls.output_text`
                // 纯是 UI 投影列,Inline→Ref 只换呈现形态,不改任何事实。
                // 收益:快照载荷、实时事件帧、回放环(REPLAY 侧 Ref 只计 64 字节)
                // 同时不再被数百 KB 正文压住。
                let externalized: Option<(String, String)> = settlement
                    .output_text
                    .as_ref()
                    .filter(|text| text.len() > TOOL_OUTPUT_INLINE_MAX_BYTES)
                    .map(|text| {
                        use sha2::Digest as _;
                        (hex::encode(sha2::Sha256::digest(text.as_bytes())), text.clone())
                    });
                let (output_text, output_ref) = match &externalized {
                    Some((content_id, _)) => (None, Some(content_id.clone())),
                    None => (settlement.output_text.clone(), None),
                };
                let externalized_diff: Option<(String, String)> = settlement
                    .diff_text
                    .as_ref()
                    .filter(|text| text.len() > TOOL_OUTPUT_INLINE_MAX_BYTES)
                    .map(|text| {
                        use sha2::Digest as _;
                        (hex::encode(sha2::Sha256::digest(text.as_bytes())), text.clone())
                    });
                let (diff_text, diff_ref) = match &externalized_diff {
                    Some((content_id, _)) => (None, Some(content_id.clone())),
                    None => (settlement.diff_text.clone(), None),
                };
                let finish = store::ToolCallFinish {
                    call_id: settlement.call_id.clone(),
                    session_id: self.session_id.clone(),
                    turn_id,
                    tool_name: settlement.tool_name,
                    state_json,
                    exec_session_id: settlement
                        .outcome
                        .exec_session_id()
                        .map(|id| id.as_str().to_string()),
                    output_text,
                    output_ref,
                    output_truncated: settlement.output_truncated,
                    diff_text,
                    diff_ref,
                    meta_json: settlement
                        .metrics
                        .as_ref()
                        .and_then(|m| serialize_typed(&who, "metrics", m)),
                    effects_json: (!settlement.effects.is_empty())
                        .then(|| serialize_typed(&who, "effects", &settlement.effects))
                        .flatten(),
                    code_delta_json: settlement
                        .code_delta
                        .as_ref()
                        .and_then(|d| serialize_typed(&who, "code_delta", d)),
                };
                let content_write = externalized.clone();
                let diff_write = externalized_diff.clone();
                let content_session_id = self.session_id.clone();
                if let Err(error) = self.db.write(move |conn| {
                    if let Some((content_id, text)) = &content_write {
                        store::put_session_content(
                            conn,
                            &content_session_id,
                            content_id,
                            "text/plain; charset=utf-8",
                            text.as_bytes(),
                            false,
                        )?;
                    }
                    if let Some((content_id, text)) = &diff_write {
                        store::put_session_content(
                            conn,
                            &content_session_id,
                            content_id,
                            "text/x-diff; charset=utf-8",
                            text.as_bytes(),
                            false,
                        )?;
                    }
                    store::finish_tool_call(conn, &finish)
                })
                .await
                {
                    log::error!(
                        "[owner] tool {} finish not recorded: {error}",
                        settlement.call_id
                    );
                    return;
                }
                self.emit(SessionEvent::ToolFinished {
                    call_id: ToolCallId::new(settlement.call_id),
                    outcome: settlement.outcome.clone(),
                    exec_session_id: settlement.outcome.exec_session_id().cloned(),
                    output: match &externalized {
                        Some((content_id, _)) => Some(ContentValue::Ref {
                            content_ref: ContentRef::new(ContentHash::new(
                                store::canonical_content_ref(content_id),
                            )),
                        }),
                        None => settlement
                            .output_text
                            .map(|text| ContentValue::Inline { text }),
                    },
                    output_truncated: settlement.output_truncated,
                    diff: match &externalized_diff {
                        Some((content_id, _)) => Some(ContentValue::Ref {
                            content_ref: ContentRef::new(ContentHash::new(
                                store::canonical_content_ref(content_id),
                            )),
                        }),
                        None => settlement
                            .diff_text
                            .map(|text| ContentValue::Inline { text }),
                    },
                    metrics: settlement.metrics,
                    code_delta: settlement.code_delta,
                    effects: settlement.effects,
                });
            }
            OwnerMsg::ExecSessionUpsert(session) => {
                let session_id = session.id.as_ref().to_string();
                let is_new = match self
                    .db
                    .read(move |conn| {
                        store::exec_session_by_id(conn, &session_id).map(|row| row.is_none())
                    })
                    .await
                {
                    Ok(is_new) => is_new,
                    Err(error) => {
                        log::error!("[owner] exec session {} lookup failed: {error}", session.id);
                        return;
                    }
                };
                let row = exec_session_row_from_view(&self.session_id, &session);
                if let Err(error) = self
                    .db
                    .write(move |conn| store::upsert_exec_session(conn, &row))
                    .await
                {
                    log::error!(
                        "[owner] exec session {} upsert not recorded: {error}",
                        session.id
                    );
                    return;
                }
                let event = if session.state.is_terminal() {
                    SessionEvent::ExecSessionFinished { session: *session }
                } else if is_new {
                    SessionEvent::ExecSessionStarted { session: *session }
                } else {
                    SessionEvent::ExecSessionStateChanged { session: *session }
                };
                // 会话建立必须先于其任何输出;started 事件在 upsert 路径发出。
                self.emit(event);
            }
            OwnerMsg::ExecSessionOutput { .. } => {
                unreachable!("ExecSessionOutput 已由 run_pump_loop 合帧消费（ADR-0030 决策 1）")
            }
            OwnerMsg::ExecSessionSettle(settlement) => {
                let exec_session_id = settlement.exec_session_id.clone();
                // v4.5.1 任务4 不变量3:settle 先冲刷该 exec_session_id 的待落库
                // 节流快照(终态水位的权威来源是行内 next_offset),再清理节流
                // 状态,由下方 settle 事务收口终态。
                self.exec_progress.drain_one(&self.db, &exec_session_id).await;
                let logged_bytes =
                    exec_output_log_path(&self.db, &self.session_id, &exec_session_id)
                        .and_then(|path| std::fs::metadata(&path).ok())
                        .map(|meta| meta.len())
                        .filter(|len| *len > 0);
                // ADR-0033（修订 ADR-0017 决策 3 的写入面）：全文权威源 = append-only
                // 日志文件本身，读取面由 `exec_session.output` 区间读承载（决策 2）。
                // settle **不读日志内容**：旧实现 read_to_string 全文 → sha256 →
                // `session_contents` BLOB 复制——同一批字节在洪峰时刻被读入内存两份
                // 并永久双存（文件 + SQLite 200MB 级重复）。对账只比「文件字节 vs 库
                // 水位」（metadata，O(1) 内存）：日志存在 ⇒ content 复制不再发生
                // （output_text=None），落盘缺口如实降级 output_complete（宪规 5，
                // 前端"仅显示尾部"横幅依此字段）。日志缺席（append 曾失败/旧会话）
                // 回退封顶 transcript（≤1MiB，原路径）。
                let (output_text, output_truncated, output_complete) = match logged_bytes {
                    Some(logged) => {
                        // `Db::read`/`Db::write` 的闭包要 `'static`,所以按引用
                        // 抓局部变量的写法一律换成先 clone 再 move 进闭包。
                        let water_id = exec_session_id.clone();
                        let water_line = self
                            .db
                            .read(move |conn| {
                                store::exec_session_by_id(conn, &water_id).map(|row| {
                                    row.map(|session_row| session_row.next_offset).unwrap_or(0)
                                })
                            })
                            .await
                            .unwrap_or(0);
                        let gap = logged < water_line;
                        (
                            None,
                            settlement.output_truncated || gap,
                            settlement.output_complete && !gap,
                        )
                    }
                    None => (
                        settlement.output_text.clone(),
                        settlement.output_truncated,
                        settlement.output_complete,
                    ),
                };
                let changed = self
                    .db
                    .write(move |conn| {
                        store::finish_exec_session(
                            conn,
                            &settlement.exec_session_id,
                            &settlement.state_json,
                            settlement.finished_at_ms,
                            output_complete,
                            settlement.output_content_id.as_deref(),
                            output_text.as_deref(),
                            output_truncated,
                            &settlement.output_preview,
                            settlement.preview_start_offset,
                        )
                    })
                    .await
                    .map_err(|error| error.to_string())
                    .unwrap_or_else(|error| {
                        log::error!(
                            "[owner] exec session {exec_session_id} finish failed: {error}"
                        );
                        false
                    });
                if !changed {
                    // 已终态:终态不可回退,迟到回调不再次提交另一种结论。
                    return;
                }
                // 终态视图从已提交行读取,不重新推断。
                let view_id = exec_session_id.clone();
                match self
                    .db
                    .read(move |conn| store::exec_session_by_id(conn, &view_id))
                    .await
                {
                    Ok(Some(row)) => match exec_session_view_from_row(&row) {
                        Ok(session) => self.emit(SessionEvent::ExecSessionFinished { session }),
                        Err(error) => {
                            log::error!("[owner] exec session final view: {error}");
                        }
                    },
                    Ok(None) => {
                        log::error!("[owner] exec session {exec_session_id} missing after settle");
                    }
                    Err(error) => {
                        log::error!("[owner] exec session final read: {error}");
                    }
                }
            }
        }
    }
}

/// 泵主循环：串行消费信箱；同一 exec 会话的 `ExecSessionOutput` 连批合帧
/// （ADR-0030 决策 1），其余消息一律先 flush 当前批再原样处理 ⇒ 全局处理
/// 顺序与到达顺序一致，settle 的日志对账永远看得到已 flush 的前缀。
///
/// **P1-2 只换载体,不动语义**（ADR-0030 决策 1/2 是冻结项）：
/// - `rx.recv()` → `rx.recv().await`；`rx.recv_timeout(dur)` →
///   `select!{ recv / sleep_until }`。两个版本都是「这段时间里要么收到消息,
///   要么窗口到点」,区别仅在于旧版把 OS 线程钉在超时上,新版把任务挂到定时器上；
/// - 判定顺序逐条照搬：**先查 deadline 是否已过（已过则先 flush 再取下一条,
///   绝不把新消息并进过期批）** → 才进入「消息 vs 窗口」的竞争；
/// - 两条阈值常量 `EXEC_OUTPUT_COALESCE_WINDOW`(20ms) /
///   `EXEC_OUTPUT_COALESCE_MAX_BYTES`(256KiB) 与「同 exec_session_id 才续批、
///   非 output 消息让位前先收尾」的分支结构一字未改。
async fn run_pump_loop(pump: Arc<OwnerPump>, mut rx: mpsc::UnboundedReceiver<OwnerMsg>) {
    let mut batch: Option<ExecOutputBatch> = None;
    loop {
        let msg = match batch.as_ref() {
            None => match rx.recv().await {
                Some(msg) => msg,
                None => return,
            },
            Some(opened) => {
                let now = tokio::time::Instant::now();
                if now >= opened.deadline {
                    let finished = batch.take().expect("match 刚确认批存在");
                    pump.flush_exec_output(finished).await;
                    continue;
                }
                // deadline 已在上面判过未过期,这里 `deadline` 恒在未来,
                // `sleep_until` 不会出现「已过期立即就绪」抢在消息前的失真。
                let deadline = opened.deadline;
                tokio::select! {
                    biased;
                    msg = rx.recv() => match msg {
                        Some(msg) => msg,
                        None => {
                            let finished = batch.take().expect("match 刚确认批存在");
                            pump.flush_exec_output(finished).await;
                            return;
                        }
                    },
                    _ = tokio::time::sleep_until(deadline) => {
                        let finished = batch.take().expect("match 刚确认批存在");
                        pump.flush_exec_output(finished).await;
                        continue;
                    }
                }
            }
        };
        let (exec_session_id, entry) = match msg {
            OwnerMsg::ExecSessionOutput {
                exec_session_id,
                start_offset,
                end_offset,
                source,
                text,
                next_offset,
                output_preview,
                preview_start_offset,
                capture_omitted_bytes,
                stream_dropped_bytes,
            } => (
                exec_session_id,
                ExecOutputEntry {
                    start_offset,
                    end_offset,
                    source,
                    text,
                    next_offset,
                    output_preview,
                    preview_start_offset,
                    capture_omitted_bytes,
                    stream_dropped_bytes,
                },
            ),
            other => {
                if let Some(finished) = batch.take() {
                    pump.flush_exec_output(finished).await;
                }
                pump.process(other).await;
                continue;
            }
        };
        let bytes = entry.text.len();
        let same = batch
            .as_ref()
            .is_some_and(|opened| opened.exec_session_id == exec_session_id);
        if same {
            let opened = batch.as_mut().expect("same 刚匹配到本批");
            opened.bytes += bytes;
            opened.entries.push(entry);
            if opened.bytes >= EXEC_OUTPUT_COALESCE_MAX_BYTES {
                let finished = batch.take().expect("same 刚匹配到本批");
                pump.flush_exec_output(finished).await;
            }
        } else {
            if let Some(finished) = batch.take() {
                pump.flush_exec_output(finished).await;
            }
            batch = Some(ExecOutputBatch {
                exec_session_id,
                entries: vec![entry],
                bytes,
                deadline: tokio::time::Instant::now() + EXEC_OUTPUT_COALESCE_WINDOW,
            });
        }
    }
}

/// exec 输出 append-only 日志路径(ADR-0017 决策 1):
/// `<data_dir>/sessions/<session_id>/exec/<exec_session_id>.log`。data_dir 由业务库
/// 路径派生(`qaqh.db` 的父目录);两类 id 都是内核生成的单段路径组件。
/// owner pump 写入与 runtime `exec_session.output` 区间读共用此定位。
pub fn exec_output_log_path(db: &Db, session_id: &str, exec_session_id: &str) -> Option<PathBuf> {
    let data_dir = db.path().parent()?;
    Some(
        data_dir
            .join("sessions")
            .join(session_id)
            .join("exec")
            .join(format!("{exec_session_id}.log")),
    )
}

/// 追加一块输出到日志。定序不变量(ADR-0017 决策 1):先写日志、再更新 DB 水位、
/// 再发 delta ⇒「客户端见过 offset X 的 delta ⇒ 日志必可以读到 X」。
/// 失败只响亮报错、不阻断实时流;完整性由 settle 侧「日志字节 vs 水位」对账
/// 如实标 `output_truncated`(宪规 5:不假装完整)。
pub(crate) fn append_exec_log(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(text.as_bytes())
}

/// 区间读 exec 输出日志(ADR-0017 决策 2)。读取上界钳制到 `watermark`(DB 行
/// `next_offset`:pump 先写文件后写库 ⇒ 库水位 ≤ 文件长度,永不读到半块);
/// 文件若短于水位(append 曾失败),按文件实有字节如实返回,游标停在实有末尾。
/// `from_offset` 必须落在 UTF-8 字符边界(正常客户端游标天然如此;错值当场报
/// `InvalidData`,不静默对齐——宪规 5)。切片终点落进多字节字符内部时向前取整
/// 到字符边界(≤ +3 字节,且不会越过 watermark:watermark 本身就是边界,字符
/// 不跨越它)。返回 `(next_offset, text)`;`text` 为空且游标未前进 ⇒ 已追平
/// 水位(或日志缺席,旧会话如此,如实返回空)。
pub fn read_exec_output_log_range(
    db: &Db,
    session_id: &str,
    exec_session_id: &str,
    watermark: u64,
    from_offset: u64,
    max_bytes: u64,
) -> std::io::Result<(u64, String)> {
    let end_cap = from_offset.saturating_add(max_bytes).min(watermark);
    if end_cap <= from_offset {
        return Ok((from_offset, String::new()));
    }
    let Some(path) = exec_output_log_path(db, session_id, exec_session_id) else {
        return Ok((from_offset, String::new()));
    };
    let file_len = match std::fs::metadata(&path) {
        Ok(meta) => meta.len(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((from_offset, String::new()));
        }
        Err(error) => return Err(error),
    };
    // 终点最多前探 3 字节补全被 max_bytes 切进的多字节字符;探查不越过水位。
    let read_end = end_cap.saturating_add(3).min(watermark).min(file_len);
    if from_offset >= read_end {
        return Ok((from_offset, String::new()));
    }
    let mut file = std::fs::File::open(&path)?;
    file.seek(SeekFrom::Start(from_offset))?;
    let mut bytes = vec![0u8; (read_end - from_offset) as usize];
    file.read_exact(&mut bytes)?;
    let text = String::from_utf8(bytes).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("exec output log {exec_session_id} is not valid UTF-8 at byte {from_offset}"),
        )
    })?;
    let want = (end_cap - from_offset) as usize;
    let take = if text.len() <= want {
        text.len()
    } else {
        let mut end = want;
        while !text.is_char_boundary(end) {
            end += 1;
        }
        end
    };
    Ok((from_offset + take as u64, text[..take].to_owned()))
}

/// ExecSessionView → 持久化行(创建/非终态迁移路径)。
fn exec_session_row_from_view(
    fallback_session_id: &str,
    session: &ExecSessionView,
) -> store::ExecSessionRow {
    let state_kind = match &session.state {
        qaqh_ringing_contract::state::ExecSessionState::Running => "running",
        qaqh_ringing_contract::state::ExecSessionState::Stopping { .. } => "stopping",
        qaqh_ringing_contract::state::ExecSessionState::Finished { .. } => "finished",
    };
    let owner = session.owner_session_id.as_ref();
    store::ExecSessionRow {
        exec_session_id: session.id.as_str().to_string(),
        session_id: if owner.is_empty() {
            fallback_session_id.to_string()
        } else {
            owner.to_string()
        },
        origin_turn_id: Some(session.origin_turn_id.as_ref().to_string()),
        origin_call_id: Some(session.origin_call_id.as_ref().to_string()),
        tty: session.tty,
        command: session.command.clone(),
        cwd: session.cwd.clone(),
        state_json: serde_json::to_string(&session.state)
            .unwrap_or_else(|_| "{\"kind\":\"running\"}".to_string()),
        state_kind: state_kind.to_string(),
        started_at_ms: session.started_at_ms,
        finished_at_ms: session.finished_at_ms,
        next_offset: session.next_offset,
        output_complete: session.output_complete,
        capture_omitted_bytes: session.capture_omitted_bytes,
        stream_dropped_bytes: session.stream_dropped_bytes,
        output_content_id: None,
        output_preview: session.output_preview.clone(),
        preview_start_offset: session.preview_start_offset,
    }
}

/// 持久化行 → ExecSessionView(终态发布路径;不重新推断状态)。
fn exec_session_view_from_row(row: &store::ExecSessionRow) -> Result<ExecSessionView, DbError> {
    let state: qaqh_ringing_contract::state::ExecSessionState =
        serde_json::from_str(&row.state_json).map_err(|error| {
            DbError::Corrupt(format!(
                "exec session {}: state: {error}",
                row.exec_session_id
            ))
        })?;
    // 库内 content_id 为裸 hex(schema 6→7 契约);wire ref 在此边界组装 `sha256:` 前缀。
    let output = row
        .output_content_id
        .clone()
        .map(|content_id| ContentValue::Ref {
            content_ref: qaqh_ringing_contract::fact::ContentRef::new(
                qaqh_ringing_contract::fact::ContentHash::new(store::canonical_content_ref(
                    &content_id,
                )),
            ),
        });
    Ok(ExecSessionView {
        id: qaqh_ringing_contract::fact::ExecSessionId::new(row.exec_session_id.clone()),
        owner_session_id: qaqh_ringing_contract::fact::SessionId::new(row.session_id.clone()),
        origin_turn_id: TurnId::new(row.origin_turn_id.clone().unwrap_or_default()),
        origin_call_id: ToolCallId::new(row.origin_call_id.clone().unwrap_or_default()),
        state,
        tty: row.tty,
        command: row.command.clone(),
        cwd: row.cwd.clone(),
        started_at_ms: row.started_at_ms,
        finished_at_ms: row.finished_at_ms,
        next_offset: row.next_offset,
        output_preview: row.output_preview.clone(),
        preview_start_offset: row.preview_start_offset,
        output_complete: row.output_complete,
        capture_omitted_bytes: row.capture_omitted_bytes,
        stream_dropped_bytes: row.stream_dropped_bytes,
        output,
    })
}

/// 会话 owner 目录:daemon 全局一份,每会话一个 owner。
pub struct OwnerDirectory {
    db: Arc<Db>,
    owners: Mutex<HashMap<String, Arc<SessionOwner>>>,
}

impl OwnerDirectory {
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<OwnerDirectory, DbError> {
        let db = Arc::new(Db::open(path)?);
        let recovery = db.write_blocking(store::recover_after_restart)?;
        if recovery.total() > 0 {
            log::info!(
                "[owner] recovered interrupted state after daemon restart: {} exec sessions, {} tool calls, {} turns, {} interactions",
                recovery.exec_sessions,
                recovery.tool_calls,
                recovery.turns,
                recovery.interactions,
            );
        }
        Ok(Self {
            db,
            owners: Mutex::new(HashMap::new()),
        })
    }

    pub fn db(&self) -> &Arc<Db> {
        &self.db
    }

    /// 会话身份生成（v4.4 P1-3 自 `SessionManager` 并入——身份与存在权威在
    /// SQLite，目录句柄只提供路径策略，业务行由调用方创建）。
    pub fn allocate_session(&self) -> String {
        format!("ses_{}", qaqh_ringing_contract::ulid::generate_ulid())
    }

    /// 业务会话行删除之后清理其资源文件（原 `SessionManager::delete`）。
    /// `data_dir` 恒取 `qaqh.db` 的父目录——与启动吸入同一空间口径。
    pub fn delete_session_resources(&self, session_id: &str) -> Result<(), String> {
        let mut components = std::path::Path::new(session_id).components();
        if !matches!(components.next(), Some(std::path::Component::Normal(_)))
            || components.next().is_some()
            || session_id.contains('\\')
        {
            return Err("session resource id must be one normal path component".into());
        }
        let data_dir = self.db.path().parent().unwrap_or(Path::new(".")).to_path_buf();
        for path in [
            data_dir.join("sessions").join(session_id),
            data_dir.join("workspace").join(session_id),
        ] {
            match std::fs::remove_dir_all(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(format!(
                        "remove session resources {}: {error}",
                        path.display()
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn delete_session_tree(&self, session_id: &str) -> Result<Vec<String>, DbError> {
        let root = session_id.to_string();
        let ids = self.db.write_blocking(move |conn| {
            let ids = store::delete_session_tree(conn, &root)?;
            // v4.5.1 任务3:删除会话树后封顶 -wal 并按碎片阈值回收主库空间
            // (小删除不达阈值自动跳过全库 VACUUM)。删除事务已提交,维护失败
            // 在 maintenance 内响亮记日志,不以维护错误掩盖已成功的删除。
            maintenance::checkpoint_after_delete(conn, &root);
            Ok::<Vec<String>, DbError>(ids)
        })?;
        let mut owners = self
            .owners
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for id in &ids {
            owners.remove(id);
        }
        Ok(ids)
    }

    /// 正常停机收口(修复单 §二.3.3):执行一次 `checkpoint_truncate` 封顶截断
    /// `-wal`。失败响亮记日志,不阻断停机——截断只是体积卫生,不是停机前提。
    pub fn shutdown_checkpoint(&self) {
        if let Err(error) = self.db.write_blocking(maintenance::checkpoint_truncate) {
            log::error!("[owner] shutdown wal checkpoint: {error}");
        }
    }

    pub fn get_or_create(&self, session_id: &str) -> Arc<SessionOwner> {
        let mut owners = self
            .owners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Arc::clone(
            owners
                .entry(session_id.to_string())
                .or_insert_with(|| Arc::new(SessionOwner::new(session_id, Arc::clone(&self.db)))),
        )
    }

    /// 闲置卸载回收（MUST 铁闸 3 / P1-2 验收「`unload_idle_sessions` 后任务句柄
    /// 立即 drop 回收」）：把 owner 的 Arc 从目录摘出交给调用方，**不碰业务行**
    /// ——与 [`Self::delete_session_tree`] 的删除语义正交。map 是 owner 的唯一
    /// 长持有者（泵任务持 `Arc<OwnerPump>`，`ToolLifecycle` 只克隆 sender），
    /// 调用方 drop 返回值即 `SessionOwner::drop` → `pump_task.abort()`，
    /// ReplayRing 随任务释放；Arc 的 Drop 一定发生在目录锁之外。
    /// 幂等：条目不存在返回 `None`，不是错误。
    pub fn take_session(&self, session_id: &str) -> Option<Arc<SessionOwner>> {
        let mut owners = self
            .owners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        owners.remove(session_id)
    }

    /// 建会话并返回其 owner:完整输入路径的第一站。
    pub async fn create_session(
        &self,
        session: store::NewSession,
    ) -> Result<Arc<SessionOwner>, DbError> {
        let session_id = session.session_id.clone();
        self.db
            .write(move |conn| store::create_session(conn, &session))
            .await?;
        Ok(self.get_or_create(&session_id))
    }

    /// Spawn 期登记(同步):行不存在则插入默认行,已存在则不动(幂等)。
    /// 根会话与子代理共用;子代理用 `parent_session_id` 落血缘,
    /// 已有血缘时不覆盖。
    pub fn ensure_session(
        &self,
        session_id: &str,
        parent_session_id: Option<&str>,
        ephemeral: bool,
    ) -> Result<(), DbError> {
        let session_id = session_id.to_string();
        let parent = parent_session_id.map(str::to_string);
        self.db.write_blocking(move |conn| {
            store::ensure_session(conn, &session_id, parent.as_deref(), ephemeral)
        })
    }

    /// 会话血缘(spawn 诊断/列表用)。
    pub fn session_parent(&self, session_id: &str) -> Result<Option<String>, DbError> {
        let session_id = session_id.to_string();
        self.db
            .read_blocking(move |conn| store::session_parent(conn, &session_id))
    }

    /// 子代理转为持久(reload/显式 persist 时把 ephemeral 落回 false)。
    pub fn set_ephemeral(&self, session_id: &str, ephemeral: bool) -> Result<(), DbError> {
        let owned = session_id.to_string();
        self.db.write_blocking(move |conn| {
            store::update_session_columns(
                conn,
                &owned,
                &store::SessionColumnPatch {
                    title: None,
                    title_source: None,
                    cwd: None,
                    model: None,
                    tool_mode: None,
                    model_selection: None,
                    custom_tools: None,
                    frozen_annotation: None,
                    archived: None,
                    ephemeral: Some(ephemeral),
                    expected_settings_revision: None,
                    increment_settings_revision: false,
                },
            )
            .map(|_| ())
        })
    }

    /// 直接子会话(team 花名册用)。
    pub fn session_children(&self, parent_session_id: &str) -> Result<Vec<String>, DbError> {
        let parent = parent_session_id.to_string();
        self.db
            .read_blocking(move |conn| store::child_session_ids(conn, &parent))
    }

    /// 运行状态读取(列表/诊断用,不建 owner):未决交互 = Waiting(优先,
    /// 不被 active turn 遮蔽),活动回合 phase = 对应状态,有回合历史 = Idle,
    /// 从未开过回合 = NeverStarted(spec §4.4)。
    pub fn session_run_state(&self, session_id: &str) -> Result<SessionRunState, DbError> {
        let session_id = session_id.to_string();
        self.db.read_blocking(move |conn| {
            if !store::pending_interactions(conn, &session_id)?.is_empty() {
                return Ok::<_, rusqlite::Error>(SessionRunState::Waiting);
            }
            match store::active_turn_phase(conn, &session_id)?.as_deref() {
                Some("waiting_user") => return Ok(SessionRunState::Waiting),
                Some("cancelling") => return Ok(SessionRunState::Cancelling),
                Some(_) => return Ok(SessionRunState::Running),
                None => {}
            }
            if store::turn_count(conn, &session_id)? > 0 {
                Ok(SessionRunState::Idle)
            } else {
                Ok(SessionRunState::NeverStarted)
            }
        })
    }
}

/// [`OwnerDirectory::session_run_state`] 的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionRunState {
    Running,
    Waiting,
    Cancelling,
    Idle,
    NeverStarted,
}
