# D01 设计文档：dsx-tools IPC 服务框架与系统工具

> 日期: 2026-05-14 | 所属: Team D (dsx-tools) | 角色: D01 工具框架+系统工具

---

## 1. 现状分析

现有 `src/dsx-tools/` 各文件已被阅读，核心观察：

| 文件 | 问题 |
|------|------|
| `mod.rs` — ToolManager | 同步注册表，函数指针 `fn(&str)->String`，无法跨进程；超时基于 `mpsc::recv_timeout`，粒度粗 |
| `exec.rs` — 命令执行 | 内嵌安全检测（`is_danger_command`）；同步+异步共存；PTY 需特殊处理 |
| `explore.rs` — 扫描 | 依赖 `CANCEL` 全局变量，输出有 30KB cap |
| `git.rs` — Git 操作 | 4 个独立函数，各有独立 ToolDef |
| `web.rs` — 网络 | 需 `reqwest` 和 `html2text` 依赖，有 API key 硬编码 |
| `file.rs` — 文件操作 | 6 个 action 聚合在 `exec_file` 里，D02 负责 |
| `agent.rs` — 大代理分发器 | 21 个 action 全部通过 `match action` 分发，D03 负责拆解 |

**核心结论**：当前架构是单进程同步模型，TUI 直接调用 `tools::execute_tool()`。目标是将 dsx-tools 改造为独立 IPC 服务进程，通过 JSON-LP 协议与 dsx-agent 或 dsx-tui 通信。

---

## 2. IPC 服务架构

### 2.1 进程模型

```
┌────────────────┐     JSON-LP (stdin/stdout)      ┌─────────────────────┐
│  dsx-agent     │  ◄══════════════════════════►   │  dsx-tools          │
│  (or dsx-tui)  │                                 │  (IPC Server)       │
│                │  ToolsInit / ToolCallReq /      │  ┌─────────────────┐│
│                │  ToolCancel / ToolsShutdown      │  │ ToolManager      ││
│                │  ◄───                           │  │ ├── router       ││
│                │  ToolsReady / ToolProgress /     │  │ ├── safety       ││
│                │  ToolResult / ToolError          │  │ ├── exec (D01)   ││
│                │  / ToolConfirmReq                │  │ ├── git (D01)    ││
│                │                                  │  │ ├── web (D01)    ││
└────────────────┘                                  │  │ ├── file (D02)   ││
                                                    │  │ └── agent (D03)  ││
                                                    │  └─────────────────┘│
                                                    └─────────────────────┘
```

### 2.2 JSON-LP 协议帧

所有帧以 `\n` 分隔，每行一个完整 JSON 对象。

**Agent → Tools:**

```json
{"type":"tools_init","allowed_tools":["exec","file","web"],"session_seed":"abc123","auto_mode":false}
{"type":"tool_call_req","id":"call_001","name":"exec","action":"run","args":{"command":"ls -la"},"timeout_secs":null}
{"type":"tool_cancel","id":"call_001"}
{"type":"tools_shutdown"}
```

**Tools → Agent:**

```json
{"type":"tools_ready","tools":[{"name":"exec","description":"...","input_schema":{...}},...]}
{"type":"tool_progress","id":"call_001","content":"stdout line 1\n","stream_type":"stdout"}
{"type":"tool_result","id":"call_001","success":true,"content":"[OK] ..."}
{"type":"tool_error","id":"call_001","error":"Timed out after 30s","code":"TIMEOUT"}
{"type":"tool_confirm_req","id":"call_001","tool_name":"exec","action":"run","danger_level":"danger","prompt":"Run sudo rm -rf /?"}
```

### 2.3 Rust 类型定义

```rust
// ── 入站帧（Agent → Tools）──

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum InboundFrame {
    #[serde(rename = "tools_init")]
    Init {
        allowed_tools: Vec<String>,
        session_seed: String,
        auto_mode: bool,
    },
    #[serde(rename = "tool_call_req")]
    CallReq {
        id: String,
        name: String,
        action: String,
        args: serde_json::Value,
        timeout_secs: Option<u64>,
    },
    #[serde(rename = "tool_cancel")]
    Cancel {
        id: Option<String>,  // None = cancel all
    },
    #[serde(rename = "tools_shutdown")]
    Shutdown,
}

// ── 出站帧（Tools → Agent）──

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
pub enum OutboundFrame {
    #[serde(rename = "tools_ready")]
    Ready { tools: Vec<ToolDef> },
    #[serde(rename = "tool_progress")]
    Progress { id: String, content: String, stream_type: String },
    #[serde(rename = "tool_result")]
    Result { id: String, success: bool, content: String },
    #[serde(rename = "tool_error")]
    Error { id: String, error: String, code: String },
    #[serde(rename = "tool_confirm_req")]
    ConfirmReq { id: String, tool_name: String, action: String, danger_level: String, prompt: String },
}
```

### 2.4 IPC 传输层 (`ipc.rs`)

```rust
/// 从 stdin 读取下一个入站帧（行缓冲）
pub fn read_frame(reader: &mut BufRead) -> Result<Option<InboundFrame>>;

/// 向 stdout 写出站帧（自动追加 \n 并 flush）
pub fn write_frame(frame: &OutboundFrame) -> Result<()>;

/// IPC 主循环：阻塞读取 stdin → 路由到 ToolManager → 写结果到 stdout
pub fn ipc_main_loop(manager: &mut ToolManager) -> ! {
    let stdin = io::stdin().lock();
    let mut reader = io::BufReader::new(stdin);
    
    loop {
        match read_frame(&mut reader) {
            Ok(Some(InboundFrame::Init { allowed_tools, session_seed, auto_mode })) => {
                manager.apply_init(allowed_tools, session_seed, auto_mode);
                write_frame(&OutboundFrame::Ready {
                    tools: manager.filtered_defs(),
                });
            }
            Ok(Some(InboundFrame::CallReq { id, name, action, args, timeout_secs })) => {
                let result = manager.handle_request_sync(&id, &name, &action, &args, timeout_secs);
                write_frame(&result);
            }
            Ok(Some(InboundFrame::Cancel { id })) => {
                manager.handle_cancel(id.as_deref());
            }
            Ok(Some(InboundFrame::Shutdown)) | Ok(None) => break,
            Err(e) => {
                write_frame(&OutboundFrame::Error {
                    id: "ipc".into(), error: e.to_string(), code: "IPC_ERROR".into(),
                });
            }
        }
    }
}
```

注意：IPC 主循环使用 `std::thread::spawn` 将每个 `CallReq` 的执行推入后台线程，并在单独的 tokio runtime 中执行异步 handler。主循环只做帧读写，不阻塞。设计上保持简单——**每条帧的长度约为 1KB 以内**，JSON-LP 无需长度前缀，从 stdin 直接按行读取即可。

---

## 3. ToolManager 改造

### 3.1 核心结构

```rust
// ── 工具键（名称 + 二级操作）──
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ToolKey {
    pub name: &'static str,
    pub action: &'static str,
}

impl ToolKey {
    pub fn new(name: &'static str, action: &'static str) -> Self {
        Self { name, action }
    }
}

// ── 安全检查结果 ──
pub enum SafetyVerdict {
    Allow,
    Confirm(String),
    Block(String),
}

// ── 工具句柄 ──
pub struct ToolHandler {
    pub key: ToolKey,
    pub description: &'static str,
    pub input_schema: serde_json::Value,
    pub handler: fn(ToolCallCtx) -> ToolResult,
    pub safety: fn(&ToolCallCtx) -> SafetyVerdict,
    pub default_timeout: Duration,
}

// ── handler 上下文 ──
pub struct ToolCallCtx {
    pub id: String,
    pub name: String,
    pub action: String,
    pub args: serde_json::Value,
    pub tx_progress: Option<crossbeam_channel::Sender<String>>,
}

// ── handler 返回值 ──
pub struct ToolResult {
    pub success: bool,
    pub content: String,
}

// ── ToolManager ──
pub struct ToolManager {
    handlers: HashMap<ToolKey, ToolHandler>,
    allowed: Option<HashSet<String>>,  // 从 ToolsInit 来
    cancel_flag: Arc<AtomicBool>,
    inflight_tasks: HashMap<String, Arc<AtomicBool>>,
    runtime: tokio::runtime::Runtime,
}
```

### 3.2 Handler 改造方式

存量 handler 目前签名是 `fn(args: &str) -> String`。改造为：

```rust
// 旧签名
pub fn exec_command(args: &str) -> String { ... }

// 新签名
pub fn handle_exec_run(ctx: ToolCallCtx) -> ToolResult {
    let command = ctx.args.get("command").and_then(|v| v.as_str()).unwrap_or("");
    // ... 实际逻辑不变，只改 IO 方式：
    // - 输出用 format! 构建 String 而非 println!
    // - 进度用 ctx.tx_progress.send(...) 推送
    ToolResult { success: true, content: result_string }
}
```

对 `exec.rs`、`explore.rs`、`git.rs`、`web.rs` 的修改模式：

1. 将 `fn exec_xxx(args: &str) -> String` → `fn handle_xxx(ctx: ToolCallCtx) -> ToolResult`
2. 内部 `parse_arg` 改为 `ctx.args.get("key")`（JSON Value 直接访问，避免二次反序列化）
3. 流式输出通过 `tx_progress` 推送
4. `ToolResult` 的 `content` 格式保持向后兼容（`[OK]`、`[ERROR]` 前缀不变）

### 3.3 请求处理流程

```
ToolCallReq 到达 ipc_main_loop
  │
  ├── 1. 检查 ToolsInit 授权（若 name 不在 allowed 中 → 返回 Error { code: "FORBIDDEN" }）
  ├── 2. 构建 ToolKey(name, action)
  ├── 3. handlers.get(&key)
  │    └── 不存在 → 返回 Error { code: "UNKNOWN_TOOL", error: "..." }
  ├── 4. 构建 ToolCallCtx（含 progress channel）
  ├── 5. 检查 safety(&ctx):
  │    ├── Block → 返回 Error { code: "BLOCKED", error: "...", id: ctx.id }
  │    ├── Confirm → 发送 ConfirmReq 到 stdout，等待 Agent 回复（同步阻塞或用异步回调）
  │    └── Allow → 继续
  ├── 6. spawn 后台线程执行 handler
  │    ├── 记录到 inflight_tasks
  │    ├── tokio::runtime 内执行
  │    ├── panic::catch_unwind 包裹
  │    └── 超时控制（tokio::time::timeout）
  ├── 7. 返回 ToolResult 或 ToolError
  └── 8. 从 inflight_tasks 移除
```

### 3.4 Confirm 交互（安全弹窗）

当安全检查返回 `Confirm` 或 `Block` 时，Tools 进程不能自行决定——需要通过 IPC 通知 Agent，由 Agent/TUI 展示给用户。

```
Tools → Agent: {"type":"tool_confirm_req","id":"call_001","tool_name":"exec","action":"run","danger_level":"danger","prompt":"Run sudo rm -rf /?"}
Agent → Tools: {"type":"tool_confirm_resp","id":"call_001","approved":false}
```

`tool_confirm_resp` 是新增入站帧：

```rust
#[serde(rename = "tool_confirm_resp")]
ConfirmResp { id: String, approved: bool },
```

当前实现简单化：当 `Confirm` 时，Tools 进程将执行 `tool_confirm_req` 写入 stdout 帧后，**阻塞等待**来自 stdin 的 `tool_confirm_resp`。这种同步阻塞对于单线程 IPC 足够——IO 是独立线程。

---

## 4. 系统工具设计（Explore + Exec + Git + Web）

### 4.0 Explore — 项目结构探索（独立顶级工具）

**USER 裁定**：explore 与 edit tools / web tools **同等级**，通过 ToolManager 统一注册，不是 exec 的子动作，也不是 file 的附属。

| Key | Action | handler | 安全 | 默认超时 | 负责人 |
|------|--------|---------|------|----------|--------|
| `explore/scan` | `scan` | `handle_explore_scan` | Allow | 30s | D01 |

`handle_explore_scan` 封装原有 `exec_explore` 逻辑（`src/dsx-tools/explore.rs`），保持不变：
- 递归目录遍历 + 文件签名提取 + 模块图推导（Rust/Go）
- 30KB cap、500KB 文件跳过、`target`/`node_modules` 隐藏目录跳过
- `CANCEL` 全局变量支持长扫描中断

**input_schema**（保持向后兼容）：
```json
{
  "type": "object",
  "properties": {
    "path": {"type": "string", "description": "Directory to scan (default: .)"}
  },
  "additionalProperties": false
}
```

### 4.1 Exec — 命令执行

| Key | Action | handler | 安全 | 默认超时 |
|------|--------|---------|------|----------|
| `exec/run` | `run` | `handle_exec_run` | 动态分类 | 300s |
| `exec/sudo_run` | `sudo_run` | `handle_exec_sudo_run` | Block（需用户密码） | 300s |
| `exec/pty` | `pty` | `handle_exec_pty` | Confirm | 300s |

`handle_exec_run` 封装原有 `exec_command` 逻辑，但：
- 输出截断逻辑不变（160 行 head+tail；error/warning 摘要）
- 超时通过 tokio runtime 的 `timeout` 控制
- `CANCEL` 标志在长命令中检查

`handle_exec_sudo_run` 封装原有 `exec_with_sudo`：
- 密码通过 IPC 的 Confirm 流获取（初次弹窗、可缓存于 session 内）
- 如果已经缓存密码则自动使用

`handle_exec_pty` 封装 `spawn_exec_pty`：
- 输出通过 `tx_progress` 流式推送（`ToolProgress` 帧）

### 4.2 Git — Git 操作

| Key | Action | handler | 安全 | 默认超时 |
|------|--------|---------|------|----------|
| `git/status` | `status` | `handle_git_status` | Allow | 10s |
| `git/diff` | `diff` | `handle_git_diff` | Allow | 10s |
| `git/log` | `log` | `handle_git_log` | Allow | 10s |
| `git/commit` | `commit` | `handle_git_commit` | Confirm | 10s |

所有 handler 封装原有 `src/dsx-tools/git.rs` 中的 `run_cmd` 逻辑，参数从 `ctx.args` 中读取。

### 4.3 Web — 网络操作

| Key | Action | handler | 安全 | 默认超时 |
|------|--------|---------|------|----------|
| `web/fetch` | `fetch` | `handle_web_fetch` | Confirm | 30s |
| `web/search` | `search` | `handle_web_search` | Allow | 15s |
| `web/context7_resolve` | `context7_resolve` | `handle_c7_resolve` | Allow | 15s |
| `web/context7_query` | `context7_query` | `handle_c7_query` | Allow | 15s |

注意：
- `web/fetch` 的 `output` 参数功能保留（写文件）
- Context7 API key 应移至配置文件/环境变量（`CONTEXT7_API_KEY`），当前硬编码需移除
- 搜索目前使用 cn.bing.com，未来可改为可配置的 search provider

### 4.4 Explore — 独立顶级工具

已由第 4.0 节定义。explore 保持 `src/dsx-tools/explore.rs` 为独立文件，通过 `explore/scan` 入口注册到 ToolManager。

**为何不是 exec 的子动作**：USER 裁定 explore 与 edit tools / web tools **同等级**。explore 要求 AI 在编辑文件前先获取全局视野——这不是一个执行性操作，而是一个独立的代码认知工具。

---

## 5. 安全检测中心化

### 5.1 统一 `safety.rs`

```rust
// src/dsx-tools/safety.rs

/// 全局工具安全检查（所有 handler 执行前调用）
pub fn check(name: &str, action: &str, args: &Value) -> SafetyVerdict {
    match (name, action) {
        // Explore (independent top-level tool)
        ("explore", _) => SafetyVerdict::Allow,

        // Exec
        ("exec", "run") => classify_execution(args),
        ("exec", "sudo_run") => SafetyVerdict::Block("sudo requires password prompt"),
        ("exec", "pty") => SafetyVerdict::Confirm("PTY exec may be interactive"),
        
        // Git
        ("git", "commit") => SafetyVerdict::Confirm("Create a git commit?"),
        ("git", _) => SafetyVerdict::Allow,  // status, diff, log are safe
        
        // Web
        ("web", "fetch") => SafetyVerdict::Confirm("Fetch external URL?"),
        ("web", _) => SafetyVerdict::Allow,  // search, context7_* are safe
        
        // File (D02 provides these, central safety here is for the overall flow)
        ("file", "read" | "list" | "search") => SafetyVerdict::Allow,
        ("file", "write" | "edit" | "diff") => SafetyVerdict::Confirm("Modify file?"),
        
        // Agent (D03)
        ("agent", _) => SafetyVerdict::Allow,
        
        _ => SafetyVerdict::Confirm("Allow?"),
    }
}

/// exec/run 的动态安全检查（原有 is_danger_command / is_safe_command 逻辑移入）
pub fn classify_execution(args: &Value) -> SafetyVerdict {
    let command = args.get("command")
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_lowercase();
    
    if is_danger_command(&command) {
        SafetyVerdict::Block(format!("Potentially destructive command: {}", command))
    } else if is_safe_command(&command) {
        SafetyVerdict::Allow
    } else {
        SafetyVerdict::Confirm(format!("Run: {}?", command))
    }
}
```

### 5.2 危险命令列表（从 exec.rs 迁移，不变）

`is_danger_command` 和 `is_safe_command` 函数保持逻辑完全不变，仅从 `exec.rs` 移动到 `safety.rs`。

---

## 6. 超时控制、Panic 捕获、全局取消

### 6.1 超时控制

```rust
pub fn execute_with_timeout(handler: &ToolHandler, ctx: ToolCallCtx) -> ToolResult {
    let timeout = match ctx.args.get("timeout_secs").and_then(|v| v.as_u64()) {
        Some(secs) if secs > 0 && secs <= 3600 => Duration::from_secs(secs),
        _ => handler.default_timeout,
    };
    
    // 使用 tokio runtime 执行异步 handler
    let rt = &manager.runtime;
    let result = rt.block_on(async {
        tokio::time::timeout(timeout, async {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                (handler.handler)(ctx)
            }))
            .unwrap_or_else(|panic| ToolResult {
                success: false,
                content: format!("[PANIC] Tool panicked: {:?}", panic),
            })
        }).await
    });
    
    match result {
        Ok(tr) => tr,
        Err(_) => ToolResult {
            success: false,
            content: format!("[TIMEOUT] Tool timed out after {:?}", timeout),
        },
    }
}
```

### 6.2 Panic 捕获

在 `panic::catch_unwind` 的基础上，额外捕获 panic 信息并格式化为 `[PANIC]` 前缀的 ToolResult（与现有 `mod.rs` 中 `Ok(Err(panic_info))` 处理一致）。

重要：panic 的 **detail message** 需要提取。现有代码已正确处理：

```rust
let msg = if let Some(s) = panic_info.downcast_ref::<String>() { s.clone() }
    else if let Some(s) = panic_info.downcast_ref::<&str>() { s.to_string() }
    else { "unknown panic".to_string() };
```

### 6.3 全局取消标志

```rust
pub static CANCEL: AtomicBool = AtomicBool::new(false);

// 取消所有
pub fn cancel_all(manager: &ToolManager) {
    CANCEL.store(true, Ordering::SeqCst);
    for flag in manager.inflight_tasks.values() {
        flag.store(true, Ordering::SeqCst);
    }
}

// 取消单个
pub fn cancel_one(manager: &ToolManager, id: &str) {
    if let Some(flag) = manager.inflight_tasks.get(id) {
        flag.store(true, Ordering::SeqCst);
    }
}
```

各 handler 在执行循环中检查 `CANCEL` 和各自 `inflight` 标志：

```rust
// explore.rs / exec.rs 中的长循环检查示例
if CANCEL.load(Ordering::Relaxed) || self_cancel.load(Ordering::Relaxed) {
    output.push_str("[CANCELLED] Interrupted by user\n");
    break;
}
```

---

## 7. D02/D03 注册接口

### 7.1 接口定义

```rust
// ── D01 提供：注册入口 ──

/// 由 main 函数或 tests 调用，构建完整 ToolManager
pub fn build_tool_manager() -> ToolManager {
    let mut tm = ToolManager::new();
    
    // D01 工具
    tm.register_explore();
    tm.register_exec();
    tm.register_git();
    tm.register_web();
    
    // D02 工具（文件/元数据）
    register_d02_tools(&mut tm);
    
    // D03 工具（任务/计划/记忆）
    register_d03_tools(&mut tm);
    
    tm
}
```

在 `registration.rs` 中：

```rust
// src/dsx-tools/registration.rs

/// D02 实现：注册 file 相关 handler
///
/// D02 需要在 file.rs 中提供以下函数：
///   pub fn register(manager: &mut ToolManager)
pub fn register_d02_tools(manager: &mut ToolManager) {
    crate::file::register(manager);
}

/// D03 实现：注册 agent（任务/计划/记忆）handler
///
/// D03 需要在 plan.rs, task.rs, memory.rs 中提供以下函数：
///   pub fn register_plan(manager: &mut ToolManager)
///   pub fn register_task(manager: &mut ToolManager)
///   pub fn register_memory(manager: &mut ToolManager)
pub fn register_d03_tools(manager: &mut ToolManager) {
    crate::plan::register_plan(manager);
    crate::task::register_task(manager);
    crate::memory::register_memory(manager);
    // 保留 agent.rs 中的 ask 和 status 等
    crate::agent::register_agent(manager);
}
```

### 7.2 D02 实现模式

D02 在 `src/dsx-tools/file.rs` 中按以下模式提供：

```rust
// file.rs — D02 负责

/// 被 registration.rs 调用
pub fn register(manager: &mut ToolManager) {
    for handler in all_handlers() {
        manager.register(handler);
    }
}

fn all_handlers() -> Vec<ToolHandler> {
    vec![
        ToolHandler {
            key: ToolKey::new("file", "read"),
            description: "Read a file with optional line range",
            input_schema: read_schema(),
            handler: |ctx| handle_read(ctx),
            safety: |ctx| SafetyVerdict::Allow,
            default_timeout: Duration::from_secs(15),
        },
        ToolHandler {
            key: ToolKey::new("file", "write"),
            description: "Create or overwrite a file",
            input_schema: write_schema(),
            handler: |ctx| handle_write(ctx),
            safety: |ctx| classify_write(&ctx.args),
            default_timeout: Duration::from_secs(15),
        },
        // ... edit, diff, list, search
    ]
}
```

### 7.3 D03 实现模式

D03 在各自模块中注册：

```rust
// plan.rs
pub fn register_plan(manager: &mut ToolManager) {
    manager.register(ToolHandler {
        key: ToolKey::new("agent", "plan_create"),
        description: "Create a new plan",
        input_schema: plan_create_schema(),
        handler: |ctx| handle_plan_create(ctx),
        safety: |_| SafetyVerdict::Allow,
        default_timeout: Duration::from_secs(5),
    });
    // ... plan_update, plan_read, plan_list
}

// task.rs
pub fn register_task(manager: &mut ToolManager) {
    manager.register(ToolHandler {
        key: ToolKey::new("agent", "task_create"),
        // ...
    });
    // ... task_update, task_list
}

// memory.rs
pub fn register_memory(manager: &mut ToolManager) {
    manager.register(ToolHandler {
        key: ToolKey::new("agent", "mem_save"),
        // ...
    });
    // ... mem_read, mem_forget, recall
}
```

### 7.4 D02/D03 不需要关心的细节

- JSON-LP 帧的解包打包
- stdin/stdout 的读写
- IPC 主循环和帧路由
- 超时控制框架
- Panic 捕获
- ToolsInit/Shutdown 生命周期
- 取消信号传播

D02/D03 只需要关心：

1. 实现 handler 函数：`fn(ToolCallCtx) -> ToolResult`
2. 实现 safety classifier：`fn(&ToolCallCtx) -> SafetyVerdict`
3. 提供 input_schema JSON
4. 在模块的 `pub fn register(manager: &mut ToolManager)` 中调用 `manager.register()`

---

## 8. 向后兼容策略

### 8.1 兼容垫片

第一阶段保留 `pub fn execute_tool(name: &str, args: &str) -> String` 作为向后兼容入口，实现方式：

```rust
/// 向后兼容垫片：从 ToolHandler 表查找并执行（同步阻塞）
pub fn execute_tool(name: &str, action: &str, args: &str) -> String {
    let ctx = ToolCallCtx::from_str(name, action, args);
    let key = ToolKey { name, action };
    let handler = match HANDLERS.lock().get(&key) {
        Some(h) => h.clone(),
        None => return format!("[ERROR] Unknown tool: {}/{}", name, action),
    };
    
    match (handler.safety)(&ctx.args) {
        SafetyVerdict::Block(reason) => {
            return format!("[ERROR] Blocked: {}", reason);
        }
        SafetyVerdict::Confirm(prompt) => {
            // 在兼容模式下自动允许（原行为由 TUI 侧 phase_check 控制）
        }
        SafetyVerdict::Allow => {}
    }
    
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        (handler.handler)(ctx)
    }));
    
    match result {
        Ok(tr) => tr.content,
        Err(panic) => format!("[ERROR] Tool panicked: {:?}", panic),
    }
}
```

### 8.2 平滑迁移路线

1. **Phase 1** — 改造 ToolManager 结构，引入 ToolKey/ToolHandler，但保持 `execute_tool` 兼容入口。D02/D03 迁移到新注册接口。
2. **Phase 2** — 添加 `ipc.rs` 和 IPC 主循环，使 dsx-tools 可作为独立进程运行。
3. **Phase 3** — TUI/Agent 侧改为 spawn 子进程方式调用 tools，移除兼容垫片。
4. **Phase 4** — 清理旧代码（`ToolEntry`、`agent.rs` 分发器等）。

---

## 9. 文件变更清单

| 文件 | 变更 | 说明 |
|------|------|------|
| `src/dsx-tools/mod.rs` | 重写 | 用 ToolManager/ToolHandler/ToolKey 替换 ToolEntry；保留 `execute_tool` 垫片 |
| `src/dsx-tools/ipc.rs` | 新增 | JSON-LP 帧定义 + 读写 + 主循环 |
| `src/dsx-tools/safety.rs` | 新增 | 集中安全检查（从 exec.rs 移入） |
| `src/dsx-tools/registration.rs` | 新增 | D02/D03 注册入口 |
| `src/dsx-tools/exec.rs` | 改造 | `exec_command` → `handle_exec_run`；安全函数移到 safety.rs |
| `src/dsx-tools/explore.rs` | 改造 | `exec_explore` → `handle_explore_scan`；独立顶级工具入口 `explore/scan`，不再是 exec 子动作；CANCEL 引用逻辑不变 |
| `src/dsx-tools/git.rs` | 改造 | 各 `exec_git_*` → `handle_git_*`；丢弃 `*_def()` 函数（由 schema 替代） |
| `src/dsx-tools/web.rs` | 改造 | `exec_web_fetch` → `handle_web_fetch` 等；API key 移至环境变量 |
| `src/dsx-tools/file.rs` | 改造（D02） | 各 `exec_file_*` → `handle_file_*` |
| `src/dsx-tools/agent.rs` | 移除 | 分发器被拆散为独立 handler（D03 负责） |

---

## 10. 风险与权衡

| 风险 | 影响 | 缓解 |
|------|------|------|
| IPC 序列化开销 | 每帧 1KB 以内，现代 CPU 下 <0.1ms | 可接受，LSP 已验证此模式 |
| 单线程主循环阻塞 | Confirm 等待时无法处理其他请求 | 近期不频繁；长远可升级为双线程 |
| 存量 handler 改造量大 | exec.rs(606行) 等需要重写签名 | 修改模式固定：parse_arg → ctx.args.get |
| CANCEL 多线程竞态 | AtomicBool 有弱保证 | 已有 `Ordering::Relaxed` 模式，不追求强一致 |
| Context7 API key 硬编码在 web.rs | 安全风险 | 改为 `std::env::var("CONTEXT7_API_KEY")` |
