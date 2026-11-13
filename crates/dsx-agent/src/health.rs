//! Phase 4 stubs: health types were in dsx-hp in the old monolith.
//! These stubs keep dsx-agent compiling until health is fully migrated to dsx-hp.

use std::collections::HashMap;

// ── Health enums ──

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HealthLevel {
    #[default]
    Green,
    Yellow,
    Red,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextTier {
    Premium,
    Healthy,
    Danger,
}

impl ContextTier {
    pub fn from_tokens(tokens: u32, limit: u32) -> Self {
        let pct = if limit > 0 {
            tokens as f64 / limit as f64
        } else {
            0.0
        };
        if pct > 0.80 || (limit > 0 && tokens >= 400_000) || tokens >= 400_000 {
            ContextTier::Danger
        } else if pct > 0.30 || tokens >= 128_000 {
            ContextTier::Healthy
        } else {
            ContextTier::Premium
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorKind {
    ToolParameter,
    ToolNotFound,
    FileAccess,
    ExecFailure,
    NetworkFailure,
    SudoFailure,
    Timeout,
    Panic,
    SessionMissing,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentEmotion {
    Flow,
    Calm,
    Anxious,
    Frustrated,
    Confused,
    Panic,
}

impl AgentEmotion {
    pub fn emoji(&self) -> &'static str {
        match self {
            AgentEmotion::Flow => "\u{26a1}",
            AgentEmotion::Calm => "\u{b7}",
            AgentEmotion::Anxious => "\u{23f3}",
            AgentEmotion::Frustrated => "\u{1f4a2}",
            AgentEmotion::Confused => "\u{2753}",
            AgentEmotion::Panic => "\u{1f525}",
        }
    }
    pub fn label(&self) -> &'static str {
        match self {
            AgentEmotion::Flow => "flow",
            AgentEmotion::Calm => "calm",
            AgentEmotion::Anxious => "anxious",
            AgentEmotion::Frustrated => "frustrated",
            AgentEmotion::Confused => "confused",
            AgentEmotion::Panic => "panic",
        }
    }
    pub fn vent(&self) -> &'static str {
        match self {
            AgentEmotion::Frustrated => {
                "This thing is being a pain. Take a breath, maybe walk away for a minute, then \
                 come back with a fresh angle."
            }
            AgentEmotion::Confused => {
                "Nothing's working huh. Try explaining the problem out loud \u{2014} sometimes \
                 saying it helps you see it differently."
            }
            AgentEmotion::Panic => {
                "OK deep breath. Context is running out, errors piling up. Stop. Save your work. \
                 /compact, then start fresh."
            }
            AgentEmotion::Anxious => {
                "Slow down cowboy. Too many things happening at once. Focus on one thing at a time."
            }
            _ => "",
        }
    }
}

#[derive(Debug, Clone)]
pub struct HealthError {
    pub turn: u32,
    pub tool: String,
    pub category: ErrorKind,
    pub message: String,
}

// ── Platform ──

#[derive(Debug, Clone)]
pub struct DsAgentsHealthPlatform {
    pub tool_calls_this_turn: u32,
    pub consecutive_tool_only_turns: u32,
    pub error_counts: HashMap<ErrorKind, u32>,
    pub context_tier: ContextTier,
    pub context_tokens: u32,
    pub context_limit: u32,
    pub idle_chat_turns: u32,
    pub has_orphan_tool_uses: bool,
    pub turn: u32,
    pub tool_loop_count: u32,
    pub trust_score: u32,
}

impl DsAgentsHealthPlatform {
    pub fn new() -> Self {
        DsAgentsHealthPlatform {
            tool_calls_this_turn: 0,
            consecutive_tool_only_turns: 0,
            error_counts: HashMap::new(),
            context_tier: ContextTier::Premium,
            context_tokens: 0,
            context_limit: 0,
            idle_chat_turns: 0,
            has_orphan_tool_uses: false,
            turn: 0,
            tool_loop_count: 0,
            trust_score: 100,
        }
    }

    pub fn record_error(&mut self, _tool: &str, _raw_result: &str) {
        // Stub
    }

    pub fn record_tool_outcome(&mut self, _name: &str, _success: bool) {
        // Stub
    }

    pub fn record_intent_compliance(&mut self, _path: &str, _declared: bool, _is_critical: bool) {
        // Stub
    }

    pub fn intent_note(&self, _path: &str) -> Option<String> {
        None
    }

    pub fn track_tool(&mut self, _name: &str, _args: &str) -> Option<String> {
        None
    }

    pub fn reset_turn(&mut self) {
        self.tool_calls_this_turn = 0;
    }

    pub fn record_turn(&mut self, _had_errors: bool) {
        self.turn += 1;
    }

    pub fn render_health(&self) -> String {
        format!(
            "level:{:?} turn:{} tier:{:?} tokens:{}/{}",
            HealthLevel::Green,
            self.turn,
            self.context_tier,
            self.context_tokens,
            self.context_limit,
        )
    }

    pub fn assess(&self) -> Assessment {
        Assessment::default()
    }

    pub fn record_tool_call(&mut self, _name: &str) {
        self.tool_calls_this_turn += 1;
    }

    pub fn record_api_error(&mut self) {}

    pub fn record_api_success(&mut self, _model: &str) {}

    pub fn should_escalate(&self) -> Option<String> {
        None
    }

    pub fn should_block(&self, _tool_name: &str) -> Option<String> {
        None
    }

    pub fn should_throttle(&self, _tool_name: &str) -> Option<String> {
        None
    }
}

// ── Assessment ──

#[derive(Debug, Clone)]
pub struct Assessment {
    pub level: HealthLevel,
    pub emotion: AgentEmotion,
    pub advice: Option<String>,
    pub interrupt: Option<String>,
    pub success_rate: f64,
}

impl Default for Assessment {
    fn default() -> Self {
        Assessment {
            level: HealthLevel::Green,
            emotion: AgentEmotion::Calm,
            advice: None,
            interrupt: None,
            success_rate: 1.0,
        }
    }
}

// ── Free functions ──

pub fn update_health_report(_report: String) {
    // Stub
}

// ── Gate module ──

pub mod gate {
    
    use crate::assembly::ContextAssembler;

    pub struct GateContext<'a> {
        pub assembler: &'a ContextAssembler,
        pub has_orphan_tool_uses: bool,
    }

    pub fn check_gate(_ctx: &GateContext) -> GateResult {
        GateResult::Pass
    }

    pub fn validate_anthropic_messages(
        _: &[dsx_types::Message],
    ) -> Result<(), GateResult> {
        Ok(())
    }

    pub fn quick_check(_assembler: &ContextAssembler) -> Result<(), String> {
        Ok(())
    }

    #[derive(Debug, Clone)]
    pub enum GateResult {
        Pass,
        Block {
            reason: String,
            repairable: bool,
        },
    }

    impl GateResult {
        pub fn is_pass(&self) -> bool {
            matches!(self, GateResult::Pass)
        }
        pub fn is_block(&self) -> bool {
            matches!(self, GateResult::Block { .. })
        }
    }
}

// ── Monitor module ──

pub mod monitor {
    use super::*;

    #[derive(Debug, Clone)]
    pub struct MonitorState {
        pub tool_calls_this_turn: u32,
        pub consecutive_tool_turns: u32,
        pub tool_fail_counts: HashMap<String, u32>,
        pub disabled_tools: Vec<String>,
        pub tool_trail: Vec<(String, String)>,
        pub tool_loop_count: u32,
        pub reasoning_sample: Vec<String>,
        pub content_buffer: String,
    }

    impl MonitorState {
        pub fn new() -> Self {
            MonitorState {
                tool_calls_this_turn: 0,
                consecutive_tool_turns: 0,
                tool_fail_counts: HashMap::new(),
                disabled_tools: Vec::new(),
                tool_trail: Vec::new(),
                tool_loop_count: 0,
                reasoning_sample: Vec::new(),
                content_buffer: String::new(),
            }
        }
    }

    impl Default for MonitorState {
        fn default() -> Self {
            Self::new()
        }
    }

    #[derive(Debug, Clone)]
    pub enum PreToolResult {
        Pass,
        Block { reason: String },
        Warn { reason: String },
    }

    pub fn pre_tool_gate(
        _tool_name: &str,
        _args: &str,
        _monitor: &MonitorState,
    ) -> PreToolResult {
        PreToolResult::Pass
    }

    pub fn post_tool_record(
        _tool_name: &str,
        _success: bool,
        _monitor: &mut MonitorState,
    ) {
    }

    pub fn record_tool_call(_state: &mut MonitorState, _tool_name: &str) {}

    pub fn reset_turn(state: &mut MonitorState) {
        state.reasoning_sample.clear();
        state.content_buffer.clear();
        state.tool_calls_this_turn = 0;
    }
}
