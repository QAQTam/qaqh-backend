# 架构工程规范：现代纯血异步 ThinAgentLoop 重构规约
**编号**: SPEC-2026-10-29-02  
**状态**: 现行立宪（APPROVED）  
**涉案范围**: `crates/qaqh-runtime`, `crates/qaqh-session`, `crates/qaqh-daemon`, `crates/qaqh-gate`, `crates/qaqh-workspace`  
**核心目标**: 彻底解构 22,011 行旧版 Agent 巨石，淘汰 `AgentState` 上帝对象与 8 路借用 `RingContext`，建立纯异步、极薄编排、内存常数级 O(1) 的现代化 `ThinAgentLoop` 体系。

---

## 0. 本规范原则与交付禁令

### 0.1 交付原则
1. **单一真源事实**：执行状态的唯一事实是 Canonical `events.jsonl`（`SessionFact`），Loop 内部不再维护对等的“内存影子状态机”（如已废弃的 `turn_actor`）。
2. **极薄编排（Thin Orchestration）**：Loop 本身只充当异步状态机调度器，自身不持有大型上下文缓冲区、不管理网络重试细节、不承载沙箱与工具执行逻辑。
3. **纯血异步（Async-First）**：全链路依托 Tokio 绿色轻量 Task，彻底消灭每个 Session 独立占用 2 个物理 OS 线程的封建机制。
4. **即死即焚（Purge without Regret）**：禁止引入过渡适配器或兼容 Facade。旧版巨石解耦完成后，原 `agent/` 目录中涉及旧状态机与多路分派的死代码一律物理删除。

### 0.2 不允许的折中妥协（Anti-Patterns）
- **严禁“换汤不换药”**：新建 `ThinAgentLoop`，内部却继续持有 `AgentState` 或通过 `Arc<Mutex<...>>` 搬进旧版状态大杂烩。
- **严禁保留全局写锁**：以“防止写入并发冲突”为借口保留 `LegacyWriterFacade::lock()`。
- **严禁在 Loop 内部重试网络**：网络抖动、429/503 退避必须由 `qaqh-gate` 透明层完全截获，严禁在 Loop 状态机中书写补偿回滚分支。
- **严禁大容量通道兜底**：使用超过容量 64 的通道来遮蔽并发阻塞问题。

---

## 1. 现状盘点与解耦边界分析

### 1.1 现有 22,011 行代码定性分布
经过源码级审计与物理行数盘点，现有 `crates/qaqh-runtime/src/agent/` 的 46 个文件被严格划分为三类：

| 资产类别 | 物理文件与模块 | 行数规模 | 重构去向与处置方案 |
| :--- | :--- | :--- | :--- |
| **📦 纯搬运资产<br>(成熟逻辑，免重写)** | `tool_runtime.rs` (1,506L)<br>`engine_tool.rs` (1,242L)<br>`plugins/engine_compact.rs` (964L)<br>`prompt.rs` (322L)<br>`workspace_audit.rs` (682L)<br>`tool_recovery.rs` (512L)<br>`context.rs` (330L)<br>`official_web.rs` (155L) 等 | **约 13,000 行** (~60%) | **原样剥离与平移**：<br>1. 沙箱/超时/并发执行算法 ➔ 独立抽离为 `ToolRunner`；<br>2. 压缩裁决与摘要策略 ➔ 平移为后台异步 `CompactionWorker`；<br>3. 提示词拼装 ➔ 平移为纯函数 `ContextPipeline`。 |
| **🗑️ 历史垃圾包袱<br>(立即焚毁，无留存价值)** | `turn_actor.rs` (1,054L)<br>`loop_injection.rs` (758L)<br>`injection.rs` (405L)<br>`state/token_calibration.rs` (513L)<br>`loop_dispatch_*.rs` (共 1,312L)<br>`turn_lap/gate.rs` (内层重试回滚段 800L)<br>`state/lifecycle.rs` (旧双写段 600L) | **约 8,500 行** (~38%) | **物理删除 (`git rm`)**：<br>1. 影子镜像状态机 `turn_actor` 彻底作废；<br>2. 破坏上下文因果律的中途强插总线 `loop_injection` 废弃；<br>3. 8 路借用的 `RingContext` 调度胶水销毁；<br>4. 遥测无消费的 `token_calibration` 拔线。 |
| **🔨 现代核心重写<br>(高密状态机，半天可成)** | 新建 `thin_agent/loop_core.rs`<br>新建 `thin_agent/state.rs`<br>新建 `thin_agent/contracts.rs` | **仅 400 ~ 600 行** (~2%) | **纯手写现代化状态机**：<br>基于 Tokio MPSC，单向流调度，严格依照 Ringing v2 契约落盘。 |

---

## 2. 目标系统架构与数据流

新架构将“巨石”切割为四个拥有明确所有权边界的单向流组件：

```
                              ┌───────────────────────────────────┐
                              │     actor.rs / Tokio Task 入口    │
                              └─────────────────┬─────────────────┘
                                                │ Tokio MPSC (深度: 32)
                                                ▼
┌─────────────────────────────────────────────────────────────────────────────────────────┐
│                               ThinAgentLoop (纯异步状态机)                              │
│                                                                                         │
│   1. 从 Mailbox 消费 TurnInput（用户指令 / 工具审批输入 / 终结控制命令）                 │
│                                      │                                                  │
│                                      ▼                                                  │
│   2. 驱动 ContextPipeline 拼装请求模型 ◄─── 无状态读取 events.jsonl + Skills + MCP      │
│                                      │                                                  │
│                                      ▼                                                  │
│   3. 请求模型推演 (GateClient) ──────┴───► 透明层自动指数退避重试 (对 Loop 完全透明)     │
│         │                                                                               │
│         ├─► 模型流式输出 Chunk ─────────► 追加 FactPayload::AssistantBlockSealed        │
│         │                                                                               │
│         └─► 模型决策 ToolCall ──────────► 追加 FactPayload::ToolIntent                  │
│                                      │                                                  │
│                                      ▼                                                  │
│   4. 派发工具执行 (ToolRunner) ──────────► 并发调度 / 沙箱执行 / 超时熔断               │
│                                      │                                                  │
│                                      ▼                                                  │
│   5. 收集 ToolOutcome ───────────────────► 追加 FactPayload::ToolFinished               │
│                                      │                                                  │
│                                      ▼                                                  │
│   6. 判断是否继续 Lap 或封卷 ────────────► 循环至步 2 或追加 FactPayload::TurnFinished   │
└─────────────────────────────────────────────────────────────────────────────────────────┘
                                       │ 写入事件
                                       ▼
┌─────────────────────────────────────────────────────────────────────────────────────────┐
│              CanonicalSessionStore (events.jsonl) —— 全系统单一真源事实                │
└─────────────────────────────────────────────────────────────────────────────────────────┘
                                       │ 投影消费
                                       ▼
                 Ringing v2 Hub (/events SSE) ──► 前端 qaqh-electron
```

---

## 3. 核心解耦组件与 Trait 契约定义

所有组件必须遵循显式参数输入、纯函数或无锁异步调用的设计准则。

### 3.1 事实账本抽象：`CanonicalLedger`
Loop 与磁盘持久化的唯一交互点，保证每条 `SessionFact` 递增有序、原子持久化：

```rust
use qaqh_session::session_fact_v2::types::{FactPayload, SessionFact};
use qaqh_session::canonical::CanonicalSessionStore;
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Clone)]
pub struct CanonicalLedger {
    store: Arc<Mutex<CanonicalSessionStore>>,
    session_id: String,
    log_id: String,
}

impl CanonicalLedger {
    pub fn new(store: Arc<Mutex<CanonicalSessionStore>>, session_id: String, log_id: String) -> Self {
        Self { store, session_id, log_id }
    }

    /// 向 events.jsonl 追加单条事实并刷盘，递增 fact_seq，返回赋予的 event_id
    pub async fn append_fact(
        &self,
        turn_id: Option<String>,
        causation_id: Option<String>,
        payload: FactPayload,
    ) -> Result<SessionFact, LedgerError> {
        let mut guard = self.store.lock().await;
        guard.append_payload(turn_id, causation_id, payload).await
    }

    /// 获取当前物理日志最新水位
    pub async fn current_fact_seq(&self) -> u64 {
        self.store.lock().await.current_fact_seq()
    }
}
```

### 3.2 无状态上下文构建器：`ContextPipeline`
彻底消灭长久驻留在内存中的全量消息树。只在发起 LLM 推演时，按需构建只读 `PreparedPrompt`：

```rust
use qaqh_types::message::Message;
use qaqh_types::ToolDef;

pub struct PreparedContext {
    pub system_instruction: String,
    pub messages: Vec<Message>,
    pub tool_definitions: Vec<ToolDef>,
}

#[async_trait::async_trait]
pub trait ContextPipeline: Send + Sync {
    /// 基于 Canonical 事实日志投影与当前环境，即时渲染供 Gate 使用的完整上下文
    async fn assemble_context(
        &self,
        session_id: &str,
        turn_id: &str,
    ) -> Result<PreparedContext, ContextError>;
}
```
**内存优化保证**：`assemble_context` 产出的 `PreparedContext` 仅在单次推理 Lap 中借用，推理结束即销毁，不在堆中持久保留。

### 3.3 透明网关客户端：`GateClient`
网络层负责一切断网、限流、超载退避，向 Loop 呈现确定性的单向结果：

```rust
use futures::Stream;
use std::pin::Pin;

pub enum ModelStreamChunk {
    TextDelta(String),
    ReasoningDelta(String),
    ToolCallAcc(qaqh_types::ToolCall),
}

#[derive(Debug)]
pub struct LapInferenceResult {
    pub assistant_text: String,
    pub reasoning_text: Option<String>,
    pub tool_calls: Vec<qaqh_types::ToolCall>,
    pub usage: qaqh_types::UsageInfo,
}

#[async_trait::async_trait]
pub trait GateClient: Send + Sync {
    /// 发起流式推理。底层透明实施指数退避重试（Jittered Backoff）
    async fn stream_round(
        &self,
        context: &PreparedContext,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<
        (
            LapInferenceResult,
            Pin<Box<dyn Stream<Item = ModelStreamChunk> + Send>>,
        ),
        GateError,
    >;
}
```

### 3.4 外部并发工具执行器：`ToolRunner`
将沙箱权限校验、进程拉起、超时熔断、大输出截断（`MAX_OUTPUT_BYTES`）全量封闭在内：

```rust
use qaqh_types::tool_result::ToolResult;

pub struct ToolExecutionIntent {
    pub call_id: String,
    pub tool_name: String,
    pub arguments: serde_json::Value,
    pub timeout_secs: u64,
}

pub struct ToolExecutionOutcome {
    pub call_id: String,
    pub tool_name: String,
    pub result: ToolResult,
    pub duration_ms: u64,
    pub output_bytes: usize,
}

#[async_trait::async_trait]
pub trait ToolRunner: Send + Sync {
    /// 并发批量执行工具意图，内部调度线程池、隔离沙箱与超时熔断
    async fn execute_batch(
        &self,
        session_id: &str,
        turn_id: &str,
        intents: Vec<ToolExecutionIntent>,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Vec<ToolExecutionOutcome>;
}
```

---

## 4. ThinAgentLoop 状态机规格

`ThinAgentLoop` 由纯异步事件循环驱动，状态严格单向演进，不存在回退或交叉借用。

```
     ┌──────────┐
 ┌──►│   Idle   │◄─────────────────────────────┐
 │   └────┬─────┘                              │
 │        │ Recv TurnInput                     │
 │        ▼                                    │
 │   ┌──────────┐                              │
 │   │ Admitting│                              │
 │   └────┬─────┘                              │
 │        │ InputAccepted 落盘                  │
 │        ▼                                    │
 │   ┌──────────┐                              │
 │   │ Building ├──────────────┐               │
 │   └────┬─────┘              │               │
 │        │ Prompt Ready       │               │
 │        ▼                    │               │
 │   ┌──────────┐              │               │
 │   │Inference ◄────────┐     │               │
 │   └────┬─────┘        │     │               │
 │        │ Stream Done  │     │               │
 │        ▼              │     │ (Next Lap)    │
 │   ┌──────────┐        │     │               │
 │   │Executing ├────────┘     │               │
 │   └────┬─────┘              │               │
 │        │ No More Tools      │               │
 │        ▼                    │               │
 │   ┌──────────┐              │               │
 │   │ Sealing  ├──────────────┘               │
 │   └────┬─────┘                              │
 │        │ TurnFinished 落盘                  │
 └────────┴────────────────────────────────────┘
```

### 4.1 核心状态枚举与上下文
```rust
pub enum LoopState {
    /// 等待客户端指令或输入
    Idle,
    /// 接纳输入：写入 InputAccepted 事实
    AdmittingInput { input: TurnInput },
    /// 构建推演上下文
    BuildingContext { turn_id: String },
    /// 正在向大模型流式推理
    StreamingInference { turn_id: String, lap_index: u32 },
    /// 执行工具调用意图
    ExecutingTools {
        turn_id: String,
        lap_index: u32,
        intents: Vec<ToolExecutionIntent>,
    },
    /// 回合封卷：写入 TurnFinished 事实
    SealingTurn { turn_id: String },
    /// 终结与清理：退出 Task
    Terminating,
}
```

### 4.2 极薄 Loop 驱动函数（核心逻辑骨架）
```rust
pub struct ThinAgentLoop<L, C, G, T> {
    ledger: L,
    pipeline: C,
    gate: G,
    tools: T,
    mailbox_rx: tokio::sync::mpsc::Receiver<TurnInput>,
    cancel_token: tokio_util::sync::CancellationToken,
}

impl<L, C, G, T> ThinAgentLoop<L, C, G, T>
where
    L: CanonicalLedgerTrait,
    C: ContextPipeline,
    G: GateClient,
    T: ToolRunner,
{
    pub async fn run(mut self) {
        let mut state = LoopState::Idle;

        loop {
            tokio::select! {
                _ = self.cancel_token.cancelled() => {
                    log::info!("[LOOP] Cancelled, sealing state");
                    break;
                }
                cmd = self.mailbox_rx.recv(), if matches!(state, LoopState::Idle) => {
                    match cmd {
                        Some(input) => state = LoopState::AdmittingInput { input },
                        None => break, // 通道对端挂断，会话优雅退出
                    }
                }
                _ = async { () }, if !matches!(state, LoopState::Idle) => {
                    state = self.step(state).await;
                }
            }
        }
    }

    async fn step(&mut self, current: LoopState) -> LoopState {
        match current {
            LoopState::AdmittingInput { input } => {
                let turn_id = generate_ulid();
                self.ledger.append_fact(
                    Some(turn_id.clone()),
                    None,
                    FactPayload::InputAccepted(input.into_payload()),
                ).await.expect("Durable append must succeed");
                LoopState::BuildingContext { turn_id }
            }
            LoopState::BuildingContext { turn_id } => {
                let ctx = self.pipeline.assemble_context(&self.session_id, &turn_id).await.unwrap();
                LoopState::StreamingInference { turn_id, lap_index: 0, context: ctx }
            }
            LoopState::StreamingInference { turn_id, lap_index, context } => {
                let (lap_result, mut stream) = self.gate.stream_round(&context, &self.cancel_token).await.unwrap();
                // 实时处理流式 Chunk，落盘 AssistantBlockSealed
                self.handle_stream_and_seal(&turn_id, lap_index, lap_result).await
            }
            LoopState::ExecutingTools { turn_id, lap_index, intents } => {
                // 派发给外部 ToolRunner
                let outcomes = self.tools.execute_batch(&self.session_id, &turn_id, intents, &self.cancel_token).await;
                // 记录 ToolFinished 事实
                self.record_outcomes(&turn_id, outcomes).await;
                // 工具执行完毕，进入下一推演 Lap
                LoopState::BuildingContext { turn_id }
            }
            LoopState::SealingTurn { turn_id } => {
                self.ledger.append_fact(Some(turn_id), None, FactPayload::TurnFinished(Default::default())).await.unwrap();
                LoopState::Idle
            }
            LoopState::Idle | LoopState::Terminating => current,
        }
    }
}
```

---

## 5. 并发模型重塑与内存清剿方案

### 5.1 通道瘦身：消灭 15.8 MB 固定税
- **全面替换**：废除 `std::sync::mpsc::sync_channel(4096)`，改用 `tokio::sync::mpsc::channel(32)`。
- **背压语义**：输入缓冲区上限设为 32。超出容量时，外部投递 API 显式返回 `429 Too Many Requests (Agent Mailbox Saturated)`，禁止以无底洞缓冲掩盖死锁。
- **内存测算**：每个通道固定开销从 **15.8MB 暴降至 < 20KB**，降幅超 **99.9%**。

### 5.2 消除线程税：纯 Tokio Task 调度
- **全面替换**：废除 `crates/qaqh-runtime/src/actor.rs` 中的 `std::thread::Builder::spawn`。
- **调度模型**：所有 Session Actor 与 Subagent Actor 均运行在守护进程共享的 Tokio 多线程运行时中。
- **内存测算**：单会话不再独占 2 个 8MB OS 栈空间，单个 Actor 运行时消耗降至协程级（约 2KB ~ 4KB）。

### 5.3 严格 LRU 自动驱逐（Eviction）
- 将 `session_idle_unload_secs` 强制基线定为 **300 秒（5 分钟）**。
- Daemon 调度器每 60 秒轮询，凡空闲超过 300 秒的 Actor 直接触发优雅关闭。
- 由于所有状态在 `events.jsonl` 中完备落盘，下次请求到来时通过 `CanonicalSessionStore` 执行秒级热唤醒。

---

## 6. 分阶段实施路线图（绞杀者模式）

重构施工严禁“停产大翻修”，采用**“隔离舱建新房 ➔ 挂载验证 ➔ 毫秒级拔线”**的渐进路线：

```
Phase 0: 隔离舱脚手架建设 (src/thin_agent/)
   │
Phase 1: 剥离沉淀资产 (提取 ContextPipeline 与 ToolRunner)
   │
Phase 2: 编写 ThinAgentLoop 状态机并完成孤岛测试
   │
Phase 3: actor.rs 切换接线 (单点替换 run_actor)
   │
Phase 4: 物理销毁老旧 agent/ 巨石代码与 Timeline 遗留
```

### Phase 0: 隔离舱脚手架
- 在 `crates/qaqh-runtime/src/` 下创建新目录 `thin_agent/`：
  - `thin_agent/mod.rs`
  - `thin_agent/contracts.rs`（Trait 声明）
  - `thin_agent/ledger.rs`（Canonical 事实账本包装）
  - `thin_agent/loop_core.rs`（极薄状态机）

### Phase 1: 资产平移与接口封装
1. **构建 `ToolRunner`**：
   - 提取 `agent/tool_runtime.rs` 与 `agent/engine_tool.rs` 中的执行沙箱核心；
   - 封装为实现 `ToolRunner` Trait 的 `DefaultToolRunner`，去除一切对 `AgentState` 的依赖。
2. **构建 `ContextPipeline`**：
   - 平移 `agent/prompt.rs`、`agent/context.rs` 中的提示词渲染算法；
   - 接口输入仅绑定 `SessionId` 与 Canonical 读取器。

### Phase 2: ThinAgentLoop 实现与全生命周期 Mock 验证
- 编写 `ThinAgentLoop` 的状态驱动逻辑；
- 复用 `crates/qaqh-session/tests/canonical_takeover_mock.rs` 测试集，挂载 `ThinAgentLoop`，验证其产生的 `events.jsonl` 与规范 100% 吻合。

### Phase 3: 接线点毫秒级切流
- 在 `crates/qaqh-runtime/src/actor.rs:145`：
  ```rust
  // 将原有的同步阻塞调用：
  // crate::agent::spawn_agent(...);
  
  // 切换为纯异步 Tokio 任务：
  tokio::spawn(async move {
      let thin_loop = ThinAgentLoop::new(...);
      thin_loop.run().await;
  });
  ```
- 此时整个会话生命周期已在底层无声无息地完成现代化换心。

### Phase 4: 物理火化历史垃圾（Purge Day）
- 执行删除指令，彻底移除历史毒瘤：
  ```bash
  git rm -r crates/qaqh-runtime/src/agent/
  git rm crates/qaqh-runtime/src/timeline.rs
  git rm crates/qaqh-runtime/src/timeline_store.rs
  ```
- 将 `thin_agent` 重命名或作为标准的 `agent` 模块。

---

## 7. 验收标准与硬性指标

重构验收不接受口头保证，必须以物理指标和回归测试为准绳：

1. **常驻内存基线**：在 Daemon 打开 10 个空闲 Session 的场景下，进程物理 RSS 内存必须**稳定在 35MB ~ 50MB 之间**，严禁突破 60MB。
2. **OS 线程数量**：在并发执行 5 个会话时，Daemon 进程的总 OS 线程数不得随会话数线性增长，线程数增量必须为 **0**。
3. **事实完整性**：所有会话执行必须产出合法的 `events.jsonl`，且能够被 `Ringing v2` 投影 100% 无损还原为 Transcript 视图。
4. **零锁等待**：全流程中彻底清除 `LegacyWriterFacade::lock()`，无全局互斥锁瓶颈。

---
**审批确认**: QAQ-Harness Architecture Council  
**生效日期**: 2026-10-29
