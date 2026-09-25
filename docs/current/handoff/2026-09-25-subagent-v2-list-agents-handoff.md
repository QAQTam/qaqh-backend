# Subagent V2 `list_agents` Handoff

> 日期：2026-09-25
> 基线：`7205894`（`main`）
> 实现分支：`feat/subv2-list-agents-20260925`
> 状态：实现完成，待 PR 审核
> 范围：`qaqh-subagent`、`qaqh-runtime`

## 1. 本次结论

Phase 1 遗留的 `list_agents` path-prefix 工具面已落地：

```text
list_agents(path_prefix?)
  -> SubagentHost::list_agents(caller_session_id, prefix)
  -> AgentRegistry::list_agents_for_caller
  -> AgentCatalog::list_prefix
```

工具只读取逻辑 metadata，不启动、不 reload、不改变 agent residency。

## 2. 行为

- `path_prefix` 可选，默认 `/root`；
- absolute prefix 必须与 caller 属于同一 Agent namespace；
- relative prefix 只允许从 caller 的 `AgentPath` 向下解析；
- 返回 root tree 内的逻辑 agent metadata：
  - `root_session_id`
  - `agent_id`
  - `agent_path`
  - `parent_agent_path`
  - `nickname`
  - `role`
  - `created_at_ms`
- unloaded agent 仍可被列出；
- 无 host 时返回稳定 `HOST_UNAVAILABLE`。

## 3. 验收证据

```text
cargo test -p qaqh-subagent --offline -- --test-threads=1
cargo test -p qaqh-runtime --test host_direct --offline -- --test-threads=1
```

覆盖：

- `list_agents` 已注册，`path_prefix` 可选；
- root caller 用相对 prefix 只列 child；
- child caller 可用 `/root` 列出整个 root tree；
- `/morpheus` 等跨 namespace prefix fail closed。

## 4. 未决项

- 工具当前只返回 metadata，不包含 runtime status / residency；这两项要等 Phase 3。
- 尚未接入 TUI `/subagents`、roster 或 `@` 自动补全；属于 Phase 4。
- child reload 后 path 恢复仍未完成。

## 5. 接手注意事项

- `list_agents` 不得触发 idle agent，也不得被解释为 delivery。
- legacy alias seed 不得进入 agent catalog；只使用 canonical `agent_id`。
- 后续 status/residency 字段必须来自 Phase 3 的显式状态，不得从进程是否 loaded 猜测。
