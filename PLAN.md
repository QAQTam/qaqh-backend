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

公共类型命名固定为：

```text
RingingChannel
RingingEventEnvelope
RingingCommandEnvelope
RingingServerFrame
RingingClientFrame

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
| 连接 | 单WebSocket | 三条独立WebSocket |
| 命令 | 部分Ui2Agent、部分字符串RPC | 原Ui2Agent语义全部变成typed command |
| 事件 | 单Agent2Ui流 | 三个独立事件流 |
| 顺序 | 全局seq和session seq | 每session/channel独立seq，保留因果session seq |
| 恢复 | replay UI事件数组 | 每频道snapshot + cursor replay |
| 错误 | 全局字符串Error | 结构化、可关联、可去重的失败终态 |
| 高频输出 | 与关键生命周期共用队列 | 可合并、截断、覆盖；终态可靠 |
| 前端 | 单reducer、单replay buffer | 三个domain store和合成selector |
| 命令确认 | RPC返回与业务终态含混 | accepted/rejected与最终事件明确分离 |

## Ringing 公共接口

### 事件 Envelope

```text
RingingEventEnvelope {
  schema: "deepx.Ringing"
  version: 1
  channel
  server_epoch
  seed
  channel_seq
  session_seq
  event_id
  causation_id?
  correlation_id?
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
  from_seq
  to_seq
  events
}

RingingChannelSnapshot {
  channel
  seed
  baseline_seq
  snapshot_version
  state
}
```

Snapshot 必须表达领域状态，禁止使用事件数组模拟状态。

## 三连接模型

新客户端建立三条独立 WebSocket，均可复用现有 `/control/v1` 地址，通过 Hello 的 `connection_role` 区分：

```text
control
conversation
tool
```

连接流程：

1. Control 首先连接并获得随机 `connection_group_id`。
2. Conversation/Tool 使用相同 `client_instance_id` 和 group id加入。
3. Control持有session lease，负责attach/detach和连接组生命周期。
4. Conversation/Tool heartbeat只检测连接，不续租。
5. Conversation/Tool可发送各自的Ringing命令，但服务端必须验证同组Control仍持有目标session lease。
6. Conversation/Tool断开时独立恢复。
7. Control断开时立即注销连接组、关闭数据连接，并重建三条连接。

旧客户端只建立原Control连接，不会收到Ringing消息。

Control protocol在双协议期保持版本1，仅增加可选Hello字段。服务端只有在能力协商成功后才允许发送Ringing frame。

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
2. 服务端先订阅live boundary，再生成Ringing snapshot，并缓冲boundary后的事件。
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

## 迁移阶段

### 1. Ringing基础设施

- 新增DomainCommand、DomainEvent和Ringing协议类型。
- agent worker输入输出支持显式判别：
  - legacy记录保持原格式。
  - 新记录使用`wire: "Ringing_domain_v1"`。
- worker reader必须先检查`wire`，禁止使用untagged猜测。
- daemon建立三个独立ChannelBus、journal、snapshot projection和发送队列。
- legacy EventBus继续存在，但只能接收DomainEvent经过LegacyProjector生成的事件，或尚未迁移的原始legacy事件。
- 增加连接组、三连接握手、per-session/channel cursor和两阶段切流。
- 自动生成Rust→TypeScript bindings，并由CI检查漂移。
- 初始默认全部legacy，Ringing只运行协议和连接测试。

### 2. 第一优先级：Tool Event

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
- Desktop建立Tool store；TUI直接处理RingingToolEvent。
- ToolSnapshot直接从MessageStore和当前工具运行状态构建。
- Tool切为Ringing后，legacy `RoundComplete.tool_calls`不再拥有工具卡渲染权。

### 3. 第二优先级：Conversation Event

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
- provider HTTP失败只生成一个可靠TurnFailed。
- message/reasoning/compact delta按帧合并。
- terminal不得排在未受限delta backlog之后。
- ConversationSnapshot直接从持久化session消息构建。
- v2会话不再使用Agent2Ui replay签名去重，也不通过错误字符串触发resume。

### 4. 第三优先级：Control Event

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

### 5. Ringing Command迁移

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

## 迁移错误热点

- agent stdout/stdin当前分别严格解析Agent2Ui和Ui2Agent。
- activity tracker当前通过序列化后的legacy `type`判断生命周期。
- EventBus直接存储ControlServerMessage和Agent2Ui projection。
- persisted session snapshot直接创建SessionRestored。
- 单outbound queue隐含全局到达顺序。
- Electron当前拆散EventBatch并逐事件IPC。
- frontend replay、local snapshot和reducer假设只有一个全局流。
- resume同时依赖RPC、SessionRestored和session.replay_events，容易产生重复baseline。
- TUI对未知ControlServerMessage静默忽略。
- TypeScript协议bindings当前依赖人工复制。
- 命令重试没有统一command id，accepted和completed边界不清晰。
- 当前compact/token calibration改动属于现有用户工作，实施时必须保留。

## 发布顺序

发布周期1：

- Ringing基础设施。
- Tool Event默认Ringing。
- Conversation、Control Event和全部Command保持legacy。
- Desktop和TUI均支持按session切流。

发布周期2：

- Conversation Event和Control Event默认Ringing。
- 原Ui2Agent命令逐频道切换为Ringing Command。
- legacy仍保留显式诊断回滚开关。

两个兼容周期结束并满足验收门槛后：

- 将Control protocol升至2。
- 删除Agent2Ui和Ui2Agent。
- 删除LegacyProjector、legacy ingress、旧EventBus projection、旧replay buffer、旧前端reducer和legacy TS bindings。
- 删除对应字符串session/interaction RPC兼容入口。
- 保留不属于Agent命令域的daemon查询RPC。
- 全仓搜索要求Agent2Ui/Ui2Agent生产引用为零。

## 测试与验收

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
- 三连接：
  - 数据连接不能独立获得lease。
  - Control断开销毁连接组。
  - Tool/Conversation断开只恢复自身。
- 压力测试：
  - 10 MiB exec输出时内存保持有界。
  - renderer每session/channel每帧最多commit一次。
  - Tool洪峰期间Control RPC/heartbeat本地p95低于250 ms。
  - compact成功或HTTP失败后1秒内显示唯一终态。
  - 后端工具完成后前端不存在分钟级渲染积压。
- UI：
  - 每session/channel/direction只有一个权威协议。
  - 单个error id最多生成一个Toast。
  - snapshot与live并发不重复应用terminal。
- 最终验证：
  - 协议单测。
  - worker输入输出边界测试。
  - daemon三连接WebSocket集成测试。
  - Desktop/TUI focused tests。
  - TypeScript typecheck。
  - Rust affected crates check。
  - 高吞吐端到端压力测试。

## 默认假设

- Ringing是整个新双向协议的正式名称，代码、能力名、日志和文档统一使用该拼写。
- daemon和agent worker来自同一可执行文件，不支持新worker与旧daemon内部混搭。
- 已切换的Ringing频道不会自动退回legacy，故障必须在Ringing恢复路径中解决。
- 迁移诊断只记录频道、类型、长度、序号、耗时、command id和丢弃字节数，不记录消息正文、工具参数、provider响应或凭据。
