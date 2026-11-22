//! 门决策类型:dispatcher 各门的返回形状。
//!
//! 口径(调研):只有显式 deny/block 改变控制流;hook 崩溃/超时/输出非法 =
//! fail-open(除非 handler `on_failure = block`)。`updatedInput` last-wins,
//! schema 校验失败 = 该次调用 deny,不是 fail-open(grok 特例)。

#[derive(Debug, Clone, PartialEq)]
pub struct HookRefusal {
    /// 人类可读来源,如 `hook 'guard' (PreToolUse)`。
    pub source: String,
    pub reason: String,
}

/// PreToolUse 门结果。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PreToolUseOutcome {
    pub denial: Option<HookRefusal>,
    /// hook ask:升级权限交互(即使策略本会放行)。
    pub ask_reason: Option<String>,
    /// last-wins 改写后的工具入参。
    pub updated_input: Option<serde_json::Value>,
    /// PreToolUse 放行路径上的模型上下文注入。
    pub additional_context: Vec<String>,
    /// 全部 handler 的记录(事件面用)。
    pub runs: Vec<RunRecord>,
}

/// PermissionRequest 门结果。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PermissionOutcome {
    /// Some(true) = hook 预答放行;Some(false) = 预答拒绝;None = 不干预。
    pub decision: Option<bool>,
    /// 预答拒绝时的理由(随既有拒绝路径落记录)。
    pub denial_reason: Option<String>,
    pub runs: Vec<RunRecord>,
}

/// UserPromptSubmit 门结果。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PromptOutcome {
    /// blocked prompt 不入库;reason 只给人看(客户端展示),不进模型上下文。
    pub block: Option<HookRefusal>,
    /// 放行时可注入的模型上下文(codex 口径;grok 丢弃,codex 注入,取并集)。
    pub additional_context: Vec<String>,
    pub runs: Vec<RunRecord>,
}

/// Stop 门结果。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StopOutcome {
    /// Some(reason) = 阻止停回合,reason 回灌模型继续干。
    pub block: Option<HookRefusal>,
    /// 放行时的模型上下文注入。
    pub additional_context: Vec<String>,
    pub runs: Vec<RunRecord>,
}

/// PostToolUse 门结果。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PostToolUseOutcome {
    /// 追加到工具结果后的模型上下文。
    pub additional_context: Vec<String>,
    /// block reason:工具已跑完,只能作为反馈随结果回给模型。
    pub block_reasons: Vec<String>,
    /// updatedToolOutput:替换模型可见输出(落库与回传同字节)。
    pub output_replacement: Option<String>,
    pub runs: Vec<RunRecord>,
}

/// 观察事件结果:只聚合记录。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ObserveOutcome {
    pub runs: Vec<RunRecord>,
}

/// 单条 hook 运行记录(事件面 + 诊断)。
#[derive(Debug, Clone, PartialEq)]
pub struct RunRecord {
    pub hook_name: String,
    pub status: RunStatus,
    /// 摘要:deny/block reason 或失败首行 stderr。
    pub summary: Option<String>,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    Completed,
    Failed,
    TimedOut,
    SkippedUntrusted,
    SkippedDisabled,
}

impl RunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::TimedOut => "timed_out",
            Self::SkippedUntrusted => "skipped_untrusted",
            Self::SkippedDisabled => "skipped_disabled",
        }
    }
}
