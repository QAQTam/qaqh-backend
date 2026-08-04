# Ringing V1 收敛计划（legacy 拆除 × Solid 2 细粒度对齐）

> 定位：在 **Ringing V1 协议框架内**（不引入 v2）完成两件事：
> 1. **legacy 轨道完全拆除**（Agent2Ui / WriterEvent::Legacy / 旧 daemon 兼容回退；TUI 重做另起炉灶）；
> 2. **数据格式对齐前端 SolidJS 2 细粒度响应**（完整值 / 稳定键 / 低频写入 / 键控快照）。
>
> 本文件是 `optimization-plan.md`（A/B/C 分档）的**执行版**：方案分档不变，
> 补充 legacy 拆除与前端重构两个工作流，并把每项改动的双端契约钉死。

## 0. 背景与原则

- Ringing V1（2026-07-31，commit `912810d`）刚从前身协议重构定型，**不引入 v2**；
  协议演进遵守 `protocol-anchor.md` 第 3 节：新增事件 + 旧保留 + 白名单；语义收紧允许，
  语义变更必须新增事件类型。
- 桌面端前后端**捆绑发布**：协议格式调整（如信封瘦身）可前后端同步一发，不受旧版本约束。
- 前端 SolidJS 2 与 Ringing **同步演进**：本次前端重构是"吃改动"，不是独立优化。
- **已拍板（2026-08-04）**：legacy 业务帧**完全拆除**；旧 daemon 不再支持；
  TUI/WinUI 后续重做时另起炉灶，不保留兼容边界。

## 1. 现状盘点（拆除与重构的依据）

### 1.1 文本热路径"三发"现状

| 轨道 | 事件 | 发射位置 | 消费者 | 处置 |
|---|---|---|---|---|
| Timeline | `TextDelta` intent（块级事实源） | `engine_turn.rs` 每 token | 前端 `timelineMonitor` | **保留**（唯一块级事实源） |
| Legacy | `Agent2Ui::RoundDelta` 等 | `engine_turn.rs`/`engine_tool.rs` 每 token/chunk | 无（前端已全切 Ringing） | **完全拆除**（见 §4） |
| Ringing | `ConversationEvent::RoundDelta` | 同上 | 前端 `ringingStores` | 保留，升级为 checkpoint 语义（§2.1） |

同一 token 三路发射的位置：`engine_turn.rs`（Answering/Thinking 两个分支，各三发）；
writer 线程三种帧并存：`loop_core.rs` `WriterEvent::Legacy/Ringing/Timeline` 各写一行 JSON-LP。

### 1.2 前端兜底/重复实现清单

| 项 | 位置 | 处置 |
|---|---|---|
| `conversationReducer`（不可变双实现 + 等价性测试） | `ringingStores.ts` | 删除（C1） |
| `pendingDeltas` 乱序缓冲 / resume 重放 | `ringingStores.ts` | 删除（C4，依赖 A1） |
| `progressTail` 128KB 拼接 | `toolReducer` | 替换语义（A2） |
| 投影缓存三件套（WeakMap/`indexListCache`） | `sessionPresentation.ts` | 删除（C4） |
| `projectedTurnCache` | `ChatView.tsx` | 删除（C4） |
| MarkdownBody stale-while-revalidate + 防过期 generation | `MarkdownBody.tsx` | 简化（C5，依赖 A1） |

## 2. 事件表契约（新增 / 收紧）

### 2.1 新事件 `block_checkpoint`（replaceable，治 D1）

载荷：

```json
{ "type": "block_checkpoint", "turn_id": "t7", "round_num": 1,
  "kind": "thinking" | "answering", "text": "<完整文本>", "char_count": 1234 }
```

- **发射时机**：每 64 token 或 ~2s（先到为准），由引擎累积文本（`content`/`reasoning`
  push 缓冲）构造；`round_delta` 保留，用于首字延迟与旧前端兼容；
- **前端处理**：reducer +1 case，覆盖式写入（`draft.turnsById[t].rounds[n].answer = text`）；
  `pendingDeltas`/乱序缓冲/replay 兜底退役——checkpoint 是完整值，乱序/丢 delta 由下一次
  checkpoint 自愈；
- **兼容**：新事件类型 + 白名单；旧前端忽略未知事件（protocol-anchor 演进规则）。

### 2.2 语义收紧 `tool_progress`（渲染尾部协议，治 D2）

字段不变，语义收紧：`chunk` = **渲染尾部**（≤4KB，自 `seq_start` 起的连续完整尾部），
`truncated: true`；`seq_start` 与前端上次 `progressSeqEnd` 对齐；**不连续 → 前端替换而非拼接**。
旧前端按拼接路径处理仍正确（尾部是完整前缀），属"语义收紧"而非"语义变更"。

### 2.3 发射时机契约 `usage_updated`（节流）

每 ~1s 或每 256 token 发射（replaceable 覆盖）；`round_completed`/`turn_completed`
之前必发终值。语义不变（单请求覆盖 + 会话累计追加）。

### 2.4 信封瘦身（语义收紧，前后端同步一发）

- batch 已携带 `schema/version/channel/seed/server_epoch` → envelope 删除同名重复字段；
- envelope 保留：`stream_seq`/`event_id`/`delivery`/`causation_id`/`correlation_id`/`state_revision`；
- `session_seq`/`channel_seq`：实施时确认消费方，无消费者则随批次删除；
- 风险：与旧 worker/daemon 混跑时字段缺失——捆绑发布不受影响。

## 3. 快照契约（恢复 O(N²) 收敛）

### 3.1 分页物化视图

- bootstrap 返回**最近 N=25 个 turn** 的键控物化视图（现有 neutral JSON 形状，
  见 `conversation_snapshot.rs`）+ `total_turns`/`has_more`；
- 旧 turn：`GET /ringing/v1/session/{seed}/turns?cursor=<turn_id>&limit=50` 分页拉取，
  前端虚拟列表按需加载；
- 流式增量不变（snapshot baseline 之后补 tail）。

### 3.2 前端 `reconcile` 键控消费

`applyConversationSnapshot` 从"整子树替换"改为 draft 内
`reconcile(turns, "turn_id")` + 嵌套 `reconcile(rounds, "round_num")`；
未变 turn 身份保持 → 恢复零全量重渲染。

**锚点修订登记**（实施时同步更新 `protocol-anchor.md` 第 4 节）：
组件唯一数据依赖从"每事件重建的 RawSessionState 对象"演进为
"RingingStores store 视图（TurnView/RoundView/CardView）"；
`RawSessionState` 保留为**恢复/序列化形状**，不再是运行时唯一真相源。

## 4. legacy 完全拆除清单（已拍板）

### 4.1 删除项

| 项 | 位置 | 说明 |
|---|---|---|
| 引擎业务帧发射（`emit`/`emit_delta` 的 Agent2Ui 业务事件） | `engine_turn.rs`/`engine_tool.rs`/`engine_compact.rs`/`engine_misc.rs`/`engine_goal.rs`/`engine_input.rs` | RoundDelta/ToolCallPreview/UsageUpdated/ExecProgress/CodeDelta/CompactDelta/AuditRecord/CacheDiagnostics/ToolResults/SkillsChanged/TurnEnd/MoreTurns 等 |
| `WriterEvent::Legacy` + `write_legacy_event_frame` | `types.rs`/`loop_core.rs`/`wire.rs` | writer 线程只写 Ringing/Timeline |
| `Emitter::emit`/`emit_delta`（Agent2Ui 签名） | `types.rs` | 保留 `set_seed`/`enter_causation`/`emit_domain`/`emit_timeline` |
| legacy wire reader（stdin `Ui2Agent` 解析）与 fallback | `loop_core.rs`/`wire.rs` | engine dispatch 输入收敛为 RingingCommand/DomainCommand |
| daemon legacy WS 数据端点（`/control/v1` 数据协议） | `server.rs`/`registry.rs`/`service.rs` | 旧 daemon 不再支持；`/control/v1/stop` 生命周期接口独立评估 |
| 前端 legacy 回退分支 | `controlClient.ts`/`backendClient.ts`/`browserBridge.ts` | transport 固定 ringing，删除回退协商 |
| `deepx-client` crate 与 legacy probe/fixtures | `crates/deepx-client`、各 crate tests | 与数据协议同批退役（标记后单批删除） |

### 4.2 逐事件替代确认（实施时登记）

每个被删 legacy 事件必须登记 Ringing 替代（复用 `legacy-protocol-retirement.md` §1.1 表格式）：
生产者 → 消费者 → 回归证据；无替代的事件按"该语义已由 X 覆盖"登记。

**删除前置条件**：三频道 SSE/bootstrap/cursor/lease/terminal receipt 回归 fixture 全绿；
Electron 主链测试稳定；`cargo check` + daemon 集成测试 + Electron test 通过。

## 5. 前端重构清单（同步吃改动）

| # | 改动 | 替换什么 | 依赖 |
|---|---|---|---|
| C1 | 单一事件应用器（事件→路径映射表） | 删 `conversationReducer` 双实现 + 等价性测试 | — |
| C2 | `turnsById` O(1) 寻址 | 删每事件 `findIndex` O(turns) 扫描 | — |
| C3 | 快照 `reconcile` 键控消费 | `applyConversationSnapshot` 整子树替换 | — |
| C4 | 删投影缓存三件套 + 拼接兜底 | `sessionPresentation` 缓存/ChatView 缓存/`pendingDeltas`/`progressTail` 拼接（净删 ≥1000 行） | A1/A2 |
| C5 | MarkdownBody 简化 | stale-while-revalidate/防过期 generation | A1 |

锚点不变式精神保留：组件层不出现协议字段（实现从"纯函数 + 类型隔离"改为 store 视图封装）。

## 6. 执行顺序与里程碑

| 里程碑 | 内容 | 产出/验证 |
|---|---|---|
| M1 | A1 + A2 + A3（后端新事件/收紧，零破坏） | `block_checkpoint`（64 token/2s 完整值）、`tool_progress` 尾部（≤4KB 替换）、`usage_updated` 节流（~1s + 终值） | ✅ 完成（2026-08-04） |
| M2 | C1 + C2 + C3（前端消费层） | 单一事件应用器（-300 行双实现）、`turnsById` O(1) 寻址、快照 `reconcile` 键控消费 | ✅ 完成（C1-C3，2026-08-04） |
| M3 | B（legacy 完全拆除） | 引擎三发→双发；`WriterEvent::Legacy`/`Emitter::emit/emit_delta` 删除；daemon legacy WS + 前端回退分支；集成测试改 Ringing 收集 | ⬜ 待实施（B1-B4 分阶段，见 §10.1） |
| M4 | A4（信封瘦身，前后端同步一发） | envelope 删 4 冗余字段；SSE 每事件 JSON -40% | ✅ 完成（2026-08-04） |
| M5 | C4 + C5（删兜底） | **已修订**：投影缓存/pendingDeltas 是引用稳定机制本体，不可删；真正实施 = 组件改读 store 视图（锚点演进，bigbang 蓝图） | ⬜ 待排期（锚点演进） |
| M6 | A5（快照分页 + reconcile） | bootstrap 最近 N turn + 分页拉取 + 虚拟列表；恢复 O(N²)→O(可见窗口) | ⬜ 待实施（见 §10.3） |

## 7. 验证标准（量化）

- 120 token/s 时投影频率 ≤60/s（事件批次率 ≤ 渲染率）；
- 500-turn 会话恢复 <300ms 且首帧即最近内容；
- 前端净删兜底代码 ≥1000 行；
- 信封瘦身后 SSE 传输体积下降 ≥30%；
- 契约测试（`sessionPresentation`/`ringingStores`/`turnProjection`/`resumeStreaming`）全绿。

## 8. 不做清单

- ❌ 不引入 Ringing v2（协议结构/三频道/envelope 骨架不动）；
- ❌ 不做每 token 全量 replaceable（传输 O(n²)）——checkpoint 是正确折中；
- ❌ 不做 ACK 携带业务负载；
- ❌ 不恢复 `Agent2Ui → Ringing` 兼容投影（`legacy-protocol-retirement.md` §4）；
- ❌ 不动 `/control/v1/stop`（独立生命周期接口）。

## 9. 变更登记

| 日期 | 变更 | 前端影响面 | 状态 |
|---|---|---|---|
| 2026-08-04 | 收敛计划定稿；legacy 完全拆除拍板；A1/A2/A3/A4/A5 + C1-C5 契约固化 | C1-C5 全部 | 待实施 |
| 2026-08-04 | **A1 `block_checkpoint` 完成**：domain variant（Replaceable）+ router key（覆盖 + RoundCompleted invalidate）+ engine 发射（64 token / 2s）+ bindings 再生成 + 前端双 reducer case | reducer +2 case；`pendingDeltas` 退役待 C4 | ✅ 完成（Rust router 8 测试 + 前端 93 测试全绿） |
| 2026-08-04 | **A2 `tool_progress` 渲染尾部完成**：engine_tool 抽 `emit_progress_tail` helper（每 (tool_call_id, stream) 保留 ≤4KB 尾部，seq_start = 尾部起始位，truncated 按累计判定），顺带消除两个 drain 函数的重复发射体；前端 toolReducer 拼接→**总是替换**（128KB 截断/丢字逻辑删除） | toolReducer -15 行 | ✅ 完成（前端 272 测试全绿） |
| 2026-08-04 | **A3 `usage_updated` 节流完成**：流式 ~1s 窗口发射（replaceable 覆盖显示）+ Done 分支终值必发（与 record_usage 同值）；双发对称保留 | 无（显示语义不变） | ✅ 完成 |
| 2026-08-04 | **C2 `turnsById` O(1) 寻址完成**：`ConversationState.turnsById` 索引（快照/turn_started 同步维护）+ 6 处 findIndex→`turnIndex`（防御性回退） | 纯增量，零行为变化 | ✅ 完成（272 测试全绿） |
| 2026-08-04 | **C1 单一事件应用器完成**：删 `conversationReducer`/`upsertTurn`/`clearPending`/`applyRoundDelta`/`applyEnvelope`/`applyEnvelopeUnchecked`（≈300 行双实现）；抽 `applyConversationEventToDraft` 唯一实现；测试经 `makeReducer`/`applyEnvelopeWith`/`applyTool`/`applyControl` helper 走生产路径（含 Solid 2 微任务批 flush 语义） | 生产路径零行为变化；测试改用同一实现 | ✅ 完成（272 测试全绿） |
| 2026-08-04 | **C3 快照 `reconcile` 键控消费完成**：`ringingMonitor` snapshot 应用从"整子树替换"改为 `reconcile(turns, "turn_id")` 键控合并（内容未变 turn 身份保持 → 恢复零全量重渲染；本地独有 turn 保留）+ 元数据字段 draft 覆盖 | 恢复路径行为增强，零回退 | ✅ 完成（273 测试全绿，含身份保持断言） |
| 2026-08-04 | **C4 修订**：投影缓存三件套（`turnProjectionCache`/`roundProjectionCache`/`indexListCache`）与 `ChatView.projectedTurnCache` **不可删除**——它们正是引用稳定机制（Solid 跳过未变化子树）；`pendingDeltas` 仍承担 round_delta 乱序首字缓冲。C4 真正实施 = 组件改读 store 视图（锚点演进），移入"bigbang 蓝图"待独立排期 | — | 已修订 |
| 2026-08-04 | 预存失败登记（**与本次改动无关，stash 基线验证**）：`state::agent` catalog ×2 + `ask_user_lifecycle` ×11 超时——属进行中的 skill-context 工作区改动，不阻塞本计划 | — | 记录在案 |
| 2026-08-04 | 预存 typecheck 错误登记（非本次引入）：bindings 的 `bigint`/`JsonValue`/`SkillRuntimeInfo` 导出问题（ts-rs 生成与 `ringing-bindings.sh` 固定 index 列表不匹配） | — | 待独立清理 |
| 2026-08-04 | **M4 信封瘦身完成**：`RingingEventEnvelope` 删 `schema`/`version`/`channel`/`server_epoch`（`seed` 保留——多 seed 共享连接逐事件路由）；`new()` 7→6 参、`validate()`/`RingingEventBatch::validate()` 精简；`sse_frame` 参数化（epoch/channel 取连接上下文，帧 id 承载）；journal 旧数据兼容（serde 忽略未知字段）；bindings 再生成 + `envelopeToBatch(channel, envelope, serverEpoch)`（epoch 取 `getServerEpoch()`/`sseServerEpoch`）；5 个测试文件 envelope 构造删字段（batch/bootstrap 结构保留） | SSE 每事件 JSON **-40%** | ✅ 完成（Rust 28+99+19 / 前端 273 全绿） |

## 10. 后续改进方向（2026-08-04 登记）

### 10.1 M3 legacy 完全拆除（已拍板，分阶段）

| 阶段 | 内容 | 前置/风险 |
|---|---|---|
| B1 | 引擎三发→双发：删 `engine_*.rs` 的 `emit_delta(Agent2Ui::…)` 业务发射（RoundDelta/ToolCallPreview/UsageUpdated/ExecProgress/CodeDelta/CompactDelta/AuditRecord/CacheDiagnostics/ToolResults 等） | 与工作区未提交 skill-context 改动相邻，实施前先协调 |
| B2 | writer 收窄：`WriterEvent::Legacy` + `write_legacy_event_frame` + `Emitter::emit/emit_delta` 删除；wire legacy reader 同步清理 | 保留 `set_seed`/`enter_causation`/`emit_domain`/`emit_timeline` |
| B3 | daemon legacy WS 端点（`/control/v1` 数据协议）+ 前端 legacy 回退分支（`controlClient.ts`/`backendClient.ts`）删除；`/control/v1/stop` 独立评估 | `deepx-client` crate 与数据协议同批退役 |
| B4 | `ask_user_lifecycle` 等集成测试改 Ringing 事件收集 | **独立子任务**：该文件当前为预存失败态（skill 工作区），重写无基线可对照，最后处理 |

### 10.2 timeline 块级 checkpoint（新方向）

timeline 频道 `text_delta` 仍是**每 token 增量**（`engine_turn.rs` 每 token `emit_timeline(TextDelta)`，
前端 `timelineMonitor` 拼接块文本）——这是继 conversation `round_delta`（已由 A1 收敛）之后
第三个"每 token"发射源。扩展：**块级完整值 checkpoint**（每 ~2s 发整块 text 覆盖，或
`block_sealed` 前发完整值），与 A1 语义对齐 → 块身份稳定、Markdown 按 sealed 渲染更干净、
`timelineMonitor` 拼接逻辑可退役。事件走 `TimelineIntent` 扩展（新 intent 类型 + 旧保留）。

### 10.3 A5 快照分页 + 物化视图（M6）

bootstrap 返回最近 N=25 turn 的键控物化视图 + 分页拉取旧 turn（`cursor=<turn_id>&limit=50`）
+ 前端虚拟列表；恢复从 O(N²) 收敛为 O(可见窗口)，首帧即最近内容。

### 10.4 usageTotals 权威化（A3 配套发现）

现状：前端 `usageTotals` 对每次 `usage_updated` 累加——同一请求多次发射（节流后仍多条）
累加 ≠ 请求终值（虚高）。修正：流式 `usage_updated` 仅覆盖 `lastUsage`（显示）；
`usageTotals`/`usageRequestCount` 改为 `turn_completed.usage`（权威终值）累加 +
snapshot `usage_totals` 恢复。涉及 `ringingStores.ts` 两个 case + 相关测试。

### 10.5 C4/C5 锚点演进（bigbang 蓝图）

组件改读 store 视图（TurnView/RoundView/CardView）后：投影缓存三件套、
`ChatView.projectedTurnCache`、`pendingDeltas`、MarkdownBody stale-while-revalidate
可整体退役（净删 ≥1000 行）。需同步修订 `protocol-anchor.md` 第 4 节锚点不变式。

### 10.6 bindings 预存错误清理

`ringing-bindings.sh` 固定 index 列表遗漏 `SkillRuntimeInfo` 导出；ts-rs 生成的
`bigint`/`JsonValue` 类型与 TS target 不匹配。修正脚本 + 类型映射，清空 typecheck 基线。

### 10.7 真实流式验证

daemon + 前端跑一次实际 token 流，量化：120 token/s 时投影频率 ≤60/s、
`projectBlocks` 每 checkpoint 一次（2s 粒度）、SSE 体积下降实测——锁定 §7 验证标准。
