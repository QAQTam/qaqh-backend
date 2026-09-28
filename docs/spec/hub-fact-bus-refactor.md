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

- [ ] **1.1** 在 `RingingHub` 上补一个进程内命令入口
      `execute_command(envelope: RingingV2CommandEnvelope) -> RingingV2CommandAck`：
      把 `command.rs:59 handle_command` 的解析后逻辑（不含 HTTP 状态码/JSON body
      组装）搬进来。信封类型直接收 v2。
- [ ] **1.2** `axum_impl/v2.rs` 的 `handle_command_v2` 改为直调
      `execute_command`，**删除** v2.rs:1000-1021 的 v1 信封序列化与
      `handle_command` 调用。
- [ ] **1.3** driver claim/release（v2.rs:630-653）同样改直调，删 v1 信封构造。
- [ ] **1.4** 删除 `command.rs` 的 v1 handler 面与 `mod.rs:54` 的 re-export；
      `axum_server.rs` 内引用它的测试随迁或删除。
- [ ] **1.5** 验收：`cargo check --workspace` 绿；`cargo test -p qaqh-daemon --bins`
      全绿（断言 `missing_session_id` 等 code 的测试已在硬切时同步）；
      手工冒烟：webui 发命令 → ack → 事件到达。

## 3. 阶段 2：事件侧消费方迁移 【先砍】

先跑盘点（半天），再逐个迁移：

- [ ] **2.1 盘点**：grep `subscribe\b|subscribe_channel|EventBatch|RingingEventEnvelope`
      在 runtime/daemon/subagent 的全部调用点，列成清单追加到本文档附录。
      （本节的表是初始版本，以盘点结果为准。）
- [ ] **2.2 subagent 收集器**：`HostTransport.events()` 的 `EventBatch` 流改为
      V2ProjectionHub 的订阅 + 过滤（`ProjectionPayload::TurnTerminal` 等）。
      `host_impl.rs` 的 `envelope_to_batch` 删除。subagent 只关心终态，
      过滤放订阅侧，不造新抽象。
- [ ] **2.3 daemon 命令回执折叠**（server.rs:164-212）：改为订阅 facts 中
      命令终态对应的 projection event，或（更直接）由命令执行路径回调——
      **二选一以代码里最小 diff 为准**。
- [ ] **2.4 orphan_seal / 领域投影**：逐个确认它消费的 v1 事件是否已有 facts
      对应物；有的迁，没有的先把该事件补成 fact（在产生侧），再迁。
- [ ] **2.5** 验收：除 v1 hub 自身测试外，`grep RingingEventEnvelope crates/qaqh-{runtime,daemon,subagent}`
      仅剩 hub.rs 内部。

## 4. 阶段 3：删除 v1 事件总线 【大砍】

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

- [ ] **5.1 迁移前留底**：阶段 1/2 动手前，把 hub.rs 现有 47 个不变量测试跑绿存档
      （`cargo test -p qaqh-runtime --lib ringing::hub`，当前全绿）。
- [ ] **5.2 阶段 1 尾**：为 `execute_command` 补最小测试（幂等回执、lease 拒绝、
      missing_session_id），数量 ≤ 5 个，只锁不变量不锁旧形状。
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
