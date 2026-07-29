# Debug Report: Frontend Send → Backend Loop Trace

> **Bug 现象**（用户报告）
> 1. 发送单条消息后，后端无任何响应，不进入 loop，不调用 API
> 2. 连续发送 8-9 条消息后，后端开始响应，进入 loop 并处理所有消息到 API
> 3. 类似现象出现在工具调用结果：需要连续 9 次工具调用才能正常命中 loop 并流转
>
> ---

## 一、Git 改动摘要

### 修改的文件

| 文件 | 改动 |
|------|------|
| `crates/deepx-runtime/Cargo.toml` | 添加 chrono 依赖 |
| `crates/deepx-runtime/src/logger.rs` | 日志增加时间戳 |
| `crates/deepx-msglp/src/ring/loop_core.rs` | run()/dispatch_one() 增加 trace 日志 |
| `crates/deepx-msglp/src/ring/engine_input.rs` | handle_user_input() 增加 trace 日志 |
| `crates/deepx-msglp/src/ring/engine_turn.rs` | run_lap() 增加 trace 日志 |

### 新增文件

| 文件 | 用途 |
|------|------|
| `watch-agent.ps1` | PowerShell CLI 实时监听 agent.log，彩色输出 |

---

## 二、完整 Diff

<details><summary>点击展开</summary>

```diff
diff --git a/Cargo.lock b/Cargo.lock
index cee86c3..68fcc07 100644
--- a/Cargo.lock
+++ b/Cargo.lock
@@ -1769,6 +1769,7 @@ dependencies = [
 name = "deepx-runtime"
 version = "1.0.0-beta.3"
 dependencies = [
+ "chrono",
  "deepx-config",
  "deepx-msglp",
  "deepx-proto",
diff --git a/crates/deepx-msglp/src/ring/engine_input.rs b/crates/deepx-msglp/src/ring/engine_input.rs
index c59c8de..514c373 100644
--- a/crates/deepx-msglp/src/ring/engine_input.rs
+++ b/crates/deepx-msglp/src/ring/engine_input.rs
@@ -17,6 +17,7 @@ impl InputEngine {
     /// Handle user input. Returns an Outcome telling the Loop whether
     /// to start a turn, yield, or report an error.
     pub fn handle_user_input(&self, ctx: &mut RingContext, text: &str) -> Outcome {
+        log::info!("[INPUT] handle_user_input called, text_len={}", text.len());
         // Auto-create session on first input
         if ctx.agent.session.seed.is_empty() {
             log::info!("[INPUT] auto-creating session on first user input");
@@ -94,10 +95,13 @@ impl InputEngine {
             ctx.emitter.emit(Agent2Ui::SkillsChanged { status });
         }

+        log::info!("[INPUT] pushing user message to store");
         ctx.agent.msg.push_user(&text);
+        log::info!("[INPUT] flushing meta");
         ctx.agent.msg.flush_meta(&ctx.agent.config.model, &ctx.agent.config.reasoning_effort);

         let turn_id = format!("t{}", ctx.agent.msg.turn_count());
+        log::info!("[INPUT] emitting TurnStart turn_id={} round_num=0", turn_id);
         ctx.emitter.emit(Agent2Ui::TurnStart { turn_id: turn_id.clone(), user_text: text });

         Outcome::ContinueTurn { turn_id, round_num: 0, usage: None }
diff --git a/crates/deepx-msglp/src/ring/engine_turn.rs b/crates/deepx-msglp/src/ring/engine_turn.rs
index faf411e..2165f03 100644
--- a/crates/deepx-msglp/src/ring/engine_turn.rs
+++ b/crates/deepx-msglp/src/ring/engine_turn.rs
@@ -692,6 +692,7 @@ impl TurnEngine {
         round_num: u32,
         mut last_usage: Option<UsageInfo>,
     ) -> Outcome {
+        log::info!("[TURN] run_lap turn_id={} round_num={}", turn_id, round_num);
         // Rebuild provider from current config
         let ep = deepx_config::registry::find_endpoint(
             &ctx.agent.config.provider_id,
@@ -792,6 +793,7 @@ impl TurnEngine {
             let cancel_arc = ctx.cancel.arc();

             // ── SSE Gate Request ──
+            log::info!("[TURN] run_lap turn_id={} round_num={} calling chat_stream", turn_id, round_num);
             let result = deepx_gate::chat_stream(
                 &provider,
                 messages,
@@ -920,12 +922,15 @@ impl TurnEngine {
             }

             if had_error || result.is_err() {
+                log::info!("[TURN] run_lap turn_id={} round_num={} gate error or had_error={}", turn_id, round_num, had_error);
                 ctx.agent
                     .msg
                     .flush_meta(&ctx.agent.config.model, &ctx.agent.config.reasoning_effort);
                 return Outcome::Handled;
             }

+            log::info!("[TURN] run_lap turn_id={} round_num={} gate succeeded, parsing response", turn_id, round_num);
+
             // ── Parse + push assistant message ──
             let parsed = util::parse_tool_calls_from_response(
                 &content,
@@ -985,6 +990,7 @@ impl TurnEngine {
                             .iter()
                             .map(|call| call.id.clone())
                             .collect::<Vec<_>>();
+                        log::info!("[TURN] run_lap turn_id={} round_num={} admit_batch {} pending tools", turn_id, round_num, pending.len());
                         const MAX_PARALLEL_TOOL_WORKERS: usize = 4;
                         let (_serial_groups, serial_after) =
                             conflict::resolve_write_conflicts(&pending);
diff --git a/crates/deepx-msglp/src/ring/loop_core.rs b/crates/deepx-msglp/src/ring/loop_core.rs
index e9ab117..bdabb38 100644
--- a/crates/deepx-msglp/src/ring/loop_core.rs
+++ b/crates/deepx-msglp/src/ring/loop_core.rs
@@ -475,7 +475,7 @@ impl Loop {
             // ── Block for next command (with timeout to poll compact) ──
             let frame = match self.cmd_rx.recv_timeout(std::time::Duration::from_secs(1)) {
                 Ok(f) => {
-                    log::info!("[AGENT] received Ui2Agent frame");
+                    log::info!("[AGENT] received Ui2Agent frame: {:?}", f);
                     f
                 }
                 Err(mpsc::RecvTimeoutError::Timeout) => continue,
@@ -721,6 +721,7 @@ impl Loop {
     /// 3. **Fallback**: commands needing direct event_tx access (Undo, SetMode,
     ///    LoadMoreTurns, Cancel, Shutdown)
     fn dispatch_one(&mut self, frame: Ui2Agent) {
+        log::info!("[AGENT] dispatch_one: frame={:?}", frame);
         // Any inbound command ends the idle period; the next time the loop
         // returns to idle it will re-emit Ready exactly once.
         self.ready_emitted = false;
diff --git a/crates/deepx-runtime/Cargo.toml b/crates/deepx-runtime/Cargo.toml
index ce26571..eeecbcd 100644
--- a/crates/deepx-runtime/Cargo.toml
+++ b/crates/deepx-runtime/Cargo.toml
@@ -12,6 +12,7 @@ deepx-session = { path = "../deepx-session" }
 deepx-tools = { path = "../deepx-tools" }
 deepx-types = { path = "../deepx-types" }
 log = "0.4"
+chrono = { version = "0.4", features = ["clock"] }
 serde = { version = "1", features = ["derive"] }
 serde_json = "1"
 tokio = { version = "1", features = ["sync", "time"] }
diff --git a/crates/deepx-runtime/src/logger.rs b/crates/deepx-runtime/src/logger.rs
index 78a4730..4a83a29 100644
--- a/crates/deepx-runtime/src/logger.rs
+++ b/crates/deepx-runtime/src/logger.rs
@@ -1,3 +1,4 @@
+use chrono::Local;
 use log::{LevelFilter, Log, Metadata, Record};
 use std::path::Path;

@@ -12,13 +13,14 @@ impl Log for FileLogSink {
             let level = record.level();
             let target = record.target();
             let msg = record.args();
+            let ts = Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
             if let Ok(mut file) = std::fs::OpenOptions::new()
                 .create(true)
                 .append(true)
                 .open(&self.path)
             {
                 use std::io::Write;
-                let _ = writeln!(file, "[{level:5}] {target} | {msg}");
+                let _ = writeln!(file, "[{ts}] [{level:5}] {target} | {msg}");
             }
         }
     }
```

</details>

---

## 三、Send → Loop 完整调用链

### 端到端流程

```
前端 WebSocket
    │
    ▼
daemon handle_connection (server.rs)
    │  select! 循环接收 ControlClientMessage::Request
    │  检查 session_scoped + 租约 owns()
    │  放入 request_tx → request_worker.spawn_blocking
    │
    ▼
service.rs: session_scoped → session.send_message
    │  registry.send(seed, Ui2Agent::UserInput { text })
    │
    ▼
registry.rs: AgentRegistry::send()
    │  get_or_spawn(seed) → 若 agent 未运行则 spawn
    │  writeln!(stdin, json) + flush()  ← 阻塞点
    │
    ▼
子进程 stdin → agent reader thread (loop_core.rs new_ipc)
    │  BufReader<stdin> → read_frame → cmd_tx.send(frame)
    │
    ▼
loop_core.run() (loop_core.rs)
    │  cmd_rx.recv_timeout(1s) → 收到 UserInput 帧
    │  dispatch_one(frame)
    │
    ▼
dispatch_one (loop_core.rs:723)
    │  try_handle_via_engines → InputEngine.handle_user_input
    │
    ▼
engine_input.rs: handle_user_input()
    │  1. 如果 seed 为空，自动创建会话
    │  2. push_user(text) 写入消息存储
    │  3. flush_meta()
    │  4. emit TurnStart 事件
    │  5. 返回 Outcome::ContinueTurn { turn_id, round_num: 0 }
    │
    ▼
loop_core.apply_outcome() → session.turn.run() → turn_engine.run()
    │
    ▼
engine_turn.rs: run_lap()
    │  1. 构建 provider 配置
    │  2. chat_stream (SSE 请求) ← 阻塞点（调用 API）
    │  3. 解析响应
    │  4. admit_batch() → 工具授权
    │  5. emit RoundDelta / ToolCallPreview 等事件
    │  6. 若有工具需执行 → execute tools → 下一轮 loop
    │  7. 若无工具 → TurnComplete
```

### 关键阻塞点

| 阻塞点 | 位置 | 说明 |
|--------|------|------|
| stdin 写入 | `registry.rs:239` | `writeln! + flush()` 同步阻塞 |
| chat_stream | `engine_turn.rs:797` | SSE 请求阻塞整个 agent 线程 |
| send_or_drop | `server.rs:559` | 500ms 超时后丢弃消息 |

---

## 四、仓库结构

### 顶层目录

```
/
├── .cargo/          # cargo 配置
├── .codegraph/      # CodeGraph 索引目录
├── .deepx/          # 运行时数据目录
│   ├── agent.log    # agent 日志（含新增时间戳）
│   ├── config.toml
│   ├── daemon.json
│   └── sessions/
├── apps/
│   ├── desktop/     # Electron 桌面端
│   ├── installer/    # Windows 安装器
│   ├── deepx-tui/   # TUI 终端界面
│   ├── updater/     # 自更新程序
│   └── winui-desktop/ # WinUI 桌面
├── crates/
│   ├── deepx-client/    # 客户端协议库
│   ├── deepx-companion/ # 伴侣应用
│   ├── deepx-config/    # 配置管理
│   ├── deepx-daemon/    # 守护进程（WebSocket + 调度）
│   ├── deepx-gate/      # AI API 网关（SSE/chat_stream）
│   ├── deepx-gate-testui/ # 网关测试 UI
│   ├── deepx-message/   # 消息存储库
│   ├── deepx-msglp/     # 核心消息循环（loop_core, engines）
│   ├── deepx-proto/     # 协议定义
│   ├── deepx-runtime/   # 运行时（registry, service, logger）
│   ├── deepx-session/   # 会话管理
│   ├── deepx-skills/    # 技能系统
│   ├── deepx-subagent/  # 子代理
│   ├── deepx-tools/     # 工具执行
│   ├── deepx-types/     # 类型定义
│   ├── deepx-update/    # 更新器
│   ├── deepx-vector/    # 向量存储
│   └── ratatui-markdown/ # TUI markdown 渲染
├── docs/            # 文档
├── packages/        # npm 包
├── scripts/         # 构建脚本
├── target/          # 编译产物
├── Cargo.toml       # workspace 根配置
├── Cargo.lock
└── watch-agent.ps1  # 日志监听脚本
```

### 关键 crate 源文件结构

#### deepx-msglp（核心消息循环）

```
src/
├── lib.rs
└── ring/
    ├── engine.rs        # Engine trait
    ├── engine_input.rs  # 输入处理（UserInput → ContinueTurn）
    ├── engine_turn.rs   # TurnEngine（gate → tools → repeat）
    ├── engine_tool.rs   # 工具引擎
    ├── loop_core.rs     # 主循环（run / dispatch_one / drain_pending）
    ├── engine_compact.rs # 上下文压缩
    ├── engine_goal.rs   # 目标模式引擎
    ├── engine_misc.rs   # 辅助功能
    ├── engine_session.rs# 会话管理
    ├── paced_emitter.rs # 速率限制发射器
    └── types.rs         # 类型定义
├── services/
│   ├── conflict.rs      # 冲突解决
│   ├── dashboard.rs     # 仪表板
│   └── notification.rs  # 通知
├── state/
│   ├── agent.rs         # AgentState（消息存储）
│   ├── lifecycle.rs     # 会话生命周期
│   └── skill_context.rs
└── util/
    └── mod.rs
```

#### deepx-runtime（运行时服务）

```
src/
├── lib.rs       # 公共导出
├── registry.rs  # AgentRegistry（spawn / send / get_or_spawn）
├── service.rs   # DeepxService（session_scoped / handle / send_message）
├── logger.rs    # FileLogSink（新增时间戳）
├── event_bus.rs # 事件总线
├── lease.rs     # 租约管理
├── worker.rs    # run_agent_worker（agent 子进程入口）
└── activity.rs  # 会话活动追踪
```

#### deepx-daemon（守护进程）

```
src/
├── main.rs      # 入口（run / agent / status / stop 子命令）
└── server.rs    # handle_connection（WebSocket select! 循环）
```

---

## 五、关键 Git 历史（最近相关 commit）

| Commit | 消息 | 说明 |
|--------|------|------|
| `d9e540e` | 修复：应用 daemon 修复，但仍存在前端发送后阻塞 | HEAD - 确认问题未完全解决 |
| `d2f66da` | fix: stop WebSocket cascade blocking new input | 引入回归（无界 send().await） |
| `48aaf62` | fix: request isolation, polling backoff | request_tx/event_tx 容量扩充 |
| `c8f50ed` | fix: revert onMount → onSettled | 桌面端修复 |

---

## 六、日志监听脚本使用说明

```powershell
# 实时监听 agent.log（彩色输出）
powershell -ExecutionPolicy Bypass -File C:\Users\QAQTam\watch-agent.ps1 -Follow

# 监听最后 20 行后开始实时跟随
powershell -ExecutionPolicy Bypass -File C:\Users\QAQTam\watch-agent.ps1 -TailLines 20 -Follow

# 仅显示发送相关的行
powershell -ExecutionPolicy Bypass -File C:\Users\QAQTam\watch-agent.ps1 -Follow -Filter "Input,TURN,dispatch,Ui2Agent"
```

### 彩色编码

- **青色** `dispatch_one` — agent 收到并分发命令
- **绿色** `received Ui2Agent frame` — 原始帧从 stdin 解析
- **黄色** `[INPUT]` — handle_user_input 处理步骤
- **品红** `[TURN]` — run_lap / chat_stream / admit_batch
- **红色** `[ERROR]` — 任何错误

---

## 七、待观察的调试线索

### 运行后关注 agent.log 中的模式

1. **第一条消息后无响应**：检查 `[INPUT] handle_user_input called` 是否出现
2. **消息到达但 loop 未启动**：检查 `dispatch_one` → `handle_user_input` → `TurnStart` 链路
3. **TurnStart 后无 chat_stream**：检查 `[TURN] run_lap` 后是否有 `calling chat_stream`
4. **chat_stream 未返回**：检查是否在 `gate succeeded` 或 `gate error` 行
5. **单条消息 vs 9 条消息差异**：观察 `cmd_rx.recv_timeout` 的 Timeout 分布

### 关键日志条目（按时间顺序）

```
[TIMESTAMP] [AGENT] received Ui2Agent frame: UserInput { text: "..." }
[TIMESTAMP] [AGENT] dispatch_one: frame=UserInput { text: "..." }
[TIMESTAMP] [INPUT] handle_user_input called, text_len=N
[TIMESTAMP] [INPUT] pushing user message to store
[TIMESTAMP] [INPUT] flushing meta
[TIMESTAMP] [INPUT] emitting TurnStart turn_id=t1 round_num=0
[TIMESTAMP] [TURN] run_lap turn_id=t1 round_num=0
[TIMESTAMP] [TURN] run_lap turn_id=t1 round_num=0 calling chat_stream
[TIMESTAMP] [TURN] run_lap turn_id=t1 round_num=0 gate succeeded, parsing response
[TIMESTAMP] [TURN] run_lap turn_id=t1 round_num=0 admit_batch N pending tools
```

如果任何步骤缺失或延迟异常，即为阻塞点。