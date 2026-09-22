//! qaqh-subagent — spawn sub-agent tool for the QAQ-Harness agent (Ringing V1).
//!
//! The subagent is an **isolated Ringing session**, not a raw child process:
//!
//! 1. `spawn_subagent` runs the subagent as an in-process actor on a daemon
//!    thread. When the daemon installs an in-process [`SubagentHost`]
//!    (Knife-1 step-2: `QaqhService` via `qaqh_subagent::install_host`), the
//!    tool drives the actor directly through the host handle — no daemon
//!    HTTP/SSE loopback. Without a host (tests / non-daemon embedding) it
//!    falls back to the legacy `subagent.spawn` action over HTTP/SSE.
//! 2. The parent attaches (or directly addresses) the sub-seed and sends the
//!    task via the ordinary `ConversationSendMessage` Ringing command.
//! 3. A background collector thread watches the event stream for
//!    `TurnCompleted` / `TurnFailed` / `ConversationCancelled` and records the
//!    final answer into the shared [`ProcessRegistry`] — the existing
//!    `process check|wait|kill` tools then work unchanged.
//!
//! Supports model override (different model/provider per subagent), context
//! sharing, per-instance naming, and timeout/cancel semantics.
//!
//! ## Registration
//!
//! Call `qaqh_subagent::register(&mut tool_manager)` during agent
//! initialization (the subagent worker itself does this via
//! `AgentState::init_subagent`) to register the `spawn_subagent` tool.

use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use qaqh_domain::{ControlEvent, ConversationCommand, ConversationEvent};
use qaqh_ringing::{RingingCommand, RingingEvent};
// `ContentRef` / `EventBatch`（trait 签名 + transport 事件流）经下方
// `pub use host::{ContentRef, EventBatch, ..}` 引入。
use qaqh_workspace::{ToolCallCtx, ToolHandler, ToolManager, ToolResult, ToolRisk};

mod host;
pub use host::{ContentRef, EventBatch, SubagentHost, host, install_host};

/// 子代理固定身份提示：注入到子代理任务文本的 `[SYSTEM]` 段。
/// 子代理的 base system prompt（`backend_prompt.md`）与主代理同源（同 config
/// 加载），前缀天然一致、可命中 provider 前缀缓存；本段补充子代理专属身份约束。
const SUBAGENT_IDENTITY_PROMPT: &str = "\
You are a subagent engineer working in QAQ-Harness. Follow the main coding agent's \
instructions exactly, never take unauthorized actions, and complete the assigned task faithfully.";

pub fn register(mgr: &mut ToolManager) {
    mgr.register_display("spawn_subagent", project_subagent_display);
    mgr.register(ToolHandler {
        key: "spawn_subagent".to_string(),
        description: "Spawn an isolated subagent for a focused task. Returns process_id; \
            its final answer is injected as a [SUBAGENT] message when done - do not poll. \
            agent_name = verb+task phrase (e.g. 'explore_task').",
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
        category: qaqh_workspace::permission::ToolCategory::Exec,
        default_timeout: std::time::Duration::from_secs(180),
    });
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

/// 子代理进程记录统一存放在 daemon actor 进程的本地注册表中。
enum RegistryRef {
    Local { id: u32 },
}

impl RegistryRef {
    fn id(&self) -> u32 {
        match self {
            RegistryRef::Local { id } => *id,
        }
    }

    /// 是否已被 `process kill` 标记为 killed。
    fn killed(&self) -> bool {
        match self {
            RegistryRef::Local { id } => {
                qaqh_workspace::process_registry::ProcessRegistry::get_info(*id)
                    .and_then(|info| {
                        info.get("status")
                            .and_then(|s| s.as_str())
                            .map(|s| s == "killed")
                    })
                    .unwrap_or(false)
            }
        }
    }

    /// 收尾：写入最终作答与退出码。
    fn finish(&self, answer: &str, exit_code: i32) {
        match self {
            RegistryRef::Local { id } => {
                qaqh_workspace::process_registry::ProcessRegistry::set_answer(
                    *id,
                    answer.to_string(),
                );
                qaqh_workspace::process_registry::ProcessRegistry::mark_exited(*id, exit_code);
            }
        }
    }
}

/// 注册子代理进程记录到本地 actor 进程注册表。
fn register_subagent_process(name: &str) -> RegistryRef {
    let id = qaqh_workspace::process_registry::ProcessRegistry::register(name);
    log::info!("[SUBAGENT] '{name}' registered in local registry id={id}");
    RegistryRef::Local { id }
}

/// 子代理命令/事件传输抽象：宿主直连与 HTTP/SSE 回连共用同一套 collect 流程。
/// 子代理命令/事件传输抽象（PR-4-2：legacy HTTP/SSE 回连已删除，仅宿主直连）。
///
/// - [`HostTransport`]：进程内直连（无 lease / 无 HTTP），由 daemon 装配宿主。
trait SubagentTransport: Send {
    /// 向某 seed 发送命令。返回是否被 accepted。
    fn send_command(&self, seed: &str, command: RingingCommand) -> Result<bool, String>;
    /// 读取外置大内容。
    fn download_content(&self, seed: &str, reference: &ContentRef) -> Result<Vec<u8>, String>;
    /// 建立 attachment / lease（HTTP 路径需要；宿主直连为 no-op）。
    fn attach(&self, seed: &str) -> Result<(), String>;
    /// 关闭客户端连接（宿主直连为 no-op）。
    fn close(&self);
    /// 该 seed 的实时事件批次流。
    fn events(&self) -> &mpsc::Receiver<EventBatch>;
}

/// 宿主直连传输：直接调用进程内宿主（ActorRegistry + RingingHub）。
struct HostTransport {
    host: Arc<dyn SubagentHost>,
    batch_rx: mpsc::Receiver<EventBatch>,
}

impl SubagentTransport for HostTransport {
    fn send_command(&self, seed: &str, command: RingingCommand) -> Result<bool, String> {
        // SessionClose 由 daemon registry 拦截处理（loop_core 会忽略该命令），
        // 进程内宿主直接执行 close（registry + 临时会话清理），语义一致。
        if matches!(
            &command,
            RingingCommand::Control(qaqh_domain::ControlCommand::SessionClose { .. })
        ) {
            self.host.close(seed)?;
            return Ok(true);
        }
        self.host.send_ringing(seed, command)?;
        Ok(true)
    }

    fn download_content(&self, seed: &str, reference: &ContentRef) -> Result<Vec<u8>, String> {
        self.host.download_content(seed, reference)
    }

    fn attach(&self, _seed: &str) -> Result<(), String> {
        Ok(())
    }

    fn close(&self) {}

    fn events(&self) -> &mpsc::Receiver<EventBatch> {
        &self.batch_rx
    }
}

fn project_subagent_display(
    args: &serde_json::Value,
    output: &str,
) -> qaqh_workspace::tool_api::ToolDisplay {
    use qaqh_workspace::tool_api::{ToolBody, ToolDisplay, ToolHeader};

    let name = args
        .get("agent_name")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("sub");
    let view = serde_json::from_str::<serde_json::Value>(output).ok();
    let seed = view
        .as_ref()
        .and_then(|view| view.get("seed"))
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    let summary = view
        .as_ref()
        .and_then(|view| view.get("content"))
        .and_then(|value| value.as_str())
        .map(str::to_string);
    let display = ToolDisplay::new(
        ToolHeader::Other {
            label: "subagent".to_string(),
        },
        ToolBody::Subagent {
            name: name.to_string(),
            seed: seed.to_string(),
        },
    );
    match summary.filter(|summary| !summary.trim().is_empty()) {
        Some(summary) => display.with_summary(summary),
        None => display,
    }
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
        qaqh_config::Config::load()
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
        return qaqh_workspace::json_err(
            "MISSING_TASK",
            "spawn_subagent: task_description is required",
            "Provide a task description.",
        );
    }
    let task_text = build_subagent_task(&task, &context);

    // 子代理继承主代理的工作区（BUG-2026-09-12-06）：必须读 TLS 优先的
    // current_workspace 而非进程全局 CURRENT_WORKSPACE——本 handler 运行在
    // 派生工具线程上，daemon 的进程全局恒空，旧读法使继承永远失效
    // （子代理 meta.cwd = None，相对路径全部锚到 daemon 进程 cwd）。为空/
    // `.` 时不传，宿主侧同样跳过继承。
    let parent_workspace = qaqh_workspace::current_workspace();
    let workspace = if parent_workspace.is_empty() || parent_workspace == "." {
        None
    } else {
        Some(parent_workspace)
    };
    let model = if model_override.is_empty() {
        None
    } else {
        Some(model_override.as_str())
    };
    let base_url = if base_url_override.is_empty() {
        None
    } else {
        Some(base_url_override.as_str())
    };
    let max_tokens_opt = if max_tokens == 0 || max_tokens == 4096 {
        None
    } else {
        Some(max_tokens)
    };

    // ── 1. 选择传输：宿主直连（进程内，无 HTTP/SSE 回环）优先；双子代理
    //        actor 未安装宿主时回退旧 daemon HTTP/SSE 路径。两种方式产出
    //        `(seed, Box<dyn SubagentTransport>)`，后续流程共用。──
    let (seed, transport): (String, Box<dyn SubagentTransport>) = if let Some(host) = host() {
        log::info!(
            "[SUBAGENT] '{name}' using in-process host direct transport (tools={})",
            tools.len()
        );
        let seed = match host.spawn_subagent(
            &tools,
            model,
            base_url,
            max_tokens_opt,
            workspace.as_deref(),
        ) {
            Ok(seed) if !seed.is_empty() => seed,
            Ok(_) => {
                return qaqh_workspace::json_err(
                    "SPAWN_ERROR",
                    "spawn_subagent: host returned empty seed",
                    "Check host/daemon logs.",
                );
            }
            Err(e) => {
                return qaqh_workspace::json_err(
                    "SPAWN_ERROR",
                    format!("spawn_subagent: host rejected spawn: {e}"),
                    "Check that the daemon can start subagent actors.",
                );
            }
        };
        let batch_rx = host.subscribe(&seed);
        (
            seed,
            Box::new(HostTransport { host, batch_rx }) as Box<dyn SubagentTransport>,
        )
    } else {
        // PR-4-2（Q4a）：legacy daemon HTTP/SSE 回连降级路径已删除——宿主未装配
        // （非 daemon 进程 / 未 install_host）即失败，不再回连。
        return qaqh_workspace::json_err(
            "HOST_UNAVAILABLE",
            "spawn_subagent: no in-process subagent host installed",
            "Subagent spawning requires the daemon host (install_host).",
        );
    };
    log::info!("[SUBAGENT] '{name}' worker seed={seed}");

    // ── 2. Send the task (attach/lease 语义封装在 transport 内；宿主直连
    //        进程内直接入 actor 命令队列，无 lease)。──
    let send = RingingCommand::Conversation(ConversationCommand::ConversationSendMessage {
        text: task_text,
        images: vec![],
        attachments: None,
        message_id: Some(format!("subagent-task:{seed}")),
        input_purpose: qaqh_domain::ConversationInputPurpose::TriggerTurn,
        as_system: false,
    });
    // 任务发送校验：Rejected/Err 意味着子 actor 未收到任务，立即失败，不要
    // 让 collect 空等 timeout。
    let send_accepted = match transport.send_command(&seed, send) {
        Ok(accepted) if accepted => true,
        Ok(_) => {
            transport.close();
            return qaqh_workspace::json_err(
                "SEND_REJECTED",
                "spawn_subagent: daemon rejected task send",
                "Check daemon/worker logs for lease or state conflicts.",
            );
        }
        Err(e) => {
            transport.close();
            return qaqh_workspace::json_err(
                "SEND_ERROR",
                format!("spawn_subagent: send task: {e}"),
                "Check daemon/worker logs.",
            );
        }
    };
    let _ = send_accepted;
    log::info!("[SUBAGENT] '{name}' task delivered to {seed} (accepted)");

    // ── 3. Register the process and collect the result in the background. ──
    //
    // 子代理与 exec 的 process 工具都运行在 daemon actor 进程内，共享同一个
    // ProcessRegistry。最终结果仍经 Ringing 注入主代理会话回传。
    let registry_ref = register_subagent_process(&format!("subagent:{name}"));
    let registry_id = registry_ref.id();
    // 主代理会话 seed：collect 完成后把最终作答注入回主会话（模型下一轮自然看到）。
    let parent_seed = qaqh_workspace::runtime::context()
        .map(|ctx| ctx.active_session.clone())
        .unwrap_or_default();
    let name_bg = name.clone();
    let seed_bg = seed.clone();
    std::thread::spawn(move || {
        collect_subagent_result(
            transport,
            &seed_bg,
            &name_bg,
            registry_ref,
            timeout_secs,
            &parent_seed,
        );
    });

    log::info!("[SUBAGENT] '{name}' spawned (seed={seed}, process={registry_id})");
    ToolResult::ok(qaqh_workspace::json_ok(serde_json::json!({
        "process_id": registry_id,
        "seed": seed,
        "name": name,
        "content": format!("Subagent '{name}' spawned (process {registry_id}); the final answer will be injected into the conversation as a [SUBAGENT] system message when it completes."),
    })))
}

/// Background collector: watches the sub-seed's event stream (process-local or
/// HTTP/SSE, depending on the transport) until a terminal event, a kill
/// request, or the timeout — mirroring the old stdout-frame collector, but over
/// the Ringing event plane.
fn collect_subagent_result(
    transport: Box<dyn SubagentTransport>,
    seed: &str,
    name: &str,
    registry_ref: RegistryRef,
    timeout_secs: u64,
    parent_seed: &str,
) {
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let mut final_answer = String::new();
    let mut exit_code: i32 = 0;
    let mut did_finish = false;
    let mut did_cancel = false;
    // 诊断：是否收到过子 seed 的任意事件（用于区分"子代理一开始就死了"
    // 与"中途卡死"——worker 侧 [SUBAGENT-WORKER] 日志 + 落盘开关配合）。
    let mut first_event_logged = false;

    while !did_finish && !did_cancel {
        // Kill requested (process kill {id}) → cancel the sub turn.
        if registry_ref.killed() {
            log::info!("[SUBAGENT] '{name}' kill requested via process registry — cancelling");
            let cancel = RingingCommand::Conversation(
                qaqh_domain::ConversationCommand::ConversationCancel { turn_id: None },
            );
            if let Err(e) = transport.send_command(seed, cancel) {
                log::warn!("[SUBAGENT] '{name}' cancel send failed: {e}");
            }
            final_answer = format!("[SUBAGENT '{name}' CANCELLED]");
            did_cancel = true;
            break;
        }
        match transport.events().recv_timeout(Duration::from_millis(300)) {
            Ok(batch) => {
                if batch.seed != seed {
                    continue;
                }
                if !first_event_logged && !batch.envelopes.is_empty() {
                    first_event_logged = true;
                    log::info!(
                        "[SUBAGENT] '{name}' first event received ({} envelopes, stream_seq {})",
                        batch.envelopes.len(),
                        batch.from_stream_seq
                    );
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
                            } else if let Some(reference) = output_ref
                                && let Ok(bytes) = transport.download_content(seed, &reference)
                            {
                                final_answer = String::from_utf8_lossy(&bytes).to_string();
                            }
                            if is_final && !final_answer.is_empty() {
                                did_finish = true;
                            }
                        }
                        RingingEvent::Conversation(ConversationEvent::TurnCompleted { .. }) => {
                            log::info!("[SUBAGENT] '{name}' turn completed");
                            did_finish = true;
                        }
                        RingingEvent::Conversation(ConversationEvent::TurnFailed {
                            error, ..
                        }) => {
                            log::warn!("[SUBAGENT] '{name}' turn failed: {error:?}");
                            final_answer = format!("[SUBAGENT '{name}' ERROR] {error:?}");
                            exit_code = 1;
                            did_finish = true;
                        }
                        RingingEvent::Conversation(ConversationEvent::ConversationCancelled {
                            ..
                        }) => {
                            log::info!("[SUBAGENT] '{name}' conversation cancelled");
                            final_answer = format!("[SUBAGENT '{name}' CANCELLED]");
                            did_cancel = true;
                        }
                        // 控制面失败（compact 拒绝注入、lease 拒绝等）：此前被
                        // 静默忽略导致"等到超时"。至少记入日志便于归因。
                        RingingEvent::Control(ControlEvent::OperationFailed { error, .. }) => {
                            log::warn!(
                                "[SUBAGENT] '{name}' operation failed: code={:?} message={:?}",
                                error.code,
                                error.message
                            );
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
                    log::warn!(
                        "[SUBAGENT] '{name}' timeout after {timeout_secs}s (first_event={first_event_logged}) — cancelling sub turn"
                    );
                    let cancel = RingingCommand::Conversation(
                        qaqh_domain::ConversationCommand::ConversationCancel { turn_id: None },
                    );
                    if let Err(e) = transport.send_command(seed, cancel) {
                        log::warn!("[SUBAGENT] '{name}' timeout cancel send failed: {e}");
                    }
                    final_answer = format!("[SUBAGENT '{name}' TIMEOUT after {timeout_secs}s]");
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

    // ── 结果回传：注入主代理会话。 ──
    // 主代理 idle 时这条消息触发新回合（模型自动看到子代理结果并继续）；
    // 主代理仍在运行中则进入回合 lap 边界的见缝插针通道。注入被 daemon 拒绝
    // （lease/compact 等）时重试一次并告警，避免静默丢失。
    let (state_tag, header) = if did_cancel {
        ("cancelled", format!("subagent '{name}' cancelled"))
    } else if exit_code != 0 {
        (
            "error",
            format!("subagent '{name}' failed (exit={exit_code})"),
        )
    } else {
        ("completed", format!("subagent '{name}' completed"))
    };
    // T-1-2：被取消的子代理不得把结果注入父会话。取消是终态——父会话可能
    // 正是被用户取消（或本 collector 的 kill/timeout 路径）而停下的，注入会
    // 触发一个 `TurnStart` 把已判定 cancel 的会话复活，并把一段早已作废的
    // `final_answer` 当作模型输入。仅留日志痕迹，不注入、不留存正文。
    if did_cancel {
        log::info!(
            "[SUBAGENT] '{name}' cancelled — result injection suppressed (answer_len={answer_len})"
        );
    }
    if !parent_seed.is_empty() && !did_cancel {
        // 注入到主代理会话。主代理 idle 时该消息触发新回合；运行中则进入
        // cmd_rx 排队 / lap 边界见缝插针通道。daemon 的 Accepted ACK 只代表
        // "已转发"，不代表 worker 落地；worker 侧的 compact 拒绝已改为延迟处理
        // （loop_core deferral），此处再对转发级瞬时失败（daemon 写 stdin 阻塞 /
        // lease 波动 / 网络抖动）做带退避重试，避免一次失败就静默丢弃子代理结果。
        //
        // 401 lease_required 根因：collect 的 client 只 attach 了子 seed，向主
        // seed 发命令需先建立 owns 关系（SessionResume）。每次重试前重新 attach
        // （SessionResume 幂等），覆盖 lease 过期后的恢复。
        let inject = RingingCommand::Conversation(
            qaqh_domain::ConversationCommand::ConversationSendMessage {
                text: format!(
                    "<qaqh_subagent_result name=\"{name}\" state=\"{state_tag}\" exit=\"{exit_code}\">\n{header}\n{final_answer}\n</qaqh_subagent_result>"
                ),
                images: vec![],
                attachments: None,
                message_id: Some(format!("subagent-result:{seed}")),
                input_purpose: qaqh_domain::ConversationInputPurpose::TriggerTurn,
                // 以 system 角色注入（而非 user）：模型可见但不等同于用户输入，
                // 保留 [SUBAGENT ...] 标签供模型区分注入数据与系统指令。
                as_system: true,
            },
        );
        let mut accepted = false;
        let mut last_rejected: Option<String> = None;
        const INJECT_ATTEMPTS: usize = 5;
        for attempt in 0..INJECT_ATTEMPTS {
            if attempt > 0 {
                // 线性退避：300ms → 600ms → 1200ms → 2400ms
                std::thread::sleep(std::time::Duration::from_millis(
                    300 * (1 << attempt.min(3)),
                ));
            }
            // attach（HTTP/lease 语义；宿主直连为 no-op，覆盖 lease 过期后的恢复）。
            if let Err(e) = transport.attach(parent_seed) {
                log::warn!(
                    "[SUBAGENT] '{name}' attach parent {parent_seed} for inject (attempt {}): {e}",
                    attempt + 1
                );
            }
            match transport.send_command(parent_seed, inject.clone()) {
                Ok(true) => {
                    accepted = true;
                    break;
                }
                other => {
                    last_rejected = Some(format!("{other:?}"));
                    log::warn!(
                        "[SUBAGENT] '{name}' inject attempt {} not accepted: {:?}",
                        attempt + 1,
                        last_rejected
                    );
                }
            }
        }
        if accepted {
            log::info!("[SUBAGENT] '{name}' inject accepted ({} bytes)", answer_len);
        } else {
            log::error!(
                "[SUBAGENT] '{name}' inject FAILED after {INJECT_ATTEMPTS} attempts: {:?}",
                last_rejected
            );
        }
    }

    registry_ref.finish(&final_answer, exit_code);

    // ── 自动卸载：终态后关闭子 agent（actor / worker 进程），释放后台资源。──
    // SessionClose 语义由宿主执行（进程内 registry.close；HTTP 路径由 daemon
    // 拦截：registry.close → SessionShutdown 帧 → worker 优雅退出）。
    // 失败仅告警：结果已注入主会话 + 终态已回写注册表，残留不丢数据。
    if let Err(e) = transport.send_command(
        seed,
        RingingCommand::Control(qaqh_domain::ControlCommand::SessionClose {
            seed: seed.to_string(),
        }),
    ) {
        log::warn!("[SUBAGENT] '{name}' close worker {seed} failed: {e}");
    } else {
        log::info!("[SUBAGENT] '{name}' sub agent {seed} closed (auto-unload)");
    }

    transport.close();
    log::info!(
        "[SUBAGENT] '{name}' collector complete (seed={seed}), answer_len={answer_len}, exit={exit_code}, cancelled={did_cancel}, first_event={first_event_logged}"
    );
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
        for legacy in [
            "system_prompt",
            "tools",
            "model",
            "base_url",
            "max_tokens",
            "name",
            "task",
        ] {
            assert!(
                !properties.contains_key(legacy),
                "{legacy} must not be exposed"
            );
        }
        assert_eq!(
            handler.input_schema["required"][0], "task_description",
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
        assert!(with_ctx.contains(
            "<main_subagent_message>\nrepo at F:\\proj\nbranch main\n</main_subagent_message>"
        ));
        assert!(with_ctx.contains("[TASK]\nreview the diff"));
        // 身份提示在前，任务在后。
        assert!(with_ctx.find("[SYSTEM]").unwrap() < with_ctx.find("[TASK]").unwrap());
    }

    // ── kill 路径回归（PR #57 reviewer 阻断 ③）────────────────────────
    //
    // 阻断背景：subagent 的登记路径只 `register` + `mark_exited`，**从不**
    // `attach_child` → 其进程条目没有 os_pid。`collect_subagent_result`
    // 每个轮询周期只看 `RegistryRef::killed()`（即 `status == "killed"`）。
    // 若墓碑 kill 之后状态仍停留在 `exited`，子代理永远收不到 kill 请求。

    /// 墓碑路径：条目已按终态时间驱逐 → `process kill` 命中墓碑 → 状态必须
    /// 收敛为 `killed`，`RegistryRef::killed()` 必须看到 true。
    #[test]
    fn registry_ref_killed_sees_tombstone_kill() {
        use qaqh_workspace::process_registry::{KillOutcome, ProcessRegistry};

        let id = ProcessRegistry::register("subagent-tombstone-kill");
        let registry_ref = RegistryRef::Local { id };

        // subagent 形态：登记后直接进终态，无 os_pid。
        ProcessRegistry::mark_exited(id, 0);
        assert!(!registry_ref.killed(), "终态为 exited 时不得视为被 kill");

        // 把条目熬成墓碑（终态 >600s 后下一次 register 触发惰性驱逐）。
        ProcessRegistry::age_registration_for_test(id, 3600);
        let _trigger = ProcessRegistry::register("subagent-tombstone-trigger");
        let info = ProcessRegistry::get_info(id).expect("条目必须已降级为墓碑");
        assert_eq!(info["evicted"], true, "前置条件：条目已驱逐: {info}");

        // 墓碑 kill：无 os_pid，故如实报 NoOsPid，但状态仍须收敛为 killed
        // （id 有效、终态确定，子代理据此停止轮询）。
        assert_eq!(
            ProcessRegistry::kill(id),
            KillOutcome::NoOsPid,
            "无 os_pid 的墓碑不得谎报清理成功"
        );
        let after = ProcessRegistry::get_info(id).expect("墓碑仍可查询");
        assert_eq!(
            after["status"], "killed",
            "墓碑 kill 后状态必须为 killed: {after}"
        );
        assert!(
            registry_ref.killed(),
            "RegistryRef::killed() 必须覆盖墓碑路径（否则子代理无法感知 kill）"
        );
    }

    /// 对照：在册条目的 kill 路径同样被 `RegistryRef::killed()` 看见。
    #[test]
    fn registry_ref_killed_sees_in_place_kill() {
        use qaqh_workspace::process_registry::{KillOutcome, ProcessRegistry};

        let id = ProcessRegistry::register("subagent-in-place-kill");
        let registry_ref = RegistryRef::Local { id };
        assert!(!registry_ref.killed(), "运行中不得视为被 kill");

        assert_eq!(ProcessRegistry::kill(id), KillOutcome::Killed);
        assert!(
            registry_ref.killed(),
            "在册条目 kill 后 RegistryRef::killed() 必须为 true"
        );
    }

    // ── T-1-2：取消后不得注入父会话 ──────────────────────────────────────

    /// 记录投递命令的 mock 传输：事件流由测试预置（这里只放一条
    /// `ConversationCancelled`，让 collector 立即进取消终态）。
    struct RecordingTransport {
        batch_rx: mpsc::Receiver<EventBatch>,
        sent: Arc<std::sync::Mutex<Vec<(String, RingingCommand)>>>,
    }

    impl SubagentTransport for RecordingTransport {
        fn send_command(&self, seed: &str, command: RingingCommand) -> Result<bool, String> {
            self.sent
                .lock()
                .expect("test mutex must not be poisoned")
                .push((seed.to_string(), command));
            Ok(true)
        }

        fn download_content(
            &self,
            _seed: &str,
            _reference: &ContentRef,
        ) -> Result<Vec<u8>, String> {
            Err("no externalized content in this test".into())
        }

        fn attach(&self, _seed: &str) -> Result<(), String> {
            Ok(())
        }

        fn close(&self) {}

        fn events(&self) -> &mpsc::Receiver<EventBatch> {
            &self.batch_rx
        }
    }

    fn cancelled_batch(seed: &str) -> EventBatch {
        let envelope = qaqh_ringing::RingingEventEnvelope::new(
            seed,
            1,
            1,
            1,
            "ev-cancel-1",
            RingingEvent::Conversation(ConversationEvent::ConversationCancelled { turn_id: None }),
        );
        EventBatch {
            schema: qaqh_ringing::protocol::RINGING_SCHEMA.to_string(),
            version: qaqh_ringing::protocol::RINGING_VERSION,
            channel: qaqh_domain::RingingChannel::Conversation,
            seed: seed.to_string(),
            server_epoch: "test-epoch".to_string(),
            from_stream_seq: 1,
            to_stream_seq: 1,
            envelopes: vec![envelope],
        }
    }

    /// T-1-2 回归：collector 收到 `ConversationCancelled` 后**不得**把
    /// `final_answer` 注入父会话。未修复时本测试红：仍向父 seed 发
    /// `ConversationSendMessage { as_system: true }`，父会话被重新开回合
    /// （TurnStart）——「已判定 cancel 的子代理复活」的第二段根因。
    #[test]
    fn cancelled_collector_does_not_inject() {
        use qaqh_workspace::process_registry::ProcessRegistry;

        let child = "sub-cancel-inject-child";
        let parent = "sub-cancel-inject-parent";
        let (tx, rx) = mpsc::channel::<EventBatch>();
        tx.send(cancelled_batch(child))
            .expect("test channel must not fail");

        let sent: Arc<std::sync::Mutex<Vec<(String, RingingCommand)>>> = Arc::default();
        let transport = Box::new(RecordingTransport {
            batch_rx: rx,
            sent: Arc::clone(&sent),
        });
        let registry_ref = RegistryRef::Local {
            id: ProcessRegistry::register("subagent-cancel-no-inject"),
        };

        collect_subagent_result(transport, child, "cancelled_task", registry_ref, 5, parent);

        let sent = sent.lock().expect("test mutex must not be poisoned");
        let injected: Vec<_> = sent.iter().filter(|(seed, _)| seed == parent).collect();
        assert!(
            injected.is_empty(),
            "取消后不得向父会话注入任何命令，实测: {injected:?}"
        );
        assert!(
            sent.iter().any(|(seed, command)| {
                seed == child
                    && matches!(
                        command,
                        RingingCommand::Control(qaqh_domain::ControlCommand::SessionClose { .. })
                    )
            }),
            "子 worker 的自动卸载（SessionClose）不受抑制影响，实测: {sent:?}"
        );
    }
}
