# 并行子代理 UI（工作状态区）实施计划（计划）

> 状态：进行中（MVP 代码已落地，待人工视觉验证；2026-08-14 更新）
> 范围：`apps/winui`（`bridge.rs`、`composer_bar.rs`）；后端配套另列
> 关联：[README.md](./README.md)、[CHATVIEW-V4-PLAN.md](./CHATVIEW-V4-PLAN.md)、[timeline-protocol-design.md](./timeline-protocol-design.md)

## 1. 结论 / 决策

- 输入框上方 `work_status_bar` 升级为「工作状态区」：父代理状态行（原样保留）+ 子代理胶囊行（WrapLayout 多列），Idle 时整体零高占位不变。
- 子代理状态由 bridge 内部 `SubagentTracker`（per-seed，key=`tool_call_id`）承载；**不动 `WorkPhase` 枚举**（serde 契约 + 多处 match，侵入大）。
- 终态唯一可靠信号是 `[SUBAGENT ...]` system 注入回合（`ToolFinished` 仅 spawn 确认）；UI 需幽灵检测兜底（Finished 后超时无注入 → Lost）。
- MVP 只改 `apps/winui/src/bridge.rs` + `apps/winui/src/composer_bar.rs` 两文件，`deepx-subagent`/`deepx-domain`/`deepx-client` 零改动；N=1 时 UI 与现状完全一致。
- goal 模式仅预留 `status_zone` 契约（`Agent | GoalProgress | Idle`），本期不实现 goal UI；后端配套（busy 注入重试、goal mode 字段）独立排期。

## 2. 现状（代码实证）

- ✅ `work_status_bar`（`apps/winui/src/composer_bar.rs` L1003-1063）：独立组件，props=`WorkPhase`（Clone+PartialEq），内部 90ms shimmer tick（`use_async_state` + `use_effect_with_cleanup`），Idle 零高，渲染 14px `ProgressRing` + `shimmer_runs`（`crates/deepx-fluent/src/lib.rs` L375-410）。
- ✅ `WorkPhase`（`apps/winui/src/bridge.rs` L815）：`Idle/Thinking/Answering/Tool(String)/WaitingUser`；驱动源 `RoundDeltaKind`（L757-759）、`ToolStarted`→Tool(name)（L3229）、`ToolFinished`→Thinking（L3232）、TurnCompleted→Idle（L691-693）。
- ✅ Tool 频道事件（`crates/deepx-domain/src/event.rs` L389-423）：`ToolCallPrepared{args_so_far}`（L396，流式 JSON 可解析 `agent_name`）→ `ToolStarted`（L399，**无参数**）→ `ToolFinished`（L418，工具终态）。
- ✅ 子代理注入链路（`crates/deepx-subagent/src/lib.rs` L488-497）：collector 收尾后以 `as_system:true` 注入主 seed，worker 走 `handle_system_input`（`crates/deepx-msglp/src/ringing_v1/engine_input.rs` 新增方法）→ `TurnOpened.user_text` 以 `[SUBAGENT 'name' COMPLETED|ERROR|CANCELLED|TIMEOUT]` 开头（lib.rs L475-481）。
- ✅ `spawn_subagent` 工具卡常驻对话流：非 file_mutation/readonly 回收类（`apps/winui/src/chat_view.rs` L1105-1118 可见性规则），完成卡与子代理后台运行形成语义错位。
- ✅ 同轮多工具并行 `MAX_PARALLEL_TOOL_WORKERS=4`（`crates/deepx-msglp/src/ringing_v1/engine_turn.rs` L698/711）；spawn 秒级返回 → 跨轮逐个 spawn 亦可并行运行。
- ⚠️ busy 拒绝：主代理回合运行中注入被 `poll_interrupts` 拒绝（`crates/deepx-msglp/src/ringing_v1/loop_core.rs` L424-441）→ `OperationFailed(busy)`，collector 只 warn 不重试（`crates/deepx-subagent/src/lib.rs` L503）→ 结果真实丢失。
- 🔲 goal 后端半成品：`GoalEngine`（`crates/deepx-msglp/src/ringing_v1/engine_goal.rs`）activate/stop/step_complete 全部未接线；`todo_status_json` mode 硬编码 `"manual"`（`crates/deepx-workspace/src/todo.rs` L164）；前端 goalBar 数据管道已建但冻结（`apps/winui/src/bridge.rs` L1000-1009、L1637-1661、L3169-3177）。

## 3. 决策点

| # | 事项 | 建议 | 截止 |
|---|---|---|---|
| D1 | 幽灵检测超时窗口 | `ToolFinished` 后 `timeout_secs + 30s`（timeout_secs 取自 spawn 参数，默认 cfg.subagent.timeout_secs） | 实施时 |
| D2 | 胶囊上限 | 8 个，超出折叠「+N 已完成」 | 实施时 |
| D3 | busy 拒绝修复归属 | 后端独立修复（collector 重试/排队），**不阻塞**本次 MVP；UI 先以 Lost 兜底 | rc.5 前 |
| D4 | 注入回合的 agent_name 关联 | 注入文本仅带 name，用 name 匹配 tracker；同轮重名回退 tool_call_id 短哈希 | 实施时 |

## 4. 实施细节

### 4.1 SubagentTracker（bridge 内部状态源）

- 仿 `composer_activity` 模式（`apps/winui/src/bridge.rs` L1353 `set_composer_phase` 同款）：`HashMap<seed, HashMap<tool_call_id, SubagentItem>>` + 独立 rev（`composer_rev` 同机制）。
- `SubagentItem { call_id, name, state: Spawning|Working|Done|Error|Timeout|Cancelled|Lost, started_at, finished_at }`。
- 事件驱动：
  - `ToolCallPrepared{name=="spawn_subagent"}`：解析 `args_so_far` 中 `agent_name`（一次，失败回退 call_id 短哈希），登记 name 映射；
  - `ToolStarted`：建实例（Working，记 started_at）；**ToolFinished 不销毁**（关键差异：spawn 确认 ≠ 子代理完成）；
  - conversation 频道 `TurnOpened.user_text` 匹配 `^\[SUBAGENT '(.+)' (COMPLETED|CANCELLED|ERROR|TIMEOUT[^\]]*)\]`：按 name 收敛终态；
  - 幽灵定时：Finished 后 D1 窗口无注入 → Lost（琥珀 ⚠）。
- `ToolProgress` **不进 tracker**（高频，只认 Prepared/Started/Finished/tag）。

### 4.2 渲染（composer_bar.rs）

- `work_status_bar` → `work_status_area`：`vstack((父代理状态行, 子代理胶囊行))`，胶囊行仅 tracker 非空时渲染，Idle 时整体零高（保持现有 `with_key("work-status-empty")` 纪律）。
- 胶囊：状态图标（Working=小 ProgressRing / Done=✓ / Error=✗ / Timeout=⏱ / Cancelled=⊘ / Lost=⚠）+ `agent_name`（超长省略 + tooltip）+ `mm:ss` 耗时；Done 后降透明 0.6。
- 性能：父行 shimmer tick 保持 1 条（现状）；胶囊共用**单个 1s 时钟**（`use_async_state`）刷新耗时，不逐卡开线程。
- `ComposerState` 增 `subagents: Vec<SubagentItem>` + `status_zone: StatusZone`（serde default 兼容，`StatusZone { Agent, GoalProgress, Idle }` 预留 goal）。

### 4.3 goal 预留契约（本期不实现）

- `status_zone == GoalProgress` 时父状态行切换为 goal 卡片（progress 0..1 + 步骤文本），胶囊行独立并存。
- 后端配套（另排期）：① `todo_status_json` L164 mode 硬编码修复 + `DashboardSnapshot`（`crates/deepx-domain/src/event.rs` L134-141）增 `mode` 字段；② 新增 Control 命令 `GoalStart/GoalStop` 映射 `GoalEngine.activate/stop`；③ collector busy 重试。

### 4.4 数据流

```
Tool 频道事件 → bridge.rs match 增分支 → SubagentTracker 更新 → rev++
→ composer_snapshot 合并 → ComposerState JSON → work_status_area props → 重渲染
system 注入：conversation 频道 TurnOpened 解析 tag → 同一 tracker 收敛
```

## 5. 实施结果（落地后回填）

| 计划条目 | 结果 |
|---|---|
| SubagentTracker + 事件接线（bridge.rs） | ✅ 已落地：`upsert_subagent`/`converge_subagent`/`ghost_sweep`/`subagent_items` + Tool 频道（Prepared/Started）与 Conversation 频道（TurnStarted tag）接线 |
| work_status_area 渲染 + 胶囊（composer_bar.rs） | ✅ 已落地：`WorkStatusProps` + 父行/胶囊行组合，共用 1s 时钟，上限 8 折叠 |
| 幽灵检测（D1 窗口） | ✅ 已落地：`SUBAGENT_GHOST_TIMEOUT_MS` 10min，快照时惰性扫描，真实终态可覆盖 Lost |
| status_zone 契约 + goal 空占位 | ✅ 已落地：`ComposerState.status_zone`（idle/agent，goal_progress 预留） |
| 回归：N=1 与现状一致 / cargo check / 壳编译 | ✅ cargo check + clippy 无新增告警；winui 68 tests 通过（含 3 新增：tag 解析/agent_name 解析/tracker 生命周期） |

## 6. 变更记录

| 日期 | 改动摘要 | 触发原因 |
|---|---|---|
| 2026-08-14 | 初稿：工作状态区方案定稿（三份子代理探索注入：UI 设计 / 事件链路 / goal 契约） | 并行子代理 UI 设计需求 |
| 2026-08-14 | MVP 落地：bridge.rs tracker + composer_bar.rs 胶囊渲染 + 3 单测（68 全绿） | 按计划实施 |
