# QAQ-Harness 1.1 工程师交接文档

> 日期：2026-08-18
> 交接范围：后端主仓库 `QAQTam/QAQ-Harness` + 前端仓库 `QAQTam/qaqh-winui-app`
> 当前主线：以后端 `origin/main` HEAD 为准；截至本轮检查：后端 HEAD = `fd77452`（#16/#17/#19/#21 已合并），open PR 为 #20（docs T17），open issues 为 #9、#15、#18；前端最新 `origin/main` 已含 #10/#13，open issues 为 #11、#12。

---

## 0. 给下一位工程师的 5 分钟摘要

1. 后端 repo 已从 `deepx-*` 更名为 `qaqh-*`；WinUI 前端在独立仓库 `qaqh-winui-app`。
2. 必读三份文档：`REFACTOR-ROADMAP.md`、`bugfix-1.1-nextdev.md`、`EXPLORE-REFACTOR-1.1.md`。
3. 本阶段后端 PR #3/#4/#5/#6/#8/#10/#16/#17/#19/#21 及 handoff docs #7/#11/#12/#13/#14 已合并；前端 #1–#10、#13 已合并。后端当前 open issues：#9（config theme/notifications 前端仍待闭环）、#15（同轮 text↔tool 块重复回放，排期刀 1 step 2）、#18（Windows WIP review，当前 `7c3b0b3` 已确认干净合并+编译通过）；后端 open PR：#20（docs T17）。前端 open issues：#11/#12（data-root 迁移共享权威契约消费）。
4. SMJ 编辑日志已完成；刀 1 已开始分步落地：本轮 step 1 把子代理 worker 从 `Command::new(current_exe)` 子进程改为 daemon 内 Loop actor，主 agent 进程化与逐 actor 隔离仍待后续 step。
5. 后端所有测试在 Arch Linux 可跑；WinUI 只能在 Windows 编译，前端已于 #7 接入 Windows CI（`cargo check/test -p qaqh-winui`）。
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

下表中除特殊注明外，其余 PR 均已 squash merge。

| PR | Commit | 内容 |
|---|---|---|
| QAQ-Harness #3 | `1862773` | `qaqh_types::tool_mode` 单契约；工具注册器聚合 `agent_tool_registrars()`；清理假 Engine 插件文档。修 BUG-013/部分 BUG-009。 |
| QAQ-Harness #4 | `602b1e0` | `timeline_rebuild` 模块：timeline 文件缺失/损坏时从 `messages.jsonl`/compact-context 重建并写回。修 BUG-006。 |
| QAQ-Harness #5 | `49f6d97` | compact-context 损坏或 archive 边界越界时 fail-closed，禁止回退全量历史。修 BUG-007。 |
| QAQ-Harness #6 | `da31082` | `qaqh_config::Config::update` 受锁单写口；daemon 六个 config 写路径收敛；统一 `AgentReloadConfig` 广播。修后端 BUG-008/部分 BUG-001。 |
| qaqh-winui-app #1 | 前端 main | `bridge.rs` 6429 行拆成 `bridge/` 模块；BUG-004 viewport 恢复移除，恢复会话强制贴底。已 review 并合并。 |
| qaqh-winui-app #2–#5 | 前端 main | `round_renderer` / `chat_view` / `settings_view` / `composer_bar` 拆成模块目录。已合并。 |
| qaqh-winui-app #6 | `05601cd` | 前端消费 `qaqh_types::tool_mode` 单契约，移除本地 `TOOL_MODE_VALUES`（BUG-013 前端跟进）。已合并。 |
| qaqh-winui-app #7 | `ef396b1` | 新增 Windows CI workflow（`cargo check/test -p qaqh-winui`）。已合并。 |
| qaqh-winui-app #8 | `3c0c8e8` | tool-mode ComboBox 映射集中化并覆盖 BUG-014 持久化 guard。已合并。 |
| qaqh-winui-app #9 | `252b522` | 清理 WebView/renderer 残留注释。已合并。 |
| qaqh-winui-app #10 | `1799528` | legacy `DeepX` data-root marker 迁移、通知 Insights 资源 DLL 打包补齐、版本锁改读后端仓库。已合并。 |
| qaqh-winui-app #13 | `1f094eb` | `main.rs` 多 timer 收敛为单一 50ms UI pump（T15）。已合并。 |
| QAQ-Harness #16 | `4319ed2` | config theme/notifications + `qaqh_types::platform` 权威 data-root 迁移 API（issue #9 / qaqh-winui-app#11 后端部分）。已合并。 |
| QAQ-Harness #17 | `d0fce1e` | 刀 1 step 2a：主 session loop 改为 daemon in-process actor，移除 `Command::new(current_exe)` / `AgentTransport::Process`。已合并。 |
| QAQ-Harness #19 | `8623efb` | 刀 1 step 2b：退役 `worker.rs` / `logger.rs` / `qaqh agent` CLI 入口。已合并。 |
| QAQ-Harness #21 | `fd77452` | 刀 1 step 2c：`qaqh-workspace` 增加 actor 级 workspace/session/cancel 上下文。已合并。 |
| QAQ-Harness #8 | `eb8ceac` | SMJ 编辑日志基础版：`journal` 模块 + workspace 工具 + CLI；接入 write/edit/apply_patch/delete/copy_range/web_fetch；支持 query/replay/export patches。 |
| QAQ-Harness #10 | `2dc02d1` | 刀 1 step 1：`LoopChannels` / `Loop::from_channels` 通道化 loop；`AgentRegistry::spawn_subagent` 不再 `Command::new(current_exe)`，改为 daemon 内 Ringing Loop actor + 直接 Hub 发布；actor 私有 ToolManager；新增回归测试。 |

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
- `qaqh_msglp::ringing_v1::loop_core::LoopChannels` + `Loop::from_channels`
  - Ringing loop 与 OS pipe 解耦；同一 loop 可跑在 in-process channel actor 上
- `qaqh_workspace::runtime::install_actor_tool_manager` / `clear_actor_tool_manager`
  - in-process actor 临时私有 ToolManager，不污染 daemon `skills.list_tools` 快照
- `qaqh_types::platform::migrate_legacy_data_root_marker` / `migrate_legacy_data_root_marker_at`
  - 安全迁移旧 `DeepX` data-root marker；`ensure_data_root` 启动路径自带迁移
- `qaqh_workspace::set_actor_context` / `clear_actor_context` / `current_workspace` / `current_session` / `set_cancel` / `is_cancel`
  - thread-local actor 上下文；`set_workspace` / `set_current_session` 在 actor 内写 thread-local

---

## 2.1 本轮已合并 PR（SMJ 编辑日志基础版）

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
- 状态：已合并（PR #8）

## 2.2 已合并 PR（刀 1 step 1：子代理 in-process actor）

- 分支建议：`refactor/knife1-subagent-inprocess-step1`
- 内容：
  - `LoopChannels` / `Loop::from_channels`：loop 的 command/event 边界从 OS pipe 抽象为 typed channel，`new_ipc` 复用同一实现
  - `AgentRegistry::spawn_subagent` 不再启动 `qaqh agent --seed` 子进程，改为在 daemon 线程内运行子代理 `Loop`
  - 子代理事件经 channel reader 直接 `RingingHub` publish；`send_ringing` / `close` / `shutdown_all` 全路径兼容新 transport
  - `qaqh_workspace::runtime` 增加 actor-private ToolManager（daemon 快照保持稳定）
- 边界（step 1 刻意收敛）：
  - `qaqh-workspace` 仍是进程级全局状态，因此多个 in-process 子代理 actor 经 `SUBAGENT_ACTOR_SERIAL` 串行运行；主 agent 仍为原 worker 进程
  - `qaqh-subagent` 工具仍经 daemon action + SSE 收发；HTTP 三层已减为“父 worker → daemon → actor”一层边界
- 验证：
  - `cargo check --workspace`
  - `cargo test -p qaqh-runtime`
  - `cargo test -p qaqh-msglp`
  - 新增 `qaqh-msglp/tests/inprocess_loop.rs` 与 `qaqh-runtime/tests/subagent_inprocess.rs`
- 状态：已合并（PR #10）

## 2.3 下一步（刀 1 step 2 剩余 / 刀 2 / 刀 7）

- 刀 1 step 2：主/session 已全部 in-process（#17）、worker/`qaqh agent` 已退役（#19）、actor workspace/session/cancel 上下文已落地（#21）。
- 剩余 step 2：
  - 把 `ACTOR_TOOL_MANAGER` / `RUNTIME_CTX` 也 actor 化，随后移除 `ACTOR_SERIAL`，放开主 agent + N 子代理并发。
  - `qaqh-subagent` 从 daemon HTTP/SSE 回连改为宿主直连 actor 句柄。
- 刀 2 剩余：把 timeline / messages / compact / ringing journal 四套持久化收敛为一条事件溯源日志；
  SMJ 编辑日志已作为前置小步落地，可作为同一事件流的 workspace 侧事实日志。
- 刀 7：统一注入插槽（InjectionBus），对 loop 动刀，收敛 subagent / skills / MCP / 系统 / 用户注入路径。
- 前端团队 issue #9：后端部分已由 #16 合入；前端仍待消费 theme/notifications 契约。

## 3. 当前 Bug 状态表

| Bug | 状态 | 说明 |
|---|---|---|
| BUG-001 | **部分修复** | 后端 config 写路径已收敛（#6/#16）；WinUI 本地 `ui-preferences.json` / 本地 theme 仍在前端仓库；前端仍待消费 theme/notifications 契约（issue #9） |
| BUG-002 | **部分修复（#10/#17/#19/#21）** | 子代理不再启动进程；主 loop 已 in-process、worker 已退役、actor workspace/session/cancel 已上下文隔离；剩余 ToolManager/RuntimeContext actor 化 + `qaqh-subagent` 宿主直连 |
| BUG-003 | fixed | WinUI vsync/队列/resume generation 修复已在前端仓库 main |
| BUG-004 | fixed | 前端 #1 移除 viewport 恢复，恢复=贴底；active guard/resume generation 此前已修 |
| BUG-005 | open | timeline / messages / compact / journal 四套持久化仍并存 |
| BUG-006 | **fixed** | timeline 丢失或损坏时从消息重建 |
| BUG-007 | **fixed** | compact 损坏/越界 fail-closed |
| BUG-008 | **fixed（后端）** | `Config::update` 单写口 |
| BUG-009 | **部分修复** | 误导性 Engine 插件文档已清理；实质注入统一待刀 7 |
| BUG-010 | open | 大文件/crate 碎片化仍待刀 4/5/10 |
| BUG-011 | **部分修复（#10/#17/#19/#21）** | 子代理进程/三层连接成本已消除；多写、同步持久化、手写 HTTP 仍待刀 2/3 |
| BUG-012 | fixed | `session.set_tool_mode` 白名单已放行 |
| BUG-013 | **fixed** | `qaqh_types::tool_mode` 单契约；前端 #6 已消费并移除本地 `TOOL_MODE_VALUES` |
| BUG-014 | fixed | 前端 tool-mode ComboBox 空态误写；前端 #8 修复 |
| BUG-015 | open | 同轮 text↔tool 块重复回放（= issue #15），已排期刀 1 step 2 / loop 改造 |

---

## 4. 下一步工作建议

### P0：前端跟进（交给前端工程师）— 主体已完成

- ✅ `qaqh-winui-app` 已增加 `qaqh-types` 依赖，消费 `qaqh_types::tool_mode`（#6）
- ✅ 本地 `TOOL_MODE_VALUES` 已删除/收敛（#6/#8）
- ✅ Windows CI 已接入（#7）
- ✅ 前端 #10 / #13 已合入
- 剩余：issue #12 消费后端 `migrate_legacy_data_root_marker()` 并删除私有公式副本；issue #9 前端侧配置迁移（theme/notifications）

### P1：SMJ 编辑日志 ✅ 已合并（#8）

- 基础版已落地：`journal` 工具 + CLI + `write/edit/apply_patch/delete/copy_range/web_fetch` 接入。
- 后续仅随刀 2 事件日志统一，不再单独排期。

### P1：刀 1 子代理 in-process（step 1/2a/2b/2c 已合并，剩余继续）

- 已完成：
  - #10 step 1：子代理不再创建进程
  - #17 step 2a：主 session loop 改为 daemon in-process actor
  - #19 step 2b：退役 `worker.rs` / `logger.rs` / `qaqh agent` CLI
  - #21 step 2c：`qaqh-workspace` actor 级 workspace/session/cancel 上下文
- step 2 剩余（风险仍高）：
  - 把 `ACTOR_TOOL_MANAGER` / `RUNTIME_CTX` actor 化，随后移除 `ACTOR_SERIAL` 并发锁
  - `qaqh-subagent` 工具从 daemon HTTP/SSE 回连改为宿主直连 actor 句柄

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
6. **`AGENT.md` 已提交**：位于主仓库根目录，内容是 codegraph CLI 使用提示。
7. **临时 LAN git daemon（应急）**：本机 `~/git/qaqh-backend.git` + `git://60.0.0.32:9418`
   （`--enable=receive-pack`，无鉴权）。GitHub 恢复后可停用；如有新的 Windows WIP 可先推到此通道。
7. **`docs/nextdev/bugfix-1.1-nextdev.md` 是档案 + 现状混合**：旧条目保留
   `deepx-*` 路径；新增状态在文末“落地增量”区。不要大改旧记录，追加即可。
8. **后端 GitHub 无 CI**：后端 PR 均无 checks，合并前必须本地跑测试；前端 #7 起已有 Windows CI。
9. **刀 1 step 1 的 actor 是串行的**：`qaqh-workspace` 全局 workspace/session/sandbox/tool-manager 尚未逐 actor 化，daemon 内多个子代理 actor 由 `SUBAGENT_ACTOR_SERIAL` 排队执行。不要把“N 个子代理并发”当作已达成。

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

---

## 8. 本轮工程师工作补充（2026-08-17 第二轮）

### 8.1 本轮已完成

- QAQ-Harness #10（`2dc02d1`）：刀 1 step 1 子代理 in-process actor。
  - `spawn_subagent` 不再 `Command::new(current_exe)`。
  - `LoopChannels` / `Loop::from_channels` 通道化 loop；`new_ipc` 复用同一实现。
  - daemon 线程内运行子代理 Ringing `Loop`，事件直发 `RingingHub`。
  - actor 私有 ToolManager；daemon `skills.list_tools` 使用 `process_all_tool_names()` 保持稳定快照。
  - `shutdown_all` 先广播关闭再 join，覆盖多个排队 actor 的关闭路径。
  - 新增 `qaqh-msglp/tests/inprocess_loop.rs`、`qaqh-runtime/tests/subagent_inprocess.rs`。
- QAQ-Harness #11（`27c4967`）：把 #10 标记为已合并并同步 handoff 主线。
- QAQ-Harness #12（`00e027d`）：记录第二轮工作并确认前端 issue #9。
- 清理：删除 PR 分支与 worktree；当前 `origin/main = 00e027d`，后端无 open PR。
- 验证：`cargo check --workspace`、`cargo test -p qaqh-runtime`、`cargo test -p qaqh-msglp`、`cargo test -p qaqh-workspace --lib` 全部通过（workspace 测试存在既有并行全局态 flaky，单测重跑通过）。

### 8.2 下一轮工作（按建议优先级）

1. **backend issue #9**（低风险 config schema）：见 8.3，建议下一位直接实现并回链 issue。
2. **刀 1 step 2**：`qaqh-workspace` 逐 actor 上下文；主 agent loop 入 daemon；`qaqh-subagent` 宿主直连；`worker.rs` 退役。
3. **刀 2 剩余**：timeline / messages / compact / ringing journal 收敛为一条事件日志。
4. **P0 前端跟进**：`qaqh-winui-app` 消费 `qaqh_types::tool_mode`，移除本地 `TOOL_MODE_VALUES`，补 Windows CI。
5. **刀 3 / 刀 7**：HTTP 传输现代化；InjectionBus。

### 8.3 前端团队提交给 backend 的 issue（已确认）

- **Issue**：`QAQ-Harness#9 feat(config): add theme + notifications_enabled to PersistentConfig for BUG-001 frontend residue`
- **状态**：OPEN（已由后端本轮确认，待实现）。
- **请求**：
  - `Config` / `PersistentConfig` 增加 `theme: Option<String>`（`system | light | dark | dark-gray`；空 = 跟随系统）。
  - `Config` 增加 `notifications_enabled: Option<bool>`（缺省 = 开启）。
  - `config.save` 接受 camelCase wire 字段 `theme` / `notificationsEnabled`；`config.load` 返回这两个字段。
  - 旧 `config.toml` 缺字段保持现状兼容。
- **前端侧配套计划**（issue 中已声明）：
  - `settings_view` 主题写入改走 daemon `config.save`。
  - 通知开关改为 daemon config，移除/迁移 `ui-preferences.json`。
  - 清理 `SettingsProjection` Web 时代残留。
- **验收**：后端 `config.save/load` 接受并返回两字段；旧配置可加载；前端不再写 `ui-preferences.json`。
- **后端确认**：已通过 `gh issue list/view` 核实该 issue 是当时 backend 唯一 open issue，并已在 issue 下留确认评论（此后新增 #15）。

---

## 9. 本轮工程师工作补充（2026-08-17 第三轮）

### 9.1 仓库与前端状态快照

- 后端 `QAQ-Harness`：`main` 与 `origin/main` 同步；#13 合入后 main = `2dbfe11`；#14 仅修正本文件基线表述。工作区仅 `AGENT.md` untracked；后端无 open PR，open issues 为 #9 与 #15（#15 详见 9.4）。
- 前端 `qaqh-winui-app` 已 clone 到 `/home/qaqtamsy/桌面/project/qa-frontend/qaqh-winui-app`：
  - 本地 `main` 为 `252b522`，落后 `origin/main` 1 个提交；`origin/main = 1799528`（#10 已合并）。
  - PR #1–#10 已合并；当前无 open PR。
  - open issue 为 #11（data-root 迁移不要复制后端私有公式，需共享权威契约）。

### 9.2 前端 #10 与后端相关点

- 新增 `bridge::migrate_legacy_data_root_marker()`：把旧 `product: "DeepX"` 的 `.deepx-data-root.json` 原位改写为 `QAQ-Harness`；仅在 `canonicalRoot/ownerHome/rootId` 全部与当前用户数据根吻合时改写，任何不吻合都 fail-closed。
- 在任何 `connect_client` / daemon spawn 之前调用，解决旧品牌 marker 导致 daemon 拒绝启动 → 首屏会话/工作区/config 全挂的问题。
- `assemble-winui.ps1` 按 `Microsoft.WindowsAppRuntime.dll` SHA-256 从 windows-reactor-setup 缓存匹配并补齐 `Microsoft.WindowsAppRuntime.Insights.Resource.dll`（修通知 `Register` 0x8007007E）。
- `collect-payload-winui.ps1` / justfile 增加 `-BackendRoot ../QAQ-Harness`，版本锁改从后端仓库读取。

### 9.3 旧 commit 回归关注（DeepX → QAQ-Harness 重命名）

- 根因 commit：`658ece8 chore: rename deepx -> QAQ-Harness` 将 data-root marker 的 `product` 从 `DeepX` 改为 `QAQ-Harness`，但没有迁移已落盘旧 marker；后端 `ensure_data_root` 又严格校验 `product == "QAQ-Harness"`，因此旧用户会被锁在数据根外。
- 当前前端 #10 在壳侧自行镜像了后端私有公式：`normalized_path_text`、`data_root_id`（FNV-1a 64）和 `DataRootMarker` 字段。契约一旦漂移会重新复现此漏洞。
- 后端 `ensure_data_root` 本身仍不接受 legacy `DeepX` marker；CLI/第三方直连 daemon 的路径没有迁移保护。
- 联查建议（待用户确认后实施）：
  1. 后端 `qaqh_types::platform` 增加权威的 legacy-marker 迁移逻辑，并让 daemon 启动路径自带迁移；旧 marker 仅在 canonicalRoot/ownerHome/rootId 全吻合时改写，异主/损坏 marker fail-closed 不越权。
  2. 前端 #10 改为调用后端公开 API（或至少由后端共享权威公式/测试向量），避免两边各维护一份 FNV-1a/路径归一实现。
  3. 前后端各补同一组回归测试：legacy DeepX marker 可迁移；QAQ-Harness marker 幂等不动；陌生产品/异主/损坏 marker 拒绝且文件不变。

### 9.4 第四轮检查：新增 issue #15（timeline 块重复回放）

- **Issue**：`QAQ-Harness#15 bug(runtime/timeline): 同轮 text↔tool 交替时同一逻辑块反复重开并携带整轮前缀，ChatView 错误回放 N 次`
- **状态**：OPEN；后端已复核，根因在 `qaqh-msglp/src/ringing_v1/engine_turn.rs` 的 `ensure_stream_block` / `maybe_emit_block_checkpoint`：整轮累计 `content`/`reasoning` 被当作每个新 segment 的 checkpoint 全量写入，导致同轮 text↔tool 交替时产生多个前缀超集块。
- **排期**：按 issue 评论，**不单独修复**，推迟到刀 1 step 2 / loop 协议改造时一并处理；本 issue 保留 OPEN 作为 loop 改造回归清单。
- **验收清单**（沿用 issue 建议）：
  - 单轮 `reasoning→tool→text→tool→text` 只生成 1 个 reasoning + 1 个 text 稳定块；或确需分段时，每段 checkpoint 只写本段增量，不携带整轮前缀。
  - 同轮 text↔tool 切换不得无限重开 block_id 或放大 replaceable/journal/timeline 数据。
  - 补后端回归测试覆盖交替流式块。
- **当前验证**：`cargo check --workspace` 通过；`cargo test -p qaqh-runtime` 与 `cargo test -p qaqh-msglp` 通过；`qaqh-workspace --lib` 存在已知并行全局态 flaky（`confirm_apply::apply_patch_dry_run_then_confirm` 单独串行重跑通过）。


### 9.5 远端状态刷新（前端 #10 已合并 / #11 open）

- `git fetch origin --prune` 后确认：
  - 后端 `QAQ-Harness` 远端 `origin/main` 仍为 `b49c7e6`；无 open PR；open issues 为 #9、#15。
  - 前端 `qaqh-winui-app` 远端 `origin/main` 已前进到 `1799528`（`fix(winui): port data-root marker migration and notify packaging fixes (#10)`），因此 #10 已合并；远端 open PR 列表为空。
  - 前端新增 open issue **#11**：要求 data-root 迁移不要复制后端私有公式，改为共享权威契约（对应本文件 9.3 的联查建议）。
- 本地前端工作树 `main` 仍停在 `252b522`，落后 `origin/main` 1 个提交；需要 `git pull` 或 `git merge --ff-only origin/main` 同步。
- 后端本地 `main` 当前领先 `origin/main` 1 个提交（本文档的 handoff 同步提交 `eb31a7b`），尚未 push。

---

## 10. 本轮工程师工作补充（2026-08-18 第五轮）

### 10.1 本轮已完成

- **QAQ-Harness #16**（`4319ed2`）：config theme/notifications + `qaqh_types::platform` 权威 data-root 迁移。已合并。
- **QAQ-Harness #17**（`d0fce1e`）：刀 1 step 2a，主 session in-process actor，移除 process worker。已合并。
- **QAQ-Harness #19**（`8623efb`）：刀 1 step 2b，退役 `worker.rs` / `logger.rs` / `qaqh agent` CLI。已合并。
- **QAQ-Harness #21**（`fd77452`）：刀 1 step 2c，`qaqh-workspace` actor 级 workspace/session/cancel 上下文。已合并。
- **前端 #10/#13**：data-root marker 迁移、main.rs timer 收敛（T15）已合入前端。
- **前端 issue #12**：已创建，要求消费后端 `migrate_legacy_data_root_marker()` 并删除私有公式副本。

### 10.2 Windows WIP 分支（issue #18）状态

- Windows 侧分支 `refactor/nextdev-tool-mode-single-contract` 已多次 rebase：
  - 最新 tip：`7c3b0b3`（基于 `fd77452`，落后 0）
  - `git merge-tree --write-tree`：**无冲突**
  - `cargo check --workspace`：**pass**
- 结论：可以走 PR 评审合入；但 WIP 仍是 70 文件大改动，建议拆成小提交。
- 当前后端 open PR：#20（docs T17，前端 bug inventory 追加）。

### 10.3 临时 LAN git daemon（应急通道）

- 裸仓库：`~/git/qaqh-backend.git`
- 地址：`git://60.0.0.32:9418/qaqh-backend.git`
- 已启用 `receive-pack`（支持 push）
- 已同步：
  - `main` = `fd77452`
  - `refactor/nextdev-tool-mode-single-contract` = `7c3b0b3`
- 说明：无鉴权、仅限局域网临时使用；GitHub 恢复后可停用。

### 10.4 下一轮工作（按原计划）

1. 继续完成刀 1 Step 2 剩余：
   - `ACTOR_TOOL_MANAGER` / `RUNTIME_CTX` actor 化
   - 移除 `ACTOR_SERIAL`，放开主 agent + N 子代理并发
   - `qaqh-subagent` 宿主直连（去掉 daemon HTTP/SSE 回连）
2. 完成后开刀 7：InjectionBus / loop 重构。
3. 穿插：
   - 前端 issue #11/#12：消费后端 data-root 迁移 API
   - 后端 issue #9：前端仍待闭环 theme/notifications 契约
   - 后端 PR #20：docs T17 可 review / 合入
