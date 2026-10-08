# ADR：ControlCommand::PublishResourceChanged（迁移期 wire 局部解冻）

日期：2026-10-08。状态：已实施（本 ADR 同 PR 落地）。

## 授权与范围

仓库所有者在 2026-10-08 的施工会话明确授权：为迁移到 v2 架构，wire 冻结约束（I14）可暂时移除使用。本次例外仅限 Control 频道新增一个命令变体 `PublishResourceChanged { resource_kind: String }`（带 `#[serde(default)]`），不改任何既有事件/命令语义、不删字段。其他 Ringing v2 改动仍需逐一审批。

## 背景与决策

Todo 的 service 直写入口（`todo.set`/`todo.cancel`）在 actor 线程之外改写 `todo.json`，而会话 ledger 由 worker 线程独占（单写者）。为了让这些变更产生 canonical `WorkspaceResourceChanged` fact（审计优先级 1，见 todo-refresh-audit），需要一条从 service 线程进入 actor 的通道；actor 的命令通道就是 wire 命令，故新增：

- `ControlCommand::PublishResourceChanged { resource_kind }`：service 在成功持久化资源文件后发送；actor 收到后自行读取资源现状、写 summary blob、追加 fact（blob → fact 顺序由 actor 内部保证，I2/I5）。`resource_kind` 当前仅接受 `"todo"`（与 `ResourceKind` 的 snake_case 序列化一致）；未知值在 worker 侧拒绝并告警。
- actor 线程内部的 Todo 变更（typed 工具回填、UI 直调、goal 模式转换）不经 wire，直接走 `agent/resource_publish.rs` 统一发布。

不重新启用已退役的 DashboardUpdated 即时刷新；所有通知都以持久 fact 为源。

## 备选及否决理由

- service 线程直接 append ledger：破坏 worker 单写者，有并发损坏风险。
- service 直接发易失 ResourceDelta（v2 hub）：非 canonical，先发布后（无）持久化，不符合审计口径。
- 为资源类型建 domain 层枚举：qaqh-domain 不能依赖 qaqh-session（I13），复制第二套 ResourceKind 违反 I16。

## 后果与边界

- 桌面仓 ts 导出需同步重新生成（`RingingCommand`/`ControlCommand` 为 ts 导出面）。
- 会话未加载（无 worker）时 service 直写仍只落文件、无 fact——已知覆盖缺口，记入施工单 §9，待 CLEAN-3 命令模型收敛。
- 此命令只补发 fact，不回传业务数据；资源正文权威仍是各资源文件 + fact 投影。
