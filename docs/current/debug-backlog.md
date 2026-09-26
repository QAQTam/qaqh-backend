# Debug Backlog

> 日期：2026-09-25
> 基线：`2.0.0-alpha2`
> 状态：active

本文件是当前唯一 debug 待办入口。优先级按“是否影响正确性/安全/可恢复性”排序，
不按历史 handoff 的编号排序。

## P0：先做

### 1. Interaction 跨 daemon 重启持久化

**已完成**

- content store 已持久化：正文、media_type、TTL、pinned、pin_key 重启后仍在。
- ask / plan / permission 正文重启后仍可通过 content 端点读取。
- 大正文 `InputAccepted` / `InterAgentCommunication` 已外置为 `content_ref`。

**仍未完成：重启后挂起 turn 无法继续**

- orphan seal 仍会把无终态的 pending interaction 收成 `Dismissed`。
- 原因是 turn actor / tool engine 的挂起状态在内存中，当前没有 turn resume 路径。
- 正文不再丢失，但交互本身不会恢复为 pending。

**需要产品裁决**

- 明确「daemon 重启即 dismiss pending interaction」，或
- 实现 turn resume：从 canonical facts 重建挂起 tool batch / interaction 并恢复 actor。

**代码入口**

- `crates/qaqh-runtime/src/ringing/content_store.rs`
- `crates/qaqh-runtime/src/ringing/hub.rs`
- `crates/qaqh-runtime/src/ringing/orphan_seal.rs`
- `crates/qaqh-runtime/src/registry.rs`

### 2. Permission 正文 pin / 终结 unpin

**已完成**

- permission 正文与 ask / plan 一样进入 content store 并 pin。
- pin_key = `canonical_interaction_id(tool_call_id)`；hub 在发布同一 tool_call_id 的
  `ToolFinished` 时解除 pin。
- 重启后 live 表为空时，`unpin_key` 仍可按持久化的 pin_key 释放。

**剩余边界**

- 若未来出现「permission resolved 但不产生 ToolFinished」的新路径，需要补对应
  unpin 事件；否则 pin 会保留到 session close。

### 3. Sandbox fallback 安全语义

**现状**

- Linux 支持 bubblewrap 或 Landlock/seccomp。
- `Auto` 在两者不可用时可能退到 `ProcessHardening`，该后端不提供文件系统/网络隔离。

**待办**

- 决定缺失强后端时是 fail-closed 还是显式降级。
- `.git/hooks` 只读、私有 `/tmp`、读白名单。
- cgroup v2 CPU/内存限制。
- 更新 L4 描述，避免继续声称“exec may escape until sandboxing lands”而忽略现有后端。

## P1：debug 阶段高优先

### 4. Windows 实机验证

- V2-W1 未在 Windows 运行。
- 保留设备名/设备路径行为需要 Windows 实机确认。
- exec pipe OEM 解码、后台进程继承写端、shell 探测需要 Windows 回归。

### 5. Driver `not_eligible` / 显式移交优先级

**已落地**

- 3s 回收巡检
- driver watch 持久化
- daemon 重启回收
- workspace service gate
- `driver_epoch` 进入 command fingerprint

**待产品裁决**

- 新 client 何时 `not_eligible`；
- 是否允许显式抢占 live holder；
- 多端场景的优先级规则。

## P2：debug/性能 backlog

### 6. Timeline 性能残余

- `update_tool` 冗余 clone。
- timeline sidecar 大文件压力验证。
- `snapshot()` 全量 clone turns。
- 大快照 checkpoint 成本。

### 7. 文件与平台边界

- `read` 空文件返回 `"L1: "`。
- Windows 保留设备名/设备路径实机验证。
- `git2` pathspec 过滤 FIXME。

### 8. 安全/工具 backlog

- CNB MCP `repo` 参数校验。
- actor workspace 并发漂移。
- `cnb_issue_close` 默认 `state_reason` 语义。
- exec spawn 期 stdio 接线异常继续观察。

### 9. Secrets

- 非 Linux 平台仍依赖 0600 文件权限。
- keyring 集成未做。

## P3 / P6：暂缓，不阻塞 debug

- 消息正文并入 canonical fact log（compact 路线 2）。
- `compact_skip` 旧 wire 字段清理。
- `build_context_for_gate` 整段 clone / 峰值内存优化。
- 旧 `~/.config/qaqh/ringing` 目录迁移/归档/GC。
- 旧 v1 类型、envelope、handler 命名清理。
- `LegacyWriterFacade` 完全退场。
- policy/workspace 职责最终迁移。
- composition root / embedded-remote 评估。

## 10. 当前不建议做

- 不重开 v1 兼容。
- 不把 projection/cache 重新变成事实源。
- 不在没有 parity 测试的情况下直接把 gate 换成 `mutilAI-SDK`。
- 不为清理而大规模移动代码；debug 阶段优先修行为。
