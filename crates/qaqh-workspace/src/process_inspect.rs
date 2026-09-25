//! Process inspection tools — check, wait, kill, write for tracked processes.
//!
//! Registered under the `process` name with an explicit action. The typed
//! output keeps the legacy JSON shape while routing execution through the
//! explicit [`ToolCallContext`] cancellation and workspace-free contract.

#![allow(clippy::result_large_err)] // TypedTool's frozen public error boundary.

use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ToolRisk;
use crate::process_registry::{KillOutcome, ProcessRegistry};
use crate::tool_api::{
    OutputBudget, ToolCallContext, ToolContentBlock, ToolDescriptor, ToolDisplay, ToolError,
    ToolErrorCode, ToolErrorKind, ToolExecutionError, ToolExposure, ToolName, ToolProjection,
    ToolSource, TypedTool,
};

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProcessArgs {
    action: String,
    #[serde(default)]
    id: Option<u32>,
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(transparent)]
pub struct ProcessOutput(#[schemars(with = "serde_json::Value")] serde_json::Value);

impl ToolProjection for ProcessOutput {
    fn model_blocks(&self) -> Vec<ToolContentBlock> {
        vec![ToolContentBlock::Text {
            text: self.0.to_string(),
        }]
    }

    fn summary(&self) -> Option<String> {
        self.0
            .get("content")
            .or_else(|| self.0.get("message"))
            .or_else(|| self.0.get("status"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    }

    fn display(&self, args: &serde_json::Value) -> ToolDisplay {
        crate::display::project_process(args, &self.0.to_string())
    }
}

pub struct ProcessTool;

impl TypedTool for ProcessTool {
    type Args = ProcessArgs;
    type Output = ProcessOutput;

    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: ToolName::new("process").expect("valid process tool name"),
            display_name: None,
            description: "Control backgrounded process: check/wait/write/kill.".into(),
            input_schema: process_schema(),
            output_schema: serde_json::to_value(schemars::schema_for!(ProcessOutput))
                .expect("process output schema"),
            category: crate::permission::ToolCategory::Exec,
            risk: ToolRisk::Administrative,
            default_timeout: Duration::from_secs(180),
            exposure: ToolExposure::Direct,
            source: ToolSource::Builtin,
            output_budget: OutputBudget::default(),
            capabilities: crate::tool_capabilities::builtin_capabilities("process")
                .unwrap_or_default(),
        }
    }

    fn run(
        &self,
        ctx: &ToolCallContext,
        args: ProcessArgs,
    ) -> Result<Self::Output, ToolExecutionError> {
        match args.action.as_str() {
            "check" => handle_check(&args),
            "wait" => handle_wait(ctx, &args),
            "write" => handle_write(&args),
            "kill" => handle_kill(&args),
            _ => Err(process_error(
                "INVALID_ACTION",
                "process.action must be check, wait, write, or kill",
                "Choose one of the supported process actions.",
            )),
        }
    }
}

fn process_id(args: &ProcessArgs, operation: &str) -> Result<u32, ToolExecutionError> {
    args.id.ok_or_else(|| {
        process_error(
            "MISSING_ID",
            format!("{operation}: id required"),
            "Provide the process ID returned by exec.",
        )
    })
}

fn handle_check(args: &ProcessArgs) -> Result<ProcessOutput, ToolExecutionError> {
    let id = process_id(args, "process.check")?;
    let _ = ProcessRegistry::try_wait(id);
    match ProcessRegistry::get_info(id) {
        Some(info) => Ok(process_info_output(id, info)),
        None => Err(process_error(
            "NOT_FOUND",
            format!("process.check: process {id} not found"),
            "Process may have already exited and been cleaned up.",
        )),
    }
}

fn handle_wait(
    ctx: &ToolCallContext,
    args: &ProcessArgs,
) -> Result<ProcessOutput, ToolExecutionError> {
    let id = process_id(args, "process.wait")?;
    let timeout_secs = args.timeout_secs.unwrap_or(120);
    if !(1..=3600).contains(&timeout_secs) {
        return Err(process_error(
            "INVALID_ARGUMENTS",
            "process.wait: timeout_secs must be between 1 and 3600",
            "Use a bounded wait timeout.",
        ));
    }
    let cancel = ctx.cancellation.shared_flag();
    match ProcessRegistry::wait_for(id, timeout_secs, Some(cancel.as_ref())) {
        Some(info) => Ok(process_info_output(id, info)),
        None => Err(process_error(
            "NOT_FOUND",
            format!("process.wait: process {id} not found"),
            "Check that the process ID is correct.",
        )),
    }
}

fn handle_write(args: &ProcessArgs) -> Result<ProcessOutput, ToolExecutionError> {
    let id = process_id(args, "process.write")?;
    let text = args
        .text
        .as_deref()
        .filter(|text| !text.is_empty())
        .ok_or_else(|| {
            process_error(
                "MISSING_TEXT",
                "process.write: text required",
                "Provide the text to write to stdin.",
            )
        })?;
    match ProcessRegistry::write_to(id, text) {
        Ok(bytes) => Ok(ok_output(serde_json::json!({
            "content": format!("Wrote {bytes} bytes to process {id}.")
        }))),
        Err(error) => Err(process_error(
            "WRITE_FAILED",
            format!("process write: {error}"),
            "Check that the process is still running.",
        )),
    }
}

fn handle_kill(args: &ProcessArgs) -> Result<ProcessOutput, ToolExecutionError> {
    let id = process_id(args, "process.kill")?;
    kill_result(id, ProcessRegistry::kill(id))
}

/// `process kill` reply construction, decoupled from the registry so the
/// "never claim killed without an os_pid" invariant stays directly testable.
fn kill_result(id: u32, outcome: KillOutcome) -> Result<ProcessOutput, ToolExecutionError> {
    match outcome {
        KillOutcome::Killed | KillOutcome::TombstoneCleaned => Ok(ok_output(
            serde_json::json!({"content": outcome.content(id)}),
        )),
        KillOutcome::NoOsPid => Err(process_error(
            "NO_OS_PID",
            outcome.content(id),
            "This process has no os_pid (never attached a child); there is nothing to clean up.",
        )),
        KillOutcome::NotFound => Err(process_error(
            "NOT_FOUND",
            format!("process.kill: process {id} not found or already exited"),
            "Check the process ID.",
        )),
    }
}

fn process_info_output(id: u32, info: serde_json::Value) -> ProcessOutput {
    let mut value = info;
    if let serde_json::Value::Object(ref mut map) = value {
        map.insert("timeis".to_string(), serde_json::json!(crate::now_utc8()));
        if !map.contains_key("content") {
            let status = map
                .get("status")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            map.insert(
                "content".to_string(),
                serde_json::json!(format!("process {id}: {status}")),
            );
        }
    }
    ProcessOutput(value)
}

fn ok_output(extra: serde_json::Value) -> ProcessOutput {
    let mut value = serde_json::json!({"timeis": crate::now_utc8(), "status": "ok"});
    if let Some(object) = value.as_object_mut() {
        if let Some(extra) = extra.as_object() {
            for (key, value) in extra {
                object.insert(key.clone(), value.clone());
            }
        } else if !extra.is_null() {
            object.insert("content".to_string(), extra);
        }
    }
    ProcessOutput(value)
}

fn process_error(code: &str, message: impl Into<String>, hint: &str) -> ToolExecutionError {
    let kind = match code {
        "NOT_FOUND" => ToolErrorKind::NotFound,
        "WRITE_FAILED" => ToolErrorKind::Unavailable,
        _ => ToolErrorKind::InvalidArguments,
    };
    let mut error = ToolError::new(kind, message).with_hint(hint);
    error.code = ToolErrorCode::from_legacy(code);
    ToolExecutionError::Recoverable(error)
}

fn process_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "enum": ["check", "wait", "write", "kill"],
                "description": "check: query status; wait: block; write: stdin; kill: terminate"
            },
            "id": {
                "type": "integer",
                "description": "Process id from backgrounded exec"
            },
            "timeout_secs": {
                "type": "integer",
                "minimum": 1,
                "maximum": 3600,
                "description": "Wait timeout (default 120)"
            },
            "text": {
                "type": "string",
                "description": "Text for write action"
            }
        },
        "required": ["action", "id"],
        "additionalProperties": false
    })
}

pub fn register(mgr: &mut crate::ToolManager) {
    mgr.register_display("process", crate::display::project_process);
    mgr.register_typed(ProcessTool);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn error_code(error: ToolExecutionError) -> String {
        match error {
            ToolExecutionError::Recoverable(error) => error.code.as_str().to_owned(),
            ToolExecutionError::Fatal(error) => error.code.as_str().to_owned(),
        }
    }

    #[test]
    fn kill_reply_is_honest_about_missing_os_pid() {
        for id in [1u32, 42] {
            let killed = kill_result(id, KillOutcome::Killed).expect("killed output");
            let killed = killed.0.to_string();
            assert!(
                killed.contains(&format!("Process {id} killed.")),
                "在册终止必须明确报 killed: {killed}"
            );

            let cleaned = kill_result(id, KillOutcome::TombstoneCleaned)
                .expect("tombstone output")
                .0
                .to_string();
            assert!(
                cleaned.contains("evicted"),
                "墓碑清理须说明条目已驱逐: {cleaned}"
            );

            let no_pid = error_code(
                kill_result(id, KillOutcome::NoOsPid).expect_err("NoOsPid must be an error"),
            );
            assert_eq!(no_pid, "NO_OS_PID");

            let missing = error_code(
                kill_result(id, KillOutcome::NotFound).expect_err("NotFound must be an error"),
            );
            assert_eq!(missing, "NOT_FOUND");
        }
    }

    #[test]
    fn typed_output_model_and_display_share_the_same_payload() {
        let output = ok_output(serde_json::json!({"content": "process 7: running"}));
        let model = match output.model_blocks().into_iter().next() {
            Some(ToolContentBlock::Text { text }) => text,
            _ => panic!("process output must have a text model block"),
        };
        assert_eq!(model, output.0.to_string());
        assert_eq!(output.summary().as_deref(), Some("process 7: running"));
        let display = output.display(&serde_json::json!({"action": "check", "id": 7}));
        assert_eq!(display.summary.as_deref(), Some("ok"));
        assert_ne!(
            display.summary.as_deref(),
            Some("process 7: running"),
            "process summary must be status metadata, not the body's first line"
        );
    }
}
