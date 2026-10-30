//! Tool registrar aggregation point for spawned sessions.

use qaqh_workspace::registration::ToolRegistrar;

/// No built-in registrar remains: every tool group registers itself through
/// `qaqh_workspace::registration::build_tool_manager`. The empty array is kept
/// so spawn sites have one named seam instead of an inline `[]`.
pub fn agent_tool_registrars() -> [ToolRegistrar; 0] {
    []
}
