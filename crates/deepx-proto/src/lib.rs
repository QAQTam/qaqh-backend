//! deepx message protocol — shared frame definitions for agent communication.
//!
//! Every frame is a single-line JSON object (`\n` delimited), tagged with `"type"`.
//!
//! ## Channels
//!
//! | Channel | Transport | Direction |
//! |---------|-----------|-----------|
//! | UI ↔ Agent | stdin/stdout JSON-LP (child process) | Bidirectional |
//! | Agent ↔ Tools | direct call (in-process) | Bidirectional |
//! | Agent → HP | TCP localhost | Bidirectional |
//!
//! ## Submodules
//!
//! - `agent_protocol` — `Ui2Agent` / `Agent2Ui`
//! - `hp` — `AgentToHp` / `HpToAgent`

use serde::{Deserialize, Serialize, Serializer};
use std::fmt;

// ── Submodule declarations ──────────────────────────────────────────────

mod agent_protocol;
mod companion;
mod control;

// ── Re-exports ──────────────────────────────────────────────────────────

pub use agent_protocol::{
    Agent2Ui, AskAnswer, AskMode, AskQuestion, AskResolution, CodeDaily, CodeDeltaRecord,
    DaemonToFrontend, DocInfo, FileSnapshotInfo, FrontendToDaemon, PermissionRisk, RoundBlock,
    RoundData, RoundDeltaKind, SessionActivity, SessionActivityState, SkillInfo, SkillRuntimeInfo,
    SkillsStatus, TaskInfo, TodoActivationItem, ToolCallDef, ToolResultDef, TurnData, Ui2Agent,
};
pub use companion::{
    COMPANION_PROTOCOL_VERSION, CompanionClientMessage, CompanionCommandStatus, CompanionEvent,
    CompanionInteraction, CompanionInteractionKey, CompanionInteractionKind,
    CompanionInteractionPayload, CompanionInteractionResponse, CompanionServerMessage,
    CompanionSession, CompanionSnapshot, CompanionVisualState,
};
pub use control::{
    CONTROL_PROTOCOL_VERSION, ControlClientMessage, ControlServerMessage, ControlSnapshot,
    DaemonDiscovery,
};

// ── Redacted (prevents API key leaks in debug logs) ─────────────────────

/// Wrapper that serializes normally but redacts in Debug output.
/// Prevents API keys from leaking into debug logs.
#[derive(Clone, PartialEq, Eq)]
pub struct Redacted(pub String);

impl Serialize for Redacted {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(s)
    }
}

impl fmt::Debug for Redacted {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if self.0.is_empty() {
            f.write_str("\"\"")
        } else {
            f.write_str("\"***\"")
        }
    }
}

impl<'de> Deserialize<'de> for Redacted {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d).map(Redacted)
    }
}

impl From<&str> for Redacted {
    fn from(s: &str) -> Self {
        Redacted(s.to_string())
    }
}

impl From<String> for Redacted {
    fn from(s: String) -> Self {
        Redacted(s)
    }
}

// ── Frame I/O helpers ──────────────────────────────────────────────────

use std::io::{BufRead, Write};

/// Read a single JSON-LP frame (one line, parse as JSON).
/// Returns Ok(None) on EOF or empty line.
pub fn read_frame<T: serde::de::DeserializeOwned>(
    r: &mut impl BufRead,
) -> std::io::Result<Option<T>> {
    let mut line = String::new();
    let n = r.read_line(&mut line)?;
    if n == 0 || line.trim().is_empty() {
        return Ok(None);
    }
    serde_json::from_str(&line)
        .map(Some)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))
}

/// Write a single JSON-LP frame (one JSON object per line, newline terminated).
/// Flushes after every write for real-time streaming.
pub fn write_frame(w: &mut impl Write, frame: &impl Serialize) -> std::io::Result<()> {
    let json = serde_json::to_string(frame)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    writeln!(w, "{}", json)?;
    w.flush()
}

/// Convenience: write a raw string as a JSON-LP line.
pub fn write_line(writer: &mut impl Write, line: &str) {
    let _unused = writeln!(writer, "{line}");
    let _unused = writer.flush();
}
