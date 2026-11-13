//! NOTE: Disconnected from dsx-agent — dsx-agent uses its own stubs in health.rs.
//! This module is used by the dsx-hp binary internally. Wire up via IPC later.
//!
//! Agent emotion — behavioural-emotional state inferred from health signals.

use serde::{Deserialize, Serialize};

/// Behaviour-emotional state inferred from health signals.
/// Light-hearted labels to make health monitoring more human-readable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentEmotion {
    /// Smooth sailing: high success rate, no issues
    Flow,
    /// Normal operation
    Calm,
    /// Context or errors building up
    Anxious,
    /// Same thing failing over and over
    Frustrated,
    /// Repeated failures + trying wildly different approaches
    Confused,
    /// Under pressure, high API errors, tight context
    Panic,
}

impl AgentEmotion {
    pub fn emoji(&self) -> &'static str {
        match self {
            AgentEmotion::Flow => "⚡",
            AgentEmotion::Calm => "·",
            AgentEmotion::Anxious => "⏳",
            AgentEmotion::Frustrated => "💢",
            AgentEmotion::Confused => "❓",
            AgentEmotion::Panic => "🔥",
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

    /// Cathartic advice line when the agent is struggling.
    pub fn vent(&self) -> &'static str {
        match self {
            AgentEmotion::Frustrated => "This thing is being a pain. Take a breath, maybe walk away for a minute, then come back with a fresh angle.",
            AgentEmotion::Confused => "Nothing's working huh. Try explaining the problem out loud — sometimes saying it helps you see it differently.",
            AgentEmotion::Panic => "OK deep breath. Context is running out, errors piling up. Stop. Save your work. /compact, then start fresh.",
            AgentEmotion::Anxious => "Slow down cowboy. Too many things happening at once. Focus on one thing at a time.",
            _ => "",
        }
    }
}
