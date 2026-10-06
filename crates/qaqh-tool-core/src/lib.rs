//! qaqh-tool-core — Tool SDK 核心（P2 crate 拆分，研究文档 §4）。
//!
//! 从 `qaqh-workspace` 拆出的工具契约单一事实源：typed 工具契约
//! （[`tool_api`]）、内置能力迁移表（[`tool_capabilities`]）与 SDK 根类型
//! （[`ToolRisk`] / [`ToolEffect`] / [`JsonArgs`] / exec 进度通道）。
//!
//! 消费者只需本 crate 即可实现/适配工具（MCP/LSP 动态工具、subagent 内部
//! 工具面），无需依赖 `qaqh-workspace` 门面。门面经 re-export 保持
//! `qaqh_workspace::tool_api::*` 等既有路径兼容。

pub mod permission;
pub mod tool_api;
pub mod tool_capabilities;

pub use tool_api::{
    Concurrency, DescriptorError, ErasedTool, FatalToolError, Namespace, OutputBudget, PathOp,
    ProgressSink, ProgressStream, SandboxMode, SandboxSpec, ToolBody, ToolCallContext,
    ToolCallSource, ToolCapabilities, ToolContentBlock, ToolDescriptor, ToolDisplay, ToolDisplayFn,
    ToolDisplayOutcome, ToolError, ToolErrorCode, ToolErrorCodeError, ToolErrorKind,
    ToolExecutionError, ToolExposure, ToolHeader, ToolMeta, ToolMetrics, ToolModelProjection,
    ToolName, ToolOutcome, ToolOutputValue, ToolProgress, ToolProjection, ToolSource, ToolStatus,
    ToolTerminalState, TypedTool, TypedToolAdapter, map_tool_result,
};
pub use tool_capabilities::builtin_capabilities;

// ── SDK 根类型（自 qaqh-workspace/lib.rs 迁入）─────────────────────────

/// Risk level for tool operations, replacing per-handler safety functions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolRisk {
    ReadOnly,
    Write,
    Destructive,
    Administrative,
}

/// Trusted, typed state transitions emitted by tool handlers.
///
/// Keeping this wrapper generic lets the runtime add other effect families
/// without widening the textual tool-result protocol.
#[derive(Debug, Clone)]
pub enum ToolEffect {
    Skill(qaqh_skills::SkillEffect),
    /// A subagent actor has been created and is waiting for its canonical
    /// `SubagentSpawned` fact before the task is delivered.
    SubagentSpawned {
        session_id: String,
        child_session_id: String,
        name: String,
        task_text: String,
        timeout_secs: u64,
        parent_session_id: String,
        parent_agent_path: String,
        child_agent_path: String,
        process_id: u32,
        spawn_tools: Vec<String>,
        spawn_model: Option<String>,
        spawn_base_url: Option<String>,
        spawn_max_tokens: Option<u32>,
        spawn_ephemeral: bool,
        spawn_timeout_secs: u64,
    },
}

/// Typed access to tool arguments (v1 legacy helper).
pub trait JsonArgs {
    fn s(&self, key: &str) -> String;
    fn s_or(&self, key: &str, default: &str) -> String;
    fn opt_bool(&self, key: &str) -> Option<bool>;
}

impl JsonArgs for serde_json::Value {
    fn s(&self, key: &str) -> String {
        self.get(key)
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_default()
    }
    fn s_or(&self, key: &str, default: &str) -> String {
        self.get(key)
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| default.to_string())
    }
    fn opt_bool(&self, key: &str) -> Option<bool> {
        let val = self.get(key)?;
        val.as_bool()
            .or_else(|| val.as_str().and_then(|s| s.parse::<bool>().ok()))
    }
}

// ── exec 进度通道（有界、有损；09-18 展示契约 §5）───────────────────────

/// exec 子进程输出进度帧。有界 channel 有意丢弃超限帧，保留跨流交错观测
/// 顺序，流内顺序精确。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecProgressEvent {
    pub tool_call_id: String,
    pub stream: ExecOutputStream,
    pub seq: u64,
    pub chunk: String,
    /// 截至本帧的累计观测字节（成功入队 + 被有界 channel 丢弃）。
    /// 由 [`ExecProgressSender`] 统一填写，构造方填 0 即可。
    pub bytes_total: u64,
}

/// 进度字节总量句柄：与 sender 共享计数器；sender 全部 drop 后仍可读终值。
#[derive(Clone, Debug, Default)]
pub struct ExecProgressTotals {
    emitted: std::sync::Arc<std::sync::atomic::AtomicU64>,
    dropped: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl ExecProgressTotals {
    /// 累计观测字节（09-18 展示契约 §5.1：含被丢弃与被尾部裁剪的部分）。
    pub fn total_bytes(&self) -> u64 {
        self.emitted.load(std::sync::atomic::Ordering::Relaxed)
            + self.dropped.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn dropped_bytes(&self) -> u64 {
        self.dropped.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecOutputStream {
    Stdout,
    Stderr,
}

impl ExecOutputStream {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }
}

/// Bounded, lossy progress sender. Pipe readers must never wait for a slow UI.
#[derive(Clone)]
pub struct ExecProgressSender {
    tx: std::sync::mpsc::SyncSender<ExecProgressEvent>,
    emitted_bytes: std::sync::Arc<std::sync::atomic::AtomicU64>,
    dropped_bytes: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl ExecProgressSender {
    pub fn try_send(&self, mut event: ExecProgressEvent) {
        let bytes = event.chunk.len() as u64;
        let emitted = self
            .emitted_bytes
            .fetch_add(bytes, std::sync::atomic::Ordering::Relaxed)
            + bytes;
        event.bytes_total = emitted
            + self
                .dropped_bytes
                .load(std::sync::atomic::Ordering::Relaxed);
        if self.tx.try_send(event).is_err() {
            // 通道满：这批字节未被消费，从 emitted 挪到 dropped（总量口径不变）。
            self.emitted_bytes
                .fetch_sub(bytes, std::sync::atomic::Ordering::Relaxed);
            self.dropped_bytes
                .fetch_add(bytes, std::sync::atomic::Ordering::Relaxed);
        }
    }

    pub fn dropped_bytes(&self) -> u64 {
        self.dropped_bytes
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// 与本 sender 共享计数的总量句柄（供 runtime 在 seal 时读终值）。
    pub fn totals(&self) -> ExecProgressTotals {
        ExecProgressTotals {
            emitted: self.emitted_bytes.clone(),
            dropped: self.dropped_bytes.clone(),
        }
    }
}

pub const EXEC_PROGRESS_CHANNEL_CAPACITY: usize = 256;

pub fn bounded_exec_progress_channel() -> (
    ExecProgressSender,
    std::sync::mpsc::Receiver<ExecProgressEvent>,
) {
    let (tx, rx) = std::sync::mpsc::sync_channel(EXEC_PROGRESS_CHANNEL_CAPACITY);
    let dropped_bytes = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let emitted_bytes = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    (
        ExecProgressSender {
            tx,
            emitted_bytes,
            dropped_bytes,
        },
        rx,
    )
}
