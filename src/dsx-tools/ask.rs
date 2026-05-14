//! User interaction: ask_user for clarification questions.

use crate::types::ToolDef;

#[allow(dead_code)]
pub(super) fn ask_user_def() -> ToolDef {
    ToolDef {
        call_type: "function".into(),
        function: crate::types::ToolFunction {
            name: "ask_user".into(),
            description: "Ask user to choose. ⚠ Must be the ONLY tool in this response — other tools will be cancelled.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "question": {"type": "string", "description": "The question"},
                    "options": {"type": "array", "items": {"type": "string"}, "description": "Option list, e.g. [\"A. one\", \"B. two\"]"}
                },
                "required": ["question"],
                "additionalProperties": false
            }),
        },
    }
}

pub(super) fn exec_ask_user(_args: &str) -> String {
    "[ERROR] ask_user should be intercepted by TUI".to_string()
}
