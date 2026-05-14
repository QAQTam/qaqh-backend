# Design: D03 — Task, Plan, Memory, Ask, Status Tools

**Owner:** Team D member D03
**Date:** 2026-05-14 (revised)
**Status:** Draft for review

---

## 1. Scope

| File | Tool(s) | Role |
|------|---------|------|
| `task.rs` | `task_create`, `task_update`, `task_list` | Task CRUD + status workflow |
| `plan.rs` | `plan_create`, `plan_update`, `plan_read`, `plan_list` | Plan CRUD + status workflow |
| `memory.rs` | `mem_save`, `mem_read`, `mem_forget`, `recall`, `pitfall_save`, `pitfall_guide` | Memory read/write/search + cross-session learning |
| `ask.rs` | `ask_user` | User clarification (stub — intercepted by TUI) |
| `status.rs` | `status` | Working mode declaration |

All are dispatched via the `agent` tool entry in `agent.rs:exec_agent()`. The AI calls `agent` with an `action` parameter, which dispatches to the appropriate sub-module's exec function.

---

## 2. Architecture: IPC via D01 ToolHandler

### 2.1 Revised Architecture

The DSX system runs as **two processes** connected by IPC (Unix domain socket):

```
┌─────────────────────────────┐     ┌────────────────────────────────┐
│       Tools Process          │     │       Agent Process             │
│                              │     │                                │
│  ┌───────────────────────┐   │     │  ┌──────────────────────────┐  │
│  │ D01 ToolHandler        │   │     │  │ D01 IPC Server (receive) │  │
│  │ ── reg. ToolHandler    │───╪─────╪──│     dispatch to:         │  │
│  │    for each tool       │   │     │  │  └──────────────────────┘  │
│  │ ── serialize args      │   │     │  │        │                  │
│  │ ── send via UnixSocket │   │     │  │        ▼                  │
│  │ ── receive result      │   │     │  │  ┌──────────────────┐     │
│  └───────────────────────┘   │     │  │  │ session::         │     │
│                              │     │  │  │ memory_io.rs      │     │
│  ┌───────────────────────┐   │     │  │  │ plan_io.rs        │     │
│  │ D03 Tool workers:      │   │     │  │  └──────────────────┘     │
│  │ task.rs / plan.rs /    │   │     │  │        │                  │
│  │ memory.rs / status.rs  │   │     │  │        ▼                  │
│  │ ── define ToolDef      │   │     │  │  ┌──────────────────┐     │
│  │ ── exec() sends IPC    │   │     │  │  │ filesystem       │     │
│  └───────────────────────┘   │     │  │  └──────────────────┘     │
│                              │     │                                │
│  ┌───────────────────────┐   │     │  ┌──────────────────────────┐  │
│  │ D03 ask.rs:            │   │     │  │ TUI (intercepts ask)     │  │
│  │ stub only, TUI        │───╪─────╪──│ renders modal, returns   │  │
│  │ handles it             │   │     │  │ response as tool_result  │  │
│  └───────────────────────┘   │     │  └──────────────────────────┘  │
└─────────────────────────────┘     └────────────────────────────────┘
```

**Key principle:** The Tools process NEVER performs file I/O. It sends IPC requests to the Agent process, which holds the sandbox and filesystem access. This ensures:
- All file operations go through a single sandbox boundary
- No file descriptor leaking between processes
- Consistent policy enforcement (read-only phases block destructive writes)
- Crash isolation (a tool panic cannot corrupt session files)

### 2.2 IPC Protocol (Unified via D01)

D01 defines the ToolHandler trait and IPC transport. D03 tools implement the exec side:

```rust
// Provided by D01 — IPC transport layer
pub trait ToolHandler: Send + Sync {
    fn call(&self, action: &str, args: &serde_json::Value) -> Result<String, String>;
}

// D03's exec functions call through this handler.
// The handler serializes the IPC frame and sends it to Agent.
```

**IPC frame format (proposed by D01):**

```json
{
  "tool": "agent",
  "action": "task_create",
  "args": { "subject": "Add auth", "description": "..." },
  "seed": "<current-session-seed>"
}
```

**Response:**

```json
{
  "result": "[OK] Task created [pending]: ...",
  "success": true
}
```

### 2.3 IPC vs Shared Filesystem — Why IPC

| Factor | Shared Filesystem | IPC (unified socket) |
|--------|-------------------|----------------------|
| Sandbox consistency | Breakable — Tool process can write anywhere | Enforced — only Agent can touch filesystem |
| Two ToolManagers | Semi-split would require duplicating Manager | Single Manager in Agent, Tools is thin client |
| Latency | ~1ms | ~1-5ms (negligible for infrequent task/plan ops) |
| Crash isolation | Tool crash can leave partial writes | Agent owns all state, Tool crash is transparent |
| Phase enforcement | Tool must independently check | Agent enforces at dispatch time |

**Decision: IPC.** The task/plan/memory operations are infrequent (at most once per few turns), so the 1-5ms IPC overhead is negligible. The sandbox consistency guarantee is the overriding concern.

### 2.4 D03's Interface with D01's ToolHandler

D03's `exec_*` functions receive a `&dyn ToolHandler` (provided by D01 at registration) instead of directly calling `session::*`:

```rust
// Current (monolith):
fn exec_task_create(args: &str) -> String {
    let subject = parse_arg(args, "subject");
    // ...
    crate::session::append_memory(&seed, "tasks", &entry);
    // ...
}

// Revised (IPC, stubs the session layer):
// ─── exec functions are registered as closures that capture the ToolHandler ───
//
// At registration time (in D01's framework):
//   let handler: Box<dyn ToolHandler> = ...;
//   let exec = move |args| exec_task_create(args, &*handler);
//   manager.register("agent", ...);

fn exec_task_create(args: &str, handler: &dyn ToolHandler) -> String {
    let subject = parse_arg(args, "subject");
    let description = parse_arg(args, "description");
    // ... validation ...
    
    // Instead of directly writing files, IPC-call the Agent:
    let payload = serde_json::json!({
        "action": "task_create",
        "subject": subject,
        "description": description,
    });
    handler.call("agent", &payload)
}
```

D01 will define the exact `ToolHandler` trait and registration mechanism. D03 adapts to it.

---

## 3. Task Tools (`task.rs`)

### 3.1 Status Workflow (Unchanged)

```
    ┌─────────┐
    │ Pending │ ← task_create()
    └────┬────┘
         │ task_update(status="in_progress")
         ▼
    ┌────────────┐
    │ In Progress│
    └─────┬──────┘
          │
     ┌────┴────┐
     │         │ task_update(status="completed")
     ▼         ▼ task_update(status="cancelled")
 ┌─────────┐ ┌───────────┐
 │ Done    │ │ Cancelled │
 └─────────┘ └───────────┘
```

### 3.2 Storage (Unchanged — owned by Agent)

Tasks stored as Markdown lines in `{session-dir}/tasks-mem.md`:

```markdown
- [pending] Refactor plan_io — Extract write_plan to PlanIo trait
- [in_progress] Add remove_index_entry — Support session deletion
- [completed] Design session module — Document memory_io contract
```

The Tools process never touches this file directly. All reads/writes go through IPC.

### 3.3 Tool Definitions (Unchanged)

| Tool | Parameters | Agent-side Implementation |
|------|-----------|--------------------------|
| `task_create` | `subject` (1-100 chars), `description` (max 200) | `session::append_memory(seed, "tasks", line)` |
| `task_update` | `subject` (exact match), `status` (enum) | `session::read_memory + write_memory` (replace line) |
| `task_list` | `status` (optional filter) | `session::read_memory(seed, "tasks")` + filter |

### 3.4 IPC Messages

**task_create:**
```json
// Tools → Agent
{ "tool": "agent", "action": "task_create", "args": { "subject": "...", "description": "..." } }

// Agent → Tools (success)
{ "success": true, "result": "[OK] Task created [pending]: ..." }

// Agent → Tools (error)
{ "success": false, "result": "[ERROR] task_create: ..." }
```

**task_update:**
```json
{ "tool": "agent", "action": "task_update", "args": { "subject": "...", "status": "in_progress" } }
```

**task_list:**
```json
{ "tool": "agent", "action": "task_list", "args": { "status": "pending" } }
```

### 3.5 Tool-side Changes

The `exec_*` functions in `task.rs` become thin wrappers:

```rust
pub(super) fn exec_task_create(args: &str, handler: &dyn ToolHandler) -> String {
    let subject = parse_arg(args, "subject");
    let description = parse_arg(args, "description");
    
    // Validation still runs in Tools process
    if subject.is_empty() || subject.chars().count() > 100 {
        return "[ERROR] task_create: subject must be 1-100 chars".to_string();
    }
    if description.chars().count() > 200 {
        return "[ERROR] task_create: description max 200 chars".to_string();
    }
    
    // Forward to Agent for actual file I/O
    handler.call("agent", &serde_json::json!({
        "action": "task_create",
        "subject": subject,
        "description": description,
    }))
}
```

**Validation stays in Tools process.** This avoids IPC round-trips for validation errors and keeps the response fast for invalid inputs. Agent-side validation is redundant but harmless (belt-and-suspenders).

---

## 4. Plan Tools (`plan.rs`)

### 4.1 Status Workflow (Unchanged)

```
    ┌─────────┐
    │  Draft  │ ← plan_create()
    └────┬────┘
         │ plan_update(status="active")
         ▼
    ┌─────────┐
    │  Active │
    └────┬────┘
         │
     ┌────┴────┐
     │         │ plan_update(status="done")
     ▼         ▼ plan_update(status="cancelled")
 ┌─────────┐ ┌───────────┐
 │  Done   │ │ Cancelled │
 └─────────┘ └───────────┘
```

### 4.2 Storage (Unchanged — owned by Agent)

Plans are stored as individual Markdown files with frontmatter at `~/.config/dsx/plans/{seed}-{slug}-{date}.md`.

### 4.3 IPC Messages

**plan_create:**
```json
{ "tool": "agent", "action": "plan_create", "args": { "name": "...", "goal": "..." } }
```

**plan_update (including auto-summarization on "done"):**
```json
{ "tool": "agent", "action": "plan_update", "args": { "name": "...", "status": "done" } }
```

**plan_read / plan_list:**
```json
{ "tool": "agent", "action": "plan_read", "args": { "name": "..." } }
{ "tool": "agent", "action": "plan_list", "args": {} }
```

### 4.4 Auto-Summarization on Completion

When `plan_update` transitions a plan to `done`, the Agent side auto-extracts:
- The plan name + goal line
- Saves it as a `DECISION:` entry in long-term memory

This logic stays entirely in the Agent process (C03's `session/plan_io.rs` or the IPC handler). Tools never need to know about it.

```rust
// Agent-side IPC handler pseudocode:
fn handle_plan_update(args) -> String {
    let result = session::update_plan_status(seed, name, status);
    if status == "done" && result.is_some() {
        // Auto-extract summary
        if let Some(content) = session::read_plan_content(seed, name) {
            let decision = extract_goal_as_decision(name, &content);
            session::append_memory(seed, "long", &decision);
        }
    }
    result
}
```

---

## 5. Memory Tools (`memory.rs`)

### 5.1 Memory Tiers (Unchanged)

| Tier | File | Purpose | Retention |
|------|------|---------|-----------|
| `short` | `short-mem.md` | Round summaries, AI-saved notes | Session lifetime |
| `tasks` | `tasks-mem.md` | Task list (shared with task tools) | Session lifetime |
| `long` | `long-mem.md` | Decisions, architecture notes, file records | Multi-session |
| `learning` | `learning-mem.md` | Cross-session self-improvement log | Persistent |

### 5.2 Critical Design: `recall` Search Location

**`recall` must run in the Agent process**, because it reads `learning-mem.md` and `long-mem.md` which only the Agent has access to. This means:

- **Option A**: `recall` sends the query via IPC, Agent performs the search, returns results. Latency: ~5ms.
- **Option B**: Agent pre-loads memory into the IPC response (e.g., as part of session state). Tools search locally. More complex state sync.

**Recommendation: Option A.** `recall` is a slow operation anyway (searching text files), and the IPC round-trip is negligible compared to file I/O. The Tools process never needs a local copy of memory files.

**Pitfall guide** (`pitfall_save` / `pitfall_guide`) also runs in the Agent process via IPC, since it accesses `pitfalls.json`.

### 5.3 IPC Messages

**mem_save (upsert):**
```json
{ "tool": "agent", "action": "mem_save", "args": { "tier": "short", "key": "...", "content": "..." } }
```

**mem_read:**
```json
{ "tool": "agent", "action": "mem_read", "args": { "tier": "short", "key": "..." } }
```

**mem_forget:**
```json
{ "tool": "agent", "action": "mem_forget", "args": { "key": "remove-me" } }
```

**recall (search runs on Agent side):**
```json
{ "tool": "agent", "action": "recall", "args": { "query": "...", "kind": "all", "max_results": 10 } }
```

**pitfall_save / pitfall_guide:**
```json
{ "tool": "agent", "action": "pitfall_save", "args": { "description": "...", "lesson": "...", ... } }
```

---

## 6. Ask Tool (`ask.rs`)

### 6.1 Design (Unchanged)

`ask_user` is a **stub** in the tools layer. The actual interaction is handled by TUI in the Agent process.

### 6.2 Execution Flow

1. AI calls `agent(action="ask", question="...", options=["A. one", "B. two"])`
2. D03's `exec_ask_user()` returns `"[ERROR] ask_user should be intercepted by TUI"`
3. D01's ToolHandler **or TUI's tool dispatch** intercepts `action == "ask"` BEFORE sending via IPC
4. TUI renders the question in a modal, waits for user input
5. User's response is injected as the tool result

**Key detail:** `ask_user` never goes through IPC. The interception happens in the tool dispatch layer (either D01's handler or the TUI's pre-dispatch check), not in the exec function.

### 6.3 IPC-Free Path

```
AI calls agent(action="ask", ...)
  → exec_ask_user() returns error stub
  → BUT: TUI intercepts before exec_agent() is called
      (checks: action == "ask" → render modal → return as fake tool_result)
```

This is TUI's responsibility (E02). D03 just provides the stub.

---

## 7. Status Tool (`status.rs`)

### 7.1 Design

The `status` tool is **unique** — it does not need IPC because it only writes to a shared `AtomicU8` that the Agent can read directly. This works because both processes can share a memory-mapped file or the Agent exposes a simple status socket.

**Option A (recommended): IPC-based** — `status` sends via IPC like any other tool:
```json
{ "tool": "agent", "action": "status", "args": { "state": "coding", "difficulty": "medium" } }
```

Agent-side IPC handler calls `router::set_phase(phase, level)`.

**Option B: Shared memory (mmap)** — Tools writes to a shared memory region, Agent polls. Fragile and platform-specific. Not recommended.

**Decision: Option A.** Consistency with other tools > micro-optimization of avoiding IPC for status. The `AtomicU8` optimization only matters in a single-process model.

### 7.2 Phase Routing Table (Unchanged)

| Phase | Model | Effort | Max Tokens |
|-------|-------|--------|-----------|
| Explore | flash | high | 64K |
| Plan | pro | max | 300K |
| Coding | flash | high | 96K |
| Debug (low) | pro | high | 64K |
| Debug (medium) | pro | max | 96K |
| Debug (high) | pro | max | 128K |
| Chat | flash | high | 96K |

### 7.3 Agent-Side Behavior

When Agent IPC receives `action == "status"`:
1. Parse state + difficulty
2. Call `router::set_phase(phase, level)` — writes to `AtomicU8`
3. Read `AUTO_MODE` flag
4. If auto-on, look up `phase_config()` → return model/effort
5. Return status string (matching current text output format)

The next `agent_loop` call automatically picks up the new phase via `router::read_phase()` — no explicit sync needed.

---

## 8. File Access Summary: Tools vs Agent

| File | Accessed By | Mechanism |
|------|------------|-----------|
| `{seed}-{date}/tasks-mem.md` | Agent only | Tools → IPC → Agent → `memory_io` |
| `{seed}-{date}/short-mem.md` | Agent only | Tools → IPC → Agent → `memory_io` |
| `{seed}-{date}/long-mem.md` | Agent only | Tools → IPC → Agent → `memory_io` |
| `{seed}-{date}/learning-mem.md` | Agent only | Tools → IPC → Agent → `memory_io` |
| `plans/{seed}-{slug}-{date}.md` | Agent only | Tools → IPC → Agent → `plan_io` |
| `sessions/pitfalls.json` | Agent only | Tools → IPC → Agent → `memory_io` |
| (none — status uses `AtomicU8`) | — | Tools → IPC → Agent → `router::set_phase` |

**The Tools process touches no files.** All persistence is handled by the Agent process.

---

## 9. Integration with D01's Registration Interface

### 9.1 D01's Expected Interface

D01 will define something like:

```rust
// D01's ToolHandler trait (proposed shape)
pub trait ToolHandler: Send + Sync {
    fn call(&self, tool_name: &str, action: &str, args: &serde_json::Value) -> ToolResult;
}

// D01's registration mechanism (proposed shape)
pub struct ToolRegistration {
    pub name: &'static str,        // "agent"
    pub def: fn() -> ToolDef,       // tool definition
    pub safety: fn(&str) -> SafetyLevel,
    pub exec: Box<dyn Fn(&str, &dyn ToolHandler) -> String>,
}
```

### 9.2 D03 Adaptation

Each D03 tool module will:
1. Keep its `_def()` function unchanged (returns `ToolDef`)
2. Wrap `exec_*` functions to accept `&dyn ToolHandler` via registration closure
3. Keep validation logic in the exec function; defer file I/O to the handler

```rust
// task.rs — adapted
pub(super) fn task_create_def() -> ToolDef { /* unchanged */ }
pub(super) fn exec_task_create(args: &str, handler: &dyn ToolHandler) -> String {
    // validation ...
    handler.call("agent", "task_create", &serde_json::json!({...}))
}
pub(super) fn exec_task_update(args: &str, handler: &dyn ToolHandler) -> String { ... }
pub(super) fn exec_task_list(args: &str, handler: &dyn ToolHandler) -> String { ... }
```

### 9.3 Registration in D01's Manager

```rust
// Pseudocode — D01's manager registration
let handler: Box<dyn ToolHandler> = get_ipc_handler();

manager.register(ToolRegistration {
    name: "agent",
    def: agent::agent_def,
    safety: agent::classify_agent,
    exec: move |args| {
        let action = parse_arg(args, "action");
        match action {
            "task_create" => task::exec_task_create(args, &*handler),
            "task_update" => task::exec_task_update(args, &*handler),
            "task_list"   => task::exec_task_list(args, &*handler),
            "plan_create" => plan::exec_plan_create(args, &*handler),
            // ...
        }
    },
});
```

---

## 10. Summary: Changes Required

| File | Change | Reason |
|------|--------|--------|
| `task.rs` | `exec_*` functions accept `&dyn ToolHandler`; replace `session::` calls with `handler.call()` | IPC adaptation |
| `plan.rs` | Same pattern | IPC adaptation |
| `memory.rs` | Same pattern; `recall` search becomes IPC call instead of local file search | IPC adaptation |
| `ask.rs` | No change — stub remains as-is | TUI interception unchanged |
| `status.rs` | `exec_status` uses IPC instead of direct `router::set_phase()` | IPC consistency |

The `_def()` functions and `ToolDef` structs remain **unchanged** across all five files. Only the exec functions change, and only in their I/O strategy (direct call → IPC via D01's handler).

---

## 11. Open Questions for D01

1. **ToolHandler trait signature:** What is the exact `ToolHandler::call()` signature? Does it return `Result<String, String>` or `String` (with error prefix)? We need alignment before any code changes.

2. **Registration mechanism:** How does D01 expose the `ToolHandler` to D03's exec functions? Via closure capture at registration time, or via a global `OnceLock<Box<dyn ToolHandler>>`?

3. **Executor function signature:** Does D01 expect exec functions to be `fn(&str, &dyn ToolHandler) -> String`, or does it wrap them internally and expose a simpler `fn(&str) -> String`?

4. **IPC response format:** Does the IPC response include the raw result string, or does it return structured JSON that the exec function must format? (Preference: raw string — let Agent-side format it, Tools just pass through.)

5. **ask_user interception:** Does D01's ToolHandler handle the `ask_user` interception, or does TUI (E02) handle it before dispatching? We need a clear boundary.
