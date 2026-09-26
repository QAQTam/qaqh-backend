# Team Task Board Canonical Aggregate

> 日期：2026-09-26
> 基线：`ab42663`（`main`）
> 状态：accepted；TEAM-01a/b/c/d（types + reducer + durable store + runtime tools + wire）已实现；TEAM-01e TUI 待接
> 上游：`2026-09-25-subagent-v2-rewrite-spec.md` Phase 5 / TEAM-01
> 范围：`qaqh-session` team aggregate；本 spec 不覆盖 TUI/WinUI 渲染

## 1. 定位

Task board 是 root session tree 的共享工作图，不是单个 session 的 transcript，
也不进入任何 session 的 `events.jsonl`。

```text
root session tree
  -> team aggregate（独立 canonical log）
       -> tasks / claims / dependencies / artifacts / acceptance
  -> session canonical logs（保持原样）
       -> messages 通过 task_id 关联 team aggregate
```

冻结规则：

- task board facts 只写 team aggregate，不写 session canonical log；
- session canonical log 不因 task board 变化而重写或 compact；
- `InterAgentCommunication.task_id` 继续是可选字符串，值为 `TaskId`；
- team aggregate 缺失或损坏时，task 工具 fail closed，不影响普通会话启动。

## 2. 存储布局

```text
{data_dir}/team/{root_session_id}/
  team.json          team identity（team_id、log_id、created_at_ms）
  events.jsonl       TeamFact append-only JSONL
  events.commit      committed_fact_seq + committed_offset
  events.lock        OS file lock（std::fs::File::lock）
```

`team_id` = root session id。`log_id` 独立生成 UUIDv7，不与会话 `LogId` 合并。

### 2.1 Durability contract

- 每次 append：持 `events.lock` → 校验 identity → 分配 `fact_seq` → 写
  `events.jsonl` + `flush` + `sync_all` → 原子写 `events.commit` → 更新内存投影。
- 启动/重放只信任 `events.commit` 的 committed prefix；`events.jsonl` 尾部
  超出 committed offset 的 torn bytes 会被截断。
- `events.commit` 缺失时视为空 log；`events.jsonl` 缺失但 commit 非空时
  fail closed（`CommitMismatch`）。
- v1 用 OS file lock 串行化单机写入；多 daemon writer fencing 留到需要时，
  不复用 session log 的 writer fence 文件。

## 3. 类型

### 3.1 身份

```rust
TeamId(SessionId)
TaskId("task_" + ULID)
```

`TaskId` 必须通过 schema 校验；禁止正文 regex 推断 task id。

### 3.2 Actor

```rust
TeamActor {
  agent_path: AgentPath,
  session_id: Option<SessionId>,
}
```

`agent_path` 必须是 `/root` namespace 下的绝对路径。

### 3.3 TeamFact

```rust
TeamFact {
  schema: FactSchema,          // qaqh.team-fact/v1
  team_id: TeamId,
  log_id: LogId,
  fact_seq: u64,               // store 分配
  event_id: EventId,
  ts_ms: i64,
  causation_id: Option<EventId>,
  actor: TeamActor,
  payload: TeamPayload,
}
```

### 3.4 Payload

| payload | 关键字段 | 语义 |
|---|---|---|
| `TeamCreated` | `root_session_id`, `created_at_ms` | team identity 首次建立 |
| `TaskCreated` | `task_id`, `title`, `description_ref`, `created_by` | 创建 task，初始 `Open` |
| `TaskClaimed` | `task_id`, `owner`, `claim_epoch` | CAS claim，只允许 `Open` |
| `TaskReleased` | `task_id`, `owner`, `claim_epoch`, `reason` | 释放 claim，回到 `Open` |
| `TaskDependencyAdded` | `task_id`, `depends_on` | 添加依赖边，拒绝自环/重复/成环 |
| `TaskArtifactAttached` | `task_id`, `artifact_ref`, `media_type` | 追加 artifact |
| `TaskAcceptanceSet` | `task_id`, `acceptance` | 替换 acceptance 列表 |
| `TaskCompleted` | `task_id`, `owner`, `claim_epoch`, `result_ref` | 完成；依赖必须已终态 |
| `TaskClosed` | `task_id`, `closed_by` | 关闭 `Completed` task |
| `TaskCancelled` | `task_id`, `cancelled_by`, `reason` | 取消未关闭 task |

`claim_epoch` 从 1 开始；每次 claim 必须由 store 依据当前状态分配/校验，
stale owner/epoch 稳定拒绝。

## 4. 状态机

```text
Open --claim--> Claimed --release--> Open
Open --cancel--> Cancelled
Claimed --complete--> Completed --close--> Closed
Claimed --cancel--> Cancelled
Completed --cancel--> Cancelled
```

不变量：

- `Closed` / `Cancelled` 是终态，不能再 claim / complete / 加依赖 / 加 artifact；
- claim 只允许 `Open`；重复 claim 不是幂等成功，而是稳定拒绝；
- complete 必须匹配当前 owner + claim_epoch，且所有 `depends_on` 都是
  `Completed` 或 `Closed`；
- dependency cycle 在 append 前拒绝；
- 同一 task 的 `TaskCreated` 重复 id 拒绝；
- 所有时间戳必须为正。

## 5. Projection

```rust
TaskBoardSnapshot {
  team_id: Option<TeamId>,
  revision: u64,
  last_fact_seq: u64,
  tasks: Vec<TaskView>,
}

TaskView {
  task_id, title, description_ref,
  state, owner, claim_epoch,
  depends_on, artifacts, acceptance,
  result_ref, created_by, created_at_ms, updated_at_ms,
}
```

Delta 至少覆盖 create / claim / release / dependency / artifact / acceptance /
complete / close / cancel，携带 `revision` 与 `task_id`。

Projection 是**可重建**索引，不写第二个 canonical 文件。daemon 启动时从
`events.jsonl` 的 committed prefix 重建；team log 损坏时 task 面 fail closed。

## 6. 与 mailbox 的关系

- `InterAgentCommunication.task_id` 指向 `TaskId`；
- mailbox projection 不解析 task 状态，只保留绑定；
- task board 不把 message 正文复制进 team aggregate，只存 artifact/content_ref；
- 大正文仍走 durable content store，team fact 只携带 `ContentRef`。

## 7. 配额与权限

- 只有同一 root tree 的 agent 可以写该 team aggregate；
- `TaskClaimed` 的 owner 必须来自同 tree 的 canonical agent metadata；
- 创建/claim/complete 受 Phase 3 outbound/in-flight 之外独立的 task 数量上限
  （实现时给默认值，超限稳定拒绝）；
- 不允许跨 root tree 读写 task board。

## 7.1 验收矩阵

| id | 场景 | 期望 |
|---|---|---|
| TEAM-T1 | create/claim/release | 状态与 epoch 单调，release 后回到 Open |
| TEAM-T2 | stale claim / stale complete | owner/epoch 不匹配稳定拒绝 |
| TEAM-T3 | dependency gate | 依赖未终态时 complete 拒绝；成环依赖拒绝 |
| TEAM-T4 | artifact / acceptance | 追加 artifact、替换 acceptance，终态后拒绝 |
| TEAM-T5 | close / cancel | 终态不可再变更；Closed 只能从 Completed 进入 |
| TEAM-T6 | append/replay | 重放后 snapshot 与在线 projection 逐字段一致 |
| TEAM-T7 | torn tail | `events.jsonl` 尾部未 commit 的字节被截断，已 commit 事实不丢 |

## 8. 实施切片

1. **TEAM-01a**：team types + validation + reducer（本切片）。
2. **TEAM-01b**：TeamStore append/replay/commit。
3. **TEAM-01c**：runtime task tools（`task_create` / `task_claim` /
   `task_update` / `task_close`）。
4. **TEAM-01d**：daemon projection wire + TeamSnapshot/TeamDelta 扩展。
5. **TEAM-01e**：TUI/WinUI 消费（由前端团队接入）。

本切片（TEAM-01a/b）只交付 canonical types、reducer 与 durable store，
不暴露 runtime 工具面。
