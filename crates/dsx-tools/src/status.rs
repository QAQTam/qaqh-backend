//! Agent status: phase declaration and state reporting.

use std::sync::atomic::Ordering;

use crate::stubs;
use crate::AUTO_MODE;
use dsx_types::ToolDef;

#[allow(dead_code)]
pub(super) fn status_def() -> ToolDef {
    ToolDef {
        call_type: "function".into(),
        function: dsx_types::ToolFunction {
            name: "status".into(),
            description: "Declare working mode: plan|coding|debug. Controls model+effort. Auto-detected by default.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "state": {
                        "type": "string",
                        "enum": ["plan", "coding", "debug"],
                        "description": "Working mode: plan=analysis+design(read-only), coding=implementation, debug=troubleshooting"
                    },
                    "difficulty": {
                        "type": "string",
                        "enum": ["low", "medium", "high"],
                        "description": "Task difficulty. Only meaningful for debug mode: low=quick fix, medium=careful diagnosis, high=deep investigation. Optional for other modes."
                    }
                },
                "required": ["state"]
            }),
        },
    }
}

pub(super) fn exec_status(args: &str) -> String {
    let (phase, level) = stubs::parse_status_args(args);
    stubs::set_phase(phase, level);

    let auto = AUTO_MODE.load(Ordering::Relaxed);
    if auto {
        let cfg = stubs::phase_config(phase, level);
        let effort_str = cfg.effort.unwrap_or("default");
        format!(
            "[OK] Mode: {:?} (difficulty: {:?}) — auto selects {} · {} · {}K tokens",
            phase, level, cfg.model, effort_str, cfg.max_tokens / 1000
        )
    } else {
        format!(
            "[OK] Mode: {:?} (difficulty: {:?})\n[HINT] Auto-mode is OFF. Model/effort unchanged. Toggle with /auto.",
            phase, level
        )
    }
}

// ── Registration ──

use crate::{ToolHandler, ToolKey, SafetyVerdict, ToolCallCtx, ToolResult};
use std::time::Duration;

fn handle_status(ctx: ToolCallCtx) -> ToolResult {
    ToolResult::ok(exec_status(&serde_json::to_string(&ctx.args).unwrap_or_default()))
}

pub fn register(mgr: &mut crate::ToolManager) {
    mgr.register(ToolHandler {
        key: ToolKey::new("status", ""),
        description: "Declare working mode. plan|coding|debug. Controls model+effort.",
        input_schema: serde_json::json!({
            "type": "object", "properties": {
                "state": {"type": "string", "enum": ["plan", "coding", "debug"]},
                "difficulty": {"type": "string", "enum": ["low", "medium", "high"]}
            }, "required": ["state"], "additionalProperties": false
        }),
        handler: handle_status,
        safety: |_| SafetyVerdict::Allow,
        default_timeout: Duration::from_secs(15),
    });
}
