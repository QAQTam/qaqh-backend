# BETA-01 Session Identity Foundation Handoff

> 日期：2026-09-25
> 基线：`67f73c0`（`main`）
> 实现分支：`feat/beta-01-session-identity-foundation-20260925`
> 状态：基础切片实现完成，待 PR 审核
> 范围：`qaqh-session`、`qaqh-runtime`

## 1. 本次结论

BETA-01 的第一段代码已经落地：

```text
allocate CanonicalSessionIdentity
  -> session_id
  -> sessions/{session_id}
  -> canonical-identity.json
  -> meta.seed == session_id
```

普通会话、自动建会话和子代理的生产创建路径不再先产生 8 位 seed，再旁路生成
UUID。新会话现在满足：

```text
seed == session_id == sessions/{directory_name}
```

本切片不宣称 BETA-01 完成。wire/runtime 字段仍暂名为 `seed`，但新值已经是
canonical UUID；旧目录迁移、字段改名和最终删除兼容层仍未完成。

## 2. 已落地

### 2.1 Canonical identity 预分配

`crates/qaqh-session/src/canonical/identity.rs` 新增：

- `CanonicalSessionIdentity::install()`：安装预分配 identity；
- 同一 identity 重复安装幂等；
- 目录已有不同 identity 时稳定返回 `Conflict`；
- `open_or_create()` 保持旧语义：已有合法 sidecar 直接复用。

### 2.2 SessionManager canonical 创建路径

`crates/qaqh-session/src/manager.rs` 新增：

- `allocate_session(cwd)`：创建并索引普通 canonical session；
- `allocate_agent_session(cwd)`：创建不进入普通 session index 的子代理会话；
- 目录使用 `SessionId`，identity sidecar 在 meta 前落盘；
- `meta.seed` 写入同一个 canonical id；
- 普通会话 `ephemeral=false`；unindexed 子代理初始 `ephemeral=true`。

原有 `persist_new_session_if_absent*` 保留给 legacy/测试路径，行为不变。

### 2.3 生产调用切换

以下路径已改用 canonical allocation：

- `QaqhService::session.new`
- `QaqhService` 进程内 `SubagentHost::spawn_subagent`
- `subagent.spawn` service action
- 首次用户输入/系统注入的自动建会话
- corrupt-session 恢复回退路径

## 3. 验收证据

已通过：

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --offline -- -D warnings
cargo test -p qaqh-session --offline -- --test-threads=1
cargo test -p qaqh-runtime --offline -- --test-threads=1
cargo test --workspace --offline -- --test-threads=1
```

新增回归覆盖：

- 预分配 identity 安装幂等与冲突 fail-closed；
- `allocate_session` 的 seed、目录、identity、meta 一致；
- `allocate_agent_session` 不污染普通 session index；
- host direct spawn 满足 `child_session_id == seed`；
- canonical child 目录仍可写 `SessionCreated.parent_session_id`。

## 4. 未决项

- `SessionMeta.seed`、`RingingEventEnvelope.seed`、runtime key 仍是字段名
  `seed`；值已经统一为 canonical id，但字段重命名尚未开始。
- 旧 `{legacy_seed}` 目录仍需原子迁移、journal、幂等重试和崩溃恢复。
- `generate_seed()` 仍保留给无 manager 的 ephemeral/测试路径；生产创建路径已
  不再使用。
- `generate_unique_session_seed()` 已无生产调用方，可在下一清理 PR 删除。
- TUI/WinUI 的 seed 长度/格式假设尚未全面回归。

## 5. 接手注意事项

- 不得回退到“先建 8 位目录，再在目录内生成 UUID”。
- 子代理 V2 的不变量是 `child_session_id == seed == directory`。
- `canonical-identity.json` 的 `session_id` 必须与目录名一致；`log_id` 保持独立。
- 旧目录读取只能走 legacy resolver，不得进入新 graph、mailbox 或 Team projection。
- 下一步优先实现旧目录 resolver/迁移，再改 wire/runtime 字段名，最后删除 seed 兼容层。
