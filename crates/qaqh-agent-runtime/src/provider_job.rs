//! Provider requests, streaming progress and terminal classification.
//!
//! The blocking gate call
//! remains owned until it actually returns, including after cancellation.
//!
//! The runner never reports "cancelled" as a terminal business result. Anything
//! that happens after the gate call has begun is `StartedUnknown` until a
//! terminal response from the provider stream is observed and classified.

use qaqh_ringing_contract::fact::ProviderFinish;
use qaqh_types::{ContentBlock, Message, ToolDef, UsageInfo};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use crate::progress::{AssistantProgress, ProgressBlockKind, ProgressStream, ToolInputProgress};
use qaqh_workspace::arg_estimate::{ArgLineEstimator, RawPatchLineEstimator};

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
    /// 端点声明的上下文窗口（`Config::context_length`）——占用页脚与 handoff
    /// 阈值判断的分母。`0` = 未声明：不编百分比，也不发提醒。
    pub context_window: u32,
    /// 软阈值（`Config::auto_compact_threshold`，0 = 关闭提醒）。契约里已有的
    /// 开关，此前无任何消费者；现在它驱动 handoff 提醒。
    pub auto_compact_threshold: f64,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderFailureKind {
    Request,
    WorkerCrash,
}

/// 一次模型请求的结果:证据级别 + 中性消息 + 流式展示残段。
pub struct ProviderOutcome {
    /// How far the effect actually got.
    pub evidence: Evidence,
    pub message: Option<Message>,
    pub opaque_items: Vec<serde_json::Value>,
    pub usage: Option<UsageInfo>,
    /// 内核实测产出区间(毫秒):本次流式响应第一个产出碎片(reasoning/正文/
    /// 工具参数)到最后一个碎片的跨度。随 `TimelineEntry::AssistantBlock` 下发,
    /// 是前端吐字速率结算的权威分母;无产出为 `None`。
    pub generated_ms: Option<u64>,
    pub stop_reason: Option<String>,
    /// Terminal classification; only a complete finish may start tools or
    /// the next round.
    pub finish: Option<ProviderFinish>,
    pub incomplete_reason: Option<String>,
    pub partial_text: Option<String>,
    pub partial_reasoning: Option<String>,
    pub error: Option<String>,
    /// True only when run_round observed and acted on this turn's cancel signal.
    pub cancelled: bool,
    pub failure_kind: Option<ProviderFailureKind>,
}

impl ProviderRequestSpec {
    pub fn new(provider: qaqh_gate::ProviderConfig, max_tokens: u32) -> Self {
        Self {
            provider,
            credential: qaqh_types::CredentialSource::ApiKey,
            tools: None,
            max_tokens,
            effort: None,
            context_window: 0,
            auto_compact_threshold: 0.0,
        }
    }

    /// 上下文口径：分母 + 软阈值。
    pub fn with_context(mut self, context_window: u32, auto_compact_threshold: f64) -> Self {
        self.context_window = context_window;
        self.auto_compact_threshold = auto_compact_threshold;
        self
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

pub async fn run_round(
    spec: ProviderRequestSpec,
    messages: Vec<Message>,
    user_id: String,
    mut cancel: tokio::sync::watch::Receiver<bool>,
    progress: Option<ProgressStream>,
) -> ProviderOutcome {
    if *cancel.borrow() {
        let mut outcome = not_started("cancelled before provider request started");
        outcome.cancelled = true;
        return outcome;
    }
    let spec = match resolve_provider_credential(spec) {
        Ok(spec) => spec,
        Err(error) => {
            log::error!("[provider] credential resolution failed: {error}");
            return not_started(&error);
        }
    };
    let cancel_flag = Arc::new(AtomicBool::new(*cancel.borrow()));
    let flag = cancel_flag.clone();
    let display = progress.clone();
    let mut job =
        tokio::task::spawn_blocking(move || stream_once(spec, messages, user_id, &flag, display));
    let mut finished: Option<ProviderOutcome> = None;
    let mut cancelled_by_turn = false;
    loop {
        tokio::select! {
            biased;
            result = &mut job => {
                finished = Some(match result {
                    Ok(value) => value,
                    Err(error) => {
                        worker_crashed(&format!("provider worker join failed: {error}"))
                    }
                });
                break;
            }
            changed = cancel.changed() => {
                if changed.is_err() {
                    break;
                }
                if *cancel.borrow() {
                    cancel_flag.store(true, Ordering::Release);
                    cancelled_by_turn = true;
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
            Err(error) => worker_crashed(&format!("provider worker join failed: {error}")),
        },
    };
    let mut outcome = outcome;
    outcome.cancelled = cancelled_by_turn;
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
    let mut first_output_at: Option<Instant> = None;
    let mut last_output_at: Option<Instant> = None;
    let mut tool_args: HashMap<usize, ToolInputStream> = HashMap::new();
    let result = qaqh_gate::chat_stream(
        &spec.provider,
        messages,
        spec.tools,
        spec.max_tokens,
        spec.effort,
        Some(user_id),
        Some(flag),
        &mut |event| {
            // 产出锚点按**到达 gate** 的碎片计,不等节流发布:节流管展示频率,
            // 管不着模型实际产出到哪儿了。`return` 早退的碎片同样是产出证据。
            if matches!(
                event,
                qaqh_gate::StreamEvent::ContentDelta(_)
                    | qaqh_gate::StreamEvent::ReasoningDelta { .. }
                    | qaqh_gate::StreamEvent::ToolCallProgress { .. }
                    | qaqh_gate::StreamEvent::NativeToolProgress { .. }
            ) {
                let at = Instant::now();
                first_output_at.get_or_insert(at);
                last_output_at = Some(at);
            }
            match event {
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
                    .or_insert_with(|| ToolInputStream::new(id, false));
                if entry.name != name {
                    entry.rebind(name);
                }
                // 每一片都喂：节流只管发布频率，管不着计数。
                entry.push_fragment(&args_chunk);
                // 参数一吐完就抢发一帧（不等下一个窗）：这是"第一个工具到手即可
                // 解析"的那一帧，仍然只是展示旁路。
                if let Some(display) = &display
                    && (entry.mark_complete() || due(&mut last_emit))
                {
                    publish_tool_input(display, entry);
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
                // 原生面是**累计快照**语义（替换），不是增量：同一身份换正文时
                // 保留估算器状态，只做快照吸收。
                let entry = tool_args
                    .entry(index)
                    .or_insert_with(|| ToolInputStream::new(call_id.clone(), true));
                entry.call_id = call_id;
                if entry.name != name {
                    entry.rebind(name);
                }
                entry.absorb_snapshot(&input_so_far);
                if let Some(display) = &display
                    && (entry.mark_complete() || due(&mut last_emit))
                {
                    publish_tool_input(display, entry);
                }
            }
            qaqh_gate::StreamEvent::WebSearchStatus(_)
            | qaqh_gate::StreamEvent::Retrying { .. } => {}
            }
        },
    );
    // 无论这条出口走向哪里，先补欠的尾巴：失败/重复 Done 的回合同样有过在飞的
    // 参数，渲染层要的是「模型最后吐到哪」。
    flush_tool_input_tails(&display, &mut tool_args);
    if let Err(error) = result {
        let detail = stream_error.unwrap_or_else(|| error.to_string());
        let mut outcome = failed_after_start(&detail);
        outcome.partial_text = (!partial_text.is_empty()).then_some(partial_text);
        outcome.partial_reasoning = (!partial_reasoning.is_empty()).then_some(partial_reasoning);
        outcome.generated_ms = output_window_ms(first_output_at, last_output_at);
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
        generated_ms: output_window_ms(first_output_at, last_output_at),
        stop_reason,
        finish: Some(finish),
        incomplete_reason,
        partial_text: (!partial_text.is_empty()).then_some(partial_text),
        partial_reasoning: (!partial_reasoning.is_empty()).then_some(partial_reasoning),
        error: None,
        cancelled: false,
        failure_kind: None,
    }
}

/// Resolve the selected account credential immediately before dispatch.
fn resolve_provider_credential(
    mut spec: ProviderRequestSpec,
) -> Result<ProviderRequestSpec, String> {
    if spec.credential != qaqh_types::CredentialSource::DeepseekAccount {
        return Ok(spec);
    }
    if spec.provider.kind != qaqh_gate::ProviderKind::Responses {
        return Err("DeepSeek account credential requires the Responses wire".into());
    }
    if spec.provider.base_url.trim_end_matches('/') != qaqh_dsaccount::protocol::INFERENCE_ORIGIN {
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

fn publish_tool_input(display: &ProgressStream, entry: &mut ToolInputStream) {
    // 没建估算器 = 这个工具没有行数概念：出 null，让客户端画「正在生成参数」
    // 而不是替它编一个 +0 −0。
    let estimate = entry.estimate_lines();
    let args = entry.text.clone();
    let target_files = entry.target_files();
    display.sink.tool_input(ToolInputProgress {
        turn_id: &display.blocks.turn_id,
        call_id: &entry.call_id,
        tool_name: &entry.name,
        args: &args,
        lines_added: estimate.map(|estimate| estimate.lines_added),
        lines_removed: estimate.map(|estimate| estimate.lines_removed),
        target_files,
        args_complete: entry.completed,
    });
    entry.published_len = args.len();
}

/// 补齐被节流窗吞掉的参数尾巴。
///
/// [`PROGRESS_INTERVAL`] 的文档早就承诺「Final values are always published
/// regardless of this interval」，但工具参数这条通道此前只在片段到达时按窗发，
/// 模型吐完最后一个调用的收尾几十字符通常不足一个窗 —— 那一帧就永久丢了。
/// 渲染层现在直接从累计前缀解目标文件（前端
/// `tools/presentation.ts` 的 `scanArgsPrefix` / `scanPatchHeaders`），欠这一帧
/// 就等于「将改哪个文件」停在半截上，而 `tool_declared` 要等整条响应结算才发。
/// 按 index 升序补，回放顺序与声明顺序一致。
fn flush_tool_input_tails(
    display: &Option<ProgressStream>,
    tool_args: &mut HashMap<usize, ToolInputStream>,
) {
    let Some(display) = display else {
        return;
    };
    let mut indices = tool_args.keys().copied().collect::<Vec<_>>();
    indices.sort_unstable();
    for index in indices {
        let Some(entry) = tool_args.get_mut(&index) else {
            continue;
        };
        if entry.text.len() > entry.published_len {
            publish_tool_input(display, entry);
        }
    }
}

/// 一路在飞的工具参数流：累计原文 + 行差估算器。
///
/// 估算只服务展示（[`crate::progress`] 的旁路面）：永不参与执行、永不授权调用、
/// 也永不进模型上下文，所以任何一步读不出数都只是「这一帧没数」，不影响回合。
struct ToolInputStream {
    call_id: String,
    name: String,
    text: String,
    /// 参数来自哪条 provider 面：JSON 参数面，还是 codex 原生 freeform 文本面。
    native: bool,
    estimator: Option<Estimator>,
    /// 最后一次发布的累计长度。[`due()`] 的合并窗必然吞掉尾巴，终帧补齐就靠它
    /// 判断「还欠不欠一帧」——不欠就不多发一帧。
    published_len: usize,
    /// 参数是否已经吐完并已抢发过一次：只抢发一次，后续片段回到节流窗。
    completed: bool,
}

impl ToolInputStream {
    fn new(call_id: String, native: bool) -> Self {
        Self {
            call_id,
            name: String::new(),
            text: String::new(),
            native,
            estimator: None,
            published_len: 0,
            completed: false,
        }
    }

    /// 声明名换值（首帧起才有名字，或 provider 改写了名字）：估算键跟着换，并
    /// 立刻拿已累计文本重数一遍——读数不许因为改名倒退成 0。
    fn rebind(&mut self, name: String) {
        self.name = name;
        self.reseed();
    }

    fn reseed(&mut self) {
        self.estimator = Estimator::new(&self.name, self.native);
        // 正文换代（改名或 provider 重发不同快照）：长度与"已吐完"都要作废，
        // 否则新正文的补齐会被旧值判成「不欠帧」。
        self.published_len = 0;
        self.completed = false;
        if let Some(estimator) = &mut self.estimator {
            estimator.push_fragment(&self.text);
        }
    }

    /// 投喂**新到达的片段**：累计串与计数同步前进。
    fn push_fragment(&mut self, fragment: &str) {
        if fragment.is_empty() {
            return;
        }
        if let Some(estimator) = &mut self.estimator {
            estimator.push_fragment(fragment);
        }
        self.text.push_str(fragment);
    }

    /// 吸收一个累计快照：切出新到的尾巴照常喂片段；快照不再是已累计串的前缀
    /// 延伸（provider 换代重发）时整段重来，绝不把两种正文混数成一个读数。
    fn absorb_snapshot(&mut self, snapshot: &str) {
        match snapshot.strip_prefix(self.text.as_str()) {
            Some(suffix) => self.push_fragment(suffix),
            None => {
                self.text = snapshot.to_string();
                self.reseed();
            }
        }
    }

    /// 参数是否**刚刚**吐完。两条 provider 面的闭合条件本来就不是一回事，
    /// 不能都拿「末字符是 `}`」当准：
    /// - JSON 参数面：末字符是 `}` 且整段 parse 得动。先做 O(1) 末字符试探再
    ///   parse，绝不在每个片段上重 parse 一遍——一个 50KB 补丁能吐几百片。
    /// - codex 原生 freeform 面：正文不是 JSON，`*** End Patch` 行就是终态。
    ///
    /// 返回 `true` = 这一帧要抢在节流窗外立刻发（此后回到节流窗，见
    /// [`Self::completed`]）。**永不参与执行与授权**：执行仍等
    /// [`crate::session_loop`] 从终帧 `Done` 取完整参数，这里只提前渲染。
    fn mark_complete(&mut self) -> bool {
        if self.completed {
            return false;
        }
        let closed = if self.native {
            self.text.contains("*** End Patch")
        } else {
            self.text.ends_with('}')
                && serde_json::from_str::<serde_json::Value>(&self.text).is_ok()
        };
        self.completed = closed;
        closed
    }

    /// 当前累计估算。`None` = 该工具没有行数概念（或估算器还没建起来）。
    fn estimate_lines(&self) -> Option<qaqh_workspace::arg_estimate::ArgLineEstimate> {
        self.estimator
            .as_ref()
            .map(|estimator| estimator.estimate())
    }

    /// 内核认出的目标文件（见 [`qaqh_workspace::arg_estimate::ArgLineEstimator::target_files`]：
    /// 只有补丁类调用出得来）。没有估算器 = 空表，不是「不改文件」。
    /// 在发布时刻读一次 hunk 表，代价与节流窗同频，不随片段增长。
    fn target_files(&self) -> Vec<String> {
        self.estimator
            .as_ref()
            .map(|estimator| estimator.target_files())
            .unwrap_or_default()
    }
}

/// 参数文本有两条完全不同的到达形状，计数规则也完全不同，禁止混为一谈：
/// JSON 参数面按参数键数文本行，codex 原生 freeform 面直接数补丁的 `+`/`-` 行
/// （那段文本不是 JSON，当 JSON 扫必然数错）。
enum Estimator {
    Json(ArgLineEstimator),
    RawPatch(RawPatchLineEstimator),
}

impl Estimator {
    /// 值得估算才建估算器：JSON 面只认三个写工具，原生面上只有 `apply_patch`
    /// 的参数是补丁文本。其余工具（read/grep/exec/…）不建，因此不出行数字段。
    fn new(tool_name: &str, native: bool) -> Option<Self> {
        if native {
            return (tool_name == "apply_patch")
                .then(|| Self::RawPatch(RawPatchLineEstimator::default()));
        }
        ArgLineEstimator::for_tool(tool_name).map(Self::Json)
    }

    fn push_fragment(&mut self, fragment: &str) {
        match self {
            Self::Json(estimator) => estimator.push_fragment(fragment),
            Self::RawPatch(estimator) => estimator.push_fragment(fragment),
        }
    }

    fn estimate(&self) -> qaqh_workspace::arg_estimate::ArgLineEstimate {
        match self {
            Self::Json(estimator) => estimator.estimate(),
            Self::RawPatch(estimator) => estimator.estimate(),
        }
    }

    fn target_files(&self) -> Vec<String> {
        match self {
            Self::Json(estimator) => estimator.target_files(),
            Self::RawPatch(estimator) => estimator.target_files(),
        }
    }
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

/// 首末产出碎片的跨度(毫秒)。从未产出 → `None`;产出全在同一瞬间
/// (端点单 chunk 冲刷)→ `0`,消费方据此如实判定"窗口测不出",不得伪造分母。
fn output_window_ms(first: Option<Instant>, last: Option<Instant>) -> Option<u64> {
    match (first, last) {
        (Some(first), Some(last)) => {
            Some(last.saturating_duration_since(first).as_millis() as u64)
        }
        _ => None,
    }
}

/// Nothing was submitted to the provider under this identity.
fn not_started(message: &str) -> ProviderOutcome {
    ProviderOutcome {
        evidence: Evidence::NotStarted,
        message: None,
        opaque_items: Vec::new(),
        usage: None,
        generated_ms: None,
        stop_reason: None,
        finish: None,
        incomplete_reason: None,
        partial_text: None,
        partial_reasoning: None,
        error: Some(message.chars().take(512).collect()),
        cancelled: false,
        failure_kind: Some(ProviderFailureKind::Request),
    }
}

/// The request may have reached the upstream service; a result is missing.
fn failed_after_start(message: &str) -> ProviderOutcome {
    ProviderOutcome {
        evidence: Evidence::StartedUnknown,
        message: None,
        opaque_items: Vec::new(),
        usage: None,
        generated_ms: None,
        stop_reason: None,
        finish: None,
        incomplete_reason: None,
        partial_text: None,
        partial_reasoning: None,
        error: Some(message.chars().take(512).collect()),
        cancelled: false,
        failure_kind: Some(ProviderFailureKind::Request),
    }
}

fn worker_crashed(message: &str) -> ProviderOutcome {
    let mut outcome = failed_after_start(message);
    outcome.failure_kind = Some(ProviderFailureKind::WorkerCrash);
    outcome
}

#[cfg(test)]
mod tests {
    use super::ToolInputStream;

    /// 两条参数面的到达形状不同（JSON 面给增量、codex 原生面给累计快照），
    /// 但都必须收敛到同一个估算终值；快照不再是前缀延伸（provider 换体重发）
    /// 时整段重来，绝不把两种正文混数成一个读数。
    #[test]
    fn both_argument_shapes_settle_on_one_estimate() {
        let mut json = ToolInputStream::new("call_1".to_string(), false);
        json.rebind("edit".to_string());
        for fragment in [r#"{"old_string":"a\nb","new_string":"A"#, r#"\nB\nC"}"#] {
            json.push_fragment(fragment);
        }
        let edit = json.estimate_lines().expect("edit 值得估算");
        assert_eq!((edit.lines_added, edit.lines_removed), (3, 2));

        let mut native = ToolInputStream::new("call_2".to_string(), true);
        native.rebind("apply_patch".to_string());
        native.absorb_snapshot("*** Begin Patch\n*** Add File: a.txt\n+one");
        native.absorb_snapshot("*** Begin Patch\n*** Add File: a.txt\n+one\n+two\n");
        let added = native.estimate_lines().expect("apply_patch 值得估算");
        assert_eq!((added.lines_added, added.lines_removed), (2, 0));

        native.absorb_snapshot(
            "*** Begin Patch\n*** Update File: b.txt\n@@\n-x\n+Y\n*** End Patch\n",
        );
        let replaced = native.estimate_lines().expect("估算器随快照换代重建");
        assert_eq!(
            (replaced.lines_added, replaced.lines_removed),
            (1, 1),
            "换体重发后读数只属于新正文"
        );
    }

    /// 只读工具没有行数概念：不建估算器，于是事件里出行数字段缺席而不是 `+0 −0`。
    #[test]
    fn read_only_arguments_stay_unestimated() {
        let mut json = ToolInputStream::new("call_3".to_string(), false);
        json.rebind("read".to_string());
        json.push_fragment(r#"{"path":"src/a.rs"}"#);
        assert!(json.estimate_lines().is_none());
    }

    /// 「参数吐完」的两条判定各自成立，且同一份正文只抢发一次；provider 换体重发
    /// 后必须重新认定，否则新正文的补齐会被旧标记吞掉。
    #[test]
    fn completed_arguments_fire_once_per_body_generation() {
        let mut json = ToolInputStream::new("call_4".to_string(), false);
        json.rebind("edit".to_string());
        json.push_fragment(r#"{"file_path":"a.rs","old_string":"x""#);
        assert!(!json.mark_complete(), "半截 JSON 不算吐完");
        json.push_fragment(r#", "new_string":"y"}"#);
        assert!(json.mark_complete(), "闭合并 parse 得动的那一帧必须抢发");
        assert!(!json.mark_complete(), "同一正文只抢发一次，之后回到节流窗");

        let mut native = ToolInputStream::new("call_5".to_string(), true);
        native.rebind("apply_patch".to_string());
        native.absorb_snapshot("*** Begin Patch\n*** Add File: a.txt\n+one");
        assert!(!native.mark_complete(), "补丁没写到 End Patch 不算吐完");
        native.absorb_snapshot("*** Begin Patch\n*** Add File: a.txt\n+one\n*** End Patch\n");
        assert!(native.mark_complete(), "原生 freeform 面的终态是 End Patch 行");
        native.absorb_snapshot("*** Begin Patch\n*** Update File: b.txt\n@@\n-x\n+Y\n");
        assert!(
            !native.mark_complete(),
            "换体重发后旧标记作废，未闭合的正文不得抢发"
        );
    }
}
