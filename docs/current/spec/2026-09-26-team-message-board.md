# Team Message Board Canonical Aggregate

> 日期：2026-09-26
> 基线：`0151249`（`main`）
> 状态：accepted；BOARD-01a/b/c backend 已完成；BOARD-01d TUI/WinUI 待接
> 上游：`2026-09-25-subagent-v2-rewrite-spec.md` Phase 6 / BOARD-01
> 范围：`qaqh-session` board aggregate、`qaqh-runtime` host/wire；本 spec 不覆盖
> TUI/WinUI 渲染

## 1. 定位

Message board 是 root session tree 内的持久协作记录，不是 session transcript，
也不是 task board 的第二份状态。

```text
root session tree
  -> task aggregate（task.jsonl）
       -> task / claim / dependency / artifact
  -> board aggregate（board/events.jsonl）
       -> channel / thread / post / subscription
  -> session canonical logs
       -> InterAgentCommunication（仅通知副本，不是 post 真源）
```

冻结规则：

- board facts 只写 board aggregate，不写 session canonical log；
- task board 与 board aggregate 各自拥有 identity、commit marker、lock；
- `PostCreated` 是 post 的唯一持久化真源；
- notification 不是 post 的一部分，失败、丢失、重启都不会回滚 post；
- board 缺失或损坏时 board 工具 fail closed，不影响普通会话和 task board；
- 默认不跨 root tree 访问；`task_id` 只能关联同一 root tree 的 task。

## 2. 存储布局

```text
{data_dir}/team/{root_session_id}/board/
  board.json         board identity（board_id、log_id、created_at_ms）
  events.jsonl       BoardFact append-only JSONL
  events.commit      committed_fact_seq + committed_offset
  events.lock        OS file lock（std::fs::File::lock）
```

`board_id` = root session id。`log_id` 独立生成 UUIDv7，不与 session `LogId`
或 task board `LogId` 合并。

### 2.1 Durability contract

- 每次 append：持 `events.lock` → 校验 identity → 分配 `fact_seq` → 写
  `events.jsonl` + `flush` + `sync_all` → 原子写 `events.commit` → 更新内存投影。
- 启动/重放只信任 `events.commit` 的 committed prefix；`events.jsonl` 尾部
  超出 committed offset 的 torn bytes 会被截断。
- `events.commit` 缺失时视为空 log；`events.jsonl` 缺失但 commit 非空时
  fail closed（`CommitMismatch`）。
- v1 用 OS file lock 串行化单机写入；多 daemon writer fencing 留到需要时。

## 3. 类型

### 3.1 身份

```rust
BoardId(SessionId)
ChannelId("chan_" + ULID)
ThreadId("thread_" + ULID)
PostId("post_" + ULID)
```

所有 ID 必须通过 schema 校验；禁止从正文 regex 推断 ID。

### 3.2 Actor

复用 task board 的 `TeamActor`：

```rust
TeamActor {
  agent_path: AgentPath,
  session_id: Option<SessionId>,
}
```

`agent_path` 必须是 `/root` namespace 下的绝对路径。runtime 生产路径必须填充
`session_id`；replay 兼容允许旧事实缺省。

### 3.3 BoardFact

```rust
BoardFact {
  schema: FactSchema,          // qaqh.board-fact/v1
  board_id: BoardId,
  log_id: LogId,
  fact_seq: u64,               // store 分配
  event_id: EventId,
  ts_ms: i64,
  causation_id: Option<EventId>,
  actor: TeamActor,
  payload: BoardPayload,
}
```

### 3.4 Payload

| payload | 关键字段 | 语义 |
|---|---|---|
| `BoardCreated` | `root_session_id`, `created_at_ms` | board identity 首次建立 |
| `ChannelCreated` | `channel_id`, `name`, `topic?`, `created_by` | 创建 channel |
| `ThreadCreated` | `thread_id`, `channel_id`, `title`, `task_id?`, `created_by` | 创建 thread |
| `PostCreated` | `post_id`, `thread_id`, `task_id?`, `author`, `body`, `reply_to?` | 持久化 post |
| `SubscriptionChanged` | `target`, `subscriber`, `subscribed`, `updated_at_ms` | 设置订阅状态 |

`SubscriptionTarget`：

```rust
Channel { channel_id }
Thread { thread_id }
```

`body` 是 bounded inline UTF-8 文本。v1 不实现 board post 的 content-ref 正文；
单条 post 上限 16 KiB，避免 board log 变成大内容通道。

## 4. 状态与校验

### 4.1 Channel

- channel name 必须匹配 `[a-z0-9][a-z0-9_-]{0,63}`；
- channel name 在 root tree 内唯一；
- `topic` 可选，最大 1024 bytes；
- channel 删除/归档不在 v1 范围。

### 4.2 Thread

- thread 必须引用已存在 channel；
- title 非空，最大 512 bytes；
- `task_id` 可选；提供时必须通过 `TaskId` schema，并由 runtime 校验该 task
  属于同一 root tree；
- thread 不可移动 channel；
- thread 删除/归档不在 v1 范围。

### 4.3 Post

- post 必须引用已存在 thread；
- body 非空，最大 16 KiB；
- `task_id` 可选；若 thread 已绑定 task，post 的 `task_id` 必须为空或等于
  thread task；runtime 在 append 前解析为 thread task；
- `reply_to` 可选，必须引用同一 thread 内的既有 post；
- post 只追加，不编辑、不删除；
- post 接受后即 durable，即使后续 notification 全部失败。

### 4.4 Subscription

- target 必须存在；
- 同一 `(target, subscriber.agent_path)` 使用 last-write-wins；
- 重复设置相同状态幂等成功；
- subscription 只影响 notification，不影响 post 可见性。

## 5. Projection

```rust
BoardSnapshot {
  board_id: Option<BoardId>,
  revision: u64,
  last_fact_seq: u64,
  channels: Vec<BoardChannelView>,
  threads: Vec<BoardThreadView>,
  posts: Vec<BoardPostView>,
  subscriptions: Vec<BoardSubscriptionView>,
}
```

Delta 至少覆盖 board / channel / thread / post / subscription 五类变化，并携带
`revision`。Projection 是可重建索引，不是第二个 canonical 真源。

## 6. Runtime 工具

BOARD-01 提供以下工具：

| 工具 | 语义 |
|---|---|
| `board_channel_create` | 创建 channel |
| `board_thread_create` | 在 channel 内创建 thread |
| `board_post` | 追加 post，并尝试通知 running subscriber |
| `board_subscribe` | subscribe / unsubscribe channel 或 thread |
| `board_list` | 按 channel/thread 查询 board projection |

工具只接受结构化 `channel_id` / `thread_id` / `task_id`，不得从正文推断。

## 7. Notification contract

`PostCreated` 后按以下顺序处理：

1. 计算 thread subscribers 与 channel subscribers 的并集；
2. 去掉 post author；
3. 只保留当前 `status = running` 且 `residency = loaded` 的 agent；
4. 通过 queue-only `InterAgentCommunication` 投递通知；
5. notification 不触发 idle/unloaded agent，不改变 post 的 durable 结果。

冻结语义：

- missed notification 不保证补发；
- 客户端 unread 状态由 board snapshot + 本地已读水位计算；
- notification 失败只记录在工具输出/日志，post 仍然成功；
- 同一 subscriber 同时订阅 channel 和 thread 时只通知一次；
- notification body 必须包含 `post_id`、`thread_id`、`task_id?` 和 post body。

## 8. Wire

`GET /ringing/v2/sessions/{seed}/team` 在现有响应上增加 `board` 字段：

```json
{
  "schema": "qaqh.ringing.team/v1",
  "seed": "...",
  "team": {},
  "tasks": {},
  "board": {
    "board_id": "...",
    "revision": 4,
    "last_fact_seq": 4,
    "channels": [],
    "threads": [],
    "posts": [],
    "subscriptions": []
  }
}
```

board 变化通过 per-seed 单流的 ephemeral `TeamDelta::BoardChanged` 发布完整
snapshot。board 是独立 canonical aggregate，因此该 delta 不推进 session
`last_fact_seq`；断线/漏发后客户端重新读取 `/team` 即可恢复。

## 9. 上限

v1 上限用于 fail closed，不用于正常容量规划：

| 资源 | 上限 |
|---|---:|
| channels / root tree | 64 |
| threads / root tree | 512 |
| posts / root tree | 4096 |
| subscriptions / root tree | 2048 |
| channel name | 64 bytes |
| channel topic | 1024 bytes |
| thread title | 512 bytes |
| post body | 16384 bytes |

超过上限必须稳定拒绝，不能静默截断或丢弃。

## 10. 验收矩阵

| ID | 场景 | 必须成立 |
|---|---|---|
| BOARD-A1 | replay | channel/thread/post/subscription 可重建且与在线投影一致 |
| BOARD-A2 | torn tail | 未提交尾部截断，不影响 committed prefix |
| BOARD-A3 | validation | 非法 ID、重复 channel、未知引用、reply 跨 thread 稳定拒绝 |
| BOARD-A4 | task linkage | 提供 `task_id` 时只能关联同一 root tree 的 task |
| BOARD-A5 | notification | 只通知 running + loaded subscriber；idle/unloaded 不启动 |
| BOARD-A6 | best effort | notification 失败不影响 post durable 成功 |
| BOARD-A7 | wire | `/team` 返回 board snapshot；单流发布 `BoardChanged` |
| BOARD-A8 | limits | 超出 board 资源上限稳定拒绝 |

## 11. 实施切片

1. BOARD-01a：spec + `qaqh-session::team::board` types / reducer / store；
2. BOARD-01b：`BoardHost` + runtime tools；
3. BOARD-01c：`/team` board snapshot + `TeamDelta::BoardChanged`；
4. BOARD-01d：TUI/WinUI board 渲染与 unread 水位。
