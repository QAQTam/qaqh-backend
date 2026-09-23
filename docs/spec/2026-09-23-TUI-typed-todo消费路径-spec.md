# TUI typed todo 消费路径 spec（2026-09-23）

> 状态：**P3 交底稿**。本文不要求 TUI 在旧锚点上立刻迁移；P3 合并后按本文
> 一次性升级即可。
>
> 背景：TUI 当前在 plan review 面板里使用 `qaqh_client::TodoItem`。P3 将
> plan review 的载荷类型改为 `PlanReviewItem`，并让 todo 工具的 canonical
> 输出收敛到 typed `TodoListOutput`。这两个类型与 workspace todo 面板使用的
> `DashboardTask` **不是同一个语义面**。

## 1. 三条消费面

| 消费面 | 权威类型/来源 | TUI 用途 |
|---|---|---|
| plan review 的待评审项 | `qaqh_client::PlanReviewItem`（P3 后由 `qaqh-client` 根入口再导出） | plan modal 里的 Todo 预览 |
| workspace todo 面板 | `qaqh_client::DashboardTask`，来自 `session.dashboard`；失败时回退 `todo.status` JSON | 右侧/Workspace todo 列表 |
| todo 工具的结构化输出 | `qaqh_workspace::todo::typed::TodoListOutput` / `TodoItemView` | 仅 backend 内部 canonical/model/display 投影，不是 TUI 公共入口 |

`TodoItemView` 所在模块当前是 `pub(crate)`；TUI 不应直接依赖 `qaqh-workspace`
去命名它，也不要自行复制一份镜像。若未来 TUI 需要 typed workspace todo，
应先在 `qaqh-client` 增加正式再导出/视图类型，另开契约变更。

## 2. `PlanReviewItem` 与旧 `TodoItem` 的字段对应

P3 后：

```rust
pub struct PlanReviewItem {
    pub id: String,
    pub title: String,
    pub description: String,
    /// "small" | "medium" | "large"
    pub complexity: String,
}
```

旧 `TodoItem`：

```rust
pub struct TodoItem {
    pub id: String,
    pub title: String,
    pub description: String,
    pub status: TodoStatus,
    pub evidence: Option<String>,
}
```

映射：

| 旧 `TodoItem` | `PlanReviewItem` | 说明 |
|---|---|---|
| `id` | `id` | 直接保留 |
| `title` | `title` | 直接保留 |
| `description` | `description` | 直接保留 |
| `status` | — | plan review 项不携带执行状态 |
| `evidence` | — | plan review 项不携带完成证据 |
| — | `complexity` | 新增；用于评审展示，值域为 `small` / `medium` / `large` |

因此这不是“改一个类型名”，而是 plan review 专用视图的字段收敛：

- 删除 `status` / `evidence` 的读取；
- 新增 `complexity` 展示；
- 不要把 `PlanReviewItem` 塞回 workspace todo 列表。

## 3. TUI 迁移步骤

P3 合并并发布新锚点后：

1. `src/app/session.rs`：

   ```rust
   pub todo_items: Vec<qaqh_client::PlanReviewItem>,
   ```

2. `ControlEvent::PlanReviewRequested { todo_items, .. }` 继续直接赋值；P3
   的 `qaqh-client` 已从 crate root 再导出 `PlanReviewItem`。
3. plan modal 渲染：
   - `item.title` 不变；
   - `item.complexity` 保持现有 `format!("{:?}", item.complexity)` 或改为
     按字符串展示；
   - 删除对 `status` / `evidence` 的隐式依赖（当前 TUI 没有直接读取，迁移
     成本主要是类型名）。
4. workspace todo 面板不改用 `PlanReviewItem`；继续消费 `DashboardTask`。

## 4. `todo.status` 与 typed `todo.list`

P3 的 service 面：

- `todo.status` 仍返回 workspace 面板所需的 legacy 聚合 JSON；
- `todo.list` 返回 typed `TodoListOutput` 信封：

  ```json
  {
    "timeis": "...",
    "status": "ok",
    "items": [
      {
        "id": "T1",
        "title": "...",
        "description": "...",
        "status": "in_progress",
        "evidence": null
      }
    ],
    "current_id": "T1",
    "counts": {
      "idle": 0,
      "in_progress": 1,
      "completed": 0,
      "cancelled": 0,
      "total": 1
    }
  }
  ```

状态 wire 值统一为 `idle` / `in_progress` / `completed` / `cancelled`。

## 5. 验收

P3 落地后，TUI 至少验证：

- plan review modal 能显示 `PlanReviewItem.complexity`；
- approve/reject 不影响 workspace todo 列表；
- workspace todo 仍只由 `DashboardTask` 驱动；
- TUI 不直接依赖 `qaqh-domain` / `qaqh-workspace`，也不新增 `TodoItem` 镜像。
