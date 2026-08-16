# QAQ-Harness 1.1 工程师交接文档

> 日期：2026-08-16
> 交接范围：后端主仓库 `QAQTam/QAQ-Harness` + 前端仓库 `QAQTam/qaqh-winui-app`
> 当前主线：`origin/main = da31082`（所有本阶段 PR 均已合并，无 open PR）

---

## 0. 给下一位工程师的 5 分钟摘要

1. 后端 repo 已从 `deepx-*` 更名为 `qaqh-*`；WinUI 前端在独立仓库 `qaqh-winui-app`。
2. 必读三份文档：`REFACTOR-ROADMAP.md`、`bugfix-1.1-nextdev.md`、`EXPLORE-REFACTOR-1.1.md`。
3. 本阶段已完成并合并 4 个后端 PR（#3/#4/#5/#6）和 1 个前端 PR（qaqh-winui-app#1）。
4. 下一步推荐：先做 **SMJ 编辑日志**（低风险），再做 **刀 1 子代理 in-process**（高价值/高风险）。
5. 后端所有测试在 Arch Linux 可跑；WinUI 只能在 Windows 编译，当前前端仓库无 CI。
6. 写代码前必须使用 `git worktree`，不要直接在 `main` 上改。

---

## 1. 仓库与工作环境

| 仓库 | 地址 | 角色 |
|---|---|---|
| `QAQ-Harness` | https://github.com/QAQTam/QAQ-Harness | 跨平台 Rust 后端 monorepo（daemon / loop / SDK / tools） |
| `qaqh-winui-app` | https://github.com/QAQTam/qaqh-winui-app | WinUI3 桌面壳 / installer / updater（Windows） |

当前机器为 Arch Linux。后端可用 bash / cargo / gh / git / codegraph。

### 常用命令

```bash
# 后端主仓库
cargo check --workspace
cargo test -p qaqh-config -p qaqh-runtime -p qaqh-session -p qaqh-msglp

# 个别测试依赖 qaqh-workspace 二进制
cargo build -p qaqh-workspace --bin qaqh-workspace
cargo test -p qaqh-msglp --test permission_lifecycle

# codegraph（AGENT.md 要求）
export PATH="/home/qaqtamsy/.local/bin:$PATH"
codegraph status
codegraph query "<symbol>"
codegraph callers "<symbol>"
```

### 使用 git worktree 的标准流程

```bash
cd /home/qaqtamsy/桌面/project/QAQ-Harness
git fetch origin --prune
git worktree add -b <branch> /home/qaqtamsy/桌面/project/qa-worktrees/QAQ-Harness-<suffix> origin/main
cd /home/qaqtamsy/桌面/project/qa-worktrees/QAQ-Harness-<suffix>
# ... 开发 + 测试 ...
git push -u origin <branch>
gh pr create --base main --head <branch> --title "..." --body-file /tmp/pr-body.md
gh pr merge <pr-number> --squash --delete-branch
git worktree remove /home/qaqtamsy/桌面/project/qa-worktrees/QAQ-Harness-<suffix>
```

注意：

- `gh pr merge --delete-branch` 偶尔会因 worktree 占用删除本地分支失败，需要
  `git worktree remove <path>` 后再 `git push origin --delete <branch>`。
- 同一 GitHub 账号不能 approve 自己的 PR，只能 `COMMENTED` 或直接合并。
- 主工作树切回 `main` 后再删除 worktree，避免分支占用。

---

## 2. 本阶段已完成的工作

所有 PR 均已 squash merge。

| PR | Commit | 内容 |
|---|---|---|
| QAQ-Harness #3 | `1862773` | `qaqh_types::tool_mode` 单契约；工具注册器聚合 `agent_tool_registrars()`；清理假 Engine 插件文档。修 BUG-013/部分 BUG-009。 |
| QAQ-Harness #4 | `602b1e0` | `timeline_rebuild` 模块：timeline 文件缺失/损坏时从 `messages.jsonl`/compact-context 重建并写回。修 BUG-006。 |
| QAQ-Harness #5 | `49f6d97` | compact-context 损坏或 archive 边界越界时 fail-closed，禁止回退全量历史。修 BUG-007。 |
| QAQ-Harness #6 | `da31082` | `qaqh_config::Config::update` 受锁单写口；daemon 六个 config 写路径收敛；统一 `AgentReloadConfig` 广播。修后端 BUG-008/部分 BUG-001。 |
| qaqh-winui-app #1 | 前端 main | `bridge.rs` 6429 行拆成 `bridge/` 模块；BUG-004 viewport 恢复移除，恢复会话强制贴底。已 review 并合并。 |

### 新增的关键 API / 模块

- `qaqh_types::tool_mode`
  - `KNOWN_MODES` / `preset_tools` / `is_known` / `is_minimal_family`
  - `model_tool_name` / `internal_tool_name`（`bash_v2 ↔ bash` 投影）
- `qaqh_config::Config::update`
  - 唯一受锁配置写入口：`Config::update(|cfg| { ...; Ok(()) })`
  - mutator 返回 `Err` 时不写盘
  - `load` / `save` 共用进程级 config I/O lock
- `qaqh_session::SessionManager::try_global`
  - 非 panic 访问器，供 timeline 重建等可选恢复路径使用
- `qaqh_runtime::ringing::timeline_rebuild`
  - `rebuild_timeline_snapshot(seed)`
  - `timeline_snapshot_from_turns(seed, &turns)`

---

## 2.1 本轮待合并 PR（SMJ 编辑日志基础版）

- 分支建议：`refactor/knife2-smj-editing-log`
- 内容：
  - 新增 `qaqh-workspace::journal`：`steps.jsonl` + `blobs/<sha256>`
  - 接入 `write` / `edit` / `apply_patch` / `delete` / `copy_range` / `web_fetch(output=...)`
  - 新增 `journal` workspace 工具与 `qaqh-workspace journal query|replay|export` CLI
  - `journal export --format patches` 可导出 unified diff 补丁集
- 验证：
  - `cargo check --workspace`
  - `cargo test -p qaqh-workspace`
  - `cargo test -p qaqh-msglp --test permission_lifecycle`
- 状态：待 push / 待开 PR

## 2.2 下一步（刀 2 剩余 / 刀 1）

- 刀 2 剩余：把 timeline / messages / compact / ringing journal 四套持久化收敛为一条事件溯源日志；
  SMJ 编辑日志已作为前置小步落地，可作为同一事件流的 workspace 侧事实日志。
- 刀 1：子代理 in-process 化，移除 `spawn_subagent` 的 `Command::new(current_exe)` 与三层连接。
- 刀 7：统一注入插槽（InjectionBus），对 loop 动刀，收敛 subagent / skills / MCP / 系统 / 用户注入路径。

## 3. 当前 Bug 状态表

| Bug | 状态 | 说明 |
|---|---|---|
| BUG-001 | **部分修复** | 后端 config 写路径已收敛（#6）；WinUI 本地 `ui-preferences.json` / 本地 theme 仍在前端仓库 |
| BUG-002 | open | 子代理仍是完整 `qaqh agent --seed` 子进程 + 三层连接 |
| BUG-003 | fixed | WinUI vsync/队列/resume generation 修复已在前端仓库 main |
| BUG-004 | fixed | 前端 #1 移除 viewport 恢复，恢复=贴底；active guard/resume generation 此前已修 |
| BUG-005 | open | timeline / messages / compact / journal 四套持久化仍并存 |
| BUG-006 | **fixed** | timeline 丢失或损坏时从消息重建 |
| BUG-007 | **fixed** | compact 损坏/越界 fail-closed |
| BUG-008 | **fixed（后端）** | `Config::update` 单写口 |
| BUG-009 | **部分修复** | 误导性 Engine 插件文档已清理；实质注入统一待刀 7 |
| BUG-010 | open | 大文件/crate 碎片化仍待刀 4/5/10 |
| BUG-011 | open | 与 BUG-002 同源：进程成本、多写、同步持久化、手写 HTTP |
| BUG-012 | fixed | `session.set_tool_mode` 白名单已放行 |
| BUG-013 | **fixed（后端）** | `qaqh_types::tool_mode` 单契约；前端需跟进消费 |

---

## 4. 下一步工作建议

### P0：前端跟进（交给前端工程师）

- `qaqh-winui-app` 增加 `qaqh-types` 依赖，消费 `qaqh_types::tool_mode`
- 删除 `apps/winui/src/bridge/types.rs` 本地 `TOOL_MODE_VALUES`
- 给前端仓库接 Windows CI；当前 #1 这种 6.7k 行移动无 CI 保障

### P1：SMJ 编辑日志（基础版已落地）

- 依据：`docs/nextdev/SESSION-JOURNAL-PLAN.md`
- 目标：所有 workspace 写路径收口后追加 `journal/steps.jsonl` + content-addressed blobs
- 入口：
  - `crates/qaqh-workspace/src/file_edit_v2.rs`
  - `crates/qaqh-workspace/src/apply_patch.rs`
  - `crates/qaqh-workspace/src/file_mutate.rs`
  - 其它写/删除路径按 `SESSION-JOURNAL-PLAN.md §3.3`
- 判据：
  1. 模拟误删仓库后可 `journal replay` 恢复
  2. 恢复内容字节一致
  3. 模型能通过工具查询/恢复
- 风险：低；append-only，不碰协议/前端

### P1：刀 1 子代理 in-process（高价值，建议分多步）

- 修 BUG-002/011
- 关键文件：
  - `crates/qaqh-runtime/src/registry.rs`（`spawn_subagent` 仍在 `Command::new(current_exe)`）
  - `crates/qaqh-runtime/src/worker.rs`
  - `crates/qaqh-msglp/src/ringing_v1/loop_core.rs`
  - `crates/qaqh-subagent/src/lib.rs`
- 目标：
  - `spawn_subagent` 不再创建进程
  - 主 loop + N 子代理 actor 在同一 daemon 进程
  - 保持 workspace 边界 + 权限级别作为隔离
- 风险：高，需要保留 Ringing worker wire 语义或先抽 actor trait

### P2：刀 3 传输现代化

- 文件：`crates/qaqh-daemon/src/ringing_http.rs`（约 110KB）
- 目标：换 axum 或先按端点拆模块
- 判据：现有 Ringing 行为测试全过

### P2：刀 2 剩余 — 四套持久化收敛为事件日志

- BUG-005 根治
- 关键文件：
  - `crates/qaqh-runtime/src/timeline_store.rs`
  - `crates/qaqh-runtime/src/ringing/hub.rs`
  - `crates/qaqh-runtime/src/ringing/journal_store.rs`
  - `crates/qaqh-session/src/manager.rs`
  - `crates/qaqh-message/src/store.rs`
- 风险：高，需要迁移脚本 + 投影重建测试

---

## 5. 已知坑与约束

1. **Linux 不能编译 WinUI**：`qaqh-winui-app` 依赖 windows-reactor vendor，
   在 Linux 上 `windows-composition` 直接报 `windows_time::TimeSpan` 缺失。
   前端 PR 的验证必须在 Windows 做。
2. **repo 不是全局 rustfmt-clean**：直接 `cargo fmt --all` 会引入大量无关 diff。
   新文件用 `rustfmt --edition 2024 <file>`；老文件只手动格式化改动块。
3. **clippy 有历史失败**：workspace lints deny `unwrap_used` / `string_slice`，
   `qaqh-session`、`qaqh-runtime` 现存历史代码 clippy 失败。不要把历史 clippy
   failure 当作新 PR 的回归；新代码避免新增 unwrap/string slice。
4. **`permission_lifecycle` 测试需要先构建 `qaqh-workspace` 二进制**，否则报
   `serve binary missing`。
5. **主历史包含 `f` 提交**：`60b69ba`（apps/web 连接失败横幅、daemon-web.ps1
   防火墙规则）已进入 `origin/main` 历史，是 PR #3 的父提交。接手时不要
   以为它未提交或丢失。
6. **`AGENT.md` 仍 untracked**：位于主仓库根目录，内容是 codegraph CLI 使用
   提示；如果不需要，接手者应决定提交或删除。
7. **`docs/nextdev/bugfix-1.1-nextdev.md` 是档案 + 现状混合**：旧条目保留
   `deepx-*` 路径；新增状态在文末“落地增量”区。不要大改旧记录，追加即可。
8. **GitHub 无 CI**：目前所有 PR 均无 checks；合并前必须本地跑测试。

---

## 6. 提交规范与验收清单

建议 PR 前检查：

- [ ] `cargo check --workspace`
- [ ] 相关 crate 全量测试
- [ ] 新文件 rustfmt
- [ ] `git diff --check`
- [ ] 更新对应 BUG 状态 / roadmap 落地说明
- [ ] 使用 worktree 隔离，不直接在 `main` 上开发
- [ ] PR base = `main`，head branch 命名含 bug/knife 编号

分支命名建议：

- `refactor/knifeN-<topic>`
- `fix/bugXXX-<topic>`
- `docs/<topic>`

---

## 7. 相关文档索引

- `docs/nextdev/REFACTOR-ROADMAP.md` — 重构方向、七刀、判据
- `docs/nextdev/bugfix-1.1-nextdev.md` — BUG-001~013 档案
- `docs/nextdev/EXPLORE-REFACTOR-1.1.md` — 文档↔qaqh-* 仓库映射、keep/remove/merge
- `docs/nextdev/SESSION-JOURNAL-PLAN.md` — SMJ 编辑日志（基础版已落地）
- `docs/nextdev/legal/` — 用户协议/隐私政策，保留不动
- `docs/backlog/` — 旧计划归档，按需回溯
