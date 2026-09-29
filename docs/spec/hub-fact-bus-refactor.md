# SPEC：Hub 事实总线重构（H1 · 思路 B）

> 状态：可执行 spec，未开工。
> 原则：**先砍再补测试**——每个阶段先删代码到编译绿，测试作为阶段验收在阶段尾补齐，
> 不为旧路径补测试。
> 关联：`docs/legacy-compat-cleanup-draft.md`（H1/I7 条目）、agent loop 插件化重构（见 §7 协同约束）。
> 事实核查日期：2026-09-27（本文所有 file:line 均基于当日 HEAD，动手前用 grep 复核）。

---

## 0. 现状（以实际代码为准）

### 0.1 已经存在的事实总线（这是 B 可行的根据）

`qaqh-session/src/projection/sink.rs` 定义了进程级 `ProjectionSink` trait：
`publish(session_dir, fact: &SessionFact, events: &[ProjectionEvent])`。
daemon 启动时由 `V2ProjectionHub::install()`（`qaqh-runtime/src/ringing/v2.rs:141`）
安装自身为全局 sink。**durable append 成功之后**才 publish——消费者永远不会看到
未落盘的 fact。`V2ProjectionHub`（`ringing/v2.rs`）已经具备：per-session 可重建投影、
committed canonical log 重放、游标（`CanonicalCursor`/`CursorToken`）、单流
`RingingV2EventEnvelope<ProjectionPayload>` 发布。

**即：facts 扇出 + 游标 + 重放 = 已经存在。B 的本质不是造新总线，是消灭与它并行的
另一条总线。**

### 0.2 与之并行的 v1 总线（要砍的）

`RingingHub`（`crates/qaqh-runtime/src/ringing/hub.rs`，~4000 行）：
- `publish(session_id, event: DomainEvent)`（hub.rs:951）→ v1 `RingingEventEnvelope`
  → per-channel + per-(channel,seed) broadcast 扇出（hub.rs:1150/1166）；
- `replay_since` / `replay_channel_since`（hub.rs:1221/1266）——**第二套游标重放**；
- `snapshot` / `conversation_snapshot`（hub.rs:1311/1325）——v1 频道快照；
- 命令路由 + `PendingCommandStore`（幂等回执）+ `RingingLeaseStore` + content store
  + interaction content + worker liveness + checkpoint/journal 持久化。

### 0.3 v1 总线的全部消费方（迁移对象，穷举）

| 消费方 | 位置 | 需要什么 |
|---|---|---|
| V2ProjectionHub | 经 sink？**否**——它吃 facts，不吃 v1 事件 | 无需迁移 |
| daemon 命令回执折叠 | `daemon/src/server.rs:164-212`（`subscribe_channel`） | 命令终态回执 |
| subagent 收集器 | `qaqh-subagent/src/lib.rs`（v1 `EventBatch`，等 `TurnCompleted/TurnFailed`） | 这两类终态事件 |
| orphan_seal / 领域投影 | runtime 内部（见 qaqh-client lib.rs 注释） | 待逐个盘点 |
| `host_impl.rs` 的 `envelope_to_batch` | `runtime/src/host_impl.rs:1631` | v1 batch 包装（随之删除） |

### 0.4 命令面的 JSON 圈（阶段 1 目标）

`daemon/axum_impl/v2.rs:1000-1021`：v2 handler 把 v2 信封 `serde_json::to_vec` 成 v1
信封 JSON → 调用已无路由的 v1 handler `command.rs:59 handle_command` → hub。
driver claim/release 同款（v2.rs:630-653）。

---

## 1. 目标形态（North Star）

```text
命令：v2 handler ──► 引擎执行 ──► durable fact append ──► sink.publish
                                  （唯一发布 API）

事件：V2ProjectionHub = 唯一事件总线（facts 扇出 + 游标 + 重放）
        ├── v2 SSE 单流（序列化 facts，翻译层消失）
        ├── subagent（facts 过滤订阅：TurnTerminal 等）
        ├── timeline（订阅或保留独立 hub，见阶段 4 决策点）
        └── 持久化（内存总线与 events.jsonl 同构）

RingingHub 残余职责：命令路由/幂等回执、content store、interaction、
                     worker liveness（全部是服务，不是事件总线）
```

**不变量（每阶段验收线，红一条即回滚该阶段）**：
1. 因果序：单流内事件有序，`stream_key` demux 语义不变；
2. 游标重放：断线续传不丢不重，`CursorExpired` 边界行为不变；
3. 幂等回执：command_id 去重 + `ack.existing` 语义不变；
4. lease 归属校验仍发生在命令入口；
5. durable-before-publish：消费者不见未落盘 fact（sink.rs 的既有契约）。

---

## 2. 阶段 1：命令面去圈 【先砍】

> 状态：**已完成（见 docs/handoff/hub-fact-bus-stage-1.md）**。偏差：入口落在
> daemon `axum_impl::command::execute_command` 而非 `RingingHub`（leases/pending/
> service 在 daemon AppState，hub 不持有）；ack 全路径统一 v2 形状。

- [x] **1.1** 在 `RingingHub` 上补一个进程内命令入口
      `execute_command(envelope: RingingV2CommandEnvelope) -> RingingV2CommandAck`：
      把 `command.rs:59 handle_command` 的解析后逻辑（不含 HTTP 状态码/JSON body
      组装）搬进来。信封类型直接收 v2。
- [x] **1.2** `axum_impl/v2.rs` 的 `handle_command_v2` 改为直调
      `execute_command`，**删除** v2.rs:1000-1021 的 v1 信封序列化与
      `handle_command` 调用。
- [x] **1.3** driver claim/release（v2.rs:630-653）同样改直调，删 v1 信封构造。
- [x] **1.4** 删除 `command.rs` 的 v1 handler 面与 `mod.rs:54` 的 re-export；
      `axum_server.rs` 内引用它的测试随迁或删除。
- [x] **1.5** 验收：`cargo check --workspace` 绿；`cargo test -p qaqh-daemon --bins`
      全绿（断言 `missing_session_id` 等 code 的测试已在硬切时同步）；
      手工冒烟：webui 发命令 → ack → 事件到达。（check/自动化测试绿；
      手工冒烟未执行，移交下一位）

## 3. 阶段 2：事件侧消费方迁移 【先砍】

> 状态：**2.2/2.3 已完成**（2026-09-28，见 docs/handoff/hub-fact-bus-stage-1.md）。
> 盘点修正（§0.3 表的两处偏差）：① orphan_seal 是**发布者**不是订阅者；
> ② 漏了 `axum_impl/sse.rs` 的 timeline SSE（`hub.subscribe_timeline`）——
> 它消费 hub 内 timeline 专用广播，按 §6 归入"timeline 是否并入 fact 总线"
> 的独立决策点，阶段 3 不动它。

先跑盘点（半天），再逐个迁移：

- [x] **2.1 盘点**：grep `subscribe\b|subscribe_channel|EventBatch|RingingEventEnvelope`
      在 runtime/daemon/subagent 的全部调用点，列成清单追加到本文档附录。
      （本节的表是初始版本，以盘点结果为准。）——盘点结果见下方附录 A。
- [x] **2.2 subagent 收集器**：改为 V2ProjectionHub 单流桥接
      （`host_impl::subscribe` 单线程桥 + `V2Subscription::try_next`，
      ProjectionPayload 翻译成 subagent 自有 `CollectorEvent{AnswerSealed, TurnFinished}`，
      qaqh-subagent 不依赖 qaqh-session）。`envelope_to_batch` 已删除。
      语义收缩：v1 `RoundCompleted.is_final` 提前完成路径删除（等 TurnFinished）；
      `Control(OperationFailed)` 仅日志、fact 侧无对应物，不翻译。
- [x] **2.3 daemon 命令回执折叠**：改挂 canonical fact 投影链——
      `FoldingSink`（daemon server.rs）先 `PendingCommandStore::observe_projection_events`
      再委托 V2ProjectionHub；三频道 `subscribe_channel` 观察循环删除。
      按 spec"最小 diff"选了 sink 链而非命令路径回调。
      降级（有意）：`SkillsUpdated/OperationCompleted/OperationFailed/SessionStateChanged`
      fact 侧无对应物（§6 冻结磁盘格式，不补 fact），相关回执靠 TTL 过期。
- [x] **2.4 orphan_seal / 领域投影**：盘点确认 orphan_seal 是发布者（补终态事件），
      其发布的 4 类事件在 fact 侧均有对应物（TurnFinished/CompactionApplied/
      ToolFinished/InteractionResolved）；它对 hub 的 `snapshot` 读取是内部状态读取，
      非事件总线消费，随阶段 3 保留。
- [x] **2.5** 验收：除 v1 hub 自身（含 journal/outbox/lease_store/router 内部）外，
      `RingingEventEnvelope|EventBatch` 在 daemon/subagent 的运行时消费为零；
      `subscribe_channel|hub.subscribe(` 在 daemon/subagent 为零。
      存量测试：subagent 15 passed、daemon --bins 66 passed。

### 附录 A：阶段 2 盘点结果（2026-09-28）

| 消费方 | v1 使用 | 迁移后 |
|---|---|---|
| subagent 收集器 | `SubagentHost::subscribe`（host_impl.rs:518-567 三频道桥）+ `envelope_to_batch` | V2 单流桥 + `CollectorEvent` |
| daemon 回执折叠 | server.rs 三频道 `subscribe_channel` 循环 | `FoldingSink` fact 链 |
| orphan_seal | （发布者）补终态事件 ×4 | fact 侧已有对应物，无需迁 |
| timeline SSE（sse.rs:164） | `hub.subscribe_timeline` | **未迁**，归 §6 timeline 决策点 |
| 回执折叠依赖的 v1 事件→fact 映射 | — | TurnFinished/CompactionApplied/ToolFinished(failed→Failed)/InteractionResolved·Expired |

## 4. 阶段 3：删除 v1 事件总线 【大砍】

> 状态：**前置审计已完成（2026-09-28），删除面已定型，未动代码**。
> 决策记录：A1（无消费方事件直接删）+ timeline 族 API 保留（§6 决策点未决）。

### 4.0 前置审计结论（actor 发布面副作用）

1. **fact 侧有独立外化机制**：canonical fact 写入路径自带外化
   （`loop_dispatch_conversation.rs::externalize_canonical_content`，inline 上限 8KiB，
   超限入 content store 留 ref）。actor.rs:65 的 `externalize_large_content`
   只服务 v1 广播展示面（10MiB 阈值），**删广播不丢 fact 外化**。
2. **`stash_interaction_body` 是 v2 wire 依赖的持久化副作用**：ask/plan/permission
   正文入 content store 并 pin（"canonical fact 里只有 ref，正文走展示面旁路"）。
   阶段 3 删 `publish_with_causation` 时**必须保留该调用**（与广播解耦，挂在
   WriterEvent::Ringing 分支原地）。
3. **activity 面连带**：`domain_activity_observe` 读的是广播前的事件（tracker 状态机），
   `publish_activity` 发的 `SessionActivityChanged` 也是 v1 publish；fact 侧
   `ControlDelta::Activity`（replaceable）已有对应物，需确认 activity fact 的
   写入点独立于广播后再删。
4. **orphan_seal 的补终态广播目前不写 fact**：它 seal timeline（fact 面）+
   publish v1 事件（conversation/tool/control 孤儿终态）。删广播后 v2 客户端
   看不到这部分孤儿终态的 ConversationDelta——今天 v2 就已缺这段（孤儿场景），
   阶段 3.4 应改为 orphan_seal 直写 fact（ledger append），而不是简单删调用。
5. **（2026-09-28 实测发现，3a 回滚依据）publish 是隐式副作用的载体**：
   `publish_with_causation` 内部完成 ① live_interactions 登记与解除
   （orphan_seal 的 force=false 防误杀守卫依赖；空表会导致 bootstrap 误杀
   1ms 前的 ask——"ask 弹不出"老 bug 回归）；② 交互正文 pin 的释放
   （ToolFinished/Resolved 时 unpin，否则 content store 泄漏）。
   **删 publish 的前置 = 把这两组副作用迁到 fact append / 命令执行路径**。

### 4.1 执行清单（按此顺序，每步 check 绿）

- [x] a. actor.rs：`WriterEvent::Ringing` 分支保留 stash + activity observe，
        ~~删 `publish_with_causation`~~ → **回滚**（见 §4.0.5）：publish 是
        live_interactions 登记与交互 pin 释放的载体，副作用迁移前必须保留；
        `externalize_large_content` 删除维持（fact 外化独立，已验证）。
- [x] b. service.rs / registry.rs：`SessionStateChanged{Closed}`、`ConfigChanged`
        发布点删除（A1 已 grep 确认：4 类事件 wire 层零消费方——webui/TUI 走 v2，
        gate 命中为注释，剩余为 daemon 测试与 producer 自身；连带
        `auth.rs::publish_session_created` 的 SessionStateChanged{Created} 发布点
        与 mod.rs 的 replay 测试随删/随迁）。
- [x] c. orphan_seal：~~广播删除~~ → **回滚**（§4.0.5 同因：pin 释放/守卫/状态
        一致性依赖 publish）；fact 补写仍为遗留债；`snapshot()` 内部读取保留。
- [x] c'. activity：`publish_activity` 删除，tracker 状态机保留
        （/activity 查询权威）；SessionActivityChanged 的 fact 产生侧缺失
        已记录（v2 wire 活动推送目前依赖查询轮询）。
- [x] c''. 退役锁 v1 广播行为的测试 ×4：hub.rs orphan/seal 三测 +
        daemon `session_create_event_carries_command_causation`
        （待 §5.4 重挂到 fact 面）。
- [x] d. hub.rs：删 `publish/publish_with_causation/subscribe/subscribe_channel/
        replay_since/replay_channel_since` + DomainEvent broadcast 通道 +
        journal 事件持久化；**保留** `publish_timeline/subscribe_timeline/
        snapshot/live_watermark`（timeline SSE 与 orphan_seal 依赖）。
        现状：广播面已零生产者/零消费者（registry/service 的 subscribe_channel
        是订阅邮箱机制，非 hub 广播），删除是纯 hub.rs 内部手术。
        （2026-09-29 完成，见 handoff §阶段 3d。偏差：orphan_seal 补终态改为
        进程内收敛入口 `apply_seal_event`（投影 + 序号 + 水位，不广播/不落盘）；
        fact 补写仍为 §4.0.4 遗留债。timeline/内容/lease 等模块保留。）
- [x] e. pending_store：`observe_terminal_event`/`terminal_result`/3 个 v1 折叠测试
        已删除（零调用方）；fact 链 `observe_projection_events` 为唯一折叠入口。
        typed `existing` replay 能力随之彻底移除（降级已在 handoff 记录）。
- [x] f.（部分）验收：`cargo check --workspace` 绿；runtime --lib 312 passed
        （2 failed 为 Windows `sh` 缺失的存量环境失败，与本次无关）；
        daemon --bins 65 passed、subagent 15 passed。§5.4 全量补挂待做。

原清单（供对照，已被 4.1 取代）：

- [ ] **3.1** 删 `RingingHub::publish/publish_with_causation/subscribe/subscribe_channel/
      replay_since/replay_channel_since/snapshot/conversation_snapshot/live_watermark`
      及全部 broadcast 通道与 journal 事件持久化（hub.rs 约一半）。
- [ ] **3.2** 删 `RingingEventEnvelope` 在 qaqh-ringing 的**运行时使用**（crate 内仅剩
      历史命名则顺带改名/删除，wire 无消费方）。
- [ ] **3.3** 删 v1 频道快照相关：`RingingChannelSnapshot`、`Channel`/`ChannelStatus`
      若已无消费方（client 已确认无）。
- [ ] **3.4** agent/actor 侧：所有 `hub.publish(DomainEvent)` 调用点改为
      「durable fact append → sink 自动发布」。**此处与 agent loop 插件化协同，
      见 §7。**
- [ ] **3.5** 验收：`cargo check --workspace` 绿；grep 确认 `DomainEvent`/v1 envelope
      在 runtime 的扇出路径为零；全量 `cargo test --workspace`（此刻补测试，
      见 §5）。

## 5. 测试策略（先砍后补，但不是不补）

- [x] **5.1 迁移前留底**：阶段 1/2 动手前，把 hub.rs 现有 47 个不变量测试跑绿存档
      （`cargo test -p qaqh-runtime --lib ringing::hub`，当前全绿）。
- [x] **5.2 阶段 1 尾**：为 `execute_command` 补最小测试（幂等回执、lease 拒绝、
      missing_session_id），数量 ≤ 5 个，只锁不变量不锁旧形状。
      （`axum_server.rs::command_entry_tests`，5 个，全绿。）
- [ ] **5.3 阶段 2 尾**：subagent 收集器的终态订阅测试改写为 facts 订阅版
      （`RecordingTransport` 测试同改）。
- [ ] **5.4 阶段 3 尾**：把 hub.rs 存活的 47 个不变量测试逐一改挂到新总线
      （因果序/游标/重放三类），**一个都不少**——这是"先砍再补"的"补"。
- [ ] **5.5 终验**：`cargo test --workspace` + webui `bun test` + TUI 测试 +
      手工冒烟（连接/对话/权限/翻页/daemon 重启自愈/远端）。

## 6. 明确不做（scope 外）

- 不改 v2 SSE wire 契约（前端零感知——这是本次重构的自证目标）；
- 不动 canonical log 磁盘格式（events.jsonl + commit marker）；
- 不动 identity sidecar；
- 不做 opencode 式"TUI 进程内直连 runtime"（那是 G/TUI 并仓之后的事）；
- `timeline_hub` 是否并入 fact 总线 = 阶段 3 后的独立决策点（timeline 的 journal
  重建语义独立成套，强行合并收益存疑）。

## 7. 与 agent loop 插件化重构的协同约束

现状（2026-09-27 实测）：`crates/qaqh-runtime/src/agent/` 下 engine_* 文件族
直接调用 hub 的 publish/命令路径；**代码中尚无 plugin 形态**（grep 无命中），
插件化是进行中的方向性重构。

两条线并行时的碰撞点只有一个：**事件发布 API**。约定如下：

1. 插件化定义的新发布面（loop → runtime）必须以 §1 目标形态为准：
   **durable fact append 是唯一发布动作**，不要为新插件再接 v1 `hub.publish`；
2. 若插件化先落地：本 spec 阶段 3.4 直接迁插件 API，阶段 2.3 的回执折叠同理；
3. 若本 spec 先落地：插件化在"只调 fact append"的边界上开工，零返工；
4. 两线**不要同时动** `hub.rs` 与 `engine_turn.rs`/`engine_tool.rs`——
   先合并对方分支再继续，这是唯一的同步纪律。

## 8. 量级与顺序

| 阶段 | 量级 | 风险 | 可独立停止 |
|---|---|---|---|
| 1 命令面去圈 | 1-2 天 | 低 | ✅ |
| 2 消费方迁移 | 2-4 天 | 中 | ✅（迁完即稳定态） |
| 3 删 v1 总线 | 3-5 天 | 中高 | ✅ |
| 测试补齐 | 2-3 天 | 低 | —（阶段 3 的闸门） |

执行顺序即本文档章节顺序；任何阶段后停下，系统都处于"编译绿 + 可发布"状态。
