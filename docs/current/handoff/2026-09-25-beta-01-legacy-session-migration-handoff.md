# BETA-01 Legacy Session Migration Handoff

> 日期：2026-09-25
> 基线：`0291f59`（`main`）
> 实现分支：`feat/beta-01-legacy-session-migration-20260925`
> 状态：resolver 与可恢复目录迁移实现完成，待 PR 审核
> 范围：`qaqh-session`、`qaqh-runtime` 启动装配

## 1. 本次结论

BETA-01 的第二段已经落地：

```text
sessions/{legacy_seed}
  -> journal
  -> sessions/{session_id}
  -> meta/index/active/workspace repair
  -> legacy_seed alias retained read-only
```

迁移在 `SessionManager::init` 启动早期执行，发生在 actor、hub、lease 和 runtime
key 建立之前。重复执行幂等，rename 前后崩溃均可恢复。

## 2. 已落地

### 2.1 Legacy resolver

`SessionManager::canonical_identity_for_seed()`：

- 旧目录仍存在时直接读取其 canonical identity；
- 目录已迁移时读取 `.legacy-session-ids.json` alias；
- alias 指向缺失、identity 不匹配时 fail closed；
- resolver 不创建 identity，不写入 canonical facts。

### 2.2 可恢复目录迁移

新增：

- `.identity-migration.json`：rename 前写 journal，完成后清除；
- `.legacy-session-ids.json`：持久化 `legacy_seed -> session_id` 只读 alias；
- `migrate_legacy_session_dirs()`：启动时扫描并迁移旧目录；
- `apply_identity_migration()`：rename 或续跑已完成 rename，然后修复：
  - `meta.seed`
  - session index remove/upsert
  - active session marker
  - workspace 账户

崩溃恢复覆盖：

```text
rename 前崩溃       -> journal 保留，下次重放 rename
rename 后崩溃       -> target 已存在，下次只做 repair
两者都存在          -> fail closed，不合并目录
```

### 2.3 WorkspaceStore

新增 `WorkspaceStore::rename_session()`：

- 保留原 workspace 内的会话顺序；
- 新 id 已存在时删除旧 id，避免重复账户。

## 3. 验收证据

已通过：

```text
cargo fmt --all -- --check
cargo check -p qaqh-session --all-targets --offline
cargo test -p qaqh-session --lib seed_collision_tests:: --offline -- --test-threads=1
```

新增测试覆盖：

- legacy 目录迁移到 canonical 目录后：
  - `meta.seed` 更新；
  - index 删除旧 seed、写入新 id；
  - active marker 更新；
  - 旧 seed 仍可经 alias resolver 读取；
- 第二次迁移为 no-op；
- 模拟 rename 已完成后崩溃，启动恢复完成 meta/alias 修复并清空 journal。

## 4. 未决项

- 迁移只覆盖 session storage、index、active 和 workspace 账户；
  Ringing/lease/driver/quota/content 的 runtime key 改名仍属于 Phase D。
- 旧会话首次打开时才生成 identity 的场景，需要下一次启动才迁移目录。
- `.legacy-session-ids.json` 尚无删除日期；Phase E 删除 legacy resolver 时一并删除。
- TUI/WinUI 仍可能假设 seed 是 8 位 hex。

## 5. 接手注意事项

- 不得把 legacy alias 写入 canonical facts、graph、mailbox 或 Team projection。
- 迁移冲突必须 fail closed，不得删除或合并任一会话目录。
- journal/alias 都是迁移元数据，不是 session fact source。
- 下一步是 Phase D：wire/runtime 字段和 key 从 `seed` 改名为 `session_id`。
