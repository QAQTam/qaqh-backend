use std::mem::size_of;
use std::path::Path;

use crate::effect::{PendingTool, PersistOp};
use qaqh_types::Message;

/// Prefix of the synthetic result `from_messages` injects for an orphan
/// tool_use (assistant ToolUse without a persisted ToolResult — crash or kill
/// between the tool call and the next flush). The outbox reconciliation uses
/// it to recognize (and refine) these placeholders.

/// 取消路径为未执行 tool_use 补的终态文案（与
/// `engine_turn`/`admit` 的取消收割保持同一措辞，模型侧可识别）。

/// 压缩摘要是归档里的一条普通 `user` 消息，但读侧必须把它与真实用户回合区分开：
/// 模型面只保留最新摘要，人类 transcript 则完全跳过它。
pub fn is_compaction_summary(message: &Message) -> bool {
    message.role == "user"
        && message.content.iter().any(|block| {
            matches!(
                block,
                qaqh_types::ContentBlock::Text { text } if text.starts_with("[Compacted ")
            )
        })
}

/// 一次物理压缩的实际效果。
///
/// 调用方必须用它判定终态，不能用「turn 数变没变」推断：压实一个旧 turn
/// 的同时会插入一条摘要 turn，两者相消后 `turn_count()` 不变，但那是一次
/// 成功的压缩；反之只刷新检查点也可能让上下文凭空多出一段摘要。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CompactionEffect {
    /// 被折叠进摘要的**真实** turn 数（不含上一份摘要占位）。
    pub folded_turns: usize,
    /// 是否写入了新的摘要 turn（false = 上下文一字未动）。
    pub checkpoint_written: bool,
}

impl CompactionEffect {
    /// 压缩是否真的改变了活跃上下文。
    pub fn applied(self) -> bool {
        self.checkpoint_written
    }
}

// Tool results are finalized exactly once, at storage time — but the shaping
// itself (truncation / folding) now happens at the TOOL side
// (`qaqh-workspace::tool_side_fold`) before results reach this store. The
// stored message therefore IS the final form: the same bytes are rendered on
// every subsequent request, keeping KV-cache prefixes stable across rounds
// and turns.

#[derive(Debug, Clone)]
pub struct Step {
    pub assistant: Message,
    pub tool_results: Vec<Message>,
}

impl Step {
    pub fn new(assistant: Message) -> Self {
        Self {
            assistant,
            tool_results: Vec::new(),
        }
    }

    pub fn assistant_tool_ids(&self) -> Vec<String> {
        self.assistant
            .content
            .iter()
            .filter_map(|b| {
                if let qaqh_types::ContentBlock::ToolUse { id, .. } = b {
                    Some(id.clone())
                } else {
                    None
                }
            })
            .collect()
    }

    pub fn tool_result_has_id(&self, id: &str) -> bool {
        self.tool_results.iter().any(|tr| tr.content.iter().any(|b| matches!(b, qaqh_types::ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == id)))
    }

    pub fn has_tool_call(&self, id: &str) -> bool {
        self.assistant_tool_ids().iter().any(|tid| tid == id)
    }

    pub fn all_tools_satisfied(&self) -> bool {
        let ids = self.assistant_tool_ids();
        if ids.is_empty() {
            return true;
        }
        ids.iter().all(|id| self.tool_result_has_id(id))
    }

    pub fn pending_tools(&self) -> Vec<PendingTool> {
        self.assistant
            .content
            .iter()
            .filter_map(|b| {
                if let qaqh_types::ContentBlock::ToolUse { id, name, input } = b {
                    Some(PendingTool {
                        id: id.clone(),
                        name: name.clone(),
                        args: input.clone(),
                    })
                } else {
                    None
                }
            })
            .collect()
    }

    fn has_tool_use(&self) -> bool {
        self.assistant
            .content
            .iter()
            .any(|b| matches!(b, qaqh_types::ContentBlock::ToolUse { .. }))
    }
}

#[derive(Debug, Clone)]
pub struct Turn {
    pub user: Message,
    pub steps: Vec<Step>,
}

/// 一个工具调用的结果快照（`MessageStore::last_step_tool_results` 的产物）。
///
/// 携带工具侧五态 [`qaqh_types::ToolStatus`]（含 Cancelled/Backgrounded）
/// 与展示面 diff——timeline 投影（qaqh-runtime backfill）据此区分
/// succeeded / failed / cancelled / backgrounded，不再经布尔坍缩。
#[derive(Debug, Clone)]
pub struct StepToolResult {
    pub tool_call_id: String,
    pub tool_name: String,
    pub result: qaqh_types::ToolResult,
}

impl Turn {
    pub fn new(user: Message) -> Self {
        Self {
            user,
            steps: Vec::new(),
        }
    }

    pub fn find_step_for_mut(&mut self, tool_call_id: &str) -> Option<&mut Step> {
        self.steps
            .iter_mut()
            .find(|s| s.has_tool_call(tool_call_id))
    }
}

#[allow(clippy::type_complexity)]
pub struct MessageStore {
    session_id: String,
    system_messages: Vec<Message>,
    /// Trailing injections (persisted): subagent reports, skills envelopes.
    /// Written exactly once at arrival; never re-injected, never removed while
    /// they are in the live context (undo=Keep). Once the model consumes an
    /// injection it becomes ordinary history — later turns/steps append after
    /// it. On compact, injections in the folded region are folded with their
    /// turns (their text is already in the summary input). Serialized into the
    /// model context in WRITE order together with turns (see
    /// [`Self::flat_in_write_order`]), so an injection keeps the position it
    /// had when it was written — later turns and steps append AFTER it, and it
    /// becomes normal history instead of being re-presented as a fresh user
    /// message on every request.
    trailing_messages: Vec<Message>,
    /// G3：最后 step 有未满足 tool_use 时延后的 trailing 注入。进程内
    /// 缓冲（不持久化），由 flush_deferred_trailing 在安全点回灌。
    deferred_trailing: Vec<Message>,
    /// G5：被丢弃的孤儿 tool_result 的 call_id，宿主统一消费上报。
    orphan_tool_results: Vec<String>,
    turns: Vec<Turn>,
    cancelled: bool,
    /// Number of earliest turns that have been compacted (skipped in LLM context).
    /// Kept only for legacy logical-compaction replay; physical compaction
    /// appends a summary and resets this to zero.
    compact_skip: usize,
    /// Highest archived msg_id covered by the latest compaction summary.
    /// The active view is derived from `messages.jsonl` using this watermark;
    /// there is no separate compact-context file.
    compact_covered_through_msg_id: Option<u64>,
    /// Next message ID to assign (monotonic per session).
    next_msg_id: u64,
    /// Next externally visible turn sequence. Unlike `turns.len()`, this never
    /// moves backwards when old turns are replaced by a compact checkpoint.
    next_turn_seq: u64,
    /// Monotonic in-memory context generation. Background compaction captures
    /// this value and must only apply its result while the generation matches.
    context_revision: u64,
    /// If true, save_msg is a no-op — used during from_messages replay.
    replaying: bool,
    /// Messages assigned msg_id but not yet flushed to disk.
    pending_save: Vec<Message>,
    /// Queued disk writes awaiting host-side execution (PR-1-6). The host
    /// drains via [`Self::take_persist_ops`] after each command dispatch and
    /// replays the ops against the injected session manager, preserving the
    /// old synchronous write order byte-for-byte (Z5).
    pending_persist: Vec<PersistOp>,
    /// If true, skip all disk persistence. Used by subagents (disposable workers).
    ephemeral: bool,
    /// L2 WAL (optional, host opt-in via [`Self::enable_wal`]): logs
    /// message-bearing persist ops at enqueue time so a process death between
    /// `flush_meta` and the host-side drain loses nothing. `None` for
    /// ephemeral stores and clones (background compaction snapshots).
    wal: Option<crate::wal::WalWriter>,
    /// 最近一次 flush_meta 的 (model, effort)——BUG-2026-09-13-04 防御分支
    /// 发 SaveFull 时复用，避免把 meta.model 回写为空串。克隆店（后台压缩
    /// 快照）继承该值。
    last_flush_model: (String, Option<String>),
}

/// Allocation-free payload/heap estimate for one live message store.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MessageStoreMemoryUsage {
    pub message_count: u64,
    pub turn_count: u64,
    pub content_block_count: u64,
    pub pending_persist_ops: u64,
    pub payload_bytes: u64,
    pub text_bytes: u64,
    pub image_bytes: u64,
    pub heap_estimate_bytes: u64,
}

impl MessageStoreMemoryUsage {
    fn add_message(&mut self, message: &Message) {
        let item = message.memory_usage_estimate();
        self.message_count = self.message_count.saturating_add(1);
        self.content_block_count = self
            .content_block_count
            .saturating_add(message.content.len() as u64);
        self.payload_bytes = self.payload_bytes.saturating_add(item.payload_bytes);
        self.text_bytes = self.text_bytes.saturating_add(item.text_bytes);
        self.image_bytes = self.image_bytes.saturating_add(item.image_bytes);
        self.heap_estimate_bytes = self
            .heap_estimate_bytes
            .saturating_add(item.heap_estimate_bytes);
    }
}

impl std::fmt::Debug for MessageStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MessageStore")
            .field("seed", &self.session_id)
            .field("turns", &self.turns.len())
            .field("cancelled", &self.cancelled)
            .field("compact_skip", &self.compact_skip)
            .field(
                "compact_covered_through_msg_id",
                &self.compact_covered_through_msg_id,
            )
            .field("next_msg_id", &self.next_msg_id)
            .finish()
    }
}

impl Clone for MessageStore {
    fn clone(&self) -> Self {
        Self {
            session_id: self.session_id.clone(),
            system_messages: self.system_messages.clone(),
            trailing_messages: self.trailing_messages.clone(),
            deferred_trailing: Vec::new(),
            orphan_tool_results: self.orphan_tool_results.clone(),
            turns: self.turns.clone(),
            cancelled: self.cancelled,
            compact_skip: self.compact_skip,
            compact_covered_through_msg_id: self.compact_covered_through_msg_id,
            next_msg_id: self.next_msg_id,
            next_turn_seq: self.next_turn_seq,
            context_revision: self.context_revision,
            replaying: false,
            pending_save: Vec::new(),
            pending_persist: Vec::new(),
            ephemeral: self.ephemeral,
            wal: None,
            last_flush_model: self.last_flush_model.clone(),
        }
    }
}

/// 注入型 user 消息的来源标记（`name` 字段）。
///
/// 这些消息 role=user 但**不是真实用户轮次**：不建回合、不触发 timeline、
/// 不算 turn-start，崩溃/重启恢复时必须回到 trailing 而不是 push_user。
/// 新增 ContextSource（User role + Trailing sink）时在这里登记它的 name。
///
/// 背景：此前四处判定各自硬编码 "subagent" 字面量，任何新注入源都会在
/// 恢复路径被误认成真实用户输入（workspace 变更审计注入即为此而登记）。
const USER_INJECTION_SOURCES: &[&str] = &["subagent", "workspace"];

/// 判定注入型 user 消息。text 用于旧档前缀回退（早期 subagent 注入只靠
/// [SUBAGENT 标签、没有 name 字段）。
fn is_user_injection(msg: &Message, text: &str) -> bool {
    msg.role == "user"
        && (msg
            .name
            .as_deref()
            .is_some_and(|name| USER_INJECTION_SOURCES.contains(&name))
            || text.starts_with("[SUBAGENT "))
}
impl MessageStore {
    pub fn new(session_id: &str) -> Self {
        Self {
            session_id: session_id.to_string(),
            system_messages: Vec::new(),
            trailing_messages: Vec::new(),
            deferred_trailing: Vec::new(),
            orphan_tool_results: Vec::new(),
            turns: Vec::new(),
            cancelled: false,
            compact_skip: 0,
            compact_covered_through_msg_id: None,
            next_msg_id: 1,
            next_turn_seq: 1,
            context_revision: 0,
            replaying: false,
            pending_save: Vec::new(),
            pending_persist: Vec::new(),
            ephemeral: false,
            wal: None,
            last_flush_model: (String::new(), None),
        }
    }

    /// Create a MessageStore that never persists to disk (subagent / disposable worker).
    pub fn new_ephemeral(session_id: &str) -> Self {
        let mut s = Self::new(session_id);
        s.ephemeral = true;
        s
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn context_revision(&self) -> u64 {
        self.context_revision
    }

    pub fn cancel(&mut self) {
        self.cancelled = true;
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled
    }

    /// Assign msg_id and buffer for batched persistence.
    /// Flushed to disk via [`flush_meta`].
    /// No-op in ephemeral mode (returns None).
    ///
    /// Returns the assigned write-order id so callers can stamp the in-memory
    /// message: `msg_id` doubles as the store's write sequence, which
    /// [`Self::flat_in_write_order`] uses to merge turns and trailing
    /// injections in arrival order.
    fn save_msg(&mut self, msg: &Message) -> Option<u64> {
        if self.ephemeral {
            return None;
        }
        let mut m = msg.clone();
        m.msg_id = Some(self.next_msg_id);
        self.next_msg_id += 1;
        let id = m.msg_id;
        if !self.replaying {
            self.pending_save.push(m);
        }
        id
    }

    /// Write buffered messages to JSONL, then update meta.json + index.
    /// No-op if the session seed has not been initialized yet (empty seed),
    /// or if ephemeral mode is enabled.
    ///
    /// PR-1-6: the disk writes are enqueued as [`PersistOp`]s instead of
    /// being executed here; the host drains and replays them through the
    /// injected session manager in enqueue order (byte-identical to the old
    /// synchronous write order).
    pub fn flush_meta(&mut self, model: &str, effort: &str) {
        if self.session_id.is_empty() || self.ephemeral {
            return;
        }
        // BUG-2026-09-13-04：记住最近一次 flush 的身份字段，供后续防御性
        // SaveFull 重写复用（避免把 meta.model 回写为空串）。
        self.last_flush_model = (model.to_string(), Some(effort.to_string()));
        let turn_count = self.turns.len();
        let mut ops: Vec<PersistOp> = Vec::new();
        if !self.pending_save.is_empty() {
            let batch = std::mem::take(&mut self.pending_save);
            ops.push(PersistOp::Append {
                session_id: self.session_id.clone(),
                messages: batch,
                model: model.to_string(),
                effort: Some(effort.to_string()),
                compact_skip: self.persisted_compact_skip(),
                compact_covered_through_msg_id: self.compact_covered_through_msg_id,
                turn_count,
            });
        } else {
            ops.push(PersistOp::UpdateMeta {
                session_id: self.session_id.clone(),
                model: model.to_string(),
                effort: Some(effort.to_string()),
                compact_skip: self.persisted_compact_skip(),
                turn_count,
            });
        }
        // L2 WAL: log message-bearing ops BEFORE they enter the drain queue,
        // so a process death between this flush and the host-side drain loses
        // nothing (recovery replays the log; SaveFull is deliberately not
        // logged — see crate::wal module docs). fsync policy is round-boundary:
        // flushes carrying Appends happen at lap boundaries, so syncing here
        // bounds the power-loss window to one round while keeping meta-only
        // flushes cheap.
        if let Some(wal) = &mut self.wal {
            let mut logged_message_op = false;
            for op in &ops {
                match op {
                    PersistOp::Append { .. } => logged_message_op = true,
                    PersistOp::UpdateMeta { .. } => {}
                    // Derived checkpoints are regenerated after recovery.
                    _ => continue,
                }
                if let Err(error) = wal.log_op(op) {
                    log::error!(
                        "MessageStore: WAL log_op failed for {}: {error}",
                        self.session_id
                    );
                }
            }
            if logged_message_op && let Err(error) = wal.sync() {
                log::error!(
                    "MessageStore: WAL sync failed for {}: {error}",
                    self.session_id
                );
            }
        }
        self.pending_persist.extend(ops);
    }

    /// Opt in to enqueue-time WAL persistence for this store. `session_dir`
    /// is the session's on-disk directory (the WAL file lives next to
    /// `messages.jsonl`). No-op for ephemeral stores and double-enable.
    pub fn enable_wal(&mut self, session_dir: &Path) {
        if self.ephemeral || self.wal.is_some() {
            return;
        }
        match crate::wal::WalWriter::open(session_dir) {
            Ok(writer) => self.wal = Some(writer),
            Err(error) => {
                log::error!(
                    "MessageStore: WAL open failed for {}: {error}",
                    self.session_id
                )
            }
        }
    }

    /// 持久化到 meta 的 legacy compact_skip。物理压缩会把被压缩 turn 从活跃
    /// 视图移除并写入新的摘要水位，因此新链路下该值恒为 0。
    fn persisted_compact_skip(&self) -> usize {
        self.compact_skip
    }

    /// Take the queued persistence ops for host-side execution. The queue is
    /// this store's host-facing turn-completion surface (PR-1-6): drain it after each
    /// command dispatch and replay the ops in order against the injected
    /// session manager.
    pub fn take_persist_ops(&mut self) -> Vec<PersistOp> {
        std::mem::take(&mut self.pending_persist)
    }

    /// Push a **front-of-context** system message.
    ///
    /// # 不变量：只在会话建立时调用一次
    ///
    /// `system_messages` 渲染时**永远前置**（见 [`Self::build_context_for_gate`]），
    /// 与 msg_id 的写入序无关。因此**在历史已经存在之后**再 `push_system`，会让
    /// 新内容落到整个上下文的最前面，从第一个字节起击穿 provider 前缀缓存。
    ///
    /// 生产路径只有会话建立的三个入口（`create_session` /
    /// `create_session_with_preset_seed` / resume 建立）会调用本方法，且此时
    /// `turns` 与 `trailing_messages` 均为空。运行期系统注入（skills envelope、
    /// subagent 报告、MCP 清单）一律走
    /// [`Self::push_trailing_system`]，按写入序追加在尾部——那是缓存友好的位置
    /// （goal-mode 提示词不属此类：它是 user 代打回合，走 [`Self::push_user`]）。
    ///
    /// 需要新增「运行期系统消息」时，请用 trailing 路径，不要放宽这里的断言。
    pub fn push_system(&mut self, msg: Message) -> bool {
        debug_assert_eq!(msg.role, "system", "push_system requires role=system");
        debug_assert!(
            self.turns.is_empty() && self.trailing_messages.is_empty(),
            "push_system 只能用于会话建立（历史为空）；运行期注入请用 push_trailing_system，\
             否则新内容会落到上下文最前面并击穿前缀缓存"
        );
        // Guard: skip if an identical system message already exists.
        // This prevents double-injection when lifecycle paths are called
        // multiple times (e.g. create_session after a failed resume).
        let new_text = msg
            .content
            .iter()
            .find_map(|b| match b {
                qaqh_types::ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .unwrap_or("");
        if !new_text.is_empty()
            && self.system_messages.iter().any(|m| {
                m.content.iter().any(|b| match b {
                    qaqh_types::ContentBlock::Text { text } => text == new_text,
                    _ => false,
                })
            })
        {
            return false;
        }
        let mut msg = msg;
        let id = self.save_msg(&msg);
        if let Some(id) = id {
            msg.msg_id = Some(id);
        }
        self.system_messages.push(msg);
        self.context_revision = self.context_revision.saturating_add(1);
        false
    }

    /// Append a trailing injection, once.
    ///
    /// Idempotent: an identical message is skipped, so repeated sync calls on
    /// an unchanged activation set never duplicate. The message is persisted
    /// (save_msg, which also assigns the write-order msg_id) and stays in
    /// place for the rest of the session — never re-injected, never removed —
    /// keeping the request prefix byte-stable. Context serialization merges
    /// trailing injections with turns in write order
    /// ([`Self::flat_in_write_order`]), so this message lands at the position
    /// where it was written and later turns/steps append after it.
    pub fn push_trailing_system(&mut self, msg: Message) -> bool {
        let new_text = msg
            .content
            .iter()
            .find_map(|b| match b {
                qaqh_types::ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .unwrap_or("");
        debug_assert!(
            msg.role == "system" || msg.role == "developer" || is_user_injection(&msg, new_text),
            "push_trailing_system requires role=system|developer|user(已登记的注入源), got role={} name={:?}",
            msg.role,
            msg.name
        );
        // M-low②/M-doc：收窄为仅相邻去重——原全历史扫描会把间隔重现的
        // 相同报告（如子代理同名任务的两次完成报告）误判为重复丢弃；
        // 崩溃重放/双击产生的重复必然相邻，相邻口径已足够防护。
        let adjacent_duplicate = self.trailing_messages.last().is_some_and(|m| {
            m.content.iter().any(|b| match b {
                qaqh_types::ContentBlock::Text { text } => text == new_text,
                _ => false,
            })
        });
        if !new_text.is_empty() && adjacent_duplicate {
            return false;
        }
        // G3：最后 step 有未满足 tool_use 时延后写入——trailing 插在
        // tool_use 与迟到 result 之间会被 Anthropic/OpenAI 直接 400。
        if self.has_pending_tools() {
            const MAX_DEFERRED_TRAILING: usize = 64;
            if self.deferred_trailing.len() >= MAX_DEFERRED_TRAILING {
                let dropped = self.deferred_trailing.remove(0);
                log::error!(
                    "[store] deferred trailing overflow; dropping oldest (msg_id={:?})",
                    dropped.msg_id
                );
            }
            self.deferred_trailing.push(msg);
            return true;
        }
        let mut msg = msg;
        let id = self.save_msg(&msg);
        if let Some(id) = id {
            msg.msg_id = Some(id);
        }
        self.trailing_messages.push(msg);
        self.context_revision = self.context_revision.saturating_add(1);
        true
    }

    /// 安全点回灌：无悬空 tool_use 时把延后的 trailing 注入按序落盘。
    pub fn flush_deferred_trailing(&mut self) {
        if self.deferred_trailing.is_empty() || self.has_pending_tools() {
            return;
        }
        let pending = std::mem::take(&mut self.deferred_trailing);
        for msg in pending {
            let _ = self.push_trailing_system(msg);
        }
    }

    /// G5：取走本 store 记录的孤儿 tool_result call_id（观测上报用）。
    pub fn take_orphan_tool_results(&mut self) -> Vec<String> {
        std::mem::take(&mut self.orphan_tool_results)
    }

    /// Access the persisted trailing system messages (injection history).
    pub fn trailing_messages(&self) -> &[Message] {
        &self.trailing_messages
    }

    pub fn push_user(&mut self, text: &str) -> bool {
        if !self.replaying
            && let Some(turn) = self.turns.last_mut()
            && let Some(step) = turn.steps.last_mut()
        {
            auto_complete_unfulfilled(
                step,
                "[CANCELLED] Tool was not executed (user interrupted).",
            );
        }
        let mut msg = Message::user(text);
        let id = self.save_msg(&msg);
        if let Some(id) = id {
            msg.msg_id = Some(id);
        }
        self.turns.push(Turn::new(msg));
        self.context_revision = self.context_revision.saturating_add(1);
        // G3：新 turn 前旧悬空已被 auto_complete 消解，安全点回灌延后注入。
        self.flush_deferred_trailing();
        false
    }

    /// Push a system-role message as a standalone turn. Persisted like a user
    /// turn (`save_msg`), so `to_vec()` emits a mid-stream `system` message
    /// that OpenAI/Responses both accept.
    ///
    /// Reached via the flow for `Sink::Turn` + System/Developer sources;
    /// subagent reports no longer use this path — they are user-role trailing
    /// injections (see [`Self::push_trailing_system`]).
    pub fn push_system_input(&mut self, text: &str) -> bool {
        if !self.replaying
            && let Some(turn) = self.turns.last_mut()
            && let Some(step) = turn.steps.last_mut()
        {
            auto_complete_unfulfilled(
                step,
                "[CANCELLED] Tool was not executed (system injection arrived).",
            );
        }
        let mut msg = Message::system(text);
        let id = self.save_msg(&msg);
        if let Some(id) = id {
            msg.msg_id = Some(id);
        }
        self.turns.push(Turn::new(msg));
        self.context_revision = self.context_revision.saturating_add(1);
        false
    }

    /// Add an image block to the last user message (the most recent turn's user message).
    ///
    /// BUG-2026-09-13-04：图片块只改内存 turn 的话，ingest 时 save_msg 克隆
    /// 进 pending_save 的纯文本副本会在 flush 后落盘——归档 JSONL 的用户
    /// 消息永远无图片块，重启后图片索引永久丢失。同步修补 pending_save
    /// 中同 msg_id 的克隆；若该消息已随先前 flush 落盘（生产路径不可达，
    /// 此处仅防御），发 SaveFull 全量重写补齐并告警。
    ///
    /// BUG-2026-09-13-25：无 user turn（ingest 被拒 / replaying 回放）时
    /// 图片无处可挂。此前静默跳过——既不入 store 也无任何错误通道，带图
    /// 输入无声消失。现在返回 `bool`（`true` = 已挂到当前 user turn），
    /// 并由调用方决定是否上报；缺 turn 时另留 error 级诊断，与
    /// [`Self::push_assistant`] 的 "no turn exists" 路径对齐。
    ///
    /// 注意：返回 `false` 时图片字节可能已按内容寻址落盘（`store_image_b64`
    /// 成功），但没有任何消息引用它——存储侧仍需调用方给出用户可见的拒绝。
    pub fn push_image_to_last_user(&mut self, mime_type: &str, data: &str) -> bool {
        // A-2 L0：图片字节外置磁盘（内容寻址，见 qaqh_types::image_store），
        // 消息内只留 ImageRef 索引——daemon 常驻内存不再随图片数线性膨胀。
        // 落盘失败（IO 异常）回退 inline Image：宁可内存膨胀也不丢用户图。
        let block = match qaqh_types::image_store::store_image_b64(data, mime_type) {
            Ok(sha256) => qaqh_types::ContentBlock::ImageRef {
                sha256,
                mime_type: mime_type.to_string(),
                bytes_len: data.len(),
            },
            Err(e) => {
                log::warn!("[store] user image externalization failed, keeping inline: {e}");
                qaqh_types::ContentBlock::Image {
                    mime_type: mime_type.to_string(),
                    data: data.to_string(),
                }
            }
        };
        if let Some(turn) = self.turns.last_mut() {
            turn.user.content.push(block.clone());
            self.context_revision = self.context_revision.saturating_add(1);
            // BUG-2026-09-13-04：把图片块同步进 pending_save 的同 msg_id 克
            // 隆，flush 落盘的归档消息才带图片。
            if !self.ephemeral
                && !self.replaying
                && let Some(msg_id) = turn.user.msg_id
                && let Some(pending) = self
                    .pending_save
                    .iter_mut()
                    .find(|m| m.msg_id == Some(msg_id))
            {
                pending.content.push(block);
            } else if !self.ephemeral && !self.replaying {
                // 已 flush 落盘（防御分支）：发 SaveFull 全量重写补齐。
                log::warn!(
                    "[store] push_image_to_last_user: user msg already flushed; queuing SaveFull rewrite"
                );
                self.pending_persist.push(PersistOp::SaveFull {
                    session_id: self.session_id.clone(),
                    messages: self.to_vec(),
                    model: self.last_flush_model.0.clone(),
                    effort: self.last_flush_model.1.clone(),
                    compact_skip: self.persisted_compact_skip(),
                    compact_covered_through_msg_id: self.compact_covered_through_msg_id,
                    turn_count: self.turns.len(),
                });
            }
        } else {
            // BUG-2026-09-13-25：无 user turn（ingest 被拒 / replaying）→
            // 图片无处可挂，且不会有任何持久化记录。error 级（与
            // push_assistant 的 "no turn exists" 对齐）而非 warn：这是
            // 用户数据丢失，调用方必须把它当作失败上报。
            log::error!(
                "[store] push_image_to_last_user: no user turn to attach image (dropped, mime={mime_type}, bytes={})",
                data.len()
            );
            return false;
        }
        true
    }

    pub fn push_assistant(&mut self, mut msg: Message) -> bool {
        debug_assert_eq!(
            msg.role, "assistant",
            "push_assistant requires role=assistant"
        );

        if self.turns.is_empty() {
            log::error!(
                "push_assistant: no turn exists — assistant response without user input. Dropping."
            );
            return false;
        }

        if !self.replaying
            && let Some(step) = self.turns.last_mut().and_then(|t| t.steps.last_mut())
        {
            auto_complete_unfulfilled(
                step,
                "[AUTO] Tool was not executed before next assistant response.",
            );
        }

        // BUG-2026-09-13-13 / #13：**持久化前清洗**悬挂 ToolUse。
        //
        // 中断流的抢救路径会把没拿到 id/name 的调用一起交进来（`input` 亦可能
        // 为 `Null`）。它不可执行，存进 `messages.jsonl` 之后每次 reload 都会
        // 在回放面复现成幽灵工具卡（且部分端点会因 `arguments:"null"` 报 400）。
        // 这里丢弃该块而不是整条 assistant 消息——正文/思考是中断前的真实产物，
        // 丢了就是丢历史。判据与回放过滤共用 `is_hanging_tool_use()`。
        msg.content.retain(|b| !b.is_hanging_tool_use());
        let id = self.save_msg(&msg);
        if let Some(id) = id {
            msg.msg_id = Some(id);
        }
        let step = Step::new(msg);
        let has_tools = step.has_tool_use();
        self.turns
            .last_mut()
            .expect("checked above")
            .steps
            .push(step);
        self.context_revision = self.context_revision.saturating_add(1);

        !has_tools
    }

    pub fn push_tool_result(&mut self, tool_call_id: &str, result: &str, success: bool) -> bool {
        if self.push_tool_result_inner(tool_call_id, result, success, None, &[]) {
            self.context_revision = self.context_revision.saturating_add(1);
        }

        if let Some(turn) = self.turns.last()
            && let Some(step) = turn.steps.last()
            && step.all_tools_satisfied()
        {
            return step.pending_tools().is_empty();
        }
        false
    }

    fn push_tool_result_inner(
        &mut self,
        tool_call_id: &str,
        result: &str,
        success: bool,
        diff: Option<String>,
        images: &[qaqh_types::ToolImage],
    ) -> bool {
        let mut canonical = if success {
            qaqh_types::ToolResult::ok(result)
        } else {
            qaqh_types::ToolResult::error(result)
        };
        canonical.diff = diff;
        self.push_tool_canonical_inner(tool_call_id, canonical, images)
    }

    fn push_tool_canonical_inner(
        &mut self,
        tool_call_id: &str,
        mut result: qaqh_types::ToolResult,
        images: &[qaqh_types::ToolImage],
    ) -> bool {
        // 词元钱包读数：这里**是**工具结果进历史（进模型上下文）的唯一落盘口，
        // 所以盖戳也放在这里——所有路径（执行回填、权限拒绝、取消收尾、UI
        // 直调）一视同仁，不必在每个调用点重复一遍。
        //
        // 为什么在落盘时盖而不是在渲染时算：`render_xml_envelope` 每个请求都会
        // 重渲染全部历史工具结果，动态值会让整条历史每回合都变一次（前缀缓存
        // 全灭）。冻结之后就只是一条普通的历史字节。
        result.stamp_api_context();
        // 工具结果已在工具侧定型（qaqh-workspace::tool_side_fold）：
        // 存储的就是最终形态，message 层不再改写。
        let mut tool_msg = Message::tool_result(tool_call_id, result);
        // 工具附着的图片（read_image）：字节外置磁盘后以 ImageRef 追加。
        // gate 投影时按需读盘，降级为紧随 tool 结果的合成 user 消息
        // （image_url / input_image / anthropic base64 source）。落盘失败
        // 回退 inline Image。
        for image in images {
            let block =
                match qaqh_types::image_store::store_image_b64(&image.data, &image.mime_type) {
                    Ok(sha256) => qaqh_types::ContentBlock::ImageRef {
                        sha256,
                        mime_type: image.mime_type.clone(),
                        bytes_len: image.data.len(),
                    },
                    Err(e) => {
                        log::warn!(
                            "[store] tool image externalization failed, keeping inline: {e}"
                        );
                        qaqh_types::ContentBlock::Image {
                            mime_type: image.mime_type.clone(),
                            data: image.data.clone(),
                        }
                    }
                };
            tool_msg.content.push(block);
        }

        for turn in self.turns.iter_mut().rev() {
            if let Some(step) = turn.find_step_for_mut(tool_call_id) {
                if !step.tool_result_has_id(tool_call_id) {
                    step.tool_results.push(tool_msg.clone());
                    // push 之后不再使用 step/turn（NLL 释放借用），再保存。
                    let id = self.save_msg(&tool_msg);
                    if let Some(id) = id {
                        // 新借用回写：最后压入的 tool_result 即本条消息。
                        for t in self.turns.iter_mut().rev() {
                            if let Some(s) = t.find_step_for_mut(tool_call_id) {
                                if let Some(tr) = s.tool_results.last_mut() {
                                    tr.msg_id = Some(id);
                                }
                                break;
                            }
                        }
                    }
                    return true;
                }
                return false;
            }
        }
        // M-low③：原 last-step fallback 分支已删——前置逆向扫描按同一谓词
        // (has_tool_call) 遍历全部 turn，能到达此处说明任何 step 都不含该
        // call_id，fallback 永不可达（死代码），孤儿统一走下方记录。
        log::error!(
            "push_tool_result: orphan tool_result {} — nowhere to place, dropped",
            tool_call_id
        );
        self.orphan_tool_results.push(tool_call_id.to_string());
        false
    }

    /// Flatten turns (optionally skipping the first `skip_turns`) and trailing
    /// injections into a single message stream ordered by WRITE time.
    ///
    /// `msg_id` is assigned monotonically by [`Self::save_msg`] at write time
    /// to every persisted message (user turns, assistant steps, tool results,
    /// trailing injections), so id order == arrival order. `turns` and
    /// `trailing_messages` are each internally id-ascending; sorting the merged
    /// list by id reproduces the exact arrival order — a subagent report that
    /// landed mid-turn sits between that turn's earlier and later steps, and
    /// once the model acknowledges it the report is followed by the ack, i.e.
    /// it becomes history instead of re-reading as a fresh user message on
    /// every request.
    ///
    /// Fallback: if any message lacks a msg_id (ephemeral stores), emit turns
    /// first then trailing — the legacy order. This keeps ephemeral/subagent
    /// workers (which never persist ids) byte-compatible with the old layout.
    fn flat_in_write_order(&self, skip_turns: usize) -> Vec<Message> {
        // A compaction summary is appended to the archive with a fresh,
        // monotonic msg_id, but logically belongs before every kept message.
        // Emit summary turns first and only write-order-sort ordinary history;
        // otherwise the fresh id would push the summary to the tail.
        let mut summaries: Vec<Message> = Vec::new();
        let mut items: Vec<(Option<u64>, Message)> = Vec::new();
        for turn in self.turns.iter().skip(skip_turns) {
            if Self::compacted_turn_count(&turn.user).is_some() {
                summaries.push(turn.user.clone());
                continue;
            }
            items.push((turn.user.msg_id, turn.user.clone()));
            for step in &turn.steps {
                items.push((step.assistant.msg_id, step.assistant.clone()));
                for tr in &step.tool_results {
                    items.push((tr.msg_id, tr.clone()));
                }
            }
        }
        for m in &self.trailing_messages {
            items.push((m.msg_id, m.clone()));
        }
        if items.iter().all(|(id, _)| id.is_some()) {
            // Stable sort; msg_ids are unique per session, so this is a total
            // order. O(n log n) on a few hundred messages is negligible next
            // to the provider round-trip it serves.
            items.sort_by_key(|(id, _)| id.unwrap_or(u64::MAX));
        }
        summaries
            .into_iter()
            .chain(items.into_iter().map(|(_, m)| m))
            .collect()
    }

    pub fn build_context_for_gate(&self, annotations: &[String]) -> Vec<Message> {
        let mut full: Vec<Message> = {
            let mut v = Vec::new();
            v.extend(self.system_messages.clone());
            // Turns + trailing injections merged in WRITE order (msg_id): a
            // subagent report that landed mid-turn keeps that position — the
            // model's acknowledgement of it is written after the report, so it
            // serializes after it and the report becomes history. (Previously
            // trailing was pinned after ALL turns: every request ended with
            // the report as the last message, which the model re-read as a
            // freshly-arrived user message on each round.)
            v.extend(self.flat_in_write_order(self.compact_skip));
            v
        };

        if !annotations.is_empty() {
            let ann_text = annotations.join("\n");
            // Inject into the FIRST real user message: its position is fixed for
            // the lifetime of the context, so the [Environment] block never
            // moves between user messages. (Injecting into the last user message
            // made turn-1's message render differently once turn 2 arrived,
            // breaking the whole prefix cache at the first user message — cache
            // hits collapsed to the system prefix only.) In write-order
            // serialization a trailing injection could theoretically precede
            // the first user turn (injection into an auto-created session),
            // so skip every registered injection source — annotations belong
            // to the human user's message, not to an injected report.
            if let Some(first_user) = full.iter_mut().find(|m| {
                m.role == "user"
                    && !m
                        .name
                        .as_deref()
                        .is_some_and(|name| USER_INJECTION_SOURCES.contains(&name))
            }) {
                let existing = first_user.content.iter_mut().find_map(|b| {
                    if let qaqh_types::ContentBlock::Text { text } = b {
                        Some(text)
                    } else {
                        None
                    }
                });
                if let Some(text) = existing {
                    let original = text.clone();
                    *text = format!("[Environment]\n{}\n\n[UserMessage]\n{}", ann_text, original);
                } else {
                    first_user
                        .content
                        .push(qaqh_types::ContentBlock::text(&ann_text));
                }
            }
        }

        full
    }

    /// Get pending tools from the last step (for manual execution with streaming).
    pub fn get_last_step_pending(&self) -> Vec<PendingTool> {
        let step = match self.turns.last().and_then(|t| t.steps.last()) {
            Some(s) => s,
            None => return Vec::new(),
        };
        let tool_ids = step.assistant_tool_ids();
        step.pending_tools()
            .into_iter()
            .filter(|t| tool_ids.contains(&t.id) && !step.tool_result_has_id(&t.id))
            .collect()
    }

    /// Push a tool result directly (for manual execution).
    pub fn push_tool_result_direct(&mut self, tool_call_id: &str, result: &str, success: bool) {
        if self.push_tool_result_inner(tool_call_id, result, success, None, &[]) {
            self.context_revision = self.context_revision.saturating_add(1);
        }
        // G3：结果落地后尝试回灌延后的 trailing 注入。
        self.flush_deferred_trailing();
    }

    /// 同 `push_tool_result_direct`，另附着展示平面 diff（编辑/写入类工具）。
    /// diff 仅存在于结构化 ToolResult（不进模型投影），供 timeline 抽屉消费。
    pub fn push_tool_result_direct_with_diff(
        &mut self,
        tool_call_id: &str,
        result: &str,
        success: bool,
        diff: Option<String>,
    ) {
        if self.push_tool_result_inner(tool_call_id, result, success, diff, &[]) {
            self.context_revision = self.context_revision.saturating_add(1);
        }
    }

    /// 同 `push_tool_result_direct_with_diff`，另把工具产出的图片
    /// （如 `read_image`）以 `ContentBlock::Image` 追加到 tool 消息。
    /// gate 投影层负责把图片降级为 provider 原生 media part。
    pub fn push_tool_result_direct_with_attachments(
        &mut self,
        tool_call_id: &str,
        result: &str,
        success: bool,
        diff: Option<String>,
        images: &[qaqh_types::ToolImage],
    ) {
        if self.push_tool_result_inner(tool_call_id, result, success, diff, images) {
            self.context_revision = self.context_revision.saturating_add(1);
        }
        // G3：结果落地后尝试回灌延后的 trailing 注入。
        self.flush_deferred_trailing();
    }

    /// Snapshot of the last step's tool results, preserving the tool-side
    /// five-state [`qaqh_types::ToolStatus`] and display-plane diff.
    pub fn last_step_tool_results(&self) -> Vec<StepToolResult> {
        let step = match self.turns.last().and_then(|t| t.steps.last()) {
            Some(s) => s,
            None => return Vec::new(),
        };
        let mut results = Vec::new();
        for tr in &step.tool_results {
            if let Some((tc_id, result)) = tr.content.iter().find_map(|b| {
                if let qaqh_types::ContentBlock::ToolResult {
                    tool_use_id,
                    result,
                } = b
                {
                    Some((tool_use_id.clone(), result.clone()))
                } else {
                    None
                }
            }) {
                let tool_name = step
                    .assistant
                    .content
                    .iter()
                    .find_map(|b| {
                        if let qaqh_types::ContentBlock::ToolUse { id, name, .. } = b {
                            if id == &tc_id {
                                Some(name.clone())
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    })
                    .unwrap_or_default();
                results.push(StepToolResult {
                    tool_call_id: tc_id,
                    tool_name,
                    result,
                });
            }
        }
        results
    }

    /// Find the tool_call_id of a named tool in the most recent assistant step.
    pub fn find_last_step_tool_call(&self, tool_name: &str) -> Option<String> {
        let step = self.turns.last()?.steps.last()?;
        step.assistant.content.iter().find_map(|block| match block {
            qaqh_types::ContentBlock::ToolUse { name, id, .. } if name == tool_name => {
                Some(id.clone())
            }
            _ => None,
        })
    }

    pub fn has_pending_tools(&self) -> bool {
        self.turns
            .last()
            .and_then(|t| t.steps.last())
            .map(|s| !s.all_tools_satisfied())
            .unwrap_or(false)
    }

    pub fn turn_count(&self) -> usize {
        self.turns.len()
    }

    pub fn message_count(&self) -> usize {
        self.system_messages.len()
            + self
                .turns
                .iter()
                .map(|t| {
                    1 + t
                        .steps
                        .iter()
                        .map(|s| 1 + s.tool_results.len())
                        .sum::<usize>()
                })
                .sum::<usize>()
    }

    /// Return a low-overhead estimate of message payloads and owned heap
    /// capacities. This traverses the live store but does not clone or encode
    /// message bodies, so it is suitable for opt-in runtime probes.
    pub fn memory_usage_estimate(&self) -> MessageStoreMemoryUsage {
        let mut usage = MessageStoreMemoryUsage {
            turn_count: self.turns.len() as u64,
            pending_persist_ops: self.pending_persist.len() as u64,
            heap_estimate_bytes: size_of::<Self>() as u64,
            ..MessageStoreMemoryUsage::default()
        };
        usage.heap_estimate_bytes = usage
            .heap_estimate_bytes
            .saturating_add(self.session_id.capacity() as u64)
            .saturating_add(
                self.system_messages
                    .capacity()
                    .saturating_mul(size_of::<Message>()) as u64,
            )
            .saturating_add(
                self.trailing_messages
                    .capacity()
                    .saturating_mul(size_of::<Message>()) as u64,
            )
            .saturating_add(
                self.deferred_trailing
                    .capacity()
                    .saturating_mul(size_of::<Message>()) as u64,
            )
            .saturating_add(
                self.orphan_tool_results
                    .capacity()
                    .saturating_mul(size_of::<String>()) as u64,
            )
            .saturating_add(self.turns.capacity().saturating_mul(size_of::<Turn>()) as u64)
            .saturating_add(
                self.pending_save
                    .capacity()
                    .saturating_mul(size_of::<Message>()) as u64,
            )
            .saturating_add(
                self.pending_persist
                    .capacity()
                    .saturating_mul(size_of::<PersistOp>()) as u64,
            )
            .saturating_add(self.last_flush_model.0.capacity() as u64);
        if let Some(effort) = &self.last_flush_model.1 {
            usage.heap_estimate_bytes = usage
                .heap_estimate_bytes
                .saturating_add(effort.capacity() as u64);
        }

        for message in &self.system_messages {
            usage.add_message(message);
        }
        for message in &self.trailing_messages {
            usage.add_message(message);
        }
        for message in &self.deferred_trailing {
            usage.add_message(message);
        }
        for id in &self.orphan_tool_results {
            usage.payload_bytes = usage.payload_bytes.saturating_add(id.len() as u64);
            usage.heap_estimate_bytes = usage
                .heap_estimate_bytes
                .saturating_add(id.capacity() as u64);
        }
        for turn in &self.turns {
            usage.heap_estimate_bytes = usage
                .heap_estimate_bytes
                .saturating_add(size_of::<Turn>() as u64)
                .saturating_add(turn.steps.capacity().saturating_mul(size_of::<Step>()) as u64);
            usage.add_message(&turn.user);
            for step in &turn.steps {
                usage.add_message(&step.assistant);
                for result in &step.tool_results {
                    usage.add_message(result);
                }
            }
        }
        for message in &self.pending_save {
            usage.add_message(message);
        }
        for operation in &self.pending_persist {
            match operation {
                PersistOp::Append {
                    session_id,
                    messages,
                    model,
                    effort,
                    ..
                }
                | PersistOp::SaveFull {
                    session_id,
                    messages,
                    model,
                    effort,
                    ..
                } => {
                    usage.payload_bytes = usage
                        .payload_bytes
                        .saturating_add(session_id.len() as u64)
                        .saturating_add(model.len() as u64);
                    usage.heap_estimate_bytes = usage
                        .heap_estimate_bytes
                        .saturating_add(session_id.capacity() as u64)
                        .saturating_add(model.capacity() as u64)
                        .saturating_add(
                            messages.capacity().saturating_mul(size_of::<Message>()) as u64
                        );
                    if let Some(effort) = effort {
                        usage.heap_estimate_bytes = usage
                            .heap_estimate_bytes
                            .saturating_add(effort.capacity() as u64);
                    }
                    for message in messages {
                        usage.add_message(message);
                    }
                }
                PersistOp::UpdateMeta {
                    session_id,
                    model,
                    effort,
                    ..
                } => {
                    usage.payload_bytes = usage
                        .payload_bytes
                        .saturating_add(session_id.len() as u64)
                        .saturating_add(model.len() as u64);
                    usage.heap_estimate_bytes = usage
                        .heap_estimate_bytes
                        .saturating_add(session_id.capacity() as u64)
                        .saturating_add(model.capacity() as u64);
                    if let Some(effort) = effort {
                        usage.heap_estimate_bytes = usage
                            .heap_estimate_bytes
                            .saturating_add(effort.capacity() as u64);
                    }
                }
            }
        }
        usage
    }

    pub fn turns(&self) -> &[Turn] {
        &self.turns
    }

    pub fn system_messages(&self) -> &[Message] {
        &self.system_messages
    }

    pub fn to_vec(&self) -> Vec<Message> {
        let mut v: Vec<Message> = self.system_messages.clone();
        // Write-order merge (same as gate context): full-snapshot round-trips
        // (snapshot_full → save_full → from_messages) and compact checkpoints
        // preserve the interleaved position of trailing injections instead of
        // flattening them to the tail.
        v.extend(self.flat_in_write_order(0));
        v
    }

    /// Save the current active view as a full archive rewrite. Used for undo
    /// and defensive image repair — **not** for compaction. Compaction appends
    /// a summary through [`Self::persist_compaction`] and never rewrites the
    /// archive.
    ///
    /// No-op if the session seed has not been initialized yet. Enqueues a
    /// [`PersistOp`] for host-side execution (see [`Self::flush_meta`]).
    pub fn snapshot_full(&mut self, model: &str, effort: &str) {
        if self.session_id.is_empty() || self.ephemeral {
            return;
        }
        let msgs = self.to_vec();
        let turn_count = self.turns.len();
        // The rewrite replaces the archive with the active view. If a summary
        // turn remains, preserve its watermark so the rewritten archive still
        // derives the same active view after restart. Clearing every turn is
        // the one case where the old covered prefix no longer exists.
        let compact_covered_through_msg_id = if self.turns.is_empty() {
            None
        } else {
            self.compact_covered_through_msg_id
        };
        self.compact_skip = 0;
        self.compact_covered_through_msg_id = compact_covered_through_msg_id;
        self.pending_persist.push(PersistOp::SaveFull {
            session_id: self.session_id.clone(),
            messages: msgs,
            model: model.to_string(),
            effort: Some(effort.to_string()),
            compact_skip: 0,
            compact_covered_through_msg_id,
            turn_count,
        });
        self.pending_save.clear();
    }

    /// Persist a just-applied compaction. `apply_compact` has already assigned
    /// the summary a fresh msg_id and placed it in `pending_save`; this method
    /// flushes that append together with the new covered watermark.
    pub fn persist_compaction(&mut self, model: &str, effort: &str) {
        self.flush_meta(model, effort);
    }

    /// Reconstruct the internal turn/step structure by replaying saved messages
    /// through `push_user` / `push_assistant` / `push_tool_result`.
    pub fn from_messages(
        session_id: &str,
        msgs: &[Message],
        compact_skip: usize,
    ) -> (Self, Vec<String>) {
        let mut store = Self::new(session_id);
        store.compact_skip = compact_skip;
        store.replaying = true;
        let mut repairs = Vec::new();
        let mut i = 0;

        // Keep the base prompt plus distinct protected skill injections.
        // Other duplicate system messages from old persistence bugs are dropped.
        let mut has_system = false;
        let mut protected_system_texts = std::collections::HashSet::new();
        while i < msgs.len() && msgs[i].role == "system" {
            let text = msgs[i]
                .content
                .iter()
                .find_map(|block| match block {
                    qaqh_types::ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .unwrap_or("");
            let protected_skill =
                text.starts_with("[QAQH_SKILL_V1]") && text.contains("[END_QAQH_SKILL_V1]");
            if !has_system || (protected_skill && protected_system_texts.insert(text.to_string())) {
                store.system_messages.push(msgs[i].clone());
                has_system = true;
            } else {
                repairs.push(
                    "dropped duplicate system message (msg_id collision or prior bug)".into(),
                );
            }
            if let Some(mid) = msgs[i].msg_id {
                store.next_msg_id = store.next_msg_id.max(mid + 1);
            }
            i += 1;
        }

        while i < msgs.len() {
            match msgs[i].role.as_str() {
                "user" => {
                    let text = msgs[i]
                        .content
                        .iter()
                        .find_map(|b| {
                            if let qaqh_types::ContentBlock::Text { text } = b {
                                Some(text.clone())
                            } else {
                                None
                            }
                        })
                        .unwrap_or_default();
                    // 子代理注入（user 角色 + [SUBAGENT 前缀）：恢复为 trailing，
                    // 不当作真实用户 turn（不建回合、不触发 timeline）。经
                    // push_trailing_system 回放以分配写入顺序 msg_id（replaying
                    // 期间 save_msg 不落盘），使 to_vec/build_context 的写入顺序
                    // 合并能恢复注入的原始时间位置。
                    // M-low④：判别优先用持久化 name 字段（写侧恒置 name），
                    // 前缀仅作旧档回退——以 [SUBAGENT 开头的真实用户消息
                    // 不再被误改类为注入。登记表见 USER_INJECTION_SOURCES。
                    let is_injection = is_user_injection(&msgs[i], &text);
                    if is_injection {
                        store.push_trailing_system(msgs[i].clone());
                    } else {
                        store.push_user(&text);
                    }
                    i += 1;
                }
                "assistant" => {
                    store.push_assistant(msgs[i].clone());
                    i += 1;
                }
                "tool" => {
                    let (tc_id, result, success) = msgs[i]
                        .content
                        .iter()
                        .find_map(|b| {
                            if let qaqh_types::ContentBlock::ToolResult {
                                tool_use_id,
                                result,
                            } = b
                            {
                                Some((
                                    tool_use_id.clone(),
                                    result.model_text().to_string(),
                                    result.is_success(),
                                ))
                            } else {
                                None
                            }
                        })
                        .unwrap_or_default();
                    store.push_tool_result(&tc_id, &result, success);
                    i += 1;
                }
                _ => {
                    // Mid-stream system/developer messages: recognize persisted
                    // activation-set envelopes AND sub-agent result injections
                    // ([SUBAGENT ...] tag) and restore them as trailing
                    // injections (消息流末尾，wire 序列合法)；显式 developer
                    // role 的消息（ContextFlow 注入）无论文本一律恢复为
                    // trailing；drop anything else silently. 此前 [SUBAGENT]
                    // 被恢复为 system turn、固化在 function_call_output 之后，
                    // provider 端丢弃/挂起。
                    let text = msgs[i]
                        .content
                        .iter()
                        .find_map(|block| match block {
                            qaqh_types::ContentBlock::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .unwrap_or("");
                    if msgs[i].role == "developer"
                        || is_user_injection(&msgs[i], text)
                        || text.starts_with("<skill_context_envelope")
                        // 旧档回退：[SUBAGENT 前缀不限角色（push_system_input 造的
                        // 是 system 角色 + 该前缀），收紧成 role==user 会静默丢弃。
                        || text.starts_with("[SUBAGENT ")
                    {
                        // 同上：经 push_trailing_system 回放，分配写入顺序 id。
                        store.push_trailing_system(msgs[i].clone());
                    }
                    i += 1;
                }
            }
        }

        for turn in store.turns.iter_mut() {
            for step in turn.steps.iter_mut() {
                let missing_ids: Vec<(String, String)> = {
                    let tool_ids = step.assistant_tool_ids();
                    tool_ids
                        .iter()
                        .filter(|id| !step.tool_result_has_id(id))
                        .map(|id| {
                            let name = step
                                .assistant
                                .content
                                .iter()
                                .find_map(|b| {
                                    if let qaqh_types::ContentBlock::ToolUse {
                                        id: tid, name, ..
                                    } = b
                                    {
                                        if tid == id { Some(name.clone()) } else { None }
                                    } else {
                                        None
                                    }
                                })
                                .unwrap_or_default();
                            (id.clone(), name)
                        })
                        .collect()
                };
                if missing_ids.is_empty() {
                    continue;
                }
                for (id, name) in missing_ids {
                    let note = format!(
                        "[RESTORE] Tool \"{name}\" had no result when session was saved — execution outcome unknown; side effects may have occurred.\n[HINT] Do NOT retry automatically; verify workspace and execution records first."
                    );
                    step.tool_results.push(Message::tool(&id, &note, false));
                    repairs.push(format!("injected [RESTORE] for orphan tool_use {}", id));
                }
            }
        }

        // Restore next_msg_id: max(msg_id) + 1, or 1 if empty.
        let max_id = msgs.iter().filter_map(|m| m.msg_id).max().unwrap_or(0);
        store.next_msg_id = store.next_msg_id.max(max_id + 1);
        store.next_turn_seq = store.next_turn_seq.max(store.turns.len() as u64 + 1);
        store.replaying = false;

        (store, repairs)
    }

    /// Allocate a session-monotonic turn ID. Compaction may shrink the active
    /// model view, but must never make a future TurnStart reuse an archived ID.
    pub fn allocate_turn_id(&mut self) -> String {
        let seq = self.next_turn_seq;
        self.next_turn_seq = self.next_turn_seq.saturating_add(1);
        format!("t{seq}")
    }

    /// Restore the turn allocator against the immutable archive.
    pub fn ensure_next_turn_seq(&mut self, next: u64) {
        self.next_turn_seq = self.next_turn_seq.max(next.max(1));
    }

    pub fn remove_last_step_if_incomplete(&mut self) -> bool {
        if let Some(turn) = self.turns.last_mut()
            && let Some(step) = turn.steps.last()
            && !step.all_tools_satisfied()
        {
            turn.steps.pop();
            self.context_revision = self.context_revision.saturating_add(1);
            return true;
        }
        false
    }

    /// Undo semantics: keep turns strictly before `turn_id`, drop the target
    /// turn and everything after it.
    ///
    /// `turn_id` is a session-monotonic sequence (`t{next_turn_seq}`) — it is
    /// NOT an index into `turns`. After a physical compaction
    /// ([`Self::apply_compact`]) `turns` only holds the live suffix (synthetic
    /// summary + kept turns) while `next_turn_seq` keeps counting, so the live
    /// array's first element maps to `compacted_count + 1`, recovered from the
    /// summary's `[Compacted N turns]` header (same encoding
    /// [`Self::previous_compact_summary`] parses).
    pub fn truncate_before_turn(&mut self, turn_id: &str) -> bool {
        let seq: usize = match turn_id
            .strip_prefix('t')
            .and_then(|n| n.parse::<usize>().ok())
        {
            Some(n) if n > 0 => n,
            _ => return false,
        };
        // 活跃 turns 数组起点的全局序号：无 compact 时 = 1；物理 compact 后
        // turns[0] 是合成摘要（占一个数组位），起点 = 被压缩 turn 数 + 1。
        let (first_seq, summary_offset) = match self
            .turns
            .first()
            .and_then(|t| Self::compacted_turn_count(&t.user))
        {
            Some(skip) => (skip + 1, 1),
            None => (1, 0),
        };
        if seq < first_seq {
            // 撤回已被压缩的 turn：清空全部（含摘要——它正是这些 turn 的替身）。
            self.turns.clear();
            self.context_revision = self.context_revision.saturating_add(1);
            return true;
        }
        let idx = seq - first_seq + summary_offset;
        if idx >= self.turns.len() {
            return false;
        }
        self.turns.truncate(idx);
        self.context_revision = self.context_revision.saturating_add(1);
        // After truncation, need full rewrite on next save.
        true
    }

    /// Compact: keep `keep` recent turns in the active model view, append the
    /// summary as a new archive message, and advance the covered watermark.
    ///
    /// `messages.jsonl` is append-only: the summary gets a fresh monotonic
    /// `msg_id` (never the id of a compacted turn). `flat_in_write_order`
    /// explicitly emits summary turns first, so the fresh high id does not
    /// move the summary to the tail.
    ///
    /// Trailing injections in the compacted region (msg_id < the first kept
    /// turn) are folded together with the old turns: their text has already
    /// entered the compact summary input, so keeping them would both duplicate
    /// the text and re-position a stale injection at the "latest message"
    /// slot. Only injections in the kept region survive.
    ///
    /// 返回值是这次调用**实际**做了什么（[`CompactionEffect`]）。没有真实
    /// turn 可折叠时这里是纯 no-op：**不写摘要、不推进 `context_revision`、
    /// 不消耗 msg_id**。摘要本身是一段要进模型的上下文，若在无可折叠轮次时
    /// 仍插入，压缩就变成"上下文变长"——这正是「显示已取消却多出一段摘要」
    /// 的形态。
    pub fn apply_compact(&mut self, summary: &str, keep: usize) -> CompactionEffect {
        // 防御：确保至少保留 1 个 turn，防止 caller 传入 keep=0 导致清空全部历史。
        let keep = keep.max(1);
        let skip = self.turns.len().saturating_sub(keep);
        if skip == 0 {
            return CompactionEffect::default();
        }

        // Remove old compact markers from the legacy front-of-context slot.
        self.system_messages.retain(|m| !m.content.iter().any(|b| matches!(b, qaqh_types::ContentBlock::Text { text } if text.starts_with("[COMPACT"))));

        // Build compact summary as a synthetic user turn (no steps).
        // The next LLM sees only the handoff summary; the original user
        // message is intentionally omitted — it's redundant with the
        // summary and, after a manual compact, the user will send a fresh
        // message to resume work.
        // H4：合成摘要占一个数组位但非真实 turn。重复压缩时把它从 skip
        // 中扣除并累加上一轮头部计数，保证 [Compacted N] 恒等于"真实被
        // 折叠的 turn 总数"，undo 的 seq→index 映射不再漂移。
        let prev_compacted = self
            .turns
            .first()
            .and_then(|t| Self::compacted_turn_count(&t.user))
            .unwrap_or(0);
        let summary_slot = if prev_compacted > 0 { 1 } else { 0 };
        let skip_real = skip.saturating_sub(summary_slot);
        // 压缩区里只有上一份摘要占位 → 没有真实 turn 进摘要，新摘要是凭空
        // 多出来的一段上下文（旧轮次一个没少）。此时什么都不做，让调用方
        // 如实上报"无可折叠轮次"。
        if skip_real == 0 {
            return CompactionEffect::default();
        }
        let total_real = prev_compacted + skip_real;
        let compact_text = format!("[Compacted {} turns]\n{}", total_real, summary.trim(),);

        // 首个保留 turn 的写入 id：压缩区内的 trailing 注入（id < 此值）随旧 turn
        // 折叠丢弃；保留区内的注入留在原位（A1 折叠语义）。摘要本身拿到新的
        // 高水位 id，归档顺序仍是 append-only；读侧按 watermark 重新拼活跃视图。
        let first_kept_id = self.turns[skip].user.msg_id.unwrap_or(0);
        let mut compact_turn = Turn::new(Message::user(&compact_text));
        let summary_id = self.save_msg(&compact_turn.user);
        compact_turn.user.msg_id = summary_id;

        // 折叠压缩区 trailing 注入：文本已进摘要输入，随旧 turn 一起丢弃，
        // 不再"复活"到摘要之后/活跃 turn 之前（双重表示）。
        self.trailing_messages
            .retain(|m| m.msg_id.is_none_or(|id| id >= first_kept_id));

        // Physically remove compacted turns, keep only the most recent `keep`.
        let kept = self.turns.split_off(skip);
        self.turns = kept;
        // Prepend compact summary as a synthetic turn before the kept turns.
        self.turns.insert(0, compact_turn);
        // No logical skipping needed — the watermark derives the archive view.
        self.compact_skip = 0;
        if first_kept_id > 0 {
            let covered = first_kept_id.saturating_sub(1);
            self.compact_covered_through_msg_id = Some(
                self.compact_covered_through_msg_id
                    .map_or(covered, |existing| existing.max(covered)),
            );
        }
        self.context_revision = self.context_revision.saturating_add(1);
        CompactionEffect {
            folded_turns: skip_real,
            checkpoint_written: true,
        }
    }

    /// Get the text of any previous compaction summary (for incremental update mode).
    /// Format: `[Compacted N turns]\n{summary}` — everything after the first newline.
    /// Searches turns[0] since compact summary is stored as a synthetic turn.
    pub fn previous_compact_summary(&self) -> Option<String> {
        self.turns.first().and_then(|turn| {
            turn.user.content.iter().find_map(|b| {
                if let qaqh_types::ContentBlock::Text { text } = b {
                    if text.starts_with("[Compacted") {
                        // 首个 '\n'（ASCII）之后必为 char boundary。
                        let summary = match text.find('\n') {
                            Some(n) => text.get(n + 1..).unwrap_or("").trim(),
                            None => "",
                        };
                        if summary.len() > 20 {
                            Some(summary.to_string())
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
        })
    }

    /// Parse the `[Compacted N turns]` header of a synthetic summary turn.
    /// Used to map global turn sequence numbers onto the live `turns` array
    /// after physical compaction (see [`Self::truncate_before_turn`]).
    fn compacted_turn_count(msg: &qaqh_types::Message) -> Option<usize> {
        msg.content.iter().find_map(|b| {
            if let qaqh_types::ContentBlock::Text { text } = b {
                let rest = text.strip_prefix("[Compacted ")?;
                let n = rest.split_whitespace().next()?.parse::<usize>().ok()?;
                Some(n)
            } else {
                None
            }
        })
    }

}

fn auto_complete_unfulfilled(step: &mut Step, reason: &str) {
    let missing: Vec<(String, String)> = {
        let tool_ids = step.assistant_tool_ids();
        tool_ids
            .iter()
            .filter(|id| !step.tool_result_has_id(id))
            .map(|id| {
                let name = step
                    .assistant
                    .content
                    .iter()
                    .find_map(|b| {
                        if let qaqh_types::ContentBlock::ToolUse { id: tid, name, .. } = b {
                            if tid == id { Some(name.clone()) } else { None }
                        } else {
                            None
                        }
                    })
                    .unwrap_or_default();
                (id.clone(), name)
            })
            .collect()
    };
    if !missing.is_empty() {
        log::warn!(
            "auto-complete: {} unfulfilled tool(s) — {}",
            missing.len(),
            reason
        );
        for (id, name) in missing {
            step.tool_results.push(Message::tool(
                &id,
                &format!("{} Tool \"{name}\" was not executed.", reason),
                false,
            ));
        }
    }
}

#[cfg(test)]
mod tests;


#[cfg(test)]
mod cancelled_step_cleanup_tests;
