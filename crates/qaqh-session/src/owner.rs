//! 会话 owner:单个会话的业务顺序与生命周期唯一决定者(架构报告一 §12.5-2)。
//!
//! - 所有持久化写经 [`Db`] 的串行写执行器落业务表;
//! - 所有业务变化以带 `seq` 的 [`SessionEventFrame`] 广播(单流 SSE 的唯一来源);
//! - 执行路径经 [`ToolLifecycle`] 非阻塞上报工具生命周期与结构化效果;
//!   实时输出走有界有损通道,绝不让工具执行等慢消费者。
//!
//! owner 自身是一个同步泵线程:不依赖 tokio 上下文,daemon、CLI、测试同构。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};

use qaqh_ringing_contract::events::{InteractionEndStatus, OutputStream, SessionEvent, SessionEventFrame};
use qaqh_ringing_contract::fact::{
    ContentValue, InteractionDecision, InteractionId, ToolCallId, ToolError, ToolMetrics, TurnId,
    TurnMode,
};
use qaqh_ringing_contract::state::{
    CodeDelta, InteractionView, SessionState, TimelineEntry, ToolCallStatus, ToolEffectInfo,
    TurnOutcome,
};
use qaqh_tool_core::{
    bounded_exec_progress_channel, ExecOutputStream, ExecProgressEvent, ExecProgressSender,
    ExecProgressTotals,
};

use crate::db::{store, view, Db, DbError};

/// 每会话 SSE 事件缓冲;落后客户端经快照 + `seq` 水位重新同步(报告一 §194)。
const EVENT_CHANNEL_CAPACITY: usize = 1024;

/// 会话 owner:回合、timeline、工具生命周期与交互的业务顺序决定者。
pub struct SessionOwner {
    session_id: String,
    db: Arc<Db>,
    events: tokio::sync::broadcast::Sender<SessionEventFrame>,
    next_seq: Arc<AtomicU64>,
    lifecycle_tx: mpsc::Sender<OwnerMsg>,
}

impl SessionOwner {
    /// 创建 owner 并启动业务顺序泵线程。
    pub fn new(session_id: impl Into<String>, db: Arc<Db>) -> SessionOwner {
        let session_id = session_id.into();
        let (events, _) = tokio::sync::broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let (lifecycle_tx, lifecycle_rx) = mpsc::channel();
        let next_seq = Arc::new(AtomicU64::new(0));
        let pump = OwnerPump {
            session_id: session_id.clone(),
            db: Arc::clone(&db),
            events: events.clone(),
            next_seq: Arc::clone(&next_seq),
        };
        std::thread::Builder::new()
            .name(format!("qaqh-owner-{session_id}"))
            .spawn(move || {
                while let Ok(msg) = lifecycle_rx.recv() {
                    pump.process(msg);
                }
            })
            .expect("spawn session owner thread");
        SessionOwner {
            session_id,
            db,
            events,
            next_seq,
            lifecycle_tx,
        }
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
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

    /// 当前事件水位:快照必须携带,客户端据此衔接 SSE。
    pub fn current_seq(&self) -> u64 {
        self.next_seq.load(Ordering::Relaxed)
    }

    /// SessionState 快照:业务表直查 + 内存水位。
    pub async fn snapshot(&self) -> Result<SessionState, DbError> {
        let session_id = self.session_id.clone();
        let mut state = self
            .db
            .read(move |conn| view::load_session_state(conn, &session_id))
            .await?;
        state.seq = self.current_seq();
        Ok(state)
    }

    /// 开回合:turn 行 + TurnStarted/ActivityChanged 事件。
    pub async fn start_turn(
        &self,
        turn_id: &str,
        input_id: Option<String>,
        mode: Option<TurnMode>,
    ) -> Result<(), DbError> {
        let turn_id = turn_id.to_string();
        let turn_for_event = turn_id.clone();
        let session_id = self.session_id.clone();
        let input_id_for_row = input_id.clone();
        let mode_json = mode
            .map(|mode| serde_json::to_string(&mode))
            .transpose()
            .map_err(|error| DbError::Corrupt(format!("turn {turn_id} mode: {error}")))?;
        self.db
            .write(move |conn| {
                store::start_turn(
                    conn,
                    &turn_id,
                    &session_id,
                    input_id_for_row.as_deref(),
                    mode_json.as_deref(),
                )
            })
            .await?;
        let turn = TurnId::new(turn_for_event);
        emit(
            &self.events,
            &self.next_seq,
            SessionEvent::TurnStarted {
                turn_id: turn.clone(),
                input_id: input_id.map(qaqh_ringing_contract::fact::InputId::new),
                mode,
            },
        );
        emit(
            &self.events,
            &self.next_seq,
            SessionEvent::ActivityChanged {
                activity: qaqh_ringing_contract::fact::ActivityState::Running,
                current_turn_id: Some(turn),
            },
        );
        Ok(())
    }

    /// 回合终态:TurnEnded + ActivityChanged(Idle)。
    pub async fn finish_turn(
        &self,
        turn_id: &str,
        status: &'static str,
        outcome: TurnOutcome,
    ) -> Result<(), DbError> {
        let outcome_json = serde_json::to_string(&outcome)
            .map_err(|error| DbError::Corrupt(format!("turn {turn_id} outcome: {error}")))?;
        let turn_id_owned = turn_id.to_string();
        self.db
            .write(move |conn| store::finish_turn(conn, &turn_id_owned, status, Some(&outcome_json)))
            .await?;
        let turn = TurnId::new(turn_id);
        emit(
            &self.events,
            &self.next_seq,
            SessionEvent::TurnEnded {
                turn_id: turn.clone(),
                outcome,
            },
        );
        emit(
            &self.events,
            &self.next_seq,
            SessionEvent::ActivityChanged {
                activity: qaqh_ringing_contract::fact::ActivityState::Idle,
                current_turn_id: Some(turn),
            },
        );
        Ok(())
    }

    /// 追加 timeline 块:业务表 + TimelineAppended 事件。
    pub async fn append_block(&self, entry: TimelineEntry) -> Result<(), DbError> {
        let block = timeline_block(&self.session_id, &entry)?;
        self.db.write(move |conn| store::append_block(conn, &block)).await?;
        emit(
            &self.events,
            &self.next_seq,
            SessionEvent::TimelineAppended { entry },
        );
        Ok(())
    }

    /// 交互升起:interaction 行 + InteractionRequested 事件。
    pub async fn interaction_requested(&self, interaction: InteractionView) -> Result<(), DbError> {
        let interaction_id = interaction.interaction_id.as_ref().to_string();
        let payload_json = serde_json::to_string(&interaction)
            .map_err(|error| DbError::Corrupt(format!("interaction {interaction_id}: {error}")))?;
        let kind = serde_json::to_string(&interaction.kind)
            .map_err(|error| DbError::Corrupt(format!("interaction {interaction_id} kind: {error}")))?;
        let row = store::NewInteraction {
            interaction_id: interaction_id.clone(),
            session_id: self.session_id.clone(),
            turn_id: interaction.turn_id.as_ref().map(|turn| turn.as_ref().to_string()),
            call_id: interaction.call_id.as_ref().map(|call| call.as_ref().to_string()),
            kind,
            payload_json,
        };
        self.db
            .write(move |conn| store::create_interaction(conn, &row))
            .await?;
        emit(
            &self.events,
            &self.next_seq,
            SessionEvent::InteractionRequested { interaction },
        );
        Ok(())
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
        let changed = self
            .db
            .write(move |conn| store::resolve_interaction(conn, &id, status_db, &decision_json))
            .await?;
        if changed {
            emit(
                &self.events,
                &self.next_seq,
                SessionEvent::InteractionResolved {
                    interaction_id: interaction_id.clone(),
                    decision,
                    status,
                },
            );
        }
        Ok(changed)
    }
}

/// 序号 + 广播:单流事件出口的唯一写入点。
fn emit(
    events: &tokio::sync::broadcast::Sender<SessionEventFrame>,
    next_seq: &AtomicU64,
    event: SessionEvent,
) {
    // 无订阅者 = 无 SSE 连接;持久化数据不回滚,客户端经快照重同步。
    let _ = events.send(SessionEventFrame {
        seq: next_seq.fetch_add(1, Ordering::Relaxed),
        event,
    });
}

/// typed 列的 serde 形式;序列化失败按代码错误响(不静默降级为 NULL)。
fn serialize_typed<T: serde::Serialize>(who: &str, label: &str, value: &T) -> Option<String> {
    serde_json::to_string(value)
        .map_err(|error| log::error!("[owner] {who} {label} not serializable: {error}"))
        .ok()
}

/// TimelineEntry → 业务行:kind/call_id/text 是查询用冗余列,与 wire 形式同写。
fn timeline_block(session_id: &str, entry: &TimelineEntry) -> Result<store::NewBlock, DbError> {
    let (block_id, turn_id, kind, call_id, text): (
        String,
        Option<String>,
        &'static str,
        Option<String>,
        Option<String>,
    ) = match entry {
        TimelineEntry::Input {
            block_id, turn_id, content, ..
        } => (
            block_id.as_ref().to_string(),
            turn_id.as_ref().map(|turn| turn.as_ref().to_string()),
            "input",
            None,
            inline_text(content),
        ),
        TimelineEntry::AssistantBlock {
            block_id, turn_id, content, ..
        } => (
            block_id.as_ref().to_string(),
            Some(turn_id.as_ref().to_string()),
            "assistant",
            None,
            inline_text(content),
        ),
        TimelineEntry::ToolCall { call_id, turn_id } => (
            // 工具调用的 timeline 锚点用 call_id 作块标识。
            call_id.as_ref().to_string(),
            turn_id.as_ref().map(|turn| turn.as_ref().to_string()),
            "tool_call",
            Some(call_id.as_ref().to_string()),
            None,
        ),
        TimelineEntry::Compaction { summary } => (
            // 压缩块没有独立 block_id,按时间派生保证多次压缩不冲突。
            format!("compaction-{}", store::now_ms()),
            None,
            "compaction",
            None,
            inline_text(summary),
        ),
    };
    let payload_json = serde_json::to_string(entry)
        .map_err(|error| DbError::Corrupt(format!("block {block_id}: {error}")))?;
    Ok(store::NewBlock {
        block_id,
        session_id: session_id.to_string(),
        turn_id,
        kind,
        text,
        call_id,
        payload_json,
    })
}

fn inline_text(content: &ContentValue) -> Option<String> {
    match content {
        ContentValue::Inline { text } => Some(text.clone()),
        _ => None,
    }
}

enum OwnerMsg {
    Started {
        call_id: String,
        tool_name: String,
        args_json: Option<String>,
    },
    Output {
        call_id: String,
        stream: OutputStream,
        text: String,
    },
    Settle(Box<ToolSettlement>),
}

/// 工具终态:落库行与 ToolFinished 事件的共同来源。
pub struct ToolSettlement {
    pub call_id: String,
    pub tool_name: String,
    pub args_json: Option<String>,
    pub terminal: ToolTerminal,
}

/// 工具调用终态载荷。
pub struct ToolTerminal {
    pub status: ToolCallStatus,
    pub output_text: Option<String>,
    pub output_truncated: bool,
    pub error: Option<ToolError>,
    pub metrics: Option<ToolMetrics>,
    pub code_delta: Option<CodeDelta>,
    pub effects: Vec<ToolEffectInfo>,
}

impl ToolTerminal {
    fn status_db(&self) -> &'static str {
        match self.status {
            ToolCallStatus::Finished => "finished",
            ToolCallStatus::Failed => "failed",
            ToolCallStatus::Cancelled => "cancelled",
            // 后台进程仍在跑:调用已结清,状态保持运行中(view 映射 Running)。
            ToolCallStatus::Running => "backgrounded",
            other => unreachable!("non-terminal tool status {other:?} cannot settle a call"),
        }
    }
}

/// 一次工具执行的生命周期凭据:执行路径持有,终态时交还。
pub struct ToolRun {
    call_id: String,
    tool_name: String,
    exec_sender: ExecProgressSender,
    rx: Option<std::sync::mpsc::Receiver<ExecProgressEvent>>,
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

/// 执行路径侧同步句柄:非阻塞上报,owner 泵消费。
#[derive(Clone)]
pub struct ToolLifecycle {
    session_id: String,
    tx: mpsc::Sender<OwnerMsg>,
}

impl ToolLifecycle {
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// 工具进入实际执行:登记 running 行,返回终态凭据(含输出通道)。
    pub fn started(&self, call_id: &str, tool_name: &str, args_json: Option<String>) -> ToolRun {
        let (sender, rx) = bounded_exec_progress_channel();
        self.send(OwnerMsg::Started {
            call_id: call_id.to_string(),
            tool_name: tool_name.to_string(),
            args_json,
        });
        let totals = sender.totals();
        ToolRun {
            call_id: call_id.to_string(),
            tool_name: tool_name.to_string(),
            exec_sender: sender,
            rx: Some(rx),
            totals,
            started_at_ms: store::now_ms(),
        }
    }

    /// 终态上报(工具已 Started)。
    ///
    /// 同线程先清空已断开输出通道的余量,再上终态——ToolFinished 不得越过
    /// 同一调用的 ToolOutputDelta。通道未断开 = 后台进程仍在输出,转交
    /// 转发线程继续流式(终态之后的后台输出对客户端是合法增量)。
    pub fn finished(&self, mut run: ToolRun, terminal: ToolTerminal) {
        drop(run.exec_sender);
        if let Some(rx) = run.rx.take() {
            loop {
                match rx.try_recv() {
                    Ok(event) => {
                        self.forward_output(&run.call_id, event);
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        let tx = self.tx.clone();
                        let call_id = run.call_id.clone();
                        std::thread::Builder::new()
                            .name("qaqh-tool-output".into())
                            .spawn(move || {
                                while let Ok(event) = rx.recv() {
                                    let stream = match event.stream {
                                        ExecOutputStream::Stdout => OutputStream::Stdout,
                                        ExecOutputStream::Stderr => OutputStream::Stderr,
                                    };
                                    if tx
                                        .send(OwnerMsg::Output {
                                            call_id: call_id.clone(),
                                            stream,
                                            text: event.chunk,
                                        })
                                        .is_err()
                                    {
                                        break;
                                    }
                                }
                            })
                            .expect("spawn tool output forwarder");
                        break;
                    }
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
                }
            }
        }
        self.send(OwnerMsg::Settle(Box::new(ToolSettlement {
            call_id: run.call_id,
            tool_name: run.tool_name,
            args_json: None,
            terminal,
        })));
    }

    fn forward_output(&self, call_id: &str, event: ExecProgressEvent) {
        let stream = match event.stream {
            ExecOutputStream::Stdout => OutputStream::Stdout,
            ExecOutputStream::Stderr => OutputStream::Stderr,
        };
        self.send(OwnerMsg::Output {
            call_id: call_id.to_string(),
            stream,
            text: event.chunk,
        });
    }

    /// 调度前终态(没有 Started 行):终态直落,upsert 建行。
    pub fn settled(
        &self,
        call_id: &str,
        tool_name: &str,
        args_json: Option<String>,
        terminal: ToolTerminal,
    ) {
        self.send(OwnerMsg::Settle(Box::new(ToolSettlement {
            call_id: call_id.to_string(),
            tool_name: tool_name.to_string(),
            args_json,
            terminal,
        })));
    }

    fn send(&self, msg: OwnerMsg) {
        // 通道无界且泵线程随会话存活;发送失败意味着进程正在退出。
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
}

impl OwnerPump {
    fn emit(&self, event: SessionEvent) {
        emit(&self.events, &self.next_seq, event);
    }

    /// 活跃回合:工具行落库时的回合归属。
    fn active_turn_id(&self) -> Option<String> {
        let session_id = self.session_id.clone();
        match self
            .db
            .read_blocking(move |conn| store::active_turn_id(conn, &session_id))
        {
            Ok(turn_id) => turn_id,
            Err(error) => {
                log::error!("[owner] active turn lookup failed: {error}");
                None
            }
        }
    }

    fn process(&self, msg: OwnerMsg) {
        match msg {
            OwnerMsg::Started {
                call_id,
                tool_name,
                args_json,
            } => {
                let turn_id = self.active_turn_id();
                let upsert = store::ToolCallUpsert {
                    call_id: call_id.clone(),
                    session_id: self.session_id.clone(),
                    turn_id,
                    tool_name,
                    args_json,
                    status: "running",
                };
                if let Err(error) =
                    self.db.write_blocking(move |conn| store::upsert_tool_call(conn, &upsert))
                {
                    log::error!("[owner] tool {call_id} start not recorded: {error}");
                    return;
                }
                self.emit(SessionEvent::ToolStarted {
                    call_id: ToolCallId::new(call_id),
                });
            }
            OwnerMsg::Output {
                call_id,
                stream,
                text,
            } => {
                self.emit(SessionEvent::ToolOutputDelta {
                    call_id: ToolCallId::new(call_id),
                    stream,
                    text,
                });
            }
            OwnerMsg::Settle(settlement) => {
                let turn_id = self.active_turn_id();
                let who = format!("tool {}", settlement.call_id);
                let finish = store::ToolCallFinish {
                    call_id: settlement.call_id.clone(),
                    session_id: self.session_id.clone(),
                    turn_id,
                    tool_name: settlement.tool_name,
                    status: settlement.terminal.status_db(),
                    output_text: settlement.terminal.output_text.clone(),
                    output_ref: None,
                    output_truncated: settlement.terminal.output_truncated,
                    error_json: settlement
                        .terminal
                        .error
                        .as_ref()
                        .and_then(|e| serialize_typed(&who, "error", e)),
                    meta_json: settlement
                        .terminal
                        .metrics
                        .as_ref()
                        .and_then(|m| serialize_typed(&who, "metrics", m)),
                    effects_json: (!settlement.terminal.effects.is_empty())
                        .then(|| serialize_typed(&who, "effects", &settlement.terminal.effects))
                        .flatten(),
                    code_delta_json: settlement
                        .terminal
                        .code_delta
                        .as_ref()
                        .and_then(|d| serialize_typed(&who, "code_delta", d)),
                };
                if let Err(error) =
                    self.db.write_blocking(move |conn| store::finish_tool_call(conn, &finish))
                {
                    log::error!("[owner] tool {} finish not recorded: {error}", settlement.call_id);
                    return;
                }
                self.emit(SessionEvent::ToolFinished {
                    call_id: ToolCallId::new(settlement.call_id),
                    status: settlement.terminal.status,
                    output: settlement
                        .terminal
                        .output_text
                        .map(|text| ContentValue::Inline { text }),
                    output_truncated: settlement.terminal.output_truncated,
                    error: settlement.terminal.error,
                    metrics: settlement.terminal.metrics,
                    code_delta: settlement.terminal.code_delta,
                    effects: settlement.terminal.effects,
                });
            }
        }
    }
}

/// 会话 owner 目录:daemon 全局一份,每会话一个 owner。
pub struct OwnerDirectory {
    db: Arc<Db>,
    owners: Mutex<HashMap<String, Arc<SessionOwner>>>,
}

impl OwnerDirectory {
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<OwnerDirectory, DbError> {
        Ok(Self {
            db: Arc::new(Db::open(path)?),
            owners: Mutex::new(HashMap::new()),
        })
    }

    pub fn db(&self) -> &Arc<Db> {
        &self.db
    }

    pub fn get_or_create(&self, session_id: &str) -> Arc<SessionOwner> {
        let mut owners = self.owners.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        Arc::clone(
            owners
                .entry(session_id.to_string())
                .or_insert_with(|| Arc::new(SessionOwner::new(session_id, Arc::clone(&self.db)))),
        )
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
}
