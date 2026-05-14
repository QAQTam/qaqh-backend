# Design: C03 — Session + Router + Skills Subsystem

**Owner:** Team C member C03
**Date:** 2026-05-14
**Status:** Draft for review

---

## 1. Scope

Responsible for six modules within `dsx-agent`:

| Module | File | Role |
|--------|------|------|
| Session | `session/` (mod, persist, index, restore, snapshot, plan_io, memory_io) | Conversation persistence, crash recovery, index |
| Router | `router.rs` | TaskPhase + DebugLevel → model/effort routing |
| Tokenizer | `tokenizer.rs` | Token counting (DeepSeek / heuristic fallback) |
| Prompt | `prompt.rs` | System prompt templates (EN/ZH) |
| Skills | `skills.rs` | Skill discovery / matching / loading |
| Config | `config.rs` | Config loading (file + env var + profile) |
| Log | `dsc_log.rs` | File logging infrastructure |

---

## 2. Architecture Overview

```
┌────────────────────────────────────────────────────────────────────┐
│                        dsx-agent (process boundary)                 │
│                                                                     │
│  ┌──────────┐   ┌──────────┐   ┌──────────┐   ┌────────────────┐  │
│  │  Config   │──▶│  Prompt  │──▶│ Tokenizer│──▶│  Skills Index   │  │
│  └──────────┘   └──────────┘   └──────────┘   └────────────────┘  │
│       │              │              │                    │          │
│       │              │              │                    │          │
│  ┌────▼──────────────▼──────────────▼────────────────────▼──────┐  │
│  │                     Session Layer                              │  │
│  │  ┌────────┐ ┌──────────┐ ┌─────────┐ ┌──────────┐ ┌───────┐  │  │
│  │  │persist │ │  index   │ │ restore │ │ snapshot │ │plan_io│  │  │
│  │  └────────┘ └──────────┘ └─────────┘ └──────────┘ └───────┘  │  │
│  │  ┌──────────┐                                                    │  │
│  │  │ memory_io│ ←── shared with C02 (memory subsystem)           │  │
│  │  └──────────┘                                                    │  │
│  └─────────────────────────────────────────────────────────────────┘  │
│       │                                            ▲                  │
│       │ (save/load/snapshot)                       │ (phase config)   │
│       ▼                                            │                  │
│  ┌──────────────┐        ┌──────────┐              │                  │
│  │ Orchestrator  │───────▶│  Router  │──────────────┘                  │
│  │ (C01)         │        └──────────┘                                 │
│  └──────────────┘                                                      │
│       │                     ┌──────────────────┐                       │
│       │  dsc_log            │  Disk (filesystem)│                       │
│       │  (init + seed)      │  sessions/       │                       │
│       │                     │  plans/          │                       │
│       └─────────────────────│  skills/         │                       │
│                             │  logs/           │                       │
│                             └──────────────────┘                       │
└────────────────────────────────────────────────────────────────────┘
```

### 2.1 Data Flow: Agent Startup

```
TUI start → Config::load() → dsc_log::init() → skills::global_skills()
  ↓
(first user message)
  ↓
session::generate_seed() → dsc_log::set_session(seed)
  ↓
router::detect_initial_phase(input) [if auto_mode]
  ↓
session::save_live_snapshot(seed, messages, model, ...)
  ↓
router::phase_config(phase, level) → (model, effort, max_tokens)
  ↓
router::phase_prompt_suffix(phase, lang) → injected into system prompt
  ↓
agent_loop sends API request
```

### 2.2 Data Flow: Per-Turn

```
User input → agent_loop:
  1. skill_index.match_skills(input) → matched skills loaded into context
  2. ContextBuilder (C02) assembles: system + memory + conversation
  3. tokenizer::estimate_messages_tokens() for context pressure check
  4. API call → streaming response
  5. session::save_live_snapshot() after each tool result (crash recovery)
  6. On turn completion: memory_io::append_memory() or write_memory()
```

### 2.3 Data Flow: Session End

```
User exits or /clear:
  session::finalize_session(seed, messages, model, effort, semantic_memory)
    → writes session.json
    → updates index.json
    → cleans up .live.json (snapshot)
```

---

## 3. Module Design

### 3.1 Session Layer (`session/`)

#### 3.1.1 Persistence (`persist.rs`)

**Current:** Free functions `save_session` (private), `finalize_session` (public), `load_session`, `load_session_or_live`.

**Design:**

```
// ── SessionPersistence trait (for testability / alternate backends) ──
pub trait SessionPersistence: Send + Sync {
    fn save(&self, file: &SessionFile) -> Result<()>;
    fn load(&self, seed: &str) -> Result<Option<SessionFile>>;
    fn load_live(&self, seed: &str) -> Result<Option<(SessionFile, bool)>>;
    fn delete_live(&self, seed: &str) -> Result<()>;
}

// ── Production implementation ──
pub struct FileSessionPersistence {
    sessions_dir: PathBuf,
}

impl SessionPersistence for FileSessionPersistence { ... }

// ── Free functions (thin wrappers for backward compat and public API) ──
pub fn finalize_session(seed: &str, messages: &[Message], ...) { ... }
pub fn load_session(seed: &str) -> Option<SessionFile> { ... }
pub fn load_session_or_live(seed: &str) -> Option<(SessionFile, bool)> { ... }
```

**Internal:** `save_session` serializes `SessionFile` with `task_phase: Some(router::read_phase())` — this is the coupling point between session and router. It is intentional: the session record remembers what phase it was in.

#### 3.1.2 Index (`index.rs`)

**Current:** `load_index`, `update_index_entry` — JSON index of all sessions.

**Design (minimal change):**

```
pub fn load_index() -> Vec<SessionMeta> { ... }
pub fn update_index_entry(file: &SessionFile) { ... }
pub fn remove_index_entry(seed: &str);  // ADD: for session deletion
```

**Key invariant:** Index is a performance optimization (session listing), not a source of truth. If index is corrupted, sessions can be recovered by scanning `sessions/` directory.

#### 3.1.3 Restore (`restore.rs`)

**Current:** `find_live_sessions` — scans for `*.live.json`.

**Design (unchanged):** Simple directory scan. No changes needed.

#### 3.1.4 Snapshot (`snapshot.rs`)

**Current:** `save_live_snapshot`, `load_live_snapshot`, `delete_live_snapshot` — atomic write (tmp → rename).

**Design (unchanged):** Keep the atomic write pattern. This is crash-recovery infrastructure and must remain simple and reliable.

#### 3.1.5 Plan I/O (`plan_io.rs`)

**Current:** CRUD for `.md` plan files with YAML-free frontmatter.

**Design (unchanged):** The frontmatter-based markdown format is deliberate — readable by both humans and TUI. Plan files live in `~/.config/dsx/plans/`.

#### 3.1.6 Memory I/O (`memory_io.rs`) — Interface Boundary with C02

This is the **shared interface** between C03 (session) and C02 (memory). C02 holds the in-memory data structures (`ShortTermMemory`, `LongTermMemory`, `SemanticMemory`, `PitfallGuide`). C03's `memory_io` provides the persistence layer.

**Current functions and their C02 consumers:**

| Function | Memory Type | Consumed By | Notes |
|----------|-------------|-------------|-------|
| `load_pitfalls` / `save_pitfalls` | `PitfallGuide` | C02 (learning.rs) | Cross-session, loaded at startup |
| `read_memory` / `write_memory` | Short/Long-term | C02 | Reads during context assembly, writes after turns |
| `append_memory` | Short-term | C02 | Appends new observations |
| `append_learning` | Learning log | C02 | Cross-session self-improvement |
| `write_memory_preserving_notes` | Short-term (AI notes) | C02 | Preserves AI-authored `- key: value` lines |
| `write_semantic_memory` / `read_semantic_entry` | SemanticMemory | C02 / SessionFile | Persisted in session directory |
| `forget_memory_key` | Any | C02 | Deletes matching lines by key |

**Contract:**

```
// Interface: memory_io functions accept seed + tier strings.
// C02 controls what gets written; C03 controls WHERE it goes on disk.

// File layout per session:
//   ~/.config/dsx/sessions/{seed}-{date}/
//     session.json          — final SessionFile
//     session.live.json     — live snapshot (crash recovery)
//     short-mem.md          — ShortTermMemory render
//     long-mem.md           — LongTermMemory render
//     learning-mem.md       — Cross-session learning log
//     semantic-mem.json     — SemanticMemory (for glance tool queries)
```

> **Recommendation to C02:** Wrap `memory_io` functions behind a `MemoryStore` trait in C02's memory module so that the in-memory structures (`ShortTermMemory`, `LongTermMemory`) accept a generic persistence backend. C03 provides the `FileMemoryStore` implementation.

---

### 3.2 Router (`router.rs`)

**Current design is solid. Key elements:**

#### 3.2.1 TaskPhase + DebugLevel Encoding

```
[bits 2:0] = TaskPhase (Chat=0, Explore=1, Plan=2, Coding=3, Debug=4)
[bits 4:3] = DebugLevel (Low=0, Medium=1, High=2)
```

Stored in `AtomicU8` — lock-free, cross-task communication between:
- **status tool** (called by AI, handled by tool dispatch) → calls `set_phase`
- **agent_loop** (in orchestration, C01) → calls `read_phase` + `phase_config`
- **snapshot/persist** → calls `read_phase` for saving

#### 3.2.2 Routing Table

```
TaskPhase::Explore  → deepseek-v4-flash, effort=high,     max_tokens=64K
TaskPhase::Plan     → deepseek-v4-pro,   effort=max,       max_tokens=300K
TaskPhase::Coding   → deepseek-v4-flash, effort=high,      max_tokens=96K
TaskPhase::Debug    → deepseek-v4-pro,   effort=high|max,  max_tokens=64K-128K
TaskPhase::Chat     → deepseek-v4-flash, effort=high,      max_tokens=96K
```

#### 3.2.3 Phase Prompt Suffixes

Located in `router.rs` for convenience (they are phase-specific). Each phase has a short instruction string in EN and ZH. C01's `agent_loop` injects these into the system prompt.

> **Design decision:** Keep phase prompt suffixes in `router.rs`, not `prompt.rs`, because:
> - They are inherently tied to phase routing logic
> - They are short and change when routing semantics change
> - `prompt.rs` holds the stable base system prompt

#### 3.2.4 Detect Initial Phase

Keyword-based. Order of checking matters — Explore > Plan > Debug > Code > Chat (default). This is only used on the first user message in auto_mode.

#### 3.2.5 Integration with C01

```
// C01 (agent_loop.rs) usage:
fn handle_start_agent_loop(app) {
    let phase = router::read_phase();
    let level = router::read_debug_level();
    let config = router::phase_config(phase, level);
    let suffix = router::phase_prompt_suffix(phase, lang);

    // Use config.model, config.effort, config.max_tokens for API call
    // Inject suffix into system message
}
```

---

### 3.3 Tokenizer (`tokenizer.rs`)

#### 3.3.1 Alignment with A03's TokenCount Trait

The current implementation uses free functions. To align with A03's expected `TokenCount` trait:

```
// ── Proposed trait (in dsx-types or dsx-agent) ──
pub trait TokenCounter: Send + Sync {
    fn count_tokens(&self, text: &str) -> u32;
    fn count_message_tokens(&self, msg: &Message) -> u32;
    fn estimate_messages_tokens(&self, messages: &[Message]) -> u32;
    fn format_tokens(&self, n: u32) -> String;
    fn context_usage_ratio(&self, used: u32, max_tokens: u32) -> f64;
}

// ── DeepSeek implementation ──
pub struct DeepSeekTokenizer {
    tokenizer: OnceLock<Option<Tokenizer>>,
}

impl TokenCounter for DeepSeekTokenizer {
    fn count_tokens(&self, text: &str) -> u32 {
        // Try actual tokenizer, fall back to heuristic
    }
    // ...
}

// ── Heuristic fallback ──
pub struct HeuristicTokenizer;

impl TokenCounter for HeuristicTokenizer {
    fn count_tokens(&self, text: &str) -> u32 {
        // CJK-aware heuristic
    }
}
```

**C03 to A03:** We expect the `TokenCount` trait to be defined in `dsx-types` or in the `dsx-agent` trait exports. C03 provides `DeepSeekTokenizer` and `HeuristicTokenizer` implementations. A global `token_counter()` singleton returns a `&dyn TokenCounter` for C01/C02 consumption.

#### 3.3.2 TokenBreakdown

Used by `ContextBuilder` (C02) for layered token accounting:

```
pub struct TokenBreakdown {
    pub system: u32,      // System prompt
    pub semantic: u32,    // Semantic memory block
    pub long_mem: u32,    // Long-term memory / backbone
    pub episodic: u32,    // Conversation history (after compaction)
    pub total: u32,
}
```

---

### 3.4 Prompt (`prompt.rs`)

**Current:** Two static strings (`SYSTEM_PROMPT_EN`, `SYSTEM_PROMPT_ZH`). Simple `system_prompt(lang)` function.

**Design (minimal change):**

```
pub struct SystemPrompt {
    pub en: &'static str,
    pub zh: &'static str,
}

impl SystemPrompt {
    pub fn for_lang(lang: &str) -> &'static str {
        if lang == "zh" { SYSTEM_PROMPT_ZH } else { SYSTEM_PROMPT_EN }
    }
}
```

**Integration with C01:** C01 calls `prompt::system_prompt(&config.prompt_lang)` and appends phase suffix from `router::phase_prompt_suffix()`.

---

### 3.5 Skills (`skills.rs`)

#### 3.5.1 Discovery

Scans `~/.config/dsx/skills/*/SKILL.md` for YAML frontmatter (parsed without YAML dependency — simple line scan for `name:` and `description:`).

#### 3.5.2 Matching

```
SkillIndex::match_skills(input: &str) -> Vec<&SkillMeta>
```

Current algorithm:
1. Split user input into keywords (len > 2)
2. Score each skill by counting keyword matches in description
3. Return top 3 by score

**Improvement (suggested):** Add name-based exact match scoring (if input contains a skill name verbatim, boost its score significantly).

#### 3.5.3 Loading

```
SkillIndex::load_skill_body(name)  → strips frontmatter, returns body
SkillIndex::load_reference(name, path) → reads from references/ subdirectory
```

#### 3.5.4 Integration with C01

```
// In agent_loop.rs (handle_send_message):
let matched = app.skill_index.match_skills(&input);
for skill in matched {
    if let Some(body) = app.skill_index.load_skill_body(&skill.name) {
        app.active_skill_bodies.push((skill.name, body));
        // These bodies are injected as system messages by ContextAssembler
    }
}
```

#### 3.5.5 Global Singleton

```
static GLOBAL_SKILLS: OnceLock<SkillIndex> = OnceLock::new();
pub fn global_skills() -> &'static SkillIndex { ... }
```

Scanned once at startup. Call `skills::global_skills()` from anywhere.

---

### 3.6 Config (`config.rs`)

#### 3.6.1 Priority Chain

```
Defaults < config.json < active_profile < flat fields < env vars
```

Where env vars are:
- `DEEPSEEK_API_KEY` — API key
- `DEEPSEEK_MODEL` — model name
- `DEEPSEEK_BASE_URL` — API endpoint
- `DEEPSEEK_MAX_TOKENS`, `DEEPSEEK_CONTEXT_LIMIT` — limits
- `DEEPSEEK_EFFORT` — effort setting
- `DEEPSEEK_PROMPT_LANG` — language

#### 3.6.2 Config Structure

```
pub struct Config {
    pub api_key: String,
    pub model: String,
    pub base_url: String,
    pub max_tokens: u32,
    pub context_limit: u32,
    pub effort: Option<String>,
    pub prompt_lang: String,
    pub profiles: HashMap<String, ProfileConfig>,
    pub active_profile: String,
    pub auto_mode: bool,
}
```

#### 3.6.3 Profile System

- `/profile <name>` — switch to named profile
- `/profile save <name>` — save current config as a profile
- `/profile delete <name>` — remove a profile
- `"default"` profile is protected (cannot be deleted)

Each `ProfileConfig` stores: model, max_tokens, effort, context_limit, base_url, prompt_lang.

---

### 3.7 Log (`dsc_log.rs`)

**Current:** Uses `simplelog::WriteLogger`. Log level from `DSC_LOG` env var. File at `~/.config/dsc/logs/dsc.pending.log` renamed to `{seed}.{date}.log` once session starts.

**Design (unchanged):** This is infrastructure. Only two public functions:
- `init()` — call once at startup
- `set_level("debug"/"trace"/...)` — runtime level change

---

## 4. File Layout on Disk

```
~/.config/dsx/
├── config.json                    # Config + profiles
├── sessions/
│   ├── index.json                 # Session index (performance optimization)
│   ├── pitfalls.json              # Cross-session PitfallGuide
│   ├── {seed}-{date}/
│   │   ├── session.json           # Final session
│   │   ├── session.live.json      # Live snapshot
│   │   ├── session.live.tmp       # Temp for atomic write
│   │   ├── semantic-mem.json      # SemanticMemory
│   │   ├── short-mem.md           # ShortTermMemory
│   │   ├── long-mem.md            # LongTermMemory
│   │   ├── learning-mem.md        # Learning log
│   │   └── <tier>-mem.md          # Generic tiered memory files
├── plans/
│   └── {seed}-{slug}-{date}.md    # Plan files
├── skills/
│   └── <name>/
│       ├── SKILL.md               # Skill definition (frontmatter + body)
│       └── references/            # Reference files
│           └── ...
└── logs/
    ├── dsc.pending.log            # Before session is named
    └── {seed}.{date}.log          # After session is named
```

---

## 5. C02 ↔ C03 Memory I/O Contract

### 5.1 Shared Files

| File | Written By | Read By | Format |
|------|-----------|---------|--------|
| `{seed}-mem.md` | C03 (`memory_io`) | C02 (context assembly) | Markdown, ~4K token truncation |
| `semantic-mem.json` | C03 (`memory_io`) | C02 + glance tool | JSON (`SemanticMemory`) |
| `pitfalls.json` | C03 (`memory_io`) | C02 (learning system) | JSON (`PitfallGuide`) |

### 5.2 Interface

```
// C03 provides these — C02 calls them.
// All accept: seed: &str (session identifier), tier: &str (memory level)

// Read for context injection (C02 context assembly):
read_memory(seed, tier) -> String        // Truncated to ~4K tokens

// Write after turn (C02):
write_memory(seed, tier, content)        // Overwrite
append_memory(seed, tier, line)          // Append with size cap
write_memory_preserving_notes(seed, ...) // Overwrite but keep AI notes
forget_memory_key(seed, key)            // Delete lines containing key

// Semantic memory (C02's extractor):
write_semantic_memory(seed, &SemanticMemory)
read_semantic_entry(seed, file_path) -> Option<FileSemanticEntry>

// Cross-session learning:
load_pitfalls() -> PitfallGuide
save_pitfalls(&PitfallGuide)
append_learning(seed, entry)
```

### 5.3 Recommended C02 Wrapper

C02 should create a thin wrapper in its `memory/mod.rs`:

```
pub struct MemoryStore {
    seed: String,
}

impl MemoryStore {
    pub fn new(seed: &str) -> Self { ... }

    pub fn read_short_term(&self) -> ShortTermMemory { ... }
    pub fn write_short_term(&self, mem: &ShortTermMemory) { ... }
    pub fn read_long_term(&self) -> LongTermMemory { ... }
    pub fn write_long_term(&self, mem: &LongTermMemory) { ... }
    pub fn read_semantic(&self) -> SemanticMemory { ... }
    pub fn write_semantic(&self, mem: &SemanticMemory) { ... }
    pub fn load_pitfalls(&self) -> PitfallGuide { ... }
    pub fn save_pitfalls(&self, guide: &PitfallGuide) { ... }
}
```

C03 will keep `memory_io.rs` stable — treat it as a contract.

---

## 6. Integration Points with Other Teams

### 6.1 With C01 (Orchestration) — CRITICAL

| Integration | Direction | Mechanism |
|-------------|-----------|-----------|
| Phase routing | C03 → C01 | `router::phase_config(phase, level)` → `PhaseConfig { model, effort, max_tokens }` |
| Phase prompt suffix | C03 → C01 | `router::phase_prompt_suffix(phase, lang)` → `Option<&str>`, injected into system message |
| Auto-mode initial phase | C03 → C01 | `router::detect_initial_phase(input)` → `TaskPhase` |
| Status tool parsing | C03 → C01 | `router::parse_status_args(args)` → `(TaskPhase, DebugLevel)` |
| Session save | C01 → C03 | `session::finalize_session(...)`, `session::save_live_snapshot(...)` |
| Session restore | C01 → C03 | `session::find_live_sessions()` → `Vec<SessionMeta>`, `session::load_session(seed)` |
| Token counting | C03 → C01 | `tokenizer::estimate_messages_tokens(msgs)` for context pressure |
| Skills injection | C03 → C01 | `SkillIndex::match_skills(input)` → load bodies → inject as system messages |

### 6.2 With C02 (Context + Memory)

See Section 5 above. The key shared interface is `memory_io.rs`.

### 6.3 With A03 (TokenCount Trait)

C03 provides `DeepSeekTokenizer` implementing the A03-defined `TokenCounter` trait. Global singleton access via `tokenizer::global_counter() -> &'static dyn TokenCounter`.

### 6.4 With Team D (Tools)

- **`status` tool** → calls `router::parse_status_args()` + `router::set_phase()`
- **`mem_save` tool** → calls `memory_io::append_memory()`
- **`list_skills` tool** → calls `skills::tool_list_skills()`
- **`read_skill_ref` tool** → calls `skills::tool_read_skill_ref()`

### 6.5 With Team E (TUI)

- Config loaded at startup, displayed in `/lang`, `/model`, `/effort`, `/profile` commands
- Skills listed in `/skills` or similar UI
- Session index displayed for resume selection
- Live session detection on startup

---

## 7. Key Design Decisions

### 7.1 Why Session Persistence Stays in the Agent Process

All session I/O (save/load/snapshot/restore/plan/memory) runs in the agent process. No RPC or cross-process calls. Rationale:
- Session operations are synchronous file I/O — no benefit from offloading
- The agent must be self-contained for offline/resume scenarios
- Multi-process coordination would add complexity without corresponding value

### 7.2 Why Phase Prompt Suffixes Are in `router.rs`

They are phase-specific instructions that change when routing semantics change. Keeping them in `router.rs` avoids a circular dependency (`prompt.rs` has the base system prompt, `router.rs` has phase-specific additions).

### 7.3 Why AtomicU8 for Phase/Level

The `AtomicU8` encoding (`3 bits phase + 2 bits debug`) enables:
- Lock-free reads from any thread (token bucket, snapshot, agent_loop)
- Atomic swap from the status tool handler
- Single-word synchronization without mutex overhead

### 7.4 Why YAML-Free Frontmatter Parsing in Skills and Plans

Avoiding a YAML dependency (serde_yaml) reduces compile time and binary size. The frontmatter format is deliberately simple: `key: value` lines between `---` markers.

---

## 8. Open Questions

1. **C02:** Does the `MemoryStore` wrapper suggestion (Section 5.3) align with your memory subsystem design? We need to finalize the `memory_io` → C02 interface before implementation.

2. **A03:** What is the exact `TokenCounter` trait signature you expect? We'll align `DeepSeekTokenizer` to it.

3. **C01:** Should `PhaseConfig` be a struct in `router.rs` (current) or a trait in `dsx-types`? If the latter, C03 provides `DefaultPhaseRouter` as the canonical implementation.

4. **Session deletion:** Should we add a `/session delete` command to clean up old sessions? This would require `remove_index_entry` in `index.rs`.

---

## 9. Implementation Plan

### Phase 1 (Current — analysis complete)
- [x] Read all assigned files
- [x] Design document (this file)

### Phase 2 (Structural)
- [ ] Extract `SessionPersistence` trait from `persist.rs`
- [ ] Add `remove_index_entry` to `index.rs`
- [ ] Define `TokenCounter` trait (align with A03)
- [ ] Rename existing tokenizer functions to implement `DeepSeekTokenizer`
- [ ] Minor refactoring of `router.rs` to isolate routing table

### Phase 3 (Integration)
- [ ] Finalize `memory_io` contract with C02
- [ ] Validate integration tests with C01 agent_loop
- [ ] Validate skills injection path with C01

### Phase 4 (Polish)
- [ ] Comprehensive error handling for edge cases (corrupted files, missing directories)
- [ ] Session cleanup utilities (old/large session archival)
