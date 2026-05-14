# Team C: dsx-agent Orchestrator Design

## 1. Overview

The orchestrator is the central nervous system of dsx-agent. It owns the main agent loop: receiving user input, building context, sending API requests, processing streaming responses, dispatching tool calls, and managing turn-level safety gates.

**Architectural role** (from IPC Protocol, A02):

```
TUI --(stdin/stdout)--> Agent --(stdin/stdout)--> Tools
                            |
                      (TCP IPC)
                            |
                           HP
```

The orchestrator lives in the Agent process. It does NOT call the API directly in the new architecture — it delegates to HP via TCP IPC for API calls, and to Tools via stdin/stdout for tool execution.

---

## 2. Twelve Modules — Pub API

### 2.1 `mod.rs` — Module declarations

No change needed. Re-exports of public types if any.

### 2.2 `agent_loop.rs` — Core agent loop

**Current functions:**
- `handle_start_agent_loop(app: &mut App, stream_tx)`
  - Syncs messages, guards (streaming/confirm/exec-pending), pre-flight gate check, compact context, configure model/tools, spawn API request
- `handle_send_message(app, tx)` — Process user input, skill match, then start loop

**New design — pub API:**

```rust
// Called by TUI (stdin handler) when user submits input
pub fn handle_user_input(state: &mut AgentState, input: &str) -> Result<(), OrchestratorError>;

// Called internally to start/fork the agent loop
pub fn start_agent_loop(state: &mut AgentState) -> Result<(), OrchestratorError>;

// Called by HP response handler when streaming is done + tools processed
pub fn continue_agent_loop(state: &mut AgentState) -> Result<(), OrchestratorError>;
```

**Flow:**
1. `handle_user_input` — guard checks, skill matching, session init, balance refresh, call `start_agent_loop`
2. `start_agent_loop` — message sync, gate check (via HP IPC), context compact, send API request to HP via TCP
3. HP streams back → `stream_handler` processes → tool calls → `continue_agent_loop` → back to step 2

### 2.3 `stream_handler.rs` — Stream event dispatch

**Current functions:**
- `handle_stream_event(app, event, tx)` — match StreamEvent variants

**New design — pub API:**

```rust
// Called by the HP TCP IPC receiver thread for each event
pub fn handle_stream_event(
    state: &mut AgentState,
    event: StreamEvent,
) -> Result<StreamAction, OrchestratorError>;
```

**Returns `StreamAction` enum** (replaces implicit control flow via App state):

```rust
pub enum StreamAction {
    Continue,            // Nominal — keep streaming
    Finalize(Message, Option<UsageInfo>), // Done — proceed to response processing
    ShowToolProgress { name: String, args: String }, // TUI update
    UpdateContent(String),
    UpdateReasoning(String),
    ShowBalance(String),
    ShowError(String),
    ExecStart(String, u32),
    ExecProgress(String),
    ExecDone(String, String),
}
```

**Key separation:** stream_handler no longer mutates TUI state directly. It returns action enums that the caller (agent loop / IPC bridge) forwards to TUI.

### 2.4 `response_processor.rs` — Tool response finalization

**Current functions:**
- `finalize_stream_response(app, raw_message, usage, tx)` — process done event: push assistant, iterate tool calls

**New design — pub API:**

```rust
pub fn finalize_stream_response(
    state: &mut AgentState,
    raw_message: Message,
    usage: Option<UsageInfo>,
) -> Result<LoopDecision, OrchestratorError>;
```

**Returns `LoopDecision`** (replaces implicit control flow):

```rust
pub enum LoopDecision {
    ContinueAgentLoop,           // No tools or all auto-executed → loop
    ConfirmTools(Vec<PendingTool>), // Tools need user confirmation
    AwaitSudo(Vec<(ToolCall, String)>), // Sudo needed
    AwaitIntent(Vec<(ToolCall, SafetyLevel, String)>), // Intent picker
    ForceTextResponse(String),   // Safety gate triggered
    EndTurn,                     // Nothing more to do
}
```

**Extracted responsibilities:**
- Tool execution dispatch → sends commands to Tools subprocess via stdin/stdout
- Safety gating → checks with HP via TCP for circuit breaker status
- Auto-verify → via Tools subprocess
- Learning capture → writes to session/memory files
- Escalation detection → queries HP via TCP

### 2.5 `tool_dispatch.rs` — User-initiated tool actions

**Current functions:**
- `handle_confirm_tools(app, tx)` — execute confirmed tools
- `handle_reject_tools(app)` — rollback context
- `handle_cancel_stream(app)` — cancel streaming, preserve partial content
- `handle_cancel_exec(app)` — kill child processes

**New design — pub API:**

```rust
pub fn confirm_tools(state: &mut AgentState) -> Result<LoopDecision, OrchestratorError>;
pub fn reject_tools(state: &mut AgentState) -> Result<(), OrchestratorError>;
pub fn cancel_stream(state: &mut AgentState) -> Result<(), OrchestratorError>;
pub fn cancel_exec(state: &mut AgentState) -> Result<(), OrchestratorError>;
```

**Key changes:**
- `confirm_tools` sends tool execution commands to Tools subprocess (instead of calling `execute_tool` directly)
- `reject_tools` modifies AgentState context (still in-process since context is agent-owned)
- `cancel_stream` sends cancellation signal to HP via TCP (HP cancels the HTTP request)
- `cancel_exec` sends kill signal to Tools subprocess (instead of direct `kill` syscall)

### 2.6 `context_compactor.rs` — Context preparation

**Current functions:**
- `prepare_and_compact(app)` — build message list, compact, inject skills/pitfalls

**New design — pub API:**

```rust
pub fn prepare_and_compact(state: &AgentState) -> PreparedContext;

pub struct PreparedContext {
    pub messages: Vec<Message>,
    pub token_estimate: u32,
    pub token_breakdown: Option<ContextBreakdown>,
    pub system_prompt: String,
    pub health_note: Option<String>,
    pub pitfall_block: String,
    pub skill_bodies: Vec<(String, String)>,
}
```

**No architectural change needed** — context building is purely in-process (it's memory layout, not I/O). The output is sent to HP via TCP as part of the API request payload.

### 2.7 `gates.rs` — Execution gates

**Current functions:**
- `phase_check_tool(app, name, id)` — block destructive tools in readonly phases
- `pre_tool_health_check(app, name)` — circuit breaker (delegated to HP)
- `post_tool_health_record(app, name, success)` — record outcome (to HP)
- `explore_gate(app, name, id, args)` — explore-before-read, intent, stale-edit

**Two-tier gate model (v3.2.0 lesson applied selectively):**

Per USER ruling and v3.2.0 history, there are two fundamentally different gate types:

| Type | Domain | Mechanism | Examples |
|------|--------|-----------|---------|
| **Workflow Block** | Agent (in-process) | Orchestrator dispatch rejection | explore-before-edit, phase_check |
| **Safety Block** | HP (cross-process) | TCP barrier, unbypassable | circuit breaker, tool loop, alternation |

**Why workflow blocks are safe:** The orchestrator simply refuses to dispatch the tool. The model never gets to "decide" whether to obey — the tool result comes back as `[ERROR]` or `[CANCELLED]`. The model can attempt to re-request, but the orchestrator applies the same check each time. This is not a bypass-vulnerable prompt gate — it's a dispatch-level enforcement.

**Safety blocks (HP)** go further: because HP is a separate process with a TCP barrier, the agent cannot fabricate a "pass" response even if compromised.

**New design — pub API:**

```rust
// ── Workflow gates (Agent-side, dispatch-level blocking) ──

/// Phase gate: blocks destructive tools in Explore/Plan phase.
/// Block is at the dispatch level — the tool never executes.
pub fn phase_check_tool(state: &AgentState, tool_name: &str) -> GateAction;

/// Explore gate: enforce explore-before-read, explore-before-exec,
/// intent compliance, and stale-edit detection.
/// is_explore = (tool_name == "explore")  // top-level tool, not exec(subaction)
/// has_explored is set when explore() tool completes successfully.
pub fn explore_gate(state: &AgentState, tool_name: &str, args: &str) -> GateAction;

// ── Safety gates (HP-side, TCP barrier) ──

/// Circuit breaker check: queries HP via TCP.
/// HP owns per-tool failure counts and disabled-tools state.
pub fn hp_pre_tool_check(state: &AgentState, tool_name: &str) -> Result<(), GateBlocked>;

/// Post-tool outcome: reports to HP via TCP.
/// HP updates circuit breaker, failure counters, emotion inference.
pub fn hp_post_tool_record(state: &mut AgentState, tool_name: &str, success: bool);

/// Unified gate result — blocks can come from both tiers.
pub enum GateAction {
    Allow,
    /// Workflow block (Agent-side, dispatch-level).
    /// The orchestrator refuses to dispatch the tool.
    /// Sets gate_message for the model to see.
    Block { reason: String, hint: String },
    /// Advisory warnings/notes (injected into context, non-blocking).
    Warn { message: String },
    Note { message: String },
}

/// Safety block (HP-side, cross-process — always overrides Agent decision).
pub enum GateBlocked {
    CircuitBreaker { tool: String, reason: String },
    ToolLoop { reason: String },
    ForceTextResponse { reason: String },
}
```

**Key principles:**
- Workflow gates (`phase_check_tool`, `explore_gate`) dispatch-block at the Agent level. The tool never reaches Tools subprocess.
- Safety gates (`hp_pre_tool_check`) are queries to HP via TCP. HP returns `GateBlocked` or Ok.
- Safety blocks always override workflow decisions — if HP says block, it's blocked regardless of what Agent-side gates say.
- `explore_gate` matching: `tool_name == "explore"` (top-level tool, no longer `exec + action=="explore"`).

### 2.8 `learning.rs` — Self-learning

**Current functions:**
- `auto_extract_memory(app, final_msg)` — post-turn extraction
- `capture_learning_entry(app, final_msg, round, ...)` — write learning log

**New design — pub API:**

```rust
pub fn auto_extract_memory(
    state: &mut AgentState,
    final_msg: &Message,
) -> Result<(), OrchestratorError>;

pub fn capture_learning_entry(
    state: &AgentState,
    final_msg: &Message,
    round: usize,
    tools: &[String],
    files: &[String],
    file_ops: &[(String, String, String)],
    had_errors: bool,
    emotion: AgentEmotion,
    label: &str,
) -> Result<(), OrchestratorError>;
```

**No architectural change needed** — all file I/O (session, memory, pitfalls). The orchestrator calls these at the end of each turn.

### 2.9 `tracker.rs` — File tracking

**Current functions:**
- `track_file_written(app, args)` — track files written in this turn
- `track_tool_code(app, name, args, result)` — update code preview state
- `last_assistant_content(app)` — get last assistant message
- `read_active_plan(app)` — read plan from temp file

**New design — pub API:**

```rust
pub fn track_file_written(state: &mut AgentState, args: &str);
pub fn track_tool_code(state: &mut ToolCodeState, tool_name: &str, args: &str, result: &str);
pub fn last_assistant_content(state: &AgentState) -> String;
pub fn read_active_plan(state: &AgentState) -> String;
```

**No architectural change.** Internal helper module.

### 2.10 `turn_scorer.rs` — Turn scoring

**Current functions:**
- `score_current_turn(app)` — score for compaction priority
- `detect_tool_equivalent(cmd)` — detect `cat` → `read_file` equivalence
- `extract_why(text, path, tool)` — extract intent for learning log
- `classify_path(path)` → `PathTrust`

**New design — pub API:**

```rust
pub fn score_current_turn(state: &AgentState) -> f32;
pub fn detect_tool_equivalent(cmd: &str) -> Option<(String, String)>;
pub fn extract_why(text: &str, path: &str, tool: &str) -> String;
pub fn classify_path(path: &str) -> PathTrust;
```

**No architectural change.** Pure utility module.

### 2.11 `arg_parser.rs` — Argument parsing

**Current functions:**
- `tool_action(args)` → extract "action" field
- `parse_file_arg(args)` → extract "path" field
- `parse_cmd_arg(args)` → extract "command" field

**No change.** Pure utility module.

### 2.12 `session_persistence.rs` — Session save

**Current functions:**
- `repair_messages(app)` — re-import through ContextAssembler
- `maybe_save_session(app)` — save session to disk

**New design — pub API:**

```rust
pub fn repair_messages(state: &mut AgentState);
pub fn maybe_save_session(state: &mut AgentState);
```

**No architectural change.** Session persistence is file-I/O local to Agent.

---

## 3. Main Agent Loop Flow (Refactored)

```
                      ┌──────────────────────────────────────┐
                      │           Agent Loop                 │
                      │                                      │
UserInput ──► handle_user_input()                            │
                │                                            │
                ▼                                            │
         guard checks                                        │
                │                                            │
                ▼                                            │
         start_agent_loop()                                  │
                │                                            │
                ├──► context_compactor::prepare_and_compact() │
                │                                            │
                ├──► [TCP] HP: gate_check()                  │
                │    (delegated from agent_loop)              │
                │                                            │
                ├──► [TCP] HP: chat_stream(context)          │
                │                                            │
                ▼                                            │
         stream_handler::handle_stream_event() ◄── HP streams│
                │                                            │
                ▼                                            │
         StreamAction::Finalize ──► response_processor::     │
                                    finalize_stream_response()│
                │                                            │
                ▼                                            │
         LoopDecision::                                        │
           ├── ContinueAgentLoop ─────────────────────────────┘
           ├── ConfirmTools ──► [TUI] show confirm prompt
           │                       │
           │                       ▼ (user confirms)
           │                 tool_dispatch::confirm_tools()
           │                       │
           │                       ├──► [TCP] HP: pre_tool_gate()
           │                       ├──► [stdin/stdout] Tools: execute()
           │                       ├──► [TCP] HP: post_tool_record()
           │                       └──► continue_agent_loop()
           │
           ├── AwaitSudo ──► [TUI] sudo prompt → exec
           ├── ForceTextResponse ──► annotate context → continue_agent_loop()
           └── EndTurn ──► learning::auto_extract_memory()
                           session_persistence::maybe_save_session()
                           wait for next user input
```

---

## 4. IPC Integration Points

### 4.1 Agent ↔ HP (TCP)

| Direction | Messages | Triggered by |
|-----------|----------|-------------|
| Agent → HP | `ChatStreamRequest { context, model, tools, config }` | `start_agent_loop()` |
| HP → Agent | `StreamEvent` (ContentDelta, ReasoningDelta, ToolCallProgress, Done, Error) | HP's SSE parser |
| Agent → HP | `GateCheckRequest { context }` | `start_agent_loop()` pre-flight |
| HP → Agent | `GateCheckResponse { Pass / Block { reason } }` | HP's `check_gate()` |
| Agent → HP | `PreToolGate { tool, args }` | Tool execution route |
| HP → Agent | `PreToolGateResponse { Pass / Block / Warn }` | HP's `pre_tool_gate()` |
| Agent → HP | `PostToolRecord { tool, success }` | After tool execution |
| HP → Agent | (ack) | — |
| Agent → HP | `CancelStream` | `cancel_stream()` |

### 4.2 Agent ↔ Tools (stdin/stdout)

| Direction | Messages | Triggered by |
|-----------|----------|-------------|
| Agent → Tools | `Execute { tool_name, args, tool_call_id }` | `confirm_tools()`, `finalize_stream_response()` |
| Tools → Agent | `ToolResult { tool_call_id, output }` | Sync execution |
| Tools → Agent | `ToolStarted { tool_call_id, pid }` | Long-running exec |
| Tools → Agent | `ToolProgress { tool_call_id, line }` | PTY stdout |
| Agent → Tools | `CancelExec { pid }` | `cancel_exec()` |

### 4.3 Agent ↔ TUI (stdin/stdout, connection-based)

| Direction | Messages | Triggered by |
|-----------|----------|-------------|
| TUI → Agent | `UserInput { text }` | User submits input |
| TUI → Agent | `ConfirmTools` | User confirms tools |
| TUI → Agent | `RejectTools` | User rejects tools |
| TUI → Agent | `CancelStream` | User presses Esc |
| Agent → TUI | `StreamAction` (see 2.3) | Stream processing |
| Agent → TUI | `ModeChange { mode }` | Control flow change |
| Agent → TUI | `StatusUpdate { msg }` | Status bar |
| Agent → TUI | `ToolConfirmRequest { tools }` | Await user decision |
| Agent → TUI | `SudoPromptRequest { count }` | Sudo needed |
| Agent → TUI | `BalanceUpdate { balance }` | Balance check result |

---

## 5. Gates Overlap Analysis (gates.rs vs dsx-hp/gate.rs)

### Current Split

| Concern | Owner | Location |
|---------|-------|----------|
| Pre-API structural validation | HP | `dsx-hp/gate.rs::check_gate()` |
| Alternation validation | HP | `dsx-hp/gate.rs::validate_anthropic_messages()` |
| Orphan tool_use detection | HP (also flagged by agent) | Both `dsx-hp/gate.rs` and `agent_loop.rs` |
| Phase-based tool blocking | Agent | `orchestrator/gates.rs::phase_check_tool()` |
| Explore-before-read enforcement | Agent | `orchestrator/gates.rs::explore_gate()` |
| Intent compliance tracking | Agent | `orchestrator/gates.rs::explore_gate()` |
| Circuit breaker check | HP | `dsx-hp/monitor.rs::pre_tool_gate()` (also in `hp/circuit_breaker.rs`) |
| Stale-edit detection | Agent | `orchestrator/gates.rs::explore_gate()` |
| Exec-after-write blocking | Agent | `orchestrator/gates.rs::explore_gate()` |
| Tool loop detection | HP + Agent | `hp/circuit_breaker.rs` and `orchestrator/response_processor.rs` (both check 100-call, 3-failure) |

### Overlap That Should Be Eliminated

**1. Tool loop limits (100 calls / 3 failures / 30 tool-only turns)**
- Currently checked in BOTH `hp/circuit_breaker.rs` AND `orchestrator/response_processor.rs`
- **Decision:** HP is the authority. Agent should NOT duplicate these checks. Agent queries HP: `should_escalate()` via TCP before deciding to force text response.

**2. Orphan tool_use detection**
- Currently the flag is set in agent-stream_handler and checked in HP gate
- This is acceptable as a two-phase check (flagged downstream, validated upstream)
- No change needed.

**3. Circuit breaker per-tool disabling**
- Currently `gates.rs::pre_tool_health_check()` calls `app.health.should_block()`
- **Decision:** HP should own circuit breaker state. Agent queries HP via TCP before execution.

### Clear Division (After Refactoring)

```
HP owns:                              Agent owns:
────────────────────────────────────  ──────────────────────────────────
- Pre-API structural validation       - Phase-based tool blocking
- Message alternation validation      - Explore-before-read enforcement
- Circuit breaker (per-tool)          - Intent compliance tracking
- Tool loop detection                 - Stale-edit detection
- Failure threshold monitoring        - Exec-after-write blocking
- Emotion inference                   - Context compaction
- Health assessment                   - Turn scoring
- API error sentinel                  - Learning/memory capture
- Balance tracking                    - Session persistence
                                      - Skill injection
                                      - Pitfall guide injection
```

The rule: **HP gates the API connection and tool execution safety; agent gates the *policy* around tool usage.** Policy gates (phase, explore, intent) require deep knowledge of AgentState that HP doesn't have. Safety gates (circuit breaker, loop detection) belong to HP because they're cross-session and independent of any particular agent's state.

---

## 6. Key Design Decisions

### 6.1 AgentState replaces `&mut App`
The orchestrator must not depend on the TUI's `App` struct. Define a focused `AgentState` that holds only what the orchestrator needs:
- `ContextAssembler` (message context)
- `HealthClient` (HP TCP connection handle)
- `ToolClient` (Tools subprocess stdin/stdout handle)
- `SessionMetadata` (seed, model, config)
- `AgentPhase`, `TaskPhase`
- `HealthState` (local cache of HP responses)
- Turn counters, tool call tracking
- Memory/session file handles

### 6.2 StreamAction + LoopDecision replace implicit `&mut App` control flow
Instead of the orchestrator directly setting `app.mode = Mode::Streaming` etc., it returns typed enums that the caller (the IPC bridge or TUI) acts on.

### 6.3 All HP communication is async TCP
The orchestrator sends/receives via the `HealthClient` handle. This is a `tokio::spawn`-friendly async TCP connection. All HP calls are non-blocking from the orchestrator's perspective.

### 6.4 All tool execution is via Tools subprocess
`execute_tool` is no longer a direct function call. The orchestrator sends a JSON command on stdin to the Tools subprocess and reads the result from stdout.

---

## 7. Error Handling

```rust
#[derive(Debug)]
pub enum OrchestratorError {
    HpConnectionError(String),
    ToolExecutionError(String),
    ContextError(String),
    SessionError(String),
    GateBlocked { reason: String, repairable: bool },
    Internal(String),
}
```

Errors are propagated up to the IPC bridge, which translates them to `StreamAction::ShowError` for the TUI.

---

## 8. Implementation Order (Recommended)

1. **AgentState** — Define the struct, extract from App what the orchestrator needs
2. **ToolClient** — stdin/stdout subprocess manager for Tools
3. **HealthClient** — TCP connection manager for HP
4. **context_compactor** — Minimal changes, mostly extraction from App
5. **stream_handler** — Convert to StreamAction return type
6. **gates** — Split: policy gates stay, safety gates delegate to HP
7. **agent_loop** — Wire up the new flow with HP + Tools IPC
8. **response_processor** — Convert to LoopDecision + delegate tool execution
9. **tool_dispatch** — Delegate to Tools subprocess
10. **learning + tracker + turn_scorer + arg_parser + session_persistence** — Mostly unchanged
