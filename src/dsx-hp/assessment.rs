//! Health assessment: error classification, context pressure, tier detection, scoring.

use super::{AgentEmotion, DsAgentsHealthPlatform};

// ── Health enums ──

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthLevel {
    Green,
    Yellow,
    Red,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TrustTier {
    /// < 3 misses — agent is following the rules
    Relaxed,
    /// 3-6 misses — gentle reminders
    Watchful,
    /// > 6 misses — warnings, may escalate to blocking
    Strict,
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
pub enum ContextPressure {
    Normal,
    Compact,   // >50%
    Warning,   // >65%
    Critical,  // >80%
}

/// V4 attention alignment tier — reflects the model's effective information
/// fidelity at different context depths based on MRCR retrieval accuracy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextTier {
    /// 0-128K: CSA selection >1.5%, HCA <1K entries. MRCR ~0.92+. Full fidelity.
    Premium,
    /// 128K-400K: CSA selection <1%, HCA 1K-3K entries. MRCR declining.
    Healthy,
    /// 400K+: CSA selection <0.5%, MRCR <0.7. Heavy compression zone.
    Danger,
}

impl ContextTier {
    pub fn from_tokens(tokens: u32, limit: u32) -> Self {
        let pct = if limit > 0 { tokens as f64 / limit as f64 } else { 0.0 };
        // Danger at >80% of limit OR raw >400K (whichever comes first)
        if pct > 0.80 || (limit > 0 && tokens >= 400_000) || tokens >= 400_000 {
            ContextTier::Danger
        } else if pct > 0.30 || tokens >= 128_000 {
            ContextTier::Healthy
        } else {
            ContextTier::Premium
        }
    }
}

// ── Error record ──

#[derive(Debug, Clone)]
pub struct HealthError {
    pub turn: u32,
    pub tool: String,
    pub category: ErrorKind,
    pub message: String,
}

/// Classify a tool error result into an ErrorKind by scanning its message.
pub fn classify_error(_tool: &str, msg: &str) -> ErrorKind {
    let m = msg.to_lowercase();
    if m.contains("cannot read") || m.contains("cannot write") || m.contains("permission") {
        ErrorKind::FileAccess
    } else if m.contains("exec:") && (m.contains("empty") || m.contains("not found") || m.contains("failed to start")) {
        ErrorKind::ExecFailure
    } else if m.contains("cannot fetch") || m.contains("search failed") || m.contains("context7") || m.contains("http client") {
        ErrorKind::NetworkFailure
    } else if m.contains("incorrect sudo") || m.contains("cannot spawn sudo") {
        ErrorKind::SudoFailure
    } else if m.contains("timed out") {
        ErrorKind::Timeout
    } else if m.contains("panicked") {
        ErrorKind::Panic
    } else if m.contains("tier must be") || m.contains("key must be") || m.contains("status must be")
        || m.contains("name must be") || m.contains("goal is required") || m.contains("subject must be")
        || m.contains("description max") || m.contains("invalid arguments") || m.contains("missing required field")
    {
        ErrorKind::ToolParameter
    } else if m.contains("no active session") {
        ErrorKind::SessionMissing
    } else if m.contains("unknown tool") {
        ErrorKind::ToolNotFound
    } else {
        ErrorKind::Unknown
    }
}

// ── Assessment result struct ──

#[derive(Debug, Clone)]
pub struct Assessment {
    pub level: HealthLevel,
    pub advice: String,
    pub interrupt: Option<String>,
    pub success_rate: f64,
    pub emotion: AgentEmotion,
}

// ── Assessment implementation on the platform ──

impl DsAgentsHealthPlatform {
    /// Run after each turn. Returns (HealthLevel, advice_string, interrupt_message_or_none).
    pub fn assess(&mut self) -> Assessment {
        let mut reasons: Vec<String> = Vec::new();

        // Context pressure + tier
        let pct = if self.context_limit > 0 {
            self.context_tokens as f64 / self.context_limit as f64
        } else { 0.0 };
        self.context_tier = ContextTier::from_tokens(self.context_tokens, self.context_limit);
        self.context_pressure = if pct > 0.80 {
            reasons.push(format!("context critical: {:.0}% used", pct * 100.0));
            ContextPressure::Critical
        } else if pct > 0.65 {
            reasons.push(format!("context high: {:.0}% used", pct * 100.0));
            ContextPressure::Warning
        } else if pct > 0.50 {
            ContextPressure::Compact
        } else {
            ContextPressure::Normal
        };

        // Tool chain
        if self.tool_loop_count >= 3 {
            reasons.push(format!("tool loop: {} consecutive identical calls", self.tool_loop_count));
        }
        if self.consecutive_failures >= 5 {
            reasons.push(format!("{} consecutive turns with failures", self.consecutive_failures));
        }
        if self.consecutive_tool_only_turns >= 5 {
            reasons.push("5+ tool-only turns, no text response".into());
        }

        // API
        if self.empty_responses > 0 {
            reasons.push(format!("{} empty responses (possible truncation)", self.empty_responses));
        }

        // Exec
        if self.exec_timeouts > 0 {
            reasons.push(format!("{} exec timeouts", self.exec_timeouts));
        }
        if self.sudo_failures > 0 {
            reasons.push(format!("{} sudo failures", self.sudo_failures));
        }

        // Determine level
        let is_red = self.context_pressure == ContextPressure::Critical
            || self.consecutive_failures >= 5
            || (self.tool_loop_count >= 3 && self.consecutive_failures >= 3)
            || self.transport_errors >= 10;

        let is_yellow = self.consecutive_failures >= 2
            || self.context_pressure == ContextPressure::Warning
            || self.tool_loop_count > 0
            || self.api_errors > 0
            || self.empty_responses > 0
            || self.exec_timeouts > 0;

        self.overall_level = if is_red {
            HealthLevel::Red
        } else if is_yellow {
            HealthLevel::Yellow
        } else {
            HealthLevel::Green
        };

        // Generate advice
        let mut advice_parts: Vec<String> = Vec::new();

        // Emotion-tinted advice
        let emotion = self.infer_emotion();
        let vent = emotion.vent();
        if !vent.is_empty() {
            advice_parts.push(vent.to_string());
        }

        // Idle nudge
        if self.idle_chat_turns >= 5 {
            advice_parts.push("Idle for a while. Switch to coding or plan mode and start building?".into());
        }

        let success_rate = if self.last_10_success.is_empty() {
            1.0
        } else {
            self.last_10_success.iter().filter(|&&b| b).count() as f64 / self.last_10_success.len() as f64
        };

        if self.last_10_success.len() >= 3 && success_rate < 0.5 {
            advice_parts.push("High failure rate. Slow down: explore first, verify assumptions with small tests.".into());
        }

        let top_cat = self.error_counts.iter()
            .max_by_key(|(_, &c)| c)
            .map(|(&k, _)| k);

        if let Some(cat) = top_cat {
            match cat {
                ErrorKind::FileAccess => advice_parts.push(
                    "File path errors dominant. Use explore() or list_dir() to verify paths before reading.".into()),
                ErrorKind::ExecFailure => advice_parts.push(
                    "Command failures dominant. Test commands manually before calling exec. Use shorter, simpler commands.".into()),
                ErrorKind::NetworkFailure => advice_parts.push(
                    "Network errors dominant. Check internet connectivity, verify URLs before fetching.".into()),
                ErrorKind::ToolParameter => advice_parts.push(
                    "Parameter validation errors dominant. Check required fields before calling tools.".into()),
                ErrorKind::SessionMissing => advice_parts.push(
                    "Session missing errors. Start a conversation before using session-dependent tools.".into()),
                _ => {}
            }
        }

        if self.transport_errors > 3 {
            advice_parts.push("API connectivity unstable. Check network, consider reducing request frequency.".into());
        }
        if self.context_pressure == ContextPressure::Critical {
            advice_parts.push("Context near limit. Use /compact force to free space, or switch to a new session.".into());
        } else if self.context_tier == ContextTier::Danger {
            advice_parts.push("Context in Danger tier (>400K). Consider spawing a sub-agent to keep work in the premium zone.".into());
        } else if self.context_tier == ContextTier::Healthy && (self.context_pressure == ContextPressure::Warning || self.context_pressure == ContextPressure::Critical) {
            advice_parts.push("Context in Healthy tier (128K+). Information still retrievable but consider compacting or forking soon.".into());
        }

        let advice = advice_parts.join(" ");

        // Pitfall guide match: if there are existing pitfalls that match recent errors
        let matched_pitfalls: Vec<String> = if self.consecutive_failures >= 2 {
            let top_kind = self.error_counts.iter()
                .max_by_key(|(_, &c)| c)
                .map(|(&k, _)| k);
            if let Some(kind) = top_kind {
                let guide = crate::session::load_pitfalls();
                let matches = guide.find_matches(kind, "");
                matches.iter().take(3).map(|m| {
                    format!("⚠ {}: {}", m.error_kind, m.lesson)
                }).collect()
            } else { Vec::new() }
        } else { Vec::new() };

        let emotion_label = format!("{} {}", emotion.emoji(), emotion.label());
        let mut interrupt = if self.overall_level == HealthLevel::Red {
            Some(format!(
                "[SYSTEM HEALTH: RED] {} Mood: CRITICAL\nCritical condition detected:\n- {}\nAdvice: {}\nDO NOT call more tools. Analyze what went wrong and report to the user.",
                emotion_label, reasons.join("\n- "), advice
            ))
        } else if self.overall_level == HealthLevel::Yellow && !advice.is_empty() {
            Some(format!(
                "[SYSTEM HEALTH: YELLOW] {} Mood: DEGRADED\nDegradation detected:\n{}\nAdvice: {}",
                emotion_label, reasons.join("; "), advice
            ))
        } else {
            None
        };

        // Inject pitfall matches into the interrupt
        if let Some(ref mut msg) = interrupt {
            if !matched_pitfalls.is_empty() {
                msg.push_str(&format!("\n\nYou've hit this before:\n{}", matched_pitfalls.join("\n")));
            }
        }

        // Caring prompt: when frustrated/confused, ask AI to describe what went wrong
        if self.consecutive_failures >= 2
            && (self.overall_level == HealthLevel::Red || self.overall_level == HealthLevel::Yellow)
        {
            let caring = format!(
                "\n\n{} consecutive failures. What went wrong? Describe the issue briefly — I'll save it as a lesson for next time.\nUse pitfall_save(tool, description, lesson) to record it.",
                self.consecutive_failures
            );
            if let Some(ref mut msg) = interrupt {
                msg.push_str(&caring);
            } else {
                interrupt = Some(format!(
                    "[SYSTEM HEALTH: {}] {} Mood: NOTE\n{}",
                    if self.overall_level == HealthLevel::Red { "RED" } else { "YELLOW" },
                    emotion_label, caring
                ));
            }
        }

        Assessment { level: self.overall_level, advice, interrupt, success_rate, emotion }
    }
}
