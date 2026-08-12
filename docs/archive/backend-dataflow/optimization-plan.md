# 后端数据流优化 PLAN

> 目标：在不反转"即时传输"架构决策的前提下，根治协议级脏点（D1-D4），
> 让前端删掉拼接/缓冲兜底，让长会话恢复从 O(N²) 收敛。

## 0. 考古结论（为什么需要这份 PLAN）

git 考古（commit `135a558` / `912810d` / `aeb752a`）：

- **2026-07-26 首个 commit（legacy 协议）**：`round_delta`（每 token 增量）、
  `tool_exec_delta`/`exec_progress`（每 chunk 增量）已存在；前端配套
  `sessionReplayBuffer`/`controlEventBatcher`；daemon 注释已自认 O(N²) 快照问题。
  → **数据流的"脏"（增量粒度 + 前端拼接 + 高频事件）自创立第一天就有**；
- **2026-07-31 Ringing（`912810d`）**：块级事件/三频道/幂等/快照恢复——协议**结构化**
  但**粒度未变**（`text_delta` 仍每 token、`tool_progress` 仍每 chunk）；
- **2026-08-02 `paced_emitter.rs`**：显式决策"后端即时传输、渲染器负责帧级合并"——
  前端从未实现帧级合并（责任悬空，见 C2）。

**结论**：后端协同优化不是给 Ringing 打补丁，而是修正创立时的协议设计遗留。

## 1. 目标与原则

```
原则 1：保留 paced_emitter 决策（后端即时传输）——C1 需实测数据背书才允许反转
原则 2：协议演进遵循 protocol-anchor.md 第 3 节（新增 + 兼容 + 翻译层隔离）
原则 3：每一项必须登记前端影响面（锚点 RawSessionState 不变）
原则 4：先量化后优化（事件率/恢复耗时/长任务基线先行）
```

## 2. 方案分档

### A 档：协议级优化（不改传输时序 · 高价值低风险 · 优先）

#### A1. `tool_progress` 渲染尾部协议（治 D2）

| 维度 | 内容 |
|---|---|
| 现状 | 后端每 chunk 发完整增量；前端拼至 128KB 尾（`progressTail`）；seq 不连续丢弃拼接（丢字风险） |
| 方案 | 后端只发**渲染尾部**（≤4KB）：`{ seq_start, seq_end, chunk: 尾部, truncated: true }`；`seq_start` 与前端上次 `progressSeqEnd` 对齐，不连续时前端**替换而非拼接** |
| 前端影响 | `toolReducer` 一处：拼接→替换（约 -30 行） |
| 兼容 | 事件结构不变、字段语义收紧；旧前端按拼接路径处理仍正确（尾部是完整前缀） |
| 验证 | msglp 集成测试：10MB 输出流式，前端 tail 恒 ≤4KB，seq 连续无丢字 |

#### A2. 文本块周期 checkpoint（治 D1）

| 维度 | 内容 |
|---|---|
| 现状 | `text_delta` 纯增量；前端维护拼接状态（`pendingDeltas` 乱序缓冲、snapshot 修复、resume 重放——全为拼接兜底） |
| 方案 | 每 N 个 delta（或 ~2s）发射一次该块**完整文本**（新事件 `block_checkpoint`，replaceable）：`{ block_id, text: 完整 }` |
| 前端影响 | reducer +1 case（覆盖式更新）；`pendingDeltas` 缓冲/重放逻辑可删（约 -100 行） |
| 兼容 | 新事件类型 + 白名单；旧前端忽略未知事件 |
| 风险 | 传输量 O(n²/64)（每 2s 全量 ≈ 1KB 级，可忽略） |
| 验证 | 模拟丢 delta（乱序/断线）→ checkpoint 后文本收敛正确 |

**A1+A2 合计前端收益**：删 300+ 行拼接/缓冲兜底，锚点零改动。

### B 档：快照级优化（治 D3 · 中价值中风险）

#### B1. 增量 checkpoint 快照

| 维度 | 内容 |
|---|---|
| 现状 | 恢复 = 全量 snapshot + 事件补 tail；长会话 O(N²)（序列化/传输/克隆） |
| 方案 | 后端维护滚动 checkpoint（每 K 个 turn 或 ~1MB 存完整快照）；恢复从最近 checkpoint 起步 + 重放 checkpoint 后事件 |
| 前端影响 | **零改动**（快照协议不变，只是更小更近） |
| 风险 | 后端存储与一致性边界（checkpoint ↔ 事件窗口） |
| 验证 | 500-turn 会话恢复耗时对比 |

### C 档：传输级优化（低优先 · 需论证）

#### C1. 后端 16ms 微批（潜在反转即时决策）

- 现有决策反对：`paced_emitter` 明确"immediate prevents hidden server-side latency"；
- 支持理由：token 流常态 >60/s 时事件洪峰存在；
- **决定**：先实测事件率分布（dev 遥测），>120/s 占比高再议；否则维持即时。

#### C2. 前端渲染背压（补齐"renderer owns frame-level coalescing"）

- 现状：责任已分配（08-02 决策）但前端未执行——每事件一投影一渲染；
- 方案：投影 memo rAF 门控（一帧最多一次投影）+ 脏集增量（只投影变化 turn）；
- 性质：纯前端（`sessionPresentation` + App 层 memo），与后端 A/B 互不阻塞。

## 3. 里程碑

| 里程碑 | 内容 | 产出 |
|---|---|---|
| M1（本周） | A1 + A2（后端发射端 + 前端翻译层） | tool_progress 免拼接；文本拼接容错；前端删 300+ 行兜底 |
| M2（下周） | B1 快照 checkpoint | 长会话恢复 O(N²) → O(最近窗口) |
| M3（并行） | C2 前端渲染背压 | 事件率不再等于渲染率 |
| M4（实测后定） | C1 后端微批（仅数据支持时） | 事件率钉在 ~60/s |

## 4. 不做清单（明确拒绝）

- ❌ ACK 携带业务负载（如创建命令直接返回 seed）——协议破坏性变更，前端因果等待已兜底；
- ❌ 会话列表事件推送——`turn_completed` 后刷新已够用，收益低；
- ❌ 增量事件改 replaceable 全量（每 token 全量 = 传输 O(n²)）——方向错误，A2 checkpoint 是正确折中；
- ❌ 前端组件层协议化——违反锚点不变式（protocol-anchor.md 第 4 节）。

## 5. 变更登记

| 日期 | 变更 | 前端影响面 | 状态 |
|---|---|---|---|
| — | A1 tool_progress 尾部 | toolReducer 拼接→替换（-30 行） | 待实施 |
| — | A2 block_checkpoint | reducer +1 case；pendingDeltas 可删（-100 行） | 待实施 |
| — | B1 快照 checkpoint | 零改动 | 待实施 |
| — | C2 渲染背压 | sessionPresentation + App memo（纯前端） | 待实施 |

> **2026-08-04**：本 PLAN 已由 [convergence-plan.md](./convergence-plan.md) 承接执行——
> A1/A2/B1 保留分档与验证口径，新增 legacy 完全拆除（已拍板）与前端 C1-C5 重构工作流；
> C1（后端 16ms 微批）维持"实测后定"，C2（前端渲染背压）由 A4 信封瘦身 + 批次率收敛替代。
