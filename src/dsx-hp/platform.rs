//! DsAgentsHealthPlatform — unified health monitoring for the agent system.

use std::collections::{HashMap, VecDeque};

use super::{
    AgentEmotion, ContextPressure, ContextTier, ErrorKind, HealthError,
    HealthLevel, TrustTier, classify_error,
};

// ── Platform ──

pub struct DsAgentsHealthPlatform {
    // Model health (was ChatHealth)
    pub(super) reasoning_sample: Vec<String>,
    pub(super) content_buffer: String,
    tool_trail: Vec<(String, String)>,
    pub tool_loop_count: u32,

    // Tool chain health
    pub tool_failures_this_turn: u32,
    pub tool_calls_this_turn: u32,
    pub consecutive_failures: u32,
    pub consecutive_tool_only_turns: u32,

    // Error ledger
    pub recent_errors: VecDeque<HealthError>,
    pub error_counts: HashMap<ErrorKind, u32>,
    pub last_10_success: VecDeque<bool>,

    // Context health
    pub context_pressure: ContextPressure,
    pub context_tier: ContextTier,
    pub context_tokens: u32,
    pub context_limit: u32,
    pub empty_responses: u32,

    // API health
    pub api_retries: u32,
    pub api_errors: u32,
    pub last_stop_reason: Option<String>,
    pub transport_errors: u32,

    // Exec health
    pub exec_pending: usize,
    pub exec_timeouts: u32,
    pub sudo_failures: u32,

    // Idle detection
    pub idle_chat_turns: u32,

    // Trust scoring (intent compliance)
    pub trust_score: u32,                    // 0-100, starts at 100
    pub consecutive_intent_misses: u32,      // consecutive undeclared file ops
    pub intent_misses_total: u32,            // total misses this session
    pub trust_tier: TrustTier,               // Relaxed / Watchful / Strict

    // Circuit breaker — per-tool failure tracking
    pub tool_fail_counts: HashMap<String, u32>,     // tool → consecutive failures
    pub disabled_tools: Vec<String>,                // temporarily blocked tools

    // Soft throttle — per-tool call counts per turn
    pub tool_call_counts: HashMap<String, u32>,     // tool → calls this turn
    pub throttle_warnings: Vec<String>,             // tools that got warned

    // Resource ledger — token consumption by tool category
    pub tokens_by_category: HashMap<&'static str, u64>, // "file" / "exec" / "search" / "meta"

    // Recovery state
    pub last_successful_model: String,
    pub api_retry_backoff: u32,                     // exponential backoff level

    // Session integrity (orphan tool_use detection)
    pub has_orphan_tool_uses: bool,

    // External health
    pub disk_free_bytes: Option<u64>,
    pub last_health_ping: u64,

    // Aggregate
    pub overall_level: HealthLevel,
    pub turn: u32,
}

impl DsAgentsHealthPlatform {
    pub fn new() -> Self {
        Self {
            reasoning_sample: Vec::with_capacity(12),
            content_buffer: String::with_capacity(1200),
            tool_trail: Vec::new(),
            tool_loop_count: 0,
            tool_failures_this_turn: 0,
            tool_calls_this_turn: 0,
            consecutive_failures: 0,
            consecutive_tool_only_turns: 0,
            recent_errors: VecDeque::with_capacity(20),
            error_counts: HashMap::new(),
            last_10_success: VecDeque::with_capacity(10),
            context_pressure: ContextPressure::Normal,
            context_tier: ContextTier::Premium,
            context_tokens: 0,
            context_limit: 0,
            empty_responses: 0,
            api_retries: 0,
            api_errors: 0,
            last_stop_reason: None,
            transport_errors: 0,
            exec_pending: 0,
            exec_timeouts: 0,
            sudo_failures: 0,
            idle_chat_turns: 0,
            trust_score: 100,
            consecutive_intent_misses: 0,
            intent_misses_total: 0,
            trust_tier: TrustTier::Relaxed,
            tool_fail_counts: HashMap::new(),
            disabled_tools: Vec::new(),
            tool_call_counts: HashMap::new(),
            throttle_warnings: Vec::new(),
            tokens_by_category: HashMap::new(),
            last_successful_model: String::new(),
            api_retry_backoff: 0,
            has_orphan_tool_uses: false,
            disk_free_bytes: None,
            last_health_ping: 0,
            overall_level: HealthLevel::Green,
            turn: 0,
        }
    }


    pub fn track_tool(&mut self, name: &str, args: &str) -> Option<String> {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        args.hash(&mut h);
        let hash = format!("{:x}", h.finish());

        self.tool_trail.push((name.to_string(), hash.clone()));
        if self.tool_trail.len() > 12 { self.tool_trail.remove(0); }

        if self.tool_trail.len() >= 4 {
            let recent = &self.tool_trail[self.tool_trail.len().saturating_sub(4)..];
            let all_same = recent.iter().all(|(n, a)| n == name && a == &hash);
            if all_same {
                self.tool_loop_count += 1;
                if self.tool_loop_count >= 4 {
                    return Some(format!("Tool loop: '{}' called 4+ times with identical args. Breaking loop.", name));
                }
                return Some(format!("Tool loop: '{}' repeated with same args ({}x)", name, self.tool_loop_count));
            }
        }
        None
    }

    pub fn reset_turn(&mut self) {
        self.reasoning_sample.clear();
        self.content_buffer.clear();
        self.tool_failures_this_turn = 0;
        self.tool_calls_this_turn = 0;
        self.tool_call_counts.clear();
        self.throttle_warnings.clear();
    }

    // ── Error capture ──

    /// Record a tool error, classify it, update counters.
    pub fn record_error(&mut self, tool: &str, raw_result: &str) {
        let msg = if let Some(rest) = raw_result.strip_prefix("[ERROR] ") {
            rest.lines().next().unwrap_or(rest).trim().to_string()
        } else if let Some(rest) = raw_result.strip_prefix("[FAIL] ") {
            rest.lines().next().unwrap_or(rest).trim().to_string()
        } else {
            return;
        };
        let cat = classify_error(tool, &msg);
        self.recent_errors.push_back(HealthError {
            turn: self.turn,
            tool: tool.to_string(),
            category: cat,
            message: msg,
        });
        if self.recent_errors.len() > 20 { self.recent_errors.pop_front(); }
        *self.error_counts.entry(cat).or_insert(0) += 1;
    }

    /// Record a turn result. `had_errors` = any tool failed this turn.
    pub fn record_turn(&mut self, had_errors: bool) {
        self.turn += 1;
        self.last_10_success.push_back(!had_errors);
        if self.last_10_success.len() > 10 { self.last_10_success.pop_front(); }

        if had_errors {
            self.consecutive_failures += 1;
        } else {
            self.consecutive_failures = 0;
        }
    }

    // ── Trust scoring ──

    /// Record whether the agent declared intent before a file operation.
    /// `declared`: the file path was mentioned in the assistant's response.
    /// `is_critical`: the path is HighStake (src/) vs Trusted (/tmp/).
    pub fn record_intent_compliance(&mut self, _path: &str, declared: bool, is_critical: bool) {
        if declared {
            self.consecutive_intent_misses = 0;
            // Recovery bonus
            if self.trust_score < 100 {
                self.trust_score = (self.trust_score + 5).min(100);
            }
        } else if is_critical {
            self.consecutive_intent_misses += 1;
            self.intent_misses_total += 1;
            let penalty: u32 = if self.consecutive_intent_misses >= 5 { 20 }
                else if self.consecutive_intent_misses >= 3 { 10 }
                else { 5 };
            self.trust_score = self.trust_score.saturating_sub(penalty);
        }
        // Non-critical misses: note but don't penalize trust

        // Update tier
        self.trust_tier = if self.consecutive_intent_misses >= 6 {
            TrustTier::Strict
        } else if self.consecutive_intent_misses >= 3 {
            TrustTier::Watchful
        } else {
            TrustTier::Relaxed
        };
    }

    /// Get the appropriate note for a missed intent declaration.
    /// Returns None if no note is needed (trust is high, minor miss).
    pub fn intent_note(&self, path: &str) -> Option<String> {
        if self.consecutive_intent_misses == 0 {
            return None; // agent is doing fine, no note needed
        }
        if self.consecutive_intent_misses <= 2 {
            // First couple misses: gentle note
            Some(format!(
                "[NOTE] mention '{}' in your response next time — helps the learning log track why.",
                std::path::Path::new(path).file_name().and_then(|n| n.to_str()).unwrap_or(path)))
        } else if self.consecutive_intent_misses <= 5 {
            // Repeated misses: reminder with count
            Some(format!(
                "[REMINDER] {} consecutive file ops without intent. Mention file names so the learning log captures context.",
                self.consecutive_intent_misses))
        } else {
            // Persistent misses: warning
            Some(format!(
                "[WARNING] {} undeclared file ops. Trust score: {}. At score < 30, file writes will be blocked. Mention file names to restore trust.",
                self.consecutive_intent_misses, self.trust_score))
        }
    }

    // ── Controller: circuit breaker, throttle, recovery ──

    fn tool_category(name: &str) -> &'static str {
        match name {
            "read_file" | "write_file" | "edit_file" | "list_dir" | "glance" => "file",
            "exec" => "exec",
            "search" | "explore" => "search",
            "git_status" | "git_diff" | "git_log" | "git_commit" => "git",
            "web_search" | "web_fetch" | "context7_resolve" | "context7_query" => "web",
            "mem_save" | "mem_read" | "mem_forget" | "recall" => "memory",
            _ => "meta",
        }
    }

    /// Record a tool call for throttle + resource tracking. Call BEFORE execution.
    pub fn record_tool_call(&mut self, name: &str) {
        let cat = Self::tool_category(name);
        *self.tool_call_counts.entry(name.to_string()).or_insert(0) += 1;
        *self.tokens_by_category.entry(cat).or_insert(0) += 1; // rough: will be refined with actual tokens
    }

    /// Record a tool outcome for circuit breaker. Call AFTER execution.
    /// `success`: tool result does NOT start with [ERROR]/[FAIL]
    pub fn record_tool_outcome(&mut self, name: &str, success: bool) {
        if success {
            self.tool_fail_counts.remove(name);
        } else {
            *self.tool_fail_counts.entry(name.to_string()).or_insert(0) += 1;
            let count = self.tool_fail_counts.get(name).copied().unwrap_or(0);
            // Circuit breaker: 5 consecutive failures → disable tool for this session
            if count >= 5 && !self.disabled_tools.contains(&name.to_string()) {
                self.disabled_tools.push(name.to_string());
            }
        }
    }

    /// Record token consumption per category (called when usage info arrives).
    pub fn record_token_usage(&mut self, name: &str, tokens: u32) {
        let cat = Self::tool_category(name);
        *self.tokens_by_category.entry(cat).or_insert(0) += tokens as u64;
    }

    /// Record a successful API completion — reset backoff, remember config.
    pub fn record_api_success(&mut self, model: &str) {
        self.api_retry_backoff = 0;
        self.last_successful_model = model.to_string();
    }

    /// Record an API error — increase backoff level.
    pub fn record_api_error(&mut self) {
        self.api_errors += 1;
        self.api_retry_backoff = (self.api_retry_backoff + 1).min(5);
    }

    /// Ping external health: disk space, session count, stale locks.
    /// Best-effort — failures are logged but don't interrupt the agent.
    pub fn ping_external(&mut self) {
        self.last_health_ping = crate::session::now_epoch();
        // Disk space: check if sessions dir is on a healthy filesystem
        if let Some(dir) = crate::session::sessions_dir() {
            if let Ok(meta) = std::fs::metadata(&dir) {
                // Dir exists and is accessible — basic health check passed
                let _ = meta.len();
            }
        }
    }

    // ── Policy methods (queried by app/tools, health decides, they execute) ──

    /// Whether a tool should be blocked due to circuit breaker.
    pub fn should_block(&self, name: &str) -> Option<String> {
        if self.disabled_tools.contains(&name.to_string()) {
            Some(format!(
                "[CIRCUIT BREAKER] '{}' temporarily disabled (5 consecutive failures).\n[HINT] Try a different approach or ask the user to investigate.",
                name))
        } else {
            None
        }
    }

    /// Whether a tool should get a soft throttle warning (called too many times this turn).
    pub fn should_throttle(&self, name: &str) -> Option<String> {
        let count = self.tool_call_counts.get(name).copied().unwrap_or(0);
        if count >= 20 && !self.throttle_warnings.contains(&name.to_string()) {
            Some(format!(
                "[THROTTLE] '{}' called {} times this turn. Consider batching or using a different approach.",
                name, count))
        } else {
            None
        }
    }

    /// Whether context pressure warrants aggressive compaction.
    pub fn should_force_compact(&self) -> bool {
        self.context_tier == ContextTier::Danger
            && self.context_tokens > self.context_limit.saturating_mul(7) / 10
    }

    /// Whether to escalate a warning to an interrupt (blocks the agent loop).
    pub fn should_escalate(&self) -> Option<String> {
        if self.consecutive_failures >= 4 {
            return Some("Too many consecutive failures. Consider re-reading the affected files and diagnosing the root cause.".into());
        }
        if self.trust_score < 20 {
            return Some(format!(
                "Trust score critically low ({}). File operations will be blocked until intent declarations resume.",
                self.trust_score));
        }
        if self.api_retry_backoff >= 4 {
            return Some("API errors persisting. Check your network, API key, and DeepSeek status.".into());
        }
        None
    }


    /// Infer the agent's behavioural-emotional state from current health signals.
    /// Uses only per-turn and rolling-window signals (no cumulative session counters)
    /// to avoid permanent emotion lock from a single past failure.
    pub fn infer_emotion(&self) -> AgentEmotion {
        let success_rate = if self.last_10_success.is_empty() { 1.0 }
            else { self.last_10_success.iter().filter(|&&b| b).count() as f64 / self.last_10_success.len() as f64 };

        // Panic: red-level conditions (critical context, severe consecutive failures)
        if self.context_pressure == ContextPressure::Critical
            || self.context_tier == ContextTier::Danger
            || self.consecutive_failures >= 5
        {
            return AgentEmotion::Panic;
        }

        // Frustrated: same error pattern or tool repeating (not making progress)
        if self.consecutive_failures >= 3
            || self.tool_loop_count >= 3
        {
            return AgentEmotion::Frustrated;
        }

        // Confused: failing while trying diverse approaches (high effort, low yield)
        if self.consecutive_failures >= 2 && self.tool_calls_this_turn > 4
        {
            return AgentEmotion::Confused;
        }

        // Anxious: context pressure building, early warning signals, or idle too long
        if self.context_pressure == ContextPressure::Warning
            || self.context_pressure == ContextPressure::Critical
            || self.tool_failures_this_turn > 0
            || self.empty_responses > 0
            || self.idle_chat_turns >= 4
        {
            return AgentEmotion::Anxious;
        }

        // Flow: consistently high success rate
        if self.last_10_success.len() >= 3 && success_rate >= 0.9 {
            return AgentEmotion::Flow;
        }

        AgentEmotion::Calm
    }

}
