# Subagent V2 改进 / 重写标准计划

> 日期：2026-09-25
> 基线：`qaqh-backend@2f362e0`（`origin/main`）
> 状态：accepted
> 适用：`qaqh-subagent`、`qaqh-runtime`、`qaqh-session`、`qaqh-ringing`、`qaqh-client`、`qaqh-daemon`；消费侧包括 `qaqh-tui-app`、`qaqh-winui-app`
> 参考实现：OpenAI Codex `multi_agent_v2`（`myXCode` upstream 历史）

## 0. 一句话

Subagent V2 的标准重写方向不是“把结果注入父模型”或“反注入子模型”，而是：

> **把子代理从若干 session/UUID，升级成一棵有稳定地址、持久拓扑、独立 mailbox、可卸载/重载、可被前端消费的 agent tree。**

消息注入仍然保留，但它只负责投递；`AgentPath`、agent graph、mailbox、residency 分别负责寻址、拓扑、调度和生命周期。

---

## 1. 为什么要重写

### 1.1 当前 qaqh 的事实

当前 `spawn_subagent` 的基本模型是：

1. 父代理调用工具，daemon 创建隔离子 session/actor。
2. 父代理发送一次 task/context。
3. 后台 collector 等子代理终态。
4. 最终答案以 `<qaqh_subagent_result>` 注入父会话并触发父 turn。
5. 子代理默认 ephemeral，终态后自动关闭。
6. 父子边只存在于 `SubagentSupervisor` 内存中。
7. TUI 主要从 `spawn_subagent` 工具卡和子 timeline 推导 subagent 身份/状态。

这能工作，但只能支撑“单层 fan-out/fan-in”，不能稳定支撑：

- 多层 agent tree；
- sibling / peer 通信；
- 子代理重启后恢复身份；
- 子代理完成后保留可寻址身份；
- 前端 roster / task / inbox；
- `@` 语义；
- 持久化的权限、所有权和审计。

### 1.2 Codex 的历史给出的直接证据

Codex 的路径设计不是凭空做的，顺序很清楚：

| 时间 | 提交 | 设计动作 |
|---|---|---|
| 2026-03-19 | `70cdb1770` | 先增加 agent graph，用于 cascade close/resume，修复 `#14458` 的 depth>1 问题 |
| 2026-03-20 | `79ad7b247` | 把 multi-agent 从 UUID 改成 URI/path-like system |
| 2026-03-23 | `18f1a08bc` | 新增 `InterAgentCommunication`，显式携带 author/recipient |
| 2026-03-24 | `38c088ba8` | `list_agents` 支持 path prefix |
| 2026-03-24 | `773fbf56a` | 形成 V2 communication pattern |
| 2026-03-27 | `426f28ca9` | spawn V2 本身也改成 inter-agent communication |
| 2026-04-20 | `b528ff02b` | `/morpheus` 也纳入 path primitive |
| 2026-04-29 | `782191547` | 抽出持久化 `AgentGraphStore` |
| 2026-06-05 | `d5e4f01af` | unloaded V2 agent 可在 delivery 时 reload |
| 2026-06-08 | `4e803a017` | 引入 V2 residency LRU |
| 2026-06-?? | `8d415050f` | V2 `close_agent` 改成 `interrupt_agent` |
| 2026-08-24 | `d21794d6b` | child reload 必须经 loaded parent |
| 2026-08-24 | `2126f9361` | peer completion 回到 initiating turn |
| 2026-09-21 | `8d282f018` | message board 接入 persistent multi-agent runtime |

`79ad7b247` 的 PR 正文明确写了：

- `/root` 是用户创建的主 agent；
- 子代理是 `/root/<task_name>`，`task_name` 由模型选择；
- 任意 agent 都可以通过 path 联系任意 agent；
- path 支持绝对引用和相对引用；
- 第一版 path 暂不支持 resume，后续才通过 residency/reload 补上。

### 1.3 结论：注入模型不够，不是因为它错，而是因为它少了一层

必须区分四个层次：

```text
AgentPath / AgentId    = 我是谁、我要找谁
AgentGraph             = 谁是谁的父/子，属于哪棵树
Mailbox / Injection    = 消息何时、以什么身份进入某个模型上下文
Task Board             = 为什么要做、谁负责、做到哪
```

当前 qaqh 只有“注入 + 内存父子关系”，缺的是另外三层。

---

## 2. 重写范围

### 2.1 本 spec 覆盖

- Agent identity：`AgentId` / `AgentPath` / nickname / role。
- Agent topology：durable parent-child edge、root tree、cascade lifecycle。
- Agent communication：mailbox、定向消息、queue/trigger 语义。
- Agent lifecycle：status、interrupt、residency、unload/reload。
- Agent discovery：`list_agents`、path prefix、status snapshot。
- 前端消费契约：roster、activity、message/inbox、status。
- 与 canonical facts / Ringing v2 的接线。
- 旧 `spawn_subagent` / result injection 的迁移策略。

### 2.2 本 spec 不直接覆盖

以下内容另开 spec，但本计划定义接口依赖：

- **Agent Team Task Board**：task 图、claim、dependencies、artifact。
- **Agent Message Board**：channel/thread/post/subscription。
- **Steer / Interject**：在 queue/trigger/interrupt 稳定后再上。
- **跨 root tree 通信**：默认非目标。
- **真实模型质量评估**：本 spec 只验证协议与生命周期。

### 2.3 非目标

- 不复制 Codex 的内部实现。
- 不引入泄露的 Claude 源码。
- 不允许 actor handle 直连形成不可观测 mesh。
- 不把消息正文当任务状态。
- 不把 `AgentGraphStore` 变成第二份可写事实源。
- 不因为收到 `@` 就自动触发 turn。
- 不允许 child 默认 `TriggerTurn` root。
- 不继续用 `spawn_subagent` 工具卡 JSON 作为前端 subagent 身份源。

---

## 3. 标准术语与事实源

### 3.1 AgentId

```text
AgentId = session_id
```

要求：

- 全局稳定；
- 可持久化；
- 不随 loaded/unloaded 改变；
- 不作为模型首选寻址方式，但保留为 fallback / 审计键。

### 3.2 AgentPath

标准格式：

```text
/root
/root/<task_name>
/root/<task_name>/<subtask_name>
```

规则：

- `task_name` 只能包含 `[a-z0-9_]`；
- `root`、`.`、`..` 为保留名；
- segment 不得包含 `/`；
- absolute path 必须以 `/root` 开头；
- `/morpheus` 作为 internal agent namespace 保留；
- relative path 只能向当前节点的子树解析；
- 不使用 `..` 跨分支；
- 跨分支必须使用 absolute path。

### 3.3 AgentGraph

```text
AgentGraph {
  root_session_id
  nodes: AgentMetadata[]
  edges: ParentChildEdge[]
}

ParentChildEdge {
  parent_agent_id
  child_agent_id
  parent_call_id
  status: open | closed
  created_at
  closed_at?
}
```

要求：

- 每个 child 最多一个 parent；
- 不允许 cycle；
- edge 必须持久化；
- edge 是逻辑身份的一部分，不因 runtime unload 而消失；
- cascade close/interrupt 按 tree 执行；
- child reload 必须验证 parent ownership。

### 3.4 Canonical facts 与索引

遵循 D2：

- canonical facts 是事实源；
- graph store / SQLite / projection 都是可重建索引；
- `SubagentSpawned` / `SubagentFinished` 等 edge facts 必须由生产路径真正写入；
- graph store 不得成为绕过 canonical log 的第二真源；
- 重建失败必须 fail closed，不猜 topology。

### 3.5 AgentStatus 与 Residency 分离

```text
AgentStatus:
  pending_init
  running
  waiting_user
  interrupted
  completed
  errored
  shutdown
  not_found

Residency:
  loaded
  unloaded
```

不变量：

- `unloaded != completed`；
- `completed != closed`；
- interrupt 不等于 delete；
- unloaded agent 仍可被 list；
- delivery 可触发 reload；
- reload 必须走 parent ownership 校验。

---

## 4. 标准通信模型

### 4.1 InterAgentCommunication

```text
InterAgentCommunication {
  message_id
  root_session_id
  author: AgentPath
  recipient: AgentPath
  other_recipients: AgentPath[]
  task_id?
  content: inline | content_ref
  reply_to?
  causation_id?
  trigger_turn: bool
  created_at
}
```

要求：

- author/recipient 必须是 canonical `AgentPath`；
- `@` 只用于 UI 输入和渲染，wire 必须使用结构化 mentions；
- recipient 解析失败必须稳定拒绝；
- 内容可内联或走 content store；
- 消息接受不等于模型已读。

### 4.2 Delivery

V1 标准只冻结三种：

| Delivery | 语义 |
|---|---|
| `queue` | 只进 mailbox，不启动 idle agent |
| `trigger` | 进 mailbox；idle 时启动 turn |
| `interrupt` | 中断当前 turn；agent 保留可复用 |

后续扩展：

| Delivery | 状态 |
|---|---|
| `steer` | 第二阶段 |
| `interject` | 第二阶段 |

### 4.3 Mailbox 安全边界

必须满足：

- queue-only 消息可在当前 turn 的下一个模型请求前合并；
- 一旦当前 turn 已经产生可见 final answer，mailbox 延期到下一 turn；
- trigger-turn 消息优先于普通 idle turn；
- `wait_agent` 只等待 mailbox/steer activity，不返回正文；
- completion result 默认 `trigger=false` 投递父 mailbox；
- 父代理必须显式 `wait_agent` 或下一 turn 处理结果；
- 不允许 child completion 无界唤醒父代理。

### 4.4 工具面

| 工具 | 语义 |
|---|---|
| `spawn_agent` | 创建 child，initial message `trigger=true` |
| `send_message` | 定向 mailbox，`trigger=false` |
| `followup_task` | 定向 mailbox，`trigger=true` |
| `wait_agent` | 等待 mailbox/steer activity |
| `interrupt_agent` | 中断目标 turn |
| `list_agents` | 按 root tree / path prefix 列举 |
| `close_agent` | V1 legacy；V2 不用作删除逻辑身份 |

V2 不提供手动 `resume_agent`；reload 是 delivery/ownership 的内部行为。

---

## 5. 前端消费标准

### 5.1 核心投影

新增 Team projection，至少包含：

```text
TeamSnapshot {
  root_session_id
  agents: AgentSnapshot[]
  unread_messages: InboxSummary[]
  revision
}

AgentSnapshot {
  agent_id
  agent_path
  nickname?
  role?
  status
  residency
  parent_agent_path?
  current_task_id?
}

TeamDelta {
  AgentJoined
  AgentStatusChanged
  AgentResidencyChanged
  AgentMessageQueued
  AgentMessageDelivered
  AgentInterrupted
  AgentCompleted
}
```

### 5.2 TUI / WinUI 要求

- roster 以 `AgentPath` 为主，nickname 为辅；
- `/subagents` 支持 path prefix；
- 点击 agent 打开 child transcript；
- unloaded agent 仍可见，不能显示成 deleted；
- `@` 自动补全来自 roster；
- transcript 区分 user / main / subagent message；
- inbox 显示 author、recipient、task、delivery；
- 不再从 `spawn_subagent` 工具卡推导身份；
- `control.subagents` bootstrap 必须被消费；
- `SubagentSpawned` 即使早于工具卡到达也能创建条目；
- `SubagentFinished` 即使 timeline 丢失也能收敛状态。

---

## 6. 权限与配额

### 6.1 默认权限

- 只允许同一 root tree 内通信。
- relative path 只允许向下寻址。
- 跨分支必须使用 absolute path。
- root/self interrupt 默认拒绝。
- child 默认不能 `TriggerTurn` root。
- parent-owned child 默认拒绝 app-server/direct input。
- child reload 必须经 loaded immediate parent。

### 6.2 配额

- 并发按“活跃执行 turn”计数，不按逻辑 agent 数计数。
- logical agent 数量与 loaded residency 分离。
- max depth 可配置；默认 1，允许后续提高。
- 单消息大小上限。
- sender-target in-flight 上限。
- sender attempt 总 outbound 上限。
- broadcast/`@all` 默认禁止，后续只允许 root/user。

---

## 7. 重写阶段

### Phase 0：文档与类型冻结（2026-09-25 accepted）

- 写本 spec 的 accepted 版本。
- 在 `decisions.md` 增加 AgentPath、AgentGraph、Mailbox、Residency 决策。
- 冻结 `AgentPath` grammar 和 `InterAgentCommunication` 形状。
- 补 `SubagentSpawned/Finished` 的真实 producer 设计。

Phase 0 的 producer 冻结如下：

1. `spawn_subagent` 在父 session 的某个 `ToolIntent.call_id` 下执行；父
   `session_id` 和 `call_id` 是 edge fact 的权威归属。
2. 工具 handler 只负责向宿主申请 child identity；宿主返回
   `child_session_id + child_agent_path`，不得在 handler 内另造事实。
3. runtime 在 child actor 已创建、task 尚未发送前，使用父 session 的
   `ToolLedger` 写入 `SubagentSpawned`。写入失败必须关闭 child 并让工具失败，
   不得留下无 canonical edge 的可运行 child。
4. child 的 `SessionCreated.parent_session_id` 只是便于单 session 恢复的
   denormalized hint；parent-child edge 的权威仍是父 log 的
   `SubagentSpawned/Finished`。
5. child 终态由 runtime 从 child canonical terminal fact 映射为父 log 的
   `SubagentFinished`；同一 `child_session_id` 的重复同值事实幂等，冲突状态
   稳定拒绝。
6. daemon 重启时从父 log 重建 graph。只有父 log 与 child log 均存在且 edge
   匹配时才恢复为 loaded；edge 缺失、冲突或无法读取时必须 fail closed。

### Phase 1：Canonical identity + graph

- 引入 `AgentPath` 与 `AgentMetadata`。
- 注册 root `/root`。
- spawn 时生成/校验 child path。
- 持久化 parent-child edge。
- `list_agents` 支持 path prefix。
- 修复 child reload 后 path 丢失。

### Phase 2：Mailbox + delivery

- 引入 `InterAgentCommunication`。
- `spawn_agent` initial message 走 mailbox。
- `send_message` / `followup_task` 分离。
- `wait_agent` 接 mailbox activity。
- child completion 改为 queue-only。
- 禁止 completion 无界触发父 turn。

### Phase 3：Residency + reload

- `AgentStatus` 与 `Residency` 分离。
- completed/errored/interrupted idle child 可 unload。
- delivery 自动 reload。
- reload 经 parent ownership。
- V2 停用 `close_agent` 删除语义，改用 `interrupt_agent`。

### Phase 4：前端 Team projection

- `TeamSnapshot/TeamDelta` 落地。
- TUI roster / inbox / status / child transcript。
- 删除从工具卡 JSON 推导身份的旧路径。
- `@` 使用结构化 mention。

### Phase 5：Task board

另开 spec：

- task 图；
- owner / claim / epoch；
- dependencies；
- artifact / acceptance；
- task 与 message 绑定。

### Phase 6：Message board

另开 spec：

- channel / thread / post / subscription；
- 通知只投给 running agent；
- post 持久化，missed notification 不保证补发；
- 与 task board 通过 `task_id` 关联。

### Phase 7：Steer / Interject

- 在 queue/trigger/interrupt 稳定后引入；
- 需要明确 safe point 和优先级；
- 必须有配额和防止 agent 互喷策略。

---

## 8. 验收矩阵

| ID | 场景 | 必须成立 |
|---|---|---|
| SA2-P1 | path 解析 | absolute/relative 正确；`..` 和非法 segment 稳定拒绝 |
| SA2-P2 | path 唯一性 | 同 tree 内重复 path 原子拒绝 |
| SA2-G1 | edge 持久化 | spawn edge 可重建；daemon 重启不丢拓扑 |
| SA2-G2 | cascade | close/interrupt 父节点正确处理 descendants |
| SA2-M1 | send_message | queue-only，不启动 idle agent |
| SA2-M2 | followup_task | idle 时触发 turn；running 时按 mailbox 边界投递 |
| SA2-M3 | completion | child result 只进父 mailbox，不自动触发父 turn |
| SA2-W1 | wait_agent | mailbox/steer 可唤醒；timeout 语义稳定 |
| SA2-I1 | interrupt | 只中断 turn；身份保留；可继续收消息 |
| SA2-R1 | unload | completed/idle child 可 unload；状态不丢失 |
| SA2-R2 | reload | delivery 可 reload；必须经 parent ownership |
| SA2-S1 | ownership | parent-owned child 拒绝 direct input |
| SA2-S2 | root safety | child 默认不能 trigger root；root/self interrupt 拒绝 |
| SA2-F1 | frontend roster | bootstrap/delta 能恢复 agent path/status/residency |
| SA2-F2 | frontend dedup | timeline 工具卡与 control delta 重复不产生双条目 |
| SA2-T1 | task board | claim 原子；dependencies/close gate 稳定 |
| SA2-B1 | message board | post 持久化；只通知 running agent；不启动 idle |
| SA2-Q1 | quota | 并发按 active turn；消息 in-flight/outbound 超限稳定拒绝 |

---

## 9. Issue 拆分建议

1. **SUBV2-01**：`AgentPath` grammar + resolver + tests。
2. **SUBV2-02**：Agent registry / metadata / root registration。
3. **SUBV2-03**：durable parent-child graph + cascade lifecycle。
4. **SUBV2-04**：canonical `SubagentSpawned/Finished` producer。
5. **SUBV2-05**：`InterAgentCommunication` + mailbox。
6. **SUBV2-06**：`spawn_agent` initial message 改为 communication。
7. **SUBV2-07**：`send_message` / `followup_task` / `wait_agent` / `interrupt_agent`。
8. **SUBV2-08**：residency LRU + reload-on-delivery + parent ownership。
9. **SUBV2-09**：`list_agents` path prefix + status snapshot。
10. **SUBV2-10**：TeamSnapshot/TeamDelta + TUI roster/inbox。
11. **TEAM-01**：task board spec + implementation。
12. **BOARD-01**：message board spec + implementation。
13. **SUBV2-11**：steer/interject 与配额扩展。

依赖顺序：

```text
SUBV2-01
  -> SUBV2-02
  -> SUBV2-03
  -> SUBV2-04
  -> SUBV2-05
  -> SUBV2-06/07
  -> SUBV2-08/09
  -> SUBV2-10
  -> TEAM-01
  -> BOARD-01
  -> SUBV2-11
```

---

## 10. 兼容与迁移

### 10.1 保留兼容

- V1 `spawn_subagent` 的 agent_id 语义保留到兼容窗口结束。
- `close_agent` 在 V1 保留。
- 旧 subagent result injection 可保留 feature flag，但不再是 V2 默认。

### 10.2 硬切点

V2 启用后：

- 新 spawn 必须生成 `AgentPath`；
- 新消息必须携带 author/recipient；
- completion 不得默认触发父 turn；
- frontend 不得只依赖工具卡发现 subagent；
- graph store 不可用时 fail closed。

### 10.3 删除路径

满足以下条件后删除旧路径：

- V2 path/graph/mailbox 全量 fixture 绿；
- TUI/WinUI 消费 Team projection；
- reload/residency 在 daemon 重启与断线场景通过；
- 旧 v1 compatibility 窗口结束。

---

## 11. 已裁决

以下按建议冻结；实现不得自行改变默认值：

1. qaqh 默认 max depth 保持 `1`，配置可开 `2`。
2. child 可在同一 root tree 内 `followup_task` 同级，但不得 trigger root。
3. task board 另开 Team canonical aggregate，不混 session log；需单独 spec。
4. message board post 持久化；notification 只保证 running，unread 由前端
   projection 计算。
5. V1 `close_agent` 保留兼容；V2 只保留 `interrupt_agent` + residency eviction。

---

## 12. 文档重写规则

本 spec accepted 后：

1. 新权威文档固定为：
   `docs/current/spec/2026-09-25-subagent-v2-rewrite-spec.md`。
2. 旧 subagent 相关文档全部归档，不再作为当前依据。
3. `docs/current/decisions.md` 新增 AgentPath / AgentGraph / Mailbox / Residency 决策。
4. `docs/current/architecture.md` 增加 AgentControl、AgentRegistry、AgentGraphStore、Mailbox、Residency 组件。
5. `docs/current/status.md` 记录实现阶段。
6. `docs/current/debug-backlog.md` 只保留未完成项，不重复 spec。
7. TUI/WinUI 各自维护消费侧附录，但不得分叉 wire 语义。

---

## 附录 A：Codex 参考提交链

```text
70cdb1770  feat: add graph representation of agent network (#15056)
79ad7b247  feat: change multi-agent to use path-like system instead of uuids (#15313)
18f1a08bc  feat: new op type for sub-agents communication (#15556)
38c088ba8  feat: list agents for sub-agent v2 (#15621)
773fbf56a  feat: communication pattern v2 (#15647)
970386e8b  fix: root as std agent (#15881)
426f28ca9  feat: spawn v2 as inter agent communication (#15985)
b528ff02b  chore: morpheus to path (#18353)
782191547  Add agent graph store interface (#19229)
d5e4f01af  feat: reload v2 agents on delivery (#26623)
4e803a017  feat: add v2 agent residency lru (#26632)
743f5aad3  feat: count V2 concurrency by active execution (#26969)
8d415050f  Rename multi-agent v2 close_agent to interrupt_agent (#26994)
d21794d6b  Reload Multi-Agent V2 children through their parent (#40477)
b6333bb1b  Enforce subagent ownership across app-server inputs (#40464)
2126f9361  Route peer agent completion activity to the initiating turn (#40449)
8d282f018  Wire agent message boards into persistent multi-agent runtimes (#47017)
30fc6864c  Add an option to disable multi-agent v2 direct messaging (#47540)
```

## 附录 B：核心裁决

> 注入/反注入是 delivery，不是 identity。
>
> UUID 能标识，但不能提供模型可读、可列举、可恢复、可授权的 tree address。
>
> `/root/<task_name>` 的价值在于把 agent graph 变成稳定 namespace；mailbox 和 residency 再分别解决调度与生命周期。
>
> qaqh 应保留注入机制，但必须新增 AgentPath、durable graph、mailbox、residency 和 Team projection。
