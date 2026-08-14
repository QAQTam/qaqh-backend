# WinUI 侧栏工作区化（Workspace 导航 + 标签页过滤）实施计划（计划）

> 状态：待实施（2026-08-14 定稿）
> 范围：`crates/deepx-types`、`crates/deepx-session`、`crates/deepx-runtime`、`apps/winui`（`shell_store.rs`、`bridge.rs`、`sidebar.rs`、`session_tabs.rs`、`main.rs`）
> 关联：[README.md](./README.md)、[rc4-native-client-contract.md](./rc4-native-client-contract.md)、[dsh-alignment.md](./dsh-alignment.md)

## 1. 结论 / 决策

- 会话按「工作区」组织：新增 Workspace 实体 `{id: uuid, path: canonical realpath, title, order}`，会话归属采用**显式账户 + cwd 自动归属双轨**（参考 `F:\deepseek-harness-winui\packages\workspace\workspace\src\types.ts`：WorkspaceId 用 uuid 不用路径，成员资格 = 账户内 + header cwd 与 workspace path 双重校验）。
- WinUI 布局改造：左侧 sidebar 从「任务/归档」平铺改为「**Workspace 树 + 未分组 + 归档**」三段；上方 TabView 从「全量非归档」改为「**当前 workspace 的非归档会话**」（`session_tabs.rs` 数据源加 workspace 过滤）。
- **孤儿 = 未分组（Ungrouped）**：sidebar 常驻固定项，点击后标签页显示全部未分组会话；新建任务在未分组选中时创建孤儿会话——孤儿有入口、可恢复，不丢失。
- `active_seed` 保持全局唯一（引擎单活跃，不动）；切换 workspace 仅过滤标签页，chat 视图持续显示活跃会话，标签高亮在跨 workspace 时自然消失。
- 一期**不做拖拽**，跨 workspace 移动走「移动到…」菜单（RPC `workspace.move_session`）；拖拽二期评估。
- 与现有「运行环境 workspace」（`workspace.set` local/wsl/remote，`apps/winui/src/bridge.rs` L2865-2883）**解耦**：本计划只做组织语义，不改运行语义。
- daemon 侧落地（P1）与 WinUI 侧落地（P2）分两个提交；P1 完成即 TUI 可复用。

## 2. 现状（代码实证）

- ✅ 侧栏平铺列表：`apps/winui/src/sidebar.rs` L302-339 按 `archived` 二分「任务/归档」两组，组头 L291-298；行结构契约 D15（L88-96）：恒为 `border(grid(竖条, 圆点, 标题, 删除))`。
- ✅ 标签条：`apps/winui/src/session_tabs.rs` L3 数据源 = 快照 `filter(!archived)`（L107、L133 两处），`TAB_STRIP_HEIGHT=44`（L26），500ms 轮询 rev（L82-95）；标签 × → `spawn_archive`，`+` → `spawn_new_session`。
- ✅ 布局：`apps/winui/src/main.rs` L381-395 右侧文档区 = row0 TabView + row1 内容区，tabs 与内容同一坐标系。
- ✅ 前端会话模型：`apps/winui/src/shell_store.rs` L52-61 `SessionItem {seed,title,state,running,updated_at,archived}`——**无 workspace/cwd 字段**；投影 `project_session_meta` L67-109。
- ✅ 数据通道：`apps/winui/src/bridge.rs` L1863-1883 `spawn_refresh_sessions`（`session.list` + `session.activity` → 缓存 → rev++）；`navigate` L3150。
- ✅ daemon 会话元数据：`crates/deepx-types/src/session.rs` L62-116 `SessionMeta` 持久化字段无 cwd/workspace；`session.list` 直接序列化 meta（`crates/deepx-runtime/src/service.rs` L664-676）。
- ✅ 会话存储：`{data_dir}/sessions/{seed}/meta.json`（atomic replace-write，`crates/deepx-session/src/store/mod.rs` L18-40）+ 中央索引 `index.json`（L161-248，IndexLock 互斥）；`persist_mode` 等「只改 meta」先例 `crates/deepx-session/src/manager.rs` L209-215。
- ✅ RPC 分发：`crates/deepx-runtime/src/ringing/query.rs` L15-17 `session.list/meta/activity` 三个只读方法；`requires_seed` 集合 L136-142。
- 🔲 参考实现（外部仓库）：`F:\deepseek-harness-winui\packages\workspace\workspace\src\types.ts`（Workspace 实体 + `attachSession/detachSession/insertSessionBefore` + `status()='ok'|'missing-dir'`）；`packages/client/ui-workspace/src/client/tree.ts` L12-16（`UNGROUPED_KEY=''` + `UNGROUPED_LABEL='Ungrouped'` 孤儿桶）；`packages/client/runtime/src/client/contract/workspaces.ts` L36 `create({path})`、L86 `insertSessionBefore`。

## 3. 决策点

| # | 事项 | 建议 | 截止 |
|---|---|---|---|
| D1 | 归属规则 | 显式账户优先；新会话 cwd（realpath）等于/位于某 workspace path 时自动 attach；其余 = 未分组 | 实施时 |
| D2 | 新建任务默认归属 | 当前选中 workspace（未选中/未分组 → 孤儿）；home 视图新建同规则 | 实施时 |
| D3 | 切换 workspace 时活跃会话 | 标签高亮消失、chat 保持显示；标签条尾不插跨 workspace 会话 | 实施时 |
| D4 | workspace 注册表位置 | daemon：`{data_dir}/workspaces.json`（与 `sessions/` 同根，`manager.rs` L56-63 `init(data_dir)` 处挂载）；前端零持久化 | 实施时 |
| D5 | 一期交互范围 | 增删改 workspace + 菜单移动会话；拖拽、搜索、flat/grouped 切换全部二期 | rc.5 前 |
| D6 | 未分组项 | 常驻可折叠（计数徽标）；不允许删除/隐藏 | 实施时 |
| D7 | cwd 自动归属匹配 | realpath 相等或 `strip_prefix(workspace_path)` 成功即归属（大小写不敏感仅 Windows） | 实施时 |

## 4. 实施细节

### 4.1 P1：daemon 数据模型 + RPC

**SessionMeta 增 cwd**（`crates/deepx-types/src/session.rs` L62-116）：

- `#[serde(default, skip_serializing_if = "Option::is_none")] pub cwd: Option<String>`——旧 meta.json 缺省 = None = 未分组，无需迁移。
- 写入口：`persist_new_session`（`manager.rs` L257-270）创建时记录 canonical path（`std::fs::canonicalize` 失败则记录原样字符串）；`persist_mode/persist_skills` 同款「只改 meta 不动消息」通道（L209-248）不需改。

**新增 WorkspaceStore**（`crates/deepx-session/src/workspace.rs` 新文件）：

- 结构 `WorkspaceMeta { id: String(uuid v4), path: String, title: String, order: u32, session_ids: Vec<String> }`；持久化 `{data_dir}/workspaces.json`，原子写 + IndexLock 同款锁（复用 `store/mod.rs` L193-230 模式）。
- 方法：`create(path) -> id`（realpath + basename 默认标题）、`rename/delete`、`attach(ws_id, seed)`（校验 session 存在且 cwd 匹配，不匹配拒绝——对齐 dsh 双重校验）、`detach`、`move_session(seed, to_ws_id)`、`list() -> Vec<WorkspaceMeta>`。
- `list_sessions`（`service.rs` L664-676）输出 enrich `workspace_id`：seed ∈ 某 ws.session_ids 即归属（cwd 兜底仅用于自动 attach，不用于查询期推导，保持单一事实源）。

**RPC**（`query.rs` L15-17 同表追加 + `service.rs` 实现）：

- 只读：`workspace.list`（含 session_ids 与 status 探测：目录缺失标 `missing-dir`，对齐 dsh `status()`）。
- 写入走既有 Action 通道（`ActionRequest`，与 `WorkspaceSet` 同模式，`bridge.rs` L2878 参照）：`workspace.create / rename / delete / attach / detach / move_session`；全部不要求 seed（非 session 作用域）。
- 变更后 rev++ 语义：workspace 独立 rev（`workspace.list` 轮询 500ms，与 session 同机制）；`session.list` 输出变化沿用现有 rev。

### 4.2 P2：WinUI 侧栏 + 标签页

**shell_store.rs**：

- `SessionItem` 增 `workspace_id: Option<String>`、`cwd: Option<String>`（L52-61；`project_session_meta` L67-109 透传）。
- 新增 `WorkspaceItem { id, title, path, missing_dir: bool, session_ids: Vec<String> }` + `workspace_snapshot() -> (Vec<WorkspaceItem>, u64 rev)`，与 `session_snapshot`（L963 注释、bridge.rs L1863 链路）同模式；`current_workspace: Option<String>` 状态放 bridge core（跨 sidebar/tabs 共享，`main.rs` 只读）。

**bridge.rs**：

- `spawn_refresh_workspaces`（workspace.list → 缓存 → rev++，500ms 轮询，仿 `spawn_refresh_sessions` L1865）；
- `spawn_workspace_create(path) / spawn_workspace_delete(id) / spawn_workspace_rename(id, title)`；
- `spawn_session_move_workspace(seed, to_ws)`（ActionRequest 直发）；
- `set_current_workspace(id | None)` → rev++ → sidebar/tabs 重渲染。

**sidebar.rs**（三段式，复用现有组头模式 L291-339）：

```
Workspace 组头（列表头：计数 + ➕ 添加）
├ workspace 行（图标 + title + 会话数徽标 + 菜单⋯）  ← 点击 = set_current_workspace
├（展开态）会话行：复用 session_row L100（active 高亮不变）
未分组（固定项 + 计数徽标）                        ← 点击 = current_workspace=None
归档（保持现状，L320-337 不动）
```

- workspace 行选中态 = 现有药丸高亮（L93-96 语义）；行内 `⋯` 菜单 = 重命名/删除/展开会话（菜单用现有 `menu` 原语）。
- 结构契约沿用 D15（L88-96）：行元素类型恒定，只改修饰字段。

**session_tabs.rs**：

- 组件 props 增 `current_workspace: Option<String>`；L107/L133 两处 `filter` 改 `filter(|s| !s.archived && s.workspace_id == current_workspace)`（`current_workspace=None` = 未分组视图，`workspace_id=None` 的会话全部显示）。
- `+` 新建：先 `set_current_workspace`（若选中 workspace）再 `spawn_new_session`，daemon 侧 attach 由 cwd 自动归属完成。
- 归档 × / 选中 resume 逻辑（L3 注释链路）零改动。

**main.rs**：`component(session_tabs_component, ())`（L384）传 `current_workspace`；home_view 会话卡片（`apps/winui/src/home_view.rs` L113-160）按 workspace 分组展示（二期可延后）。

### 4.3 数据流

```
daemon:  persist_new_session(cwd) → workspace.list 自动归属 → index.json/workspaces.json
前端:    workspace.list + session.list → bridge 缓存 rev++ → sidebar 树 / tabs 过滤
交互:    点 workspace → set_current_workspace → tabs 重过滤；菜单移动 → ActionRequest → 双 rev++ → 双向刷新
```

### 4.4 验收清单

- [ ] 旧数据零迁移：无 cwd 的老会话全部落「未分组」，daemon 启动不报错
- [ ] 新建会话在 workspace 目录内自动 attach；目录外新建 = 未分组
- [ ] 移动会话后 `session.list.workspace_id` 与 `workspace.list.session_ids` 一致；重启 daemon 持久
- [ ] sidebar 三段渲染、workspace 选中高亮、未分组计数正确
- [ ] tabs 随 workspace 切换过滤；归档/未分组/workspace 三视图切换无 panic、无控件树错位
- [ ] `cargo check` + clippy 无新增告警；winui 既有测试全绿 + 新增单测（归属判定 / RPC 序列化 / 过滤）

## 5. 实施结果（落地后回填）

| 计划条目 | 结果 |
|---|---|
| P1：SessionMeta.cwd + WorkspaceStore + RPC | 待实施 |
| P2：sidebar 三段式 + tabs 过滤 + bridge 接线 | 待实施 |
| 回归：零迁移兼容 / 双 rev 同步 / 单测 | 待实施 |

## 6. 变更记录

| 日期 | 改动摘要 | 触发原因 |
|---|---|---|
| 2026-08-14 | 初稿：方案定稿（Workspace 实体 + 双轨归属 + 三段侧栏 + tabs 过滤 + 未分组孤儿语义；参考 dsh `packages/workspace` 与 `ui-workspace` 实现） | WinUI 会话按 workspace 分类需求 |
