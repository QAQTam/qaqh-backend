//! deepx-subagent — spawn sub-agent tool for the DeepX agent.
//!
//! The subagent is a normal agent process (`deepx agent --seed {seed} --tools [...]`)
//! that auto-creates a session on startup. The parent sends a `UserInput` frame
//! via stdin and reads the response from stdout.
//!
//! Supports model override (different model/provider per subagent), context sharing,
//! and per-instance naming for debugging.
//!
//! ## Registration
//!
//! Call `deepx_subagent::register(&mut tool_manager)` during agent initialization
//! to register the `spawn_subagent` tool.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use deepx_tools::{ToolCallCtx, ToolHandler, ToolManager, ToolResult, ToolRisk};

pub fn register(mgr: &mut ToolManager) {
    mgr.register(ToolHandler {
        key: "spawn_subagent".to_string(),
        description: "Spawn a sub-agent to handle a focused task independently. \
            The subagent has its own isolated context and can use a restricted set of tools. \
            Supports model override for using cheaper/faster models on sub-tasks. \
            Use for complex multi-step sub-tasks that benefit from isolation. \
            Returns the subagent's final answer.",
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "Short name for this subagent (e.g. 'code-reviewer')."},
                "task": {"type": "string", "description": "The task for the subagent to perform. Be specific."},
                "system_prompt": {"type": "string", "description": "Custom system-level instructions for the subagent."},
                "context": {"type": "string", "description": "Optional background context to inject before the task."},
                "tools": {"type": "array", "items": {"type": "string"}, "description": "Tool names the subagent can use. Empty = all tools."},
                "model": {"type": "string", "description": "Override model (e.g. 'gpt-4o-mini'). Inherits parent if empty."},
                "base_url": {"type": "string", "description": "Override API base URL. Inherits parent if empty."},
                "api_key": {"type": "string", "description": "Override API key. Inherits parent if empty."},
                "max_tokens": {"type": "integer", "description": "Max output tokens. Default 4096."},
                "timeout_secs": {"type": "integer", "description": "Maximum time in seconds. Default 120."}
            },
            "required": ["task"],
            "additionalProperties": false
        }),
        handler: handle_spawn_subagent,
        risk: ToolRisk::Administrative,
        default_timeout: std::time::Duration::from_secs(180),
    });
}

fn handle_spawn_subagent(ctx: ToolCallCtx) -> ToolResult {
    let name: String = ctx
        .args
        .get("name")
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_else(|| "sub".to_string());
    let task: String = ctx
        .args
        .get("task")
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_default();
    let system_prompt: String = ctx
        .args
        .get("system_prompt")
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_default();
    let context: String = ctx
        .args
        .get("context")
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_default();
    let tools: Vec<String> = ctx
        .args
        .get("tools")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let model_override: String = ctx
        .args
        .get("model")
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_default();
    let base_url_override: String = ctx
        .args
        .get("base_url")
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_default();
    let api_key_override: String = ctx
        .args
        .get("api_key")
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_default();
    let max_tokens: u32 = ctx
        .args
        .get("max_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(4096) as u32;
    let _timeout_secs: u64 = ctx
        .args
        .get("timeout_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(120);

    if task.is_empty() {
        return ToolResult {
            success: false,
            content: deepx_tools::json_err(
                "MISSING_TASK",
                "spawn_subagent: task is required",
                "Provide a task description.",
            ),
        };
    }

    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            return ToolResult {
                success: false,
                content: deepx_tools::json_err(
                    "EXE_ERROR",
                    &format!("spawn_subagent: cannot get exe path: {e}"),
                    "Check the installation.",
                ),
            };
        }
    };

    let parent_seed = deepx_tools::CURRENT_SESSION
        .lock()
        .ok()
        .and_then(|g| g.clone())
        .unwrap_or_default();
    let sub_seed = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        parent_seed.hash(&mut h);
        name.hash(&mut h);
        task.hash(&mut h);
        format!("{:08x}", h.finish())
    };

    let task_text = if system_prompt.is_empty() && context.is_empty() {
        task
    } else if !system_prompt.is_empty() && context.is_empty() {
        format!("[SYSTEM]\n{system_prompt}\n\n[TASK]\n{task}")
    } else if system_prompt.is_empty() && !context.is_empty() {
        format!("[CONTEXT]\n{context}\n\n[TASK]\n{task}")
    } else {
        format!("[SYSTEM]\n{system_prompt}\n\n[CONTEXT]\n{context}\n\n[TASK]\n{task}")
    };

    let tools_json = serde_json::to_string(&tools).unwrap_or_default();
    let registry_id =
        deepx_tools::process_registry::ProcessRegistry::register(&format!("subagent:{}", name));

    let mut cmd = Command::new(&exe);
    cmd.arg("subagent")
        .arg("--seed")
        .arg(&sub_seed)
        .arg("--tools")
        .arg(&tools_json)
        .arg("--ephemeral");
    if !model_override.is_empty() {
        cmd.arg("--model").arg(&model_override);
    }
    if !base_url_override.is_empty() {
        cmd.arg("--base-url").arg(&base_url_override);
    }
    if !api_key_override.is_empty() {
        cmd.arg("--api-key").arg(&api_key_override);
    }
    if max_tokens > 0 && max_tokens != 4096 {
        cmd.arg("--max-tokens").arg(max_tokens.to_string());
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    log::info!(
        "[SUBAGENT] spawning '{}' seed={} tools={}",
        name,
        &sub_seed[..sub_seed.floor_char_boundary(sub_seed.len().min(8))],
        tools.len()
    );

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return ToolResult {
                success: false,
                content: deepx_tools::json_err(
                    "SPAWN_ERROR",
                    &format!("spawn_subagent: failed to spawn: {e}"),
                    "Check that deepx is installed correctly.",
                ),
            };
        }
    };

    let child_stdin = match child.stdin.take() {
        Some(s) => s,
        None => {
            return ToolResult {
                success: false,
                content: deepx_tools::json_err(
                    "STDIN_ERROR",
                    "spawn_subagent: failed to get stdin",
                    "Check subagent process.",
                ),
            };
        }
    };
    let child_stdout = match child.stdout.take() {
        Some(s) => s,
        None => {
            return ToolResult {
                success: false,
                content: deepx_tools::json_err(
                    "STDOUT_ERROR",
                    "spawn_subagent: failed to get stdout",
                    "Check subagent process.",
                ),
            };
        }
    };

    deepx_tools::process_registry::ProcessRegistry::attach_child(registry_id, child);

    // Write task to subagent's stdin
    {
        let mut stdin_writer = std::io::BufWriter::new(child_stdin);
        let frame = serde_json::json!({"type": "user_input", "text": task_text});
        let line = serde_json::to_string(&frame).unwrap_or_default();
        if writeln!(stdin_writer, "{}", line).is_err() || stdin_writer.flush().is_err() {
            deepx_tools::process_registry::ProcessRegistry::kill(registry_id);
            return ToolResult {
                success: false,
                content: deepx_tools::json_err(
                    "WRITE_ERROR",
                    "spawn_subagent: failed to write task",
                    "Check subagent process.",
                ),
            };
        }
    }

    let reader = BufReader::new(child_stdout);
    let registry_id_bg = registry_id;
    let name_bg = name.clone();

    // Spawn background thread to collect results
    std::thread::spawn(move || {
        let mut final_answer = String::new();
        let mut exit_code: i32 = 0;
        let mut did_cancel = false;
        let mut did_finish = false; // true only on turn_end
        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => break,
            };
            if line.trim().is_empty() {
                continue;
            }
            let event: serde_json::Value = match serde_json::from_str(&line) {
                Ok(e) => e,
                Err(_) => continue,
            };
            let event_type = event.get("type").and_then(|v| v.as_str()).unwrap_or("");
            match event_type {
                "round_complete" => {
                    // Only capture answer from the FINAL round (is_final=true)
                    // or if no final answer yet, take any non-empty answer
                    let is_final = event
                        .get("is_final")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    let has_tool_calls = event
                        .get("tool_calls")
                        .and_then(|v| v.as_array())
                        .map(|a| !a.is_empty())
                        .unwrap_or(false);
                    // Skip intermediate rounds that have pending tool calls
                    if has_tool_calls && !is_final {
                        continue;
                    }
                    // Prefer `answer` field; fall back to `blocks` text content
                    if let Some(answer) = event.get("answer").and_then(|v| v.as_str()) {
                        if !answer.is_empty() {
                            final_answer = answer.to_string();
                        }
                    }
                    if final_answer.is_empty() {
                        if let Some(blocks) = event.get("blocks").and_then(|v| v.as_array()) {
                            for block in blocks {
                                if block.get("type").and_then(|v| v.as_str()) == Some("text") {
                                    if let Some(content) =
                                        block.get("content").and_then(|v| v.as_str())
                                    {
                                        final_answer.push_str(content);
                                    }
                                }
                            }
                        }
                    }
                }
                "turn_end" => {
                    did_finish = true;
                    if final_answer.is_empty() {
                        if let Some(answer) = event.get("answer").and_then(|v| v.as_str()) {
                            if !answer.is_empty() {
                                final_answer = answer.to_string();
                            }
                        }
                    }
                    break;
                }
                "error" => {
                    if let Some(msg) = event.get("message").and_then(|v| v.as_str()) {
                        final_answer = format!("[SUBAGENT '{}' ERROR] {}", name_bg, msg);
                    }
                    exit_code = 1;
                    did_finish = true; // error is a "clean" exit
                    break;
                }
                "cancelled" => {
                    final_answer = format!("[SUBAGENT '{}' CANCELLED]", name_bg);
                    did_cancel = true;
                    break;
                }
                _ => {}
            }
        }
        let answer_len = final_answer.len();
        deepx_tools::process_registry::ProcessRegistry::set_answer(registry_id_bg, final_answer);
        if did_cancel {
            deepx_tools::process_registry::ProcessRegistry::kill(registry_id_bg);
        } else if did_finish {
            deepx_tools::process_registry::ProcessRegistry::mark_exited(registry_id_bg, exit_code);
        } else {
            // Abnormal exit: pipe broke before turn_end/error/cancelled
            deepx_tools::process_registry::ProcessRegistry::mark_exited(registry_id_bg, -1);
            log::warn!(
                "[SUBAGENT] '{}' abnormal exit (no turn_end), partial answer_len={}",
                name_bg,
                answer_len
            );
        }
        log::info!(
            "[SUBAGENT] '{}' background collection complete, answer_len={}, exit={}",
            name_bg,
            answer_len,
            exit_code
        );
    });

    log::info!(
        "[SUBAGENT] '{}' spawned asynchronously, pid={}",
        name,
        registry_id
    );
    ToolResult {
        success: true,
        content: deepx_tools::json_ok(serde_json::json!({
            "process_id": registry_id,
            "name": name,
            "content": format!("Subagent '{}' spawned successfully.", name),
        })),
    }
}
