# Ringing 双协议迁移计划

## 总结与命名

新协议统一命名为 **Ringing**，最终同时取代：

- `Agent2Ui`：后端到客户端事件协议。
- `Ui2Agent`：客户端/daemon 到 agent 的命令协议。

Ringing 是双向、分频道协议族：

```text
Ringing
├─ Control
├─ Conversation
└─ Tool
```

本计划属于**协议与运行时架构升级**，其中包含前后端连接方式升级，但不把
Ringing 绑定到某一种传输：

```text
Provider HTTP/SSE 或 WebSocket
        ↓ provider adapter（解析、修复、归一化）
DomainCommand / DomainEvent
        ↓ Ringing projector / router
┌─────────────────────────────────────────────────────┐
│ client boundary: HTTP command/query + 3×SSE event   │
│ worker boundary: framed OS pipe                     │
│ optional boundary: WebSocket（PTY/realtime专用）     │
└─────────────────────────────────────────────────────┘
```

根因判断固定为：

> 当前主要故障并非 WebSocket 本身，而是 Agent2Ui 同时承担领域事件、
> 传输帧、重放日志和前端状态输入，令瞬时增量、可靠终态、错误和连接状态
> 共享同一队列、背压与恢复语义。

因此，替换 WebSocket 而不拆分领域模型、可靠性等级、快照和消费预算，不算
完成 Ringing 升级。

公共类型命名固定为：

```text
RingingChannel
RingingEventEnvelope
RingingCommandEnvelope
RingingCommandAck
RingingEventBatch
RingingChannelSnapshot
RingingContentRef
RingingWorkerCommandEnvelope
RingingWorkerEventEnvelope

RingingControlEvent
RingingConversationEvent
RingingToolEvent

RingingControlCommand
RingingConversationCommand
RingingToolCommand
```

线协议标识固定为：

```json
{
  "schema": "deepx.Ringing",
  "version": 1
}
```

能力名称固定为：

```text
Ringing_v1
Ringing_session_cutover_v1
Ringing_batch_v1
```

## 架构硬规则

内部业务层使用中立模型：

```text
Legacy input ───────→ DomainCommand ───────→ Agent core
Ringing command ───→ DomainCommand ───────→ Agent core

Agent core ─────────→ DomainEvent ─┬───────→ Ringing event
                                   └───────→ LegacyProjector → Agent2Ui
```

Ringing 固定分为四层，依赖只能向下：

```text
Domain        业务命令、业务事件、不变量
Projection    snapshot、cursor、content reference
Wire          Ringing envelope、版本、能力协商
Transport     HTTP、SSE、WebSocket、OS pipe、in-process channel
```

- `Domain` 不得知道 SSE、WebSocket、HTTP、pipe、JSON frame 或 Electron。
- `Projection` 只能从领域状态/领域事件生成，不从 legacy wire 反推。
- `Wire` 不决定业务可靠性；可靠性由事件定义显式声明。
- `Transport` 可以被替换，但不得重新解释事件、错误或终态。
- provider adapter 必须在后端把供应商 SSE/WebSocket 转成中立 `DomainEvent`；
  供应商原始 stream frame 永不进入 daemon-client 或 renderer 边界。

严格禁止：

```text
Agent2Ui → Ringing
Ui2Agent → Ringing
Ringing → Agent2Ui → 新前端
Ringing → Ui2Agent → 新后端
```

具体约束：

- `domain` 模块不得引用 `Agent2Ui`、`Ui2Agent` 或 Ringing wire 类型。
- legacy ingress 和 Ringing ingress 分别校验并构造 `DomainCommand`。
- `LegacyProjector` 只能接受 `DomainEvent` 并生成 `Agent2Ui`。
- Ringing serializer 只能接受 `DomainEvent`，不能接受 legacy 类型。
- 新前端/TUI不得把 Ringing 事件转换成 `Agent2Ui` 后交给旧 reducer。
- 尚未迁移的生产点继续只产生 legacy 事件，不允许用 `Agent2Ui → Ringing` 临时桥接。

## 新旧协议区别

| 维度 | Agent2Ui / Ui2Agent | Ringing |
|---|---|---|
| 方向 | 两个独立大枚举 | 同一协议族下的 Command/Event |
| 领域 | 消息、工具、session、错误混杂 | Control、Conversation、Tool独立 |
| 生产模型 | 业务层直接构造线协议 | 业务层只产生DomainCommand/DomainEvent |
| 连接 | 单WebSocket | HTTP命令/查询 + Control/Conversation/Tool三条SSE事件流 |
| worker边界 | stdin/stdout legacy JSON-LP | framed OS pipe上的Ringing worker envelope |
| 命令 | 部分Ui2Agent、部分字符串RPC | 原Ui2Agent语义变成typed HTTP command |
| 事件 | 单Agent2Ui流 | 三个独立事件流 |
| 顺序 | 全局seq和session seq | 每频道stream seq + 每session/channel seq，保留因果session seq |
| 恢复 | replay UI事件数组 | 每频道领域snapshot + reliable cursor replay |
| 错误 | 全局字符串Error | 结构化、可关联、可去重的失败终态 |
| 高频输出 | 与关键生命周期共用队列 | 可合并、截断、覆盖；终态可靠 |
| 前端 | 单reducer、单replay buffer | 三个domain store和合成selector |
| 命令确认 | RPC返回与业务终态含混 | accepted/rejected与最终事件明确分离 |
| 大内容 | 直接塞入事件并反复复制 | 事件携带摘要/tail，完整内容通过HTTP content ref读取 |

## Ringing 公共接口

### 事件 Envelope

```text
RingingEventEnvelope {
  schema: "deepx.Ringing"
  version: 1
  channel
  delivery: "reliable" | "replaceable" | "ephemeral"
  server_epoch
  seed
  stream_seq
  channel_seq
  session_seq
  event_id
  causation_id?
  correlation_id?
  state_revision?
  event
}
```

### 命令 Envelope

```text
RingingCommandEnvelope {
  schema: "deepx.Ringing"
  version: 1
  channel
  command_id
  client_instance_id
  seed?
  expected_revision?
  command
}
```

命令响应：

```text
RingingCommandAck {
  command_id
  status: "accepted" | "rejected"
  code?
  message?
  retry_after_ms?
}
```

`accepted` 仅代表命令已被校验并进入正确 actor/worker，不代表业务完成。业务终态必须通过带有 `causation_id = command_id` 的 Ringing Event 返回。

### Batch 与 Snapshot

```text
RingingEventBatch {
  channel
  seed
  from_stream_seq
  to_stream_seq
  state_revision
  events
}

RingingChannelSnapshot {
  channel
  seed
  baseline_seq
  state_revision
  snapshot_version
  state
}
```

Snapshot 必须表达领域状态，禁止使用事件数组模拟状态。

### 可靠性等级与回放

- `reliable`：生命周期、终态、interaction、错误和revision变更。进入有界journal，
  必须按cursor回放，不能静默丢弃。
- `replaceable`：message/reasoning/tool progress等增量。允许按identity合并或用较新
  checkpoint覆盖，不承诺逐token重放。
- `ephemeral`：仅用于诊断性live提示，不进入snapshot和journal。
- terminal事件发送前必须flush或覆盖同identity的replaceable事件。
- journal只保存可靠事件和稀疏progress checkpoint，禁止保存每个provider token。
- cursor超出保留窗口时发送`ringing.reset_required`，客户端通过HTTP读取权威snapshot；
  禁止把历史错误、retry或toast重新注入snapshot。
- 相同`event_id`至少一次投递但只允许应用一次；frontend reducer必须幂等。
- `stream_seq`在`server_epoch + channel`内全局递增，供一条SSE连接恢复；`channel_seq`
  在`seed + channel`内递增，供领域状态检测乱序。不得试图用一个SSE `Last-Event-ID`
  表达多个session cursor。

### 大内容外置

工具完整输出、compact archive、超大diff和诊断内容使用：

```text
RingingContentRef {
  content_id
  media_type
  bytes
  sha256
  truncated
}
```

事件只携带可渲染tail、统计信息和`content_ref`。客户端通过带鉴权的HTTP GET按需读取，
服务端支持range/分页，并给content设置会话所有权和生命周期。API key、provider原始
响应和未脱敏错误禁止进入content store。

## 目标拓扑与传输分工

### daemon-client边界

普通HTTP负责：

```text
POST /ringing/v1/clients/open
POST /ringing/v1/leases/renew
POST /ringing/v1/commands/{control|conversation|tool}
GET  /ringing/v1/snapshots/{channel}/{seed}
GET  /ringing/v1/content/{content_id}
GET  /ringing/v1/query/...
```

三条独立SSE负责server→client事件：

```text
GET /ringing/v1/events/control
GET /ringing/v1/events/conversation
GET /ringing/v1/events/tool
```

SSE frame使用标准字段：

```text
id: <server_epoch>:<channel>:<stream_seq>
event: <Ringing event type>
data: <RingingEventEnvelope JSON>
```

约束：

1. Electron main持有daemon token和SSE连接；renderer不得直接持有token。
2. Electron main可以使用支持header的fetch stream，不把token放入query string。
   `client_session_id`同样通过header传递。
3. main→renderer必须发送完整batch，禁止像现有`EventBatch`一样重新展开为逐事件IPC。
4. SSE断开只表示该频道退化；Conversation或Tool断开不得显示daemon全局断联。
5. Control SSE断开也不立即撤销session lease。lease绑定逻辑`client_session_id`，由有界
   TTL和HTTP renew维护，避免一次网络抖动终止会话所有权。
6. 每频道独立重连、cursor、snapshot和健康状态；全局“后端断联”只能由daemon
   discovery/health与Control lease共同判定。
7. HTTP command ack只表达accepted/rejected；业务完成仍由对应频道可靠终态表达。

### daemon-worker边界

- daemon与本地agent worker使用framed OS pipe；若未来同进程运行，可替换为in-process
  bounded channel，但语义不变。
- stdin只承载`RingingWorkerCommandEnvelope`，stdout只承载
  `RingingWorkerEventEnvelope`，stderr只承载脱敏日志。
- frame必须有长度上限、版本、方向、channel、session、command/event id。
- stdout/stderr必须并发drain；spawn、读写、等待和teardown共享同一总超时与cancel。
- worker不对外提供SSE，也不把provider SSE复制到pipe。

### WebSocket保留范围

WebSocket不是Ringing默认承载，仅在以下能力出现时单独协商：

- PTY按键/resize与双向字节流。
- 实时音频或其它真正高频双向媒体。
- 未来远程控制需要单连接全双工且HTTP/SSE不可用的环境。

普通message、compact、tool progress、permission和session管理不得仅因为“已有WS代码”
继续使用WebSocket。

旧客户端继续建立现有`/control/v1` WebSocket，不会收到Ringing消息。双协议期间，
legacy WebSocket和Ringing HTTP/SSE并行存在，互不嵌套。

legacy Control protocol在双协议期保持版本1且不承载Ringing frame。新客户端先调用
`POST /ringing/v1/clients/open`完成版本/能力协商；端点不存在或版本不兼容时才显式选择
legacy，禁止在同一连接上猜测frame类型。

## 相对Codex与Reasonix的取舍

- 采用Codex的优点：provider SSE/WebSocket只停留在adapter层；核心消费统一领域事件；
  transport失败可以回落但不改变业务协议。
- 不复制Codex的单一客户端事件管道：DeepX已有Electron IPC和高吞吐exec progress，
  需要Control/Conversation/Tool物理隔离和独立消费预算。
- 采用Reasonix的优点：HTTP POST命令、SSE事件、共享typed event和前端帧批处理。
- 不复制Reasonix慢订阅者直接丢frame再整体读取history的策略：DeepX的permission、
  compact terminal和tool terminal不能丢，必须区分reliable与replaceable。
- DeepX额外增加：领域snapshot、双序号、content ref、逻辑lease、terminal revision和
  main→renderer整batch IPC。这些是当前三个故障的直接约束，不是通用框架装饰。

## 每会话、每频道切流

客户端维护：

```text
sessionChannelMode[seed][channel] {
  event_protocol: legacy | Ringing
  command_protocol: legacy | Ringing
}
```

事件和命令可以分阶段切换，但同一方向、同一session/channel只能有一个权威协议。

事件切流采用两阶段提交：

1. 客户端发送`channel_prepare`。
2. 服务端先建立SSE live boundary，再生成Ringing领域snapshot，并缓冲boundary后的可靠事件。
3. 客户端应用snapshot后发送`channel_commit`。
4. 服务端原子切换event owner，停止向该客户端发送对应legacy事件并释放缓冲。
5. prepare失败、超时或断线时保持legacy。

命令切流：

1. 客户端请求`command_mode_prepare`。
2. 服务端等待该session/channel已有legacy命令完成入队。
3. 服务端返回Ringing command mode ready。
4. 客户端之后只发送Ringing command。
5. `command_id`必须幂等；重连重试不得重复执行已经accepted的命令。

已经切换到Ringing的频道发生故障时保持Ringing模式，通过cursor/snapshot恢复，不自动退回legacy。

切流期间可以从同一个`DomainEvent`同时投影legacy与Ringing用于影子验证，但只有一个
协议能进入可见store。严禁把已经序列化的legacy frame作为Ringing的生产源。

## 实施状态（2026-07-31）

> 本文档为迁移计划。以下为当前实施进度快照，随迁移推进更新。
> 里程碑提交：`912810d feat(ringing): Ringing 双协议迁移 + skills 自动卸载移除`
> （相对 HEAD `fe957f2`，121 文件，+8257/-318；Rust 48 套件全绿，前端 tsc 0 错误、177/182）

### ✅ 已完成

- **映射清单**（阶段 0 产物）：`docs/ringing-migration-map.md` — 42 事件/21 命令/10 帧全量映射、
  可靠性矩阵、snapshot 资格、10 项设计决策定稿（Q1-Q10）。
- **domain 层**：`crates/deepx-domain` — DomainCommand(21)/DomainEvent(36)/RingingChannel/
  Delivery，零 legacy/wire 依赖（架构测试 `ringing_architecture.rs` 3/3 保证）。
- **wire 层**：`crates/deepx-ringing` — envelope/ack/batch/snapshot/content_ref/worker frame/
  能力协商（`Ringing_v1`/`Ringing_session_cutover_v1`/`Ringing_batch_v1`）。
- **daemon 运行时**：`deepx-runtime/ringing/` — 三频道 router（reliable FIFO + replaceable
  slots + FIFO 淘汰）、有界 journal（event_id 幂等 + CursorExpired）、领域 snapshot
  projection（禁事件数组模拟状态）、分级 outbox（背压）、sequencer（双序号 + revision）、
  ToolProgressCoalescer（16ms/256KiB/tail）、ContentStore（10MiB 外置/sha256/所有权/TTL）、
  两阶段切流状态机（prepare/commit/abort/sticky）、LegacyProjector（DomainEvent→Agent2Ui
  唯一合法出口，无对应表达返回 None）。
- **传输层**：`deepx-daemon/ringing_http.rs` — `clients/open`（能力协商 + lease 签发）、
  `leases/renew`（TTL 30s）、`commands/{channel}`（校验 → 幂等表 → worker 转发 → 失败回滚）、
  `snapshots/{channel}/{seed}`（Conversation 频道返回持久化消息构建的完整 turns）、
  三条独立 SSE（`id: epoch:channel:seq` + Last-Event-ID 按 cursor 回放可靠 tail +
  `ringing.reset_required` + keepalive + 独立重连）；server.rs peek 分流，legacy WS 与
  Ringing HTTP/SSE 单端口并行互不嵌套。
- **恢复语义收口**：SSE 重连先订阅再回放，跨 seed 按全局 `stream_seq` 合并；journal 记录
  `evicted_through`，仅当某 seed 确有被淘汰且 seq > cursor 的事件才发
  `ringing.reset_required`（避免"晚创建 session 首事件 seq 大"误报）；Electron 客户端修复
  typed event 帧解析（此前只处理 `event: message` 导致事件全丢）、Last-Event-ID 携带
  server_epoch，收到 reset 后经 HTTP 重取 snapshot 并重置流 cursor。
- **causation 贯通**：worker 事件信封新增 `causation_id`，Ringing 命令执行期间
  `emit_domain` 产出的事件携带 `command_id`（命令作用域 guard），daemon 发布时写入
  `RingingEventEnvelope.causation_id`；业务终态可与 accepted 命令关联。
- **领域事件生产点补齐**：`UsageUpdated`、`ToolCallPrepared`、`ProviderToolStatus`
  （web_search）、`AuditRecorded`、`ToolFailed`（拒绝路径）、`ToolNotice`（compact）、
  `DashboardUpdated`（worker 仪表盘 5 处）已双发接入；daemon 活动状态变化改为
  `SessionActivity` 与 `SessionActivityChanged` 双发；前端三 store 增加对应消费
  （dashboard/usage/provider status/notices/audits）。`SystemNotice` 无 legacy 数据源，
  待 daemon 通知集成（升级/维护）时接入。
- **worker seed 修复**：Ringing worker 事件信封改用真实 session seed（此前硬编码
  `"worker"`，会导致所有会话事件落入同一伪 seed）。
- **ConversationSnapshot HTTP**：`GET /ringing/v1/snapshots/conversation/{seed}` 从
  `SessionManager::load_for_resume` 持久化消息构建完整 turns（中立 JSON，非 legacy wire），
  与 hub 领域投影摘要合并返回。
- **worker 边界**（阶段 1）：stdin/stdout `wire` 判别（无 wire→legacy / `Ringing_domain_v1`→
  Ringing / 未知→拒绝）；Ringing 命令→legacy ingress 映射（19 命令，SessionClose 显式拒绝）；
  writer 双协议通道（`WriterEvent`）。
- **生产点双发**（零 `Agent2Ui→Ringing` 转换函数）：engine_tool（ToolStarted/ToolProgress/
  ToolFinished/CodeChanged）、engine_turn/input（TurnStarted/RoundDelta Answering+Thinking）、
  loop_core（SessionStateChanged/AgentLifecycleChanged）；daemon 双投管道
  （hub.publish → Ringing 客户端 + LegacyProjector → EventBus → legacy 客户端）。
- **前端**（阶段 1）：`scripts/ringing-bindings.sh`（ts-rs bindings 合并 + `--check` 漂移
  检查，48 文件）；三 store（Control/Conversation/Tool reducer + AppliedEventRegistry 幂等，
  7 vitest）；`electron/ringingClient.ts`（三频道 SSE 独立重连、整 batch 回调、token 仅 header、
  lease renew）。
- **responses API 真迁移**：base system → 顶层 `instructions`（DeepSeek 文档语义），
  动态注入（skills catalog/envelope）保持 developer item。
- **skills 自动卸载移除**：删 lease/review_due 全链路（激活后保持注入直到显式 release），
  长程任务后系统提示词前缀稳定、缓存命中不再被破坏。
- **存量编译故障修复**：UserInput.images 测试同步（4 文件）、TUI 6 处、companion
  PlanSubmitted、proto Dashboard 字段。

### ⚠️ 部分完成（基础设施就绪，未接线/未切换）

- **生产点双发**：三频道全量补全（Tool：ToolStarted/Progress/Finished/Failed/CodeChanged/
  PermissionRequested/RoundCompleted；Conversation：TurnStarted/TurnCompleted/TurnFailed/
  RoundDelta/RoundCompleted/CompactStarted/Progress/Finished/ProviderRetrying/
  ConversationCancelled；Control：SessionState/AgentLifecycle/InteractionRequested/Resolved/
  PlanReviewRequested/Resolved/SkillsUpdated/OperationFailed×4）。
- **阶段 5（Ringing Command）**：HTTP 命令已真正执行（校验 → 幂等 → wire 转发）；cutover 端点
  已接 HTTP（`POST /ringing/v1/cutover/events/{channel}` prepare/commit/abort +
  `POST /ringing/v1/cutover/commands/{channel}` 命令切换，lease 校验 + 409 冲突语义）；
  legacy 通道按切流状态过滤（domain 投影 + Agent2Ui 直发双路径，36 变体归属表）；
  但 `sessionChannelMode` 默认值仍为 legacy（发布周期 1 翻转）。
- **大内容外置**：ContentStore 写入侧（daemon 拦截 ToolFinished >10MiB → store + tail +
  output_ref，外置时跳过 legacy 投影）+ 读取侧（`GET /ringing/v1/content/{id}?seed=`）已闭环。
- **legacy 双发去重**：daemon legacy 通道 5s 相同 JSON 去重（双发 + 投影双路径）。
- **前端切流闭环（桌面）**：electron main RingingManager（三 SSE + sessionChannelMode 表 +
  cutover/snapshot IPC）；renderer 三 store 影子模式 + 调试面板切流按钮（prepare→commit→reload）；
  SessionPresentationSelector（已切流会话主 UI 数据源切换为 Ringing store 投影）；
  snapshot 摘要重建；ConversationSnapshot HTTP 已实现，renderer 完整 turns 消费待前端阶段接入。
- **命令/查询接管（契约 `docs/ringing-command-query-contract.md`）**：
  `GET /ringing/v1/query/*` 已实现（session.list/meta/activity/dashboard/get_activity、
  workspace.get/status、config.load、skills.list_tools、todo.status、daemon.version；
  同时接受斜杠与点号路径，seed 依赖方法缺参 400）；
  `SessionClose` 在 daemon 侧拦截（registry close + hub 发布
  `SessionStateChanged{Closed}`，causation=command_id，幂等关闭，无 seed 400）；
  Electron IPC `ringing:command` / `ringing:query` 已接线；
  renderer 命令路由 `ringingCommandRouter`（按 (seed,channel) commandProtocol 接管
  send/cancel/compact/undo/set_mode/load_more/close/resume/new/interaction/skills，
  "ringing not connected" 回退 legacy）+ `ringingCommands` localStorage 强制开关；
  只读查询在命令已切流的会话上走 Ringing，失败安全回退 legacy。
- **持久化 journal**：`JournalStore`（append-only JSONL `{data}/ringing/journal/{channel}/{seed}.jsonl`
  + cutover.json 原子写）；`RingingHub::with_persistence` 启动装载重建
  journal/router/projection/sequencer/cutover；I/O 失败只记录日志不阻塞 publish；
  daemon 启动改走持久构造（`server.rs`）。
- **发布周期 1**：基础设施与 identity spine ✅；Tool 默认 Ringing、按 session 切流 ⬜。

### ⬜ 未开始

- TUI 三个 Ringing handler。
- 发布周期 2（Conversation/Control 默认 Ringing、命令逐频道切换）。
- 最终迁移（固化 v1、删除 Agent2Ui/Ui2Agent/LegacyProjector/legacy WS/旧 reducer/旧 bindings）。

### 下一步建议（按优先级）

1. 发布周期 1 实战：补 daemon 重启 → 面板影子验证 → 切流 Tool → 主 UI 接管验证。
2. ConversationSnapshot HTTP（历史恢复闭环）；3. renderer 消费完整 turns；
4. TUI handler；5. 发布周期 2；6. 最终删除 legacy。

## 迁移阶段

### 0. 先固定故障基线与可观测性 — ⚠️ 部分（映射清单✅；回归fixture/诊断字段⬜）

- 为compact期间“假断联”、单HTTP错误重复、10 MiB exec progress积压建立回归fixture。
- 记录每频道producer rate、coalesce ratio、queue depth、IPC batch size、renderer commit
  duration、cursor gap和terminal latency；只记录类型、长度和id，不记录正文。
- 为当前legacy链路加上唯一`error occurrence id`诊断字段，但不改变旧wire语义。
- 明确现有`Agent2Ui`每个variant的领域归属、可靠性等级、snapshot资格和最终Ringing类型。

### 1. Ringing基础设施与身份骨架 — ✅ 完成（见「实施状态」）

- 新增DomainCommand、DomainEvent和Ringing协议类型。
- agent worker输入输出支持显式判别：
  - legacy记录保持原格式。
  - 新记录使用`wire: "Ringing_domain_v1"`。
- worker reader必须先检查`wire`，禁止使用untagged猜测。
- daemon建立三个独立ChannelRouter、可靠journal、领域snapshot projection和分级发送队列。
- legacy EventBus继续存在，但只能接收DomainEvent经过LegacyProjector生成的事件，或尚未迁移的原始legacy事件。
- 增加HTTP command/query、三条SSE、逻辑client session/lease、per-session/channel cursor和两阶段切流。
- 先迁移所有频道共用的session/turn/operation/interaction identity，避免Tool先迁移时继续从
  legacy字符串推断turn归属。
- 自动生成Rust→TypeScript bindings，并由CI检查漂移。
- 初始默认全部legacy；Ringing只运行协议、transport和shadow projection测试，不进入UI。

### 2. 第一优先级：Tool Event — ⚠️ 部分（生产点双发✅；ToolPermissionRequested/content端点/默认切换⬜）

完整迁移工具领域，不能只迁移progress：

```text
ToolCallPrepared
ToolStarted
ToolProgress
ToolFinished
ToolFailed
ToolPermissionRequested
ToolNotice
AuditRecorded
CodeChanged
```

`ToolProgress`必须包含：

```text
tool_call_id
turn_id
round_num
stream
seq_start
seq_end
chunk
dropped_bytes
truncated
```

流控规则：

- 同一工具/stream在16ms内合并。
- 单batch不超过256 KiB。
- 每工具最多保留256 KiB progress tail。
- 前端自动渲染最多128 KiB并显示截断提示。
- terminal发送前先flush该工具保留的progress。
- Electron必须把整个batch作为一次IPC发送，禁止展开。
- Tool router使用独立reliable queue和replaceable slots；慢消费者只能覆盖progress，
  不能阻塞或丢弃ToolFinished/ToolFailed。
- 10 MiB以上完整输出只进入content store，SSE不重发完整正文。
- Desktop建立Tool store；TUI直接处理RingingToolEvent。
- ToolSnapshot直接从MessageStore和当前工具运行状态构建。
- Tool切为Ringing后，legacy `RoundComplete.tool_calls`不再拥有工具卡渲染权。

### 3. 第二优先级：Conversation Event — ⚠️ 部分（TurnStarted/RoundDelta双发✅；终态/compact/usage等⬜）

迁移：

```text
TurnStarted
RoundDelta
RoundCompleted
TurnCompleted
TurnFailed
ProviderRetrying
UsageUpdated
CompactStarted
CompactProgress
CompactFinished
ConversationCancelled
```

要求：

- compact事件携带`compact_id`。
- CompactFinished具有`completed/skipped/failed/cancelled`明确状态。
- provider HTTP失败只生成一个可靠OperationFailed/TurnFailed，必须绑定
  `operation_id + occurrence_id`；retry与最终失败不得共用event id。
- message/reasoning/compact delta按帧合并。
- terminal不得排在未受限delta backlog之后。
- terminal包含完整最终文本或content ref，允许客户端直接覆盖未消费完的delta backlog。
- compact期间transport健康、session activity、compact operation状态独立；compact开始/
  结束不得触发backend connected状态。
- ConversationSnapshot直接从持久化session消息构建。
- v2会话不再使用Agent2Ui replay签名去重，也不通过错误字符串触发resume。

### 4. 第三优先级：Control Event — ⚠️ 部分（SessionState/AgentLifecycle双发✅；Interaction/OperationFailed等⬜）

迁移：

```text
SessionStateChanged
SessionActivityChanged
AgentLifecycleChanged
DashboardUpdated
InteractionRequested
InteractionResolved
PlanReviewRequested
PlanReviewResolved
SkillsUpdated
SystemNotice
OperationFailed
```

要求：

- transport状态、agent进程状态和业务失败完全分离。
- 错误携带`error_id`、scope、code、retryable、dedupe_key和operation id。
- Toast按error id去重并有数量上限。
- ControlSnapshot只承载session/control状态，不承载Conversation或Tool事件数组。

### 5. Ringing Command迁移 — ⚠️ 部分（HTTP执行/幂等/wire转发✅；切流默认值与cutover端点⬜）

在三个Event频道稳定后，迁移所有原`Ui2Agent`语义。

Control命令：

```text
SessionCreate
SessionResume
SessionClose
SessionShutdown
AgentReloadConfig
InteractionAskRespond
InteractionAskDismiss
PlanReviewRespond
SkillsActivate
SkillsRelease
SkillsReload
```

Conversation命令：

```text
ConversationSendMessage
ConversationCancel
ConversationUndoTurn
ConversationCompact
ConversationLoadMore
ConversationSetMode
```

Tool命令：

```text
ToolInvoke
ToolPermissionRespond
```

行为要求：

- `SessionResume` accepted后，由三个频道分别完成snapshot/cursor恢复。
- `ConversationSendMessage` accepted表示输入已被session actor接收；TurnStarted是开始执行的权威事件。
- `ConversationCompact` accepted不代表成功；CompactFinished才是终态。
- Permission/Ask/Plan响应必须携带对应interaction id和expected revision。
- 断线发生在accepted之后时，客户端通过command id查询/等待终态，不得重新执行。
- legacy RPC handler和Ringing command handler分别映射到DomainCommand；两者不得互相调用。
- 与Agent无关的config、git、workspace查询RPC不要求在本计划中删除，除非其语义原本属于Ui2Agent。
- 只读查询继续使用HTTP query，不为了“协议统一”伪装成Command/Event。

## 前端与TUI状态结构

Desktop建立：

```text
ControlStore
ConversationStore
ToolStore
LegacySessionStore
SessionPresentationSelector
```

Selector根据每session/channel owner合并领域状态，禁止合成Agent2Ui。

前端每频道独立维护：

```text
connection status
server epoch
cursor
snapshot status
pending cutover
command mode
```

TUI增加三个Ringing handler，并保留legacy handler两个发布周期。TUI不得通过LegacyProjector消费Ringing。

Renderer调度固定为：

- main进程按频道收包和校验，按batch通过preload发送。
- renderer每session/channel每animation frame最多commit一次。
- reliable事件到达时先drain/覆盖同identity replaceable状态，再原子应用terminal。
- selector只读取已物化状态，不在render期间重放事件数组或拼接全部历史输出。
- 完成态到达后，未处理的旧progress通过`state_revision`立即作废，不允许继续追赶渲染。

## 迁移错误热点

- `Agent2Ui`当前同时充当领域模型、wire frame、snapshot projection和frontend reducer输入。
- agent stdout/stdin当前分别严格解析Agent2Ui和Ui2Agent。
- activity tracker当前通过序列化后的legacy `type`判断生命周期。
- EventBus直接存储ControlServerMessage和Agent2Ui projection。
- persisted session snapshot直接创建SessionRestored。
- 单outbound queue隐含全局到达顺序。
- Electron当前拆散EventBatch并逐事件IPC。
- Electron连接错误、频道错误与业务错误容易汇入同一status/toast路径。
- frontend replay、local snapshot和reducer假设只有一个全局流。
- resume同时依赖RPC、SessionRestored和session.replay_events，容易产生重复baseline。
- TUI对未知ControlServerMessage静默忽略。
- TypeScript协议bindings当前依赖人工复制。
- 命令重试没有统一command id，accepted和completed边界不清晰。
- 当前compact/token calibration改动属于现有用户工作，实施时必须保留。

## 发布顺序

发布周期1：— ⚠️ 进行中（基础设施与identity spine✅；Tool默认Ringing、按session切流⬜）

- Ringing HTTP/SSE/pipe基础设施与identity spine。（✅ 已交付）
- Tool Event默认Ringing。（⬜ 双发就绪，默认值未切）
- Conversation、Control Event和全部Command保持legacy。（✅ 现状符合）
- Desktop和TUI均支持按session切流。（⬜ Desktop SSE客户端就绪，cutover接线未做；TUI未开始）

发布周期2：— ⬜ 未开始

- Conversation Event和Control Event默认Ringing。
- 原Ui2Agent命令逐频道切换为Ringing Command。
- legacy仍保留显式诊断回滚开关。

两个兼容周期结束并满足验收门槛后：— ⬜ 未开始

- 固化Ringing v1最低客户端/daemon版本并拒绝不支持的组合。
- 删除Agent2Ui和Ui2Agent。
- 删除`/control/v1` legacy WebSocket业务事件/RPC入口、LegacyProjector、legacy ingress、旧EventBus projection、旧replay buffer、旧前端reducer和legacy TS bindings。
- 删除对应字符串session/interaction RPC兼容入口。
- 保留不属于Agent命令域的daemon查询RPC。
- 全仓搜索要求Agent2Ui/Ui2Agent生产引用为零。

## 测试与验收

> 状态：架构测试 ✅（3/3 通过，见 `deepx-runtime/tests/ringing_architecture.rs`）；
> 协议/幂等/边界/SSE/压力/UI 类目 ⬜（组件级测试已覆盖部分，端到端验收未执行）。

- 架构测试：
  - domain模块不能引用Agent2Ui、Ui2Agent或Ringing wire。
  - 不存在Agent2Ui→Ringing、Ui2Agent→Ringing转换函数。
- 混合版本：
  - 新客户端+旧daemon走legacy。
  - 旧客户端/TUI+新daemon走legacy。
  - 新客户端+新daemon可对不同session采用不同频道模式。
- 命令幂等：
  - accepted前断线可安全重试。
  - accepted后断线不得重复执行。
  - command id与最终causation id正确关联。
- HTTP/SSE/lease：
  - SSE连接不能独立获得或续租session lease。
  - lease renew停止后在有界TTL内释放，不依赖一次socket close。
  - Tool/Conversation断开只恢复自身，不触发全局断联。
  - `Last-Event-ID`有效时只回放可靠tail；gap时强制snapshot。
  - native/EventSource限制不导致token进入URL、日志或renderer。
- 压力测试：
  - 10 MiB exec输出时内存保持有界。
  - renderer每session/channel每帧最多commit一次。
  - Tool洪峰期间Control HTTP command/lease renew本地p95低于250 ms。
  - compact成功或HTTP失败后1秒内显示唯一终态。
  - 后端工具完成后前端不存在分钟级渲染积压。
  - terminal到达后旧revision progress不再触发render。
- UI：
  - 每session/channel/direction只有一个权威协议。
  - 单个error id最多生成一个Toast。
  - snapshot与live并发不重复应用terminal。
- 最终验证：
  - 协议单测。
  - worker输入输出边界测试。
  - daemon HTTP command/query、三SSE和lease集成测试。
  - SSE partial frame、malformed data、重连、cursor gap和慢消费者测试。
  - Desktop/TUI focused tests。
  - TypeScript typecheck。
  - Rust affected crates check。
  - 高吞吐端到端压力测试。

## 默认假设

- Ringing是整个新双向协议的正式名称，代码、能力名、日志和文档统一使用该拼写。
- Ringing是领域协议族，不等于SSE、HTTP、WebSocket或pipe；transport crate不得成为
  domain crate的依赖。
- daemon和agent worker来自同一可执行文件，不支持新worker与旧daemon内部混搭。
- legacy使用现有WebSocket，Ringing默认使用HTTP+SSE；二者在迁移期并行但不互相封装。
- 已切换的Ringing频道不会自动退回legacy，故障必须在Ringing恢复路径中解决。
- 迁移诊断只记录频道、类型、长度、序号、耗时、command id和丢弃字节数，不记录消息正文、工具参数、provider响应或凭据。
