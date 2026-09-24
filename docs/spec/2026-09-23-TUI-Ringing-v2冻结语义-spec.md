# TUI Ringing v2 冻结语义

日期：2026-09-23  
状态：**FROZEN（后端实现前冻结）**  
适用：TUI beta、Windows alpha、后续桌面壳层  
Wire 标识：`schema = qaqh.Ringing`，`version = 2`  
锚点 tag：`tui-ringing-v2-frozen-2026-09-23`

> 本文件冻结的是 **Ringing v2 的语义与 wire 契约**，不是 P5 的全部实现细节。
> 后端实现必须与本文件一致；TUI 只通过 `qaqh-client` 消费，不直接依赖
> `qaqh-ringing`、`qaqh-domain` 或 `qaqh-session`。
>
> 任何会改变 cursor、reset、replay、interaction 或 driver 语义的修改，都必须
> 提升冻结 tag，并给出 v1/v2 兼容矩阵和 TUI 迁移说明；不得在本 tag 指向的
> 语义上原地改写。

## 0. 裁决

1. **v2 是 beta 起的权威重连协议。** v1 继续保留，但只作为 2.0 兼容面。
2. **canonical fact log 是唯一权威源。** 三频道是 wire 过滤视图，不是事实源。
3. **cursor 是 canonical cursor。** v1 `stream_seq` 不是 v2 cursor，不能直接复用。
4. **replaceable 不推进 cursor，ephemeral 不回放。**
5. **snapshot baseline 与 subscribe 必须在服务端 actor 串行点原子化。**
6. **pending interaction 以 `interaction_id` 重放，first-answer-wins。**
7. **driver 是显式能力，不是“谁先连上谁就是 driver”。**
8. **TUI 不读取服务端 journal/checkpoint/offload/messages 布局。**
9. **Windows alpha 与 Linux/TUI 共用同一 v2 语义，不按平台分叉 cursor 或 reset 行为。**

## 1. 端点面

v2 使用 `/ringing/v2` 前缀。核心端点如下：

| 用途 | 方法 | 路径 |
|---|---|---|
| open / 版本协商 | `POST` | `/ringing/v2/clients/open` |
| lease 续期 | `POST` | `/ringing/v2/leases/renew` |
| 三频道权威快照 | `GET` | `/ringing/v2/sessions/{seed}/bootstrap` |
| 频道 SSE | `GET` | `/ringing/v2/sessions/{seed}/events/{channel}` |
| timeline HTTP 页 | `GET` | `/ringing/v2/sessions/{seed}/timeline` |
| timeline SSE | `GET` | `/ringing/v2/sessions/{seed}/timeline/events` |
| 命令提交 | `POST` | `/ringing/v2/commands/{channel}` |
| 命令状态查询 | `GET` | `/ringing/v2/commands/{command_id}` |
| typed service RPC | `POST` | `/ringing/v2/service/{method}` |
| content 读取 | `GET` | `/ringing/v2/content/{content_id}` |
| content 上传 | `POST` | `/ringing/v2/content` |
| driver claim | `POST` | `/ringing/v2/sessions/{seed}/driver/claim` |
| driver release | `POST` | `/ringing/v2/sessions/{seed}/driver/release` |

`channel ∈ {control, conversation, tool}`。v2 的 SSE 是 **per-seed** 订阅；
v1 的单频道多 seed 行为不继承到 v2。

## 2. 版本协商

### 2.1 open

请求：

```json
{
  "schema": "qaqh.Ringing",
  "version": 2,
  "client_instance_id": "uuid"
}
```

响应：

```json
{
  "schema": "qaqh.Ringing",
  "version": 2,
  "accepted": true,
  "client_session_id": "cs-uuid",
  "server_epoch": "epoch-...",
  "lease_ttl_ms": 30000,
  "renew_interval_ms": 10000,
  "capabilities": {
    "subscribe": true,
    "interact": true,
    "drive": true,
    "timeline": true,
    "service": true,
    "content": true
  }
}
```

规则：

- `client_instance_id` 是客户端进程/实例身份。
- `client_session_id` 是 daemon 签发的连接身份；driver、lease 和命令都以它为准。
- `accepted=false` 时不得继续发命令。
- 版本不兼容返回 HTTP `426`，稳定 code 为 `unsupported_version`。
- 未知 capability 字段必须忽略，不得推测语义。
- v2 client 不得把 v1 open 响应当作 v2 capability 证据。

### 2.2 兼容窗口

- 2.0 beta / rc / stable：v1 与 v2 端点并存。
- v1 端点服务端做一发布周期映射；TUI beta 默认走 v2。
- v1 映射层最早在 2.1 移除；移除前必须发布兼容矩阵和回滚说明。
- v1 的 `Last-Event-ID = epoch:channel:stream_seq` 只由服务端映射，
  TUI 不得自行把 `stream_seq` 当 canonical cursor。

## 3. Canonical cursor

### 3.1 逻辑结构

```json
{
  "log_id": "log-...",
  "fact_seq": 42,
  "projection_index": 1
}
```

- `fact_seq` 在单个 canonical log 内单调递增。
- `projection_index` 区分同一个 fact 产生的多个 reliable projection。
- `projection_index = 65535` 是 snapshot 的 `END_OF_FACT` 哨兵，不是普通事件 cursor。
- 不同 `log_id` 的 cursor 不可比较。
- reliable 事件排序键严格为 `(fact_seq, projection_index)` 字典序。

### 3.2 wire 表示

- SSE 查询参数 `since_cursor` 使用 **opaque token**，由服务端签发。
- token 的规范化编码为：

```text
v2.<base64url(no-padding) of compact JSON cursor>
```

- snapshot 使用 `snapshot_cursor` 返回同一类 token。
- `ResetRequired` 使用 `snapshot_cursor` 返回同一类 token。
- 客户端必须原样保存和回传 token，不得自行拼接、截断或跨 `log_id` 复用。
- 事件 body 同时携带可诊断的 `log_id`、`fact_seq`、`projection_index`，
  但 cursor 推进只认服务端签发的 `cursor` token。

### 3.3 推进规则

只有满足以下全部条件时，客户端才推进 cursor：

1. 事件 `delivery == "reliable"`；
2. `log_id` 与当前 SessionModel 的 `log_id` 相同；
3. `(fact_seq, projection_index)` 严格大于当前 cursor；
4. 事件 payload 已成功进入 reducer；
5. 事件不属于旧 `server_epoch` 或旧 `log_id`。

replaceable 事件不推进 cursor。ephemeral 事件不推进 cursor。

## 4. Snapshot 与原子订阅

### 4.1 bootstrap

`GET /ringing/v2/sessions/{seed}/bootstrap` 返回：

```json
{
  "schema": "qaqh.Ringing",
  "version": 2,
  "server_epoch": "epoch-...",
  "seed": "s1",
  "snapshot_cursor": "v2....",
  "control": {
    "channel": "control",
    "state_revision": 7,
    "snapshot_version": 1,
    "state": {}
  },
  "conversation": {
    "channel": "conversation",
    "state_revision": 19,
    "snapshot_version": 1,
    "state": {}
  },
  "tool": {
    "channel": "tool",
    "state_revision": 11,
    "snapshot_version": 1,
    "state": {}
  }
}
```

- `snapshot_cursor` 是三频道共同的 canonical baseline。
- `state` 必须是领域状态，不得用事件数组模拟状态。
- `control.state.interactions` 必须包含当前未决 interaction。
- `control.state.driver` 必须包含当前 driver holder 与 `driver_epoch`。
- snapshot 不得包含旧 `log_id` 的尾部事实。

### 4.2 原子订阅

客户端恢复顺序固定为：

```text
open
  -> bootstrap
  -> 用 snapshot_cursor 建立 v2 SSE 订阅
  -> 服务端在 actor 串行点完成 subscribe + replay
  -> reliable replay（严格大于 snapshot_cursor）
  -> live
```

服务端不变量：

- snapshot 与 subscribe 之间不能出现未覆盖窗口。
- replay 只回放 reliable。
- live 在 replay 期间缓冲，replay 结束后按 cursor 顺序交付。
- 同一 cursor 重复出现必须去重。
- 如果 snapshot 后 cursor 已过期，返回 `ResetRequired`，客户端重新 bootstrap。
- 如果无法给出 `snapshot_cursor`，返回 `ResetRequired`，`reason = "snapshot_missing"`；
  客户端进入只读/upgrade-required 状态，不得猜测历史。

### 4.3 v2 事件 envelope

v2 SSE `data` 的规范化形状：

```json
{
  "schema": "qaqh.Ringing",
  "version": 2,
  "server_epoch": "epoch-...",
  "seed": "s1",
  "event_id": "evt-...",
  "stream_key": {
    "kind": "channel",
    "data": "conversation"
  },
  "delivery": "reliable",
  "cursor": "v2....",
  "log_id": "log-...",
  "fact_seq": 42,
  "projection_index": 1,
  "revision": 19,
  "payload": {
    "kind": "conversation_delta",
    "data": {}
  }
}
```

字段规则：

| 字段 | reliable | replaceable | ephemeral |
|---|---|---|---|
| `cursor` | 必填 | `null` | `null` |
| `log_id` | 必填 | 当前 log，可空 | `null` |
| `fact_seq` | 必填 | 来源 fact，可空 | `null` |
| `projection_index` | 必填 | `null` | `null` |
| `revision` | 必填 | 必填 | `null` |
| replay | 允许 | 不逐条回放 | 不回放 |

SSE `id` 固定为 `v2:<server_epoch>:<event_id>`，仅用于诊断和去重辅助；
cursor 只认 body 中的 `cursor`。客户端不得从 SSE `id` 推导 canonical cursor。

## 5. 频道映射

`stream_key` 决定事件进入哪个频道视图：

| projection | wire channel |
|---|---|
| ConversationDelta | `conversation` |
| TimelineDelta（普通） | `conversation` |
| TimelineDelta（ToolCallDeclared / ToolFinished） | `tool` |
| ControlDelta（普通） | `control` |
| ControlDelta（ToolIntent） | `tool` |
| ResourceDelta（WorkspaceResourceChanged） | `tool` |
| ResourceDelta（其他） | `control` |
| MetaDelta | `control` |

频道只是过滤视图。客户端不得因为一个频道没收到事件就推断 canonical log
没有事实；需要完整状态时必须走 bootstrap 或 canonical cursor replay。

## 6. Delivery 语义

### 6.1 reliable

- 必须进入 reducer，并且是唯一能推进 cursor 的 delivery。
- 重连时按 canonical cursor 回放。
- 同一 `(log_id, fact_seq, projection_index)` 只允许应用一次。
- `event_id` 用于诊断和辅助去重，但 canonical 去重键是 cursor。

### 6.2 replaceable

- 连接或重基线后只发送当前值，不逐条回放历史。
- 不推进 canonical cursor。
- `revision` 必须单调；旧 revision 不得覆盖新 revision。
- 如果 replaceable 与 reliable 都能表达同一状态，reducer 必须以 revision
  和稳定 ID 去重，不得依赖到达顺序。

### 6.3 ephemeral

- 只存在于当前 live 连接。
- 不进入 snapshot，不进入 replay，不推进 cursor。
- 客户端可以丢弃；不得用 ephemeral 推导持久状态。
- 典型用途：流式进度、短生命周期 UI 提示。

## 7. ResetRequired

v2 reset 是正常状态迁移，不是错误 toast。

SSE：

```text
event: ringing.reset_required
data: {
  "schema": "qaqh.Ringing",
  "version": 2,
  "server_epoch": "epoch-...",
  "seed": "s1",
  "log_id": "log-...",
  "snapshot_cursor": "v2....",
  "reason": "cursor_expired"
}
```

冻结 reason：

```text
cursor_expired
log_id_mismatch
unknown_fact
upgrade_required
replay_overflow
v1_epoch_mismatch
cross_session
snapshot_missing
snapshot_expired
snapshot_hash_mismatch
stale_writer
content_quota_exceeded
per_connection_overflow
progress_buffer_overflow
actor_mailbox_overflow
```

客户端处理：

1. 立即停止把旧 epoch / 旧 log 的事件送入 reducer；
2. 保留旧 UI，但标记 rebaseline；
3. 重新请求 bootstrap；
4. 用新 `snapshot_cursor` 重新订阅；
5. 新 snapshot 完整验证通过后，原子替换 SessionModel；
6. 不得让旧响应、旧 SSE 帧或旧 service response 回滚新状态；
7. `snapshot_missing` 或 `upgrade_required` 时进入只读/升级提示，不得伪造历史。

## 8. Pending interaction

### 8.1 事实

- permission / ask / plan review 都由 canonical
  `InteractionRequested` / `InteractionResolved` / `InteractionExpired` 驱动。
- `interaction_id` 是唯一稳定键。
- `call_id`、`turn_id`、`kind` 必须随 interaction 保存。
- pending interaction 在 bootstrap 的 `control.state.interactions` 中恢复。
- 连接建立后，snapshot cursor 之后的 interaction 事件按 reliable 语义回放。

### 8.2 first-answer-wins

- 服务端是唯一裁决者。
- 多个客户端同时回答时，第一个有效 resolution 获胜。
- 后续回答返回稳定错误 `interaction_already_resolved`，并携带既有结果。
- 重复 `InteractionRequested` 对同一 `interaction_id` 是幂等的。
- 已 resolved/expired 的 interaction 不得重新打开。
- 命令 ACK 只代表“已接受”，不代表 interaction 已解决；解决必须等
  `causation_id = command_id` 的 reliable 事件。

### 8.3 TUI 规则

- reducer 维护 `interaction_id -> pending/resolved/expired`。
- 收到重复 request：若本地已 resolved/expired，忽略；否则按 stable ID 去重。
- 收到重复 response：忽略，不生成第二个 modal 终态。
- 重连后以 bootstrap 为准，清掉 snapshot 中不存在的 pending。
- 不得用本地时间戳决定 winner。
- `ResetRequired` 期间不得继续提交 interaction 命令。

## 9. Driver capability

### 9.1 定义

- `subscription`：连接能收到哪些 session event。
- `interact`：连接可以回答 permission / ask / plan。
- `drive`：连接可以提交输入、取消、undo、workspace/session 控制命令。
- driver 身份是 `client_session_id`，不是 `client_instance_id`。

### 9.2 bootstrap 中的 driver 状态

```json
{
  "driver": {
    "holder": "cs-...",
    "driver_epoch": 3,
    "can_claim": false
  }
}
```

- `holder = null` 表示当前无 driver。
- `driver_epoch` 单调递增；旧 epoch 的命令返回 `stale_driver_epoch`。
- `can_claim` 只是服务端建议；最终以 claim 响应为准。
- driver 变化产生 reliable `DriverChanged` ControlDelta。
- **席位真源是 canonical fact**：`DriverChanged { holder, driver_epoch,
  changed_at_ms }` 由 session actor 的 `ToolLedger` 单写者追加，bootstrap 读
  control 投影，客户端增量只认 `DriverChanged`。daemon 不持有席位状态。
- `holder` 的 lease 过期时，席位在投影里仍记录旧 holder，但 bootstrap 呈现为
  `holder = null`；下一次 claim 通过 `stale_holder` 显式接管并推进 epoch。

### 9.3 claim / release

`POST /ringing/v2/sessions/{seed}/driver/claim` 返回：

```json
{
  "accepted": true,
  "holder": null,
  "driver_epoch": 3,
  "reason": "claim_requested"
}
```

**claim 是两段式**：canonical 席位由 session actor 的 `ToolLedger` 单写者分配
epoch，daemon 无法同步拿到新值。因此：

- `accepted = true` 只表示**请求已转发**；`holder` / `driver_epoch` 是**请求时**
  的服务端视图，不是变更后的值。
- 变更后的权威席位经 reliable `DriverChanged`（或重新 bootstrap）到达。
- daemon 能同步裁决的分支仍然是权威的：
  - `already_holder`：调用方已持有席位（epoch 为当前值）；
  - `driver_busy`：另一 lease 持有席位；
  - `not_driver`（release）：调用方不是 holder。

`reason` 取值：

```text
claim_requested   请求已转发，等待 DriverChanged
release_requested 请求已转发，等待 DriverChanged
already_holder    daemon 同步裁决：已是 holder
driver_busy       daemon 同步裁决：他人持有
not_driver        daemon 同步裁决：release 方不是 holder
```

被占席位时 `accepted = false`；`driver_busy` / `not_driver` 保持稳定语义。
命令层的 `stale_driver_epoch` 仍由 daemon 依据 canonical 投影同步拒绝。

`POST /ringing/v2/sessions/{seed}/driver/release` 只允许当前 holder 调用。
holder 断线或 lease 过期后，服务端可以按自身策略自动移交，并发布
`DriverChanged`；TUI 不得本地推测 holder。

### 9.4 TUI 行为

- 非 driver：composer / cancel / undo / workspace 控制进入只读态。
- 非 driver 仍可订阅和回答 interaction。
- driver 状态只从 bootstrap / `DriverChanged` 更新。
- 收到 `not_driver` 时刷新 driver 状态，不得重试命令直到状态更新。
- `driver_epoch` 变化后，旧的在途控制命令不得再显示为可成功。

## 10. v1 → v2 映射

服务端兼容映射表键为：

```text
(server_epoch, channel, stream_seq)
```

值为：

```text
{
  seed,
  log_id,
  fact_seq,
  projection_index,
  delivery
}
```

规则：

| v1 情况 | v2 行为 |
|---|---|
| 命中且 seed 属于当前订阅 | 转换为 canonical cursor 后 replay |
| 命中但 seed 不属于当前订阅 | `ResetRequired { reason = cross_session }` |
| epoch 不匹配 | `ResetRequired { reason = v1_epoch_mismatch }` |
| 映射不存在或已过期 | `ResetRequired { reason = cursor_expired }` |
| replaceable v1 checkpoint | 只发送当前值，不推进 cursor |
| v1 `stream_seq` | 绝不作为 v2 `since_cursor` |

TUI beta 不得实现自己的 v1 映射；只调用 v2。v1 兼容由 daemon 负责。

## 11. TUI SessionModel 不变量

一个 seed 对应一个 `SessionModel`，至少保存：

```text
server_epoch
log_id
cursor
last_fact_seq
last_projection_index
state_revision
pending_interactions
driver_holder
driver_epoch
```

reducer 必须：

1. 拒绝旧 `server_epoch`、旧 `log_id`、旧 cursor、旧 revision；
2. reliable 严格按 `(fact_seq, projection_index)` 推进；
3. replaceable 按 revision 覆盖，不推进 cursor；
4. ephemeral 不进入持久状态；
5. bootstrap、live、service response 都进入同一个 reducer；
6. reset 时先完成新 snapshot 验证，再原子替换旧模型；
7. 保持 interaction stable ID；
8. 不把 renderer 状态、滚动位置、动画放进 wire/reducer key；
9. 不读取 journal/checkpoint/offload/messages 路径；
10. 不直接 `serde_json::from_str` 解析展示面 JSON，只消费 `qaqh-client` 类型。

## 12. Windows alpha 约束

- 使用同一 `/ringing/v2`、cursor、reset、interaction、driver 语义。
- 不引入 Windows 专用 cursor 或 reset 分支。
- 不依赖 Linux daemon 的本地路径布局。
- 共享同一 fixture 与 acceptance matrix。
- 平台差异只允许出现在进程启动、路径展示和终端能力层。

## 13. 验收矩阵

| ID | 场景 | 必须成立 |
|---|---|---|
| V2-C1 | snapshot + subscribe | 无 gap、无 dup、live replay 顺序一致 |
| V2-C2 | reliable reconnect | 只回放 cursor 之后事件，严格字典序 |
| V2-C3 | replaceable reconnect | 只收当前值，cursor 不变 |
| V2-C4 | ephemeral reconnect | 不回放，不影响持久状态 |
| V2-C5 | log_id mismatch | 返回 ResetRequired，客户端重基线 |
| V2-C6 | cursor expired | 返回 ResetRequired + snapshot_cursor |
| V2-C7 | snapshot missing | 进入只读/升级提示，不猜历史 |
| V2-R1 | permission reconnect | pending permission 按 interaction_id 恢复 |
| V2-R2 | ask reconnect | pending ask 按 interaction_id 恢复 |
| V2-R3 | plan review reconnect | pending plan 按 interaction_id 恢复 |
| V2-R4 | concurrent answers | first-answer-wins，重复回答稳定拒绝 |
| V2-D1 | driver claim | 无 holder 时可 claim，epoch 单调 |
| V2-D2 | driver busy | 活跃 holder 时稳定拒绝 |
| V2-D3 | driver handover | DriverChanged reliable，旧 epoch 命令拒绝 |
| V2-V1 | v1 cursor mapping | 命中映射成功，否则稳定 ResetRequired |
| V2-T1 | TUI static | 不直接依赖 ringing/domain/session，不读存储路径 |
| V2-T2 | TUI reset | 旧响应/旧帧不能回滚新 SessionModel |
| V2-W1 | Windows alpha | 与 Linux fixture 结果一致 |

## 14. 后端实现顺序

1. `qaqh-ringing` 增加 v2 wire 类型、cursor token 编解码和 schema 常量。
2. `qaqh-client` 增加 v2 open/subscribe/bootstrap/reset/interaction API。
3. daemon 增加 v2 端点，先复用现有 hub，再切换到 canonical projection replay。
4. v1 兼容映射表与 ResetRequired 转换。
5. 产出 v2 fixture；TUI 与 Windows alpha 共用。
6. P5 Gate 通过后，再决定 v1 兼容层的 2.1 删除时间。

## 15. 非目标

- 本文件不改变 timeline 渲染器语义。
- 本文件不决定 service response 的字段级 schema；只要求 v2 typed。
- 本文件不删除 v1 端点。
- 本文件不包含 P6 的存储清理或 composition root。
- 本文件不把 sandbox 策略带入 client/TUI。
