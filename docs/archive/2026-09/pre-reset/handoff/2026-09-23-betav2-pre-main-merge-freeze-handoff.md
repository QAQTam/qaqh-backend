# betav2 合并窗口前封仓 Handoff

冻结时间：2026-09-23 23:38 CST  
封仓范围：`main <- betav2` 合并窗口前的全部已完成工作与未完成工作  
封仓前集成基线（本 handoff 的父提交）：`85c8c7b8f5683d0c9e043db295bac4f1717681aa`  
当前 `main`：`87e709bd8ccd1327bde2ea49ef516d5db937dafd`  
合并关系：`main` 是 `betav2` 的祖先，可 fast-forward  
open PR：`betav2` 目标 `0`，`main` 目标 `0`  
本地 worktree：仅主仓 `/home/qaqtamsy/项目/qaqh-backend`  
本地 branch：`betav2`、`main`

> 本 handoff 是封仓记录。0:00 的 `main <- betav2` 应以此为最终集成基线；
> 之后的 P4-2b、P4-3、P5 实现等均视为封仓后工作，不应再混入本次合并窗口。

## 1. 本次合并窗口已包含

### 1.1 版本与发布基线

- 版本链回滚到 `2.0.0-alpha1`：PR #309。
- `qaqh-backend.lock.json` 的 `git_commit` 固定到 alpha1 回滚合并：PR #311。
- `QAQH_UA_VERSION` / `QAQH_USER_AGENT` 按既有发布纪律保留主版本 `2.0.0`。

### 1.2 P3 ToolRuntime / ToolLedger / recovery

已完成并进入 `betav2`：

- ToolRuntime 统一 admit/schedule/execute/progress/cancel/audit 边界。
- 全部内置工具 typed output 迁移 Wave 1-6。
- canonical `events.jsonl` ToolLedger：`ToolIntent` / `ToolFinished` durable barrier。
- SessionActor 的 resume/terminal/cancel 原子 CAS。
- interaction `Requested/Resolved/Expired` first-answer-wins。
- recovery intent/executor、`SessionRecovered` batch marker。
- `tool_outbox` 停止写入，恢复改走 canonical intent/terminal。
- P3 的迁移收尾与 TUI 契约测试钩子已完成。

P3-6 的旧未提交工作面已归档，不属于封仓基线：

- `/home/qaqtamsy/项目/qaqh-backend-p3-6-dirty.patch`
- `/home/qaqtamsy/项目/qaqh-backend-p3-6-untracked.tgz`

### 1.3 P4 Policy / Sandbox 已完成部分

- A1：write/exec/net 在 handler 前写 durable v2 `tool_intent`；失败返回
  `AUDIT_UNAVAILABLE`，不执行 handler。
- A2：result audit 写失败进入 quarantine/emergency sink，返回
  `AUDIT_QUARANTINED`，并阻止后续 write/exec/net。
- `qaqh-policy` 抽出核心词汇：
  `ToolCategory` / `PermissionRisk` / `PermissionLevel` /
  `PermissionDecision` / `SandboxSpec`。
- Linux sandbox 第一刀：
  - bwrap 与 Landlock/seccomp 两条后端；
  - `Auto` 优先 bwrap、回退 helper；
  - workspace-write、network deny、denial 埋点；
  - Windows/macOS 不因缺少 Linux 后端拒绝启动。
- `ToolCallContext` 正式携带显式 `SandboxSpec` 字段：PR #316。
- ApprovalRegistry 第一刀：PR #320。
  - pending/resolved 状态分离；
  - first-answer-wins；
  - 重复 permission response 返回 `interaction_already_resolved`；
  - 不产生第二个 grant，不二次执行工具。

### 1.4 合并窗口前收口修复

- #318：timeline seq gap 与 seal 裁剪修复。
- #319：sync 请求 `.send()` 移入 `block_on`，修复无 runtime 线程 panic。
- TUI Ringing v2 冻结语义：PR #317。
  - tag：`tui-ringing-v2-frozen-2026-09-23`
  - 指向：`b40ff698f4211526f139c8a620cf159dd4ef9542`
- TUI 侧锚定 issue：`QAQ-Harness/qaqh-tui-app#46`。
- 本地 anchor / 旧 P3 worktree 已移除；已合入 `betav2` 的本地 branch 已清理。

### 1.5 当前验证口径

封仓前已完成：

```text
cargo test --workspace -- --test-threads=1 PASS
cargo clippy --workspace --all-targets -- -D warnings PASS
cargo check --workspace --all-targets PASS
```

合并 #318/#319 后，针对受影响 crate 已复跑：

```text
cargo test -p qaqh-runtime --test timeline_rebuild -- --test-threads=1 PASS
cargo test -p qaqh-gate --test gate_test -- --test-threads=1 PASS
cargo clippy -p qaqh-runtime -p qaqh-gate --all-targets -- -D warnings PASS
```

已知非阻断：`cargo fmt --all -- --check` 仍只报告未改动的
`crates/qaqh-client/src/lib.rs` 与 `crates/qaqh-client/src/types.rs` 既有差异。

## 2. 封仓时仍未完成

### 2.1 P4-2b：canonical interaction / amend / escalation

- permission / ask / plan 统一接入 canonical `InteractionRegistry`。
- `InteractionRequested/Resolved/Expired` 的跨重启 replay 完整性。
- 一次性 grant 的 registry-backed 生命周期与恢复语义。
- amend rule：将“记住决定”落为显式 amend，而不是复制一次性凭证。
- sandbox denial 后的一次性 escalation approval。
- 审批重放与 first-answer-wins 的完整 Gate 矩阵。

### 2.2 P4-3：Linux sandbox hardening

- bwrap 读白名单。
- `.git/hooks` 只读。
- 私有 `/tmp` / `TMPDIR`。
- cgroup v2 CPU / memory 限制。
- 更强资源上限与 process limits。
- 读/写边界、symlink/device/FIFO/TOCTOU 完整矩阵。

### 2.3 P4-4：policy engine 完整抽离

- `needs_permission()`、路径 helper、`TrustedFolderSet` 迁入 `qaqh-policy`。
- 四级档位改为规则 preset。
- 所有工具通过显式 `ToolCallContext` 获取 workspace/mode/grants。
- 规则输出统一为 allow/deny/ask/amend。

### 2.4 P4-5：audit 收口

- 180 天留存、分段压缩与容量告警。
- segment manifest 与链头锚点。
- Ed25519 每日段签名与公钥导出。
- `qaqh-daemon audit verify/query/export`。
- process/net/mcp/skill 对象扩展与子代理血缘字段。
- audit chain/torn/rotate 的完整 Gate。

### 2.5 P5：Ringing v2 后端实现

语义已冻结，运行时代码尚未开工：

- `qaqh-ringing` v2 wire 类型、cursor token 编解码、schema 常量。
- `qaqh-client` v2 open/subscribe/bootstrap/reset/interaction API。
- daemon `/ringing/v2` 端点与 canonical projection replay。
- v1 `Last-Event-ID` → v2 cursor 映射。
- v2 fixture、TUI/Windows alpha 共用验收。
- TUI `SessionModel` reducer、rebaseline、interaction 幂等。

冻结语义与 TUI 跟进入口：

- `docs/spec/2026-09-23-TUI-Ringing-v2冻结语义-spec.md`
- tag `tui-ringing-v2-frozen-2026-09-23`
- `QAQ-Harness/qaqh-tui-app#46`

### 2.6 P6：清理与 composition root

- 删除三频道 journal、latest checkpoint、独立 timeline 快照、offload 重复存储。
- 删除 legacy service JSON、旧 tool display 解析和 thread-local 路径。
- 旧目录迁移/归档与 consumer 判据收口。
- 评估单 binary embedded/remote composition root。

## 3. 合并窗口操作

推荐：

```bash
git switch main
git pull --ff-only origin main
git merge --ff-only origin/betav2
git push origin main
```

不要移动以下不可变 tag：

```text
tui-anchor-2026-09-23
tui-anchor-2026-09-23-p3
tui-anchor-2026-09-23-p3-typed-tools
tui-anchor-2026-09-23-v2.0.0-rc
tui-ringing-v2-frozen-2026-09-23
```

## 4. 封仓后优先级

1. P4-2b：canonical interaction / amend / sandbox escalation。
2. P4-3：Linux 读隔离与 cgroup。
3. P5-1：Ringing v2 wire 类型与 cursor token。
4. P5-2：qaqh-client v2 与 daemon v2 端点。
5. P4-4/P4-5：policy engine 与 audit 收口。
6. P6：存储清理与 composition root。

## 5. 封仓结论

- `betav2` 封仓前集成基线为 `85c8c7b`；本 handoff 提交后，`main <- betav2`
  应包含 handoff 提交本身。
- 所有 open PR 已清空。
- 已完成工作均已在 `betav2`，未完成工作已在本 handoff 登记。
- 本地 worktree 已清理，仅保留主仓。
- `main <- betav2` 可 fast-forward。

## 6. main 合并后执行登记

`main <- betav2` 已通过 PR #321 合并，`main` 当前提交为
`e0753baca66d557f393de9bb020911a3a200d050`。

本地合并后状态：

- 本地 branch 仅保留 `main`，`main` 与 `origin/main` 一致。
- 本地 worktree 仅保留主仓 `/home/qaqtamsy/项目/qaqh-backend`。
- 已删除本地 `betav2` 分支。
- 已移除 detached TUI anchor worktree；冻结 tag 仍保留。
- 前后端同步进入 `v2.0.0-alpha1`，等待团队负责人下一步分工。

原计划接下来的执行顺序仍以第 4 节为准：

1. **P4-2b：canonical interaction / amend / escalation**
   - permission、ask、plan 统一接入 `InteractionRegistry`；
   - 跨重启 replay、一次性 grant、amend rule；
   - sandbox denial escalation；
   - first-answer-wins 完整 Gate 矩阵。
2. **P4-3：Linux sandbox hardening**
   - bwrap 读白名单、`.git/hooks` 只读、私有 `/tmp` / `TMPDIR`；
   - cgroup v2 CPU / memory limits；
   - symlink/device/FIFO/TOCTOU 完整边界矩阵。
3. **P5-1：Ringing v2 wire 类型与 cursor token**
4. **P5-2：qaqh-client v2 与 daemon `/ringing/v2` 端点**
5. **P4-4 / P4-5：policy engine 与 audit 收口**
6. **P6：存储清理与 composition root**

执行起点原定为 **P4-2b**，不是直接开始 P5。P5 的语义当时虽已冻结，但运行时
代码尚未开工。

后续记录（2026-09-24）：TUI 侧提权后，实际执行顺序调整为
P5-1/P5-2（v2 wire + client）→ P0-3（daemon canonical 最小闭环）→
P0-4 第一刀（interaction causation）。本节保留的是调整前的原计划记录。
