//! API client stubs — Phase 4: replace with real dsx-hp IPC client.

use dsx_types::{BalanceInfo, UsageInfo};

/// Stream events from the API — matches the legacy enum shape.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    ContentDelta(String),
    ReasoningDelta(String),
    ToolCallProgress { name: String, args_so_far: String },
    Done { raw_message: dsx_types::Message, usage: Option<UsageInfo>, stop_reason: Option<String> },
    Error(String),
    BalanceResult(String),
    ExecProgress(String),
    ModelListResult(Vec<String>),
    SudoDone(String),
    ExecDone(String, String),
    ExecStarted(String, u32),
}

/// Stub: start a chat stream. Phase 4: replace with IPC to dsx-hp.
pub async fn chat_stream(
    _cfg: &crate::config::Config,
    _model: &str,
    _system: Option<String>,
    _messages: Vec<dsx_types::Message>,
    _extended: bool,
    _effort: Option<&str>,
    _tools: Option<Vec<dsx_types::ToolDef>>,
    _tool_choice: Option<&str>,
    _max_tokens: u32,
    _stop: Option<&[String]>,
    _uid: Option<&str>,
    _tx: tokio::sync::mpsc::Sender<StreamEvent>,
) -> anyhow::Result<()> {
    let _ = (_cfg, _model, _system, _messages, _extended, _effort, _tools, _tool_choice, _max_tokens, _stop, _uid, _tx);
    Ok(())
}

/// Stub: get account balance. Phase 4: replace with real API call.
pub async fn get_balance(_cfg: &crate::config::Config) -> anyhow::Result<BalanceInfo> {
    Ok(BalanceInfo {
        is_available: true,
        balance_infos: vec![],
    })
}
