# 事件协议语义契约（Ringing v1）

> 本文件是后端发射端与前端 reducer 的共同参照。**语义以本文件为准**，实现偏离即为 bug。

## 1. 传输与投递语义

| 维度 | 契约 | 实现位置 |
|---|---|---|
| 传输 | SSE（daemon → main 进程 → renderer IPC batch） | `electron/ringingClient.ts` |
| 频道 | control / conversation / tool 三频道独立连接与 cursor | `ringingStores.ts` `ChannelConnectionState` |
| 投递 | reliable；`event_id` 幂等（前端 `AppliedEventRegistry` 去重） | `ringingStores.ts` |
| 顺序 | `stream_seq` 每频道单调；断线经 Last-Event-ID 续传 | `electron/ringingClient.ts` |
| 恢复 | bootstrap 全量 snapshot + snapshot 后事件补 tail；快照是权威恢复点 | `ringingMonitor.activate` |
| 传输时序 | **即时传输，禁止服务端微批**——渲染器负责帧级合并（`paced_emitter.rs` 决策，前端尚未执行，见 optimization-plan.md C2） | `paced_emitter.rs` |

## 2. 事件分类（语义决定前端处理方式）

### 2.1 replaceable（状态式：覆盖，天然幂等）

| 事件 | 语义 | 前端处理 |
|---|---|---|
| `agent_lifecycle_changed` | 会话生命周期状态覆盖 | 覆盖赋值 |
| `dashboard_snapshot` / `dashboard_updated` | 仪表盘状态覆盖 | 覆盖赋值 |
| `skills_updated` | 技能目录/运行态全量覆盖 | 覆盖赋值（`selectSkillsPresentation`） |
| `usage_updated` | 单请求用量覆盖 + 会话累计追加 | `lastUsage` 覆盖；`usageTotals` 累加 |
| `provider_tool_status` | 提供方内建工具状态（replaceable） | 覆盖赋值 |
| `round_completed` | 回合终态（thinking/answer 全量覆盖 + is_final） | 覆盖该 round |
| `turn_completed` / `turn_failed` / `conversation_cancelled` | turn 终态 | 覆盖状态 |
| `compact_finished` / `operation_failed` / `system_notice` | 控制终态 | 覆盖/记录 |

### 2.2 增量式（前端负责拼接——**遗留设计，优化见 A1/A2**）

| 事件 | 语义 | 前端处理（现状） |
|---|---|---|
| `round_delta` / `text_delta` | 文本增量追加（thinking/answering 按 kind 分槽） | 前端拼接；乱序缓冲 `pendingDeltas`；snapshot 修复 |
| `tool_progress` | 工具输出增量（带 `seq_start/seq_end`） | 前端拼接至 128KB 尾（`progressTail`）；seq 不连续时丢弃拼接 |
| `block_opened` / `block_sealed` | 块生命周期（timeline 频道） | 追加/封口 |
| `tool_call_prepared` / `tool_started` / `tool_finished` | 工具卡生命周期 | 卡列表追加/状态覆盖 |

### 2.3 时序与因果

- 命令 ACK 无业务负载；创建类命令的因果事件可能先于/后于 ACK 到达——前端以
  `causation_id` 关联（`createdSeedsByCommand`），**不允许后端假定前端只依赖 ACK**；
- `turn_started` 前可能到达 `round_delta`（乱序）——前端缓冲不丢弃，**后端不应改变
  此行为假设**（若实现 A2 checkpoint，则此缓冲可逐步退役）。

## 3. 演进规则（锚点钉子的第一颗）

1. **新增事件**：新增 `type` + 旧事件保留；前端白名单加一项 + reducer 加一个 case；
2. **语义收紧**：允许（如 A1 尾部协议），但必须保持旧前端兼容（旧前端按旧语义处理新负载仍正确）；
3. **语义变更**（如增量→replaceable）：禁止直接改旧事件，必须新增事件类型（如 `block_checkpoint`）；
4. **协议替换**：仅当演进规则无法满足时才允许（参考 `docs/legacy-protocol-retirement.md`），
   且必须保留 UI 锚点（RawSessionState）不变——重写仅限翻译层；
5. 所有变更必须在 `optimization-plan.md` 登记并注明前端影响面。

## 4. 前端锚点不变式（锚点钉子的第二颗）

UI 锚点：`RawSessionState`（`src/store/rawSession.ts`）+ `TurnViewModel`（`src/presentation/turnProjection.ts`）。
**以下不变式由契约测试锁定，违反即为回归**：

| 不变式 | 锁定测试 |
|---|---|
| 未变化的 turn/round 投影引用稳定（`p1.turns[i] === p2.turns[i]`） | `sessionPresentation.test.ts` "reuses stable projections" / "keeps the round stable" |
| `toolResults` 只包含 finished 且有 result 的卡 | `sessionPresentation.test.ts` "keeps every tool card" |
| 同一 round 多工具全部保留（缓存不吞卡） | `sessionPresentation.test.ts` "keeps every tool card when a round has multiple tool calls" |
| reducer ↔ path 应用器行为等价 | `ringingStores.test.ts` "converges with conversationReducer" |
| 组件层不出现协议字段（类型级） | `rawSession.ts` 类型 + review 规则 |
| 未知事件被白名单拒绝（协议演进安全） | `agentEventBoundary` 模式（已退役）→ 新实现由 reducer default 分支保证 |

## 5. 已知脏点（供优化计划引用）

- **D1**：`text_delta` 每 token 增量 → 前端拼接状态 + 乱序缓冲 + 重放兜底（>300 行兜底代码）；
- **D2**：`tool_progress` 每 chunk 增量 → 前端 128KB 尾拼接；seq 不连续时丢字风险；
- **D3**：全量快照恢复 O(N²)（首日遗留，`server.rs` 注释自认）；
- **D4**：帧级合并责任悬空（paced_emitter 决策 → 前端未实现）。

→ 各脏点的优化方案见 [optimization-plan.md](./optimization-plan.md)。
