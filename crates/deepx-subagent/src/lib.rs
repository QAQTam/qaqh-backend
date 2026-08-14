//! deepx-subagent — spawn sub-agent tool for the DeepX agent (Ringing V1).
//!
//! The subagent is an **isolated Ringing session**, not a raw child process:
//!
//! 1. `spawn_subagent` connects to the local daemon (Ringing client), calls
//!    the `subagent.spawn` action, and the daemon starts a dedicated worker
//!    (`deepx agent --seed <sub> --ephemeral --tools […]`) with its own
//!    isolated context and a restricted tool allowlist.
//! 2. The parent attaches the sub-seed and sends the task via the ordinary
//!    `ConversationSendMessage` Ringing command.
//! 3. A background collector thread watches the conversation SSE stream for
//!    `TurnCompleted` / `TurnFailed` / `ConversationCancelled` and records the
//!    final answer into the shared [`ProcessRegistry`] — the existing
//!    `process check|wait|kill` tools then work unchanged.
//!
//! Supports model override (different model/provider per subagent), context
//! sharing, per-instance naming, and timeout/cancel semantics.
//!
//! ## Registration
//!
//! Call `deepx_subagent::register(&mut tool_manager)` during agent
//! initialization (the subagent worker itself does this via
//! `AgentState::init_subagent`) to register the `spawn_subagent` tool.

use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use deepx_client::{
    ActionRequest, Client, ClientHandlers, ClientOptions, CommandOptions, ConversationEvent,
    EventBatch, RingingCommand, RingingEvent,
};
use deepx_workspace::{ToolCallCtx, ToolHandler, ToolManager, ToolResult, ToolRisk};

/// Run a future on the shared deepx-client tokio runtime. Safe from any
/// non-tokio thread (tool handlers and collector threads are std threads).
fn rt_block_on<F: std::future::Future>(fut: F) -> F::Output {
    deepx_client::runtime_handle().block_on(fut)
}

/// 子代理固定身份提示：注入到子代理任务文本的 `[SYSTEM]` 段。
/// 子代理的 base system prompt（`backend_prompt.md`）与主代理同源（同 config
/// 加载），前缀天然一致、可命中 provider 前缀缓存；本段补充子代理专属身份约束。
const SUBAGENT_IDENTITY_PROMPT: &str = "\
你现在是工作于DeepX的子代理工程师，你被要求严格执行主coding agents的一切要求，\
不得擅自违背未经允许的操作，并且忠实地把主代理的任务精准完成。";

pub fn register(mgr: &mut ToolManager) {
    mgr.register(ToolHandler {
        key: "spawn_subagent".to_string(),
        description: "Spawn a sub-agent to handle a focused task independently. \
            The subagent runs as an isolated Ringing session with its own context \
            and the parent's tool set (restricted by user settings). Returns a \
            process_id immediately for optional kill/check tracking; the final \
            answer is automatically injected into your conversation as a \
            [SUBAGENT ...] system message when the subagent finishes - do NOT \
            poll with `process wait`, just continue after the injection arrives. \
            Use for complex multi-step sub-tasks that benefit from isolation. \
            `agent_name` should be a verb+task phrase (e.g. 'explore_task').",
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "task_description": {"type": "string", "description": "Short description of the task for the subagent."},
                "agent_name": {"type": "string", "description": "Name for this subagent, verb+task phrase (e.g. 'explore_task', 'review_code')."},
                "context": {"type": "string", "description": "Optional background context to hand to the subagent before the task."},
                "timeout_secs": {"type": "integer", "description": "Maximum time in seconds before the subagent is cancelled. Default 120."}
            },
            "required": ["task_description"],
            "additionalProperties": false
        }),
        handler: handle_spawn_subagent,
        risk: ToolRisk::Administrative,
        category: deepx_workspace::permission::ToolCategory::Exec,
        default_timeout: std::time::Duration::from_secs(180),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_subagent_schema_never_accepts_api_keys() {
        let mut manager = ToolManager::new();
        register(&mut manager);

        let handler = manager
            .lookup("spawn_subagent")
            .expect("spawn_subagent should be registered");
        let properties = handler.input_schema["properties"]
            .as_object()
            .expect("tool properties should be an object");

        assert!(!properties.contains_key("api_key"));
        assert!(!handler.input_schema.to_string().contains("--api-key"));
    }

    #[test]
    fn spawn_subagent_schema_exposes_only_the_llm_facing_params() {
        let mut manager = ToolManager::new();
        register(&mut manager);
        let handler = manager
            .lookup("spawn_subagent")
            .expect("spawn_subagent should be registered");
        let properties = handler.input_schema["properties"]
            .as_object()
            .expect("tool properties should be an object");

        // 模型面只有 4 个参数；system_prompt / tools / model / base_url /
        // max_tokens 不再暴露（由设置页配置或内置身份提示提供）。
        let expected: Vec<&str> = vec!["task_description", "agent_name", "context", "timeout_secs"];
        assert_eq!(properties.len(), expected.len());
        for key in expected {
            assert!(properties.contains_key(key), "missing {key}");
        }
        for legacy in ["system_prompt", "tools", "model", "base_url", "max_tokens", "name", "task"] {
            assert!(!properties.contains_key(legacy), "{legacy} must not be exposed");
        }
        assert_eq!(
            handler.input_schema["required"][0],
            "task_description",
            "task_description must be the only required param"
        );
    }

    #[test]
    fn task_builder_injects_identity_and_wraps_context() {
        // 无 context：身份提示 + 任务，无 context 段。
        let bare = build_subagent_task("do the thing", "");
        assert!(bare.contains("[SYSTEM]"));
        assert!(bare.contains(SUBAGENT_IDENTITY_PROMPT));
        assert!(bare.contains("[TASK]\ndo the thing"));
        assert!(!bare.contains("[CONTEXT]"));

        // 有 context：必须包裹在 <main_subagent_message> 内，防止子代理把
        // 传入内容误判为自身 user 消息而直接修改项目。
        let with_ctx = build_subagent_task("review the diff", "repo at F:\\proj\nbranch main");
        assert!(with_ctx.contains("<main_subagent_message>\nrepo at F:\\proj\nbranch main\n</main_subagent_message>"));
        assert!(with_ctx.contains("[TASK]\nreview the diff"));
        // 身份提示在前，任务在后。
        assert!(with_ctx.find("[SYSTEM]").unwrap() < with_ctx.find("[TASK]").unwrap());
    }
}

/// 构造子代理任务文本：固定身份提示（`[SYSTEM]`）+ 显式包裹的上下文
/// （`<main_subagent_message>`，防止子代理把传入内容当作自己的 user 消息
/// 而直接动项目）+ 任务（`[TASK]`）。
fn build_subagent_task(task_description: &str, context: &str) -> String {
    let mut parts = vec![format!("[SYSTEM]\n{SUBAGENT_IDENTITY_PROMPT}")];
    if !context.trim().is_empty() {
        parts.push(format!(
            "[CONTEXT]\n<main_subagent_message>\n{}\n</main_subagent_message>",
            context.trim()
        ));
    }
    parts.push(format!("[TASK]\n{}", task_description.trim()));
    parts.join("\n\n")
}

fn handle_spawn_subagent(ctx: ToolCallCtx) -> ToolResult {
    let name: String = ctx
        .args
        .get("agent_name")
        .and_then(|v| v.as_str())
        .map(String::from)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "sub".to_string());
    let task: String = ctx
        .args
        .get("task_description")
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_default();
    let context: String = ctx
        .args
        .get("context")
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_default();

    // 模型面只暴露 4 个参数；工具白名单 / 模型 / base-url / max-tokens /
    // 超时默认值一律取自用户设置（cfg.subagent.*，前端设置页可调），
    // 空值=继承主代理。
    let (tools, model_override, base_url_override, max_tokens, cfg_timeout) =
        deepx_config::Config::load()
            .ok()
            .map(|cfg| {
                (
                    cfg.subagent.default_tools.clone(),
                    cfg.subagent.model.clone(),
                    cfg.subagent.base_url.clone(),
                    cfg.subagent.max_tokens,
                    cfg.subagent.timeout_secs,
                )
            })
            .unwrap_or_default();
    let timeout_secs: u64 = ctx
        .args
        .get("timeout_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(cfg_timeout.max(1))
        .clamp(1, 3600);

    if task.trim().is_empty() {
        return ToolResult::error(deepx_workspace::json_err(
            "MISSING_TASK",
            "spawn_subagent: task_description is required",
            "Provide a task description.",
        ));
    }
    let task_text = build_subagent_task(&task, &context);

    // ── 1. Connect to the local daemon (the parent worker is daemon-spawned,
    //       so the discovery record is guaranteed live). ──
    let (batch_tx, batch_rx) = mpsc::channel::<EventBatch>();
    let handlers = ClientHandlers {
        on_batch: Arc::new(move |batch| {
            let _ = batch_tx.send(batch);
        }),
        ..Default::default()
    };
    let client = match Client::connect(ClientOptions {
        handlers,
        launch_daemon_if_missing: false,
        daemon_path: None,
        start_timeout: Duration::from_secs(8),
    }) {
        Ok(c) => c,
        Err(e) => {
            return ToolResult::error(deepx_workspace::json_err(
                "DAEMON_CONNECT",
                &format!("spawn_subagent: cannot connect to daemon: {e}"),
                "Ensure the DeepX daemon is running.",
            ));
        }
    };

    // ── 2. Ask the daemon to spawn an isolated subagent worker. ──
    let seed: String = {
        // 子代理继承主代理的工作区：CURRENT_WORKSPACE 为空/`.` 时不传，
        // daemon 侧同样跳过继承（子 worker 退化为 daemon cwd）。
        let parent_workspace = deepx_workspace::CURRENT_WORKSPACE
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let workspace = if parent_workspace.is_empty() || parent_workspace == "." {
            None
        } else {
            Some(parent_workspace)
        };
        let request = ActionRequest::SubagentSpawn {
            tools: tools.clone(),
            model: if model_override.is_empty() {
                None
            } else {
                Some(model_override.clone())
            },
            base_url: if base_url_override.is_empty() {
                None
            } else {
                Some(base_url_override.clone())
            },
            max_tokens: if max_tokens == 0 || max_tokens == 4096 {
                None
            } else {
                Some(max_tokens)
            },
            workspace,
        };
        match rt_block_on(client.action(request)) {
            Ok(value) => match value.get("seed").and_then(|v| v.as_str()) {
                Some(seed) if !seed.is_empty() => seed.to_string(),
                _ => {
                    client.close();
                    return ToolResult::error(deepx_workspace::json_err(
                        "SPAWN_ERROR",
                        "spawn_subagent: daemon returned no seed",
                        "Check daemon logs.",
                    ));
                }
            },
            Err(e) => {
                client.close();
                return ToolResult::error(deepx_workspace::json_err(
                    "SPAWN_ERROR",
                    &format!("spawn_subagent: daemon rejected spawn: {e}"),
                    "Check that the daemon can start agent workers.",
                ));
            }
        }
    };
    log::info!("[SUBAGENT] '{name}' worker seed={seed}");

    // ── 3. Attach the sub-seed and send the task (ordinary Ringing flow). ──
    if let Err(e) = rt_block_on(client.attach(&seed)) {
        client.close();
        return ToolResult::error(deepx_workspace::json_err(
            "ATTACH_ERROR",
            &format!("spawn_subagent: attach {seed}: {e}"),
            "Check daemon lease state.",
        ));
    }
    let send = RingingCommand::Conversation(
        deepx_client::ConversationCommand::ConversationSendMessage {
            text: task_text,
            images: vec![],
            attachments: None,
            as_system: false,
        },
    );
    if let Err(e) = rt_block_on(client.send_command(Some(&seed), send, CommandOptions::default())) {
        client.close();
        return ToolResult::error(deepx_workspace::json_err(
            "SEND_ERROR",
            &format!("spawn_subagent: send task: {e}"),
            "Check daemon/worker logs.",
        ));
    }

    // ── 4. Register the process and collect the result in the background,
    //        reusing the existing ProcessRegistry (process check/wait/kill). ──
    //
    // 进程隔离说明：ProcessRegistry 是进程内单例（exec 后台进程在 serve 进程、
    // 子代理在 worker 进程各自从 1 编号），跨进程的 `process wait {id}` 查不到
    // 子代理。因此最终结果经 Ringing 注入主代理会话回传（下方 collect 完成时），
    // ProcessRegistry 仅保留进程内存在性记录。
    let registry_id =
        deepx_workspace::process_registry::ProcessRegistry::register(&format!("subagent:{name}"));
    // 主代理会话 seed：collect 完成后把最终作答注入回主会话（模型下一轮自然看到）。
    let parent_seed = deepx_workspace::runtime::context()
        .map(|ctx| ctx.active_session.clone())
        .unwrap_or_default();
    let name_bg = name.clone();
    let seed_bg = seed.clone();
    std::thread::spawn(move || {
        collect_subagent_result(
            client,
            &seed_bg,
            &name_bg,
            registry_id,
            timeout_secs,
            batch_rx,
            &parent_seed,
        );
    });

    log::info!("[SUBAGENT] '{name}' spawned (seed={seed}, process={registry_id})");
    ToolResult::ok(deepx_workspace::json_ok(serde_json::json!({
        "process_id": registry_id,
        "seed": seed,
        "name": name,
        "content": format!("Subagent '{name}' spawned (process {registry_id}); the final answer will be injected into the conversation as a [SUBAGENT] system message when it completes."),
    })))
}

/// Background collector: watches the conversation SSE stream of the sub-seed
/// until a terminal event, a kill request, or the timeout — mirroring the old
/// stdout-frame collector, but over the Ringing event plane.
fn collect_subagent_result(
    client: Client,
    seed: &str,
    name: &str,
    registry_id: u32,
    timeout_secs: u64,
    batch_rx: mpsc::Receiver<EventBatch>,
    parent_seed: &str,
) {
    use deepx_workspace::process_registry::ProcessRegistry;
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let mut final_answer = String::new();
    let mut exit_code: i32 = 0;
    let mut did_finish = false;
    let mut did_cancel = false;

    while !did_finish && !did_cancel {
        // Kill requested (process kill {id}) → cancel the sub turn.
        if ProcessRegistry::get_info(registry_id)
            .and_then(|info| info.get("status").and_then(|s| s.as_str()).map(String::from))
            .as_deref()
            == Some("killed")
        {
            let cancel = RingingCommand::Conversation(
                deepx_client::ConversationCommand::ConversationCancel { turn_id: None },
            );
            let _ = rt_block_on(client.send_command(Some(seed), cancel, CommandOptions::default()));
            final_answer = format!("[SUBAGENT '{name}' CANCELLED]");
            did_cancel = true;
            break;
        }
        match batch_rx.recv_timeout(Duration::from_millis(300)) {
            Ok(batch) => {
                if batch.seed != seed {
                    continue;
                }
                for envelope in batch.envelopes {
                    match envelope.event {
                        RingingEvent::Conversation(ConversationEvent::RoundCompleted {
                            answer,
                            output_ref,
                            is_final,
                            ..
                        }) => {
                            // Prefer the authoritative full answer; fall back to
                            // externalized content when the body is large.
                            if let Some(answer) = answer {
                                if !answer.is_empty() {
                                    final_answer = answer;
                                }
                            } else if let Some(reference) = output_ref {
                                if let Ok(bytes) =
                                    rt_block_on(client.download_content(seed, &reference))
                                {
                                    final_answer =
                                        String::from_utf8_lossy(&bytes).to_string();
                                }
                            }
                            if is_final && !final_answer.is_empty() {
                                did_finish = true;
                            }
                        }
                        RingingEvent::Conversation(ConversationEvent::TurnCompleted { .. }) => {
                            did_finish = true;
                        }
                        RingingEvent::Conversation(ConversationEvent::TurnFailed { error, .. }) => {
                            final_answer =
                                format!("[SUBAGENT '{name}' ERROR] {error:?}");
                            exit_code = 1;
                            did_finish = true;
                        }
                        RingingEvent::Conversation(
                            ConversationEvent::ConversationCancelled { .. },
                        ) => {
                            final_answer = format!("[SUBAGENT '{name}' CANCELLED]");
                            did_cancel = true;
                        }
                        _ => {}
                    }
                    if did_finish || did_cancel {
                        break;
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if Instant::now() >= deadline {
                    let cancel = RingingCommand::Conversation(
                        deepx_client::ConversationCommand::ConversationCancel { turn_id: None },
                    );
                    let _ = rt_block_on(client.send_command(
                        Some(seed),
                        cancel,
                        CommandOptions::default(),
                    ));
                    final_answer = format!(
                        "[SUBAGENT '{name}' TIMEOUT after {timeout_secs}s]"
                    );
                    exit_code = 1;
                    did_finish = true;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // Event stream closed before a terminal event (daemon gone?).
                log::warn!(
                    "[SUBAGENT] '{name}' event stream closed, partial answer_len={}",
                    final_answer.len()
                );
                did_finish = true;
            }
        }
    }

    let answer_len = final_answer.len();

    // ── 结果回传：注入主代理会话（绕开跨进程 ProcessRegistry 隔离）。 ──
    // 主代理 idle 时这条消息触发新回合（模型自动看到子代理结果并继续）；
    // 主代理仍在运行中则进入 inbox，下一轮合并。注入失败仅告警，不影响注册表。
    let header = if did_cancel {
        format!("[SUBAGENT '{name}' CANCELLED]")
    } else if exit_code != 0 {
        format!("[SUBAGENT '{name}' ERROR exit={exit_code}]")
    } else {
        format!("[SUBAGENT '{name}' COMPLETED]")
    };
    if !parent_seed.is_empty() {
        // 401 lease_required 根因：collect 的 client 只 attach 了子 seed，
        // 向主 seed 发命令需先建立 owns 关系（SessionResume）。失败仅告警。
        if let Err(e) = rt_block_on(client.attach(parent_seed)) {
            log::warn!("[SUBAGENT] '{name}' attach parent {parent_seed} for inject: {e}");
        }
        let inject = RingingCommand::Conversation(
            deepx_client::ConversationCommand::ConversationSendMessage {
                text: format!("{header}\n\n{final_answer}"),
                images: vec![],
                attachments: None,
                // 以 system 角色注入（而非 user）：模型可见但不等同于用户输入，
                // 保留 [SUBAGENT ...] 标签供模型区分注入数据与系统指令。
                as_system: true,
            },
        );
        if let Err(e) = rt_block_on(client.send_command(
            Some(parent_seed),
            inject,
            CommandOptions::default(),
        )) {
            log::warn!("[SUBAGENT] '{name}' inject result to parent {parent_seed}: {e}");
        }
    }

    ProcessRegistry::set_answer(registry_id, final_answer);
    ProcessRegistry::mark_exited(registry_id, exit_code);
    client.close();
    log::info!(
        "[SUBAGENT] '{name}' collector complete (seed={seed}), answer_len={answer_len}, exit={exit_code}, cancelled={did_cancel}"
    );
}
