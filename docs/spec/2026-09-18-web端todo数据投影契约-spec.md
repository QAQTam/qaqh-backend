# web 端 todo 数据投影契约（前端发起，2026-09-18）

> 发起方：web 前端（qaqh-webui）。读者：后端 workspace/todo 域 + 契约层。
> 性质：**前端需求提案**——按 §0b 兼容政策（前后端共进退、不做向前兼容），
> 本提案的 wire 改动可直接落地，不需要迁移。
> 状态：**草案待评审**（由前端负责人起草，后端确认字段与实现成本）。

## 0. 一句话

todo 的**存储模型是好的**（TodoStore.items + 状态机 + evidence），坏的是
**投影层**：状态名三处分裂、无事件、无界字段、无类型导出、无 bootstrap 集成。
web 不需要新能力，需要**同一个状态只有一个名字、一条到达路径**。

## 1. 现状问题（前端视角，附证据）

| # | 问题 | 证据 |
|---|---|---|
| C1 | **状态名三分裂**：同一 `Pending` 状态，存储 serde 名 `pending`（model.rs:31），wire 投影名 `idle`（store.rs:192 `status_name`），counts 键名 `idle`（actions.rs:465），错误提示教用户 "Use idle, …"（actions.rs:257）——而 `todo.status` 的响应里又**同时**存在 `idle` 和 `pending` 两个计数键 | store.rs:75-120、actions.rs:461-470 |
| C2 | **同名异义**：`qaqh_domain::event::TodoItem`（评审项：id/title/description/complexity，event.rs:258）与 `qaqh_workspace::todo::model::TodoItem`（状态项：…/status/evidence，model.rs:14）撞名，ts 导出必然冲突 | 两 crate |
| C3 | **投影是工具输出复用**：service 层直接调工具路径函数（`todo_list_for` 等），`todo.status` 的 `mode` **硬编码 "manual"**，与 `TodoStore.mode` 真值（manual/goal）脱节 | store.rs:112、actions.rs:439 |
| C4 | **无事件**：todo 变更只能靠 tool 频道 `tool_finished(todo_*)` 启发式 + `DashboardSnapshot.current_todo_id` 侧写推断，web 无法事件驱动刷新 | service.rs:493-501 |
| C5 | **evidence 无界**：实测单条 500+ 字符且无上限语义；wire 无 truncated 标记 | 用户会话 todo.json T1-T3 |
| C6 | **无时间戳**：created/completed 时间缺失，监视面板无法回答"何时完成、耗时多久" | model.rs:14-23 |
| C7 | **无 TS 类型**：todo 类型未进 ts-rs 导出链，web 只能手抄 | 全仓 ts export 无 todo |
| C8 | **不在 bootstrap**：新页面首屏必须额外调 `todo.status` 才知道计划状态 | ControlState 无 todo 字段 |

## 2. 目标契约（v2）

### 2.1 状态词统一（C1）

wire 一律使用存储枚举的 serde 名：

```
pending | in_progress | completed | cancelled
```

- `status_name()` 删除或改为直映；counts 键名同步；
- `todo.set`/过滤参数接受 `pending`（旧 `idle` 按 §0b 直接断）；
- 错误提示同步改为 "Use pending, in_progress, completed, or cancelled."

### 2.2 条目投影 `TodoView`

```jsonc
{
  "id": "T2",
  "title": "…",                 // wire 上限 200 字符 + "titleTruncated": true
  "description": "…",           // 上限 2000 + truncated 标记（同语义）
  "status": "pending | in_progress | completed | cancelled",
  "evidence": "…",              // 上限 2000 + "evidenceTruncated": true；仅 completed 常见
  "order": 1                    // 展示序（store 数组序照抄），排序不再依赖数组隐式序
}
```

### 2.3 轻投影 `TodoSummary`（进控制频道）

```jsonc
{
  "revision": 7,                // TodoStore 每次写盘 +1（写路径统一入口处递增）
  "mode": "manual | goal",      // 真值，废除硬编码（C3）
  "currentId": "T2",            // 现有 current_id 解析语义保持
  "currentTitle": "…",
  "counts": { "pending": 1, "inProgress": 1, "completed": 3, "cancelled": 0, "total": 5 }
}
```

### 2.4 事件（C4）

`ControlEvent::TodoChanged { revision, summary: TodoSummary }`——control 频道、
replaceable。设计取舍：**薄事件自带摘要**（前端看板零额外请求即可更新），
需要 evidence 全文时再调 `todo.status`。

触发点：`todo.set` / `todo.cancel`（service 写口）与工具路径写盘（todo_write /
todo_update / todo_cancel 工具）**统一**在写盘成功后 emit——写入口只有
`write_store_for` 一处，事件在公共入口 `save_todo`/`write_store_for` 的调用方
收口即可，避免漏发。

### 2.5 bootstrap 集成（C8）

`ControlState.todo: Option<TodoSummary>`——随 `RingingSessionBootstrap` 下发，
新页面首屏零额外请求。`todo.status` 保留（全文 evidence 用）。

### 2.6 类型导出（C7）

`TodoView` / `TodoSummary` 挂 ts-rs（`export_to = "qaqh/"`），并入契约产物链
（与 P0-1 TS 导出 recipe 同批）。

### 2.7 命名去重（C2）

`qaqh_domain::event::TodoItem` → `PlanReviewItem`（它只服务于
`PlanReviewRequested.todo_items`，字段是 complexity 语义）。全仓引用面小（grep
确认仅 event.rs 与 plan 相关路径）。

## 3. 第二批（明确延后，避免第一批膨胀）

- `created_at` / `completed_at`（unix ms）：需要 TodoStore 版本化（新字段），
  按 §0b 直接断数据根也可，但涉及 store 结构改动，与状态词统一解耦。
- `todo.set` 对 web 开放结构化编辑（reorder / 改 title）：agent 主权优先，
  web v1 只读 + `todo.cancel`。

## 4. web 侧承诺（对等义务）

1. 消费 typed 状态词，**删除 idle→pending 映射层**（我方设计稿里的归一逻辑作废）。
2. 事件驱动刷新：`TodoChanged.revision` 变化才更新看板；evidence 全文按需 `todo.status`。
3. M3 ToolDisplay 落地后，工具卡对 `todo_*` 不做任何参数解析特殊化（当前也未写）。
4. 看板 UI 按前端设计稿实现（current 置顶 / 分组 / evidence 折叠）。

## 5. 验收标准

1. `todo.status` 返回 typed JSON：四状态名统一，无 `idle` 键；fixture 覆盖全四态。
2. 写路径（工具 + service）每次成功写盘 `revision` 递增并 emit `TodoChanged`；
   连续两次 `todo.set` 产生两个 revision。
3. `RingingSessionBootstrap.control.state.todo` 存在且与 `todo.status` 的
   counts/current 一致。
4. `qaqh/` 目录导出 `TodoView` / `TodoSummary` 类型。
5. `rg -n '"idle"' crates/qaqh-workspace crates/qaqh-runtime` 归零（错误提示含
   idle 文案一并清）。
6. `evidence` 超 2000 字符时 wire 携带 `evidenceTruncated: true` 且不超限。
