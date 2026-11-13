//! Agent-side health tracking: error rates, tool outcomes, context pressure, emotion.
//! This is the canonical health implementation (HP's duplicate has been removed).

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

    // TODO: implement
    pub fn record_error(&mut self, _tool: &str, _raw_result: &str) {
        log::warn!("health: record_error is a stub");
    }

    // TODO: implement
    pub fn record_tool_outcome(&mut self, _name: &str, _success: bool) {
        log::warn!("health: record_tool_outcome is a stub");
    }

    // TODO: implement
    pub fn record_intent_compliance(&mut self, _path: &str, _declared: bool, _is_critical: bool) {
        log::warn!("health: record_intent_compliance is a stub");
    }

    pub fn intent_note(&self, _path: &str) -> Option<String> {
        None
    }

    // TODO: implement
    pub fn track_tool(&mut self, _name: &str, _args: &str) -> Option<String> {
        log::warn!("health: track_tool is a stub");
        None
    }

    pub fn reset_turn(&mut self) {
        self.tool_calls_this_turn = 0;
    }

    // TODO: implement error tracking
    pub fn record_turn(&mut self, _had_errors: bool) {
        self.turn += 1;
        log::warn!("health: record_turn ignores _had_errors parameter");
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

    // TODO: implement real health assessment
    pub fn assess(&self) -> Assessment {
        log::warn!("health: assess always returns default (Green/Calm/100%)");
        Assessment::default()
    }

    pub fn record_tool_call(&mut self, _name: &str) {
        self.tool_calls_this_turn += 1;
    }

    // TODO: implement
    pub fn record_api_error(&mut self) {
        log::warn!("health: record_api_error is a stub");
    }

    // TODO: implement
    pub fn record_api_success(&mut self, _model: &str) {
        log::warn!("health: record_api_success is a stub");
    }

    // TODO: implement escalation logic
    pub fn should_escalate(&self) -> Option<String> {
        log::warn!("health: should_escalate always returns None");
        None
    }

    // TODO: implement blocking logic
    pub fn should_block(&self, _tool_name: &str) -> Option<String> {
        log::warn!("health: should_block always returns None");
        None
    }

    // TODO: implement throttling logic
    pub fn should_throttle(&self, _tool_name: &str) -> Option<String> {
        log::warn!("health: should_throttle always returns None");
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

// TODO: implement
pub fn update_health_report(_report: String) {
    log::warn!("health: update_health_report is a stub");
}

// ── Gate module ──

pub mod gate {
    
    use crate::assembly::ContextAssembler;

    pub struct GateContext<'a> {
        pub assembler: &'a ContextAssembler,
        pub has_orphan_tool_uses: bool,
    }

    pub fn check_gate(ctx: &GateContext) -> GateResult {
        // 1. Previous 400 flagged orphan tool_uses → context corruption
        if ctx.has_orphan_tool_uses {
            return GateResult::Block {
                reason: "Orphan tool_uses detected from previous API 400. Context repair needed.".into(),
                repairable: true,
            };
        }

        // 2. Unfulfilled tool calls from previous assistant → premature API request
        if ctx.assembler.has_unfulfilled_tool_calls() {
            return GateResult::Block {
                reason: "Tool calls from previous assistant response not yet satisfied. Cannot start new request.".into(),
                repairable: false,
            };
        }

        // 3. Assembler structural validation (orphan tool_results, alternation)
        if let Err(e) = ctx.assembler.validate() {
            return GateResult::Block {
                reason: format!("Assembler validation failed: {}", e),
                repairable: e.contains("orphan"),
            };
        }

        GateResult::Pass
    }

    /// Validate OpenAI-compatible message array format.
    /// Catches: orphan tool results, broken alternation, duplicate tool_call_ids.
    pub fn validate_anthropic_messages(
        msgs: &[dsx_types::Message],
    ) -> Result<(), GateResult> {
        let mut i = 0;
        while i < msgs.len() {
            match msgs[i].role.as_str() {
                "system" => {
                    // system messages must be at the start
                    if i > 0 && msgs[i-1].role != "system" {
                        return Err(GateResult::Block {
                            reason: "System message after non-system content".into(),
                            repairable: false,
                        });
                    }
                }
                "user" => {
                    // user must follow assistant or another user (first non-system)
                    if i > 0 {
                        let prev = msgs[i-1].role.as_str();
                        if !matches!(prev, "assistant" | "tool" | "user") {
                            return Err(GateResult::Block {
                                reason: format!("User message after '{}' — invalid alternation", prev),
                                repairable: false,
                            });
                        }
                    }
                }
                "assistant" => {
                    if i > 0 {
                        let prev = msgs[i-1].role.as_str();
                        if !matches!(prev, "user" | "tool") {
                            return Err(GateResult::Block {
                                reason: format!("Assistant message after '{}' — expected user or tool", prev),
                                repairable: false,
                            });
                        }
                    }
                    // If assistant has tool_calls, subsequent messages must be tool results
                    if let Some(ref tcs) = msgs[i].tool_calls {
                        if !tcs.is_empty() {
                            // Check each tool_call_id has a matching tool result
                            let mut j = i + 1;
                            while j < msgs.len() && msgs[j].role == "tool" {
                                j += 1;
                            }
                            let tool_results = &msgs[i+1..j];
                            for tc in tcs {
                                let has_result = tool_results.iter().any(|tr| {
                                    tr.tool_call_id.as_deref() == Some(&tc.id)
                                });
                                if !has_result {
                                    return Err(GateResult::Block {
                                        reason: format!("Tool call '{}' ('{}') has no matching tool result", tc.id, tc.function.name),
                                        repairable: true,
                                    });
                                }
                            }
                            // Check for orphan tool results (no matching tool_call)
                            for tr in tool_results {
                                let has_call = tcs.iter().any(|tc| {
                                    tr.tool_call_id.as_deref() == Some(&tc.id)
                                });
                                if !has_call {
                                    return Err(GateResult::Block {
                                        reason: format!("Orphan tool result for '{}' — no matching tool_call", tr.tool_call_id.as_deref().unwrap_or("?")),
                                        repairable: true,
                                    });
                                }
                            }
                        }
                    }
                }
                "tool" => {
                    // tool must follow assistant with matching tool_calls
                    if i == 0 {
                        return Err(GateResult::Block {
                            reason: "Tool message at position 0 with no preceding assistant".into(),
                            repairable: true,
                        });
                    }
                    let prev = &msgs[i-1];
                    if prev.role != "assistant" && prev.role != "tool" {
                        return Err(GateResult::Block {
                            reason: format!("Tool message after '{}' — must follow assistant or tool", prev.role),
                            repairable: true,
                        });
                    }
                }
                other => {
                    return Err(GateResult::Block {
                        reason: format!("Unknown message role: '{}'", other),
                        repairable: false,
                    });
                }
            }
            i += 1;
        }
        Ok(())
    }

    /// Lightweight check for the HP runner path (called before prepare_and_compact).
    pub fn quick_check(assembler: &ContextAssembler) -> Result<(), String> {
        if assembler.has_unfulfilled_tool_calls() {
            return Err("Unfulfilled tool calls in context".into());
        }
        if let Err(e) = assembler.validate() {
            return Err(e);
        }
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

    // TODO: implement tool gating
    pub fn pre_tool_gate(
        _tool_name: &str,
        _args: &str,
        _monitor: &MonitorState,
    ) -> PreToolResult {
        log::warn!("health::monitor: pre_tool_gate always returns Pass");
        PreToolResult::Pass
    }

    // TODO: implement
    pub fn post_tool_record(
        _tool_name: &str,
        _success: bool,
        _monitor: &mut MonitorState,
    ) {
        log::warn!("health::monitor: post_tool_record is a stub");
    }

    // TODO: implement
    pub fn record_tool_call(_state: &mut MonitorState, _tool_name: &str) {
        log::warn!("health::monitor: record_tool_call is a stub");
    }

    pub fn reset_turn(state: &mut MonitorState) {
        state.reasoning_sample.clear();
        state.content_buffer.clear();
        state.tool_calls_this_turn = 0;
    }
}
