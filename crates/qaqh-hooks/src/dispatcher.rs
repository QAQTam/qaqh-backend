//! 五门 dispatcher:PreToolUse / PermissionRequest / UserPromptSubmit /
//! PostToolUse / Stop + 观察事件。
//!
//! 运行顺序 = 声明序(Config → Global → Project);第一个 deny/block 终止
//! 后续 handler(grok 同律)。fail-open 为缺省,`on_failure = block` 的
//! handler 失败按 block 处理(codex 口径)。异步 handler 发后不管,不参与
//! 门决策。`updatedInput` last-wins;改写是否合法由工具执行侧的参数
//! 归一化裁决,非法改写 = 执行失败(grok「改写不过 schema 即 deny」等效)。

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use tokio::sync::mpsc::UnboundedSender;

use crate::config::FailurePolicy;
use crate::discovery::{HookEntry, HookRegistry};
use crate::event::{HookEventEnvelope, HookEventName, HookPayload};
use crate::result::{
    HookRefusal, ObserveOutcome, PermissionOutcome, PostToolUseOutcome, PreToolUseOutcome,
    PromptOutcome, RunRecord, RunStatus, StopOutcome,
};
use crate::runner::{run_handler, DecisionJson, RunContext, RunnerOutcome};

/// 单条 hook 运行通知(事件面):Started 在执行前,Finished 带终态。
#[derive(Debug, Clone)]
pub enum HookRunNotice {
    Started {
        run_id: String,
        event: HookEventName,
        hook_name: String,
        turn_id: Option<String>,
        call_id: Option<String>,
    },
    Finished {
        run_id: String,
        event: HookEventName,
        hook_name: String,
        status: RunStatus,
        summary: Option<String>,
        duration_ms: u64,
    },
}

static RUN_SEQ: AtomicU64 = AtomicU64::new(1);

fn next_run_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("hook_{}_{:04}", nanos, RUN_SEQ.fetch_add(1, Ordering::Relaxed))
}

/// 门派发器。一次会话构造一个;`project_trusted` 可随信任批准动态更新。
pub struct Dispatch<'a> {
    pub registry: &'a HookRegistry,
    pub ctx: RunContext,
    /// 项目作用域 hooks 是否放行(discovery 产物 + trust store 判定)。
    pub project_trusted: bool,
    /// 运行通知 sink(事件面);None = 静默。
    pub notices: Option<UnboundedSender<HookRunNotice>>,
}

impl<'a> Dispatch<'a> {
    /// 事件是否有任何可执行 handler(含未信任的项目 hook:它们会以
    /// SkippedUntrusted 记录)。
    pub fn has_hooks(&self, event: HookEventName) -> bool {
        !self.registry.hooks_for(event).is_empty()
    }

    // ---- PreToolUse 门 ----

    pub async fn pre_tool_use(
        &self,
        tool_name: &str,
        tool_input: serde_json::Value,
    ) -> PreToolUseOutcome {
        let mut outcome = PreToolUseOutcome::default();
        let mut current_input = tool_input.clone();
        for entry in self.registry.hooks_for(HookEventName::PreToolUse) {
            if !self.match_entry(entry, Some(tool_name), &mut outcome.runs) {
                continue;
            }
            let payload = HookPayload::ToolUse {
                tool_name: tool_name.to_string(),
                tool_input: current_input.clone(),
                tool_output: None,
            };
            let envelope = self.envelope(HookEventName::PreToolUse, payload, None, None);
            match self.run_one(entry, &envelope).await {
                GateVerdict::Deny { reason } => {
                    outcome.denial = Some(HookRefusal {
                        source: format!("hook '{}'", entry.name),
                        reason,
                    });
                    return outcome;
                }
                GateVerdict::Ask(reason) => {
                    if outcome.ask_reason.is_none() {
                        outcome.ask_reason = Some(reason);
                    }
                }
                GateVerdict::Allow { decision } => {
                    let Some(decision) = decision else { continue };
                    if let Some(updated) = decision.updated_input {
                        // last-wins:后来的改写覆盖先前的。
                        if updated.is_object() {
                            current_input = updated;
                        } else {
                            // 改写必须为 JSON 对象;非法改写 = 直接 deny。
                            outcome.denial = Some(HookRefusal {
                                source: format!("hook '{}'", entry.name),
                                reason: "updatedInput must be a JSON object".to_string(),
                            });
                            return outcome;
                        }
                    }
                    if let Some(context) = decision.additional_context {
                        outcome.additional_context.push(context);
                    }
                }
                GateVerdict::Neutral => {}
            }
        }
        outcome.updated_input = Some(current_input);
        outcome
    }

    // ---- PermissionRequest 门 ----

    pub async fn permission_request(
        &self,
        tool_name: &str,
        tool_input: serde_json::Value,
    ) -> PermissionOutcome {
        let mut outcome = PermissionOutcome::default();
        for entry in self.registry.hooks_for(HookEventName::PermissionRequest) {
            if !self.match_entry(entry, Some(tool_name), &mut outcome.runs) {
                continue;
            }
            let payload = HookPayload::ToolUse {
                tool_name: tool_name.to_string(),
                tool_input: tool_input.clone(),
                tool_output: None,
            };
            let envelope = self.envelope(HookEventName::PermissionRequest, payload, None, None);
            match self.run_one(entry, &envelope).await {
                GateVerdict::Deny { reason } => {
                    outcome.decision = Some(false);
                    outcome.denial_reason = Some(reason);
                    return outcome;
                }
                GateVerdict::Allow { .. } => {
                    if outcome.decision.is_none() {
                        outcome.decision = Some(true);
                    }
                }
                GateVerdict::Ask(_) | GateVerdict::Neutral => {}
            }
        }
        outcome
    }

    // ---- UserPromptSubmit 门 ----

    pub async fn prompt(&self, prompt: &str) -> PromptOutcome {
        let mut outcome = PromptOutcome::default();
        for entry in self.registry.hooks_for(HookEventName::UserPromptSubmit) {
            if !self.match_entry(entry, None, &mut outcome.runs) {
                continue;
            }
            let payload = HookPayload::UserPrompt {
                prompt: prompt.to_string(),
            };
            let envelope = self.envelope(HookEventName::UserPromptSubmit, payload, None, None);
            match self.run_one(entry, &envelope).await {
                GateVerdict::Deny { reason } => {
                    outcome.block = Some(HookRefusal {
                        source: format!("hook '{}'", entry.name),
                        reason,
                    });
                    return outcome;
                }
                GateVerdict::Allow { decision } => {
                    if let Some(context) =
                        decision.and_then(|d| d.additional_context)
                    {
                        outcome.additional_context.push(context);
                    }
                }
                GateVerdict::Ask(_) | GateVerdict::Neutral => {}
            }
        }
        outcome
    }

    // ---- Stop 门 ----

    pub async fn stop(
        &self,
        last_assistant_message: Option<&str>,
        stop_hook_active: bool,
    ) -> StopOutcome {
        let mut outcome = StopOutcome::default();
        for entry in self.registry.hooks_for(HookEventName::Stop) {
            if !self.match_entry(entry, None, &mut outcome.runs) {
                continue;
            }
            let payload = HookPayload::Sparse {
                source: None,
                reason: None,
                trigger: None,
                agent_type: None,
            };
            let mut envelope = self.envelope(HookEventName::Stop, payload, None, None);
            envelope.stop_hook_active = Some(stop_hook_active);
            envelope.last_assistant_message = Some(
                last_assistant_message
                    .unwrap_or_default()
                    .chars()
                    .take(32_768)
                    .collect::<String>(),
            );
            match self.run_one(entry, &envelope).await {
                GateVerdict::Deny { reason } => {
                    outcome.block = Some(HookRefusal {
                        source: format!("hook '{}'", entry.name),
                        reason,
                    });
                    return outcome;
                }
                GateVerdict::Allow { decision } => {
                    if let Some(context) = decision.and_then(|d| d.additional_context) {
                        outcome.additional_context.push(context);
                    }
                }
                GateVerdict::Ask(_) | GateVerdict::Neutral => {}
            }
        }
        outcome
    }

    // ---- PostToolUse 门 ----

    pub async fn post_tool_use(
        &self,
        tool_name: &str,
        tool_input: serde_json::Value,
        tool_output: &str,
    ) -> PostToolUseOutcome {
        let mut outcome = PostToolUseOutcome::default();
        for entry in self.registry.hooks_for(HookEventName::PostToolUse) {
            if !self.match_entry(entry, Some(tool_name), &mut outcome.runs) {
                continue;
            }
            let payload = HookPayload::ToolUse {
                tool_name: tool_name.to_string(),
                tool_input: tool_input.clone(),
                tool_output: Some(crate::event::clip_payload_text(tool_output)),
            };
            let envelope = self.envelope(HookEventName::PostToolUse, payload, None, None);
            match self.run_one(entry, &envelope).await {
                GateVerdict::Deny { reason } => {
                    outcome.block_reasons.push(reason);
                }
                GateVerdict::Allow { decision } => {
                    let Some(decision) = decision else { continue };
                    if let Some(context) = decision.additional_context {
                        outcome.additional_context.push(context);
                    }
                    // 第一个非空替换生效(grok first-wins)。
                    if outcome.output_replacement.is_none()
                        && decision
                            .updated_tool_output
                            .as_deref()
                            .is_some_and(|text| !text.is_empty())
                    {
                        outcome.output_replacement = decision.updated_tool_output;
                    }
                }
                GateVerdict::Ask(_) | GateVerdict::Neutral => {}
            }
        }
        outcome
    }

    // ---- 观察事件 ----

    pub async fn observe(
        &self,
        event: HookEventName,
        payload: HookPayload,
        turn_id: Option<&str>,
        call_id: Option<&str>,
    ) -> ObserveOutcome {
        let mut outcome = ObserveOutcome::default();
        let match_value = event.match_value(&payload).map(str::to_string);
        for entry in self.registry.hooks_for(event) {
            if !self.match_entry(entry, match_value.as_deref(), &mut outcome.runs) {
                continue;
            }
            let envelope = self.envelope(
                event,
                payload.clone(),
                turn_id.map(str::to_string),
                call_id.map(str::to_string),
            );
            let _ = self.run_one(entry, &envelope).await;
        }
        outcome
    }

    // ---- 内部 ----

    fn match_entry(
        &self,
        entry: &HookEntry,
        match_value: Option<&str>,
        runs: &mut Vec<RunRecord>,
    ) -> bool {
        if !entry.source.trusted_by_default() && !self.project_trusted {
            runs.push(RunRecord {
                hook_name: entry.name.clone(),
                status: RunStatus::SkippedUntrusted,
                summary: Some("project hooks not trusted for this workspace".to_string()),
                duration_ms: 0,
            });
            return false;
        }
        crate::matcher::matches_any(&entry.matcher, match_value)
    }

    fn envelope(
        &self,
        event: HookEventName,
        payload: HookPayload,
        turn_id: Option<String>,
        call_id: Option<String>,
    ) -> HookEventEnvelope {
        HookEventEnvelope {
            hook_event_name: event,
            session_id: self.ctx.session_id.clone(),
            cwd: self.ctx.cwd.clone(),
            workspace_root: self.ctx.workspace_root.clone(),
            timestamp: crate::event::rfc3339_utc_now(),
            turn_id,
            call_id,
            permission_mode: self.ctx.permission_mode.clone(),
            stop_hook_active: None,
            last_assistant_message: None,
            payload,
        }
    }

    /// 执行一条 handler 并解读为门判定;Started/Finished 通知全程发出。
    async fn run_one(&self, entry: &HookEntry, envelope: &HookEventEnvelope) -> GateVerdict {
        let run_id = next_run_id();
        if let Some(notices) = &self.notices {
            let _ = notices.send(HookRunNotice::Started {
                run_id: run_id.clone(),
                event: envelope.hook_event_name,
                hook_name: entry.name.clone(),
                turn_id: envelope.turn_id.clone(),
                call_id: envelope.call_id.clone(),
            });
        }

        // 异步 handler:发后不管,不参与门决策。
        if entry.handler.async_run {
            let entry = entry.clone();
            let ctx = self.ctx.clone();
            let notices = self.notices.clone();
            let run_id = run_id.clone();
            let event = envelope.hook_event_name;
            let async_envelope = envelope.clone();
            tokio::spawn(async move {
                let started = Instant::now();
                let outcome = run_handler(
                    &entry,
                    &async_envelope,
                    &ctx,
                    entry.handler.effective_timeout(event),
                )
                .await;
                if let Some(notices) = notices {
                    let _ = notices.send(HookRunNotice::Finished {
                        run_id,
                        event,
                        hook_name: entry.name.clone(),
                        status: if outcome.succeeded() {
                            RunStatus::Completed
                        } else {
                            RunStatus::Failed
                        },
                        summary: outcome.summary(),
                        duration_ms: started.elapsed().as_millis() as u64,
                    });
                }
            });
            return GateVerdict::Neutral;
        }

        let started = Instant::now();
        let outcome = run_handler(
            entry,
            envelope,
            &self.ctx,
            entry.handler.effective_timeout(envelope.hook_event_name),
        )
        .await;
        let duration_ms = started.elapsed().as_millis() as u64;
        let verdict = self.interpret(entry, envelope.hook_event_name, &outcome);
        let status = match &verdict {
            GateVerdict::Deny { .. } | GateVerdict::Ask(_) | GateVerdict::Allow { .. } => {
                RunStatus::Completed
            }
            GateVerdict::Neutral => match &outcome {
                RunnerOutcome::Completed { exit_code: 0, .. } => RunStatus::Completed,
                RunnerOutcome::TimedOut => RunStatus::TimedOut,
                _ => RunStatus::Failed,
            },
        };
        if let Some(notices) = &self.notices {
            let _ = notices.send(HookRunNotice::Finished {
                run_id,
                event: envelope.hook_event_name,
                hook_name: entry.name.clone(),
                status,
                summary: outcome.summary(),
                duration_ms,
            });
        }
        verdict
    }

    /// exit code + stdout JSON → 门判定。`on_failure = block` 把失败升级。
    fn interpret(
        &self,
        entry: &HookEntry,
        event: HookEventName,
        outcome: &RunnerOutcome,
    ) -> GateVerdict {
        let blocking_gate = !matches!(event.traits().gate, crate::event::GateKind::Observe);
        match outcome {
            RunnerOutcome::Completed {
                exit_code, stdout, ..
            } if *exit_code == crate::event::GATE_EXIT_CODE => {
                // exit 2 无条件 block,stdout 忽略(JSON allow 也不放行)。
                if blocking_gate {
                    GateVerdict::Deny {
                        reason: format!(
                            "blocked by hook '{}' (exit code {})",
                            entry.name,
                            crate::event::GATE_EXIT_CODE
                        ),
                    }
                } else {
                    GateVerdict::Neutral
                }
            }
            RunnerOutcome::Completed {
                exit_code: 0,
                stdout,
                ..
            } => match DecisionJson::parse(stdout) {
                Some(decision) => match decision.decision.as_deref() {
                    Some("deny") | Some("block") if blocking_gate => GateVerdict::Deny {
                        reason: decision
                            .reason
                            .clone()
                            .unwrap_or_else(|| format!("denied by hook '{}'", entry.name)),
                    },
                    Some("ask")
                        if matches!(event.traits().gate, crate::event::GateKind::Tool) =>
                    {
                        GateVerdict::Ask(
                            decision.reason.clone().unwrap_or_else(|| {
                                format!("hook '{}' requests approval", entry.name)
                            }),
                        )
                    }
                    Some(other) => {
                        log::warn!(
                            "[hooks] '{}' returned unknown decision `{other}`; treating as allow",
                            entry.name
                        );
                        GateVerdict::Allow {
                            decision: Some(decision),
                        }
                    }
                    None => GateVerdict::Allow {
                        decision: Some(decision),
                    },
                },
                None => GateVerdict::Allow { decision: None },
            },
            RunnerOutcome::Completed { exit_code, .. } => {
                if entry.handler.on_failure == FailurePolicy::Block && blocking_gate {
                    GateVerdict::Deny {
                        reason: format!(
                            "hook '{}' failed with exit code {exit_code} and onFailure=block",
                            entry.name
                        ),
                    }
                } else {
                    GateVerdict::Neutral
                }
            }
            RunnerOutcome::TimedOut
            | RunnerOutcome::SpawnFailed(_)
            | RunnerOutcome::HttpRejected(..) => {
                if entry.handler.on_failure == FailurePolicy::Block && blocking_gate {
                    GateVerdict::Deny {
                        reason: format!(
                            "hook '{}' failed ({}) and onFailure=block",
                            entry.name,
                            outcome.summary().unwrap_or_else(|| "no detail".to_string())
                        ),
                    }
                } else {
                    GateVerdict::Neutral
                }
            }
        }
    }
}

/// 单条 handler 的门判定。
enum GateVerdict {
    /// 显式 deny/block。
    Deny { reason: String },
    /// PreToolUse ask:升级权限交互。
    Ask(String),
    /// 放行,可带 stdout JSON(改写/上下文)。
    Allow { decision: Option<DecisionJson> },
    /// 无决策(fail-open 的失败/无 JSON/观察事件)。
    Neutral,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{FailurePolicy, HookHandler};
    use crate::discovery::HookSource;
    use std::path::PathBuf;

    fn entry(command: &str, event: HookEventName, on_failure: FailurePolicy) -> HookEntry {
        let handler: HookHandler = serde_json::from_value(serde_json::json!({
            "type": "command",
            "command": command,
            "onFailure": if on_failure == FailurePolicy::Block { "block" } else { "continue" }
        }))
        .unwrap();
        HookEntry {
            name: format!("test#{command}"),
            event,
            matcher: None,
            handler,
            handler_kind: crate::config::HandlerKind::Command,
            source: HookSource::Config,
            source_dir: PathBuf::from("."),
        }
    }

    fn dispatch() -> Dispatch<'static> {
        // 测试只调 interpret;registry 空表即可。
        let registry = Box::leak(Box::new(HookRegistry::default()));
        Dispatch {
            registry,
            ctx: RunContext {
                session_id: "sess".to_string(),
                workspace_root: "D:/w".to_string(),
                cwd: "D:/w".to_string(),
                permission_mode: None,
                turn_id: None,
                call_id: None,
            },
            project_trusted: true,
            notices: None,
        }
    }

    #[test]
    fn interpret_exit2_always_denies_blocking_gates() {
        let test_entry = entry("x", HookEventName::PreToolUse, FailurePolicy::Continue);
        let outcome = RunnerOutcome::Completed {
            exit_code: 2,
            stdout: "{\"decision\":\"allow\"}".to_string(),
            stderr_first_line: None,
        };
        let dispatch = dispatch();
        let verdict = dispatch.interpret(&test_entry, HookEventName::PreToolUse, &outcome);
        assert!(matches!(verdict, GateVerdict::Deny { .. }));
        // 观察事件 exit 2 不 deny。
        let verdict = dispatch.interpret(&test_entry, HookEventName::PostToolUseFailure, &outcome);
        assert!(matches!(verdict, GateVerdict::Neutral));
    }

    #[test]
    fn interpret_fail_open_by_default_and_block_opt_in() {
        let dispatch = dispatch();
        let outcome = RunnerOutcome::TimedOut;
        let soft = entry("x", HookEventName::Stop, FailurePolicy::Continue);
        assert!(matches!(
            dispatch.interpret(&soft, HookEventName::Stop, &outcome),
            GateVerdict::Neutral
        ));
        let hard = entry("x", HookEventName::Stop, FailurePolicy::Block);
        assert!(matches!(
            dispatch.interpret(&hard, HookEventName::Stop, &outcome),
            GateVerdict::Deny { .. }
        ));
    }

    #[test]
    fn interpret_json_decisions() {
        let dispatch = dispatch();
        let soft = entry("x", HookEventName::PreToolUse, FailurePolicy::Continue);
        let deny = RunnerOutcome::Completed {
            exit_code: 0,
            stdout: "{\"decision\":\"deny\",\"reason\":\"no\"}".to_string(),
            stderr_first_line: None,
        };
        assert!(matches!(
            dispatch.interpret(&soft, HookEventName::PreToolUse, &deny),
            GateVerdict::Deny { .. }
        ));
        let ask = RunnerOutcome::Completed {
            exit_code: 0,
            stdout: "{\"decision\":\"ask\"}".to_string(),
            stderr_first_line: None,
        };
        assert!(matches!(
            dispatch.interpret(&soft, HookEventName::PreToolUse, &ask),
            GateVerdict::Ask(_)
        ));
        let empty = RunnerOutcome::Completed {
            exit_code: 0,
            stdout: String::new(),
            stderr_first_line: None,
        };
        assert!(matches!(
            dispatch.interpret(&soft, HookEventName::PreToolUse, &empty),
            GateVerdict::Allow { decision: None }
        ));
    }
}
