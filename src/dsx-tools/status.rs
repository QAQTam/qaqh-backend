//! Agent status: phase declaration and state reporting.

use std::sync::atomic::Ordering;

use crate::router;
use crate::tools::AUTO_MODE;
use crate::types::ToolDef;

#[allow(dead_code)]
pub(super) fn status_def() -> ToolDef {
    ToolDef {
        call_type: "function".into(),
        function: crate::types::ToolFunction {
            name: "status".into(),
            description: "Declare working mode. explore|plan|coding|debug|chat. Controls model+effort. Auto-detected by default.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "state": {
                        "type": "string",
                        "enum": ["explore", "plan", "coding", "debug", "chat"],
                        "description": "Working mode: explore=project scan(read-only), plan=analysis+design(read-only), coding=implementation, debug=troubleshooting, chat=casual"
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
    let (phase, level) = router::parse_status_args(args);
    router::set_phase(phase, level);

    let auto = AUTO_MODE.load(Ordering::Relaxed);
    if auto {
        let cfg = router::phase_config(phase, level);
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
