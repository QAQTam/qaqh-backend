# QAQ-Harness Backend 架构事实报告

- **仓库**：`E:\qaqh-backend`（Rust workspace + `webui/` 前端）
- **基线 commit**：`f51f365`（工作区有未提交改动，见 §7.0）
- **方法**：`codegraph` 索引（13,103 nodes / 51,046 edges，478 files）+ 逐文件精读
  + 一次独立的交叉审计（§8 的生产/测试拆分、unsafe 归属、错误类型口径、测试分类）
- **规模**：438 个 `.rs` 文件 / 167,957 行（`crates/`，排除 `target/`），另有 `webui/` 30 个源文件
- **性质**：只读探索。未修改任何代码、未新增测试、未给出改进建议
- **约定**：每条结论附 `路径:行号`。无法确认的写"未找到"，不猜
- **可信度约定**：凡标注"约"或给出近似方法的数字（主要在第 9 步）为**近似值**；
  §8 的数字均给出精确计数命令与口径。报告中我曾犯过并已更正的错误在
  §1.3 与 §8.5 就地标注。

> **方法学限制（影响 §9）**：`codegraph` 对本仓的跨 crate 全限定调用解析不完整。例如
> `qaqh_gate::chat_stream` 定义于 `crates/qaqh-gate/src/lib.rs:61`，实际调用点在
> `crates/qaqh-runtime/src/agent/turn_lap/gate.rs:387`，但 `codegraph callers chat_stream`
> 返回 "No callers found"。因此 §9 的扇入/扇出统计改用自建的正则调用图（纯语法层，
> 不解析宏与 trait 动态分派），并在文中标注所用方法。

---

## 第 0 步：codegraph 可用性

`codegraph --help` 输出 20 个子命令：`init / uninit / index / sync / status / query /
explore / context / node / files / daemon / unlock / callers / callees / impact / affected /
install / uninstall / telemetry / upgrade / version`。

发现时 `.codegraph/` 只有 `.gitignore` 一个文件、`codegraph status` 报 `Not initialized`，
索引不存在，因此执行了 `codegraph init -y`。产物全部落在 `.codegraph/`（该目录已被
`.codegraph/.gitignore` 自身忽略），仓库源码零改动。

**索引可用性**：`codegraph init` 报告 "13 files could not be read"，但 `.codegraph/errors.log`
记录的是 `webui/src/App.tsx` 等 **14 个当时不存在的旧路径**（对应 `git status` 的 ` D` 项）。
这些文件现已存在于新路径（`webui/src/app/App.tsx` 等），即该错误日志是**上一次索引状态的
残留**，不影响本次分析。

---

## 第 1 步：结构总览

### 1.1 crate 清单与职责

workspace 成员见 `Cargo.toml:3-24`，共 20 个 crate。职责取自各自 `Cargo.toml` 的
`description` 字段（`qaqh-skills` 未填 description）：

| crate | 职责（引 description / 模块文档） | 行数 |
|---|---|---|
| `qaqh-runtime` | "GUI-independent QAQ-Harness daemon application runtime"——agent loop、TurnActor、ToolRuntime、RingingHub | 47,588 |
| `qaqh-workspace` | "QAQ-Harness in-process tool execution library"——typed tools、permission、audit、sandbox | 34,393 |
| `qaqh-session` | "session manager — singleton, list/load/save/active" + canonical facts/projection/replay | 26,318 |
| `qaqh-gate` | "LLM API gateway — HTTP streaming, message conversion"，三协议适配 | 9,250 |
| `qaqh-daemon` | "Headless QAQ-Harness application daemon"——axum HTTP/SSE、lease、driver | 7,435 |
| `qaqh-mcp` | "MCP client support — dynamic tools/resources from MCP servers" | 5,972 |
| `qaqh-client` | "Ringing v2 daemon client (HTTP/SSE) — shared by TUI and desktop shells" | 5,913 |
| `qaqh-config` | "configuration: provider registry, config load/save" | 5,696 |
| `qaqh-message` | "message store with state-machine lifecycle" | 5,328 |
| `qaqh-subagent` | "subagent tool — spawn isolated Ringing sub-sessions" | 3,447 |
| `qaqh-types` | "shared type definitions" | 3,306 |
| `qaqh-lsp` | "LSP client support — precise code navigation via language servers" | 2,714 |
| `qaqh-webui-gateway` | "Loopback-only browser gateway for the QAQ-Harness WebUI" | 2,700 |
| `qaqh-domain` | "Ringing 领域层：中立 DomainCommand/DomainEvent，不依赖 legacy wire 类型" | 2,175 |
| `qaqh-skills` | 未填 description（`crates/qaqh-skills/Cargo.toml` 无该字段） | 1,852 |
| `qaqh-ringing` | "Ringing 线协议层（Wire）：envelope/ack/batch/snapshot/content ref/worker frame/能力协商" | 1,652 |
| `qaqh-sandbox` | "Platform capability discovery and Linux process sandboxing" | 1,072 |
| `qaqh-policy` | "Pure policy decisions and sandbox specifications" | 495 |
| `qaqh-config-api` | "配置契约层（wire DTO）：ConfigDto 读模型 / ConfigPatch 写模型" | 483 |
| `qaqh-title` | "Session title generation: fallback truncation and LLM summary task" | 168 |

**合计：438 个 `.rs` 文件 / 167,957 行**（排除 `target/`）。

### 1.2 依赖方向文本图

箭头方向 = 依赖方向（A → B 读作" A 依赖 B"）。数据取自各 `crates/*/Cargo.toml` 的
`[dependencies]` 段（**不含** `[dev-dependencies]`）。

```
                          ┌─────────────────┐
                          │   qaqh-types    │  ← 叶子（无内部依赖）
                          └────────┬────────┘
                                   │
        ┌──────────────────────────┼──────────────────────────┐
        │                          │                          │
┌───────▼────────┐        ┌────────▼────────┐        ┌────────▼────────┐
│  qaqh-domain   │        │  qaqh-gate      │        │ qaqh-config-api │ ← 叶子
└───┬────────┬───┘        └─────────────────┘        └────────┬────────┘
    │        │                    ▲                            │
    │        │                    │                            │
    │   ┌────▼─────────┐          │                    ┌───────▼────────┐
    │   │ qaqh-ringing │          │                    │  qaqh-config   │
    │   └────┬─────────┘          │                    └───────┬────────┘
    │        │                    │                            │
    │        │                    │                            │
┌───▼────────▼───┐   ┌───────────┴──────┐   ┌─────────────────┐│
│  qaqh-session  │   │   qaqh-runtime   │──▶│  qaqh-workspace ││
└───────┬────────┘   └───┬────┬─────┬───┘   └──┬───┬───┬───┬──┘│
        │                │    │     │          │   │   │   │   │
        │                │    │     │          │   │   │   │   │
        │        ┌───────┘    │     └──────────┘   │   │   │   │
        │        │            │                    │   │   │   │
   ┌────▼────────▼──┐  ┌──────▼──────┐  ┌──────────▼┐  │   │   │
   │  qaqh-message  │  │ qaqh-title  │  │qaqh-skills│  │   │   │
   └────────────────┘  └─────────────┘  └───────────┘  │   │   │
                                                       │   │   │
                        ┌──────────────────────────────┘   │   │
                        │        ┌─────────────────────────┘   │
                        │        │      ┌──────────────────────┘
                   ┌────▼────────▼──┐ ┌─▼────────────┐  ┌───────▼──────┐
                   │ qaqh-subagent  │ │ qaqh-policy  │  │ qaqh-sandbox │
                   └────────────────┘ └──────────────┘  └──────┬───────┘
                                                               │
                                                          (qaqh-policy)

   ┌──────────────┐        ┌───────────────┐        ┌────────────────────┐
   │ qaqh-client  │        │  qaqh-daemon  │        │ qaqh-webui-gateway │
   └──────────────┘        └───────────────┘        └────────────────────┘
     ↑ domain,ringing,       ↑ config,domain,mcp,       ↑ domain,ringing,
       session,types           ringing,runtime,            types
                               sandbox,session,
                               subagent,types
```

未在图中展开的边（逐条引自各 `Cargo.toml`）：

- `qaqh-session` → `qaqh-domain`, `qaqh-message`, `qaqh-types`
  （`crates/qaqh-session/Cargo.toml:11-13`）
- `qaqh-message` → **仅** `qaqh-types`（`crates/qaqh-message/Cargo.toml:12`）；
  `qaqh-session` 只出现在其 `[dev-dependencies]`（`:21`）
- `qaqh-runtime` → `qaqh-config`, `qaqh-config-api`, `qaqh-domain`, `qaqh-gate`, `qaqh-lsp`,
  `qaqh-mcp`, `qaqh-message`, `qaqh-policy`, `qaqh-ringing`, `qaqh-sandbox`, `qaqh-session`,
  `qaqh-skills`, `qaqh-subagent`, `qaqh-title`, `qaqh-types`, `qaqh-workspace`
- `qaqh-lsp` → `qaqh-config`, `qaqh-types`, `qaqh-workspace`
- `qaqh-mcp` → `qaqh-config`, `qaqh-types`, `qaqh-workspace`

**层级收敛点**：`qaqh-runtime` 依赖 **16 个**内部 crate（除自身与
`qaqh-client` / `qaqh-daemon` / `qaqh-webui-gateway` 之外的**全部**），是唯一的汇聚点；
`qaqh-types`、`qaqh-config-api`、`qaqh-policy` 是叶子（零内部依赖）。
`qaqh-client` / `qaqh-daemon` / `qaqh-webui-gateway` 是三个"顶层消费者"，
不被任何 crate 依赖。

### 1.3 依赖环 / 下层依赖上层

**未发现环。** 用各 `Cargo.toml` 的 `[dependencies]` 边（**不含** `[dev-dependencies]`）
构造有向图，可拓扑排序，是 DAG。关键链：`qaqh-types` → `qaqh-message` → `qaqh-session`
→ `qaqh-runtime`，以及 `qaqh-types` → `qaqh-domain` → `qaqh-ringing` → `qaqh-runtime`。

三处需要单独说明的边界：

1. **`qaqh-message` → `qaqh-session` 是一条 "下层依赖上层" 的边，但只存在于
   dev-dependencies**：`crates/qaqh-message/Cargo.toml:21` 声明
   `qaqh-session = { path = "../qaqh-session" }`，而**生产依赖段**（`:11-15`）只有
   `qaqh-types` / serde / serde_json / log。反向的真实生产边是
   `qaqh-session → qaqh-message`（`crates/qaqh-session/Cargo.toml:12`）。
   验证：`crates/qaqh-message/src/*.rs` 内 grep `qaqh_session` → **0 命中**；
   反向 `crates/qaqh-session/src/*.rs` 内 grep `qaqh_message` → **23 命中**。
   即**生产依赖图是 DAG**（`qaqh-types` → `qaqh-message` → `qaqh-session` →
   `qaqh-runtime`），环只出现在测试图里（`dev-dependencies` 双向声明，
   `qaqh-message` 的 dev 边注释见 `crates/qaqh-session/Cargo.toml:25`
   "Test-only: deterministic WAL read-fault injection (BUG-2026-09-13-06)"）。

   全部 **3 条 dev-only 内部依赖边**（`[dev-dependencies]` 段扫描）：
   | 边 | 位置 |
   |---|---|
   | `qaqh-config` --dev--> `qaqh-workspace` | `crates/qaqh-config/Cargo.toml` `[dev-dependencies]` |
   | `qaqh-message` --dev--> `qaqh-session` | `crates/qaqh-message/Cargo.toml:21` |
   | `qaqh-session` --dev--> `qaqh-message` | `crates/qaqh-session/Cargo.toml:25` |

2. **`qaqh-config` → `qaqh-workspace` 同样只在 dev-dependencies**：
   `crates/qaqh-config/Cargo.toml` 的 `[dev-dependencies]` 段有
   `qaqh-workspace = { path = "../qaqh-workspace" }`（注释说明为 BUG-2026-09-13-15 回归测试
   复用 permission 词汇表）。因 `qaqh-workspace` 依赖 `qaqh-domain`/`qaqh-policy`/`qaqh-sandbox`/
   `qaqh-skills`/`qaqh-types`，这是**测试期的下→上依赖**，不进生产二进制。

### 1.4 最大 10 个文件

| 行数 | 文件 |
|---|---|
| 2,968 | `crates/qaqh-message/src/store.rs` |
| 2,657 | `crates/qaqh-runtime/src/registry.rs` |
| 2,434 | `crates/qaqh-session/src/manager.rs` |
| 2,419 | `crates/qaqh-daemon/src/axum_server.rs` |
| 2,276 | `crates/qaqh-gate/src/responses_api.rs` |
| 2,220 | `crates/qaqh-runtime/src/timeline.rs` |
| 2,140 | `crates/qaqh-subagent/src/lib.rs` |
| 2,027 | `crates/qaqh-runtime/src/agent/engine_turn.rs` |
| 1,799 | `crates/qaqh-runtime/src/ringing/hub.rs` |
| 1,706 | `crates/qaqh-webui-gateway/src/lib.rs` |

注：`crates/qaqh-daemon/src/axum_server.rs` 虽仍有 2,419 行，但主体已拆到
`crates/qaqh-daemon/src/axum_server/axum_impl/`（见 `mod.rs:1-4` 的拆分说明）。

---

## 第 2 步：核心 agent loop

### 2.1 入口与完整调用链

loop 的**唯一分派入口**是 `Loop::dispatch_frame`，其文档明确要求"Every dispatcher must
route through here; never call `dispatch_ringing_one` directly"
（`crates/qaqh-runtime/src/agent/loop_core.rs:570`）。

```text
① 用户消息到达 daemon
   POST /ringing/v2/commands/{id}
   └ crates/qaqh-daemon/src/axum_server/axum_impl/mod.rs:158-161
   └ handle_command_v2        crates/qaqh-daemon/src/axum_server/axum_impl/v2.rs:869

② 投递到会话线程
   AgentRegistry::send_ringing
   └ crates/qaqh-runtime/src/registry.rs:1410
   └ :1435-1452  AgentTransport::InProcess { cmd_tx, cancel } → cmd_tx.send(WorkerCommand)
      · :1441-1444 若为 interrupt 类命令，先置 cancel 再入队（"立即生效"语义）

③ 会话线程装配（每会话一线程）
   AgentRegistry::spawn_session_inprocess
   └ crates/qaqh-runtime/src/registry.rs:1133
   └ :1146 LoopChannels::new()（cmd/event 双向 sync_channel）
   └ :1201-1217 std::thread::Builder::name("qaqh-session-{seed}") → run_session_actor
   └ crates/qaqh-runtime/src/actor.rs:158

④ 主循环
   Loop::run                  crates/qaqh-runtime/src/agent/loop_core.rs:426
   └ :428 init_session()      crates/qaqh-runtime/src/agent/loop_core.rs:493
   └ :457 cmd_rx.recv_timeout(1s)
   └ :481 self.dispatch_frame(cmd)

⑤ 唯一分派闸门（causation 作用域 + safe_dispatch + persist 排空）
   Loop::dispatch_frame       crates/qaqh-runtime/src/agent/loop_core.rs:570
   └ :573 paced_emitter.enter_causation(causation)
   └ :574 dispatch_ringing_one
   └ :577 session.agent.drain_persist_ops()

⑥ 三路路由
   Loop::dispatch_ringing_one crates/qaqh-runtime/src/agent/loop_core.rs:621
   └ :637 RingingCommand::Control      → on_control
   └ :640 RingingCommand::Conversation → on_conversation
   └ :643 RingingCommand::Tool         → on_tool

⑦a 用户输入（ConversationSendMessage 分支）
   Loop::on_conversation      crates/qaqh-runtime/src/agent/loop_dispatch_conversation.rs:199
   └ :361-378 若存在悬挂 turn，先 abort_suspended（新输入取代旧 turn）
   └ :379 record_input_accepted → 追加 canonical InputAccepted fact
      （>8 KiB 走外部化，见 :66-77）
   └ :396-405 构造 RingContext
   └ :406 InputEngine::handle_user_input
      └ crates/qaqh-runtime/src/agent/engine_input.rs:28
      └ :39-45 无会话则 auto-create + set_session
      └ :85-90 cancel.clear() + workspace clear_cancel()
      └ :92-124 compliance 守卫（qaqh_policy::content_guard）
      └ :126 activate_explicit_skills
      └ :234 Outcome::ContinueTurn { turn_id, round_num: 1, .. }
   └ :415 apply_outcome(outcome)

⑧⑨ 上下文组装（每 lap 一次）
   Outcome::ContinueTurn 分支    crates/qaqh-runtime/src/agent/loop_outcome.rs:543
   └ :565 TurnEngine::run(ctx, tool, turn_id, round_num, usage)
      └ crates/qaqh-runtime/src/agent/engine_turn.rs:534 → run_lap :542
      └ :1445 drain_persist_ops()（L1 回合边界持久化）
      └ :1451-1465 MCP / LSP 动态工具投影刷新
      └ :1468 sync_mcp_resource_injection
      └ :1470 provider_for(ctx, &turn_id)
         └ crates/qaqh-runtime/src/agent/turn_lap/gate.rs:712
      └ :1475-1483 取消检查（cancel.is_set() / workspace::is_cancel()）
      └ :1594 Self::prepare_gate_snapshot(ctx)  ← 上下文组装
         └ crates/qaqh-runtime/src/agent/engine_turn.rs:1304
         └ :1312 ctx.agent.build_context()
            └ MessageStore::build_context_for_gate
              crates/qaqh-message/src/store.rs:991
              └ :1002 flat_in_write_order(compact_skip)
                 └ crates/qaqh-message/src/store.rs:956（按 msg_id 排序的写序序列化）
         └ :1320 estimate_prepared_request（token 估算）
         └ :1332-1389 compact_preflight → 可能触发 run_auto_compact

⑩ 调 LLM（唯一 provider 出口）
   TurnEngine::run_lap :1616 gate_request(...)
   └ crates/qaqh-runtime/src/agent/turn_lap/gate.rs:345
   └ :387 qaqh_gate::chat_stream(
          provider, messages, tools,
          ctx.agent.config.max_tokens,            ← token 上限
          reasoning_effort, session_id,
          Some(&cancel_arc),                      ← 取消
          &mut |event| ... )                      ← 流式回调
      └ crates/qaqh-gate/src/lib.rs:61
      └ :71-105 按 provider.kind 三路分派：
         responses_api::chat_stream_responses   crates/qaqh-gate/src/responses_api.rs:542
         message_api::chat_stream_anthropic     crates/qaqh-gate/src/message_api.rs:782
         chat_completions_api::chat_stream_openai
                                                crates/qaqh-gate/src/chat_completions_api.rs:69

⑪ 解析流（gate 内 → 回调 → timeline/domain 双发）
   transport::run_with_retry    crates/qaqh-gate/src/transport.rs:160
   └ 单次尝试闭包 → SseDecoder::next_frame
      crates/qaqh-gate/src/sse.rs:64
   └ 每帧 → StreamEvent（types.rs:462）
      ContentDelta     → gate.rs:396-441 → TextDelta + RoundDelta
      ReasoningDelta   → gate.rs:442-487 → TextDelta + RoundDelta
      Done{raw_message,usage,stop_reason}
                       → gate.rs:488-544 → record_usage + UsageUpdated
      UsageUpdate / Error / Retrying / WebSearchStatus / ToolCallProgress

⑫ 工具调用
   解析入 store：parse::parse_and_ingest
   └ crates/qaqh-runtime/src/agent/turn_lap/parse.rs:36
   准入 + 执行：admit::admit_and_dispatch
   └ crates/qaqh-runtime/src/agent/turn_lap/admit.rs:136
   └ :149 phase = ToolsRunning
   └ :151 get_last_step_pending()
   └ :160-195 重复 call_id 检查 → TurnAborted
   └ :199-209 MAX_TOOL_CALLS_PER_ROUND = 16，超出直接回填错误
   └ :222 ToolRuntime::serial_call_ids（写序列化判定）
   └ :223 tool.admit_batch → engine_tool.rs:451（权限/progress 事件）
   └ :105-115 admit::execute_admitted_batch
      └ ToolRuntime::execute_batch  crates/qaqh-runtime/src/agent/tool_runtime.rs:114
      └ :43 MAX_PARALLEL_TOOL_WORKERS = 4

⑬ 结果回填
   turn_lap::backfill::handle_tools_done
   └ crates/qaqh-runtime/src/agent/turn_lap/backfill.rs:146
   └ :153 emit_completed_tool_round
   └ :155 skills.complete_model_lap()
   └ :172 Outcome::ContinueTurn { round_num: round_num + 1 }
   └ 回到 ⑧（loop_outcome.rs:565 递归重入 run）

⑭ 结束
   turn_lap::backfill::handle_turn_complete
   └ crates/qaqh-runtime/src/agent/turn_lap/backfill.rs:182
   └ :217-221 TimelineIntent::TurnSealed { state: Completed }
   └ :222 Outcome::TurnComplete
   └ loop_outcome.rs:432-500：
      :444 lifecycle.turn_completed → :448 session.flush()
      :449-456 发射 ConversationEvent::TurnCompleted
      :499 phase = LoopPhase::Idle
```

**递归而非循环**：`ContinueTurn` 的处理体在 `crates/qaqh-runtime/src/agent/loop_outcome.rs:565`
**再次调用 `turn.run(...)`**，然后 `:580 self.apply_outcome(next_outcome)`。即 lap 之间是
Rust 调用栈递归（`run_lap` 内的 `return Outcome::ContinueTurn` 只是把栈展开一层
再由 `apply_outcome` 重新压入），深度随工具轮数线性增长。
`crates/qaqh-runtime/src/agent/turn_lap/gate.rs:345` 的 `gate_request` 自身**不含 lap 循环**
（`engine_turn.rs:1472-1473` 注释："单回合执行块：所有路径均 return（clippy::never_loop），无需循环"）。

### 2.2 loop 是否依赖具体 provider 的类型？

**不依赖。** 证据：

- `crates/qaqh-runtime/src/agent/mod.rs:37-40` 的模块规则明确："`agent/` 内禁止引用 runtime
  自身的 ringing 模块"；同一文件 `:32-33`："引擎模块为固定集合，无独立 `Engine` trait"。
- loop 与 provider 的唯一接触面是 `provider_for`（`crates/qaqh-runtime/src/agent/turn_lap/gate.rs:712`）。
  它返回 `qaqh_gate::ProviderConfig`（一个**数据**结构，`crates/qaqh-gate/src/types.rs:167-218`），
  内部按 `EndpointSpec.protocol` 字符串三路构造（`:714-715`）。
- LLM 调用点是 `qaqh_gate::chat_stream(provider, ...)`（`gate.rs:387`），签名只吃
  `&ProviderConfig`（`crates/qaqh-gate/src/lib.rs:62`）。
- 唯一的 provider 分支泄漏是 `gate.rs:766-773`：`if p.model.contains("muse-spark")` 硬编码
  模型名特判（注释自述"Muse Spark 专项"）。其余差异全部走 `EndpointSpec` 字段映射
  （`gate.rs:737-742`、`:754-764`、`:796-802`）。

### 2.3 取消 / 超时 / 重试 / token 上限的处理位置

#### 取消（4 层）

| 层 | 位置 | 机制 |
|---|---|---|
| token | `crates/qaqh-runtime/src/agent/types.rs:49` `CancelToken` | `CancelNode` 树，父子传播（`:69-158`）；`set/clear/is_set/arc`（`:194-212`） |
| 置位（命令侧） | `crates/qaqh-runtime/src/registry.rs:1441-1444` | interrupt 类命令入队**前**置 `cancel`，再入 `cmd_tx`；同时 `qaqh_workspace::set_session_cancel` |
| 置位（会话侧） | `crates/qaqh-runtime/src/agent/loop_dispatch_conversation.rs:418-419` | `self.cancel.set(); qaqh_workspace::set_cancel(true);` |
| 判定（lap 边界） | `crates/qaqh-runtime/src/agent/engine_turn.rs:1475`、`:1626` | `cancellation().is_set() \|\| qaqh_workspace::is_cancel()` |
| 判定（工具执行） | `crates/qaqh-workspace/src/lib.rs:221` `CANCEL` + `:340` `SESSION_CANCELS` | 工具执行前检查；`file:line` 见 `registry.rs:1443` 写入端 |
| 判定（HTTP 流） | `crates/qaqh-gate/src/transport.rs:50-52` `is_cancelled` | 由 `gate.rs:379` `ctx.cancel.arc()` 传入，流循环按 `SSE_POLL_INTERVAL = 50ms`（`transport.rs:25`）轮询 |

interrupt 类命令的判定集合：`crates/qaqh-runtime/src/agent/loop_core.rs:67-78`
（`SessionResume | SessionShutdown | SessionCreate | ConversationCancel`）。

#### 超时（5 处）

| 位置 | 值 | 用途 |
|---|---|---|
| `crates/qaqh-gate/src/lib.rs:43` | `connect_timeout 30s` | 连接建立 |
| `crates/qaqh-gate/src/lib.rs:44-45` | `tcp_keepalive 60s` / `pool_idle_timeout 120s` | 长连接维持 |
| `crates/qaqh-gate/src/lib.rs:46` | `timeout 30min` | 请求总预算（注释：流式响应可合法超 5 分钟） |
| `crates/qaqh-gate/src/transport.rs:34` | `STREAM_IDLE_TIMEOUT = 300s` | 空闲看门狗（半开连接判定） |
| `crates/qaqh-runtime/src/agent/loop_core.rs:457` | `recv_timeout(1s)` | 主循环轮询（兼作 compact 轮询） |
| `crates/qaqh-daemon/src/server.rs:270` | `interval 60s` | idle 会话卸载周期 |
| `crates/qaqh-daemon/src/server.rs:313` | `interval 3s` | 死 worker 重生 + 僵尸 receipt 巡检 |
| `crates/qaqh-daemon/src/axum_server/axum_impl/sse.rs:256` | `KeepAlive 15s` | SSE 保活 |
| `crates/qaqh-gate/src/transport.rs:224-226` | `retry_after_cap = max_delay × 5` | 服务端 `retry-after` 头信任上限 |

#### 重试

统一实现在 `crates/qaqh-gate/src/transport.rs`：

- 策略结构 `RetryPolicy`（`:78-88`），默认 `max_retries = 5`、`base_delay = 1s`、
  `max_delay = 30s`、`idle_timeout = 300s`（常量 `:29-34`，`Default` 于 `:90-99`）
- 从 `EndpointSpec.retry`（`RetrySpec`）编译，`max_retries` 上限截断到 32（`:104-123`）
- 退避 `delay_for`：`base × 2^(attempt-1)`，±10% jitter，封顶 `max_delay`（`:127-137`）；
  `checked_pow` 防溢出（`:131-134`）
- 执行器 `run_with_retry`（`:160-200`）：`Attempt::{Ok,Retry,Fatal}` 三态分类；
  取消检查在每次尝试前（`:172`）；`Retrying` 事件（`:188-193`）；可取消睡眠（`:194-196`）
- 错误分类 `is_retryable`（`:70-72`）：**仅** `429 | 500 | 503` 可重试
- 错误描述表 `http_error_description`（`:271-282`）
- `retry-after` 解析（`:233-269`）：`retry-after-ms` 优先，其次秒/HTTP-date，超上限钳制
- **注意**：`is_retryable` 是 `pub(crate)`，三协议适配器各自决定何时调用；`run_with_retry`
  是唯一的计数/退避/事件循环

#### token 上限

| 位置 | 值 | 含义 |
|---|---|---|
| `crates/qaqh-runtime/src/agent/turn_lap/gate.rs:391` | `ctx.agent.config.max_tokens` | 单次请求输出上限，直传 `chat_stream` |
| `crates/qaqh-runtime/src/agent/engine_turn.rs:216-218` | `hard_context_limit` = `context_window` 或 `context_limit` | 硬上下文窗口 |
| `crates/qaqh-runtime/src/agent/engine_turn.rs:185-207` | `compact_preflight` | 软阈值 `context_limit × auto_compact_threshold`；硬窗口触发 `ForcedCompact` |
| `crates/qaqh-runtime/src/agent/engine_turn.rs:134` | `MAX_CONTEXT_OVERFLOW_RECOVERIES = 2` | 端点报超限后的压缩重试次数（`:1655`） |
| `crates/qaqh-runtime/src/agent/engine_turn.rs:129` | `MAX_STREAM_CONTINUATIONS = 3` | 上游掐流续写次数（`:1755`） |
| `crates/qaqh-runtime/src/agent/turn_lap/admit.rs:199` | `MAX_TOOL_CALLS_PER_ROUND = 16` | 单轮工具调用上限 |
| `crates/qaqh-runtime/src/agent/injection.rs:37,39` | `MAX_STEER_PER_SAFE_POINT = 8` / `MAX_INTERJECT_PER_SAFE_POINT = 4` | 注入配额 |
| `crates/qaqh-runtime/src/agent/tool_runtime.rs:43` | `MAX_PARALLEL_TOOL_WORKERS = 4` | 并行工具数 |
| `crates/qaqh-runtime/src/agent/loop_core.rs:135-136` | cmd 4096 / event 16384 slots | 通道背压（注释记录旧值 655360 导致单会话 300MB+） |
| `crates/qaqh-types/src/tool_result.rs:50` | `TOOL_MODEL_MAX_CHARS` / `TOOL_SUMMARY_MAX_CHARS` | 工具结果进模型前的截断 |
| `crates/qaqh-runtime/src/agent/loop_dispatch_conversation.rs:70` | `8 * 1024` | canonical inline content 上限，超出外部化 |
| `crates/qaqh-webui-gateway/src/lib.rs:301` / `crates/qaqh-daemon/src/axum_server/axum_impl/mod.rs:71` | `MAX_BODY_BYTES = 16 MiB` | HTTP body 上限 |
| `crates/qaqh-daemon/src/axum_server/axum_impl/mod.rs:72` | `MAX_CONNECTIONS = 128` | 并发连接上限 |
| `crates/qaqh-runtime/src/ringing/v2.rs:30` / `hub.rs:31` | `LIVE_CAPACITY = 1024` / `LIVE_BROADCAST_CAPACITY = 1024` | SSE 广播环形缓冲 |

### 2.4 会话状态存在哪 / 重启后能恢复什么

#### 内存（进程内，重启即失）

- 每会话一个 **OS 线程** + 一对 `mpsc::sync_channel`：`LoopChannels`
  （`crates/qaqh-runtime/src/agent/loop_core.rs:111-146`），线程创建于
  `crates/qaqh-runtime/src/registry.rs:1201-1217`
- `Loop.session: SessionBundle`（`loop_core.rs:178`；结构定义
  `crates/qaqh-runtime/src/agent/types.rs:579`），含 `AgentState` / `StatsCollector` /
  `TurnEngine` / `ToolEngine`（`loop_core.rs:12-15`）
- `TurnEngine.suspended: Option<TurnState>`（`crates/qaqh-runtime/src/agent/engine_turn.rs:225`）
  ——**悬挂 turn 只存在于内存**
- `MessageStore`（`crates/qaqh-message/src/store.rs:139-197`）的 `turns` / `trailing_messages` /
  `deferred_trailing` / `orphan_tool_results` / `pending_persist`（`:185`）
- `AgentRegistry.instances: HashMap<String, AgentInstance>`（`crates/qaqh-runtime/src/registry.rs:316`），
  外层 `Arc<Mutex<AgentRegistry>>`（`crates/qaqh-runtime/src/service.rs:22`）
- `V2ProjectionHub.sessions: RwLock<HashMap<..>>`（`crates/qaqh-runtime/src/ringing/v2.rs:120`）
- 运行期 residency overlay（`registry.rs:333-336`，注释明确"never persisted"）
- 消息配额计数 `outbound_attempts`（`registry.rs:346-348`，注释"不要求跨 daemon 重启持久化"）

#### 磁盘

| 产物 | 路径常量 | 位置 |
|---|---|---|
| canonical facts | `events.jsonl` | `crates/qaqh-session/src/canonical/log.rs:25` |
| commit 屏障 | `events.commit.json` | `crates/qaqh-session/src/canonical/log.rs:28` |
| 消息归档（append-only） | `messages.jsonl` | `crates/qaqh-session/src/store/mod.rs:5` |
| 会话元数据 | `meta.json`（原子替换：tmp + `sync_all` + rename） | `crates/qaqh-session/src/store/mod.rs:4`、`:21-34` |
| 列表索引 | `index.json` | `crates/qaqh-session/src/store/mod.rs:7-8` |
| 消息 WAL（L2） | `WalWriter` | `crates/qaqh-message/src/store.rs:192`、`crates/qaqh-message/src/wal.rs` |
| timeline 快照 | — | `crates/qaqh-runtime/src/timeline_store.rs:15` |
| 命令幂等 receipt | — | `crates/qaqh-runtime/src/ringing/pending_store.rs:97-100`（注释："只保存哈希，不把命令正文、用户文本或附件元数据写入磁盘"） |
| 工具生命周期账本 | canonical `events.jsonl` 之上 | `crates/qaqh-session/src/canonical/tool_ledger.rs:1` |
| 团队/看板事件 | `events.jsonl` | `crates/qaqh-session/src/team/store.rs:20`、`crates/qaqh-session/src/team/board/store.rs:20` |

数据根：Windows `%USERPROFILE%\.qaqh`，Unix `$XDG_CONFIG_HOME/qaqh` 或 `$HOME/.config/qaqh`
（`crates/qaqh-types/src/platform.rs:54-69`），可被 `QAQH_DATA_DIR` 覆盖（`:57`）。

#### 重启后能恢复 / 不能恢复

**能恢复**（有明确代码路径）：

1. **canonical facts 与其投影**——`V2ProjectionHub::bootstrap` 从 committed 前缀重建
   （`crates/qaqh-runtime/src/ringing/v2.rs:145-171`，`:151` `resolve_identity`，
   `:160` `CanonicalCursor::snapshot`）
2. **消息历史**——`SessionManager::load_for_resume`
   （`crates/qaqh-session/src/manager.rs:284`）→
   `lifecycle::init_session`（`crates/qaqh-runtime/src/agent/state/lifecycle.rs:135`）
   → `MessageStore::from_messages`（`:174-178`）
3. **compact 水位**——`meta.compact_covered_through_msg_id`
   （`crates/qaqh-session/src/manager.rs:32`、`lifecycle.rs:163-186`）
4. **未完成的工具 intent**——canonical recovery 执行器
   （`crates/qaqh-session/src/canonical/recovery_executor.rs`，`:172` `recover_open_intents`，
   `:184-188` 落盘 `SessionRecovered { outcome: Writable }`）
5. **timeline**——快照落后时从 `messages.jsonl` 重建
   （`crates/qaqh-runtime/src/ringing/timeline_rebuild.rs:3`、
   `crates/qaqh-runtime/tests/timeline_rebuild.rs:81`）
6. **turn id 分配器不碰撞**——`lifecycle.rs:188-219` 用 `msg.turn_count()` 与注入的
   `timeline_turn_count` 取上界（`registry.rs:1170-1189`）
7. **命令幂等 receipt**——`PendingCommandStore::new_persistent`
   （`crates/qaqh-runtime/src/ringing/pending_store.rs:99`）

**不能恢复（内存态丢失）**：

1. **悬挂的 turn（`TurnEngine.suspended`）**——`engine_turn.rs:225` 是纯内存字段。
   重启后 in-flight turn 不在内存中，只能靠 canonical recovery 把 open tool intent
   seal 成不确定态，turn 本身不会被续跑。
2. `MessageStore.deferred_trailing`——`crates/qaqh-message/src/store.rs:154-156`
   注释明确"进程内缓冲（不持久化）"
3. `InjectionBus` 队列（`loop_core.rs:191`）
4. `CompactionPort` 后台压缩任务状态（`loop_core.rs:193`）
5. `pending_persist` 未 drain 的写操作（`store.rs:185`）——由 L2 WAL 兜底
6. residency overlay、`outbound_attempts` 计数（见上）

### 2.5 daemon 是否继续运行 agent、以及无前端时的挂起

- **daemon 不停**：daemon 进程的生命周期只由以下事件终止——`/control/v1/stop`
  （`crates/qaqh-daemon/src/axum_server/axum_impl/control.rs:39-51`）、
  `/control/v1/stop-if-idle`（`:53-70`）、OS 信号（`crates/qaqh-daemon/src/server.rs:249`
  `spawn_signal_shutdown`）。**没有任何"前端全部断开则退出"的逻辑**（全仓 grep
  `stop_if_idle` 只命中上述两处路由与其 handler）。
- **前端断开不影响 agent 执行**：agent 跑在独立的会话线程上
  （`registry.rs:1201-1217`），SSE 只是通过 `RingingHub.timeline_live` 广播
  （`crates/qaqh-runtime/src/ringing/hub.rs:268` `broadcast::channel`）与
  `V2ProjectionHub.live_tx` 观察它。
- **idle 卸载**：唯一"自动停 agent"的机制是 idle 卸载周期任务
  （`crates/qaqh-daemon/src/server.rs:266-301`，每 60s）；
  `AgentRegistry::unload_idle_sessions`（`crates/qaqh-runtime/src/registry.rs:1839`）
  只卸载 `liveness.unloadable() && idle_secs() >= idle_secs` 的实例（`:1848`），
  阈值来自 `config.session_idle_unload_secs`，`0` 表示禁用（`:277-278`、`:1840`）。
  下一次输入从磁盘 resume（`:1836-1838` 注释）。

### 2.6 授权请求 / AskUser 在无前端时的挂起与恢复

**挂起**：

1. `TurnEngine.suspended` 被置位并 `return Outcome::YieldToUser`；
   `loop_outcome.rs:582-585` 的处理体是空操作——注释："Turn suspended. Loop returns to
   Idle. The next PermissionResponse or a typed ask command will trigger resume."
2. 挂起内容**落盘为 canonical fact**：`TurnEngine::persist_interaction_requests`
   （`crates/qaqh-runtime/src/agent/engine_turn.rs:288-388`）为
   permission / ask / plan / todo_activation 四类各追加一条
   `InteractionRequested`（`:318-348`），正文以 `sha256` content ref 形式外置
   （`:353-367`）。
3. 正文可持久化取回：`AppState` 侧 `interaction_body_value`
   （`crates/qaqh-daemon/src/axum_server/axum_impl/v2.rs:435-447`）直接解 Inline，
   或经 content store 查 Ref。

**恢复**：

- 前端连上后查 `GET /ringing/v2/sessions/{session_id}/approvals`
  （`crates/qaqh-daemon/src/axum_server/axum_impl/mod.rs:170-173`），
  handler `handle_pending_approvals_v2`（`v2.rs:343`）从
  `state.v2_hub.bootstrap(...)` 的 `projections.control.interactions` 里筛出
  `resolution.is_none() && expired_reason.is_none()` 的条目（`:366-369`），
  分别返回 `pending_permission`（`:371-375`）与 `pending_interaction`
  （`:376-398`，kind ∈ {ask, plan}）。
- 答复经 `POST /ringing/v2/commands/{id}` 走 `handle_permission_resolved`
  （`crates/qaqh-runtime/src/agent/engine_turn.rs:649`）/
  `handle_ask_response`（`:763`）/ `handle_plan_response`（`:847`）/
  `handle_ask_dismiss`（`:981`），最终 `TurnEngine::resume`（`:546`）继续 lap。
- **"first-answer-wins"**：`TurnActor::admit_interaction_resolution`
  （`crates/qaqh-runtime/src/agent/turn_actor.rs:232`）；测试
  `interaction_resolution_is_first_answer_wins`（`:776`）。

**关键事实：无超时。** 全仓 grep `expired_reason|InteractionExpired` 命中 37 处，其中
`InteractionExpiryReason` 的**唯一生产写入点**是
`crates/qaqh-session/src/actor.rs:626-631`，其 reason 硬编码为
`InteractionExpiryReason::TurnCancelled`。**未找到**任何基于时间/租约的 interaction 过期逻辑。
因此：**前端全部断开且用户不答复时，turn 无限期挂起在 `YieldToUser`**，直到
(a) 用户取消（`ConversationCancel` → `cancel_tool_batch` 写入 `TurnCancelled` 过期）、
(b) 新用户输入取代它（`abort_suspended`，`engine_turn.rs:613`）、或
(c) 会话被 idle 卸载 / daemon 停止（`shutdown_all`）。

---

## 第 3 步：Provider 网关层

### 3.1 所有 provider 适配

`ProviderKind` 只有三个变体（`crates/qaqh-gate/src/types.rs:71-75`）：

```rust
pub enum ProviderKind { OpenAi, Responses, Anthropic }
```

由 `ProviderKind::from_str`（`:79-85`）从字符串映射，`"responses"` → Responses，
`"anthropic"` → Anthropic，**其余一律回退 OpenAi**（含未知值）。

| 适配 | 流式入口 | 同步入口 | 文件行数 |
|---|---|---|---|
| OpenAI Chat Completions | `chat_stream_openai` `chat_completions_api.rs:69` | `chat_sync_openai` `:927` | 1,445 |
| OpenAI Responses | `chat_stream_responses` `responses_api.rs:542` | `chat_sync_responses` `:693` | 2,276 |
| Anthropic Messages | `chat_stream_anthropic` `message_api.rs:782` | `chat_sync_anthropic` `:947` | 1,349 |

分派点：`crates/qaqh-gate/src/lib.rs:71-105`（流式）、`:114-124`（同步）。

#### 逐维度对比

| 维度 | OpenAi | Responses | Anthropic | 重复度 |
|---|---|---|---|---|
| 请求构造 | `convert_messages` `chat_completions_api.rs:746` | `convert_messages_to_input` `responses_api.rs:47` | `convert_messages_to_anthropic` `message_api.rs:71` | 三份独立实现，**协议本质差异，未共享**（`transport.rs:5-7` 注释明确保留） |
| 工具 schema | 无独立转换（直接内联） | `convert_tools` `responses_api.rs:465` + `sanitize_openai_schema` `:323`（142 行，41 分支） | `convert_tools` `message_api.rs:325` | 两份 |
| URL 构造 | `build_chat_url` `:1049` | `build_responses_url` `:25` | `build_anthropic_url` `:31` | 三份，逻辑同构（base + path + 去重斜杠） |
| SSE 流循环 | `stream_sse` `:482` | `parse_responses_sse` `:1153` + `feed_responses_sse` `:1306` | `stream_sse_anthropic_with_policy` `:585` | 三份循环骨架 |
| SSE 帧解码器 | 共享 `SseDecoder`（`sse.rs:31`） | 共享 | 共享 | **已统一** |
| 帧 → StreamEvent | `handle_chat_frame` `:367` + `emit_delta_fields` `:264` | `handle_responses_event` `:956` | `handle_anthropic_frame` `:353` | 三份 |
| 错误分类 | 共享 `is_retryable`（`transport.rs:70`）+ `http_error_description`（`:271`） | 共享 | 共享 | **已统一** |
| 重试 | 共享 `run_with_retry`（`transport.rs:160`） | 共享 | 共享 | **已统一** |
| usage 统计 | 帧内联解析 | `parse_usage` `responses_api.rs:887` | 帧内联解析 | 两份 |
| 消息组装 | `assemble_streamed_message` `:674` | `preserve_completed_output_item` `:1069` + `preserve_terminal_output` `:1140` | 帧内累积 | 三份 |
| stateful 增量过滤 | 共享 `filter_stateful_messages`（`transport.rs:304`） | 端点声明 stateful 时**明确警告无效**（`gate.rs:718-727`） | 共享 | **已统一** |
| tool_parser | 共享 `tool_parser.rs`（667 行） | 共享 | 共享 | **已统一** |

#### 重复代码量的量化

`crates/qaqh-gate/src/transport.rs:1-14` 的模块文档**自述**了收敛历史：三协议曾各持一份
字节级相同的实现——"3 个独立 current-thread tokio runtime、cancel 轮询、重试退避、错误
描述、skill envelope 归一、stateful 过滤、`SseTrace` 诊断"，现已收敛为本模块单一来源。
文档同时列出收敛时消除的 3 处行为漂移（`:9-14`）。

**剩余未统一的量**：三协议各自的 `convert_messages` + 帧处理 + `convert_tools` 共约
`746→927`（181 行）+ `47→496`（450 行）+ `71→353`（283 行）等区段，加上各自的流循环。
`transport.rs` 自身 682 行。

关键常量（判断重复消除程度）：
- `crates/qaqh-gate/src/lib.rs:40-52` 进程级共享 `reqwest::Client`（`LazyLock`），
  注释明确"禁止再复制构造"
- `crates/qaqh-gate/src/transport.rs:39-44` 进程级共享 tokio current-thread runtime
  （`FALLBACK_RT`）
- `crates/qaqh-gate/src/sse.rs:1-8` **明确记录了一处刻意的重复**：本 crate 的 `SseDecoder`
  与 `qaqh-client/src/sse_decoder.rs` 的同名解码器"**刻意不合一**（D3 决策，暂缓）"，
  理由是帧语义不同（本实现产出聚合 `data: String`；client 实现产出 `SseFrame{id, event_type, data}`），
  "合一需泛型 sink + 语义开关，收益 60 行不抵复杂度"。

### 3.2 统一的消息 / 事件类型

**消息类型**：`qaqh_types::Message` / `ContentBlock` / `ToolCall`（`crates/qaqh-types/src/message.rs`，
经 `crates/qaqh-types/src/lib.rs:39` 重导出）。三协议适配器都以此为输入输出。

**事件类型**：`qaqh_gate::StreamEvent`（`crates/qaqh-gate/src/types.rs:461-489`）：

```rust
pub enum StreamEvent {
    ContentDelta(String),
    ReasoningDelta(String),
    ToolCallProgress { index: usize, id: String, name: String, args_chunk: String },
    WebSearchStatus(String),                    // "in_progress" | "searching" | "completed"
    Done { raw_message: Message, usage: Option<UsageInfo>, stop_reason: Option<String> },
    UsageUpdate(UsageInfo),
    Error(String),
    Retrying { attempt: u32, max_retries: u32, delay_secs: u64, error: String },
}
```

**provider 特有字段是否泄漏？**

- **向上（loop）**：`StreamEvent` 已是归一化形态，`gate_request` 的回调只 match 上述 8 个变体
  （`crates/qaqh-runtime/src/agent/turn_lap/gate.rs:396-544`）。
- **向 provider 的配置面**：`ProviderConfig` 有 **22 个字段**（`types.rs:167-218`），其中
  大量是 provider 特有开关，例如 `tool_call_content_null`（OpenRouter 的
  `with_openrouter_compat` `:444-451`）、`stateful`（web proxy 增量模式）、
  `thinking_budget_large`（注释点名"zcode GLM-5.3"）、`responses_compat`
  （`ResponsesCompat` 7 个字段 `:228-250`）、`opencode_headers`（`x-opencode-*` 管理头
  `:116-136`）。这些是**配置数据**，不是运行时事件泄漏。
- **明确的泄漏点**：
  1. `crates/qaqh-runtime/src/agent/turn_lap/gate.rs:766-773` 的 `if p.model.contains("muse-spark")`
     模型名硬编码特判。
  2. `crates/qaqh-gate/src/types.rs:91-93` 的 `OPENCODE_CLIENT_ID` / `OPENCODE_CLIENT_VERSION`
     常量与 `types.rs:402-407` 的 `base_url.contains("opencode.ai/zen")` URL 子串判定。
  3. `crates/qaqh-gate/src/responses_api.rs:535` `is_muse_model`。
- **向前端协议的泄漏**：**未找到** provider 特有字段进入 `qaqh-domain`/`qaqh-ringing` 的
  wire 类型。前端消费的是 `qaqh_domain::{DomainEvent, TimelineIntent}`
  （`crates/qaqh-runtime/src/agent/types.rs:413-432` 的 `Emitter` trait 只有这两个出口），
  而二者的变体面（如 `crates/qaqh-session/src/session_fact_v2/projection_event.rs:289-810`
  的 `ProjectionPayload`）不含 provider 概念。

### 3.3 对外暴露的接口（完整签名）

`crates/qaqh-gate/src/lib.rs` 的公开面**只有 6 项**（`pub use` 3 项 + `pub fn` 2 项 +
`pub mod tool_parser`）：

```rust
// crates/qaqh-gate/src/lib.rs:22-23
pub use transport::RetryPolicy;
pub use types::{ProviderConfig, ProviderKind, ResponsesCompat, StreamEvent};

// crates/qaqh-gate/src/lib.rs:61-70
#[allow(clippy::string_slice)]
#[allow(clippy::too_many_arguments)] // 参数面塑形另立项（PLAN D-5）
pub fn chat_stream(
    provider: &ProviderConfig,
    messages: Vec<Message>,
    tools: Option<Vec<ToolDef>>,
    max_tokens: u32,
    effort: Option<String>,
    user_id: Option<String>,
    cancel: Option<&Arc<AtomicBool>>,
    on_event: &mut dyn FnMut(StreamEvent),
) -> anyhow::Result<()>;

// crates/qaqh-gate/src/lib.rs:109-113
pub fn chat_sync(
    provider: &ProviderConfig,
    messages: Vec<Message>,
    max_tokens: u32,
) -> Result<String, String>;
```

`pub mod tool_parser`（`crates/qaqh-gate/src/lib.rs:18`）是一个额外的公开模块。

`ProviderConfig` 的公开方法（`crates/qaqh-gate/src/types.rs`）：
`openai()` `:280`、`responses()` `:321`、`anthropic()` `:362`、
`with_opencode_headers()` `:402`、`with_retry()` `:410`、`with_stateful()` `:433`、
`with_stream_usage()` `:438`、`with_openrouter_compat()` `:444`、`with_tail_system_support()` `:453`。
公开自由函数：`normalize_reasoning_effort()` `:21`、`clamp_effort_to_allowlist()` `:39`。
公开常量：`EFFORT_LADDER` `:9`、`OPENCODE_CLIENT_ID` `:91`、`OPENCODE_CLIENT_VERSION` `:93`。

`RetryPolicy` 的公开面（`crates/qaqh-gate/src/transport.rs`）：4 个 `pub` 字段
（`:81-87`）、`Default` `:90`、`from_spec()` `:104`、`delay_for()` `:127`。

### 3.4 它依赖了本仓库哪些其他模块？能否独立抽成 SDK？

**内部依赖闭包 = 1 个 crate。**

- `crates/qaqh-gate/Cargo.toml:7-18` 的 `[dependencies]` 只有一行内部依赖：
  `qaqh-types = { path = "../qaqh-types" }`（`:8`）。其余全部是外部 crate
  （serde / serde_json / reqwest / tokio / futures / anyhow / log / httpdate /
  unicode-normalization）。
- `qaqh-types` **零内部依赖**（`crates/qaqh-types/Cargo.toml:7-18` 全是外部 crate）。
- 因此 `qaqh-gate` + `qaqh-types` 构成一个封闭的 2-crate 依赖闭包，**不含任何其他本仓库模块**。

**但有两处非代码耦合需要在抽取时处理**：

1. `crates/qaqh-gate/src/sse.rs:1-8` 记录了与 `qaqh-client/src/sse_decoder.rs` 的
   **刻意重复**（D3 决策）。因 `qaqh-client` 不在依赖闭包内，这不构成依赖，
   但抽取 SDK 后这 60 行重复会跨仓库存在。
2. `crates/qaqh-gate/src/lib.rs:47` 使用 `qaqh_types::QAQH_USER_AGENT`，
   而该常量定义于 `crates/qaqh-types/src/platform.rs`
   （经 `crates/qaqh-types/src/lib.rs:68` 重导出）。属于 `qaqh-types`，仍在闭包内。

**结论**：从依赖图角度，`qaqh-gate` 已经是可独立抽取的（`qaqh-types` + `qaqh-gate`
即为完整闭包）。是否存在非依赖图层面的耦合（构建脚本、feature 约定等），
本次分析**未找到**其他证据。

---

## 第 4 步：前后端协议

### 4.0 首要纠正：SSE 流的数量与命名

任务书称"现为 tools/messages/history 三条"。**这与当前代码不符。**

- v1 的三条 `/ringing/v1/events/{channel}` 全局流**已被硬切删除**：
  `crates/qaqh-domain/src/channel.rs:22-23` 注释"v1 的 `/ringing/v1/events/{channel}`
  路由已删除"；`crates/qaqh-client/src/v2_stream.rs:3` 注释"2026-09-24 硬切：v1 的三条
  `/ringing/v1/events/{channel}` 全局流被…取代"；
  `crates/qaqh-runtime/src/ringing/v2.rs:315-317` 注释"There is no per-channel filter
  any more"。
- 现存路由中**没有任何 `events/tools`、`events/messages`、`events/history`**
  （全仓 grep 该三串 → 未找到）。
- 负向测试确认已删：`crates/qaqh-daemon/src/axum_server.rs:633-635` 对
  `/ringing/v1/events/{tool,conversation,control}` 断言 404。

**当前恰好有 2 条 SSE 流**，不是 3 条（详见 §4.2）。

### 4.1 所有 HTTP 路由

#### daemon（`crates/qaqh-daemon/src/axum_server/axum_impl/mod.rs:141-187`，`build_router`）

| 方法 | 路径 | handler | 定义位置 |
|---|---|---|---|
| GET | `/health` | `health` | `control.rs:9` |
| GET | `/activity` | `activity` | `control.rs:18` |
| POST | `/ringing/v2/clients/open` | `handle_open_v2` | `v2.rs:73` |
| POST | `/ringing/v2/leases/renew` | `handle_renew_v2` | `v2.rs:125` |
| GET | `/ringing/v2/sessions/{session_id}/bootstrap` | `handle_bootstrap_v2` | `v2.rs:154` |
| GET | `/ringing/v2/sessions/{session_id}/team` | `handle_team_snapshot_v2` | `v2.rs:284` |
| GET | `/ringing/v2/sessions/{session_id}/events` | `handle_events_v2` | `v2.rs:453` |
| POST | `/ringing/v2/commands/{id}` | `handle_command_v2` | `v2.rs:869` |
| GET | `/ringing/v2/commands/{id}` | `handle_command_status_v2` | `v2.rs:1144` |
| POST | `/ringing/v2/sessions/{session_id}/driver/claim` | `handle_driver_claim_v2` | `v2.rs:656` |
| POST | `/ringing/v2/sessions/{session_id}/driver/release` | `handle_driver_release_v2` | `v2.rs:735` |
| GET | `/ringing/v2/sessions/{session_id}/approvals` | `handle_pending_approvals_v2` | `v2.rs:343` |
| GET | `/ringing/v2/sessions/{session_id}/timeline` | `handle_timeline_snapshot` | `timeline_api.rs:57` |
| GET | `/ringing/v2/content/{content_id}` | `handle_content_get` | `content.rs` |
| POST | `/ringing/v2/content` | `handle_content_upload` | `content.rs` |
| POST | `/ringing/v2/service/{method}` | `handle_service` | `service_api.rs` |
| GET | `/ringing/v2/sessions/{session_id}/timeline/events` | `handle_timeline_events` | `sse.rs:127` |
| POST | `/control/v1/stop` | `handle_stop` | `control.rs:39` |
| POST | `/control/v1/stop-if-idle` | `handle_stop_if_idle` | `control.rs:53` |
| * | fallback | `not_found` | `control.rs:35` |

中间件层（顺序见 `mod.rs:188-192`）：`RequestBodyLimitLayer(16 MiB)` →
`ConcurrencyLimitLayer(128)` → `TraceLayer` → `log_http_errors`（自定义，
非 2xx 记入 daemon 日志，`mod.rs:129-138`）。

#### webui-gateway（`crates/qaqh-webui-gateway/src/lib.rs:260-302`，`build_router`）

浏览器只经此代理，**从不直接持有 daemon bearer token 或 lease id**
（`webui/src/lib/transport.ts:5-9`）。

| 方法 | 路径 | handler |
|---|---|---|
| GET | `/` | `serve_index` |
| GET | `/assets/{*path}` | `serve_asset` |
| GET | `/__gateway/bootstrap.js` | `bootstrap_js` |
| POST | `/__gateway/session` | `create_session` |
| POST | `/__gateway/logout` | `logout` |
| GET | `/__gateway/sessions` | `list_sessions` |
| POST | `/__gateway/sessions/{session_id}/attach` | `attach_session` |
| POST | `/__gateway/approvals` | `list_approvals` |
| POST | `/__gateway/approvals/{id}` | `respond_approval` |
| POST/GET | `/__gateway/ringing/commands/{channel}` | `proxy_command` / `proxy_command_status` |
| GET | `/__gateway/ringing/content/{content_id}` | `proxy_content_get` |
| POST | `/__gateway/ringing/content` | `proxy_content_upload` |
| GET | `/__gateway/ringing/sessions/{session_id}/events` | `proxy_events`（`lib.rs:911`） |
| GET | `/__gateway/ringing/sessions/{session_id}/bootstrap` | `proxy_bootstrap` |
| GET | `/__gateway/ringing/sessions/{session_id}/timeline` | `proxy_timeline` |
| GET | `/__gateway/ringing/sessions/{session_id}/timeline/events` | `proxy_timeline_events`（`lib.rs:960`） |
| POST | `/__gateway/ringing/service/{method}` | `proxy_service` |
| * | fallback | `serve_spa` |

安全头中间件：`middleware::from_fn(security_headers)`（`lib.rs:302`）。

### 4.2 所有 SSE 流（当前共 2 条）

#### 流 A：canonical 事件流

- **端点**：`GET /ringing/v2/sessions/{session_id}/events`（`mod.rs:154-157`），
  网关代理 `GET /__gateway/ringing/sessions/{session_id}/events`（`lib.rs:283-286`）
- **handler**：`handle_events_v2`（`crates/qaqh-daemon/src/axum_server/axum_impl/v2.rs:453`）
- **查询参数**：`since_cursor`（`V2EventsQuery`，`v2.rs:69-71`），解析为
  `CursorToken::from_opaque`（`:472-475`）
- **SSE frame 构造**（`v2.rs:489-505`）：
  - `V2StreamItem::Event` → `id: "v2:{server_epoch}:{event_id}"`、`event: "ringing.event"`、
    `data: <RingingV2EventEnvelope<ProjectionPayload> 的 JSON>`（`:492-495`）
  - `V2StreamItem::Reset` → `event: "ringing.reset_required"`、`data: <RingingV2ResetRequired>`，
    **发完即 break**（`:500-505`）
- **保活**：`KeepAlive::new().interval(15s)`（`:510-511`），**无 keep-alive 文本**

#### 流 B：timeline 转录流

- **端点**：`GET /ringing/v2/sessions/{session_id}/timeline/events`（`mod.rs:181-184`），
  网关代理 `lib.rs:295-298`
- **handler**：`handle_timeline_events`（`crates/qaqh-daemon/src/axum_server/axum_impl/sse.rs:127`）
- **游标来源**（`sse.rs:161-166`）：HTTP 头 `last-event-id` → query `last_event_id` →
  query `last-event-id`，默认 `""`
- **游标解析** `parse_timeline_cursor`（`sse.rs:6-30`）：严格形状 `{epoch}:timeline:{seq}`；
  形状不符或 seq 非法则**回落 0 并 warn**（`:24-26`，注释解释两类失败为何都要告警）
- **SSE frame 构造**（`sse.rs:68-89`）：
  - `id: "{epoch}:timeline:{entry.timeline_seq}"`（`:85`）
  - `event: "timeline.entry"`（`:86`）
  - `data:` = `{schema, version, server_epoch, session_id, entry}`（`:73-83`）
    其中 `version` **必须**是 `RINGING_V2_VERSION`——注释记录写死 `1` 曾导致
    "每一帧都被判 protocol violation…timeline 流 1s 重连死循环、transcript 永远无法渲染"（`:75-78`）
- **`ringing.stream_terminated` 帧**：仅在两处产生——
  (a) 测试注入（`sse.rs:50-53`）；(b) **广播 Lagged**（`:229-246`），
  带 `code: "lagged"`、`session_id`、`skipped` 计数
- **保活**：`KeepAlive::new().interval(15s).text("keep-alive")`（`sse.rs:254-258`）

### 4.3 每种事件的 schema、谁产生、谁消费、有无序号/event id

#### 流 A 的事件载荷：`RingingV2EventEnvelope<P>`

定义 `crates/qaqh-ringing/src/v2/types.rs:106-137`：

```rust
pub struct RingingV2EventEnvelope<P> {
    pub schema: String,
    pub version: u32,
    pub server_epoch: String,
    #[serde(rename = "session_id")] pub session_id: String,
    pub event_id: String,
    pub stream_key: RingingV2StreamKey,
    pub delivery: RingingV2Delivery,
    pub cursor: Option<CursorToken>,
    pub log_id: Option<String>,
    pub fact_seq: Option<u64>,
    pub projection_index: Option<u16>,
    pub revision: Option<u64>,
    pub causation_id: Option<String>,
    pub correlation_id: Option<String>,
    pub ts_ms: Option<u64>,
    pub payload: P,
}
```

**序号 / event id：有，且按 delivery 分档**（`types.rs:158-191` 的 `validate`）：

| delivery | 序号保证 | 必填字段 |
|---|---|---|
| `Reliable` | `cursor` + `log_id` + `fact_seq` + `projection_index` 四者必须自洽（`:159-171`，不符报 `reliable_cursor_mismatch`） | `revision` 也必填 |
| `Replaceable` | **禁止** cursor/projection_index；只允许 `revision`（`:172-179`） | `revision` 必填 |
| `Ephemeral` | cursor / log_id / fact_seq / projection_index / revision **全部禁止**（`:180-189`） | 无 |

`stream_key` 取值（`types.rs:100-102`）：`Channel(RingingChannel)` 或
`Resource { kind, id }`。单流设计下客户端自行 demux（`v2.rs:449-452` 注释：
"事件带 `stream_key`，客户端自行 demux；per-channel 的 `events/{channel}` 已硬切删除"）。

`ProjectionPayload` 的变体面（`crates/qaqh-session/src/session_fact_v2/projection_event.rs:289-810`）
覆盖 40+ 个变体，含 `TurnStarted :563`、`AssistantBlockSealed :569`、
`ToolCallDeclared :578`、`ToolFinished :586`、`TurnFinished :595`、`TurnInterrupted :602`、
`CompactionApplied :609`、`InteractionRequested :704`、`InteractionResolved :712`、
`InteractionExpired :722`、`DriverChanged :728`、`SessionRecovered :734`、
`SubagentSpawned :740`、`SubagentFinished :746`、`TeamDelta::*`（`:496-531`）等。

**产生者**：worker 侧发 `DomainEvent`/`TimelineIntent`（`crates/qaqh-runtime/src/agent/types.rs:413-432`
的 `Emitter` trait）→ `PacedEmitter`（`crates/qaqh-runtime/src/agent/paced_emitter.rs:15`）
→ `WriterEvent` channel → `actor::run_inprocess_event_reader`
（`crates/qaqh-runtime/src/actor.rs:20`）→ `RingingHub` / `V2ProjectionHub`
（`crates/qaqh-runtime/src/ringing/v2.rs`，作为 `ProjectionSink` 安装于
`crates/qaqh-daemon/src/server.rs:216-219`）。

**消费者**：`qaqh-client`（TUI/desktop 壳）与 webui-gateway → 浏览器。
客户端解码在 `crates/qaqh-client/src/v2.rs:269`（`ringing.reset_required` 分支）、
`crates/qaqh-client/src/sse_decoder.rs`。

#### 流 B 的事件载荷：timeline entry

schema（`sse.rs:73-83`）：`{schema, version, server_epoch, session_id, entry}`，
`entry` 类型 `qaqh_domain::TimelineEntry`（含 `timeline_seq`、`turn_id`、`round_num`、`event`）。

**序号 / event id：有**——`id` = `{epoch}:timeline:{timeline_seq}`（`sse.rs:85`），
`timeline_seq` 是全局单调序号。客户端断线重连时把上一帧的 `id` 作为
`Last-Event-ID` 或 `last_event_id` 参数回传。

**产生者**：`TimelineAppender`（`crates/qaqh-runtime/src/timeline.rs`，经
`crates/qaqh-runtime/src/lib.rs:21` 重导出）→ `RingingHub.timeline_live` 广播
（`crates/qaqh-runtime/src/ringing/hub.rs:268`）。

**消费者**：webui 的 `SessionStore`（`webui/src/session/store.ts:123`
`addEventListener("timeline.entry", ...)`）。

### 4.4 多条流之间有无顺序依赖？

**有，但方向单一：流 A 与流 B 是同一份 canonical fact 的两个不同投影，彼此独立推进，
不存在交叉顺序依赖；顺序保证各自流内。**

- 同一 `SessionFact` 会同时产出 projection event（→ 流 A）与 timeline intent（→ 流 B）。
  两者的对应关系由 `crates/qaqh-session/src/session_fact_v2/projection.rs` 声明
  （例如 `:62` `FactPayload::InteractionExpired(_) => CONTROL_ONLY`、
  `:66` `FactPayload::SessionRecovered(_) => CONTROL_META_TEAM`），
  即**并非每个 fact 都进两条流**。
- 流 A 的顺序锚点是 `(fact_seq, projection_index)`（`crates/qaqh-ringing/src/v2/cursor.rs:20-25`）；
  流 B 的顺序锚点是 `timeline_seq`。**两者是不同的序列空间**，代码中**未找到**任何
  跨流序号比较或跨流屏障。
- 前端的实际做法是**只用流 B 做 transcript 渲染**，流 A 用于 projection 快照/审批/团队态
  （`webui/src/session/store.ts:120` 连 timeline，`:123` 只监听 `timeline.entry`；
  另有 `connectEvents()` 走 `/events`，见 `webui/src/lib/transport.ts:182-185`）。

### 4.5 断线重连时客户端如何补齐？服务端是否保留事件缓冲？

#### 流 A（canonical 事件）

- **客户端**：`since_cursor` 查询参数（`v2.rs:472-475`）——一个不透明
  `CursorToken`（`CursorToken::from_opaque`）。
- **服务端补齐**：`V2ProjectionHub::subscribe`（`crates/qaqh-runtime/src/ringing/v2.rs:318-413`）
  - 校验 cursor 的 `log_id` 是否匹配：不匹配 → `ResetReason::LogIdMismatch`（`:348-360`）
  - cursor 的 `fact_seq` 是否超前于本地 `last_fact_seq`：超前 → `ResetReason::UnknownFact`（`:361-373`）
  - 正常 → 从 committed canonical log **replay**（`:384-393` `replay_after`）
  - **不重放 replaceable 历史**：`:394-402` 注释"Replaceable history is never replayed.
    Reconnect/rebaseline gets only the latest value for each stable identity."
- **服务端缓冲**：
  - 有界 replay 来自磁盘 canonical log（可无限回溯至 committed 前缀）
  - live 通道是 `tokio::sync::broadcast`，容量 `LIVE_CAPACITY = 1024`
    （`crates/qaqh-runtime/src/ringing/v2.rs:30`、`:611`）
  - 溢出时 `RecvError::Lagged` → 发 `V2StreamItem::Reset { reason: ReplayOverflow }`
    （`:542-552`），**无逐条补偿**

#### 流 B（timeline）

- **客户端**：`Last-Event-ID` 头或 `last_event_id` 参数（`sse.rs:161-166`），
  值形如 `"{epoch}:timeline:{seq}"`。
- **服务端补齐**（`sse.rs:167-195`）：
  - `after = parse_timeline_cursor(...)`（`:167`）
  - `replay = state.hub.timeline_replay_since(&session_id, after)`（`:169`）
  - `replayed: HashSet<u64>` 记录已回放的 seq（`:170`）
  - 先逐条吐 replay，再转入 live 循环（`:180-195`）
- **服务端缓冲**：`crates/qaqh-runtime/src/ringing/hub.rs:31`
  `LIVE_BROADCAST_CAPACITY = 1024`
- **live 投递判据** `should_deliver_timeline_live`（`sse.rs:106-125`）：
  1. 逐事件复查 seed 归属（`:115-121`）——注释（`:92-104`）记录 BUG-2026-09-13-10：
     旧实现只在建流时查一次 `owns_seed`，长连接内 seed 级吊销后仍继续投递（数据暴露窗口）
  2. replay 窗口与去重：`!(seq <= after || replayed.contains(&seq))`（`:124`）
- **溢出处理**：`RecvError::Lagged(skipped)` → 记 warn + 下发
  `ringing.stream_terminated { code: "lagged", skipped, message: "server event buffer
  overflow; reconnect to re-baseline" }` 然后 break（`sse.rs:229-246`）。
  **不逐条补偿**，要求客户端重新 baseline。
- **前端对 `stream_terminated` 的响应**：清空 cursor、重置重试计数、
  `resnapshot()` 后重连（`webui/src/session/store.ts:134-139`）。

### 4.6 同一会话被多个前端同时连接时会怎样？

**广播，不是抢占；但写入侧有 driver seat 互斥。**

#### 读侧：广播给所有订阅者

- 流 A：`V2ProjectionHub` 每会话一个 `broadcast::Sender`（`crates/qaqh-runtime/src/ringing/v2.rs:611`
  `let (live_tx, _) = broadcast::channel(live_capacity);`），
  `subscribe` 调用 `state.live_tx.subscribe()`（`:333`）——**每个连接一个独立 receiver，
  各自收到全部事件**。
- 流 B：`RingingHub.timeline_live` 同样是 `broadcast::channel`
  （`crates/qaqh-runtime/src/ringing/hub.rs:268`），
  `subscribe_timeline`（`:804`）返回 `broadcast::Receiver`。
- **无"同一会话只允许一个前端"限制**：`handle_events_v2` / `handle_timeline_events`
  只校验 (a) bearer token（`is_authorized`）、(b) lease 存在（`require_v2_lease` /
  `get_session_id`）、(c) `leases.owns_session(client_session_id, session_id)`。**多个
  不同 `client_session_id` 可以同时 attach 同一 seed 并各自建流。**

#### 写侧：driver seat 单持有者

- `POST /ringing/v2/sessions/{session_id}/driver/claim`（`v2.rs:656`）：
  若已有 holder 且 holder 的 lease 仍活跃 → 返回
  `{ accepted: false, reason: "driver_busy" }`（`:693-703`）；holder 是自己 →
  `"already_holder"`（`:681-692`）；holder 的 lease 已过期 → 走 `forward_driver_command`
  并带 `stale_holder`（`:705-719`）。
- 席位由 canonical `DriverChanged` fact 承载（`v2.rs:517-527` `canonical_driver_state`
  注释"Canonical driver seat, or `None` when the session has no `DriverChanged` fact"）。
- 过期席位自动回收：`reclaim_dead_driver_seats`（`v2.rs:65` 重导出），
  周期任务每 3s 调一次（`crates/qaqh-daemon/src/server.rs:327`）。
- 哪些命令受 driver 门控：`driver_gated` / `driver_admission`（`v2.rs:799`、`:825`）。

#### 命令幂等（防重复执行）

- `PendingCommandStore`（`crates/qaqh-runtime/src/ringing/pending_store.rs:21`），
  上限 4096 条（`:92`），可按 `command_id` 查 receipt（含 terminal payload）。
  注释（`:19`）："已 accepted 命令的幂等表（有界 TTL；accepted 后断线重试不得重复执行）"。
- 同一前端重试同一 `command_id` 幂等；不同前端用不同 `command_id` 则会各自执行。

#### 陈旧身份截断

- 同一 `client_instance_id` 重新 `open` 会签发新的 `client_session_id`，
  **旧 cs 的归属被清除**，其长连接在下一个事件处被截断
  （`sse.rs:358-408` 测试 `timeline_live_is_truncated_after_renegotiation`）。

---

## 第 5 步：多会话与 daemon 生命周期

### 5.1 会话并行是怎么实现的？

**OS 线程 + 有界 mpsc channel，每会话一线程。没有 async task，没有全局串行锁。**

| 机制 | 位置 |
|---|---|
| 线程模型 | `crates/qaqh-runtime/src/actor.rs:3-6`："Both main session loops and subagent loops run on daemon threads using the typed `WorkerCommand` / `WriterEvent` channels. Each actor owns its thread and its per-actor workspace state (`qaqh-workspace` thread-locals), so session and subagent actors can run concurrently **without a process-wide serialization lock**" |
| 线程创建 | `crates/qaqh-runtime/src/registry.rs:1201-1217`，`std::thread::Builder::new().name(format!("qaqh-session-{actor_session}"))` |
| 每会话通道 | `LoopChannels::new()`（`crates/qaqh-runtime/src/agent/loop_core.rs:128-145`）：`cmd` sync_channel(4096) + `event` sync_channel(16384) + `CancelToken` + `writer_dead: Arc<AtomicBool>` |
| 事件消费线程 | 每会话另起 reader 线程（`registry.rs:1160-1168`），跑 `actor::run_inprocess_event_reader`（`actor.rs:20`） |
| 隔离手段 | `qaqh-workspace` 的 **thread-local** actor context：`ACTOR_WORKSPACE` / `ACTOR_SESSION` / `ACTOR_CANCEL`（`crates/qaqh-workspace/src/lib.rs:235-239`），由 `set_actor_context`（`:247-251`）安装，`clear_actor_context`（`:253`）清理 |
| 并发上限 | `MAX_PARALLEL_TOOL_WORKERS = 4`（单会话内工具并行，`crates/qaqh-runtime/src/agent/tool_runtime.rs:43`）；HTTP 层 `MAX_CONNECTIONS = 128`（`crates/qaqh-daemon/src/axum_server/axum_impl/mod.rs:72`） |
| 会话锁 | `SessionManager` 有 per-session `Arc<Mutex<()>>`（`crates/qaqh-session/src/manager.rs:1357` `session_lock`），用于元数据读改写事务 |
| 单实例锁 | `acquire_single_instance()`（`crates/qaqh-daemon/src/server.rs:164`），daemon 进程级 |

**子代理**也是同构线程：`spawn_subagent_inprocess`（`registry.rs:1024`）。

### 5.2 全局共享可变状态

按进程级 vs 线程级分类。以下为全仓 `static`/`thread_local!` 扫描结果（排除测试）：

#### 进程级 `static`（跨会话共享）

| 状态 | 位置 | 类型 |
|---|---|---|
| `AgentRegistry` | `crates/qaqh-runtime/src/service.rs:22` | `Arc<Mutex<AgentRegistry>>`（**最大的一处**：2621 行文件） |
| `QaqhService.team_stores` / `board_stores` | `service.rs:30`、`:33` | `Arc<Mutex<HashMap<String, Arc<Mutex<..>>>>>` |
| `SessionManager` 单例 | `crates/qaqh-session/src/manager.rs:21` | `OnceLock<Arc<SessionManager>>` |
| `PROJECTION_SINK` | `crates/qaqh-session/src/projection/sink.rs:16` | `OnceLock<Arc<dyn ProjectionSink>>` |
| canonical `WORKSPACE` | `crates/qaqh-workspace/src/lib.rs:233` | `RwLock<String>` |
| `CURRENT_SESSION` | `crates/qaqh-workspace/src/lib.rs:222` | `Mutex<Option<String>>` |
| `CANCEL` | `crates/qaqh-workspace/src/lib.rs:221` | `AtomicBool`（**进程级**，但 `:1439-1443` 注释说明会话级取消已改走下面这张表） |
| `SESSION_CANCELS` | `crates/qaqh-workspace/src/lib.rs:340` | `LazyLock<Mutex<HashMap<String, bool>>>`（会话键控取消） |
| 工具注册表 | `crates/qaqh-workspace/src/runtime.rs:31` | `OnceLock<Mutex<ToolManager>>` + 多个 `thread_local!`（`:27`、`:40`、`:45`、`:53`） |
| 审计链 + 追加锁 | `crates/qaqh-workspace/src/audit/v2.rs:45`、`:42`；`audit/mod.rs:38`、`:53`、`:357` | `LazyLock<Mutex<..>>` / `Mutex<()>` / `LazyLock<Mutex<HashSet<PathBuf>>>` |
| 文件状态/缓存/偏移 | `crates/qaqh-workspace/src/file_state.rs:25,42,55`；`file_cache.rs:19,20` | `OnceLock<Mutex<..>>` |
| 待应用变更 | `crates/qaqh-workspace/src/pending.rs:28` | `LazyLock<RwLock<HashMap<String, PendingApply>>>` |
| 日志追加锁 | `crates/qaqh-workspace/src/journal.rs:63` | `Mutex<()>` |
| todo 锁 | `crates/qaqh-workspace/src/todo/model.rs:11` | `Mutex<()>` |
| 图片能力缓存 | `crates/qaqh-workspace/src/runtime.rs:557` | `Mutex<Option<ImageCaps>>` |
| 消息 WAL thread-local | `crates/qaqh-message/src/wal.rs:892` | `thread_local!` |
| 旧 writer 锁 | `crates/qaqh-message/src/legacy_writer.rs:12` | `Mutex<()>` |
| tokenizer | `crates/qaqh-types/src/token.rs:6` | `OnceLock<tokenizers::Tokenizer>` |
| LSP manager | `crates/qaqh-lsp/src/lib.rs:29` | `OnceLock<RwLock<Arc<LspManager>>>` |
| LSP/MCP 子进程 PID 表 | `crates/qaqh-lsp/src/adapter.rs:35`；`crates/qaqh-mcp/src/adapter.rs:80` | `OnceLock<StdMutex<BTreeMap<String, u32>>>` |
| MCP manager | `crates/qaqh-mcp/src/lib.rs:92` | `OnceLock<RwLock<Arc<McpManager>>>` |
| MCP 连接通知表 | `crates/qaqh-mcp/src/connection.rs:66` | `OnceLock<StdMutex<BTreeMap<String, Weak<ServerConnection>>>>` |
| subagent host 槽 | `crates/qaqh-subagent/src/host.rs:436`、`:467`、`:549` | `OnceLock<Arc<dyn ..>>` / `OnceLock<Mutex<Option<Arc<dyn SubagentHost>>>>` |
| 配置热更新通道 | `crates/qaqh-config/src/watch.rs:19`、`:22` | `OnceLock<watch::Sender<..>>` + `OnceLock<Mutex<..>>` |
| 沙箱能力探测 | `crates/qaqh-sandbox/src/capability.rs:31` | `OnceLock<BubblewrapSupport>` |
| prompt 缓存 | `crates/qaqh-runtime/src/agent/prompt.rs:12`、`:15`、`:19` | `OnceLock<String>` |
| 系统 PATH / shell 探测 | `crates/qaqh-runtime/src/registry.rs:30`；`qaqh-workspace/src/exec/shell.rs:35,37,39,370,371` | `OnceLock<..>` |
| 工作区分组单例 | `crates/qaqh-session/src/grouping.rs:89` | `OnceLock<WorkspaceStore>` |

#### 会话级可变状态（但被 `Mutex` 全局包裹）

- `AgentRegistry.instances`（`registry.rs:316`）、`last_spawn`（`:325`）、
  `residency`（`:333`）、`quota_ledgers`（`:338`）、`outbound_attempts`（`:348`）、
  `armed_collectors`（`:350`）、`agent_catalog`（`:329`）、`supervisor`（`:327`）——
  全部在 `Arc<Mutex<AgentRegistry>>` 内，**即 registry 是全局单点串行化瓶颈**：
  任何会话的 spawn/send/close/quota 查询都要抢同一把锁。
- `AppState.leases` / `driver_watch` / `pending`（`crates/qaqh-daemon/src/axum_server/axum_impl/mod.rs:86`、`:92`、`:93`）——
  各自 `Arc<Mutex<..>>`。测试代码里对它们统一用
  `.lock().unwrap_or_else(|e| e.into_inner())` 的 poison 容错模式（如 `v2.rs:100-103`、`sse.rs:117-120`）。

### 5.3 前端全部断开后 daemon 是否继续运行 agent？

**是，继续运行。** 依据：

1. **无"连接数归零即退出"逻辑**（§2.5 已述）。
2. **agent 与会话线程不感知 SSE 连接**：会话线程由 `spawn_session_inprocess` 创建，
   生命周期由 `AgentRegistry.instances` 与 `WorkerLiveness` 决定；
   SSE 只是 `broadcast::Receiver` 的消费者，断开只导致 `send` 返回 `Err` 后
   `tx.send(..).await.is_err()` break（`sse.rs:192-194`、`:225-227`）。
3. **唯一的自动停止路径是 idle 卸载**（`crates/qaqh-daemon/src/server.rs:266-301` +
   `registry.rs:1839`），且默认阈值来自配置；配置为 `0` 时完全禁用（`server.rs:277-278`）。
4. **显式停止**仅 `/control/v1/stop`、`/control/v1/stop-if-idle` 与 OS 信号。
   `/control/v1/stop-if-idle` 用 `service.has_active_work()` 判断（`control.rs:60`），
   有活跃工作时返回 409 而非停止。

### 5.4 授权请求 / AskUser 在无前端时如何挂起与恢复？

见 §2.6（完整引用链）。要点复述：

- **挂起**：内存态 `TurnEngine.suspended`（`engine_turn.rs:225`）+
  磁盘态 canonical `InteractionRequested` fact（`engine_turn.rs:288-388`），
  正文以 `sha256` content ref 外置（`:353-367`）。
- **恢复**：`GET /ringing/v2/sessions/{id}/approvals`（`mod.rs:170-173`）从
  canonical projection 读回（`v2.rs:362-398`），答复经
  `POST /ringing/v2/commands/{id}` 回到 `handle_permission_resolved` /
  `handle_ask_response` / `handle_plan_response` / `handle_ask_dismiss`
  （`engine_turn.rs:649/763/847/981`），再 `TurnEngine::resume`（`:546`）。
- **无超时**：`InteractionExpiryReason` 的生产写入点只有
  `crates/qaqh-session/src/actor.rs:626-631`，reason 恒为 `TurnCancelled`。
  **未找到**任何时间驱动的 interaction 过期。若 daemon 在悬挂期间重启，
  canonical `InteractionRequested` 仍在（可被 `/approvals` 读到），
  但内存中的 `suspended` turn 已丢失，**该 turn 不会被续跑**——
  代码中**未找到**把已悬挂 turn 重新接回 `TurnEngine.suspended` 的路径。
- **first-answer-wins**：`turn_actor.rs:232`；取消不得覆盖已到达的答复
  （`crates/qaqh-session/src/actor.rs:640-643`）。

---

## 第 6 步：长上下文与历史

### 6.1 历史消息的存储格式

**`messages.jsonl`——每行一条 `Message` 的 JSON，append-only。**
定义与说明：`crates/qaqh-session/src/store/mod.rs:1-8`（模块文档列出会话目录四个产物：
`meta.json` / `messages.jsonl` / 中央 `index.json`）。

写入路径：
- `crates/qaqh-session/src/store/mod.rs:45-59`（单条 append）
- `crates/qaqh-session/src/store/mod.rs:61-77`（批量 append）
- `crates/qaqh-session/src/store/mod.rs:79-95`（**全量重写**——注释标注用于
  undo / image repair aftermath）

`msg_id`：每会话单调递增。`max_persisted_msg_id` 于
`crates/qaqh-session/src/store/mod.rs:97-109`；`count_lines`（不解析 JSON）于 `:285-287`。

`meta.json` 原子替换写：`store/mod.rs:21-34`（tmp → `flush` → `sync_all` → `rename`）。

**compact 摘要也是 `messages.jsonl` 里的真实行**，通过
`meta.compact_covered_through_msg_id` 水位标记被遮盖的前缀：
`crates/qaqh-types/src/session.rs:85-88`、`crates/qaqh-session/src/manager.rs:25-32`。
`crates/qaqh-runtime/src/agent/engine_compact.rs:239` 注释："messages.jsonl 仍是压缩真相"。

**没有 `compact-context.json`**：`README.md:42-43` 明确"没有 `compact-context.json`"；
`crates/qaqh-message/src/store.rs:166-168` 注释"There is no separate compact-context file"。

**L2 WAL**：`crates/qaqh-message/src/wal.rs`，由
`MessageStore::enable_wal`（`crates/qaqh-message/src/store.rs:382`）opt-in，
`wal_checkpoint`（`:400`）标记已应用水位。注释（`:188-192`）："logs message-bearing persist
ops at enqueue time so a process death between `flush_meta` and the host-side drain loses nothing"。

### 6.2 加载接口是否支持分页 / 游标

#### HTTP 层：`GET /ringing/v2/sessions/{id}/timeline`——**支持排除式游标分页**

- 查询参数 `TimelineQuery { before_index: Option<usize>, limit: Option<usize> }`
  （`crates/qaqh-daemon/src/axum_server/axum_impl/mod.rs:101-111`）。
  `before_index` 文档（`:103-108`）："**排他**游标：返回全局序号 **小于** 它的那一页。
  `None` = 最新一页"，并记录该字段是从 `before_turn`（turn_id）改来的
  BUG-2026-09-15-05，原因是 turn_id 与归档下标都不稳定。
- 默认页大小 `TIMELINE_PAGE_LIMIT = 30`（`mod.rs:73`），上限 200
  （`timeline_api.rs:100`）。
- 分页规划 `page_plan`（`timeline_api.rs:35-53`）：
  - `end = before_index.unwrap_or(total).min(total)`（`:42`）
  - `start = end.saturating_sub(limit)`（`:43`）
  - `from_archive = start < window_base`（`:47`）——**整页要么全走常驻窗口（零 I/O），
    要么全走归档**，注释（`:44-46`）解释"不跨两个来源拼一页"的理由：两套 id/序号算法不同
- 响应体（`timeline_api.rs:148-157`）：`{schema, version, server_epoch, session_id,
  snapshot: {watermark, turns}, has_more, total_turns, truncated_before}`
- `has_more` 的结构性保证：空页绝不宣称还有更多
  （`:142-143`，注释指向 BUG-2026-09-13-18）
- `truncated_before` 的语义 `window_metadata`（`timeline_api.rs:15-19`）：
  `total_turns = persisted.unwrap_or(materialized).max(materialized)`；
  `truncated_before = persisted > materialized`。注释（`:11-14`）明确**不能用 `has_more`
  表达**，否则客户端会反复请求永远为空的页。
- 归档单页上限：`archive_turn_page` 的 **capped** 返回值（`timeline_api.rs:109`、`:119-120`）

#### 会话管理层（非 HTTP）

- `SessionManager::load_recent_for_projection(session_id, recent)`（`manager.rs:316`）——
  带 `recent` 参数，走 `bounded_read::read_messages_tail`（`:329`）
- `SessionManager::load_archive_tail(session_id, recent)`（`manager.rs:358`）
- 尾部反向读取器 `read_messages_tail`（`crates/qaqh-session/src/store/bounded_read.rs:126`）：
  超读 `max_messages + max_messages/4 + 16` 行吸收损坏行（`:130-131`），
  损坏行过多时扩窗重读一次、上限 64×（`:147-150`）

### 6.3 上下文压缩 / 截断策略在哪

| 策略 | 位置 | 说明 |
|---|---|---|
| 软阈值自动压缩决策 | `crates/qaqh-runtime/src/agent/engine_turn.rs:185-207` `compact_preflight` | 4 态：`None` / `Suppressed` / `Compact` / `ForcedCompact` |
| 硬窗口 | `engine_turn.rs:216-218` `hard_context_limit` | 优先 `cfg.context_window`，回落 `cfg.context_limit` |
| 触发点 | `engine_turn.rs:1332-1389`（`prepare_gate_snapshot` 内） | 压缩后重建 `messages` 并重估（`:1368-1371`），仍超阈值则 warn（`:1376-1386`） |
| 端点超限兜底 | `engine_turn.rs:1643-1683` | `is_context_overflow_error`（`:156`）匹配 + `MAX_CONTEXT_OVERFLOW_RECOVERIES = 2`（`:134`），强制压缩后重试同一轮 |
| 超限识别词表 | `engine_turn.rs:~140-151` `CONTEXT_OVERFLOW_MARKERS` | 含 `"reduce the length of the messages"`（`:150`） |
| 后台压缩任务 | `crates/qaqh-runtime/src/agent/compaction_port.rs`；`CompactionPort` 于 `loop_core.rs:193` | 异步，主循环每次 `recv_timeout` 超时轮询（`loop_core.rs:447`） |
| compact 应用点 | `crates/qaqh-runtime/src/agent/plugins/engine_compact.rs`（`build_prompt_and_meta :83`、`apply_result :274`） | 摘要 append 到 `messages.jsonl` |
| 摘要行识别 | `crates/qaqh-message/src/store.rs:18` `is_compaction_summary` | transcript 侧显式过滤（`manager.rs:333`、`:365`） |
| 摘要前置 | `crates/qaqh-message/src/store.rs:956-989` `flat_in_write_order` | 摘要虽拿到新鲜 msg_id（更大），但逻辑上要在所有保留消息之前（`:957-960` 注释） |
| 上下文 revision 保护 | `crates/qaqh-message/src/store.rs:174-176` `context_revision` | 后台压缩结果只在 generation 匹配时生效 |
| 单条工具结果截断 | `crates/qaqh-types/src/tool_result.rs:50` `TOOL_MODEL_MAX_CHARS` / `TOOL_SUMMARY_MAX_CHARS` | 进模型前的字符上限 |
| 续写中的视图变换 | `crates/qaqh-runtime/src/agent/engine_turn.rs:1410-1413` `apply_continuation_view` | `StripTrailingReasoning` 剥思考链；`ContinuePartialText` 保留原文+补全提示 |
| 传输层请求快照 dump | `engine_turn.rs:50-60` `dump_request_log` | `QAQH_REQUEST_LOG=1` 时写 `request-log.jsonl`，默认关闭 |

**"两条读者分野"是显式设计**：`crates/qaqh-session/src/manager.rs:338-354` 的长注释说明——
模型读 compact 视图（`load_recent_for_projection`），人类 transcript 读 append-only 归档
（`load_archive_tail`），"混用会同时坏两件事"。

### 6.4 前端渲染的是全量还是分页？最近消息与旧消息的加载路径是否分开？

**是分页的，且两条路径明确分开。**

- **页大小**：`SNAPSHOT_LIMIT = 50`（`webui/src/session/store.ts:23`）、
  `PAGE_LIMIT = 30`（`:24`，注释"spec §14.2 limit=30"）
- **最近消息路径**：`SessionStore.resnapshot()`（`store.ts:224-234`）
  → `transport.timelinePage(seed, "?limit=50")`（**不带 `before_index`**，`:226`）
  → `applySnapshot`（`webui/src/session/reducer.ts:421-449`）——**整体替换**
  `draft.turns = {}` / `draft.slots = []`（`:432-433`）后逐回合重建
  （`:434-438`）
- **旧消息路径**：`SessionStore.loadOlder()`（`store.ts:377-394`）
  → `transport.timelinePage(seed, "?limit=30&before_index=" + state.oldestIndex)`（`:383`）
  → `prependPage`（`reducer.ts:452-487`）——**前插** `draft.slots.unshift(...)`（`:478`），
  按 `turn_index` 去重（`:467-472`，注释"后端明示 turn_id 会复用，不能当稳定键"）
- **触发阈值**：`shouldLoadOlder` = `hasMore && !loading && scrollTop < viewportHeight * 1.5`
  （`webui/src/session/pagination.ts:21-23`）
- **两条路径的差异不止替换 vs 前插**：翻页路径额外做 DOM 测量 + 窗口淘汰 + 滚动补偿
  （`webui/src/session/SessionView.tsx:42-63`：`querySelectorAll("[data-slot-key]")`
  全量测 `offsetHeight` → `evictForWindow` → `setPlaceholderHeight` → `anchorScrollTop`）
- **服务端游标权威语义**：`before_index` 是**排他**游标（`timeline_api.rs:23`），
  且服务端会回填 `turn_index` 供客户端作下次游标（`:134-137`，注释"turn_id 会复用，当不了游标"）

---

## 第 7 步：前端

### 7.0 两个前提修正

1. **框架不是 React，是 SolidJS 2.0-rc。**
   - `webui/package.json:14` `"@solidjs/web": "2.0.0-rc.9"`、`:20` `"solid-js": "2.0.0-rc.9"`；
     **package.json 内无任何 react 依赖**
   - `webui/package.json:5` description 自述 `"Solid 2 rc + Vite + TS strict"`
   - `webui/vite.config.ts:2` `import solid from "@solidjs/vite-plugin"`；`:44` `plugins: [solid(), ...]`
   - `webui/tsconfig.json:8` `"jsxImportSource": "@solidjs/web"`
   - `webui/src/index.tsx:1` `import { render } from "@solidjs/web"`
   - 全 `webui/` grep `react|React|useSyncExternalStore|useReducer|zustand|useMemo|useCallback|useState|useEffect`
     → **No matches found**
   - `git show HEAD:webui/package.json` 同样是 Solid 2，**不是工作区改动造成的**
   - 因此任务书点名的 `memo` / `React.memo` / `useMemo` / `useCallback` / keys **均未找到**
     （Solid 无这些 API）。下文以 Solid 对应物作答。
2. **没有 WinUI3 前端。** `justfile:7-9` 明确："Windows 桌面层（WinUI3 壳 / installer /
   updater）已拆分为独立仓库 `F:\qaqh-winui-app`；本仓库只保留跨平台后端核心与公共 SDK。"
   全仓 **未找到** 任何 C#/XAML/WinUI 文件。
   本仓库内的前端只有 `webui/`（SolidJS）+ `crates/qaqh-client`（TUI/desktop 壳共用的
   HTTP/SSE 客户端库，`crates/qaqh-client/Cargo.toml` description）。

**git 工作区状态**：`webui/` 处于大重构中途——`git status --porcelain` 显示
旧文件 ` D`（已删除，工作区未恢复）与 `??`（新增未跟踪目录）并存。
任务书点名的以下文件**在磁盘上不存在**：`webui/src/App.tsx`、
`webui/src/components/Settings.tsx`、`webui/src/components/TodoTicker.tsx`、
`webui/src/lib/protocol.ts`、`webui/src/lib/ringing.ts`、`webui/src/lib/safe-url.ts`、
`webui/src/lib/streaming-md.ts`、`webui/src/lib/transcript.ts`、`webui/src/state.ts`、
`webui/src/perf/PerfHarness.tsx`、`webui/src/styles.css`、
`webui/tests/rendering-safety.test.ts`、`webui/tests/streaming-md.test.ts`、
`webui/tests/transcript-pagination.test.ts`。
`webui/` 磁盘实存 30 个源文件（排除 `node_modules` 与 `out/`）。

### 7.1 渲染相关的主要组件

| 组件 | 定义位置 | 渲染内容 |
|---|---|---|
| `App` | `webui/src/app/App.tsx:30` | 应用壳；`:137` `<For each={tabs()}>` 逐标签渲染，`:140` `<Show when={tab.id === activeId()}>` **仅活动标签挂载 DOM**；活动列依次 `SessionView`/`ApprovalStack`/`ThinkingChain`/`Composer`（`:142-158`） |
| `ConnectionStatus` | `webui/src/app/App.tsx:172` | 连接状态；connected 时不渲染（`:177-187`） |
| `SessionView` | `webui/src/session/SessionView.tsx:13` | 消息滚动区；`:85` `#messages`；`:97` `<For each={state().slots}>`；`:106` 每 slot 内 `<TurnView>` |
| `TurnView` | `webui/src/turn/TurnView.tsx:116` | 单回合：用户气泡 `:134`、运行中 step 流 `<For each={turn().steps}>` `:139`、折叠行 `:160-168`、最终作答 `<Markdown text={answerText} revision={turn().answerStepId ?? ""}/>` `:171-175` |
| `Timeline`（内部） | `webui/src/turn/TurnView.tsx:50` | `createMemo` 合并 steps+waits 并按时间排序（`:51-62`），`<For each={items()}>`（`:79`） |
| `StepRow` | `webui/src/tools/StepRow.tsx:158` | 工具调用行；`:180-193` `Switch`/`Match` 状态图标；`:212-214` `Collapse` 包 `ToolDetail` |
| `ToolDetail` | `webui/src/tools/StepRow.tsx:69` | 展开详情：入参表 `:90-109`、输出头 `:111-121`、`DiffList` `:122-124`、`progressTail` `:125-130`、`AnsiBlock` `:131-133` |
| `LiveElapsed`（内部） | `webui/src/tools/StepRow.tsx:43` | 每秒 tick 的耗时（`:45` `setInterval(..., 1_000)`）——**独立定时器局部刷新** |
| `ThinkingChain` | `webui/src/thinking/ThinkingChain.tsx:10` | 28px 单行思考链；`:14-25` `createEffect` 遍历 **所有** turns 找 active reasoning |
| `Markdown` | `webui/src/markdown/Markdown.tsx:11` | `md-host` + `md-tail`（`:52-56`）；`:31-50` `createEffect` 分块，**只重解析尾块** |
| `DiffList` / `DiffFileView` / `HunkRows` | `webui/src/diff/DiffView.tsx:233` / `:149` / `:120` | diff 解析与逐 hunk 行渲染；`BIG_DIFF_LINES=400` 默认折叠（`:22`、`:152`）；`:165-189` shiki 异步高亮失败回退纯文本 |
| `ApprovalStack` / `ApprovalCard` / `AskCard` / `PlanCard` | `webui/src/approval/ApprovalCards.tsx:158` / `:41` / `:78` / `:133` | 授权卡；`:181` `props.pending.slice(0, 1)` **只渲染最早 1 个**，`:184-186` 显示"还有 N 个" |
| `Composer` | `webui/src/composer/Composer.tsx:14` | 输入区；`:66-72` Enter 发送但 IME composing / `keyCode===229` 不发送；`:76-94` 运行中替换为停止 |
| `TabBar` / `Dot` | `webui/src/tabs/TabBar.tsx:31` / `:12` | 标签栏与状态点（优先级：新回复 > 待审批 > failed > running，`:13-23`） |
| `Collapse` | `webui/src/ui/Collapse.tsx:4` | 纯 CSS `grid-template-rows 0fr↔1fr`（`:4-8`），**不卸载 DOM** |

### 7.2 状态来源

**SolidJS 细粒度响应式：`createStore`（深层 store）+ 多个 `createSignal`，
模块级/类实例级单例。无 React Context、无 `useSyncExternalStore`、无 zustand、无 `useReducer`。**

- 会话级：`webui/src/session/store.ts:28` `export class SessionStore`；
  `:56` `this.state = createStore<SessionState>(emptySession())`；
  `:63-65` `private mutate(fn) { this.state[1](fn); }`——**Solid 2 的 draft-mutating setter**
- 独立 signal：`:32` `connection`、`:33` `activity`、`:34` `title`、`:35` `pending`、
  `:36` `loadError`、`:37` `hasNewReply`、`:39` `compactedAfter`、`:40` `loadingOlder`
- Tab 级全局：`webui/src/tabs/store.ts:22-29` 模块级 `createSignal`；
  `:27` 草稿是非响应式 `Map`
- reducer 是纯 draft 变更函数：`webui/src/session/reducer.ts:5-6` 注释 +
  `:175` 签名 `(draft, entry): SessionState`，`:304` `return draft;`
- 组件以 getter 读取：`webui/src/session/SessionView.tsx:18` `const state = () => store.state[0];`
- 传输层：SSE 经 `webui/src/session/store.ts:120` `new EventSource(...)`；
  URL 构造 `webui/src/lib/transport.ts:188-191`

### 7.3 列表是否虚拟化？

**没有虚拟化。**

- `webui/package.json:13-21` 无任何 windowing/virtual 依赖
- 全 `webui/` grep `virtual|Virtual|windowing|react-window|virtua|tanstack|VList`
  → 仅命中 `webui/bun.lock:426`、`:440`（`unplugin` 的传递依赖
  `webpack-virtual-modules`），与列表渲染无关
- **全量渲染**：`webui/src/session/SessionView.tsx:97` `<For each={state().slots}>`
  对**全部** slot 建 DOM；回合内部同样全量（`TurnView.tsx:139`、`:79`；
  `StepRow.tsx:55`、`:95`；`DiffView.tsx:217`、`:133`）

唯一与"裁剪"相关的两处，**都不是虚拟化**：

1. **内存窗口淘汰**：`webui/src/session/pagination.ts:8` `TURN_WINDOW = 50`；
   `:34-58` `evictForWindow` 在 `loaded > window` 时把「视口上方超过 `screens`(=2) 屏」
   的最旧回合替换为等高占位（`:54` `draft.slots[i] = { kind: "placeholder", ... }`）；
   `:50` 运行中回合永不淘汰（`break`）。**回合数据仍留在 `draft.turns`**
   （`webui/src/session/pagination.ts:49` 只读不删；测试
   `webui/tests/pagination.test.ts:48` 明确断言 `state.turns["#0"]` 仍存在）。
   触发点 `webui/src/session/SessionView.tsx:46-59`
2. **CSS 绘制跳过**：`webui/src/styles/app.css:164`
   `.turn { content-visibility: auto; contain-intrinsic-size: auto 320px; }`
   ——浏览器跳过离屏绘制，**DOM 节点仍在**

### 7.4 消息流式更新的刷新粒度

**结论：单个 token 不触发全列表重建。** 完整路径：

1. SSE `timeline.entry`（`webui/src/session/store.ts:123`）→ `JSON.parse`（`:128`）
   → `onTimelineEntry`（`:170`）
2. 幂等去重：`:172-173` `if (entry.timeline_seq <= watermark) return;`
3. 缺口检测：`:174-178` `seq > watermark + 1 && watermark > 0` → `scheduleResnapshot()`，
   **丢弃该事件**（走快照路径，有 2s 去抖 `RESNAPSHOT_DEBOUNCE_MS`，`:26`、`:218`）
4. **文本增量 rAF 合批**（`:179-186`）：`:181` key = `` `${turn_id}\u0000${block_id}` ``；
   `:182-185` 累加进 `pendingText: Map`（声明于 `:47`）**不立即写 store**；`:186` `scheduleFlush()`
5. **合批落地**（`:195-214`）：`:196-197` `if (this.rafHandle != null) return;`
   → `requestAnimationFrame(...)`——**每帧最多一次 store 写入**；
   `:200-201` 取缓冲快照并清空；`:202-212` 逐条 `applyEntry`
6. **reducer 原地变更**：`webui/src/session/reducer.ts:175-305`，
   `text_delta` 分支 `:206-218` 经 `findStep`（`:38-40`）定位后
   `step.text += delta`（`:211`/`:213`）——**只写该叶子**
7. **组件层只重解析尾块**：`webui/src/markdown/Markdown.tsx:31-33`
   `createEffect(() => [props.text(), props.revision ?? 0] as const, ...)`；
   `:41-46` 闭合块只在第一次渲染并入 `cache[]`；`:47-48` 仅尾块
   `tail.innerHTML = renderMarkdownHtml(tailBlock.content)` 每帧重算
8. `TurnView` 用 **thunk** 而非值快照传文本：`:131` `const answerText = () => turn().answer?.text ?? "";`，
   `:173` `<Markdown text={answerText} .../>`——配合 Solid 细粒度依赖追踪

**非文本事件不缓冲**（`:187-192`）：每个结构事件直接 `mutate` 一次。

**Solid 层面的支撑事实**：
- `For` 用 `mapArray` 做 keyed diff（`webui/node_modules/solid-js/dist/solid.js` 内
  `function For`），因此 `reducer.ts:34` 的 `draft.slots.push(...)` **不重建已存在行**
- `Show` 的条件经 `createMemo(..., { equals: (a,b) => !a === !b, sync: true })` 只比真假
  （同文件 `function Show`），因此 `answer` 由 `null` 变对象只切换一次，
  之后 `answer.text` 变更**不重建该子树**
- 局部定时器：`StepRow.tsx:44-46` 的 `<LiveElapsed>` 每秒独立刷新

### 7.5 所有"全量重建列表"或"每 token 触发整体重渲染"的位置

#### 真正的全量重建（2 处，均**不在** per-token 热路径）

1. **`applySnapshot`**：`webui/src/session/reducer.ts:432-433`
   ```ts
   draft.turns = {};
   draft.slots = [];
   ```
   随后 `:434-438` 逐回合 `buildTurn` 重建全部 Turn 对象。
   **这是唯一把顶层容器整体换新的地方。** 调用点唯一：
   `webui/src/session/store.ts:227-229` `resnapshot()`；其触发者：
   `:74`（activate/首次附加）、`:138`（`ringing.stream_terminated`）、
   `:145`（重连成功且此前有失败）、`:176`→`:216-221`（seq 缺口）。
2. **`prependPage`**：`webui/src/session/reducer.ts:473-478`
   `buildTurn` 造新对象（`:473`）+ `draft.slots.unshift(...newSlots)`（`:478`）；
   `:467-471` 对每个入场回合做 `Object.values(draft.turns).some(...)` **全表扫描去重**。
   调用点唯一：`webui/src/session/store.ts:385`（向上翻页）。

#### 每个 token **不会**触发的整体重渲染

全 `webui/src` grep `JSON.parse(JSON.stringify` → **未找到**；
grep `{...state}` / `{...draft}` 整顶层 spread → **未找到**。
仅 3 处 `[...` 命中，均与状态重建无关：
`webui/src/lib/ansi.ts:2`（注释里的正则）、
`webui/src/tabs/store.ts:45`（**仅新增标签时**复制 tabs 数组）、
`webui/src/session/store.ts:200`（**每帧**取缓冲值快照，有界：只含本帧有 delta 的块）。

#### 每回合级信号变化即重跑的**整表遍历**（不重建 DOM，但在 text 热路径的邻域）

| 位置 | 遍历内容 | 触发频率 |
|---|---|---|
| `webui/src/thinking/ThinkingChain.tsx:17-23` | `Object.values(turns)` × `turn.steps.find` | **每个含 thinking 的 delta 都跑一次**——这是最接近"per-token 全表扫描"的位置。但 DOM 写入被限制为 `:32` 一个 signal（整行文本） |
| `webui/src/session/reducer.ts:500`、`:502-508` | `Object.values(draft.turns)` 反向扫描找 open reasoning | 快照后一次（`:447` 调用） |
| `webui/src/tabs/TabBar.tsx:16` | `Object.values(props.store.state[0].turns).some(...)` | 每标签状态点重算 |
| `webui/src/session/store.ts:299-300`、`:338` | `Object.values(this.stateGet.turns)` | compact 锚点 / 查找 |
| `webui/src/app/App.tsx:91`、`:99` | `Object.values(tab.store.state[0].turns).some(...)` | 标签状态 |
| `webui/src/session/SessionView.tsx:81` | `state().waits.filter(...)` | **每个 slot 渲染时一次**（`:106` 每个 TurnView 都调用） |
| `webui/src/session/SessionView.tsx:42-45` | `querySelectorAll("[data-slot-key]")` 测**全部** slot `offsetHeight` | 每次翻页 |
| `webui/src/session/store.ts:397-404` | `setPlaceholderHeight` 对每个被淘汰 key 线性扫 `draft.slots` | 淘汰 K 个回合 = K × O(slots) 次 store 写 |
| `webui/src/turn/TurnView.tsx:52-56` | `steps.map` + `:60` 全排序 | `Timeline` 的 `createMemo`；每次新 step 都重建整个 items 数组；仅 `turn.expanded` 时挂载（`:165-167`，且 `Collapse` 不卸载 DOM） |
| `webui/src/session/pagination.ts:43` | `draft.slots.filter(...)` | 每次翻页 |
| `webui/src/tabs/store.ts:92`、`:97` | `.map` 全表 | 每 15s 轮询（`webui/src/app/App.tsx:39`） |

#### 其他整体替换（非文本热路径）

- `webui/src/session/store.ts:327` `this.pending[1](next)`——审批数组整体替换，
  但 `ApprovalCards.tsx:181` `slice(0, 1)` 只渲染 1 个
- `webui/src/session/store.ts:408`/`:411` 单信号写入（`title` / `hasNewReply`）
- `webui/src/tabs/store.ts:52` `setFocusToken(focusToken() + 1)`

### 7.6 前端测试

`webui/package.json:10` `"test": "bun test tests"`。
`happy-dom` 在 devDependencies（`:26`）但**未被任何测试 import**。

磁盘实存 5 个测试文件，全部 `import { describe, expect, test } from "bun:test"`，
共 21 个 `test(...)`，**全部针对纯函数**（`webui/README.md:12` 自述"仅纯逻辑单测(spec §1.3)"），
**没有一个测试渲染组件或挂载 DOM**——因此**没有任何测试覆盖 §7.4/§7.5 的渲染/重渲染行为**。

| 文件 | 覆盖内容 | 判定为"实现细节"的断言 |
|---|---|---|
| `webui/tests/reducer.test.ts`（108 行） | 回合生命周期推进、作答提升/降级、`tool_updated` 终态映射、`turn_sealed` 状态映射、`prependPage` 按 `turn_index` 去重 | `:83` 用 `turns["#3"]` 耦合 `turnKeyOf` 的 key 格式（`reducer.ts:14-16`）；`:104` 对 `slots[0]` 完整结构深比较，写死 `Slot` 形状 |
| `webui/tests/pagination.test.ts`（73 行） | `anchorScrollTop` 数值、1.5× 视口阈值、窗口淘汰语义（含运行中回合不淘汰、数据保留）、占位高度钳位 | `:43` `key.replace("#","")` 桩与 `:46-48` `toMatchObject({kind:"placeholder",key:"#0"})` 编码 slot/key 内部形状；`:55`/`:59` 直接读写 `turns["#1"].status` |
| `webui/tests/diff-parse.test.ts`（114 行） | 多文件 diff 解析、行号/统计/noeol/二进制、省略行数计算、等长 del/add 配对、词级片段标记、行尾空白、语言推断 | `:87` 用 `2001` 锚定实现的 2000 字符阈值；`:6-10` 有一个未使用的哑 helper（`void seededTurns;`），不参与断言 |
| `webui/tests/md-split.test.ts`（42 行） | 段落/围栏/标题分块、松散列表不切分保序号、增量追加时闭合块稳定 | 无明显实现细节耦合 |
| `webui/tests/time.test.ts`（65 行） | `formatWorkDuration` 六组文案、工作=结束−开始−等待、未结束等待扣到当下、无 `workStartedAt` 返回 `undefined`、`formatOffset`、退避 0.5→10s + 抖动 ±20%、连续 6 次失败转 offline | 文案类断言到具体字符串（`:8-15`、`:40-44`）；`:61-64` 的 `6` 与 `reconnect.ts:8` 常量关系被硬编码 |

另注：`webui/tsconfig.json:21` `"include": ["src", "vite.config.ts"]`——
**`tests/` 不在 typecheck 覆盖范围内**（`bun run typecheck` 不检查测试文件）。

### 7.7 未找到的性能采集代码

全 `webui/src` grep `performance\.|PerformanceObserver|mark\(|measure\(` → **未找到**；
**未找到** `webui/src/perf/` 目录（该目录在 git 历史中存在，
`webui/src/perf/PerfHarness.tsx` 已被删除）。

---

## 第 8 步：工程健康度

> **本节数据为自行统计 + 一次独立交叉审计**（不依赖 codegraph）。所有计数均给出所用方法。
> 统计范围：`crates/**/*.rs`，排除 `target/`；`webui/` 单独标注。
> **口径警告**：PowerShell 的 `Select-String`/`-match` **默认大小写不敏感**。
> 下表数字均已按大小写敏感重跑；大小写实质影响的只有 `unsafe`
> （不敏感 117 vs 敏感 110）。

### 8.1 unwrap / expect / panic 数量及集中位置

**全量计数（含测试代码）**：

| 模式 | 出现次数 | 计数方法 |
|---|---|---|
| `.expect(` | 2,934 | `[regex]::Matches($text, '\.expect\(')` |
| `.unwrap()` | 1,248 | `[regex]::Matches($text, '\.unwrap\(\)')` |
| `panic!(` | 202 | `[regex]::Matches($text, 'panic!\(')` |
| `unreachable!(` | 11 | `[regex]::Matches($text, 'unreachable!\(')` |
| `todo!` / `unimplemented!` | **0** | `[regex]::Matches($text, '(todo!\|unimplemented!)\(')` |
| `assert!` | 2,662 | `(?<![\w])assert!` |
| `assert_eq!` | 3,716 | `(?<![\w])assert_eq!` |
| `assert_ne!` | 39 | `(?<![\w])assert_ne!` |
| `debug_assert*` | 19 个真实宏调用 | 26 处匹配 = 19 宏 + 3 注释 + 2 `cfg!(debug_assertions)` + 2 标识符 `debug_assert_gate_invariants` |

**注**：上表 `.unwrap()` 未统计 `.unwrap_or(..)` / `.unwrap_or_else(..)` /
`.unwrap_or_default()` 这三类非 panic 的变体（它们不 panic，不应计入危险项）。

#### 生产 / 测试拆分（关键结果）

拆分方法（四桶，行级）：
1. 路径含 `\tests\` 或文件名为 `tests.rs` ⇒ 100% 测试；
2. 行掩码：属性行匹配
   `^\s*#\[(cfg\(\s*(all\(\s*)?test\b|test\b|tokio::test\b|rstest\b|test_case\b|serial_test)`，
   然后 **括号深度感知** 地跳过属性/注释行、花括号配对到该项闭合处
   （该修正使 10 个此前漏掩码的项被正确归类，例如
   `crates/qaqh-workspace/src/exec/handler.rs:305` 的 `#[cfg(test)]`）；
3. 独立桶：`#[cfg(any(test, feature = "test-harness"))]` ⇒ 不进普通 release 构建；
4. 计数前用有状态扫描器剥离注释与字符串（`//`、`/* */`、`"…"`、`r#"…"#`、字符字面量，
   **状态跨行保持**——2 个掩码 bug 被追溯到逐行状态与多行 raw string）。

自检：0 个括号不配对区域；0 个生产命中落在测试命名函数内；
**独立交叉验证：0 个文件在完全不含任何测试属性的情况下出现 `.unwrap()`**。

| 模式 | 全部出现 | **测试外（生产）** | 仅 harness |
|---|---|---|---|
| `.unwrap()` | 1,248 | **0** | 0 |
| `.expect(` | 2,934 | **171** | 19 |
| `panic!(` | 202 | **5** | 0 |
| `unreachable!(` | 11 | **8** | 0 |
| `assert!` | 2,662 | **0** | 0 |
| `assert_eq!` | 3,716 | 4（全在 `examples/` 的 `main()`） | 0 |
| `assert_ne!` | 39 | 0 | 0 |

**生产命中合计 188 处**（148 在 `src/`、37 在 `examples/`、3 在 `build.rs`）。
harness 桶的 19 处 `expect` 全在 `crates/qaqh-message/src/wal.rs`
（`:523,572,587,601,611,631,707,880`）。

**测试代码豁免是显式配置的**：`clippy.toml:1-3`

```toml
# 测试代码允许 unwrap（生产代码仍 deny）。
# 覆盖 #[cfg(test)] 模块与 tests/ 集成测试目录。
allow-unwrap-in-tests = true
```

配合 `Cargo.toml:42-44` 的工作区 lint 配置：

```toml
[workspace.lints.clippy]
unwrap_used = "deny"
string_slice = "deny"
```

**核实结果**：`unwrap_used = "deny"` 在 workspace 层生效，
**生产代码 `.unwrap()` 实测为 0**——两条独立证据：
(a) 掩码拆分结果；(b) "0 个文件在无任何测试属性的情况下含 `.unwrap()`"这一全文件级反证。
这与 `cargo clippy --workspace --all-targets -- -D warnings`（`README.md:50`）门禁一致。

#### 生产 `panic!`（5 处）

| 位置 | 内容 |
|---|---|
| `crates/qaqh-config/src/registry.rs:32` | `panic!("assets/providers.toml baseline parse failed: {e}")`，在 `fn builtin_providers()` 的 `OnceLock` 初始化内 |
| `crates/qaqh-workspace/src/manager.rs:240` | `.unwrap_or_else(\|error\| panic!("invalid builtin tool descriptor for {key}: {error}"))` |
| `crates/qaqh-workspace/src/manager.rs:263` | `.unwrap_or_else(\|error\| panic!("invalid typed tool descriptor: {error}"))` |
| `crates/qaqh-workspace/src/edit/handler.rs:338` | `.unwrap_or_else(\|fatal\| panic!("edit tool fatal: {}", fatal.message))` |
| `crates/qaqh-webui-gateway/build.rs:31` | release 构建缺 `webui/out/renderer/index.html`（构建期，不进二进制） |

#### 生产 `unreachable!`（8 处）

`crates/qaqh-daemon/src/axum_server/axum_impl/command.rs:284`、`:322`；
`crates/qaqh-lsp/src/tool.rs:369`；`crates/qaqh-runtime/src/host_impl.rs:297`；
`crates/qaqh-sandbox/src/lib.rs:189`；
`crates/qaqh-session/src/projection/conversation.rs:421`；
`crates/qaqh-session/src/projection/timeline.rs:377`；
`crates/qaqh-workspace/src/todo/parse.rs:322`。

#### 生产命中的危险性分布

- **36 处落在 `fn main()` 内**（33 在 `examples/`，加 `crates/qaqh-daemon/src/main.rs:73`、`:93`
  与 `crates/qaqh-webui-gateway/src/main.rs:10`）⇒ panic = 进程退出。
  例：`crates/qaqh-client/examples/command.rs:50,57,84,92,99,103,126`；
  `crates/qaqh-session/examples/e2e_seed.rs:24,27,29,37,40,42,47,86,125,149,162`。
- **22 处位于签名返回 `-> Result<…>` 的函数内** ⇒ panic 会中止而非返回 `Err`：
  `crates/qaqh-config/src/dto.rs:132`、`registry.rs:32`；
  `crates/qaqh-gate/src/transport.rs:383`；
  `crates/qaqh-runtime/src/host_impl.rs:297`、`registry.rs:1455,1490`、
  `ringing/pending_store.rs:238`、`timeline.rs:428,433`；
  `crates/qaqh-sandbox/src/lib.rs:189`；
  `crates/qaqh-session/src/canonical/clock.rs:77`、`canonical/replay_window.rs:511`、
  `canonical/tool_ledger.rs:916,957`、`grouping.rs:308`（`move_session -> Result<(), String>`）、
  `projection/agent_graph.rs:458`；
  `crates/qaqh-workspace/src/skill.rs:168,173,174,177`、
  `todo/parse.rs:322`（`insertion_index -> Result<usize, String>`）、
  `todo/typed.rs:583`（`todo_update_for_typed -> Result<TodoUpdateOutput, String>`）。
- **3 处 `.expect("tool thread spawn")` 在 `std::thread::Builder::…spawn(..)` 调用链的父侧**
  （不在闭包内）：`crates/qaqh-runtime/src/agent/tool_runtime.rs:660`、`:683`、`:953`。
- **3 处在 `build.rs`**（构建期失效，不进二进制）：`crates/qaqh-daemon/build.rs:37`、
  `crates/qaqh-webui-gateway/build.rs:31`、`crates/qaqh-workspace/build.rs:22`。

#### 最大单一惯用法：72 / 188 = 38% 的生产命中

同一模式集中在 `fn descriptor(&self) -> ToolDescriptor`：**188 处生产命中中有 72 处**
（`qaqh-subagent` 36 + `qaqh-workspace` 36）都是
`ToolName::new("…").expect("valid … tool name")` / `.expect("… output schema")`：

- `crates/qaqh-subagent/src/lib.rs:216,232,327,334,362,368,396,402,430,436,503,522,638,655,784,792`
- `crates/qaqh-workspace/src/file_query.rs:107,113`；`file_mutate.rs:263,269,647,652`；
  `grep_tool.rs:199,205`；`journal.rs:440,447`；`web.rs:89,95`

#### 生产命中 Top 10 文件

| # | 命中数 | 行数 | 文件 |
|---|---|---|---|
| 1 | 16 | 2,306 | `crates/qaqh-subagent/src/lib.rs` |
| 2 | 11 | 163 | `crates/qaqh-session/examples/e2e_seed.rs` |
| 3 | 10 | 416 | `crates/qaqh-subagent/src/task_tools.rs` |
| 4 | 10 | 474 | `crates/qaqh-subagent/src/board_tools.rs` |
| 5 | 7 | 139 | `crates/qaqh-client/examples/command.rs` |
| 6 | 6 | 381 | `crates/qaqh-workspace/src/skill.rs` |
| 7 | 6 | 56 | `crates/qaqh-config/examples/export_providers.rs` |
| 8 | 6 | 2,137 | `crates/qaqh-runtime/src/agent/engine_turn.rs` |
| 9 | 6 | 1,300 | `crates/qaqh-runtime/src/agent/tool_runtime.rs` |
| 10 | 5 | 770 | `crates/qaqh-workspace/src/todo/typed.rs` |

次席（各 4）：`crates/qaqh-workspace/src/exec/handler.rs`、`crates/qaqh-client/examples/remote_fs.rs`、
`crates/qaqh-workspace/src/file_mutate.rs`、`crates/qaqh-session/src/grouping.rs`、
`crates/qaqh-workspace/src/exec/truncate.rs`。

#### 按 crate 拆分（全部命中 / 生产命中）

`u/e/p/uq` = unwrap / expect / panic / unreachable。

| crate | 全部 u/e/p/uq | 生产 u/e/p/uq |
|---|---|---|
| `qaqh-client` | 12/108/13/0 | 0/17/0/0 |
| `qaqh-config` | 37/193/5/0 | 0/8/1/0 |
| `qaqh-config-api` | 0/7/0/0 | 0/0/0/0 |
| `qaqh-daemon` | 244/49/4/2 | 0/4/0/2 |
| `qaqh-domain` | 0/23/0/0 | 0/0/0/0 |
| `qaqh-gate` | 48/66/14/1 | 0/5/0/0 |
| `qaqh-lsp` | 15/7/1/1 | 0/1/0/1 |
| `qaqh-mcp` | 64/36/10/1 | 0/1/0/0 |
| `qaqh-message` | 18/58/4/0 | 0/1/0/0（+19 harness expect） |
| `qaqh-policy` | 0/0/1/0 | 0/0/0/0 |
| `qaqh-ringing` | 0/40/0/0 | 0/0/0/0 |
| `qaqh-runtime` | 310/880/64/2 | 0/19/0/1 |
| `qaqh-sandbox` | 0/16/0/1 | 0/0/0/1 |
| `qaqh-session` | 24/881/20/2 | 0/26/0/2 |
| `qaqh-skills` | 49/1/0/0 | 0/0/0/0 |
| `qaqh-subagent` | 2/63/5/0 | 0/36/0/0 |
| `qaqh-title` | 0/0/0/0 | 0/0/0/0 |
| `qaqh-types` | 19/71/0/0 | 0/0/0/0 |
| `qaqh-webui-gateway` | 34/2/1/0 | 0/1/1/0 |
| `qaqh-workspace` | 372/433/60/1 | 0/52/3/1 |
| **合计** | **1248/2934/202/11** | **0/171/5/8** |

**panic 被显式兜住的地方**：`Loop::safe_dispatch`
（`crates/qaqh-runtime/src/agent/loop_core.rs:277`）用 `catch_unwind` 包裹每次派发；
其文档（`:555-569`）记录 BUG-2026-09-13-07：`drain_pending` 与
`dispatch_deferred_ringing` 曾裸调 `dispatch_ringing_one`，引擎 panic 会逃出 `run()`
导致进程死亡 + 丢失已出队命令 + 跳过 liveness `busy` 标记。
`loop_core.rs:28-34` 的模块文档描述 panic 后的 4 步恢复（重置引擎 → 清 cancel →
发 `OperationFailed` → 继续处理后续命令）。

### 8.2 unsafe 的位置

**大小写敏感计数：110 处 `\bunsafe\b`**（大小写不敏感为 117；多出的 7 处是字符串字面量，
如 `qaqh-gate/tests/gate_test.rs` 与 `qaqh-workspace/tests/cjk_integration.rs` 中的
`unsafe-checker` / `UNSAFE`）。

按掩码拆分：**生产 29 处、测试 63 处**，其余在字符串字面量内。

#### 生产 `unsafe`（29 处，完整清单）

| 文件:行 | 上下文 |
|---|---|
| `crates/qaqh-client/src/discovery.rs:285` | `let handle = unsafe {` |
| `crates/qaqh-client/src/discovery.rs:295` | `let exit_code = unsafe {` |
| `crates/qaqh-client/src/discovery.rs:300` | `unsafe {` |
| `crates/qaqh-config/src/secrets.rs:428` | `unsafe {` → `MoveFileExW(` |
| `crates/qaqh-config/src/secrets.rs:451` | `unsafe {` → `CryptProtectData(`（DPAPI 加密） |
| `crates/qaqh-config/src/secrets.rs:482` | `unsafe {` → `CryptUnprotectData(`（DPAPI 解密） |
| `crates/qaqh-gate/src/message_api.rs:605` | `unsafe { (*callback)(event) };` |
| `crates/qaqh-lsp/src/connection.rs:480` | `unsafe {` |
| `crates/qaqh-mcp/src/connection.rs:646` | `let probe = unsafe { libc::killpg(pgid as libc::pid_t, 0) };` |
| `crates/qaqh-mcp/src/connection.rs:648` | `let killed = unsafe { libc::killpg(pgid as libc::pid_t, libc::SIGKILL) };` |
| `crates/qaqh-runtime/src/registry.rs:69` | `unsafe {` |
| `crates/qaqh-runtime/src/service/common.rs:12` | `let returned = unsafe { libc::malloc_trim(0) };` |
| `crates/qaqh-sandbox/src/lib.rs:80` | `unsafe {` |
| `crates/qaqh-sandbox/src/linux.rs:92` | `if unsafe { libc::setrlimit(libc::RLIMIT_CORE, &core) } != 0 {` |
| `crates/qaqh-sandbox/src/linux.rs:102` | `if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &nofile) } != 0 {` |
| `crates/qaqh-sandbox/src/linux.rs:212` | `if unsafe { libc::dup2(null.as_raw_fd(), libc::STDIN_FILENO) } < 0 {` |
| `crates/qaqh-sandbox/src/linux.rs:232` | `unsafe {` |
| `crates/qaqh-workspace/src/file_shared.rs:70` | `unsafe {` |
| `crates/qaqh-workspace/src/process_registry.rs:650` | `unsafe {` |
| `crates/qaqh-workspace/src/process_registry.rs:717` | `unsafe {` |
| `crates/qaqh-workspace/src/process_registry.rs:735` | `unsafe {` |
| `crates/qaqh-workspace/src/exec/pipe.rs:230` | `let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };` |
| `crates/qaqh-workspace/src/exec/pipe.rs:234` | `unsafe { libc::fcntl(fd, libc::F_SETFL, flags \| libc::O_NONBLOCK) };` |
| `crates/qaqh-workspace/src/exec/pipe.rs:248` | `unsafe extern "system" {` |
| `crates/qaqh-workspace/src/exec/pipe.rs:259` | `let ok = unsafe {` |
| `crates/qaqh-workspace/src/exec/pipe.rs:376` | `unsafe extern "system" {` |
| `crates/qaqh-workspace/src/exec/pipe.rs:396` | `let code_page = unsafe { GetOEMCP() };` |
| `crates/qaqh-workspace/src/exec/pipe.rs:405` | `let wide_len = unsafe {` |
| `crates/qaqh-workspace/src/exec/pipe.rs:423` | `let written = unsafe {` |

按 crate 原始计数：`qaqh-runtime` 33、`qaqh-workspace` 28、`qaqh-gate` 16、
`qaqh-message` 9、`qaqh-client` 8、`qaqh-config` 5、`qaqh-sandbox` 5、`qaqh-mcp` 2、
`qaqh-lsp` 1、`qaqh-types` 1。

#### 测试 `unsafe`（63 处）

绝大多数是 Rust 2024 起 `set_var`/`remove_var` 成为 `unsafe fn` 所致，
例如 `crates/qaqh-client/src/session.rs:424,441,442`（位于 `#[test]` 内）、
`crates/qaqh-message/src/store.rs:1885,1921,1922,2958,2992,2993,3025,3038,3039`、
`crates/qaqh-workspace/src/read_image/mod.rs:407,421,434,451,464,483`，
以及 `qaqh-runtime/tests/`、`qaqh-workspace/tests/`、`qaqh-config/tests/`、
`qaqh-client/tests/` 下约 40 处。

**`unsafe` 相关属性**：全仓**未找到** `#![forbid(unsafe_code)]`、
`#![deny(unsafe_code)]` 或 `#[allow(unsafe_code)]` 的任何出现。
即 `unsafe` **既未被禁止也未被显式允许**（走默认：允许）。

### 8.3 错误类型：统一枚举还是 String/anyhow 混用？

**三种风格并存，且已形成明确的分层惯例。**

#### 依赖证据

| 依赖 | 位置 |
|---|---|
| `anyhow = "1"` | 仅 `crates/qaqh-gate/Cargo.toml:14` 与 `crates/qaqh-workspace/Cargo.toml:20`（注释 `:19`：ToolError/FatalToolError 的诊断 `source`，不序列化） |
| `thiserror = "2"` | 仅 `crates/qaqh-client/Cargo.toml:20` 与 `crates/qaqh-session/Cargo.toml:16` |

`anyhow` 词频：gate 53、workspace 6。`thiserror` 出现 7 处 `use thiserror::Error`：
`crates/qaqh-client/src/error.rs:4`；`crates/qaqh-session/src/actor.rs:9`、
`canonical/identity.rs:17`、`canonical/log.rs:13`、`canonical/recovery_executor.rs:18`、
`canonical/tool_ledger.rs:11`、`team/error.rs:3`。
**未找到** `snafu` / `eyre` / `miette`。

#### 公开签名口径（`src/` 非测试文件）

- 有显式返回类型的公开函数（`pub`/`pub(crate)`/`pub(super)`）：**1,768**
  （1,401 `pub`、343 `pub(crate)`、24 `pub(super)`）
- 其中 **436 个返回 `Result` 类**：

| 返回类型 | 数量 | 说明 |
|---|---|---|
| **`Result<_, String>`** | **174** | **主流**。判定式：`->\s*((std\|core)::result::)?Result\s*<.*?,\s*((std\|alloc)::string::)?String\s*>\s*$` |
| `anyhow::Result` | **5** | 全部在 `qaqh-gate`：`chat_completions_api.rs chat_stream_openai`、`lib.rs chat_stream`、`message_api.rs chat_stream_anthropic`、`responses_api.rs chat_stream_responses`、`transport.rs run_with_retry` |
| `io::Result` | 11 | — |
| typed / 别名 `Result<T>` | 246 | 含 `qaqh-ringing` 的 `RingingV2*` 别名返回 |

`Result<_, String>` 按 crate：`qaqh-runtime` 61、`qaqh-workspace` 48、`qaqh-session` 16、
`qaqh-config` 14、`qaqh-webui-gateway` 11、`qaqh-skills` 6、`qaqh-gate` 5、`qaqh-daemon` 3、
`qaqh-types` 3、`qaqh-policy` 2、`qaqh-subagent` 2、`qaqh-client` 1、`qaqh-config-api` 1、
`qaqh-sandbox` 1。代表：`crates/qaqh-config/src/config.rs:667 pub fn load() -> Result<Self, String>`、
`:1037 pub fn save(&self) -> Result<(), String>`。

#### 按 crate 的风格归属

| crate | 风格 | 代表类型定义 |
|---|---|---|
| `qaqh-client` | thiserror enum + 1 处 `Result<_,String>` 边缘 | `src/error.rs:9 pub enum ClientError` |
| `qaqh-config` | 仅 `Result<_, String>`，**未找到 error enum** | `src/config.rs:667`、`:1054` |
| `qaqh-config-api` | `Result<_, String>` | `src/lib.rs:262 pub fn validate(&self) -> Result<(), String>` |
| `qaqh-daemon` | `Result<_, String>`，**未找到 error enum** | `src/server.rs`（`parse`/`run`/`run_with`） |
| `qaqh-domain` | 无 `pub fn` 返回 `Result`；`ErrorScope` 是数据枚举 | `src/event.rs:212 pub enum ErrorScope` |
| `qaqh-gate` | **anyhow** 在流式边界，同步路径 `Result<_,String>`；**未找到 error enum** | `Cargo.toml:14` |
| `qaqh-lsp` | 本地枚举 | `src/error.rs:6 pub enum LspErrorKind` |
| `qaqh-mcp` | 本地枚举 | `src/error.rs:11 pub enum McpErrorKind` |
| `qaqh-message` | 本地枚举 | `src/context_flow.rs:207 pub enum FlowError`；`src/wal.rs:483 pub enum WalReadError` |
| `qaqh-policy` | `Result<_, String>`，**未找到 error enum** | `src/input_guard.rs content_guard` |
| `qaqh-ringing` | 本地枚举 | `src/v2/cursor.rs:175 pub enum CursorError` |
| `qaqh-runtime` | 混合：61 处 `Result<_,String>` + 本地枚举 | `src/timeline.rs:26 pub enum TimelineError`；`src/ringing/v2.rs:35 pub enum V2HubError` |
| `qaqh-sandbox` | 本地枚举 | `src/linux.rs:30 pub enum SandboxError` |
| `qaqh-session` | **thiserror 类型化核心 + String 边缘**（96 typed vs 16 String） | `canonical/log.rs:33 CanonicalError`、`canonical/tool_ledger.rs:84 ToolLedgerError`、`actor.rs:479 SessionActorError`、`actor.rs:89 TurnCoreError`、`actor.rs:125 ToolAdmissionError`、`canonical/identity.rs:33 CanonicalIdentityError`、`canonical/recovery_executor.rs:33 RecoveryExecutionError`、`session_fact_v2/validation.rs:12 ValidationError`、`projection/agent_graph.rs:53 AgentGraphError`、`session_fact_v2/agent.rs:41 AgentPathError`、`team/error.rs:6 TeamError` |
| `qaqh-skills` | `Result<_, String>`，**未找到 error enum** | `src/lib.rs`（`load`、`load_named`、`read_resource`） |
| `qaqh-subagent` | `Result<_, String>` 汇入 workspace 的冻结 `TypedTool` 边界 | `src/lib.rs:27 #![allow(clippy::result_large_err)] // TypedTool's frozen public error boundary.` |
| `qaqh-title` | 无 `pub fn` 返回 `Result`，**未找到 error enum** | — |
| `qaqh-types` | `Result<_, String>`，**未找到 error enum** | `src/image_store.rs`、`src/token.rs:init_tokenizer` |
| `qaqh-webui-gateway` | `Result<_, String>`（11）+ 6 typed，**未找到 error enum** | `src/daemon.rs` |
| `qaqh-workspace` | 最丰富：类型化工具错误 + anyhow 仅作 `source` | `tool_api/error.rs:11 ToolErrorKind`、`:83 ToolErrorCode`、`:87 ToolErrorCodeError`、`:305 ToolExecutionError`；`src/lib.rs:695 pub enum ToolError`；`apply_patch_engine/mod.rs:42 EngineError`；`apply_patch_engine/parser.rs:43 ParseError`；`audit/mod.rs:164` + `audit/v2.rs:207 AuditError`；`tool_api/descriptor.rs:219 DescriptorError`；`authorization.rs:289 ApprovalError` |

**未找到任何 crate-local error enum 的 crate**：`qaqh-config`、`qaqh-config-api`、
`qaqh-daemon`、`qaqh-gate`、`qaqh-policy`、`qaqh-skills`、`qaqh-subagent`、`qaqh-title`、
`qaqh-types`、`qaqh-webui-gateway`（共 10 个）。

#### 错误码枚举模块（唯一真相）

`crates/qaqh-workspace/src/tool_api/error.rs`：

- `:11 pub enum ToolErrorKind`——闭集：InvalidArguments / NotFound / Conflict /
  PermissionDenied / Unauthorized / Timeout / Cancelled / Network / Execution /
  Unavailable / Custom
- `:38-52 pub fn builtin_code(self) -> Option<&'static str>`——映射为 snake_case：
  `invalid_arguments`、`not_found`、`conflict`、`permission_denied`、`unauthorized`、
  `timeout`、`cancelled`、`network`、`execution`、`unavailable`
- `:83 pub struct ToolErrorCode(String)`；`:92-117 pub fn parse(raw: &str) -> Result<Self, ToolErrorCodeError>`
  强制 `^[a-z][a-z0-9_]*$`（文档 `:78-81`："形态：`^[a-z][a-z0-9_]*$`
  （与 canonical fact v2 的 `error.code` 校验完全一致）"）
- 配套机器可读映射：`crates/qaqh-workspace/src/lib.rs:814-832 pub fn code(&self) -> &'static str`
  （`manager_unavailable`、`unknown_tool`、`invalid_arguments`、`io_error`、
  `blocked_by_mode`、`audit_unavailable`、`audit_quarantined` …）
- 对应 commit：`aa218cf fix(workspace): 工具错误码全量统一 snake_case——根除 canonical fact 校验拒收 legacy 大写码`，
  格式归一 `e032cb9`

#### 其他错误码位置

- `crates/qaqh-config-api/src/lib.rs`（483 行）是"配置契约层（wire DTO）"，
  被 `qaqh-config` 与 `qaqh-runtime` 共同依赖（两端共享唯一真相）；
  校验入口 `:262 pub fn validate(&self) -> Result<(), String>`
- canonical fact 校验器：`crates/qaqh-session/src/session_fact_v2/validation.rs:185` `validate`
  （扇出 24、圈复杂度约 51，见 §9）
- HTTP 层稳定错误码：`crates/qaqh-daemon/src/axum_server/axum_impl/v2.rs:1219`
  `api_error_response(status, code, message)`，实际使用的 code 字符串包括
  `"invalid_body"`（`:86`）、`"unsupported_version"`（`:94`）、`"missing_session_id"`（`:468`、`:357`）、
  `"lease_required"`（`axum_impl/mod.rs:157`、`sse.rs:84`）；命令层 code 见
  `crates/qaqh-runtime/src/registry.rs` 的 `emit_operation_failed(...)` 调用点，
  如 `"input_accept_append_failed"`（`loop_dispatch_conversation.rs:392`）、
  `"compact_in_progress"`（`:351`）、`"subagent_finish_append_failed"`（`:222`）、
  `"inter_agent_interrupt_not_implemented"`（`:262`）、
  `"duplicate_tool_call"`（`admit.rs:170`）、`"orphan_tool_result"`（`loop_outcome.rs:419`）

### 8.4 配置、密钥、日志

#### 配置

| 入口 | 位置 |
|---|---|
| `Config::load()`（持 `config_io_lock`） | `crates/qaqh-config/src/config.rs:667-673` |
| `Config::load_unlocked()` → `ConfigStore::default_location()` + `SecretStore::default_location()` | `crates/qaqh-config/src/config.rs:675-678` |
| **`Config::update<F>()`——唯一写入端口**（load → mutate → save → `watch::publish`） | `crates/qaqh-config/src/config.rs:686-699` |
| `Config::load_from_paths_with(store, secrets)` | `crates/qaqh-config/src/config.rs:703` |
| `Config::save()` / `save_unlocked()` | `crates/qaqh-config/src/config.rs:1038-1043`、`:1045-1050` |
| `Config::save_with(&ConfigStore, &SecretStore)` | `crates/qaqh-config/src/config.rs:1054` |
| 热更新 `subscribe`/`latest`/`publish`/`authoritative`/`reload_from_disk` | `crates/qaqh-config/src/watch.rs:34,40,49,64,76` |
| 契约层校验入口 | `crates/qaqh-config-api/src/lib.rs:262 pub fn validate(&self) -> Result<(), String>` |

- 契约层：`crates/qaqh-config-api`（483 行，`ConfigDto` 读模型 / `ConfigPatch` 写模型，
  `Cargo.toml` description 自述"winui/ratatui/web 三端共享的唯一真相"）
- 最大复杂度函数 `load_from_paths_with`（`config.rs:703`，圈复杂度约 76——见 §9）
- 数据根：`crates/qaqh-types/src/platform.rs:54-69`；
  `config_path()` `:333`、`daemon_discovery_path()` `:338`、`sessions_dir()` `:347`
- 数据根有**所有权校验**：`:193-209` 校验 data root 必须是当前用户的直接 `.qaqh` 目录，
  否则报错（`:209`）

#### 密钥

- **不落 `config.toml`**：`crates/qaqh-config/src/secrets.rs:1-16` 模块文档明确
  "Secret store: API keys never touch `config.toml`"，
  并记录审计结论 P0-1（凭据曾以明文存在 `config.toml`，含 `.toml.tmp` 原子写残留）
- **独立文件**：`{config_dir}/secrets.toml`（`secrets.rs:67-69` `default_location`）
- **确实写盘**：TOML，键位 `[main] api_key` / `[subagent] api_key` / `[secrets.mcp] <name>`
  （`load` `:81-89`、`set` `:109-121`、mcp 段 `:136-140`）

**平台矩阵（已逐行核实 cfg 门控）**：

| 目标平台 | 加密 | 文件权限 | 依据 |
|---|---|---|---|
| Windows | **DPAPI**，blob 形如 `dpapi:<base64>` | `restrict_permissions` 是**空实现** | `encrypt` `#[cfg(windows)]` `:438-439`（`CryptProtectData` `:451`，前缀写于 `:465`）；`decrypt` `:468-469`（`CryptUnprotectData` `:482`）；`#[cfg(not(unix))] fn restrict_permissions(_path: &Path) {}` `:509-510` |
| Unix | **明文**（UTF-8 直通） | **0600** | `encrypt` `#[cfg(not(windows))]` `:491-496`；`#[cfg(unix)] restrict_permissions` = `set_permissions(from_mode(0o600))` `:503-507` |
| 其他（非 unix 非 windows） | 明文 | **不限制** | 同上两条 `cfg` 的组合 |

- **文档与代码不一致（已确认）**：`secrets.rs:322` 注释称锁文件为
  "0600（Unix）；**Windows ACL 同步收紧**"，但 `#[cfg(not(unix))]` 分支（`:509-510`）
  是**空函数体**，Windows 上实际不做任何权限收紧。同一注释的不准确表述也出现在
  `:323`（lock）、`:371`（tmp）、`:386`（final）三处调用点所依赖的该函数上。
- **跨进程事务**：`set()` 全程持 `secrets.toml.lock` 独占文件锁
  （`lock_exclusive` `:309-327`；注释 `:107-108`"读-改-写是一个**跨进程**事务…
  否则 daemon 与 CLI 的并发 read → mutate → write 会互相覆盖丢密钥"；
  锁文件独立于目标文件，理由见 `:306-308`——rename 会换 inode）。
  写入机制：tmp（含 pid + 单调 nonce）→ `flush` → `sync_all` → rename
  （`:357-388`，`replace_file` `:406-437`）
- **`config.toml` 只存不透明标记 `"set"`**：`CONFIG_MARKER` `secrets.rs:57`；
  写入 `config.rs:1088-1092`（main）/ `:1122`（subagent）；
  读回 `:771-783` / `:836-849`，含**单向**的 legacy 明文迁移（`:959-965`）
- **解密失败不回退明文**：`secrets.rs:15-16` + `:79-80` 文档
- **其他 0o600 写者**：daemon discovery `daemon.json`
  （`crates/qaqh-daemon/src/server.rs:514-518`，`restrict_discovery_permissions` `:524`）
  ——**该文件含 token 字段**（`crates/qaqh-types/src/discovery.rs`
  `pub struct DaemonDiscovery { pub endpoint: String, pub token: String, pub pid: u32, … }`）；
  审计账本 `crates/qaqh-workspace/src/audit/mod.rs:288-289`、`audit/v2.rs:345-346`

**脱敏（4 处，均已核实）**：

| 机制 | 位置 | 行为 |
|---|---|---|
| `safe_provider_error_body(body, api_key)` | `crates/qaqh-gate/src/types.rs:269-276` | `body.replace(api_key, "[REDACTED]")`（`:273`）后 `.chars().take(200)`（`:275`）；空 key 短路（`:270`）。调用点 `chat_completions_api.rs:237,243,1016,1023`；`message_api.rs:932,938,1035,1042`；`responses_api.rs:659,663,803,807` |
| `qaqh_mcp::sanitize::redact_secrets` | `crates/qaqh-mcp/src/sanitize.rs:12-22` | 最长值优先替换，固定标记 `[redacted]`，跳过空值 |
| `qaqh_sandbox::redact_text` | `crates/qaqh-sandbox/src/lib.rs:398-408` | 用于 `output_snippet`（`:387`） |
| `redact_image_payloads` | `crates/qaqh-runtime/src/agent/state/token_calibration.rs:44` | 图片载荷脱敏 |

- **鉴权 token**：daemon 侧 `crates/qaqh-daemon/src/server.rs:169` 随机生成；
  `:170-173` 仅在**非 loopback 且未显式给 token** 时把生成值打到 stderr
  （注释"临时跨端模式"）；`:200-205` 非 loopback 时 warn "no transport security"。
  discovery 文件里**含 token**（`:185-199`）。
  `/health` 曾回显 `token_len`，已移除（`control.rs:10-11` 注释"长度本身就是对凭据的旁路信息"）。

#### 日志

- **facade**：`log` crate（`Cargo.toml` 中 `log = "0.4"`；**未找到** `tracing` 作为日志 facade——
  `tracing` 仅在 `qaqh-daemon` 通过 `tower_http::trace::TraceLayer` 间接使用）
- **初始化位置**：`crates/qaqh-daemon/src/main.rs:9-48` `init_file_logging()`，
  在 `main()` 第 3 行调用（`:54`）
- **唯一 sink**：文件 `<数据目录>/qaqh-daemon.log`，`create + append`（`:39-42`）
  - 注释（`:36-37`）"与 `platform::data_dir()` 同根：多实例（QAQH_DATA_DIR）时日志必须
    落在各自数据根，否则两个 daemon 混写同一文件（冒烟测试实证）"
- **级别过滤**：`metadata.level() <= log::Level::Info`（`:13`）+ `set_max_level(Info)`（`:47`）
  ——即 **Debug/Trace 被丢弃**
- **格式**：`"[{unix_secs}] {level:<5} {target}: {args}"`（`:26-32`）
- **`flush()` 是空实现**（`:34` `fn flush(&self) {}`），但 `log()` 每次 `writeln!` 不显式 flush
  ——依赖 `File` 的缓冲行为
- **失败静默**：文件打不开时直接 `return`（`:43-45`），**无任何告警**
- **绕过 logger 的 `println!`/`eprintln!`**（非 `tests/` 目录、非 `examples/`）：
  共 **85 处**，集中在：
  | 文件 | 数量级 | 代表性行 |
  |---|---|---|
  | `crates/qaqh-daemon/src/main.rs` | 最多（CLI 子命令输出，合理） | `:69`、`:80`、`:155`、`:474` |
  | `crates/qaqh-runtime/src/agent/state/agent.rs` | 3 | `:1171`、`:1175` `eprintln!("DBG last: {:?}", ...)`、`:1480` |
  | `crates/qaqh-runtime/src/agent/loop_core.rs` | 3 | `:291`（engine panic）、`:442`（writer 死）、`:475`（stdin 断） |
  | `crates/qaqh-runtime/src/agent/prompt.rs` | 5 | `:192-195`、`:213`（prompt 尺寸统计） |
  | `crates/qaqh-runtime/src/agent/loop_dispatch_conversation.rs` | 1 | `:477` |
  | `crates/qaqh-runtime/src/ringing/hub.rs` | 1 | `:1842` |
  | `crates/qaqh-types/src/config.rs` | 4 | `:322`、`:330`、`:337`、`:341`（ConfigStore 写失败） |
  | `crates/qaqh-sandbox/src/lib.rs` / `bin/qaqh-sandbox-exec.rs` | 5 | `:70`、`:419`、`:426` |
  | `crates/qaqh-workspace/src/journal.rs` | CLI 子命令 | `:626`、`:657`、`:664` 等 |
  | `crates/qaqh-workspace/src/exec/tests.rs` | `#[cfg(test)]` | `:376`、`:422`、`:499` 等 |
  | `crates/qaqh-webui-gateway/src/lib.rs` | 1 | `:131`（listening 提示） |
  | `crates/qaqh-gate/src/transport.rs:14` | 仅注释 | 记录了一处**已删除**的 release 期 `eprintln!("[filter] 输出…")` stderr 污染缺陷 |

  其中 `crates/qaqh-runtime/src/agent/state/agent.rs:1175`
  `eprintln!("DBG last: {:?}", context.last().map(|m| m.content.clone()))` 会**打印消息正文到 stderr**，
  需确认其是否在条件分支内——本次**未确认**该行的门控条件。

### 8.5 测试：数量、分布、行为验证 vs 实现细节

#### 数量与分布

**总计 1,831 个测试函数** = `#[test]` **1,709** + `#[tokio::test` **122**
（`#\[test\]` 与 `#\[tokio::test` 精确匹配，大小写敏感）。
全仓**未找到** `rstest` / `test_case` / `serial_test`。

> 口径修正说明：更宽的正则（如 `#\[(tokio::)?test\]`）会**漏计** `#[tokio::test]`
> 的变体写法，必须分开精确匹配。

| crate | `#[test]` | `#[tokio::test]` | `mod tests {` | `tests/` 下 `.rs` 文件 |
|---|---|---|---|---|
| `qaqh-workspace` | 498 | 0 | 43 | 14 |
| `qaqh-runtime` | 372 | 13 | 41 | 36 |
| `qaqh-session` | 252 | 0 | 7 | 28 |
| `qaqh-gate` | 152 | 0 | 5 | 3 |
| `qaqh-config` | 99 | 0 | 4 | 8 |
| `qaqh-message` | 69 | 0 | 3 | 2 |
| `qaqh-client` | 56 | 6 | 9 | 4 |
| `qaqh-types` | 33 | 0 | 5 | 0 |
| `qaqh-daemon` | 26 | 42 | 7 | 1 |
| `qaqh-lsp` | 21 | 4 | 4 | 1 |
| `qaqh-ringing` | 21 | 0 | 5 | 0 |
| `qaqh-skills` | 21 | 0 | 2 | 0 |
| `qaqh-webui-gateway` | 18 | 3 | 4 | 0 |
| `qaqh-subagent` | 15 | 0 | 1 | 0 |
| `qaqh-domain` | 14 | 0 | 2 | 0 |
| `qaqh-policy` | 13 | 0 | 3 | 0 |
| `qaqh-mcp` | 11 | 53 | 4 | 8 |
| `qaqh-sandbox` | 7 | 0 | 1 | 1 |
| `qaqh-config-api` | 6 | 0 | 1 | 0 |
| `qaqh-title` | 2 | 0 | 1 | 0 |
| **合计** | **1,709** | **122** | **152** | **106** |

`tests/` 目录：**11 个 crate** 有（client、config、daemon、gate、lsp、mcp、message、
runtime、sandbox、session、workspace），共 **192 个文件、其中 106 个 `.rs`**
（`qaqh-workspace/tests` 有 97 个文件但仅 14 个 `.rs`——83 个是非 Rust fixture）。

**`#[ignore]` = 5**（逐条已核实）：

| 位置 | 原因 |
|---|---|
| `crates/qaqh-client/tests/lease_renegotiation.rs:197` | "requires compiled daemon binary; run with `-- --ignored`" |
| `crates/qaqh-client/tests/lease_renegotiation.rs:429` | 同上 |
| `crates/qaqh-runtime/tests/timeline_load_latency_probe.rs:7` | 模块级 ignore |
| `crates/qaqh-runtime/tests/timeline_load_latency_probe.rs:98` | "measurement harness; run explicitly with `--ignored --nocapture`" |
| `crates/qaqh-runtime/tests/timeline_load_latency_probe.rs:183` | 同上 |

**`#[should_panic]` = 1**：`crates/qaqh-message/src/store.rs:2258`，
`expected = "push_system 只能用于会话建立"`。

**按位置拆分**：

| 位置 | 测试数 |
|---|---|
| `crates/*/tests/`（集成测试目录） | **530**（463 `#[test]` + 67 `#[tokio::test]`） |
| 源文件内 | **1,297** |
| ├─ `src/**/tests.rs`（out-of-file 单元模块） | 110 |
| └─ 其余 `src/*.rs` 内联（`mod tests` 块 + 模块级裸 `#[test]`） | 1,187 |

**266 个文件含至少一个测试**，这些文件合计 137,483 行。

`#[cfg(test)] mod tests;` 形式的外置测试模块：
`crates/qaqh-workspace/src/edit/mod.rs:24`（→ `src/edit/tests.rs` 465 行）、
`exec/mod.rs:41`（→ `src/exec/tests.rs` 1,424 行）、
`todo/mod.rs:26`（→ `src/todo/tests.rs` 687 行）；
`crates/qaqh-gate/src/lib.rs:16` `#[cfg(test)] mod rt_test;`（→ `src/rt_test.rs` 16 行）；
`crates/qaqh-workspace/src/lib.rs:901` `#[cfg(test)] mod schema_spot_check;`；
**未门控** `pub mod turn_lap_test_api;`（`crates/qaqh-runtime/src/agent/mod.rs:72`）。

**集成测试文件分布**（`crates/*/tests/*.rs`）：

| crate | 测试文件数 |
|---|---|
| `qaqh-runtime` | 36 |
| `qaqh-session` | 28 |
| `qaqh-workspace` | 14 |
| `qaqh-config` | 8 |
| `qaqh-mcp` | 8 |
| `qaqh-client` | 4 |
| `qaqh-gate` | 3 |
| `qaqh-message` | 2 |
| `qaqh-daemon` / `qaqh-lsp` / `qaqh-sandbox` | 各 1 |

**最大的测试文件**：
- `crates/qaqh-gate/tests/gate_test.rs`（1,652 行）
- `crates/qaqh-gate/tests/common/mock_server.rs`（286 行）
- 前端：`webui/tests/diff-parse.test.ts`（114 行）、`reducer.test.ts`（108 行）

**测试规模对照**：`qaqh-config` 生产 5,696 行 / 100 测试；`qaqh-policy` 495 行 / 13 测试；
`qaqh-gate` 9,250 行（含 `tests/gate_test.rs` 1,652 行）/ 154 测试。

#### 真正验证行为 vs 断言实现细节

**验证真实行为（占绝大多数）** —— 判据：断言的是跨模块可观察的输入/输出契约、
持久化产物字节、或状态机的对外效果。典型例证：

- **字节级持久化契约**：`crates/qaqh-message/tests/persist_effects.rs:246`
  断言两种写入路径产生 **byte-identical** 的 `messages.jsonl`：
  `"messages.jsonl must be byte-identical between the legacy inline path and the queue path"`
- **磁盘文件内容断言**：`crates/qaqh-session/tests/save_append_watermark.rs:127-215`
  直接读写 `messages.jsonl` 并比对行内容
- **canonical fact 序列断言**：`crates/qaqh-session/tests/team_task_board.rs:384-396`、
  `crates/qaqh-session/tests/message_board.rs:297-305` 读 `events.jsonl` 校验 fact
- **重启/崩溃恢复**：`crates/qaqh-runtime/tests/timeline_rebuild.rs:81`、
  `:121`（从 `messages.jsonl` 重建 timeline）；
  `crates/qaqh-runtime/tests/restart_prefix_cache.rs:13` 注释"走**真实磁盘**"
- **升级栅栏**：`crates/qaqh-session/tests/upgrade_fence.rs:88-110`
  断言 `events.jsonl` 与 `events.commit.json` 字节不变
- **协议端到端**：`crates/qaqh-gate/tests/gate_test.rs` 用
  `crates/qaqh-gate/tests/common/mock_server.rs`（`tiny_http`）打真实 HTTP
- **取消不丢结果**：`crates/qaqh-runtime/tests/cancel_keeps_tool_results.rs`
- **工具顺序契约**：`crates/qaqh-runtime/tests/tool_ordering_contract.rs`

**断言实现细节 —— 判据在计数前固定**：一个测试若断言的不是外部可观察行为，即为
"实现细节导向"：

- **D1** 断言的比较/失败信息依赖 `Debug` 格式化（`{…:?}`）
- **D2** 测试只能经 test-only 钩子（`*_for_test(…)` / `*_for_tests(…)`）触达内部状态
- **D3** 断言精确的方括号日志/标签串（`"[COMPACT]"`、`"[SYSTEM]"`、`"[DONE]"` …）
- **D4** `assert_eq!(<expr>.len(), <字面量>)`——内部集合的精确计数

| 判据 | 出现次数 | 不同测试函数数 |
|---|---|---|
| D1 DebugFmt | 5 | 3 |
| D2 ForTestHook | 17 | 9 |
| D3 LogString | 9 | 6 |
| D4 LenCounter | 206 | 153 |
| **并集** | — | **171 / 1,831 = 9.3%** |

第 5 条判据（"`assert_eq!` 对结构体字面量"）实测 **0 命中**，已弃用。

**置信度**：D1/D2/D3 高（每个命中都无歧义）；**D4 中**——精确长度常常是外部契约
唯一的可表达形式（例如"provider 注册表恰好暴露 N 个 model"），
因此 **9.3% 是"真正与实现耦合的测试"的上界**。

已定位的具体例子：

**D1**：`crates/qaqh-gate/tests/gate_test.rs:46,1099,1333`；
`crates/qaqh-workspace/src/permission.rs:1088,1099`（同在
`shell_cwd_and_patch_targets_enter_authorization_resources`）。

**D2**：`crates/qaqh-client/tests/lease_renew_timeout.rs:113,134,145`
（`adopt_for_test` / `run_renewal_for_test` / `renew_failures_for_test`）；
`crates/qaqh-mcp/tests/resources.rs:832`（`arm_idle_reclaim_for_tests`）；
`crates/qaqh-runtime/src/timeline.rs:2043`（`set_journal_byte_limit_for_test`）。

**D3**：`crates/qaqh-config/src/config.rs:1446` `assert!(text.contains("[exec]"))`；
`crates/qaqh-gate/src/sse.rs:183` `assert_eq!(d.next_frame(), Some(Ok("[DONE]".into())))`；
`crates/qaqh-runtime/src/agent/plugins/engine_compact.rs:811`
`assert!(user_text.starts_with("[COMPACT]"))`；
`crates/qaqh-subagent/src/lib.rs:1813,1816,1826`（`[SYSTEM]`/`[CONTEXT]`/`[TASK]` 顺序）。

**D4**：`crates/qaqh-config/src/registry.rs:826`
`assert_eq!(endpoint.models.len(), 15)`（`workbuddy_proxy_endpoint_exists`）；
`crates/qaqh-client/src/v2.rs:681`；`crates/qaqh-gate/src/message_api.rs:1302`。

此外前端另有 8 处（见 §7.6），判据为 (a) 内部结构形状 / (b) 精确文案 / (c) 绑实现常量：
`webui/tests/reducer.test.ts:83`（`turns["#3"]` 耦合 `turnKeyOf` 的 key 格式）、
`:104`（`slots[0]` 完整结构深比较）、`webui/tests/pagination.test.ts:43`、`:46-48`、`:55`、`:59`、
`webui/tests/diff-parse.test.ts:87`（`2001` 锚定 2000 阈值）、
`webui/tests/time.test.ts:61-64`（`6` 硬编码对应 `reconnect.ts:8` 常量）。

**更保守的对照项（可辩护为行为断言）**：
`crates/qaqh-message/src/store.rs:2760,2773` 与
`crates/qaqh-session/src/manager.rs:1972` 断言 `compact_covered_through_msg_id`
——它是 `meta.json` 的**公开序列化字段**，断言它有持久化契约的正当性；
`crates/qaqh-runtime/src/agent/turn_actor.rs:635-980` 的 10 个测试断言
`TurnEffect`/`TurnCoreState` 内部变体，属状态机核心契约。

#### 生产代码中的测试钩子（已逐一核实门控状态）

| 钩子 | 位置 | 门控 | 生产可达性 |
|---|---|---|---|
| daemon 故障注入 | `crates/qaqh-daemon/src/axum_server/axum_impl/test_hooks.rs`（443 行） | **未门控** `pub(crate) mod test_hooks;`（`axum_impl/mod.rs:46`） | **是**。字段 `mod.rs:98`；`crates/qaqh-daemon/src/server.rs:260` `TestHooks::from_env()` **在生产装配路径上构造**；环境变量族 `QAQH_TEST_SSE_TERMINATE` / `_SKIPPED` / `_SCOPE` / `QAQH_TEST_TIMELINE_GAP` / `QAQH_TEST_SESSION_404_SEED` / `QAQH_TEST_COMMAND_ACK` / `_CHANNEL` / `_COMMAND` / `QAQH_TEST_INTERACTION_FAULT`（`test_hooks.rs:87-129`）。**在真实请求路径上被查询**：`command.rs:162`、`sse.rs:142`、`sse.rs:145`、`sse.rs:175`、`timeline_api.rs:72` |
| runtime 故障注入 | `crates/qaqh-runtime/src/test_hooks.rs`（48 行） | `pub(crate) mod test_hooks;`（`lib.rs:13`，**未门控**） | 被 `engine_turn.rs:1492`、`:1494` 消费（plan_review 测试钩子） |
| loop 测试 API | `crates/qaqh-runtime/src/agent/turn_lap_test_api.rs`（178 行） | **未门控** `pub mod turn_lap_test_api;`（`agent/mod.rs:72`） | **是**。导出 `execute_admitted_batch:19`、`observe_yield_for_test:47`、`observe_ask_yield_for_test:85`、`observe_permission_yield_for_test:131`、`record_interaction_resolution_for_test:170` |
| panic 注入缝 | `crates/qaqh-runtime/src/agent/loop_core.rs:205-206` | `#[cfg(test)]`（`:205`），使用点 `:628-631` 同样 `#[cfg(test)]` | **否**（注释 `:203-204` 保证"never enters a production binary"） |
| harness feature | `crates/qaqh-message/src/lib.rs:25`；`src/wal.rs:523,572,587,601,611,631,707,880`（9 个正向臂 + `:527`、`:613` 负向臂） | `#[cfg(any(test, feature = "test-harness"))]` | 仅 `test-harness` feature 开启时 |
| `#[doc(hidden)]` 钩子 | `crates/qaqh-client/src/session.rs:319-320,346-348,353-355`；`crates/qaqh-config/src/secrets.rs:350-352`；`crates/qaqh-mcp/src/connection.rs:566-568`；`crates/qaqh-runtime/src/ringing/lease_store.rs:177-178`；`crates/qaqh-session/src/manager.rs:161-162,173-174`；`crates/qaqh-workspace/src/process_registry.rs:763-764,769-770,775-776,786-787,804-805`；`crates/qaqh-workspace/src/audit/mod.rs:275-276` | 仅隐藏文档，**未门控** | **是**（但设计意图即"公开但不文档化"） |
| 正确门控的对照 | `crates/qaqh-runtime/src/agent/state/agent.rs:476-477`；`ringing/hub.rs:634-635`；`ringing/persistence_policy.rs:69-70`；`crates/qaqh-workspace/src/lib.rs:409-410`（`TEST_RUNTIME_SERIAL`） | `#[cfg(test)]` | **否** |

**`unsafe_code` 相关 lint 属性**：全仓**未找到** `#![allow(unsafe_code)]` /
`#[allow(unsafe_code)]` / `forbid(unsafe_code)`。仅有的 lint 属性是测试文件里的
`clippy::unwrap_used` allow，以及 3 处
`#![allow(clippy::result_large_err)]`（`crates/qaqh-subagent/src/lib.rs:27`、
`crates/qaqh-workspace/src/process_inspect.rs:7`、`crates/qaqh-workspace/src/skill.rs:8`）。

- **未找到** `#[cfg(feature = "test")]` 之类的 feature-gated 测试代码

---

## 第 9 步：复杂度热点

> **方法**：自建正则调用图（纯语法层，不解析宏/trait 动态分派）。
> 圈复杂度用 `1 + count(if|else if|match|for|while|loop|&&|\|\|)` 近似。
> 函数体边界用"下一个 `fn` 定义行"近似（会**高估**多函数嵌套块的函数长度）。
> 测试代码已包含在统计中（会在表中标注）。

### 9.1 被调用最多的函数（扇入 Top 10）

**按"不同调用方函数数量"统计**的多数被调函数是通用方法名（`new` / `clone` / `len` /
`is_empty` / `as_str` / `get` / `lock` / `contains` / `push` / `collect` / `default` /
`as_ref` / `derive` / `insert` / `open` / `write` / `ok` / `from`），
按名字匹配会与标准库同名项混淆，**该口径无法给出有意义的结果**。

改用 **codegraph 的符号精确口径**，得到本仓**架构上真正的扇入枢纽**
（`codegraph callers <symbol>`，数字为不同调用方定义数）：

| 符号 | 定义 | 调用方数 | 主要调用方（层） |
|---|---|---|---|
| `Emitter::emit_domain` | `crates/qaqh-runtime/src/agent/types.rs:416` | **21**（`PacedEmitter` 实现 13 + `RecordingEmitter` 实现 21/11 + tests） | loop / gate / tool / engine / lifecycle 全层 |
| `Emitter::emit_timeline` | `crates/qaqh-runtime/src/agent/types.rs:420` | **11** | tool 层（5）+ lap 层（8，去重后 11） |
| `TurnActor::apply` | `crates/qaqh-runtime/src/agent/turn_actor.rs:542` | 6 | `cancel_active` / `cancel` / `finish` / `suspend` / `round_started` / `start_with_input`（同文件内） |
| `ToolRuntime::execute_batch` | `crates/qaqh-runtime/src/agent/tool_runtime.rs:114` | 1 | `execute_admitted_batch`（`admit.rs:94`） |
| `ContextFlow::ingest` | `crates/qaqh-message/src/context_flow.rs:328` | 1（生产）+ 5（测试） | `parse_and_ingest`（`parse.rs:36`） |
| `ProgressSink::emit` | `crates/qaqh-workspace/src/tool_api/progress.rs:76` | 6 | `bridge_progress`、`drain_bounded`、`connect_once` 等 |
| `qaqh_gate::chat_stream` | `crates/qaqh-gate/src/lib.rs:61` | **codegraph 报 0（解析失败）** | 实际调用方 `gate_request`（`gate.rs:387`） |

**结论**：扇入集中在**两个事件发射点**（`emit_domain` 21、`emit_timeline` 11）。
这两点即 §2.1 调用链中所有引擎的**唯一输出出口**（`Emitter` trait 的设计意图，
见 `types.rs:409-412` 注释："This trait is the single point where all Ringing events
enter the output pipeline"）。**属于 agent loop 层**。

### 9.2 扇出最大的 10 个函数

按"体内调用的不同本仓 `fn` 数量"排序：

| 扇出 | 圈复杂度 | 行数 | 函数 | 位置 | 层 |
|---|---|---|---|---|---|
| **72** | 17 | 397 | `QaqhService::handle` | `crates/qaqh-runtime/src/service.rs:283` | **服务/RPC 层** |
| **45** | 38 | 423 | `TurnEngine::run_lap` | `crates/qaqh-runtime/src/agent/engine_turn.rs:1429` | **loop 层** |
| 45 | 16 | 208 | `server::run_with` | `crates/qaqh-daemon/src/server.rs:158` | daemon 装配 |
| 42 | 24 | 257 | `lifecycle::init_session` | `crates/qaqh-runtime/src/agent/state/lifecycle.rs:~135` | 会话恢复 |
| 39 | 18 | 372 | `qaqh_service_host_spawn_subscribe_send_close` | `crates/qaqh-runtime/tests/host_direct.rs:36` | **测试** |
| **35** | 28 | 348 | `Loop::on_conversation` | `crates/qaqh-runtime/src/agent/loop_dispatch_conversation.rs:199` | **loop 层** |
| 34 | 26 | 392 | `execute_authorized_with_context` | `crates/qaqh-workspace/src/execution.rs:78` | **工具执行层** |
| **31** | 23 | 195 | `chat_stream_openai` | `crates/qaqh-gate/src/chat_completions_api.rs:69` | **网关层** |
| 31 | 8 | 101 | `plan_review_hook_yields_then_resumes_on_approve_and_reject` | `crates/qaqh-runtime/tests/plan_review_hook.rs` | **测试** |
| 30 | 6 | 266 | `delivery_reloads_unloaded_child_through_loaded_parent` | `crates/qaqh-runtime/tests/host_direct.rs` | **测试** |

**非测试的前 7 名中有 5 个落在 loop / 网关 / 工具执行三层**。

### 9.3 最长的 10 个函数

按"定义行 → 下一个 `fn` 定义行"的跨度（**该方法会高估**，因嵌套块内的
闭包/局部 `fn` 会被当作边界）：

| 跨度（行） | 函数 | 位置 | 层 |
|---|---|---|---|
| **515** | `execute_command` | `crates/qaqh-daemon/src/axum_server/axum_impl/command.rs:62` | **协议/命令层** |
| 505 | `ProjectionPayload::reliable_slot` | `crates/qaqh-session/src/session_fact_v2/projection_event.rs:315` | 协议类型层 |
| **423** | `TurnEngine::run_lap` | `crates/qaqh-runtime/src/agent/engine_turn.rs:1429` | **loop 层** |
| 397 | `QaqhService::handle` | `crates/qaqh-runtime/src/service.rs:283` | 服务层 |
| 392 | `execute_authorized_with_context` | `crates/qaqh-workspace/src/execution.rs:78` | 工具层 |
| 391 | `Default::default`（巨型字面量构造） | `crates/qaqh-session/src/session_fact_v2/types.rs:126` | 类型层 |
| 385 | `direct_exec_inner` | `crates/qaqh-workspace/src/exec/direct.rs:77` | 工具层 |
| 382 | `is_zero_u64` | `crates/qaqh-domain/src/timeline.rs:225` | 领域类型 |
| 372 | `qaqh_service_host_spawn_subscribe_send_close` | `crates/qaqh-runtime/tests/host_direct.rs:36` | **测试** |
| **367** | `gate_request` | `crates/qaqh-runtime/src/agent/turn_lap/gate.rs:345` | **网关/loop 层** |

**`execute_command`（515 行）与 `run_lap`（423 行）是真实存在的巨型函数**
（两者均在前面的扇出榜上也居前，交叉印证）。

### 9.4 圈复杂度明显偏高的函数

| 圈复杂度（近似） | 扇出 | 行数 | 函数 | 位置 | 层 |
|---|---|---|---|---|---|
| **76** | 20 | 335 | `load_from_paths_with` | `crates/qaqh-config/src/config.rs:702` | **配置层** |
| **51** | 24 | 327 | `validate`（canonical fact 校验） | `crates/qaqh-session/src/session_fact_v2/validation.rs:185` | **协议/事实层** |
| 42 | 8 | 207 | `process_line` | `crates/qaqh-workspace/src/apply_patch_engine/streaming_parser.rs:198` | **工具层**（补丁解析） |
| 41 | 10 | 142 | `sanitize_openai_schema` | `crates/qaqh-gate/src/responses_api.rs:323` | **网关层** |
| 39 | 29 | 385 | `direct_exec_inner` | `crates/qaqh-workspace/src/exec/direct.rs:77` | **工具层** |
| 39 | 8 | 121 | `validate` | `crates/qaqh-session/src/canonical/replay_window.rs:119` | 恢复层 |
| **38** | 45 | 423 | `TurnEngine::run_lap` | `crates/qaqh-runtime/src/agent/engine_turn.rs:1429` | **loop 层** |
| **37** | 30 | 515 | `execute_command` | `crates/qaqh-daemon/src/axum_server/axum_impl/command.rs:62` | **协议层** |
| 35 | 18 | 229 | `read_one` | `crates/qaqh-workspace/src/file_query.rs:172` | 工具层 |
| 34 | 15 | 266 | `collect_subagent_result` | `crates/qaqh-subagent/src/lib.rs:1372` | **子代理层** |
| 34 | 2 | 139 | `apply_to` | `crates/qaqh-types/src/provider.rs:350` | 配置类型 |
| 33 | 15 | 254 | `convert_messages_to_anthropic` | `crates/qaqh-gate/src/message_api.rs:71` | **网关层** |

### 9.5 是否集中在 loop / 网关 / 协议层？

**是，明显集中。** 交叉汇总四个维度（仅计非测试项）：

| 层 | 出现在扇出 Top10 | 出现在最长 Top10 | 出现在复杂度 Top12 | 合计命中 |
|---|---|---|---|---|
| **loop 层**（`agent/`） | `run_lap`、`on_conversation`、`init_session` | `run_lap`、`gate_request` | `run_lap` | **5** |
| **网关层**（`qaqh-gate`） | `chat_stream_openai` | — | `sanitize_openai_schema`、`convert_messages_to_anthropic` | **3** |
| **协议/命令层**（daemon / ringing / session_fact） | — | `execute_command`、`reliable_slot`、`Default::default` | `execute_command`、`validate`(×2) | **5** |
| **工具执行层**（`qaqh-workspace`） | `execute_authorized_with_context` | `execute_authorized_with_context`、`direct_exec_inner` | `direct_exec_inner`、`process_line`、`read_one` | **5** |
| 服务/装配层 | `QaqhService::handle`、`server::run_with` | `QaqhService::handle` | — | 3 |
| 配置层 | — | — | `load_from_paths_with`（**最高复杂度 76**） | 1 |

**三个最突出的单点**（同时命中多个维度）：

1. **`TurnEngine::run_lap`** —— `crates/qaqh-runtime/src/agent/engine_turn.rs:1429`：
   扇出 45（第 2）、圈复杂度 38（第 7）、行数 423（第 3）。**loop 层的绝对热点。**
2. **`execute_command`** —— `crates/qaqh-daemon/src/axum_server/axum_impl/command.rs:62`：
   行数 515（第 1）、圈复杂度 37（第 8）、扇出 30。**协议层的绝对热点。**
3. **`load_from_paths_with`** —— `crates/qaqh-config/src/config.rs:703`：
   圈复杂度 76（第 1，是第 2 名的 1.5 倍）。**配置层单点最复杂。**

**loop → 网关 → 协议这条主链上的热点**（`on_conversation` → `run_lap` → `gate_request` →
`chat_stream_*` → `execute_command`）**恰好覆盖了 §2.1 完整调用链的每一跳**，
即该链上每一跳都是一个复杂度热点。

---

## 未能确认的问题清单

以下为本报告**明确未能确认**的项。均标注了我做到的程度，**未做推测**。

> **状态更新**：§8 的 A1–A5 五项原列为"未完成"，后经一次独立交叉审计完成，
> 结果已并入 §8.1 / §8.2 / §8.3 / §8.5。原条目保留在下方 A′ 以便追溯，
> 已确认部分不再列为未确认。

### A. 仍未完成的确认

1. **`crates/qaqh-runtime/src/agent/state/agent.rs:1175` 的门控条件。**
   该行 `eprintln!("DBG last: {:?}", context.last().map(|m| m.content.clone()))` 会打印
   **消息正文到 stderr**；**未确认**其外层是否有 debug 开关或 `#[cfg(test)]` 门控。
   同一文件 `:1480` 的 `eprintln!("[tool_schema:{mode}] {} tools -> {:?}", ...)` 同样未确认。
2. **`debug_assert*` 的生产/测试拆分未做。** 已确认 19 个真实宏调用全部在 `src/` 文件内
   （无一位于 `tests/` 目录），但未按 `#[cfg(test)]` 掩码进一步归属。
3. **D4 判据（精确 `.len()` 断言）的逐条人工复核未做。** 153 个不同测试函数命中，
   判据持有者自评"中等置信度"——精确长度常常是外部契约的唯一可表达形式。
   9.3% 应读作**上界**。
4. **每一处生产 `expect` 的语义分类未做。** 171 处中已归类出 72 处集中在
   `descriptor()` 的 `ToolName::new(...).expect(...)` 惯用法，其余 99 处未逐一判定
   是否为"不可能失败"（如 `OnceLock` 初始化、正则编译）还是真实可失败路径。

### A′. 已由独立审计完成的原条目（保留追溯）

1. ~~`unwrap`/`expect`/`panic` 的生产 vs 测试拆分~~ → 已完成：生产
   `unwrap` **0**、`expect` **171**、`panic` **5**、`unreachable` **8**（§8.1）。
2. ~~5 个 `#[ignore]` 与 1 个 `#[should_panic]` 的名称与原因~~ → 已列出（§8.5）。
3. ~~"只断言实现细节"的测试总数~~ → 已完成：**171 / 1,831 = 9.3%**，
   判据 D1–D4 固定于计数前（§8.5）。
4. ~~生产 `unsafe` 的精确边界~~ → 已完成：生产 **29**、测试 **63**，含 29 处完整清单（§8.2）。
5. ~~`#[ignore]` 计数~~ → 修正为 **5** 处（含 `timeline_load_latency_probe.rs:7` 的模块级 ignore）。

### B. 需要动态运行才能确认的问题

6. **`codegraph` 报 `chat_stream` 有 0 个调用方**（`crates/qaqh-gate/src/lib.rs:61`），
   但实际调用点在 `crates/qaqh-runtime/src/agent/turn_lap/gate.rs:387`。
   已判定为**工具解析跨 crate 全限定路径的局限**，但**未验证**是否还有其它
   同类漏报——因此 §9.1 的扇入口径可能系统性偏低。
7. **`is_retryable` 被三协议适配器实际调用的次数与位置。** 已确认它是
   `pub(crate)` 且 `run_with_retry` 是唯一重试循环，但**未逐协议核对**
   每个适配器在何处把 HTTP status 传给 `is_retryable`。
8. **`crates/qaqh-runtime/src/ringing/hub.rs:1842` 的 `eprintln!` 内容与触发路径**未展开。
9. **`qaqh-webui-gateway` 的 `proxy_*` 系列如何转发 SSE 流（是否逐帧透传、
   是否重写 event id）**。仅确认了路由与 handler 名（`lib.rs:283-298`、
   `:911` `proxy_events`、`:960` `proxy_timeline_events`），**未读 handler 实现体**。
10. **`crates/qaqh-runtime/src/discovery.rs` 等 3 处生产 `unsafe` 的具体 FFI 调用**。
    已定位行号（`qaqh-client/src/discovery.rs:285,295,300`），未展开其调用的
    具体 syscall/API。

### C. 需要产品决策才能回答的问题

11. **无前端时 turn 无限期挂起是否是刻意设计。** 已用事实确认"无时间驱动的
    interaction 过期"（`InteractionExpiryReason` 唯一生产写入点
    `crates/qaqh-session/src/actor.rs:626-631`，reason 恒为 `TurnCancelled`），
    但**未找到**任何说明该行为是刻意取舍的文档或注释。
12. **daemon 重启后，canonical 里仍为 `pending` 的 `InteractionRequested`
    会不会被恢复流程显式 seal。** 已确认 `/approvals` 能读回它
    （`v2.rs:362-398`），且内存 `suspended` turn 丢失；**未找到**把已悬挂 turn
    重新接回 `TurnEngine.suspended` 的代码路径，但**也未找到**显式 seal 它的代码路径
    ——即该 fact 在重启后可能长期停留在 pending。
13. **`qaqh-message` 的 dev-dependency 反向引用 `qaqh-session` 是否为刻意设计。**
    已确认这是事实（`crates/qaqh-message/Cargo.toml:21`），且生产代码零引用
    （`src/` 内 grep `qaqh_session` → 0 命中），但**未找到**注释说明为何测试需要
    反向依赖（`crates/qaqh-session/Cargo.toml:25` 的注释只解释了 feature 一侧：
    "Test-only: deterministic WAL read-fault injection"）。

### D. 文档与代码不一致（已确认事实，非未确认项）

以下三项在任务书前提与代码事实之间不一致，已在正文中给出证据与行号：

14. **SSE 流不是"tools/messages/history 三条"**，而是 2 条
    （`/events` 与 `/timeline/events`），且 v1 三频道已被硬切删除（§4.0）。
15. **前端不是 React，是 SolidJS 2.0-rc**（§7.0）。
16. **本仓库不含 WinUI3 前端**，已拆分至 `F:\qaqh-winui-app`（`justfile:7-9`）（§7.0）。
17. **Windows 上"ACL 同步收紧"的注释与代码不符。**
    `crates/qaqh-config/src/secrets.rs:322` 注释称锁文件
    "0600（Unix）；**Windows ACL 同步收紧**"，但 `#[cfg(not(unix))]` 分支
    `:509-510` 的函数体是**空的**（`fn restrict_permissions(_path: &Path) {}`），
    Windows 不做任何权限收紧。该函数同时被 `:323`（lock）、`:371`（tmp）、
    `:386`（final）三处调用。事实影响有限（Windows 侧凭据由 DPAPI 加密），
    但注释描述的机制不存在（§8.4）。
18. **`justfile:5` 注释称 `crates/` 有 17 个 crate**，而 `Cargo.toml:3-24`
    实际列出 **20** 个成员（§1.1）。
19. **版本号四处不一致（已确认全部取值）**：

    | 文件:行 | 值 |
    |---|---|
    | `version.txt`（内容） | `2.0.0-alpha4` |
    | `Cargo.toml:32`（`[workspace.package] version`） | `2.0.0-beta.1` |
    | `package.json:4` | `2.0.0-beta.1` |
    | `README.md:10`（"当前代码基线"） | `2.0.0-alpha2` |
    | `qaqh-backend.lock.json` | `2.0.0-alpha1`（另含 `protocol_version: 1`、`git_commit: f555fa29…`） |

    `scripts/sync-version.ps1:21-34` 的设计意图是**从 `version.txt` 单向同步到
    `Cargo.toml`（`[workspace.package]`）与根 `package.json`**（脚本自检于 `:30-33`），
    即 `version.txt` 是指定的事实源。按该契约，`Cargo.toml` 与 `package.json`
    应为 `2.0.0-alpha4`，实际停留在 `2.0.0-beta.1`——表明该脚本在本仓库**当前未被运行**。
    `qaqh-backend.lock.json` 不在该脚本的覆盖范围内（脚本 `:3` 注释明确
    "后端版本锁…由 QAQ-Harness 后端仓库维护，不在此同步"）。

---

## 附：本次分析使用的主要命令

```powershell
# 索引（codegraph 索引在本仓库此前不存在，本次为首次建立）
codegraph --help
codegraph init -y            # 产物仅落 .codegraph/（该目录已被自身 .gitignore 忽略）
codegraph status

# 符号级查询
codegraph query <symbol> -l N
codegraph callers <symbol> -l N
codegraph files --filter crates/qaqh-runtime
codegraph node <symbol>

# 结构统计（本报告 §1）
Get-ChildItem crates -Directory
Get-ChildItem crates -Recurse -File -Include *.rs |
  ForEach-Object { (Get-Content $_ | Measure-Object -Line).Lines }
# 复核：ReadAllLines 与 Measure-Object -Line 结果一致 = 167,957 行 / 438 文件

# 复杂度统计（本报告 §9）
#   - 函数定义：'^\s*(?:pub(?:\([a-z]+\))?\s+)?(?:async\s+)?fn\s+([a-z_][a-z0-9_]*)'
#   - 扇出：函数体窗口内 '(?<![\w"])([a-z_][a-z0-9_]*)\s*\(' 去重（限本仓已定义符号）
#   - 圈复杂度：1 + count('\b(if|else if|match|for|while|loop)\b|&&|\|\|')
#   - 函数长度：定义行 → 下一个 fn 定义行（该方法会高估，见 §9.3 说明）

# 健康度统计（本报告 §8）—— 注意 PowerShell 默认大小写不敏感，需 -CaseSensitive
'\.unwrap\(\)' / '\.expect\(' / 'panic!\(' / 'unreachable!\(' / '(todo!|unimplemented!)\('
'#\[test\]' / '#\[tokio::test'      # 必须分开匹配：合并正则会漏计
'#\[ignore' / '#\[should_panic'
'(^|[^a-zA-Z_.])(println!|eprintln!)'
'unsafe\s*(\{|impl\b|fn\b|extern\b|trait\b)'

# 生产/测试拆分（§8.1 / §8.2）—— 四桶法
#   1) 路径含 \tests\ 或文件名为 tests.rs ⇒ 100% 测试
#   2) 行掩码：'^\s*#\[(cfg\(\s*(all\(\s*)?test\b|test\b|tokio::test\b|rstest\b|test_case\b|serial_test)'
#      括号深度感知地跳过属性行，花括号配对到项闭合
#   3) 独立桶：'#[cfg(any(test, feature = "test-harness"))]'
#   4) 计数前剥离注释/字符串（有状态扫描器，状态跨行保持）
```
