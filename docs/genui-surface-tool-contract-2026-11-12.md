# render_surface 工具契约 v0（A2UI 工具链路）

SUPERSEDED-BY: docs/widget-directive-protocol-2026-11-12.md（QAQH Gen-UI v1，2026-11-12 定稿）。
A2UI/render_surface 链路已即时移除，不留兼容层。本文档仅作历史逆向参考。

日期 2026-11-12 · 状态：定稿开工 · 关联：qaqh-electron `webui/src/genui/v2/*`（wire 镜像与渲染器事实源）

## 0. 一句话

模型通过 `render_surface` 工具提交 **A2UI v1.0 createSurface 帧**（扁平组件表 + 可嵌套
dataModel）；后端 `qaqh-genui` 按 catalog 校验，**失败打回模型自修复，通过才上 wire**；
前端 `SurfaceView2` 只消费已校验消息流。围栏 DSL（0.1 IR）退役。

## 1. 分层与投递

```
模型 args ──qaqh-genui 校验(catalog/结构/预算)──┬─ 失败 → ToolError VALIDATION_FAILED(打回重试,不到前端)
                                               └─ 通过 → ToolProjection
                                                     model_blocks: 小 summary(不复述数据)
                                                     display: ToolBody::Surface{renderer:"a2ui", feed:JSONL}
                                   → wire ToolResultDisplayBody::Surface → timeline → webui StepRow → SurfaceView2
```

wire 形态（`qaqh-types::ToolResultDisplayBody` 新变体，tag=kind/snake_case，旧 client
经 `#[serde(other)] Unknown` 自动降级）：

```jsonc
{"kind":"surface","renderer":"a2ui","feed":"{\"version\":\"v1.0\",\"createSurface\":{...}}\n"}
```

`feed` = A2UI 消息 JSONL（v0 单帧 createSurface），与前端 `UiFormat:"jsonl"` 解析管线同格式。

## 2. 工具面

- 名 `render_surface`，category `Read`，risk `ReadOnly`，timeout 5s。
- Args（`deny_unknown_fields`）：

```rust
pub struct RenderSurfaceArgs {
    pub surface_id: String,                    // ≤64 字符,[a-zA-Z0-9_-]+
    pub components: Vec<SurfaceComponent>,     // 扁平表;{id, component, #[serde(flatten)] props}
    pub data_model: Map<String, Value>,        // 允许嵌套的唯一处
}
```

- Output：`{surface_id, component_count, feed_bytes}`；canonical 单行 JSONL createSurface。

## 3. Catalog v0（14 组件，与 SurfaceView2 一一对应）

信封：`id`(必填)、`component`(必填)、可选 `child:string`、`children:string[]`、
`show:DynamicValue`（条件渲染）。未知 prop 一律拒绝（封闭 props）。

| 组件 | 专有 props | 容器 | 说明 |
|---|---|---|---|
| Card | title?:Dyn | ✓ | 卡片 |
| Stack | — | ✓ | 纵排 |
| Row | — | ✓ | 横排 |
| TodoList | title?:Dyn | ✓ | 任务组 |
| Title | text:Dyn | — | |
| Text | text:Dyn | — | |
| Metric | label:Dyn, value:Dyn | — | |
| Badge | text:Dyn | — | |
| Divider | — | — | |
| Button | label:Dyn, onPress:{event:{name,userMessage?,context?}}, disabled?:Dyn | — | |
| TextInput | label?, value:Dyn(绑定), placeholder?, maxLength?:number | — | 写回路径必须 `/s/` 前缀 |
| Checkbox | label:Dyn, checked:Dyn(绑定) | — | 写回同上 |
| Todo | text:Dyn, done:Dyn(绑定), meta?:Dyn | — | 写回同上 |
| TagList | items:Dyn(解析为数组) | — | |

Dyn = 字面量 | `{"@path":"/abs/pointer"}` | 模板内相对 `{"@path":"rel"}`（仅 repeat 子树合法，渲染期逐项解析）。
`@call` 白名单：仅系统函数 `@index`（模板下标；args 只允许数字 offset）。

模板展开（repeat）：`metadata.extensions` 下 repeat 扩展 `{"path":"/abs/list"}`，
path 必须命中数组；被展开的子组件按列表逐项渲染，scope 内相对路径由前端解析。
**root 即顶层组件本体**：渲染器渲染 root 自身并递归展开其 children，不另设包装。

## 4. 校验规则（顺序即报错优先级）

1. 预算：components ≤64；feed ≤24_000 字符；嵌套深度(dataModel 与 children 链) ≤8。
2. `id` 唯一；**必须存在 id=`"root"` 的组件**（前端以 root 起步）；root 必须是容器。
3. 组件名 ∈ catalog；props 封闭集；类型对。
4. `child/children` 引用的 id 必须存在；叶子组件不得携带子引用。
5. 树连通：root 可达全部组件（孤儿拒）；引用无环（`UNALLOWED_PARENT` 语义并入结构错）。
6. 每个 `@path`（绝对 JSON Pointer）必须命中 dataModel；repeat/items 的 path 命中数组。
7. 绑定写回 props（TextInput.value/Checkbox.checked/Todo.done）的 `@path` 必须以 `/s/` 开头且在 dataModel 存在。
8. `onPress.event.name` 非空；`context` 内 `@path` 同规则 6。

错误返回（Recoverable → 模型可见，驱动单组件 patch 重试）：

```jsonc
{"code":"VALIDATION_FAILED","componentId":"m1","field":"value","reason":"@path /build/elapsed not found in dataModel"}
```

多条错误合并返回（≤10 条 + 溢出计数），全部带 componentId 定位。

## 5. 模型面纪律

`model_blocks` 只回 summary envelope：`surface "build-panel" rendered · 12 components`，
**不复述组件表与数据**（messages.jsonl 尺寸纪律，对齐 clamp_display_body 的动机）。
action 回程 v0：前端 SurfaceView2 沿用 followUp 语义把 event 拼成用户消息排队，无后端改动。

## 6. 变更面清单

- 新 crate `crates/qaqh-genui`：`wire.rs`(A2UI v1.0 子集) `catalog.rs`(§3 表) `validate.rs`(§4) `tool.rs`。
- `qaqh-tool-core/tool_api/display.rs`：`ToolBody::Surface{renderer,feed}`。
- `qaqh-tool-core/tool_api/output.rs`：to/from_wire_display 两个 match 加臂。
- `qaqh-tool-core/lib.rs`：`ToolEffect::SurfaceRendered{surface_id,feed}`（**webui 投递通道**：
  RingingV2 的 `ToolCallView.effects`，经 `tool_calls.effects_json` 持久化，历史恢复同源）。
- `qaqh-types/tool_result.rs`：`ToolResultDisplayBody::Surface{renderer,feed}`（CLI/HTTP 展示面）。
- `qaqh-ringing-contract/state.rs`：`ToolEffectInfo::SurfaceRendered`（ts-rs 导出 → webui DTO）。
- `qaqh-agent-runtime/tool_job.rs`：`effect_infos()` 加臂。
- `qaqh-workspace/registration.rs`：`qaqh_genui::register(&mut mgr)`（+Cargo.toml 依赖）。
- 前端（qaqh-electron）：`StepRow` 见 `effects.surface_rendered` → 懒加载 SurfaceView2 直接消费
  feed（不编译）；围栏 DSL 全链退役（decoder/compiler/SurfaceView v1/translate 删除）；
  `AssistantContent` 回归纯 Markdown；`genui-check` 页与 CDP 脚本改工具链路用例。
