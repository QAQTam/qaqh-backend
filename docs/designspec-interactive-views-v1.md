# Interactive Views v1：声明式方块与交互动作设计

日期：2026-10-08 · 状态：设计提案，待实施。

关联：[Tools Render v2](spec-tools-render-v2.md)、[AskUser Form](spec-ask-user-form.md)。本规范新增渲染与动作设计，不表示现有 wire 已变更，也不修改 AskUser 已定稿的请求/答案语义。前端位于独立仓库，本文是跨端目标契约。

## 1. 设计决策

统一 todo、ask、goal 等工具的**界面描述与交互消费路径**，不统一其业务状态机。

- 工具调用卡片记录一次执行；Interactive View 呈现可持续操作的对象。调用完成不意味着对象关闭。
- TypeScript builder 是可信工具作者的构造 SDK，不是工具向客户端发送的可执行代码。
- wire 是可验证的 typed JSON/AST；模型也可声明 AST，但不能登记任意执行回调。
- 前端消费有限节点与正式动作，不解析工具文案猜表单、按钮或业务状态。
- 同一视图可呈现在对话、页面或侧栏；不能因为出现多个副本而创建多个业务请求。

本仓当前工具清单已有 todo/ask；goal 在本规范中是预留对象族，不声称已存在完整 goal 工具实现。其暂停/恢复等命令需后续业务规范定义，本文不赋予新权限。

## 2. 三层架构

```text
Tool execution → Tool Card（输入、进度、错误、终态）
                         ↓ view_ref
Domain object → View projector → ViewSpec → Client renderer
                                      ↓ action intent
                          Host action registry → Domain command
                                      ↓ authoritative update
                              Updated ViewSpec
```

| 层 | 所有者 | 权威范围 |
| --- | --- | --- |
| 业务对象 | todo/interaction/goal 等宿主服务 | 状态、权限、约束、持久化 |
| 投影视图 | 工具适配器或对象 projector | 布局、语义内容、动作提议 |
| 协议宿主 | runtime/domain/API | 身份、revision、动作登记、访问校验 |
| 客户端 | 各端 renderer | 布局适配、输入草稿、焦点、动画 |

ViewSpec 是可重建投影，不取代 tool ledger 或业务事实。业务提交失败时不能仅更新视图为成功。

## 3. ViewSpec 契约

以下 TS 是概念示例；正式声明用 Rust DTO，生成 TS。整数序号采用受限安全整数或十进制字符串，不无条件把 u64 导为 number。

```ts
type ViewSpec = {
  schema_version: 1;
  view_id: string;
  revision: string;
  binding?: {
    kind: 'todo' | 'ask' | 'goal' | 'resource';
    object_id: string;
    object_revision?: string;
  };
  source: { session_id: string; source_call_id?: string };
  mode: 'draft' | 'live' | 'historical';
  availability: 'active' | 'resolved' | 'expired' | 'unavailable';
  title: string;
  root: ViewNode;
  actions: ActionDescriptor[];
  fallback: string;
};
```

- view_id/revision/source 由宿主生成或盖章。工具不得通过 AST 伪造对象归属。
- revision 是投影版本，object_revision 是业务并发版本；不能用前者代替所有业务对象的锁。
- draft 是未正式登记的参数预览；live 也不保证每个动作可用，仍看 availability 与 action descriptor。
- historical 是固定历史投影，只允许明确的只读导航；持续对象的当前视图是另外的 live 投影，不能悄悄改写历史结果。
- 无 binding 的静态组合视图可以展示内容，但 invoke 动作必须绑定明确的宿主处理器及其资源范围。

## 4. 方块：有限 AST，不是通用页面语言

所有节点都有稳定 id；正式类型采用 tagged union，而非任意组件名与 props。节点 id 在一棵树中唯一、同一语义更新时保持稳定，不使用数组下标。

| 类别 | 首版节点 | 说明 |
| --- | --- | --- |
| 布局 | card、stack、row、section | children，spacing/density 为有限枚举；row 可在窄屏折行 |
| 文本/状态 | text、badge、progress | text 是纯文本；Markdown 必须另声明受限内容类型；未知总量使用不确定进度 |
| 工具内容 | content_block | 复用 Tools Render v2 的 code/diff/streams/table/locations/image/resource 等 typed blocks |
| 对象引用 | object_ref | todo/goal/task/board 等摘要及宿主资源引用，不复制对象事实 |
| 交互引用 | interaction_ref | 引用 AskUser 等权威交互请求；客户端用对应正式 renderer |
| 动作 | button、action_group | label、action_id、有限视觉语义，不含 callback 或命令字符串 |

首版不加入任意 CSS/HTML/JS、表达式求值、循环、远程组件模块或外部 renderer URL。数据列表由 projector 构造有界 children，或由 content_block 的正式分页协议提供。

未来可增加 standalone form/select/text_input，但必须先定义业务提交协议；本版 ask 用 interaction_ref 复用 AskUser 表单，不再定义第二套答案 union。其 single_select/multi_select/text 仍由 AskUser schema 驱动。

节点不支持未知 kind/version 时显示局部 fallback；整个 major 不支持时显示整视图 fallback。动作的语义未知则禁用或隐藏，不能降级为执行一段文字。

### 4.1 最小卡片实例

```json
{
  "schema_version": 1,
  "view_id": "view_123",
  "revision": "7",
  "binding": { "kind": "todo", "object_id": "todo_123", "object_revision": "4" },
  "source": { "session_id": "session_123", "source_call_id": "call_123" },
  "mode": "live",
  "availability": "active",
  "title": "发布计划",
  "root": {
    "kind": "card", "version": 1, "id": "root", "fallback": "发布计划",
    "children": [
      { "kind": "text", "version": 1, "id": "summary", "text": "已完成 2 / 5 项", "fallback": "已完成 2 / 5 项" },
      { "kind": "button", "version": 1, "id": "details", "label": "查看变更", "action_id": "open_changes", "fallback": "查看变更" }
    ]
  },
  "actions": [
    { "action_id": "open_changes", "kind": "navigate", "target": { "kind": "resource", "id": "diff_123" }, "enabled": true }
  ],
  "fallback": "发布计划：已完成 2 / 5 项"
}
```

此实例是导航型卡片，不暗示 todo 在当前业务中已经允许用户改状态。

## 5. Action 协议

ActionDescriptor 是宿主校验后的公开能力声明，action_id 只在 view_id 范围内解析，不是任意工具名称。

```ts
type ActionDescriptor =
  | { action_id: string; kind: 'navigate'; enabled: boolean;
      target: { kind: 'resource' | 'session' | 'diff' | 'object'; id: string } }
  | { action_id: string; kind: 'invoke'; enabled: boolean;
      disabled_reason?: string; input_contract: string; input_version: number }
  | { action_id: string; kind: 'interaction_submit'; enabled: boolean;
      request_id: string; interaction_kind: 'ask_user'; schema_version: number };

type InvokeRequest = {
  view_id: string;
  action_id: string;
  expected_view_revision: string;
  expected_object_revision?: string;
  request_id: string;
  input: unknown; // 正式实现按登记的 input_contract 解析成 typed DTO
};
```

### 5.1 三种动作路径

1. **navigate**：客户端打开正式目标，必要读取仍走资源访问检查。不得接受工具生成的任意本地路径/URL作为特权目标。通常不向模型追加输入。
2. **invoke**：宿主从登记表解析业务命令，验证 action 仍存在、输入 schema、当前权限、对象归属和 revision。公开 AST 不携带 handler 代码、shell、模型提示或内部凭证。
3. **interaction_submit**：调用既有专用交互 API。AskUser 继续提交 request_id/schema_version/submission_id/typed answers，遵循原有幂等及已解决冲突规则；不包装成另一套通用 input 答案。

工具可以提议按钮，但不能仅凭模型输出启用一个任意业务操作。宿主登记的是允许的语义操作；无现有命令/权限依据时降级只读，不能用 UI 特性绕过授权。

### 5.2 幂等与冲突

- invoke 的 request_id 由客户端生成；宿主按 caller/view/action/request_id 去重，并保存输入指纹。同 ID 同输入返回原回执，不同输入返回 action_request_conflict。
- 完成业务操作与幂等回执必须有原子性或可恢复的操作账本。发生崩溃且无法确认副作用时返回 indeterminate，不自动重复调用。
- 业务对象 revision 验证与修改应在同一事务/锁范围。过期 view 返回 view_revision_conflict 及新基线，不能先执行再提示冲突。
- 只读 navigate 不要求为了看资源强制刷新视图 revision；但目标读取仍检查最新访问权。
- 回执可以 accepted/pending/applied/rejected/indeterminate；pending 带 operation_id。已成功操作但视图投影延迟，不得解释为操作失败并建议重试。
- 重试用原 request_id 查询/重发；明确新业务操作才创建新 ID。retryable 不等于 safe_to_repeat。
- 多客户端同时点同一个对象，业务端决定单赢家或合法合并，不由前端动画决定。

## 6. todo / ask / goal 映射

### todo

工具调用结果保留操作收据；卡片引用会话任务清单的 live view，展示最新列表。用户勾选只有在明确提供正式 todo 状态更新命令后开放，否则只读。不能让 checkbox 的本地视觉状态成为完成事实。

### ask

工具输入流展示 draft form 预览；完成校验并登记权威请求后呈现 interaction_ref。答案只提交到 AskUserSubmit；预填值不是提交。请求正文首版不可变，没有 form patch/revision；ViewSpec revision 可以变的是外壳/可用状态，不是问题正文。问题改变必须结束旧请求并新建。

同一 request 在对话、页面、侧栏显示同一请求；任一客户端成功提交后其余副本失效。传输 challenge_id 与 canonical request_id 不混用，授权信息不放公开视图树。

### goal（未来对象族）

目标创建调用结束后，目标视图仍可跨轮更新。进度必须来自真实业务数据；没有已知 total 就显示不确定进度。pause/resume/complete/cancel 等按钮只能绑定未来定义的正式命令；模型预算、执行状态与人为目标完成状态不能混为一个百分比。终态目标不能因为卡片展开重新启动执行。

## 7. 草稿生成与输入动画

```text
provider arguments delta → incremental parser → tool preview projector
  → draft ViewSpec → frontend field reveal / draft content animation
完整参数 → typed validation → domain request registration
  → authoritative live ViewSpec / interaction_ref
```

- draft view 身份由宿主绑定临时 call 槽，拿到正式 call id 后保留同一卡片/视图实例。
- draft 不登记 invoke/submit 能力；按钮可显示但禁用并说明“正在生成”。只允许纯展示展开等本地动作。
- AST 若由模型生成，必须先经过结构与预算校验。流式阶段只投影已完成的可信节点/字段，不执行半截 AST，不自动修复后当完整请求提交。
- draft → live 是显式权威更新；丢失该帧时由快照恢复，不能用打字动画结束推断。
- 输入取消、错误或断流时收口草稿，不制造已创建对象。旧草稿可静态保留，但不得可提交。

## 8. 持续更新、快照与多位置呈现

逻辑 API（具体 route 与既有 daemon API 对齐后确定）：

| 操作 | 约束 |
| --- | --- |
| get_view(view_id, detail) | summary/preview/expanded 基线，带 revision |
| subscribe_views(ids, after_sequence, detail) | 复用现有 Ringing bus，不建立第二套独立真相流 |
| read_view_resource(view_id, node_id, revision, cursor, limit) | 复用 Tools Render 的有界资源读取 |
| invoke_view_action(request) | typed action 回执及 operation_id（若异步） |

首版优先发布自包含 view_snapshot。后续可增加受限 upsert_node/remove_node/set_text patch，但必须带 base_revision；不匹配重取快照，不允许任意 JSON Patch 改权限或 action registry。稳定 node id 使全快照更新也能保持客户端焦点。

同一业务对象可有多个布局不同的 view；共同 binding 指向同一对象，每个 view 独立投影 revision。多个渲染位置共享 view store，禁止 each mount 再注册一遍 action 或业务请求。

视图可挂载到明确 placement（conversation/page/panel），但 placement 只是宿主呈现请求，不赋予工具自动抢占导航、创建弹窗或跳转页面的能力。页面入口由用户操作或既有产品流程控制。历史记录默认引用固定收据；“查看当前状态”明确打开 live view。

持久化保存业务事实和必要的历史视图基线/版本，live 投影可重建。tool result 不反复复制全部对象正文。资源过期提供明确状态与 fallback，不能重新执行原工具来恢复一张卡片。

## 9. 前端状态与体验

- renderer 按 node/block kind 注册，不按工具名称穷举；窄屏布局适配由客户端决定。
- 纯导航卡片可整体点击；有内部按钮/表单时使用明确 action 区域，避免整个 card 捕获点击导致误触。必须支持键盘操作、可读 label、焦点与错误提示。
- invoke 正在提交时显示本地 pending 并防重复点击，但不提前将业务状态改为成功。未来乐观更新必须由具体业务契约声明回滚策略，首版默认不做。
- 本地输入草稿与权威 view 分离；视图刷新不得无故清空正在填写的答案。ask 正文不可变，解决/过期时明确停用草稿提交。
- draft/live/历史动画分别处理；重连/历史/reduced motion 直接恢复静态状态。后台视图不持续执行逐字动画。
- 点击默认不唤醒模型；只有业务命令明确产生会话输入/恢复交互时才进入 agent loop。
- expanded 资源按需取，非可见 view 仅维护摘要；订阅降档不取消业务任务。

## 10. 版本、能力与边界预算

客户端声明支持的 ViewSpec major、node/block versions、action kinds。宿主提供能力适配与 fallback；不能因为旧端无法渲染就丢掉 pending ask。若端不支持正式 AskUser 输入协议，显示不支持状态并指引受支持入口，不改成自由文本猜答案。

建议首版预算（实施测量后调整）：最大深度 16、节点数 256、单视图内联 UTF-8 ≤128 KiB、单纯文本节点 ≤4 Ki Unicode 字符；大内容走资源，有界 table 按页加载。超限/重复 id/非法引用/未知 action target 明确拒绝或转换有界 fallback，不执行未验证节点。

所有 view/resource/action 访问校验当前 caller/session/workspace 范围。action descriptor 不是可转授 token；隐藏按钮不是权限控制。文本与 label 默认纯文本，资源导航不自动获得任意 URL 抓取或本地文件读取权。

## 11. 作者侧 SDK 与未来 DSL

```ts
const view = ui.card({
  title: '发布计划',
  children: [
    ui.text({ id: 'summary', text: '已完成 2 / 5 项' }),
    ui.objectRef({ id: 'tasks', kind: 'todo', objectId: todoId }),
    ui.button({ id: 'details', label: '查看变更', actionId: 'open_changes' }),
  ],
});
// build/validate → typed ViewSpec；宿主绑定 ID、对象和正式 actions。
```

Rust 工具也有等价 builder；TypeScript 不是强制 runtime。builder 只生成 AST，不能把 closure 序列化为客户端回调。

若未来希望模型用代码式语法生成视图，单独设计受限 DSL 编译器：仅允许白名单构造调用与字面量，编译目标仍为相同 AST；不引入完整 TS eval、import、网络、文件或任意执行能力。JSON 与 DSL 应产生等价视图，不建立两个协议。

## 12. 实施顺序与验收

1. 定义 Rust ViewSpec/Node/Action DTO、版本门禁、TS 导出；统一复用 content blocks。
2. 接通只读 todo live view、diff/resource 导航、同 view 多位置挂载；先验证页面可点击体验。
3. 用 AskUser interaction_ref 贯通草稿→登记→填写→提交→各端失效，不改变既有答案契约。
4. 对已有正式业务命令接入 invoke registry、幂等账本、revision 校验和异步回执；不存在命令的功能保持只读。
5. goal 业务协议明确后接入 object projector；再评估受限 DSL、更多交互节点和细粒度 patch。

验收覆盖：

- AST 与 builder 等价、预算/深度/id/引用校验，未知版本可读降级。
- draft 的任何按钮不能造成业务副作用；完整输入注册前不能提交表单。
- 双击、相同 ID 不同输入、并发客户端、revision 冲突、崩溃后未知副作用均有明确结果。
- 多位置展示同一 ask 只恢复一次会话；正文不可变，预填不会自动提交。
- view 断线重连/旧帧/快照刷新不漂移对象状态、不重复执行动作、不清空合法本地草稿。
- 只读点击不进入模型 loop；invoke 失败不把卡片显示成成功；历史展开不重启业务。
- 资源分页、访问控制、未知节点 fallback、键盘操作和 reduced motion 可用。

本设计定下的是协议边界与统一方向；具体 HTTP route、目标业务命令、存储接入和前端实现仍需实施时按现有架构落地，不通过设计文档假定已完成。
