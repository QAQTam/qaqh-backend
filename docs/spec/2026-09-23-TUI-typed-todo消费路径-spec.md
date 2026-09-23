# TUI typed todo 消费路径 spec（2026-09-23）

> 状态：**P3 交底稿**。本文不要求 TUI 在旧锚点上立刻迁移；P3 合并后按本文
> 一次性升级即可。
>
> 背景：TUI 当前在 plan review 面板里使用 `qaqh_client::TodoItem`。P3 将
> plan review 的载荷类型改为 `PlanReviewItem`，并让 todo 工具的 canonical
> 输出收敛到 typed `TodoListOutput`。这两个类型与 workspace todo 面板使用的
> `DashboardTask` **不是同一个语义面**。
>
> **分支事实**：本文描述的是 **PR #288**
> （`feat/p3-tool-ledger-production-wiring`，head `728487e`）的落地后契约，不是
> `betav2` 现状。当前 `betav2 @ 39a20a1` 仍是旧 `TodoItem` 形状，
> **P3 合并前不要按第 3 节改 TUI**：此时 `qaqh-client` 根入口还没有
> `PlanReviewItem` 这个名字，提前迁移只能越过 `qaqh-client` 去引
> `qaqh-domain`，正好违反第 5 节的入口纪律。
>
> 复核锚点（`betav2` 上不可见，请按 PR 号看）：
> `PlanReviewItem` 定义见 PR #288 的 `crates/qaqh-domain/src/event.rs`；
> 根入口再导出见同一 PR 的 `crates/qaqh-client/src/lib.rs` 与
> `crates/qaqh-client/src/types.rs`。
>
> 本文引用的 TUI 侧路径（`src/app/session.rs` / `src/app/mod.rs` /
> `src/ui/modal.rs` / `src/ui/v2/modal.rs`）对应 **`qaqh-tui-app @ bbdcc3b`**
> （`chore(anchor): 采纳后端 tui-anchor-2026-09-23`）——那是 TUI 锚定
> `tui-anchor-2026-09-23` 时的状态；backend 仓没有该仓库副本，这几行按该 commit
> 复核。
> 上游需求：
> [`TUI对后端的协作需求`](../../../qaqh-tui-app/docs/spec/2026-09-23-TUI对后端的协作需求-spec.md)
> §5.1 / P1。

## 1. 三条消费面

| 消费面 | 权威类型/来源 | TUI 用途 |
|---|---|---|
| plan review 的待评审项 | `qaqh_client::PlanReviewItem`（由 PR #288 在 `qaqh-client` 根入口再导出） | plan modal 里的 Todo 预览 |
| workspace todo 面板 | `qaqh_client::DashboardTask`，来自 `session.dashboard`；失败时回退 `todo.status` JSON | 右侧/Workspace todo 列表 |
| todo 工具的结构化输出 | backend 内部 `TodoListOutput` / `TodoItemView`（模块为 `pub(crate)`，不是公开路径） | 只有 JSON 没有 typed 承载：`todo.list` 走 service 面（TUI 能用 `QueryRequest` 拿到这个 JSON），但 `qaqh-client` 没有对应类型 |

`TodoItemView` 所在模块当前是 `pub(crate)`；TUI 不应直接依赖 `qaqh-workspace`
去命名它，也不要自行复制一份镜像。若未来 TUI 需要 typed workspace todo，
应先在 `qaqh-client` 增加正式再导出/视图类型，另开契约变更。

## 2. `PlanReviewItem` 与旧 `TodoItem`：**纯改名，字段一字未动**

P3 后：

```rust
pub struct PlanReviewItem {
    pub id: String,
    pub title: String,
    pub description: String,
    /// P3 当前实现：String；取值约定 "small" | "medium" | "large"
    pub complexity: String,
}
```

`betav2` 上的旧类型形状完全相同，只是名字不同：

```rust
// crates/qaqh-domain/src/event.rs:258（betav2 @ 39a20a1）
pub struct TodoItem {
    pub id: String,
    pub title: String,
    pub description: String,
    /// "small" | "medium" | "large"
    pub complexity: String,
}
```

**P3 只是把 `TodoItem` 改名为 `PlanReviewItem`，没有增删字段、没有改
`complexity` 类型、也没有 wire 形状变化。** 所以这次迁移对 TUI 是**类型名替换**，
不存在「删除 `status`/`evidence`」这种字段收敛——`qaqh_domain::TodoItem` 从来就
没有这两个字段。上一版本文写成字段收敛是错的，已按本节更正。

> 措辞限定：**Rust 侧与 JSON wire 形状无变化**。但 `PlanReviewItem` 带
> `#[cfg_attr(feature = "ts", derive(TS), export_to = "qaqh/")]`，所以 **TS 生成绑定
> 的类型名会从 `TodoItem` 变成 `PlanReviewItem`** —— 对任何 import 生成绑定的 TS
> 消费方是编译期改名（本仓 `webui` 的 `TodoItemView` 是手写类型，不受影响）。

> 容易混淆的两点，提前说清：
>
> 1. `status` / `evidence` 属于**另一个类型**
>    `qaqh_workspace::todo::typed::TodoItemView`（`pub(crate)`），那是 workspace
>    todo 的 typed 输出视图，与 plan review 载荷无关，别把两者当成同一个东西。
> 2. `docs/spec/2026-09-18-workspace-v2-输出侧契约-spec.md` 曾规划
>    `PlanComplexity` 枚举与完整 `PlanView`，这组 v2 plan 视图在当前 P3 分支尚未
>    落地。本文按 P3 已实现的 `String` 交底；未来若 `PlanComplexity` 落地，需另发
>    wire 变更说明。

迁移结论：

- TUI 侧唯一的改动是把类型名从 `qaqh_client::TodoItem` 换成
  `qaqh_client::PlanReviewItem`；
- 渲染代码一行都不用改语义（`complexity` 的展示问题见第 3 节第 3 步）；
- 不要把 `PlanReviewItem` 塞回 workspace todo 列表，那条链路是 `DashboardTask`。

## 3. TUI 迁移步骤

P3 合并并发布新锚点后（前置条件：PR #288 已合入 `betav2`，且锚点里
`crates/qaqh-client/src/lib.rs` / `types.rs` 已再导出 `PlanReviewItem`；
这两处再导出是 #288 的一部分，不需要 TUI 侧另开 PR）。

前置条件可以机械判定，不用人肉确认。**必须锚定 `pub use` 块本身**，不要用
`grep -A 20` 这种窗口——`lib.rs` 里另有一条无关的
`pub use qaqh_domain::state::{…}`，窗口会把下面的 `pub use types::{…}` 一起吞掉，
造成「命令通过但命中的不是自己声明的符号」的假阳性：

```bash
awk '/^pub use types::\{/,/^\};/' crates/qaqh-client/src/lib.rs \
  | grep -qw 'PlanReviewItem' \
&& awk '/^pub use qaqh_domain::\{/,/^\};/' crates/qaqh-client/src/types.rs \
  | grep -qw 'PlanReviewItem' \
&& echo "anchor OK"
```

（`lib.rs` 那条锚 `pub use types::`，`types.rs` 那条锚 `pub use qaqh_domain::` ——
`lib.rs` 的 `pub use qaqh_domain` 只覆盖 `::state`，本来就不该在那里命中
`PlanReviewItem`。两条都命中即满足；任一条为空说明锚点还在 `betav2` 的旧形状上，
先别动 TUI。更硬的判据仍是编译：`cargo build -p qaqh-client --features ts` 后查
生成物，或直接在 `crates/qaqh-client/tests/public_api.rs` 里加一条
`use qaqh_client::PlanReviewItem;`。）

1. `src/app/session.rs`：

   ```rust
   pub todo_items: Vec<qaqh_client::PlanReviewItem>,
   ```

2. `ControlEvent::PlanReviewRequested { todo_items, .. }` 继续直接赋值——
   **这条路有 typed 承载，不需要解 JSON**：

   - `qaqh_client::ControlEvent` 是 `qaqh_domain::ControlEvent` 的再导出
     （`crates/qaqh-client/src/types.rs` 的 `pub use qaqh_domain::{... ControlEvent ...}`），
     不是 `qaqh_ringing` 独有的新类型；
   - 该 variant 本身就是 typed 的
     `PlanReviewRequested { interaction_id, turn_id, plan_content, review_type, todo_items: Option<Vec<PlanReviewItem>> }`；
   - TUI 现在就是这么用的：`src/app/mod.rs` 里
     `RingingEvent::Control(ev) => self.handle_control(...)`，`handle_control`
     直接 `match ControlEvent::PlanReviewRequested { todo_items, .. }` 并把
     `todo_items.unwrap_or_default()` 塞进 `PlanPanel`。

   > 别和 state 快照那条路混淆：`qaqh_domain::state::PendingInteraction` 只有
   > `{ id, kind }`，plan 的 `plan_content` / `review_type` / `todo_items` 只出现在
   > `ringing/projection.rs` 写出的 state JSON `pending_interaction.details` 里，
   > Rust 侧没有对应字段。**TUI 的 plan modal 从事件流取 `todo_items`，不要改从
   > state 快照的 `details` 里捞。** 两条路是不同投影，本节的迁移只涉及事件流那条。

3. plan modal 渲染：
   - `item.title` 不变；
   - `item.complexity` 是 `String`，按字符串展示。当前 TUI 两处都是
     `format!("{:?}", item.complexity)`，对 `String` 会渲染成带引号的 `"small"`：

     ```rust
     // qaqh-tui-app @ bbdcc3b — src/ui/v2/modal.rs（plan modal 的 todo 行）
     Span::styled(
         format!("  [{:?}] ", item.complexity),
         Style::new().fg(theme.text.dim),
     ),

     // qaqh-tui-app @ bbdcc3b — src/ui/modal.rs（v1 modal 的同一行）
     Span::styled(format!("  [{:?}] ", item.complexity), theme::dim()),
     ```

     改成 `{}` 即可（或按你自己的展示格式）；这是本次迁移唯一值得顺手改的渲染点；
   - 不要对 `small` / `medium` / `large` 做穷举 `match`——该值域目前只是注释级
     约定，schema 里没有 enum 约束。
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

输出侧 wire 值承诺为 `idle` / `in_progress` / `completed` / `cancelled`。
输入侧 `TodoStatusView` 还接受 `pending` / `complete` / `canceled` 三个 alias；
TUI 若只消费输出，不应依赖这些 alias。

### 4.1 `todo.status` 与 `todo.list` **不同构**，不要共用一个解包器

两者信封形状不同，别按同一套字段读：

| | `todo.status`（`todo_status_value`） | `todo.list`（typed `TodoListOutput`） |
|---|---|---|
| 外层信封 | 无 `timeis` / `status`，直接就是业务对象 | 有 `timeis` + `status: "ok"` |
| 计数 | **平铺**：`idle` / `pending` / `in_progress` / `completed` / `cancelled` / `total` | **嵌套**：`counts: { idle, in_progress, completed, cancelled, total }` |
| 当前项 | `current_id` + `current_title` | `current_id` |
| 额外字段 | `mode` | — |
| 计数细节 | `idle` 与 `pending` 是**同值双键**（都写同一个 pending 计数），不是两个计数 | 只有 `counts.idle` 一个来源 |
| `items[]` | `{id,title,description,status,evidence}` | `{id,title,description,status,evidence}`（形状相同） |
| 无 store 文件 | `null` | 仍返回成功信封（`items: []`、计数为 0） |
| 空 seed | `null` | **不返回信封**：`todo.list` 走 `&seed()?`，空 seed 直接是 `INVALID_INPUT` 错误信封 |

所以 TUI 侧要两个解包器；`todo.status` 是 workspace todo 面板的回退数据源，
`todo.list` 是工具侧 typed 输出，两者不是同一个契约的两种拼写。

### 4.2 `evidence` 的可空语义（实测）

`TodoItemView.evidence` 是 `Option<String>`，且没有 `skip_serializing_if`。
serde 对该形状的行为（已用 `serde 1` + `serde_json 1` 最小用例实测）：

| 场景 | 结果 |
|---|---|
| 反序列化 `"evidence": null` | `None`，**不报错** |
| 反序列化时缺字段 | `None`，**不报错**（`Option` 字段隐式可选） |
| 序列化 `None` | 输出 `"evidence": null` |

所以上面示例里的 `"evidence": null` 是**合法 wire 值**，不是笔误。TUI 侧只要
按 `Option<String>` 建模即可；唯一会踩的坑是把该字段建模成非可空 `String`
——那时 `null` 才会解析失败。TUI 不需要、也不应该要求后端改 wire 形状。

## 5. 验收

P3 落地后，TUI 至少验证：

- plan review modal 能显示 `PlanReviewItem.complexity`；
- approve/reject 不影响 workspace todo 列表；
- workspace todo 仍只由 `DashboardTask` 驱动；
- TUI 不越过 `qaqh-client` 新增对 `qaqh-domain` / `qaqh-workspace` 的直接依赖，
  也不新增 `TodoItem` 镜像。这是入口纪律，不是编译期隔离保证——本仓已有的
  `crates/qaqh-client/tests/public_api.rs`（#288 新增了 `PlanReviewItem` /
  `ConversationInputPurpose` 的用例）可以当后续机械化的护栏起点：再出现「壳层只能
  自己抄一份」的名字缺口时，先往那里加一条断言，而不是等 TUI 侧报编译错误。
