# Tools Render Protocol v2：能力盘点与全生命周期卡片协议

日期：2026-10-08 · 状态：提案，待评审；不是已实现/已冻结协议。

交互视图补充设计：[Interactive Views v1](designspec-interactive-views-v1.md)。todo/ask/未来 goal 的持续对象视图、声明式方块和点击动作由该规范定义；本规范负责工具调用生命周期。二者复用内容块、资源读取与现有事件总线，不各建一套渲染事实。

本次只增加设计文档，不重写执行器、不修改现有 wire。Tool SDK v2 已存在，本规范专门定义它尚未收敛的渲染面，避免两个 v2 混名。前端在独立 qaqh-winui-app 仓库；本次没有读取或改动该仓库，前端建议是跨仓接入契约而非现状审计。

## 1. 结论与职责

支持“工具自行产出输出、错误码、流输出、输入流预览”，但不建议工具返回 UI 源码，也不建议输入未结束就执行。

- 工具/适配器拥有语义：参数预览、业务输出、业务错误、执行阶段、卡片内容。
- 宿主拥有事实：调用身份、准入/权限、调度状态、超时取消、序号、持久化和恢复。
- 前端拥有呈现：布局、主题、动画、虚拟列表、展开、按需读取正文。
- 工具不能自行宣告 permission granted，不能通过一帧 progress 宣告执行成功。
- 工具不制造假进度：快工具可以直接完成；所有工具必须支持 input preview 和最终卡片，但不要求每个工具都产生执行中增量。

四条逻辑通道：InputPreview / ExecutionProgress / TerminalResult / CardResource。ModelProjection 独立于上述渲染通道，不再把 model_text 当 UI 数据源。

## 2. 基于当前源码的能力盘点

### 2.1 注册面

`crates/qaqh-workspace/src/registration.rs` 的默认注册测试锁定 **23 个工具**；`qaqh-subagent` 注入 **18 个**，合计静态面 **41 个**。MCP/LSP 依赖启用配置，不能把它们或远程工具计为固定总量。

| 分组 | 当前工具 | 建议卡片/输入预览 | 执行增量策略 |
| --- | --- | --- | --- |
| 文件读/发现 | read, glob, grep | 文件位置、查询范围；code / locations / table | 批读按完成项、搜索按批；首版可仅阶段和终态 |
| 文件变更 | write, edit, apply_patch, copy_range, delete, confirm_apply | 目标、动作、draft diff；终态 diff | 只报告实际完成的步骤；估算与实际行差分离 |
| 命令/进程 | exec, process | shell/目标进程；streams / table | stdout/stderr、阶段；process 非长任务不强制流 |
| 网络/图像 | web_fetch, read_image | URL/路径；text / resource / image | 下载阶段或真实字节数；不伪造百分比 |
| 审计/恢复 | journal, spy | 范围/动作；table / diff / resource | 查询分批或阶段；恢复以实际变更为准 |
| 会话任务 | todo_write, todo_update, todo_list | todo_list block | 不为短任务制造无意义进度 |
| 用户交互 | ask（另有 ask_user spec） | interaction_ref，预览只显示草稿 | pending 是交互态，不是完成态 |
| Skills/发现 | skill_activate, skill_list, skill_resource, tool_search | resource / table，激活状态 | 阶段或直接终态 |
| 子 agent | spawn_subagent, list_agents, send_message, followup_task, steer_agent, interject_agent, wait_agent, interrupt_agent | agent_ref / table，目标与消息摘要 | 引用子会话；不复制整个子会话流 |
| 共享任务 | task_create, task_claim, task_update, task_close, task_list | task_ref / table | 按实际宿主操作结果 |
| 消息板 | board_channel_create, board_thread_create, board_post, board_subscribe, board_list | board_ref / table | 操作收据；不把通知数量当任务成功率 |
| 动态工具 | mcp 聚合资源工具、mcp__… 远端工具、lsp 聚合工具 | 适配器提供通用语义块 | 有上游真实进度才转发；能力不支持就声明 none |

MCP 聚合工具涵盖 servers/resources/prompts；LSP 当前 ACTIONS 含 list_servers、definition、references、hover、documentSymbol、workspaceSymbol。不要沿用旧注释的“五操作”作为精确清单。

### 2.2 已有基础与缺口

| 层 | 当前证据 | v2 缺口/处理 |
| --- | --- | --- |
| typed 契约 | tool-core/tool_api/typed.rs：Args/Output 生成 schema，Output 投影 model/display/canonical | 继续复用；新增 preview 与 card 契约，不再造一套执行 SDK |
| 终态展示 | tool_api/display.rs：header/body/outcome/metrics；typed.rs 在成功返回后调用 display | 扩展为全生命周期；失败投影目前是默认空 display，需错误卡片 |
| 展示推断 | derive_default_header 按 command/pattern/path 猜 header；workspace/display.rs 有 JSON 字符串投影和 typed todo 重建 | 新工具必须显式声明；旧 fallback 仅在兼容适配层使用 |
| 输入流 | gate SDK 归一 ToolCallProgress；runtime/agent/turn_lap/gate.rs 打开 Prepared 卡片并调用 ArgLineSlot | 不完整 JSON 仍是片段；当前旁路主要是文件行数估算，没有通用字段预览协议 |
| 通用进度 | tool_api/progress.rs 有 Text/Phase/Content/Custom 与有界 ProgressSink | 当前常规 engine_tool 的 tool_call_context 传 None；execution.rs 兼容入口也传 None，需要真正接入消费链 |
| exec 进度 | tool_runtime.rs 使用 bounded_exec_progress_channel；engine_tool.rs drain；domain TimelineEvent::ToolProgress | 从 exec 专用通道收敛到通用语义事件；保留两条流及丢帧计数 |
| 持久化 | runtime/ringing/persistence_policy.rs 将 ToolProgress/ToolEstimated 视为瞬态；session canonical/tool_ledger.rs 存执行账本 | 保留事实/投影分离；快照可独立重建卡片，不能以 delta 作为唯一依据 |
| wire | runtime/timeline.rs::wire_display 唯一映射 SDK display；domain/timeline.rs 声明 wire | 保留单一映射职责；Rust DTO 导出 TS，不手写两套独立 schema |
| 大正文 | types/tool_result.rs output_ref 主要是极端大模型输出保护阀；display 正文上限 16,000 字符 | 建立独立 CardResource，不假定现有 output_ref 能提供完整 UI 正文 |

## 3. 输入入口：增量解析但不增量执行

流程：provider delta → call identity resolver → bounded incremental JSON parser → tool PreviewProjector → CardSnapshot/patch → 前端动画。

1. call id/name 尚未齐全时，按 `(turn_id, round_num, provider_index)` 建临时槽；拿到 id 后绑定同一槽，不能重新开第二张卡片。跨轮不能仅凭 index 合并。
2. 解析器消费新增片段，保存词法状态、容器栈、JSON Pointer 和已完成字段。不能每帧重新 parse 全量字符串，也不能用正则补括号猜 JSON。
3. 预览字段事件为 `FieldPreview { path, value, completeness: partial|complete, revision }`。字符串允许有界、正确解码的 partial；转义/Unicode 未完整不得发布残字符。数字/布尔/null 等到 token 完整才发布。数组按已完成元素展示，不冒充整数组完整。
4. PreviewProjector 由工具提供，接收只读字段视图；不得访问文件/网络、改变状态或运行工具。未知字段不自动上屏；凭证和敏感字段必须明确省略。
5. draft code/diff/行差带 `provisional=true`，前端显示“正在生成/预计”，不能显示“已写入”。估算被最终实际结果整体替换，不与实际行差相加。
6. 输入结束后，对原始完整 JSON 做最终解析、typed Args 校验和权限准入，再执行。预览解析不能代替正式反序列化。
7. malformed、过大、超深、取消/断流都有明确收口：丢弃或冻结草稿，显示宿主状态；尚未执行的调用不能记为业务执行失败或创建已执行事实。

建议起始预算（可配置，实施需测量）：JSON nesting ≤64，普通输入 ≤1 MiB，patch/write 可声明 ≤8 MiB；单预览文本 ≤4 Ki Unicode 字符；服务端合并更新 ≤20Hz。超限不是静默截断后执行，应拒绝原输入并给 invalid_arguments/input_limit_exceeded。

## 4. 卡片数据 API（提议 DTO）

下面是协议草图；最终 wire 由 Rust 类型生成，TS 示例只解释消费模型。

```ts
type ToolCardSnapshot = {
  schema_version: 2;
  card_id: string; // 宿主生成，单 call 对应稳定卡片
  call_id: string;
  tool: { name: string; source: 'builtin' | 'mcp' | 'lsp'; descriptor_revision: string };
  revision: number;
  lifecycle: 'receiving_input' | 'validating' | 'waiting_permission' |
    'queued' | 'running' | 'waiting_user' | 'backgrounded' |
    'succeeded' | 'partial' | 'failed' | 'cancelled' | 'timed_out' | 'indeterminate';
  title: string;
  summary?: string;
  input: { completeness: 'partial' | 'complete' | 'invalid'; provisional: boolean };
  blocks: CardBlock[];
  error?: CardError;
  metrics: { duration_ms?: number; output_bytes?: number; dropped_frames?: number };
  actions: CardAction[];
  fallback: { text: string };
};

type CardBlock = {
  id: string; // 工具语义稳定 ID，不是数组下标
  kind: string;
  version: number;
  label?: string;
  provisional: boolean;
  completeness: 'preview' | 'complete' | 'truncated';
  payload?: unknown; // 实际 Rust 为 typed tagged union，不是无约束 Value
  resource?: { id: string; revision: string; media_type: string; bytes: number };
  fallback: { text: string };
};
```

首批正式 block kinds：`text`、`code`、`diff`、`streams`、`table`、`locations`、`image`、`resource`、`phase`、`todo_list`、`agent_ref`、`task_ref`、`board_ref`、`interaction_ref`。

补充 `view_ref`：引用 Interactive Views 的 view_id 与版本；持续对象只存引用及有界摘要，不在每次工具结果里复制整棵实时视图。工具终态不能因被引用的 live view 更新而重新变成 running。

- code：language/path、正文或 resource；locations：path、1-based line/column 和 snippet。
- diff：files/hunks、actual added/removed；预览可缺实际坐标，不能用虚构行号填满结构。实际结构复用 FileMutationDelta spec，避免第三套行差事实。
- streams：stdout/stderr 分离；包含各自 offset、observed_bytes、retained_bytes、dropped_bytes。合并视图只能称 observed ordering，不声称还原 OS 两管道真实全序。
- table：typed columns、稳定 row id、有界首屏和 cursor；不下发整张无限列表。
- image/resource：传受访问控制的引用，不默认内联 base64 巨图。
- phase：阶段名与消息；仅存在真实 total 时才带 completed/total，允许 indeterminate。
- refs：引用权威宿主对象；ask_user 表单遵循 spec-ask-user-form.md，不嵌入一个新的表单协议。
- 所有未知 kind/version 降级显示 block fallback；未知卡片协议 major 显示整卡 fallback，不丢弃调用状态。

工具只提交语义内容；lifecycle、metrics、actions 可执行性由宿主校验/补全。前端注册 renderer 按 block kind，不按工具名称穷举 switch。新工具组合已有 blocks 通常不改前端。

## 5. 更新事件、排序与重连

复用现有 Ringing/Timeline 总线，不建立第二套独立真相流。以下事件名是逻辑名称，落地时映射至现有 bus envelope。

```ts
type CardEvent = {
  epoch: string; sequence: number; card_id: string;
  base_revision: number; revision: number;
  body:
    | { type: 'card_snapshot'; snapshot: ToolCardSnapshot }
    | { type: 'card_patch'; ops: CardOp[] }
    | { type: 'card_finished'; snapshot: ToolCardSnapshot };
};
type CardOp =
  | { op: 'set_summary'; value: string }
  | { op: 'upsert_block'; block: CardBlock }
  | { op: 'remove_block'; block_id: string }
  | { op: 'append_stream'; block_id: string; stream: 'stdout'|'stderr';
      offset: number; text: string; observed_bytes: number; dropped_bytes: number }
  | { op: 'set_phase'; block_id: string; phase: string; message: string };
```

禁止任意 JSON Patch 路径改写 lifecycle/permission/error。宿主状态更新以权威快照发布。`sequence` 使用现有会话顺序语义；`revision` 在单卡片单调增长。超过 JS 精确整数范围的序号必须用 string 或受限整数，不能无条件把 Rust u64 导为 number。

- 小于等于当前 revision 的重复事件忽略；base 不匹配/offset 不匹配时请求快照，不能继续盲 append。
- bytes/offset 按 UTF-8 字节计，Unicode 字符预算另计；不得用 JS string.length 计算网络 offset。
- 满队列时优先合并 phase/summary、裁剪 stream，计数并通知 gap。不可丢终态、权限或交互状态。
- `card_finished` 自包含、替换临时块；最终正文或持久 resource 足以恢复，不依赖全部进度帧到齐。
- 终态与 late progress 竞争时，先停止/排空 producer 再封口；封口后丢弃晚帧。backgrounded 是本次前台调用收据，后续进程生命周期由 process_ref 对象承载，不把已封口 call 再改回 running。
- 重连先装载 watermark 快照，再接受更晚事件；epoch 变更重建基线。回放显示状态，不重播历史打字动画。
- 无法确认崩溃前副作用是否执行时为 indeterminate，沿用 tool ledger 恢复规则；不要自动重试写工具或伪造 succeeded。

## 6. 错误规范

复用 ToolErrorKind/ToolErrorCode：code 继续满足 `^[a-z][a-z0-9_]*$`，工具特有码用前缀，例如 edit_hash_mismatch。不另引入点分错误码。

```ts
type CardError = {
  origin: 'tool' | 'adapter' | 'host';
  kind: string;
  code: string;
  message: string;
  hint?: string;
  retryable: boolean;
  safe_to_repeat: boolean;
  details?: { field_errors?: { path: string; code: string; message: string }[] };
};
```

- 工具产出业务错误；参数反序列化、准入、超时、取消、panic/传输故障由 adapter/host 产出。要求“所有错误由工具自身产生”在工具尚未运行或崩溃时不成立。
- status 是失败权威；error 槽与 status 必须一致，不能解析 stderr 或 summary 猜失败。
- retryable 不代表可以安全重做。写/发消息/恢复等操作，副作用不确定时 safe_to_repeat=false；无自动重试动作。
- partial 允许有效结果块和分项错误；用户取消不是红色业务报错。权限等待不是 permission_denied。
- fatal 继续上抛 runtime，模型不得看到 stacktrace；UI 给宿主 internal_error 和诊断 correlation id，模型补齐路径保持现有恢复语义。
- 前端不按 message 文案判断；code 用于稳定定位，kind 用于通用视觉。不会为每个工具的每个错误码新写组件。

## 7. 工具 SDK 的建议接口

保留 TypedTool::run 和 ToolProjection::model_blocks；新增工具拥有的渲染适配器，概念接口如下（非可编译承诺）：

```rust
trait ToolRenderer {
    fn render_meta(&self) -> RenderMeta;
    fn preview(&self, fields: &PartialArgsView) -> CardContent;
    fn render_output(&self, output: &CanonicalOutput) -> CardContent;
    fn render_error(&self, error: &ToolError) -> CardContent;
}
// ctx.progress 统一提供 phase / append_stream / upsert_block 等 typed API。
// 宿主盖章 call_id、revision、metrics；工具不能填写 wire envelope。
```

实际实现建议 typed 工具采用 associated Renderer/Output::card，而不是让作者再 parse CanonicalOutput JSON。上面类型擦除视图仅是注册边界。错误渲染可有 SDK 默认实现，但最终一定得到可用卡片；“默认实现”不得回归 command/path 猜业务语义。

RenderMeta 声明 input preview 字段、内容限额、支持的 blocks/versions 和 execution_progress 能力。新 typed 工具注册缺失 renderer 时不通过验收。远程 MCP/LSP 由桥接适配器承担工具作者角色，通用 fallback 是正式 adapter 输出而非前端考古。

自定义 block 必须登记 kind/version/schema/fallback 和客户端支持；第一版优先标准块，不加载服务端下发的 JS/HTML，也不自动加载任意 renderer URL。

## 8. 前端按需消费 API

### 8.1 客户端侧 API

```ts
useToolCard(cardId, { detail: 'summary' | 'preview' | 'expanded' });
useCardResource(cardId, blockId, { cursor?: string, limit?: number });
registerCardRenderer({ kind, versions, render });
dispatchCardAction(cardId, actionId, expectedRevision);
```

这是跨端抽象，不要求 Windows 客户端使用 React hook；各端可以实现同语义 adapter。

### 8.2 后端逻辑操作（route 名待现有 daemon API 评审）

| 操作 | 返回/约束 |
| --- | --- |
| get_card(card_id, detail) | 当前 revision 的 summary/preview/expanded 快照 |
| subscribe_cards(after_sequence, detail, card_ids?) | 复用总线，摘要默认；订阅扩展 detail 先给基线 |
| read_card_resource(card_id, block_id, resource_revision, cursor, limit) | 有界页、next_cursor、截断/过期状态；cursor 绑定 revision，不可混读两版 |
| invoke_card_action(card_id, action_id, expected_revision, request_id) | 幂等操作回执；权限检查、过期检查，冲突返回最新 revision |

CardAction 仅允许宿主白名单语义（expand/copy/open_resource/open_session/open_diff/cancel 等）；可重试操作必须重新走当前权限和安全性判定。前端可以隐藏不支持的 action，不能把工具输出字符串当命令执行。interaction answer 仍走既有交互 API，不走通用 action 任意提交。

所有读取校验 session/workspace 权限；资源 id 不等于任意文件路径或任意 URL 读取权。页面 cursor 是 opaque token。资源引用有明确保留策略：终态/历史引用持久 blob；缓存资源过期返回 resource_expired 和可用 fallback，不能暗中重新运行原工具。对接当前尚在变更的 canonical blobs 实现，复用存储而非另造临时文件体系。

摘要订阅不下发巨大正文。不可见卡片可以只保留 summary；展开时按 revision 加载正文。订阅降档不取消工具、不改变结果或模型投影。订阅过滤造成的全局 sequence 跳跃不是丢帧，应以 card revision/gap 标志判定。

## 9. 动画约定

| 状态/内容 | 前端表现 |
| --- | --- |
| receiving_input | 出现稳定卡片骨架；允许字段渐显、draft 代码行出现，标注草稿 |
| validating/queued | 不确定进度指示；不假装命令运行 |
| waiting_permission/waiting_user | 停止执行动画；展示权威交互引用 |
| running + streams | 追加到对应流，视图按帧合并；用户上滚后不强制滚底 |
| running + phase | 阶段过渡；未知 total 不画伪百分比 |
| terminal | 替换 provisional 内容，短暂收口；错误明确显示 code 和可操作 hint |
| reconnect/history/reduced motion | 直接恢复静态状态；不重播所有动画 |

动画不产生业务事实；不要把 typing 完成视为输入完整，不用 spinner 是否停止判断成功。读取大正文不能触发旧执行动画。

## 10. 全部工具迁移路线

1. **协议切片**：在 domain 定义 Card DTO、tool-core 定义 renderer，唯一 wire mapper；Rust→TS 导出及未知 variant 降级。保持现有 fact bus/ledger 不变。
2. **纵向样板**：exec（双流）、write/edit/apply_patch（输入预览/diff）、ask/ask_user（交互引用）贯通 provider→parser→renderer→snapshot→客户端。验证取消、错参、断流、重连。
3. **全注册面迁移**：按 §2 表覆盖 23+18 静态工具及启用的 MCP/LSP adapter；read/grep/glob、todo/task/board、skills/web/image、journal/spy 不得留前端工具名特判。静态 41 是本次基线，测试应从实际注册器枚举而非永久硬编码数量。
4. **通用流收敛**：接通 ctx.progress，迁 exec 通道；明确每工具 none/phase/stream/blocks 能力，删除双发。保留真实丢失统计和终态权威。
5. **前端接入**：独立仓按 blocks 注册 renderer，detail 订阅/展开读取、交互 refs、动画；新旧 wire 协商，不要求所有客户端同时升级。
6. **清理旧路**：新历史保存完整卡片终态；旧会话由隔离 LegacyCardAdapter 映射 display，不伪造缺失数据。观察后删除 runtime 前端式推断、legacy JSON 字符串解析、ToolEstimated 专用估算事件（由 preview block 替代）。兼容保留期按历史格式版本决定。

不建议先逐个重画 41 张 UI，再补协议；先做最复杂的三个纵向样板，确认状态、资源和恢复语义，然后批量迁移全部工具。

## 11. 验收门槛

- 注册器枚举的所有工具（含配置启用的动态工具）都有显式渲染适配器、错误默认卡片、能力声明和 fallback。
- 输入解析覆盖逐字节切分、半个 escape/Unicode、嵌套数组、id/name 延迟、并行交错 call、错 JSON、超限和取消；preview 从不执行副作用。
- provider 归一化验证 OpenAI/Responses/Anthropic 的等价字段预览，不因重复累计帧双 append。
- exec 两流、满队列丢失、offset gap、慢客户端、取消/timeout、late progress 和 backgrounded 均可正确收口。
- 只拿终态快照能重建与实时收口等价的卡片；断线/epoch 切换/重复乱序帧不会重复内容。
- 失败、partial、取消、permission wait、indeterminate 不互相混淆；无模型文本/错误文案考古。
- summary 订阅不含完整大正文；分页 revision 冲突、历史资源保留/过期、访问控制有明确行为。
- 未知 block/version 显示 fallback；无能力客户端不影响工具执行。状态同步与动画完全解耦。
- 原 model projection、tool ledger、权限准入、文件变更事实与副作用边界不因渲染重写漂移。

## 12. 需要评审的产品选择（不阻塞此提案）

建议默认：标准 blocks 优先；流式输入只做预览；最终正文按需读取；历史回放不重播；自定义 renderer 首版不开放。后续需共同评审的范围是跨仓 wire 升级、资源保留配额与最大输入预算，不是是否让前端重新从工具文本猜结构。
