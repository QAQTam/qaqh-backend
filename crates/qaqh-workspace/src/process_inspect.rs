//! Process inspection tools — check, wait, kill, write for tracked processes.
//!
//! Registered under the `process` name with an explicit action.
//! These let the LLM inspect long-running exec/subagent processes that
//! hit their timeout, instead of blindly retrying or killing.

use crate::{
    ToolCallCtx, ToolPlacement, ToolResult, ToolRisk,
    process_registry::{KillOutcome, ProcessRegistry},
};

pub fn register(mgr: &mut crate::ToolManager) {
    mgr.register_with_placement(crate::ToolHandler {
        key: "process".into(),
        description: "Control backgrounded process: check/wait/write/kill.",
        input_schema: serde_json::json!({
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
        }),
        handler: handle_process,
        risk: ToolRisk::Administrative,
        category: crate::permission::ToolCategory::Exec,
        default_timeout: std::time::Duration::from_secs(180),
    }, ToolPlacement::Workspace);
}

fn handle_process(ctx: ToolCallCtx) -> ToolResult {
    match ctx.args.get("action").and_then(|value| value.as_str()) {
        Some("check") => handle_check(ctx),
        Some("wait") => handle_wait(ctx),
        Some("write") => handle_write(ctx),
        Some("kill") => handle_kill(ctx),
        _ => ToolResult::error("process.action must be check, wait, write, or kill"),
    }
}

/// Format a process info payload as a flat structured response.
///
/// `ProcessRegistry::get_info` already returns a structured JSON object
/// (id/name/status/exit_code/output…). Serializing it into a `content` string
/// would double-escape it (JSON inside a JSON string), so we keep the
/// fields in the model payload and add a short human-readable summary
/// that the context-fold logic can later replace with a hint.
fn process_info_ok(id: u32, info: serde_json::Value) -> String {
    let mut v = info;
    if let serde_json::Value::Object(ref mut map) = v {
        map.insert("timeis".to_string(), serde_json::json!(crate::now_utc8()));
        if !map.contains_key("content") {
            let status = map.get("status").and_then(|s| s.as_str()).unwrap_or("");
            map.insert(
                "content".to_string(),
                serde_json::json!(format!("process {id}: {status}")),
            );
        }
    }
    v.to_string()
}

fn process_error(code: &str, message: impl Into<String>, hint: &str) -> ToolResult {
    crate::json_err(code, message, hint)
}

fn process_ok(payload: String) -> ToolResult {
    ToolResult::ok(payload)
}

#[allow(clippy::result_large_err)] // 错误装箱属结构塑形，另立项
fn process_id(ctx: &ToolCallCtx, operation: &str) -> Result<u32, ToolResult> {
    match ctx.args.get("id").and_then(|v| v.as_u64()) {
        Some(v) if v <= u32::MAX as u64 => Ok(v as u32),
        _ => Err(process_error(
            "MISSING_ID",
            format!("{operation}: id required"),
            "Provide the process ID returned by exec.",
        )),
    }
}

fn handle_check(ctx: ToolCallCtx) -> ToolResult {
    let id = match process_id(&ctx, "process.check") {
        Ok(id) => id,
        Err(result) => return result,
    };

    // 刷新终态：子进程已退出则立即反映（孙进程持管道时 EOF 不达，
    // 原实现状态停在 running，模型会误以为任务未结束）。
    let _ = ProcessRegistry::try_wait(id);

    match ProcessRegistry::get_info(id) {
        Some(info) => process_ok(process_info_ok(id, info)),
        None => process_error(
            "NOT_FOUND",
            format!("process.check: process {id} not found"),
            "Process may have already exited and been cleaned up.",
        ),
    }
}

fn handle_wait(ctx: ToolCallCtx) -> ToolResult {
    let id = match process_id(&ctx, "process.wait") {
        Ok(id) => id,
        Err(result) => return result,
    };
    let timeout_secs: u64 = ctx
        .args
        .get("timeout_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(120);

    // 取消检查（报告 P1）：process wait 阻塞期间回合被取消时立即返回，
    // 不再阻塞 actor 到 timeout_secs。
    match ProcessRegistry::wait_for(id, timeout_secs, Some(&ctx.cancel)) {
        Some(info) => process_ok(process_info_ok(id, info)),
        None => process_error(
            "NOT_FOUND",
            format!("process.wait: process {id} not found"),
            "Check that the process ID is correct.",
        ),
    }
}

fn handle_kill(ctx: ToolCallCtx) -> ToolResult {
    let id = match process_id(&ctx, "process.kill") {
        Ok(id) => id,
        Err(result) => return result,
    };

    kill_result(id, ProcessRegistry::kill(id))
}

/// `process kill` 的回复构造（与注册表解耦，便于锁定「如实作答」语义）。
///
/// PR #57 reviewer 阻断 ②：`os_pid == None` 的墓碑曾返回 true，于是这里回
/// 「Process N killed.」而实际没清理任何残留孤儿。现在 `NoOsPid` 走
/// `NO_OS_PID` 错误码，文案明确「无可清理的 os_pid」。
fn kill_result(id: u32, outcome: KillOutcome) -> ToolResult {
    match outcome {
        // 成功：如实描述实际发生了什么（在册终止 / 墓碑 os_pid 清理）。
        KillOutcome::Killed | KillOutcome::TombstoneCleaned => process_ok(crate::json_ok(
            serde_json::json!({"content": outcome.content(id)}),
        )),
        // 无 os_pid 可清理：不得谎报「已杀」。
        KillOutcome::NoOsPid => process_error(
            "NO_OS_PID",
            outcome.content(id),
            "This process has no os_pid (never attached a child); there is nothing to clean up.",
        ),
        KillOutcome::NotFound => process_error(
            "NOT_FOUND",
            format!("process.kill: process {id} not found or already exited"),
            "Check the process ID.",
        ),
    }
}

fn handle_write(ctx: ToolCallCtx) -> ToolResult {
    let id = match process_id(&ctx, "process.write") {
        Ok(id) => id,
        Err(result) => return result,
    };
    let text = match ctx.args.get("text").and_then(|v| v.as_str()) {
        Some(t) if !t.is_empty() => t,
        _ => {
            return process_error(
                "MISSING_TEXT",
                "process.write: text required",
                "Provide the text to write to stdin.",
            );
        }
    };

    match ProcessRegistry::write_to(id, text) {
        Ok(n) => process_ok(crate::json_ok(
            serde_json::json!({"content": format!("Wrote {n} bytes to process {id}.")}),
        )),
        Err(e) => process_error(
            "WRITE_FAILED",
            format!("process write: {e}"),
            "Check that the process is still running.",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ToolResult` 无 `Display`：按线上 JSON 形状渲染，断言回复文本。
    fn render(result: &ToolResult) -> String {
        serde_json::to_string(result).expect("ToolResult must serialize")
    }

    /// 阻断②：`process kill` 的回复必须如实——有 os_pid 才说 killed，
    /// 无 os_pid 必须报 `NO_OS_PID` 且文案点明「无 os_pid 可清理」。
    #[test]
    fn kill_reply_is_honest_about_missing_os_pid() {
        for id in [1u32, 42] {
            let killed = render(&kill_result(id, KillOutcome::Killed));
            assert!(
                killed.contains(&format!("Process {id} killed.")),
                "在册终止必须明确报 killed: {killed}"
            );

            let cleaned = render(&kill_result(id, KillOutcome::TombstoneCleaned));
            assert!(
                cleaned.contains("evicted"),
                "墓碑清理须说明条目已驱逐: {cleaned}"
            );

            let text = render(&kill_result(id, KillOutcome::NoOsPid));
            assert!(
                !text.contains(&format!("Process {id} killed.")),
                "无 os_pid 时不得谎报 killed: {text}"
            );
            assert!(
                text.contains("NO_OS_PID") && text.contains("no os_pid to clean up"),
                "无 os_pid 必须报 NO_OS_PID 且文案明确「无 os_pid 可清理」: {text}"
            );

            let missing = render(&kill_result(id, KillOutcome::NotFound));
            assert!(
                missing.contains("NOT_FOUND"),
                "未登记 id 必须报 NOT_FOUND: {missing}"
            );
        }
    }
}
