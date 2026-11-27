# ADR-0046 无 WebView 的原生结构化可视化策略（HTML iframe widget 退役）

状态: `已采纳 · 未实施`（排期 `v4.10 P4-1`）· 起草 QAQ agent · 依据 [ADR-0042](0042-ringing-v3-rust-native-ipc-and-quic.md)（`已采纳 · 未实施`）铁律 6「保护前端帧率与内存预算」与 `v4.6` 宪法 #2 / #5 / #6 · 处置对象 [widget-directive-protocol-2026-11-12.md](../widget-directive-protocol-2026-11-12.md)

> 一句话：`v4.7` 换 GPUI 之后**没有 WebView**，那条"模型写 HTML fragment → 宿主用
> `srcdoc` 装进沙箱 iframe → `window.qqh` 桥回流"的 widget 协议**整体退役**，
> 不留兼容层。可视化的数据面改走 V3 强类型帧，渲染面交给原生组件。
>
> **编号说明**：`V4_6_DETAILED_DESIGN_PLAN.md` §二.5 原拟把本 ADR 编为 `0045`
> （审计/日志那条编 `0046`）。`v4.6 P3-2` 已占用 `0045`（`qaqh-core-vocab`），
> 故按 `V4_6_EXECUTION_PLAN.md` §三.4 的"从 `0046` 起"顺延为 **`0046`（可视化）/
> `0047`（审计与 `JOURNAL_IO`）**，文件内容与决策不变。

## 背景

1. `widget-directive-protocol-2026-11-12.md` 是一份**为 Electron 壳设计的**协议：
   三个 PUA 码位做行内/块级 marker、fragment 目录、宿主 `srcdoc` 装配、
   `window.qqh` 桥（`setWidgetState` / `sendFollowUpMessage` / `openExternal`）、
   `sandbox="allow-scripts …"` 且禁 `allow-same-origin`。它在 WebView 宿主里是自洽的。
2. `v4.7` 的落点是 `qaqh-gpui`（纯原生、无 WebView）。在那上面，
   **没有 iframe、没有 `srcdoc`、没有 `window`**，于是这份协议的每一层都失去物理载体：
   解析 marker 无处渲染，装配 fragment 无处执行，桥脚本没有宿主对象。
3. 最危险的"省事"做法是**留一层兼容**：GPUI 里内嵌一个 webview 只为跑 widget。
   那等于把 `v4.7` 的意义抹掉——用户拿到的是一个更重的 Electron，
   而代价（内存、帧率、跨平台构建、沙箱攻击面）全部保留。
4. 宪法 #5 的初心标尺是"**开箱即渲染 + 保护前端帧率**"：这条标尺要求的是
   **强类型、可分帧、可增量**的渲染契约，而不是"往 UI 里塞一份 HTML 文本"。

## 决策

1. **`widget-directive-protocol-2026-11-12.md` 正式退役**：降级为历史文档，
   不再是任何实现方的协议真源。PUA marker、`srcdoc` 装配、`window.qqh` 桥、
   `__QQH_WIDGET_STATE__` 占位注入、`qqh-widget://`（早已裁撤）
   **一律不作为 `v4.7+` 的协议面**，也**不留解析/转换层**（宪法 #2：
   退役即物理删除，不写"为了兼容旧前端"的空壳）。
2. **可视化的数据面 = V3 强类型帧**：一次可视化的内容由结构化 DTO 描述，
   经 `SessionEvents` 流送达；大数据表/图片走 `ContentRef::Blob`（内容寻址，
   客户端按需取），**绝不以 HTML 文本形式进入任何帧**。
3. **渲染面 = 原生组件**：`qaqh-gpui` 按强类型组件描述渲染，组件树增量更新
   走 `Replaceable` 帧并由后端同流合帧（`ADR-0042` 铁律 4 与守卫⑥的量化红线）。
   这条把"widget"从"一段可执行的不受信 HTML"变成"一份可校验的组件数据"。
4. **交互回流保留语义、更换通道**：`sendFollowUpMessage` 的**产品语义**
   （用户确认后带着控件当前状态发一条 follow-up）保留，但走
   `ControlCall::Command::SendPrompt` + 结构化 `widget_context`；
   动态部分一律 `JsonText(String)`，**禁止 `serde_json::Value`**
   （`ADR-0042` 铁律 1）。
5. **`privateContent` 永不出前端存储**（原协议的这条纪律继续有效）：它是
   前端 UI store 的私有快照，服务端与模型都看不到；`modelContent` 才回流。
6. **状态注入手法作废**：不再有"把状态 JSON 塞进 script 标签再由桥脚本 parse"
   这套（那是 HTML 时代的做法）。状态就是强类型帧本身，增量更新，不需要占位符替换。
7. **C2 `visualize` 工具的**位置**不变**：模型仍然发起"渲染一个交互式可视化"，
   变化只在**输出形态**（结构化描述 → 原生组件），不在工具语义。
8. **本 ADR 只定方向，实施在 `v4.10 P4-1`**：届时新增的 V3 组件描述 DTO
   必须遵守 `ADR-0045` 的目录纪律（不带 schema、不带版本前缀）与
   `ADR-0042` 的零 `serde_json` 红线。

## 被否选项

- **在 GPUI 里内嵌 webview 以复用 HTML widget**：把 `v4.7` 的原生收益全部抵消，
  且保留整条不受信 HTML 执行链的攻击面。为了一条协议把宿主降级，方向反了。
- **保留 marker 解析层"以后可能有用"**：无人渲染的解析器只会腐化成
  "看起来支持、点了没反应"的假功能，且每次协议演进都要跟着改（宪法 #2）。
- **用 Markdown + Mermaid 顶替可视化**：能看不能交互，
  而且同样把渲染负担推给"文本 → UI"的二次解析——正是宪法 #5 反对的方向。
- **把组件描述直接塞进 `SessionEvent` 的增量帧里（不设独立帧类型）**：
  会让高频 `Replaceable` 合帧逻辑与可视化组件的生命周期纠缠，
  届时"合帧吃掉半个组件"这类错误极难定位。组件描述必须是**独立帧**。

## 未验范围（诚实登记）

- `qaqh-gpui` 侧的原生组件渲染、增量更新与布局预算**本 ADR 未验证**（那是 `v4.7`/`v4.10` 的活）。
- 组件描述的最终 DTO 形状**尚未冻结**：本文只定"结构化 + 强类型 + 大内容外置"三条约束，
  具体字段在 `v4.10` 落库前另起 ADR 或附录补。
- 现有 Electron 壳（`qaqh-electron`）在 `v4.7` 之前**仍走**旧 widget 协议：
  退役的物理删除与 GPUI 切换同窗口发生，本文不要求"先删后切"。
