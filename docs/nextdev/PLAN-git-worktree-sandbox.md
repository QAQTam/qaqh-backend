# PLAN：Git UI 与模型沙箱（worktree + 快照）——CLI 起步、git2 收编

> 状态：待实施（2026-08-13）
> 范围：`crates/deepx-workspace`（git service）、`crates/deepx-runtime`/`crates/deepx-daemon`（协议路由）、`apps/winui`（git 页面）、`scripts/`（justfile）
> 关联：[rc4-native-client-contract.md](./rc4-native-client-contract.md)

## 1. 结论 / 决策

1. **git2 已是现状，不是未来**：`deepx-workspace` 后端已用 git2 0.21（vendored）实现 status/branch/switch/commit/diff/apply，且经 daemon 协议暴露（`git/diff`、`git/branch`、`git/branches`）。UI 落地**不重复造 CLI 解析**，直接消费现有 service。
2. **CLI 只补一个缺口：worktree 创建/管理**（git2 0.21 无 `worktree_add`，已核对源码）。初期用 `git worktree` 命令完成 UI 落地。
3. **快照机制用 git2 实现**（`commit(update_ref=None)` = `git stash create` 语义），是沙箱安全网的根基。
4. 后续用 alternates 副本方案收编 worktree 到 git2，CLI 依赖清零——用户机无 git.exe 也能完整使用。

## 2. 现状（2026-08-13 核对）

| 事实 | 锚点 |
|---|---|
| git2 0.21 vendored 已是生产依赖 | `crates/deepx-workspace/Cargo.toml` L30 |
| status/分支/切换/提交/单文件 diff 已实现 | `crates/deepx-workspace/src/git.rs`（`status_json` L15 / `current_branch` L66 / `list_branches` L77 / `switch_branch` L102 / `commit_all` L148 / `file_diff` L177） |
| patch apply 引擎（LLM 友好自动修正）已实现 | `crates/deepx-workspace/src/apply_patch.rs`（`apply_patch` L492） |
| daemon 协议已暴露 git 服务 | `crates/deepx-daemon/src/ringing_http.rs`（`"git/diff"`、`"git/branch"`、`"git/branches"` 路由）+ `crates/deepx-runtime/src/service.rs` 接线 |
| winui 无 git 页面消费 | `apps/winui/src` 无 git service 调用 |
| git2 无法创建 worktree | git2 0.21.0 `src/worktree.rs`：仅 open/validate/lock/prune，无 add |
| 已知性能坑：status 里全量 diff 统计曾拖垮 tokio（code=1006） | `git.rs` L36-52 FIXME（diff 统计已注释待验证） |
| `Index` 是 `!Send + !Sync` | git2 0.21.0 `src/index.rs`（快照流程须同线程完成） |

## 3. 待实施

### Phase 0 — 沙箱根基（快照 + worktree）

| 项 | 实现 | 状态 |
|---|---|---|
| 0.1 `snapshot(workspace) -> SnapshotId` | git2：`add_all(["*"])` → `write_tree()` → `commit(None, .., &[])`（无 ref 快照）；OID + 时间戳记入 `.deepx/snapshots.log`（保留最近 20 条） | 🔲 |
| 0.2 `restore(workspace, id)` | git2：`find_commit(id)` → `checkout_tree(force)` | 🔲 |
| 0.3 `changeset(workspace, id) -> {added, modified, deleted, sensitive}` | git2：`diff_tree_to_workdir(snapshot_tree)` + 敏感路径规则（Cargo.toml、Cargo.lock、.git*） | 🔲 |
| 0.4 worktree 创建（CLI 起步） | `git worktree add -b rc4/agent <主线路径> .deepx-worktrees/agent`；封装为 `scripts/sandbox.ps1` + justfile recipes（`just sandbox-new` / `just sandbox-rm`） | 🔲 |

### Phase 1 — WinUI Git 页面

**交互约束（来自 Electron 时期 bug 史，见 §5）**：

- **① 行数统计禁用**：git2 pathspec 过滤失效未修复前（`git.rs` L36 FIXME，c4e61f4 实录），页面只显示 change 类型，不显示 lines_added/removed——避免每个文件触发全量 diff（code=1006 拖垮 tokio 的前车之鉴）。
- **② 单文件 diff 按需拉取**：用户点击单个文件才请求 `git/diff`，禁止面板打开即批量 diff（48aaf62 实录：重型 git.diff 曾阻塞 daemon 全部请求）。
- **③ 状态刷新走 rev 比对模式**：沿用 overlay/composer 的「rev 比对 + 250ms poll_rev」，不引入 setInterval 式盲轮询（48aaf62 实录的前端退避教训）。

| 项 | 实现 | 状态 |
|---|---|---|
| 1.1 页面骨架 + 状态列表 | 消费现有 `git.branches`/status service → 侧栏分支列表（name + current 标记）；仅 change 类型，无行数（约束①） | 🔲 |
| 1.2 单文件 diff 视图 | 消费 `git/diff` service → unified diff 渲染（复用 diff_drawer 配色）；点击文件才请求（约束②） | 🔲 |
| 1.3 分支切换 | 消费现有 switch_branch service（stash 可选）→ 确认对话框 | 🔲 |
| 1.4 commit 入口 | 消费现有 commit_all service → 消息输入 + 提交后刷新 | 🔲 |
| 1.5 daemon 并发隔离验证 | 确认 SSE 时代 `git.*` 仍有 dedicated worker + semaphore（48aaf62 遗产）；缺失则补 | 🔲 |

### Phase 2 — worktree 收编 git2（CLI 依赖清零）

| 项 | 实现 | 状态 |
|---|---|---|
| 2.1 alternates 副本 | `copy_dir(工作树文件)` + 手写 `.git/objects/info/alternates` 指向主线 objects；git2 打开副本仓库 | 🔲 |
| 2.2 副本即沙箱 | 快照/回滚/changeset 全部在副本内运行；批准后经 `apply_patch` 或 `git format-patch` 合并回主线 | 🔲 |
| 2.3 移除 CLI 路径 | 0.4 的 `git worktree` 命令调用替换为 2.1；dev 机与用户机行为一致 | 🔲 |

### Phase 3 — 模型沙箱整合（事故防护）

| 项 | 实现 | 状态 |
|---|---|---|
| 3.1 脚本执行通道 | 模型批量脚本在 agent 副本内执行；执行前自动 snapshot | 🔲 |
| 3.2 事后审查 | 执行后 `changeset()`：safe（≤N 文件且无敏感路径）自动放行；suspicious 回滚 + 生成报告请求用户批准 | 🔲 |
| 3.3 审计日志 | 脚本原文 + 变更清单 + 决策结果 → `.deepx/sandbox-audit.log` | 🔲 |

## 4. 决策点

1. **worktree 路径与生命周期**：`.deepx-worktrees/agent` 常驻一个还是每任务一个？（建议：常驻一个 + 每次任务前 `git reset --hard` 重置）
2. **「safe」阈值**：默认建议 ≤20 文件且无删除、不触碰敏感路径；是否放宽？
3. **CLI 起步的可用性假设**：开发机有 Git for Windows；用户机不依赖（Phase 2 后清零）。接受此窗口期吗？
4. **status 行数统计的解除条件**（约束①）：需先修 git2 pathspec 过滤（或沿用 c4e61f4 的 text-based 统计方案）；修好后 UI 再开行数展示。

## 5. 变更记录

| 日期 | 改动 |
|---|---|
| 2026-08-13 | 初稿：基于 git2 现状核对（deepx-workspace 已 vendored 0.21、service 已暴露、worktree_add 缺失） |
| 2026-08-13 | 补 Phase 1 交互约束①②③ + 1.5 并发隔离验证：源自 Electron 时期 git UI bug 史（c4e61f4 pathspec 全量 diff、48aaf62 盲轮询与重操作阻塞、2cd80a3 WebSocket 级联、10efce9 TS 绑定 typecheck） |
