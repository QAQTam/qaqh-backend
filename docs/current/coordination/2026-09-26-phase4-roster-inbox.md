# Phase 4 roster / inbox 前端接线协调（2026-09-26）

> 范围：TUI（`qaqh-tui-app`）/ WinUI / `qaqh-client`
> 上游权威：`docs/current/architecture.md`（**`TeamSnapshot/TeamDelta` 是 TUI/WinUI 的唯一 roster/inbox 投影**）、
> `spec/2026-09-25-subagent-v2-rewrite-spec.md` §5.2
> 后端冻结验证：`106493c`；本协调单落地于其后
> 相关清单：`status.md` §7（alpha3 检查点）

---

## 1. 结论

后端 Phase 0-7（含 Team projection、task board、message board、steer/interject）
**已完成**。Phase 4 的唯一阻塞点是：`qaqh-client` 没有导出 Team projection 的
typed 面 —— 壳层不能依赖 `qaqh-session`（静态门禁 G1），而 match 枚举变体必须
写出枚举名，所以**没有导出类型就无法消费 `TeamDelta`**。

本协调单已经把这个面补上（`Client::team_v2` + 全套类型别名 + 契约锁测试），
TUI / WinUI 现在可以开工，**没有剩余后端阻塞**。

---

## 2. 契约（已冻结，前端不得绕过）

| 用途 | 接口 |
|---|---|
| 快照 | `GET /ringing/v2/sessions/{seed}/team` → `ClientV2TeamResponse` |
| 增量 | per-seed 单流 `ClientV2Payload::TeamDelta(ClientV2TeamDelta)` |
| 客户端入口 | `Client::team_v2(seed) -> Result<ClientV2TeamResponse>`（本次新增） |

`ClientV2TeamResponse { schema, seed, team, tasks, board }`：

- `team: ClientV2TeamSnapshot` = `TeamSnapshot`
  - `agents: Vec<ClientV2TeamAgentSnapshot>` ← **roster**
  - `unread_messages: Vec<ClientV2TeamInboxSummary>` ← **inbox**
  - `root_session_id` / `revision` / `last_fact_seq`
- `tasks: ClientV2TaskBoardSnapshot`（TEAM-01e）
- `board: ClientV2TeamBoardSnapshot`（BOARD-01d）

字段形状：

```text
ClientV2TeamAgentSnapshot
  agent_id / agent_path / nickname? / role?
  status: pending_init|running|waiting_user|interrupted|completed|errored|shutdown|not_found
  residency: loaded|unloaded
  parent_agent_path? / current_task_id?

ClientV2TeamInboxSummary
  message_id / author(AgentPath) / recipient(AgentPath) / task_id? / delivery / created_at_ms
  delivery: queue|trigger|interrupt|steer|interject

ClientV2TeamDelta（9 分支）
  AgentJoined / AgentStatusChanged / AgentResidencyChanged /
  AgentMessageQueued / AgentMessageDelivered / AgentInterrupted / AgentCompleted /
  TaskChanged / BoardChanged
```

全部类型从 `qaqh_client` 根导出；壳层**不得**为了这些类型引入 `qaqh-session`。

---

## 3. 不变量（前端必须遵守）

1. **roster 以 `AgentPath` 为主、nickname 为辅**；nickname 只是显示名，不能当键。
2. **`unloaded != completed != closed`**：`residency` 与 `status` 是两个正交字段，
   unloaded agent 必须仍可见，**不得画成 deleted**。
3. **不再从 `spawn_subagent` 工具卡 JSON 推导身份**。TUI 现在的
   `SessionState.subagents` 正是这条旧路径，接线时要删掉。
4. `control.subagents` bootstrap 必须被消费（它不是 roster 权威，但不能忽略）；
   `SubagentSpawned` 早于工具卡到达也要能建条目，`SubagentFinished` 在 timeline
   丢失时也要能收敛状态。
5. **先拉快照、再应用 delta**：`TeamDelta` 是 ephemeral，**不保证补发**；
   只靠 delta 拼状态会永久缺条目。
6. residency 是 daemon-local runtime overlay，**不写 canonical fact**；
   daemon 重启后自然回到 unloaded，前端不得把它持久化或当 status 用。

---

## 4. 工作拆分

### 后端（本次已完成）

- [x] `Client::team_v2(seed)` 与 `ClientV2TeamResponse`
- [x] Team / task / board 类型从 `qaqh_client` 根导出
- [x] 契约锁测试
      `team_response_matches_daemon_wire_shape`（wire 形状逐字段对齐 daemon）
      `team_projection_is_consumable_from_the_client_root`（壳层能命名并 match 全部分支）

### TUI（`qaqh-tui-app`）— Phase 4

- [ ] team 状态：snapshot + delta 归并，按 `agent_path` 索引
- [ ] 打开/切换会话时拉 `team_v2`；在 per-seed 流上应用 `TeamDelta`
      （当前 `src/app/mod.rs` 明确忽略 `TeamDelta(_)` / `MailboxDelta(_)`）
- [ ] `/subagents` roster：path prefix 过滤、unloaded 仍可见、status + role 展示
- [ ] inbox：author / recipient / task / delivery 四要素
- [ ] 点击 agent 打开 child transcript
- [ ] `@` 自动补全来自 roster
- [ ] 删除从 `spawn_subagent` 工具卡推导身份的旧路径

验收：单测覆盖 roster 归并与 `unloaded != deleted`；PTY 覆盖
「spawn 后 roster 出现 → unload 后条目仍在且标 unloaded」。

### WinUI — Phase 4

- [ ] 与 TUI 消费同一份 wire 契约（自己的 transport，不复制后端逻辑）
- [ ] roster / inbox / child transcript

验收：与 TUI 对同一 seed 得到一致的 roster / inbox。

### 后续（不属于 Phase 4）

- TEAM-01e：task board 渲染 + `@` mention 结构化接入
- BOARD-01d：message board 渲染 + unread 水位
- SUBV2-11d：steer / interject / interrupt 区分展示

---

## 5. 风险

| 风险 | 影响 | 缓解 |
|---|---|---|
| delta 不补发 | 漏拉快照 → 永久缺条目 | 契约要求"先快照后 delta"；把这条写进 TUI/WinUI 单测 |
| 把 residency 当 status | unloaded 被画成 completed/deleted | 两个正交字段 + `unloaded != completed != closed` 断言 |
| 继续读工具卡 JSON | 与 canonical edge 漂移，正是 V2 要删的旧路径 | 接线时删除旧推导；roster 只来自 Team projection |
| 壳层为了类型引入 `qaqh-session` | 违反静态门禁 G1 | 只从 `qaqh_client` 根取类型（已有 `v2_public_api.rs` 锁） |
