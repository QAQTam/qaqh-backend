//! Supervised provider runner and response bundle builder for committed model intents.
//!
//! Not connected to the production session lifecycle. The blocking gate call
//! remains owned until it actually returns, including after cancellation.
//!
//! The runner never reports "cancelled" as a terminal business result. Anything
//! that happens after the gate call has begun is `StartedUnknown` until a
//! terminal response from the provider stream is observed and classified.

use qaqh_agentloop_core::mailbox::FrameSize;
use qaqh_session::{
    canonical::sha256_content_hash,
    session_commit::{CommittedExecution, FenceReason, SessionCommit, SessionCommitError},
    session_fact_v2::{
        ContentHash, EventId, FactPayload, FactSchema, ModelRoundStarted, ProviderFinish,
        ProviderResponseCommitted, RequestIdentity, SessionFact, ToolCallDeclared, ToolCallId,
        TurnId,
    },
};
use qaqh_types::{ContentBlock, Message, ToolDef, UsageInfo};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use crate::progress::{
    AssistantProgress, AttemptBlocks, ProgressBlockKind, ProgressStream, ToolInputProgress,
};

/// Upper bound on the streamed text retained for a failed attempt.
const MAX_PARTIAL_TEXT_BYTES: usize = 64 * 1024;

/// Minimum spacing between display publications.
///
/// Fragments arrive per token; republishing the cumulative snapshot on every
/// one of them would rebuild the session view thousands of times per answer.
/// Final values are always published regardless of this interval.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(40);

/// Complete parameters of one provider invocation.
#[derive(Clone, Debug)]
pub struct ProviderRequestSpec {
    pub provider: qaqh_gate::ProviderConfig,
    /// Credential source is part of request identity; secret values are resolved
    /// only immediately before dispatch and never enter the committed identity.
    pub credential: qaqh_types::CredentialSource,
    pub tools: Option<Vec<ToolDef>>,
    pub max_tokens: u32,
    pub effort: Option<String>,
}

/// 一次模型请求实际走到哪(证明链外的最小等价物)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Evidence {
    /// 未出发(取消/凭据失败):无任何可见输出。
    NotStarted,
    /// 已出发但无终态(网络断/取消中):部分文本仍在。
    StartedUnknown,
    /// 拿到完整终态响应。
    Terminal,
}

impl ProviderRequestSpec {
    pub fn new(provider: qaqh_gate::ProviderConfig, max_tokens: u32) -> Self {
        Self {
            provider,
            credential: qaqh_types::CredentialSource::ApiKey,
            tools: None,
            max_tokens,
            effort: None,
        }
    }

    pub fn with_credential(mut self, credential: qaqh_types::CredentialSource) -> Self {
        self.credential = credential;
        self
    }

    pub fn with_tools(mut self, tools: Vec<ToolDef>) -> Self {
        self.tools = Some(tools);
        self
    }

    pub fn with_effort(mut self, effort: impl Into<String>) -> Self {
        self.effort = Some(effort.into());
        self
    }
}

/// Compute the deterministic request identity for one model attempt.
///
/// Every component that affects the outbound wire request is bound into a
/// content hash so a change in provider surface, parameters, tool catalog or
/// frozen prefix produces a distinct `RequestIdentity`.
pub fn compute_request_identity(
    attempt: u32,
    messages: &[Message],
    spec: &ProviderRequestSpec,
) -> RequestIdentity {
    let provider_kind = match spec.provider.kind {
        qaqh_gate::ProviderKind::Chat => "chat",
        qaqh_gate::ProviderKind::Responses => "responses",
        qaqh_gate::ProviderKind::Messages => "messages",
        qaqh_gate::ProviderKind::Gemini => "gemini",
    };
    let provider_fingerprint = serde_json::json!({
        "kind": provider_kind,
        "credential_source": spec.credential,
        "model": &spec.provider.model,
        "base_url": &spec.provider.base_url,
        "chat_path": &spec.provider.chat_path,
        "responses_path": &spec.provider.responses_path,
        "anthropic_path": &spec.provider.anthropic_path,
        "gemini_path": &spec.provider.gemini_path,
        "thinking_mode": format!("{:?}", spec.provider.thinking_mode),
        "cache_field": format!("{:?}", spec.provider.cache_field),
        "include_stream_usage": spec.provider.include_stream_usage,
        "supports_thinking": spec.provider.supports_thinking,
        "thinking_budget_large": spec.provider.thinking_budget_large,
        "supports_reasoning_effort": spec.provider.supports_reasoning_effort,
        "effort_allowlist": &spec.provider.effort_allowlist,
        "tool_call_content_null": spec.provider.tool_call_content_null,
        "supports_reasoning_content": spec.provider.supports_reasoning_content,
        "require_provider_parameters": spec.provider.require_provider_parameters,
        "do_sample": spec.provider.do_sample,
        "supports_tail_system": spec.provider.supports_tail_system,
        "responses_compat": {
            "web_search": spec.provider.responses_compat.web_search,
            "echo_web_search_call": spec.provider.responses_compat.echo_web_search_call,
            "send_include": spec.provider.responses_compat.send_include,
            "effort_max": &spec.provider.responses_compat.effort_max,
            "supports_user": spec.provider.responses_compat.supports_user,
            "search_function_alias": &spec.provider.responses_compat.search_function_alias,
            "echo_reasoning_content": spec.provider.responses_compat.echo_reasoning_content,
            "siwc_plan": spec.provider.responses_compat.siwc_plan,
        }
    });
    let provider_digest = json_hash(&provider_fingerprint);
    let params_digest = json_hash(&serde_json::json!({
        "max_tokens": spec.max_tokens,
        "effort": &spec.effort,
    }));
    let tool_catalog_digest = json_hash(&spec.tools);
    let prefix: Vec<&Message> = messages
        .iter()
        .take_while(|msg| matches!(msg.role.as_str(), "system" | "developer"))
        .collect();
    let prefix_digest = json_hash(&prefix);
    let request_digest = json_hash(&serde_json::json!({
        "attempt": attempt,
        "provider_digest": provider_digest.as_str(),
        "params_digest": params_digest.as_str(),
        "tool_catalog_digest": tool_catalog_digest.as_str(),
        "prefix_digest": prefix_digest.as_str(),
        "messages": messages,
    }));
    RequestIdentity {
        attempt,
        model: spec.provider.model.clone(),
        provider_digest,
        params_digest,
        tool_catalog_digest,
        prefix_digest,
        request_digest,
    }
}

/// Build a `ModelRoundStarted` payload whose `request_hash` and `RequestIdentity`
/// are derived directly from the session's frozen context and `ProviderRequestSpec`.
pub fn build_model_round_started(
    commit: &SessionCommit,
    turn_id: TurnId,
    round: u32,
    attempt: u32,
    spec: &ProviderRequestSpec,
) -> Result<ModelRoundStarted, SessionCommitError> {
    let head = commit.head()?;
    let messages = commit.request_messages()?;
    let request_hash = json_hash(&messages);
    let identity = compute_request_identity(attempt, messages, spec);
    Ok(ModelRoundStarted {
        turn_id,
        round,
        request_hash,
        context_revision: head,
        identity: Some(identity),
    })
}

#[derive(Debug, Clone)]
pub struct ProviderOutcome {
    /// How far the effect actually got.
    pub evidence: Evidence,
    /// Neutral assistant message (`Reasoning`, `Text`, `ToolUse`, `WebSearchCall`).
    /// Provider-specific `ResponseOutputItem` entries are separated into
    /// [`Self::opaque_items`] so they can be persisted in `opaque_refs`.
    pub message: Option<Message>,
    /// Verbatim provider output items (`encrypted_content`, `phase`, native
    /// function call items) extracted from the stream.
    pub opaque_items: Vec<serde_json::Value>,
    pub usage: Option<UsageInfo>,
    pub stop_reason: Option<String>,
    /// Terminal classification the commit port records in
    /// `ProviderResponseCommitted`. Only a complete finish may start tools or
    /// the next round.
    pub finish: Option<ProviderFinish>,
    pub incomplete_reason: Option<String>,
    /// Text streamed before the terminal event. Retained so a failed or
    /// interrupted attempt still has a display source.
    pub partial_text: Option<String>,
    /// Reasoning streamed before the terminal event.
    pub partial_reasoning: Option<String>,
    pub error: Option<String>,
}

impl FrameSize for ProviderOutcome {
    fn retained_bytes(&self) -> usize {
        let opaque_bytes: usize = self
            .opaque_items
            .iter()
            .map(|item| item.to_string().len())
            .sum();
        std::mem::size_of::<Self>()
            .saturating_add(self.message.as_ref().map_or(0, |message| {
                message.memory_usage_estimate().heap_estimate_bytes as usize
            }))
            .saturating_add(opaque_bytes)
            .saturating_add(self.error.as_ref().map_or(0, String::capacity))
            .saturating_add(self.stop_reason.as_ref().map_or(0, String::capacity))
            .saturating_add(self.incomplete_reason.as_ref().map_or(0, String::capacity))
            .saturating_add(self.partial_text.as_ref().map_or(0, String::capacity))
            .saturating_add(self.partial_reasoning.as_ref().map_or(0, String::capacity))
    }
}

/// Materialize a `ProviderOutcome` into a `ProviderResponseCommitted` fact,
/// display `AssistantBlockSealed` facts, and matching `ToolCallDeclared` facts.
///
/// `ProviderResponseCommitted` is context only: the projection registry gives it
/// no public slot, so the transcript text/reasoning must also be sealed here.
#[allow(clippy::too_many_arguments)]
pub fn prepare_response_commit_facts(
    commit: &SessionCommit,
    turn_id: &TurnId,
    round: u32,
    attempt: u32,
    model: &str,
    // Identities allocated when generation started; the sealed blocks reuse
    // them so the streamed text and the durable block are one block.
    blocks: Option<&AttemptBlocks>,
    outcome: &ProviderOutcome,
    response_event_id: EventId,
    mut next_tool_event_id: impl FnMut(usize) -> EventId,
    mut next_block_event_id: impl FnMut(usize) -> EventId,
    ts_ms: i64,
) -> Result<Vec<SessionFact>, SessionCommitError> {
    let finish = outcome.finish.unwrap_or(ProviderFinish::Unknown);
    let neutral_message = outcome.message.clone().unwrap_or_else(|| Message {
        msg_id: None,
        role: "assistant".into(),
        name: None,
        content: Vec::new(),
    });
    let message_bytes =
        serde_json::to_vec(&neutral_message).map_err(|e| SessionCommitError::Storage(e.into()))?;
    let message_ref = commit.put_blob(&message_bytes)?;

    let mut opaque_refs = Vec::with_capacity(outcome.opaque_items.len());
    for item in &outcome.opaque_items {
        let bytes = serde_json::to_vec(item).map_err(|e| SessionCommitError::Storage(e.into()))?;
        opaque_refs.push(commit.put_blob(&bytes)?);
    }

    let mut facts = Vec::new();
    facts.push(SessionFact {
        schema: FactSchema::v2(),
        session_id: commit.session_id().clone(),
        log_id: commit.log_id().clone(),
        fact_seq: 0,
        event_id: response_event_id,
        ts_ms,
        causation_id: None,
        turn_id: Some(turn_id.clone()),
        call_id: None,
        interaction_id: None,
        payload: FactPayload::ProviderResponseCommitted(ProviderResponseCommitted {
            turn_id: turn_id.clone(),
            round,
            attempt,
            message_ref,
            opaque_refs,
            usage: outcome.usage.clone(),
            finish,
            incomplete_reason: outcome.incomplete_reason.clone(),
        }),
    });

    // Display facts. The conversation projection builds assistant text from
    // `AssistantBlockSealed` only, so a missing block here means a silent turn.
    let mut block_ordinal = 0usize;
    let mut used_reasoning_id = false;
    let mut used_answer_id = false;
    let mut push_block = |facts: &mut Vec<SessionFact>,
                          kind: qaqh_session::session_fact_v2::AssistantBlockKind,
                          body: &str,
                          usage: Option<UsageInfo>|
     -> Result<(), SessionCommitError> {
        let content_ref = commit.put_blob(body.as_bytes())?;
        // Reuse the identity the display channel already published under.
        let block_id = match (blocks, kind) {
            (
                Some(blocks),
                qaqh_session::session_fact_v2::AssistantBlockKind::Reasoning,
            ) if !used_reasoning_id => {
                used_reasoning_id = true;
                qaqh_session::session_fact_v2::BlockId::new(blocks.reasoning_block_id.clone())
            }
            (Some(blocks), qaqh_session::session_fact_v2::AssistantBlockKind::Answer)
                if !used_answer_id =>
            {
                used_answer_id = true;
                qaqh_session::session_fact_v2::BlockId::new(blocks.answer_block_id.clone())
            }
            _ => qaqh_session::session_fact_v2::BlockId::new(format!(
                "block_{}",
                qaqh_session::canonical::generate_ulid()
            )),
        };
        facts.push(SessionFact {
            schema: FactSchema::v2(),
            session_id: commit.session_id().clone(),
            log_id: commit.log_id().clone(),
            fact_seq: 0,
            event_id: next_block_event_id(block_ordinal),
            ts_ms: ts_ms + block_ordinal as i64 + 1,
            causation_id: None,
            turn_id: Some(turn_id.clone()),
            call_id: None,
            interaction_id: None,
            payload: FactPayload::AssistantBlockSealed(
                qaqh_session::session_fact_v2::AssistantBlockSealed {
                    turn_id: turn_id.clone(),
                    block_id,
                    kind,
                    content_ref,
                    model: model.to_string(),
                    usage,
                },
            ),
        });
        block_ordinal += 1;
        Ok(())
    };

    let mut sealed_any = false;
    for block in &neutral_message.content {
        match block {
            ContentBlock::Reasoning { reasoning, .. } if !reasoning.is_empty() => {
                push_block(
                    &mut facts,
                    qaqh_session::session_fact_v2::AssistantBlockKind::Reasoning,
                    reasoning,
                    None,
                )?;
                sealed_any = true;
            }
            ContentBlock::Text { text } if !text.is_empty() => {
                push_block(
                    &mut facts,
                    qaqh_session::session_fact_v2::AssistantBlockKind::Answer,
                    text,
                    outcome.usage.clone(),
                )?;
                sealed_any = true;
            }
            _ => {}
        }
    }
    // A stream that never produced a terminal message still has visible text:
    // keep it rather than reporting an empty turn.
    if !sealed_any {
        if let Some(reasoning) = outcome
            .partial_reasoning
            .as_deref()
            .filter(|t| !t.is_empty())
        {
            push_block(
                &mut facts,
                qaqh_session::session_fact_v2::AssistantBlockKind::Reasoning,
                reasoning,
                None,
            )?;
        }
        if let Some(text) = outcome.partial_text.as_deref().filter(|t| !t.is_empty()) {
            push_block(
                &mut facts,
                qaqh_session::session_fact_v2::AssistantBlockKind::Answer,
                text,
                outcome.usage.clone(),
            )?;
        }
    }

    if finish == ProviderFinish::ToolUse {
        let mut tool_ordinal = 0usize;
        for block in &neutral_message.content {
            if let ContentBlock::ToolUse { id, name, input } = block {
                let args_bytes =
                    serde_json::to_vec(input).map_err(|e| SessionCommitError::Storage(e.into()))?;
                let args_ref = commit.put_blob(&args_bytes)?;
                let args_hash = sha256_content_hash(&args_bytes);
                // The provider id is the protocol identity: it goes back to the
                // model in the tool result, so it must be stored verbatim.
                let call_id = ToolCallId::new(id.clone());
                facts.push(SessionFact {
                    schema: FactSchema::v2(),
                    session_id: commit.session_id().clone(),
                    log_id: commit.log_id().clone(),
                    fact_seq: 0,
                    event_id: next_tool_event_id(tool_ordinal),
                    ts_ms: ts_ms + tool_ordinal as i64 + 1,
                    causation_id: None,
                    turn_id: Some(turn_id.clone()),
                    call_id: Some(call_id.clone()),
                    interaction_id: None,
                    payload: FactPayload::ToolCallDeclared(ToolCallDeclared {
                        turn_id: turn_id.clone(),
                        call_id,
                        tool_name: name.clone(),
                        args_ref,
                        args_hash,
                    }),
                });
                tool_ordinal += 1;
            }
        }
    }

    Ok(facts)
}

/// Run a committed model intent with default options.
pub async fn run(
    provider: qaqh_gate::ProviderConfig,
    effect: CommittedExecution,
    max_tokens: u32,
    cancel: tokio::sync::watch::Receiver<bool>,
) -> ProviderOutcome {
    run_spec(
        ProviderRequestSpec::new(provider, max_tokens),
        false,
        effect,
        cancel,
        None,
    )
    .await
}

/// 一次模型请求(新回合循环直驱;无执行证明链)。
///
/// 取消语义与证明链时代一致:gate 调用开始后取消不算终态,返回
/// `StartedUnknown` 并保留已流出的部分文本。
pub async fn run_round(
    spec: ProviderRequestSpec,
    messages: Vec<Message>,
    user_id: String,
    mut cancel: tokio::sync::watch::Receiver<bool>,
    progress: Option<ProgressStream>,
) -> ProviderOutcome {
    if *cancel.borrow() {
        return not_started("cancelled before provider request started");
    }
    let spec = match resolve_provider_credential(spec) {
        Ok(spec) => spec,
        Err(error) => {
            log::error!("[provider] credential resolution failed: {error}");
            return not_started(&error);
        }
    };
    let cancelled = Arc::new(AtomicBool::new(*cancel.borrow()));
    let flag = cancelled.clone();
    let display = progress.clone();
    let mut job = tokio::task::spawn_blocking(move || {
        stream_once(spec, messages, user_id, &flag, display)
    });
    let mut finished: Option<ProviderOutcome> = None;
    loop {
        tokio::select! {
            result = &mut job => {
                finished = Some(match result {
                    Ok(value) => value,
                    Err(error) => {
                        failed_after_start(&format!("provider worker join failed: {error}"))
                    }
                });
                break;
            }
            changed = cancel.changed() => {
                if changed.is_err() || *cancel.borrow() {
                    cancelled.store(true, Ordering::Release);
                    break;
                }
            }
        }
    }
    let outcome = match finished {
        Some(value) => value,
        // Never drop a running blocking handle and claim the provider stopped.
        None => match job.await {
            Ok(value) => value,
            Err(error) => failed_after_start(&format!("provider worker join failed: {error}")),
        },
    };
    // Publish the final cumulative value regardless of the throttle window so a
    // cancelled or failed attempt still shows everything that was generated.
    if let Some(display) = &progress {
        if let Some(text) = outcome.partial_text.as_deref() {
            publish_block(display, ProgressBlockKind::Answer, text);
        }
        if let Some(text) = outcome.partial_reasoning.as_deref() {
            publish_block(display, ProgressBlockKind::Reasoning, text);
        }
    }
    outcome
}

/// 阻塞执行一次 gate 流式调用并分类结果(run_round 的同步核心)。
fn stream_once(
    spec: ProviderRequestSpec,
    messages: Vec<Message>,
    user_id: String,
    flag: &Arc<AtomicBool>,
    display: Option<ProgressStream>,
) -> ProviderOutcome {
    let mut final_event = None;
    let mut stream_usage = None;
    let mut stream_error = None;
    let mut duplicate_done = false;
    let mut partial_text = String::new();
    let mut partial_reasoning = String::new();
    let mut last_emit: Option<Instant> = None;
    let mut tool_args: HashMap<usize, (String, String, String)> = HashMap::new();
    let result = qaqh_gate::chat_stream(
        &spec.provider,
        messages,
        spec.tools,
        spec.max_tokens,
        spec.effort,
        Some(user_id),
        Some(flag),
        &mut |event| match event {
            qaqh_gate::StreamEvent::Done {
                raw_message,
                usage,
                stop_reason,
            } => {
                if final_event.is_some() {
                    duplicate_done = true;
                }
                final_event = Some((raw_message, usage, stop_reason));
            }
            qaqh_gate::StreamEvent::UsageUpdate(usage) => {
                stream_usage = Some(usage);
            }
            qaqh_gate::StreamEvent::Error { message, .. } => {
                stream_error = Some(message);
            }
            qaqh_gate::StreamEvent::ContentDelta(text) => {
                if partial_text.len() < MAX_PARTIAL_TEXT_BYTES {
                    partial_text.push_str(&text);
                }
                if let Some(display) = &display
                    && due(&mut last_emit)
                {
                    publish_block(display, ProgressBlockKind::Answer, &partial_text);
                }
            }
            qaqh_gate::StreamEvent::ReasoningDelta { kind, text } => {
                if !kind.is_displayable() {
                    return;
                }
                if partial_reasoning.len() < MAX_PARTIAL_TEXT_BYTES {
                    partial_reasoning.push_str(&text);
                }
                if let Some(display) = &display
                    && due(&mut last_emit)
                {
                    publish_block(display, ProgressBlockKind::Reasoning, &partial_reasoning);
                }
            }
            qaqh_gate::StreamEvent::ToolCallProgress {
                index,
                id,
                name,
                args_chunk,
            } => {
                if id.is_empty() {
                    return;
                }
                let entry = tool_args
                    .entry(index)
                    .or_insert_with(|| (id, name.clone(), String::new()));
                entry.1 = name;
                entry.2.push_str(&args_chunk);
                if let Some(display) = &display
                    && due(&mut last_emit)
                {
                    publish_tool_input(display, &entry.0, &entry.1, &entry.2);
                }
            }
            qaqh_gate::StreamEvent::NativeToolProgress {
                index,
                call_id,
                name,
                input_so_far,
                ..
            } => {
                let Some(call_id) = call_id.filter(|id| !id.is_empty()) else {
                    return;
                };
                let name = name.unwrap_or_default();
                tool_args.insert(index, (call_id.clone(), name.clone(), input_so_far.clone()));
                if let Some(display) = &display
                    && due(&mut last_emit)
                {
                    publish_tool_input(display, &call_id, &name, &input_so_far);
                }
            }
            qaqh_gate::StreamEvent::WebSearchStatus(_)
            | qaqh_gate::StreamEvent::Retrying { .. } => {}
        },
    );
    if let Err(error) = result {
        let detail = stream_error.unwrap_or_else(|| error.to_string());
        let mut outcome = failed_after_start(&detail);
        outcome.partial_text = (!partial_text.is_empty()).then_some(partial_text);
        outcome.partial_reasoning = (!partial_reasoning.is_empty()).then_some(partial_reasoning);
        return outcome;
    }
    if duplicate_done {
        return failed_after_start("provider emitted multiple terminal Done events");
    }
    let Some((raw_message, done_usage, stop_reason)) = final_event else {
        return failed_after_start("missing terminal provider response");
    };
    if raw_message.role != "assistant" {
        return failed_after_start("provider response role is not assistant");
    }

    let mut neutral_blocks = Vec::with_capacity(raw_message.content.len());
    let mut opaque_items = Vec::new();
    for block in raw_message.content {
        match block {
            ContentBlock::ResponseOutputItem { item } => opaque_items.push(item),
            other => neutral_blocks.push(other),
        }
    }
    let message = Message {
        msg_id: raw_message.msg_id,
        role: raw_message.role,
        name: raw_message.name,
        content: neutral_blocks,
    };
    let (finish, incomplete_reason) = classify(stop_reason.as_deref(), &message);
    ProviderOutcome {
        evidence: Evidence::Terminal,
        message: Some(message),
        opaque_items,
        usage: done_usage.or(stream_usage),
        stop_reason,
        finish: Some(finish),
        incomplete_reason,
        partial_text: (!partial_text.is_empty()).then_some(partial_text),
        partial_reasoning: (!partial_reasoning.is_empty()).then_some(partial_reasoning),
        error: None,
    }
}

/// Run a committed model intent and verify that its `RequestIdentity` matches
/// the exact `ProviderRequestSpec` and frozen context before touching the wire.
pub async fn run_verified(
    spec: ProviderRequestSpec,
    effect: CommittedExecution,
    cancel: tokio::sync::watch::Receiver<bool>,
) -> ProviderOutcome {
    run_spec(spec, true, effect, cancel, None).await
}

/// Same as run_verified, but publishing in-flight display updates.
pub async fn run_verified_with_progress(
    spec: ProviderRequestSpec,
    effect: CommittedExecution,
    cancel: tokio::sync::watch::Receiver<bool>,
    progress: ProgressStream,
) -> ProviderOutcome {
    run_spec(spec, true, effect, cancel, Some(progress)).await
}

pub async fn run_spec(
    spec: ProviderRequestSpec,
    verify_wire_identity: bool,
    effect: CommittedExecution,
    mut cancel: tokio::sync::watch::Receiver<bool>,
    progress: Option<ProgressStream>,
) -> ProviderOutcome {
    let parts = match effect.into_parts() {
        Ok(parts) => parts,
        Err(reason) => return fenced(reason),
    };
    let qaqh_session::session_commit::CommittedParts {
        identity,
        fact,
        request_messages,
        tool_args,
    } = parts;
    let FactPayload::ModelRoundStarted(round) = &fact.payload else {
        return fenced(FenceReason::UnsupportedIntent);
    };
    debug_assert!(tool_args.is_none(), "model rounds carry no tool arguments");
    let Some(messages) = request_messages else {
        // The commit port froze the input at mint time. Rebuilding it from a
        // later context would silently run a different request.
        return not_started("committed model round has no frozen context");
    };
    if let Some(committed_id) = &round.identity {
        if committed_id.model != spec.provider.model {
            return not_started("provider model does not match committed request identity");
        }
        if verify_wire_identity {
            let actual = compute_request_identity(committed_id.attempt, &messages, &spec);
            if &actual != committed_id {
                return not_started(
                    "provider request spec does not match committed request identity",
                );
            }
        }
    } else if verify_wire_identity {
        return not_started("committed model round is missing request identity");
    }
    if *cancel.borrow() {
        return not_started("cancelled before provider request started");
    }

    let spec = match resolve_provider_credential(spec) {
        Ok(spec) => spec,
        Err(error) => {
            log::error!("[provider] credential resolution failed: {error}");
            return not_started(&error);
        }
    };

    let user_id = identity.actor.session_id;
    let cancelled = Arc::new(AtomicBool::new(*cancel.borrow()));
    let flag = cancelled.clone();
    let display = progress.clone();
    let mut job = tokio::task::spawn_blocking(move || {
        stream_once(spec, messages, user_id, &flag, display)
    });
    let mut finished: Option<ProviderOutcome> = None;
    loop {
        tokio::select! {
            result = &mut job => {
                finished = Some(match result {
                    Ok(value) => value,
                    Err(error) => {
                        failed_after_start(&format!("provider worker join failed: {error}"))
                    }
                });
                break;
            }
            changed = cancel.changed() => {
                if changed.is_err() || *cancel.borrow() {
                    cancelled.store(true, Ordering::Release);
                    break;
                }
            }
        }
    }
    let outcome = match finished {
        Some(value) => value,
        // Never drop a running blocking handle and claim the provider stopped.
        None => match job.await {
            Ok(value) => value,
            Err(error) => failed_after_start(&format!("provider worker join failed: {error}")),
        },
    };
    // Publish the final cumulative value regardless of the throttle window so a
    // cancelled or failed attempt still shows everything that was generated.
    if let Some(display) = &progress {
        if let Some(text) = outcome.partial_text.as_deref() {
            publish_block(display, ProgressBlockKind::Answer, text);
        }
        if let Some(text) = outcome.partial_reasoning.as_deref() {
            publish_block(display, ProgressBlockKind::Reasoning, text);
        }
    }
    outcome
}

/// Resolve account credentials at dispatch time so each model round sees the
/// current account token. The token is carried only in the in-flight provider
/// config; request identity contains the credential source, never the secret.
fn resolve_provider_credential(
    mut spec: ProviderRequestSpec,
) -> Result<ProviderRequestSpec, String> {
    if spec.credential != qaqh_types::CredentialSource::DeepseekAccount {
        return Ok(spec);
    }
    if spec.provider.kind != qaqh_gate::ProviderKind::Responses {
        return Err("DeepSeek account credential requires the Responses wire".into());
    }
    if spec.provider.base_url.trim_end_matches('/')
        != qaqh_dsaccount::protocol::INFERENCE_ORIGIN
    {
        return Err("DeepSeek account credential requires the official inference endpoint".into());
    }

    if qaqh_dsaccount::manager().is_none() {
        qaqh_dsaccount::install_default();
    }
    let manager = qaqh_dsaccount::manager()
        .ok_or_else(|| "DeepSeek account manager is unavailable".to_string())?;
    let token = manager.token().map_err(|error| error.to_string())?;
    let identity_headers = manager
        .config()
        .identity
        .headers()
        .into_iter()
        .map(|(name, value)| (name.to_string(), value))
        .collect();

    spec.provider = spec
        .provider
        .with_auth_header(qaqh_dsaccount::protocol::AUTH_TOKEN_HEADER, None)?;
    spec.provider.api_key = token;
    spec.provider.deepseek_account_headers = identity_headers;
    Ok(spec)
}

/// Whether the coalescing window elapsed; the first call always fires.
fn due(last: &mut Option<Instant>) -> bool {
    match last {
        Some(at) if at.elapsed() < PROGRESS_INTERVAL => false,
        _ => {
            *last = Some(Instant::now());
            true
        }
    }
}

fn publish_block(display: &ProgressStream, kind: ProgressBlockKind, text: &str) {
    if text.is_empty() {
        return;
    }
    let block_id = match kind {
        ProgressBlockKind::Reasoning => &display.blocks.reasoning_block_id,
        ProgressBlockKind::Answer => &display.blocks.answer_block_id,
    };
    display.sink.assistant_block(AssistantProgress {
        turn_id: &display.blocks.turn_id,
        block_id,
        kind,
        model: &display.blocks.model,
        text,
    });
}

fn publish_tool_input(display: &ProgressStream, call_id: &str, tool_name: &str, args: &str) {
    display.sink.tool_input(ToolInputProgress {
        turn_id: &display.blocks.turn_id,
        call_id,
        tool_name,
        args,
    });
}

fn fenced(reason: FenceReason) -> ProviderOutcome {
    not_started(&format!("capability fenced before start: {reason}"))
}

/// Terminal classification for the committed response fact.
///
/// Any hanging tool declaration or null-input tool call is classified as
/// [`ProviderFinish::Incomplete`] so partial JSON arguments can never be
/// dispatched as tools.
fn classify(stop_reason: Option<&str>, message: &Message) -> (ProviderFinish, Option<String>) {
    let has_corrupt_tool = message.content.iter().any(|block| {
        block.is_hanging_tool_use()
            || matches!(block, ContentBlock::ToolUse { input, .. } if input.is_null())
    });
    if has_corrupt_tool {
        return (
            ProviderFinish::Incomplete,
            Some("truncated or invalid tool call arguments".into()),
        );
    }
    let declares_tools = message
        .content
        .iter()
        .any(|block| matches!(block, ContentBlock::ToolUse { .. }));
    match stop_reason {
        Some("tool_use" | "tool_calls" | "FUNCTION_CALL") if declares_tools => {
            (ProviderFinish::ToolUse, None)
        }
        Some("tool_use" | "tool_calls" | "FUNCTION_CALL") => (
            ProviderFinish::Incomplete,
            Some("provider reported tool stop without tool declarations".into()),
        ),
        Some("length" | "max_tokens" | "max_output_tokens" | "MAX_TOKENS") => (
            ProviderFinish::Length,
            Some("provider truncated response at token limit".into()),
        ),
        Some("incomplete" | "content_filter" | "SAFETY" | "RECITATION") => (
            ProviderFinish::Incomplete,
            Some(format!(
                "provider reported incomplete finish: {}",
                stop_reason.unwrap_or_default()
            )),
        ),
        Some("stop" | "end_turn" | "stop_sequence" | "completed" | "STOP") if !declares_tools => {
            (ProviderFinish::Stop, None)
        }
        Some("stop" | "end_turn" | "stop_sequence" | "completed" | "STOP") => {
            (ProviderFinish::ToolUse, None)
        }
        None => (
            ProviderFinish::Unknown,
            Some("provider stream ended without terminal stop reason".into()),
        ),
        Some(other) => (
            ProviderFinish::Unknown,
            Some(format!("unrecognized provider stop reason: {other}")),
        ),
    }
}

fn json_hash<T: serde::Serialize + ?Sized>(value: &T) -> ContentHash {
    let bytes = serde_json::to_vec(value).expect("serialize canonical digest input");
    sha256_content_hash(&bytes)
}

/// Nothing was submitted to the provider under this identity.
fn not_started(message: &str) -> ProviderOutcome {
    ProviderOutcome {
        evidence: Evidence::NotStarted,
        message: None,
        opaque_items: Vec::new(),
        usage: None,
        stop_reason: None,
        finish: None,
        incomplete_reason: None,
        partial_text: None,
        partial_reasoning: None,
        error: Some(message.chars().take(512).collect()),
    }
}

/// The request may have reached the upstream service; a result is missing.
fn failed_after_start(message: &str) -> ProviderOutcome {
    ProviderOutcome {
        evidence: Evidence::StartedUnknown,
        message: None,
        opaque_items: Vec::new(),
        usage: None,
        stop_reason: None,
        finish: None,
        incomplete_reason: None,
        partial_text: None,
        partial_reasoning: None,
        error: Some(message.chars().take(512).collect()),
    }
}
