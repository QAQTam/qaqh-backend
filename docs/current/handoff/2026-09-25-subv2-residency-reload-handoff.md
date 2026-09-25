# Subagent V2 Residency / Reload Handoff

> 日期：2026-09-25
> 基线：`e3f0e3d`（`main`）
> 工作方式：直接在 `main` 推进，不创建 worktree
> 状态：reload-on-delivery 与 parent ownership 已实现并通过定向验收
> 范围：`qaqh-session`、`qaqh-workspace`、`qaqh-subagent`、`qaqh-runtime`

## 1. 本次结论

SUBV2-08 的第一条关键路径已落地：

```text
unloaded child
  -> delivery command
  -> resolve canonical child metadata
  -> require loaded immediate parent
  -> read durable SubagentSpawned.spawn_config
  -> reload child as Subagent (not Session)
  -> restore parent edge
  -> Trigger delivery re-arms result collector before command write
```

child reload 不再退化成普通 session，也不再丢失 AgentPath。

## 2. 已落地

### 2.1 Durable spawn config

`SubagentSpawned` 新增可选：

```text
spawn_config:
  tools[]
  model?
  base_url?
  max_tokens?
  ephemeral
  timeout_secs
```

canonical validation 限制工具名、model/base_url 大小和 timeout 范围。旧 fact
缺省时仍可读取，但 delivery reload 对缺失 config fail closed。

### 2.2 Parent-owned reload

`AgentRegistry::send_ringing` 对已登记的 child 不再调用普通 `get_or_spawn`：

- 已 loaded：直接投递；
- unloaded：必须找到 canonical metadata 中的直接 parent；
- immediate parent 未 loaded：拒绝 reload；
- loaded parent：读取 parent canonical `SubagentSpawned.spawn_config`；
- 以 `AgentKind::Subagent` reload，并恢复 supervisor parent edge；
- reload 不重复消费 spawn quota，不重写 canonical edge。

### 2.3 Collector re-arm

原 collector 随上一 turn 结束并 unload child。Trigger delivery 现在会在写入
命令前重新订阅 child event stream：

- 只对同一 root tree 的 child Trigger delivery；
- 复用 canonical `parent_call_id` 和 `timeout_secs`；
- completion 仍以 queue-only `InterAgentCommunication` 投递父 mailbox；
- collector 结束后 close child，但保留 logical identity。

### 2.4 Subagent idle LRU

`spawn_subagent_inprocess` 现在把同一个 `WorkerLiveness` 同时交给 actor 和
registry，不再让 `AgentInstance.liveness = None`。因此：

- idle subagent 可独立进入 `unload_idle_sessions` 候选；
- parent 仍 loaded 时只 unload child，不删除 canonical metadata；
- 后续 delivery 仍走 parent-owned reload。

## 3. 验收证据

```text
cargo test -p qaqh-subagent --offline -- --test-threads=1
cargo test -p qaqh-runtime --test host_direct --offline -- --test-threads=1
cargo clippy --workspace --all-targets --offline -- -D warnings
```

新增覆盖：

- unload 后 child 仍出现在 `list_agents`；
- Queue/Trigger delivery 经 loaded parent reload；
- reload 后 child canonical mailbox 收到投递；
- child 可独立进入 idle-unload 候选，随后仍可 delivery reload；
- immediate parent unload 后 child delivery fail closed；
- reload collector 在 Trigger 前 arm，并能把 completion 发回 parent。

## 4. 未决项

- AgentStatus / Residency 的前端 snapshot/delta 尚未落地；
- 旧 spawn fact 无 `spawn_config` 时只支持 loaded 生命周期，不支持 reload；
- steer/interject 和配额扩展仍属后续阶段；
- 大正文 `content_ref` 外置未实现。

## 5. 接手注意事项

- child reload 必须经 loaded immediate parent，不能回退到 `get_or_spawn`；
- reload 必须恢复 `AgentKind::Subagent` 和 supervisor edge；
- Trigger 前必须 arm collector，Queue 不应启动 collector 或 turn；
- canonical `SubagentSpawned` 仍是 spawn config 的唯一持久事实源。
