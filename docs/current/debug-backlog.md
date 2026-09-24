# Debug Backlog

> 日期：2026-09-25
> 基线：`2.0.0-alpha2`
> 状态：active

本文件是当前唯一 debug 待办入口。优先级按“是否影响正确性/安全/可恢复性”排序，
不按历史 handoff 的编号排序。

## P0：先做

### 1. Interaction 跨 daemon 重启持久化

**问题**

- ask / plan 正文当前写入内存 `ContentStore` 并 pin。
- daemon 重启后内存 store 丢失。
- orphan seal 会把无终态的 pending interaction 收成 `Dismissed`。

**影响**

- 重启前未回答的 ask/plan/permission 无法在新 daemon 中继续。
- 与“断线重连可恢复 pending modal”的语义不一致。

**需要裁决/实现**

- 持久化 content store，或
- 将 pending interaction 正文纳入 durable session 数据，或
- 明确产品语义：重启即 dismiss。

**代码入口**

- `crates/qaqh-runtime/src/ringing/content_store.rs`
- `crates/qaqh-runtime/src/ringing/hub.rs`
- `crates/qaqh-runtime/src/ringing/orphan_seal.rs`
- `crates/qaqh-runtime/src/registry.rs`

### 2. Permission 正文 pin / 终结 unpin

**问题**

- ask/plan 正文会 pin；permission 正文当前走普通 TTL。
- 极端容量压力下，permission modal 正文可能在客户端读取前被淘汰。

**需要**

- 为 permission 终结路径提供稳定 unpin 语义，或
- 明确 permission 正文允许 TTL 淘汰并接受 404 降级。

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
