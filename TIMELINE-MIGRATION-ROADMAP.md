# Timeline 迁移路线图（rc3 → rc5）

> 状态：进行中（Phase 1 已完成，2026-08-12）
> 关联文档：`docs/timeline-protocol-design.md`（协议决策）、`docs/backend-dataflow/convergence-plan.md`（收敛计划）、`docs/rc4-native-client-contract.md`（客户端契约）

## 1. 背景与动机

DeepX 的 transcript（会话展示）存在**两条事件通道**：

| 通道 | 事件 | 排序能力 | 前端用途 |
|---|---|---|---|
| **Timeline**（权威） | `TimelineIntent`/`TimelineEvent`：BlockOpened/TextDelta/BlockCheckpoint/BlockSealed/RoundSealed/TurnSealed | ✅ 全局单调 `timeline_seq` + 不可变 `block_order` + `fragment_seq` 连续校验 | 快照恢复、分页 |
| **DomainEvent**（legacy 投影） | `ConversationEvent`（RoundDelta/RoundCompleted/…）+ `ToolEvent`（ToolCallPrepared/ToolFinished/…） | ❌ 无顺序字段；三频道 SSE 独立连接、独立 cursor，跨频道顺序未定义 | **live 渲染（当前）** |

设计文档（`timeline-protocol-design.md`）明确：*"The renderer consumes only timeline entries/snapshots for transcript layout. Control and telemetry can remain separate streams because neither is allowed to determine transcript order."*

### 双通道带来的已知缺陷

1. **工具排序错乱**：live 渲染走无顺序的 DomainEvent，工具卡按到达顺序插入；"思考-工具-回复-工具"交错场景下工具卡被压平到 `[思考摘要][工具卡][正文]` 固定布局（`chat_view.rs` 三段式），无法反映真实块序。
2. **done 判定失效**：前端 markdown 渲染依赖 `RoundCompleted`/`TurnCompleted` 到达；后端 `TimelineAppender` 拒绝 intent 时静默丢弃（`registry.rs` 仅 log::error），封口链（BlockSealed→RoundSealed→TurnSealed）断裂后 turn 永远 Running——"实际已 done 但前端认为没 done"。
3. **三频道跨连接乱序**：`deepx-client` 每频道独立 TCP 连接，毫秒级到达乱序被 8ms 渲染帧放大。

## 2. 设计目标与约束

- **目标**：transcript 渲染（snapshot + live + prepend）统一由 timeline 驱动；DomainEvent 退守控制/遥测平面（usage、dashboard、交互、活动状态、审计）。
- **约束**：
  - `rc4-native-client-contract.md` §6：**禁止**新增平行事件模型、用 fallback flag 维持双数据源；结构变化必须在 RC4 前完成或推迟到下一发布周期。
  - `timeline-protocol-design.md` 能力审计：**当前实现不是能力对等的**——安全切换范围仅限"渲染的 transcript"，且需先补齐 Partial 行。
  - **DeepSeek 硬约束**（2026-08-12 官方文档核实）：携带 `tools` 的请求后续必须完整回传 `reasoning_content`，否则 HTTP 400（→ `echo_reasoning_content` 不能关）；思考模式下 `temperature`/`top_p`/`presence_penalty`/`frequency_penalty` **全部无效**（静默忽略）；`frequency_penalty`/`presence_penalty` 全局 deprecated。

## 3. 现状盘点（2026-08-12）

### ✅ 已完成

| 项 | 位置 |
|---|---|
| Timeline 协议类型（Entry/Snapshot/Intent/Event） | `deepx-domain/src/timeline.rs` |
| `TimelineAppender` 单写者（唯一分配 `timeline_seq`） | `deepx-runtime/src/timeline.rs` |
| 持久化 journal + terminal 同步落盘 + 快照/watermark/replay | `deepx-runtime/src/ringing/hub.rs` |
| daemon 端点：`/timeline`（快照+分页）、`/timeline/events`（live SSE） | `deepx-daemon/src/ringing_http.rs:680-694` |
| client：`activate_timeline` + cursor/gap recovery/re-baseline | `deepx-client/src/timeline.rs` |
| 前端恢复路径（`restored_turns`/`chat_prepend` 分页） | `apps/winui/src/chat_adapter.rs`、`bridge.rs` |
| 交错 block ID（`ensure_stream_block` 按 kind 连续开块） | `deepx-msglp/src/ringing_v1/engine_turn.rs` |
| 工具记录 native 完整（args/progress/output/failure/permission） | 同上 |
| **`BlockCheckpoint` 协议 + appender 应用 + producer 双发**（convergence §10.2，本次落地） | `deepx-domain`/`deepx-runtime`/`deepx-msglp` |
| 工具终态双发修复（ToolFinished 恰好一次，round 裁决后统一发射） | `deepx-msglp/src/ringing_v1/engine_turn.rs` |

### ❌ 未完成（迁移主体）

| 项 | 说明 |
|---|---|
| 前端 `Transcript` 重写为 block 级 gap-aware reducer | live 渲染仍消费 DomainEvent（`chat_adapter::render_event`） |
| `chat_view.rs` 布局改按 `block_order` 渲染 | 当前三段式 `[thinking][tool][answer]` |
| `chat_adapter` 收窄为 control/telemetry 映射 | 删除 conversation/tool 的 transcript 分支 |
| conversation/tool 通道退役 transcript 用途 | producer 停止 transcript 类 `emit_domain` |
| `ProviderToolStatus`（web_search 卡）迁移决策 | 映射为 tool block 或保留遥测消费 |

## 4. 迁移路线图

### Phase 1 — 能力补齐（✅ 已完成，rc3）

- `BlockCheckpoint` 协议 + writer 应用 + producer 双发（本次落地）。
- 纯后端增量、零 UI 行为变化；timeline 侧获得丢 delta 自愈能力，为前端切换铺路。

### Phase 2 — 前端 live 切换（rc3.1 / rc5 主力，按 `rc4-contract.md` §4 顺序执行）

| 步 | 内容 | 工作量 | 风险 |
|---|---|---|---|
| 2.1 | domain 收尾决策：usage 留遥测平面；`ProviderToolStatus` 映射决策 | 小 | 低 |
| 2.2 | client 侧确认 `TimelineEntry` 覆盖新 variant（checkpoint 已兼容） | 小 | 低 |
| 2.3 | `markdown-winui` `Transcript` 重写：round 三通道模型 → **block 级有序列表**（`BlockOpened` 流式渲染、`BlockCheckpoint` 覆盖自愈、`BlockSealed` markdown final、`TurnSealed` 完成判定） | **大** | 高 |
| 2.4 | `chat_view.rs` 按 `block_order` 渲染，删除三段式布局 | 中 | 中 |
| 2.5 | `chat_adapter` 收窄；header undo/compact 的 turn 边界改由 `TurnOpened/TurnSealed` 驱动（或保留 conversation turn 事件给 header——header 不参与 transcript 排序，允许） | 中 | 中 |

**切换策略**：2.3 期间双通道并存——timeline 新渲染路径与 conversation 旧路径对照验证，对照通过后再删旧路径，避免大爆炸切换。

### Phase 3 — 通道退役（迁移后清理）

- producer：停止 transcript 类 `emit_domain`（RoundDelta/RoundCompleted/ToolCallPrepared/Started/Finished 的渲染用途），保留 usage/audit/dashboard/control。
- 前端：删除 `is_renderable_event` 的 conversation/tool 分支与 chat 队列的 transcript 消费。
- 按 `convergence-plan.md` §10.2 预估：**净删 ≥1000 行**（"三发→双发"收官）。

## 5. RC4 前堆积的其它待改动（非 timeline）

| 来源 | 项 | 状态 | 依赖 |
|---|---|---|---|
| `convergence-plan.md` M3 B1-B4 | legacy 完全拆除：引擎三发→双发、`WriterEvent::Legacy`/`Emitter::emit/emit_delta` 删除、daemon legacy WS 端点、前端回退分支 | ⏳ 待实施 | 独立 |
| `convergence-plan.md` M6 A5 | 快照分页 + reconcile、虚拟列表（恢复 O(N²)→O(可见窗口)） | ⏳ 待实施 | 部分已完成（前端分页已落地） |
| `rc4-contract.md` §7 | **per-seed `TimelineRepository`：snapshot/live/prepend 走同一 reducer** | ⏳ 待实施 | **= Phase 2 主体** |
| `rc4-contract.md` §7 | push subscription + `DispatcherQueue` 替代 16/250/500ms revision polling | ⏳ 待实施 | 性能项，可后置 |
| `rc4-contract.md` §7 | 辅助 query/action 响应补齐 typed DTO，收缩 `shell_store` JSON 边界 | ⏳ 待实施 | 独立 |
| `rc4-contract.md` §7 | typed `Route`、observable/incremental collection、activation/accessibility | ⏳ 待实施 | 独立 |
| 本次会话 | 死循环/过度思考防护（n-gram 重复检测 + 打断 + 截断 thinking + effort 降级重发） | ⏳ backlog | 用户暂缓；依赖 Phase 2 稳定 |
| 本次会话 | `permission_lifecycle`/`ask_user_lifecycle` 回归纳入 CI 门禁 | ✅ 已修复，待纳入 | 独立 |

## 6. 外部事实记录（2026-08-12 核实自 DeepSeek 官方文档）

1. **`frequency_penalty` / `presence_penalty`：全局 deprecated**——"传入该参数将不会产生任何效果"（静默忽略，不报错）。→ 防死循环方案**放弃 penalty 透传**（对 DeepSeek）；其他 provider 可留作可选。
2. **思考模式下 `temperature`/`top_p`/penalty 全部无效**——DeepSeek 思考模式使用固定采样策略。→ "打断重发同样位置死循环"由此可解释（采样行为完全一致）。
3. **思考模式工具调用必须回传 `reasoning_content`**，否则 400；未进行工具调用的轮次可不回传（传入被忽略）。→ `echo_reasoning_content=true` 为硬需求；"删除思考链路重发"仅对**无工具轮次**合法。
4. `reasoning_effort`：v4-flash 支持 low/high/max 三档；v4-pro 暂仅 high/max（low 映射 high），2026-08 初放开。→ DeepX `effort_max` 钳制逻辑需随映射表更新。
5. 模型名：`deepseek-chat`/`deepseek-reasoner` 已于 2026-07-24 退役，指向 v4-flash 的思考/非思考模式。

## 7. 版本规划建议

| 版本 | 内容 |
|---|---|
| **rc3** | Phase 1（✅ 已完成：BlockCheckpoint + 双发修复） |
| **rc3.1**（或 rc4） | Phase 2 步骤 2.1–2.4（前端 timeline live 渲染，双通道对照并存）；M3 B1-B2 可并行 |
| **rc5** | Phase 2 步骤 2.5 + Phase 3（收窄与退役）；§5 其余待办 |

**门禁**（每阶段合入前）：
```powershell
cargo test -p deepx-domain -p deepx-runtime -p deepx-ringing
cargo test -p deepx-msglp            # 含 permission/ask_user 生命周期（63 用例）
cargo test -p deepx-client --all-targets
cargo test -p deepx-winui
cargo check --workspace && git diff --check
```

## 8. 风险清单

1. **Phase 2.3 是最大风险块**：Transcript 重写 + 渲染语义变化，必须双通道对照验证，禁止一次切换。
2. **`BlockSealed` 前渲染**：open 态允许流式/纯文本渲染，sealed 才 final markdown（设计 Invariant 4），防止流式体验退化。
3. **usage/header 联动**：undo/compact disabled 的 turn 边界信号不能断（header 不参与排序，允许保留 conversation turn 事件）。
4. **DeepSeek 回归**：工具调用 + reasoning_content 回传链路（400 风险）纳入集成测试。
5. **rc4 契约 §6 红线**：迁移过程中不得引入新的平行事件模型或 fallback flag。

## 9. 变更记录

| 日期 | 内容 |
|---|---|
| 2026-08-12 | 创建；Phase 1 完成（BlockCheckpoint + ToolFinished 双发修复，commit `c18e07b`）；核实 DeepSeek 文档约束；汇总 rc4 前待办 |
