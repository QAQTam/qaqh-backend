# Session Identity Unification / seed 退场设计

> 日期：2026-09-25
> 状态：accepted，beta 前必须完成
> 适用：`qaqh-session`、`qaqh-runtime`、`qaqh-daemon`、`qaqh-client`、`qaqh-ringing`、`qaqh-workspace`、`qaqh-message`、`qaqh-subagent`；消费侧包括 TUI / WinUI

## 0. 一句话

`seed` 是早期原型留下的存储/路由键。目标模型是：

> **`SessionId` 成为唯一会话主键；新会话必须满足 `seed == session_id`；beta 前删除 seed 的历史包袱。**

`seed` 可以短期作为 deprecated alias 存在，但不能再拥有独立身份语义。

---

## 1. 背景

当前系统同时存在两套会话标识：

```text
seed       = 8 位 hex，如 0a1b2c3d
SessionId  = UUIDv7，如 0198f1a0-...-7...
```

它们今天分别承担：

| 标识 | 当前职责 |
|---|---|
| `seed` | `sessions/{seed}/`、API 路由、runtime active session、lease、quota、Ringing envelope、TUI tab |
| `SessionId` | canonical facts、`SessionCreated.parent_session_id`、审计、恢复 |

问题不是“有两个字段”，而是：

- `seed` 同时是存储键、业务键和 wire key；
- `SessionId` 只在 canonical 层生效；
- 两者需要 `canonical-identity.json` 做桥接；
- 任何把 seed 当 `SessionId` 使用的代码都会在 fact validation 时才失败；
- subagent、graph、mailbox、Team projection 会持续面对双 id 空间。

因此不能继续把 seed 作为 AgentId 或 canonical child id。

---

## 2. 术语

### 2.1 SessionId

```text
SessionId = canonical UUIDv7
```

要求：

- 全局唯一；
- 时间有序；
- 可持久化；
- 是 `AgentId`；
- 是 wire / runtime / 存储的目标主键。

### 2.2 LogId

```text
LogId = canonical UUIDv7
```

`LogId` 继续独立存在，不与 `SessionId` 合并。

职责：

- 标识一份 canonical log；
- 检测目录复制、日志替换、reset 和升级；
- 由 `canonical-identity.json` 持久化。

### 2.3 seed

`seed` 在迁移期只允许有两种含义：

1. **新会话**：`seed` 是 `SessionId` 的 deprecated alias，值完全相同；
2. **旧会话**：`legacy_seed` 是物理目录定位键，只能经兼容 resolver 使用。

不允许再产生新的 8 位 hex seed。

### 2.4 AgentPath

`AgentPath` 继续独立：

```text
/root
/root/review
/root/review/tests
```

它解决“模型怎么称呼 agent”，不解决“会话是谁”或“日志在哪”。

---

## 3. 目标不变量

### 3.1 新会话

```text
seed == session_id
session_id == sessions/{directory_name}
canonical_identity.session_id == session_id
meta.session_id == session_id
```

### 3.2 canonical facts

```text
SessionFact.session_id == SessionId
SessionCreated.parent_session_id == parent SessionId
SubagentSpawned.child_session_id == child SessionId
```

禁止：

```rust
SessionId::new(legacy_seed)
```

除非该值已经由 resolver 解析成 canonical SessionId。

### 3.3 runtime / wire

新协议中：

```text
runtime instance key == SessionId
Ringing envelope key == SessionId
lease / driver / cancel key == SessionId
wire session key == SessionId
```

迁移窗口内可以保留 `seed` 字段名，但值必须是 canonical SessionId。

### 3.4 旧数据

旧会话允许：

```text
legacy_seed != session_id
```

但只能通过显式兼容 resolver 访问：

```text
legacy_seed
  -> canonical-identity.json
  -> session_id
```

该 resolver 不得进入新事实、新 graph 或新 Team projection。

---

## 4. 目标数据模型

### 4.1 CanonicalSessionIdentity

```text
CanonicalSessionIdentity {
  schema
  session_id: SessionId
  log_id: LogId
}
```

创建规则改为：

1. 先生成 `SessionId`；
2. 创建 `sessions/{session_id}/`；
3. 将同一个 `session_id` 写入 identity sidecar；
4. 不允许 `open_or_create(dir)` 再生成第二个 `session_id`。

### 4.2 SessionMeta

目标字段：

```text
SessionMeta {
  session_id
  created_at
  updated_at
  ...
}
```

迁移期可暂时保留：

```text
seed: String
```

但必须满足：

```text
seed == session_id
```

最终通过 serde alias / migration 删除 `seed`。

### 4.3 AgentMetadata

```text
AgentMetadata {
  agent_id: SessionId
  root_session_id: SessionId
  agent_path: AgentPath
  parent_agent_path?
  ...
}
```

如果 `session_id` 已同时作为目录名，则不需要长期保存 `session_seed`。

### 4.4 SubagentSpawned

新事实：

```text
SubagentSpawned {
  child_session_id: SessionId
  parent_call_id
  parent_agent_path?
  child_agent_path?
  role?
  spawned_at_ms
}
```

因为新 child 的 `seed == child_session_id`，不需要额外设计永久 `child_session_seed`。

旧 child 若使用 legacy seed，只能由迁移 resolver 在读取层处理。

---

## 5. 创建流程

### 5.1 新会话

```text
1. allocate SessionIdentity { session_id, log_id }
2. create sessions/{session_id}/
3. write canonical-identity.json
4. write meta.json
5. spawn actor with session_id
6. return session_id
```

### 5.2 新 subagent child

```text
1. allocate child SessionId
2. create child canonical session
   SessionCreated.parent_session_id = parent SessionId
3. create child actor
4. write parent SubagentSpawned.child_session_id = child SessionId
5. deliver initial task
```

不再出现：

```text
child_seed = 0a1b2c3d
child_session_id = UUID
```

### 5.3 旧会话 resume

```text
1. resolve legacy_seed -> session_id
2. read canonical identity
3. operate entirely on session_id
4. 后台迁移目录到 sessions/{session_id}/
```

---

## 6. 迁移策略

### Phase A：建立 resolver

- `legacy_seed -> session_id` 只读索引；
- 只用于旧数据；
- 所有新 API 只接受 `session_id`。

### Phase B：新会话统一

- `generate_seed()` 不再产生 8 位 hex；
- 新会话从 identity 分配开始；
- `seed == session_id`；
- `sessions/{session_id}` 成为默认布局。

### Phase C：旧目录迁移

对每个旧目录：

```text
sessions/{legacy_seed}
  -> sessions/{session_id}
```

必须处理：

- session index；
- active session；
- WorkspaceStore；
- todo / trusted folders；
- quota / lease / cancel；
- 任何保存的旧 seed 引用。

要求：

- 原子 rename；
- migration journal；
- 幂等重试；
- 崩溃恢复；
- 迁移完成前保留 legacy resolver。

### Phase D：wire / runtime 统一

- `RingingEventEnvelope.seed` 改为 `session_id`；
- `RingingCommandEnvelope.seed` 改为 `session_id`；
- `RUNTIME_CTX.active_session` 改为 `session_id`；
- `AgentRegistry` / `RingingHub` / lease / driver / quota 全部使用 `session_id`；
- client / TUI / WinUI 只使用 `session_id`。

### Phase E：删除 seed

- 删除 `generate_seed()`；
- 删除 `SessionMeta.seed`；
- 删除 `seed` wire 字段；
- 删除 legacy resolver；
- 删除旧目录；
- 删除 seed 文档和测试。

---

## 7. Beta 硬门禁

beta 前必须全部成立：

- [ ] 新会话全部满足 `seed == session_id`。
- [ ] 生产路径不再调用 `generate_seed()` 产生 8 位 hex。
- [ ] `SessionId::new(legacy_seed)` 不再出现在 canonical producer。
- [ ] subagent child 使用 canonical `SessionId`。
- [ ] `SubagentSpawned/Finished` 不再依赖 seed 作为逻辑身份。
- [ ] `sessions/{session_id}` 成为新默认布局。
- [ ] 旧会话完成目录迁移或仅通过 legacy resolver 读取。
- [ ] `Ringing`、lease、driver、quota、cancel 使用 session_id。
- [ ] TUI / WinUI 不再假设 seed 是 8 位 hex。
- [ ] `canonical-identity.json` 的 session_id 与目录名一致。
- [ ] `log_id` 继续独立，不与 session_id 合并。
- [ ] seed 相关兼容代码有明确删除日期。

---

## 8. 与 Subagent V2 的关系

本设计是 Subagent V2 的前置条件：

- `AgentId = SessionId`；
- `SubagentSpawned.child_session_id` 必须是 canonical UUID；
- graph loader 不能靠 seed 猜测身份；
- Team projection 只暴露 `agent_id + agent_path`；
- mailbox 不允许用 legacy seed 作为 recipient key。

Subagent V2 的 canonical producer 在 seed 统一前不得宣称完成。

---

## 9. 测试要求

### 新会话

- 使用 UUIDv7 作为 seed / directory / meta / fact id；
- identity.session_id 与目录名一致；
- `log_id != session_id`。

### 旧会话

- legacy seed 可解析到 session_id；
- 目录迁移幂等；
- 迁移中断后可恢复；
- 旧路由只读兼容。

### Subagent

- 真实 UUID seed 端到端 spawn；
- `SubagentSpawned` validation 通过；
- graph loader 按 session_id 找到目录；
- child finish 后目录仍可重建。

### Wire / frontend

- wire 只暴露 session_id；
- TUI 不依赖 8 位长度；
- tab / timeline / stream key 使用 session_id；
- legacy seed 不进入新 roster。

---

## 10. 非目标

- 不把 `SessionId` 和 `LogId` 合并。
- 不把 `AgentPath` 当 session key。
- 不允许新事实继续使用 8 位 hex seed。
- 不永久保留 `seed == session_id` 的双字段。
- 不在 beta 前继续扩大 seed 使用面。

---

## 11. 当前阻塞映射

| 阻塞 | 归属 | 修复方向 |
|---|---|---|
| `SubagentSpawned.child_session_id` 用了 8 位 seed | Subagent V2 | 先生成 canonical SessionId，再写 fact |
| graph loader 按 seed 找目录 | identity migration | 目录名统一为 session_id |
| catalog root id 与 canonical id 混用 | identity migration | AgentMetadata 只存 canonical id |
| #369 缺 commit marker 回归 | session materialization | 无 canonical log 时按新会话/空 root 处理 |
| terminal injection 不匹配 | Subagent V2 runtime | `subagent_terminal: Some` 必须进入 lap boundary |
| ephemeral child 删除 canonical 目录 | Subagent V2 lifecycle | V2 child 持久化，residency 负责 unload |
