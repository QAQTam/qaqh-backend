# DSX TUI — 应用核心设计文档

**Team E (dsx-tui) · E01 应用核心**
**设计范围：** AppState / AgentConnection / StreamBuffer / 主事件循环 / 斜杠命令 / 模式状态机

---

## 1. 架构总览

```
┌──────────────────────────────────────────────────────┐
│                    Main Event Loop                     │
│  ┌─────────────┐  ┌──────────────┐  ┌──────────────┐ │
│  │ Keyboard     │  │ IPC          │  │ Render       │ │
│  │ Event Poll   │  │ Event Poll   │  │ Tick (timer) │ │
│  └──────┬───────┘  └──────┬───────┘  └──────┬───────┘ │
│         │                 │                 │          │
│         ▼                 ▼                 ▼          │
│  ┌─────────────────────────────────────────────────┐  │
│  │               AppState (read/write)               │  │
│  │  ┌────────────┐  ┌───────────────┐               │  │
│  │  │ AgentConn  │  │ StreamBuffer  │               │  │
│  │  └────────────┘  └───────────────┘               │  │
│  └─────────────────────────────────────────────────┘  │
└──────────────────────────────────────────────────────┘
```

**核心拆分原则：** 将当前 app.rs (单文件 ~510 行, 50+ 字段) 按职责拆分为三个独立模块。

---

## 2. AppState — UI 状态

### 职责
仅管理 TUI 渲染和键盘交互所需的状态。与 Agent 通信无关。

```rust
// src/dsx-tui/app_state.rs
pub(crate) struct AppState {
    // ── 输入状态 ──
    pub input: String,
    pub input_cursor: usize,

    // ── 聊天显示 ──
    pub messages: Vec<Message>,          // 扁平化消息快照（渲染唯读）
    pub scroll_offset: usize,
    pub auto_scroll: bool,
    pub last_max_scroll: usize,

    // ── 模式 ──
    pub mode: Mode,

    // ── 斜杠菜单 ──
    pub slash_open: bool,
    pub slash_filter: String,
    pub slash_idx: usize,
    pub slash_items: Vec<(String, String)>,
    pub slash_submenu: Option<SubMenuState>,
    pub esc_just_closed_menu: bool,
    pub exit_prompt: bool,
    pub skip_all: bool,

    // ── 状态栏信息 ──
    pub status_msg: String,
    pub balance: Option<String>,
    pub cache_hit_pct: f64,
    pub reasoning_tokens: u32,
    pub token_estimate: u32,
    pub token_breakdown: Option<TokenBreakdown>,
    pub api_usage: Option<UsageInfo>,
    pub session_tokens: u64,
    pub health_status_line: String,

    // ── 正在执行的工具面板 ──
    pub tool_code_path: String,
    pub tool_code_content: String,
    pub tool_code_action: String,
    pub tool_code_status: Option<&'static str>,
    pub exec_scroll_offset: usize,

    // ── Intent picker ──
    pub intent_question: String,
    pub intent_options: Vec<String>,

    // ── Sudo ──
    pub sudo_pending: Vec<(ToolCall, String)>,
    pub sudo_password: String,

    // ── 应用级状态 ──
    pub tools_enabled: bool,
    pub auto_mode: bool,
    pub dev_mode: bool,
    pub phase: AgentPhase,
    pub phase_count: u32,
    pub dirty: bool,                        // 需要触发快照
    pub last_snapshot: Instant,
    pub session_seed: String,
    pub session_start: u64,
    pub session_list: Vec<SessionMeta>,
    pub session_pick_idx: usize,
}
```

### 移出 AppState 的内容
| 字段 | 目标模块 | 原因 |
|------|----------|------|
| `ctx: ContextAssembler` | AgentConnection | 消息组装属于 Agent 通信协议 |
| `stream_*` 系列字段 | StreamBuffer | 流式缓冲有独立生命周期 |
| `tx: mpsc::Sender` | AgentConnection | IPC 通道不属于 UI |
| `config: Config` | AgentConnection / main | 配置跨越多层，由 main 传入 |
| `monitor, health` | AgentConnection | 健康监控伴随工具调用 |
| `exec_*` 相关 | AgentConnection | 执行跟踪属于 Agent 管理 |
| `semantic_memory, long_mem, short_mem` | AgentConnection | 记忆属于 Agent 代理状态 |
| `skill_index, active_skill_bodies` | AgentConnection | 技能属于 Agent 工具集 |
| `turn_scores, auto_verify` | AgentConnection | turn 评分属于会话管理 |

---

## 3. AgentConnection — IPC 管理

### 职责
管理 TUI 与 Agent 子进程之间的 JSON-LP IPC 通信。

```rust
// src/dsx-tui/agent_conn.rs
pub(crate) struct AgentConnection {
    // ── IPC 通道 ──
    agent_tx: mpsc::Sender<IpcFrame>,     // 发往 Agent
    agent_rx: mpsc::Receiver<IpcFrame>,   // 从 Agent 接收

    // ── 会话组装 ──
    ctx: ContextAssembler,

    // ── 配置 ──
    config: Config,

    // ── 工具监控 ──
    monitor: MonitorState,
    health: DsAgentsHealthPlatform,
    tool_failures: u32,
    tool_calls_this_turn: u32,

    // ── 执行跟踪 ──
    exec_pending: usize,
    exec_started_at: Option<Instant>,
    exec_child_pids: Vec<u32>,
    exec_buffer: String,
    files_written_this_turn: Vec<String>,
    consecutive_tool_turns: u32,
    gate_message: Option<String>,

    // ── 记忆系统 ──
    semantic_memory: SemanticMemory,
    long_mem: LongTermMemory,
    short_mem: ShortTermMemory,
    pitfall_guide: PitfallGuide,

    // ── 技能索引 ──
    skill_index: SkillIndex,
    active_skill_bodies: Vec<(String, String)>,

    // ── Turn 评分 ──
    turn_scores: Vec<f32>,

    // ── 项目管理 ──
    project_map: String,
    has_explored: bool,
    turns_since_last_read: u32,
    current_task_phase: TaskPhase,
}
```

### IPC 帧协议 (采用 A02 定义的 JSON-LP)

```rust
// ── TUI → Agent 帧 ──
pub(crate) enum TuiToAgent {
    UserInput {
        text: String,
        tools_enabled: bool,
        mode: String,
    },
    SlashCommand {
        cmd: String,
        args: Vec<String>,
    },
    ConfirmTools {
        ids: Vec<String>,
    },
    Cancel,
    ModeChange {
        mode: String,
    },
}

// ── Agent → TUI 帧 ──
pub(crate) enum AgentToTui {
    StreamPayload {
        delta: String,
        reasoning: Option<String>,
        tool_progress: Vec<(String, String)>,
    },
    StreamDone {
        usage: Option<UsageInfo>,
    },
    StreamError {
        message: String,
    },
    TuiModeRequest {
        mode: String,
        context: Option<String>,
    },
    BalanceResult {
        balance: String,
    },
}
```

### AgentConnection 方法

```rust
impl AgentConnection {
    pub fn spawn(config: Config, resume: bool) -> Self;
    pub fn send_user_input(&mut self, text: &str);
    pub fn send_slash_command(&mut self, cmd: &str);
    pub fn send_confirm(&mut self, ids: &[String]);
    pub fn send_cancel(&mut self);
    pub fn try_recv(&mut self) -> Option<AgentToTui>;
    pub fn start_agent_loop(&mut self);
    pub fn prepare_messages(&mut self) -> Vec<Message>;
    pub fn repair_messages(&mut self);
    pub fn sync_messages(&self) -> Vec<Message>;  // ctx → flat view
}
```

---

## 4. StreamBuffer — 流式缓冲

### 职责
独立管理 Agent 流式输出内容，与 UI 渲染解耦。

```rust
// src/dsx-tui/stream_buffer.rs
pub(crate) struct StreamBuffer {
    // ── 当前流内容 ──
    pub content: String,                      // 可见文本
    pub reasoning: String,                    // 推理链
    pub tool_progress: Vec<(String, String)>, // (name, args_so_far)
    pub cancelled: bool,
    pub last_activity: Instant,

    // ── 工具结果 ──
    pub tool_results: Vec<(String, String)>,
    pub pending_tools: Vec<(ToolCall, SafetyLevel, String)>,
}
```

### 方法

```rust
impl StreamBuffer {
    pub fn new() -> Self;
    pub fn clear(&mut self);
    pub fn is_live(&self) -> bool;
    pub fn append_content(&mut self, delta: &str);
    pub fn append_reasoning(&mut self, delta: &str);
    pub fn update_tool_progress(&mut self, name: &str, args: &str);
    pub fn set_cancelled(&mut self);
    pub fn flush_to_message(&mut self) -> Message;
    pub fn has_content(&self) -> bool;
}
```

---

## 5. 主事件循环

### 三路事件源

```
loop {
    terminal.draw(...)           // 1. 渲染帧

    let timeout = if app_state.mode == Mode::Streaming {
        Duration::from_millis(20)
    } else {
        Duration::from_millis(150)
    };

    // 2. 键盘事件
    if event::poll(timeout)? {
        let event = event::read()?;
        let input_event = InputMapper::from_crossterm(event);
        handle_input(&mut app_state, input_event, &mut agent_conn);
    }

    // 3. IPC 事件 (非阻塞)
    while let Some(frame) = agent_conn.try_recv() {
        if matches!(frame, AgentToTui::StreamPayload { .. }) {
            stream_buffer.apply(frame);
        }
        // 模式切换请求 → 更新 app_state.mode
        // 错误 → 设置 app_state.status_msg
    }

    // 快照 (每 3s 当 dirty)
    if app_state.dirty && app_state.last_snapshot.elapsed() > 3s {
        snapshot::save(&app_state, &stream_buffer);
    }
}
```

### 事件循环与现有代码对比

| 现有问题 | 改进方案 |
|----------|----------|
| `mod.rs` 单函数 ~320 行混合所有逻辑 | 拆分为 handle_event / process_ipc / snapshot 三个阶段 |
| `App` 直接持有 `tx: Sender<StreamEvent>` | IPC 抽象到 AgentConnection |
| 流事件处理分散在 loop 中 | 统一由 AgentConnection 分发 |
| 事件 + IPC + 渲染混在一个 loop body | 三阶段清晰分离 |

---

## 6. 斜杠命令

### 全部可直接在 TUI 处理（无需转发 Agent）

| 命令 | 处理方式 | 影响模块 |
|------|----------|----------|
| `/clear` | 重置 AppState.messages + StreamBuffer | E01 (自身) |
| `/balance` | 通过 AgentConnection 异步查询后更新 AppState.balance | E01 + E03 (状态栏) |
| `/fix` | 调用 AgentConnection.repair_messages() | E01 |
| `/context` | 读取 AppState.token_breakdown 显示 | E01 (纯展示) |
| `/tools` | 切换 AppState.tools_enabled | E01 |
| `/auto` | 切换 AppState.auto_mode + 持久化配置 | E01 |
| `/skip-all` | 切换 AppState.skip_all | E01 + E02 (工具确认跳过) |
| `/dev` | 切换 AppState.dev_mode + 日志等级 | E01 |
| `/compact` | 调用 AgentConnection.prepare_messages() 再设置 token_estimate | E01 |
| `/model`, `/effort`, `/lang`, `/reset`, `/reconfig`, `/profile` | 通过 AgentConnection.config 修改后更新状态栏 | E01 |

**结论：** 斜杠命令是纯本地的 TUI 管理操作，不需要任何 IPC 转发。未来若添加需要 Agent 端执行的命令（如 `/save` 强制保存），可扩展 `TuiToAgent::SlashCommand` 帧。

### 斜杠菜单数据流

```rust
// E01 提供
pub(crate) fn get_slash_commands(lang: &str) -> Vec<(String, String)> {
    config::all_commands(lang)
}

// E02 消费（在 chat.rs 中）
// 当用户输入 "/" 时，打开 slash_open = true
// 渲染时 E03 读取 AppState.slash_items 绘制菜单
```

---

## 7. 模式状态机

### 模式定义（不变，保持现有）

```rust
pub(crate) enum Mode {
    Chat,             // 默认输入模式
    Streaming,        // Agent 正在生成回复
    ToolConfirm,      // 等待用户确认工具调用
    DangerPrompt,     // 危险操作确认
    SudoPrompt,       // Sudo 密码输入
    ApiConfig,        // 首次设置 API Key
    SessionPicker,    // 选择恢复会话
    IntentPicker,     // 选择意图
}
```

### 状态转换

```
                    ┌─────────────────────────────┐
                    │         ApiConfig            │
                    │  (API key set → Chat)        │
                    └──────────┬──────────────────┘
                               │
                    ┌──────────▼──────────────────┐
                    │         SessionPicker         │
                    │  (select → Chat, Esc → exit) │
                    └──────────┬──────────────────┘
                               │
                    ┌──────────▼──────────────────┐
              ┌─────│           Chat               │─────┐
              │     │  (Enter → send_message)      │     │
              │     └──────────┬──────────────────┘     │
              │                │                          │
              │     ┌──────────▼──────────────────┐     │
              │     │         Streaming             │     │
              │     │  (Esc/Ctrl+C → cancel)       │     │
              │     └──┬──────┬──────────┬────────┘     │
              │        │      │          │                │
              │        ▼      ▼          ▼                │
              │  ToolConfirm DangerPrompt IntentPicker    │
              │  (Enter→Chat)(Enter→Chat) (Sel→Chat)     │
              │                │                          │
              │     ┌──────────▼──────────────────┐     │
              └─────│         SudoPrompt           │─────┘
                    │  (Enter→Chat, Esc→Chat)     │
                    └─────────────────────────────┘
```

### 转换逻辑

```rust
fn handle_mode_transition(app: &mut AppState, conn: &mut AgentConnection, new_mode: Mode) {
    match (app.mode, &new_mode) {
        (Mode::Chat, Mode::Streaming) => {
            // 启动流式渲染，缩短 poll timeout
        }
        (Mode::Streaming, Mode::ToolConfirm) | (Mode::Streaming, Mode::DangerPrompt) => {
            conn.send_tui_mode_request(new_mode);
        }
        (Mode::Streaming, Mode::Chat) => {
            // 流完成或取消
            app.stream_content.clear();
            app.sync_messages(conn);
        }
        (_, Mode::Chat) => {
            // 通用回归
            stream_buffer.clear();
        }
        _ => {}
    }
    app.mode = new_mode;
}
```

---

## 8. E02 / E03 接口定义

### 事件传递 (E01 → E02 输入层)

```rust
// E01 将 crossterm Event 归一化为 InputEvent 传递给 E02
pub(crate) enum InputEvent {
    Key(KeyEvent),
    Paste(String),
    Resize(u16, u16),
}

// E02 实现的 handler 签名统一为:
pub(crate) trait InputHandler {
    fn handle(
        app: &mut AppState,
        event: InputEvent,
        conn: &mut AgentConnection,
        buf: &mut StreamBuffer,
    );
}
```

### 渲染数据提取 (E03 消费)

```rust
// E01 提供渲染所需的只读视图
pub(crate) struct RenderContext<'a> {
    pub messages: &'a [Message],
    pub input: &'a str,
    pub input_cursor: usize,
    pub scroll_offset: usize,
    pub auto_scroll: bool,
    pub mode: &'a Mode,
    pub status_msg: &'a str,
    pub stream: &'a StreamBuffer,       // 直接引用流缓冲
    pub balance: Option<&'a str>,
    pub token_estimate: u32,
    pub token_breakdown: Option<&'a TokenBreakdown>,
    pub config: &'a Config,
    // ... slash menu, tool_code, intent, sudo, session 等字段
}
```

### 模块边界总图

```
src/dsx-tui/
├── main.rs              // 入口：解析 CLI，调用 run()
├── mod.rs               // run() + 主事件循环（整合三路事件）
├── app_state.rs         // [E01] UI 状态
├── agent_conn.rs        // [E01] IPC 管理
├── stream_buffer.rs     // [E01] 流式缓冲
├── mode.rs              // [E01] 模式枚举
├── commands.rs          // [E01] 斜杠命令（消费 AppState + AgentConnection）
├── input/               // [E02] 键盘输入处理
│   ├── mod.rs
│   ├── chat.rs
│   ├── confirm.rs
│   ├── sudo.rs
│   └── picker.rs
├── render/              // [E03] 渲染引擎
│   ├── mod.rs
│   ├── chat.rs
│   ├── input.rs
│   ├── popups.rs
│   ├── tool_code.rs
│   ├── workspace.rs
│   └── status.rs
├── utils.rs             // [共享] 纯工具函数
├── status.rs            // [共享，未来可合并到 render/status]
└── tui_text.rs          // [共享] 文本常量
```
