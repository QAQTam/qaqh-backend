//! TUI mode enum and sub-menu state.

#[derive(Debug, PartialEq)]
pub(crate) enum Mode {
    Chat,
    Streaming,
    ToolConfirm,
    DangerPrompt,
    SudoPrompt,
    ApiConfig,
    SessionPicker,
    IntentPicker,
    /// Gate blocked — modal popup overlay (repairable=false) or banner (repairable=true).
    GateBlock,
}

#[derive(Debug, Clone)]
pub(crate) struct SubMenuState {
    pub(crate) parent_cmd: String,
    pub(crate) items: Vec<(String, String)>,
    pub(crate) idx: usize,
}
