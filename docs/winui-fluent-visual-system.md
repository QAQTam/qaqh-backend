# DeepX WinUI / Fluent 视觉系统

> 状态：Phase 1 已落地；Phase 2 Composer 已落地（2026-08-08）
>
> 范围：`deepx-fluent`、`apps/winui`、`markdown-winui`
> 目标：让 DeepX 成为遵循 Windows 11 交互与视觉语义的原生工作台，而不是把 Web
> 聊天页面逐像素搬进 WinUI。

## 1. 设计判断

DeepX 已经使用真正的 WinUI 3 控件和 Mica 窗口，但旧 ChatView 仍保留明显的 Web
表达：emoji 承担状态、用户与助手都是同类边框卡片、颜色写死为 RGB、长文本没有
阅读宽度、ListView 行仍有选择语义、思考和工具内容靠手工卡片层叠。

本视觉系统采用以下 Windows 11 原则：

1. **平台资源优先**：颜色、描边和状态使用 WinUI `ThemeResource`，不在组件中写死
   light/dark 色值。
2. **层级来自材质与留白**：Mica 是窗口基础层；长寿命内容用不透明 layer/card；
   Acrylic 只用于 flyout、menu、轻量弹出等临时表面。
3. **渐进圆角**：普通控件 4 DIP、卡片 8 DIP、独立会话 surface 12 DIP；避免所有
   内容都变成同一种大圆角卡片。
4. **Windows type ramp**：caption 12、body 14、body large 18、subtitle 20；正文常规，
   标题 semibold；界面文本采用 sentence case，不使用 emoji 代替状态或命令。
5. **原生控件表达行为**：披露内容用 `Expander`，状态用语义 badge，进度用
   `ProgressRing`，命令最终使用 Symbol/FontIcon + Tooltip，而不是 Web 风格字符按钮。
6. **可读性先于装饰**：助手长文使用开放画布和 880 DIP 阅读宽度；用户请求使用
   右对齐、较窄的强调 surface；代码和工具信息退居次级 surface。

官方依据：

- [Windows design principles](https://learn.microsoft.com/en-us/windows/apps/design/design-principles)
- [Typography in Windows](https://learn.microsoft.com/en-us/windows/apps/design/signature-experiences/typography)
- [Geometry in Windows](https://learn.microsoft.com/en-us/windows/apps/design/signature-experiences/geometry)
- [Materials in Windows apps](https://learn.microsoft.com/en-us/windows/apps/develop/ui/materials)
- [Mica and Acrylic system backdrops](https://learn.microsoft.com/en-us/windows/apps/develop/ui/system-backdrops)

## 2. Surface 架构

```text
Window / Mica
├── TitleBar / Mica continuous surface
├── Navigation pane / opaque or layer fill
└── Content layer
    ├── Transcript / open reading canvas
    │   ├── User message / accent-tinted, right aligned
    │   ├── Assistant answer / open surface, constrained measure
    │   ├── Thinking / native Expander
    │   ├── Tool activity / native Expander + semantic status
    │   └── Code / inset control surface
    └── Composer / elevated persistent command surface

Transient surfaces only
└── Flyout / menu / teaching tip / dialog → Acrylic where the control provides it
```

不要在侧栏、正文、composer、每条消息上连续叠加 Acrylic。微软的材质指南明确将
Acrylic 定位为临时或 light-dismiss surface；并排或嵌套的 Acrylic 会产生接缝、噪声和
可读性问题。

## 3. Token 基线

第一批 token 位于 `crates/deepx-fluent/src/lib.rs`：

| 类别 | Token | 值/来源 |
| --- | --- | --- |
| spacing | `SPACE_1/2/3/4/6` | 4/8/12/16/24 DIP |
| radius | `RADIUS_CONTROL` | 4 DIP |
| radius | `RADIUS_CARD` | 8 DIP |
| radius | `RADIUS_MESSAGE` | 12 DIP，仅独立消息 surface |
| type | caption/body/body-large/subtitle | 12/14/18/20 DIP |
| measure | `READING_MAX_WIDTH` | 880 DIP |
| measure | `USER_MESSAGE_MAX_WIDTH` | 720 DIP |
| brush | fill/stroke/text/status | `ThemeRef` → WinUI ThemeResource |

这些是语义 token，不是任意页面常量。若视觉验收后需要调整，应修改 token 或新增明确
variant，禁止重新在页面内散落 RGB 和魔法数。

### ThemeResource 使用约束

WinUI 资源名里的 `Secondary` / `Tertiary` 不等于“视觉层级较低”。很多资源是控件
VisualState 专用状态，例如 `ControlFillColorSecondaryBrush` 是 pointer-over，
`AccentFillColorSecondaryBrush` / `TertiaryBrush` 分别对应 pointer-over / pressed。它们
不能作为静态卡片或消息的 resting fill。

- 静态内容：`CardBackground*`、`LayerFill*` 与对应 stroke；
- 交互状态：由 Button、ComboBox、MenuFlyout 等原生控件模板消费 Control/Accent state
  brush，页面不直接借用；
- accent 实心背景上的文本：`TextOnAccentFillColorPrimaryBrush`；
- High Contrast：保持系统 ThemeResource 映射，不用 RGB 或自行计算透明度绕开系统资源。

每次新增 surface 时，必须先回答它是 resting content、persistent command surface，还是
transient interaction surface，再选择资源。

## 4. ChatView 语义

### 用户消息

- 右对齐，最大宽度 720 DIP；
- `CardBackground` resting fill，左侧 2 DIP accent ownership indicator；
- 作者和状态在独立 header 行；
- 状态使用“正在处理 / 已完成 / 失败”与系统 caution/success/critical 资源；
- 消息文本保持可选择，不把状态字符混入复制内容。

### 助手消息

- 不复制一个对称气泡；以开放内容画布呈现；
- `DeepX` 作者标签仅建立信息层级，不与正文争夺注意力；
- 正文最大宽度 880 DIP，以左对齐和稳定行长提高长文阅读效率；
- streaming 与 final 共享同一视觉容器，封口不应导致整体跳动。

### 思考、工具和代码

- 思考使用原生 `Expander`，默认折叠；
- 工具调用使用原生 `Expander`，header 采用文本状态，不使用脑、扳手、沙漏等 emoji；
- 代码块使用 secondary card resting fill、`CardStroke` 和 Cascadia Mono；
- 后续为工具 header 和代码复制动作增加真正的 `SymbolIcon`、Tooltip 和
  `CommandBarFlyout`，但不能用不可访问的 glyph 字符冒充按钮。

### 列表行为

- transcript 的 ListView 使用 `SelectionMode::None`；聊天行不是可选择业务对象；
- 文本自身继续支持 selection/copy；
- 每个 turn 使用稳定 key、24 DIP 横向 gutter 和虚拟化；
- 跟尾、顶部分页、锚点保持属于行为层，不得因换皮退化。

## 5. 通用 crate 边界

`deepx-fluent` 只拥有视觉语义和无状态构建函数：

- spacing、type、radius、reading measure token；
- `StatusTone` 与 theme-aware status badge；
- user/assistant conversation surface；
- inset/code surface；
- empty/loading state。

它不得依赖 Ringing、Bridge、Transcript 或页面 ViewModel。页面负责把领域状态映射为
`StatusTone`；通用 crate 只负责如何以 WinUI/Fluent 方式表达。未来 `markdown-winui`、
sidebar、settings 和独立诊断窗口可以复用同一 crate。

## 6. 后续阶段

### Phase 2：Composer 与 Shell（Composer 已完成）

1. [完成] Composer 使用 Windows 11 持久 command surface；输入区、附件、mode、
   permission、send/stop 建立主次动作层级。
2. [完成] 附件使用原生 `MenuFlyout`；权限四选一使用 `ComboBox`；发送、停止、删除和
   附件入口使用 `SymbolIcon`，icon-only 控件具备 Tooltip、AutomationName、AutomationId。
3. [待办] sidebar、tab strip、info pane 统一 selection indicator、layer fill 和 divider。
4. [持续约束] Mica 保留在窗口/标题栏基础层；面板不随意改 Acrylic。

### Phase 3：富内容与视觉状态

1. code surface 增加复制命令、水平滚动和语法 token theme；
2. table、quote、callout、error、permission 使用统一 surface variants；
3. streaming 使用克制的 progress/activity 状态，不给文本本身做持续闪烁动画；
4. 补 focus、pointer-over、pressed、disabled、high-contrast 状态。

### Phase 4：视觉 Gallery 与回归

建立独立 `deepx-fluent-gallery` 示例，以固定 fixture 展示：

- light/dark/high-contrast；
- 100%/150%/200% scaling；
- 空态、流式、长 Markdown、工具、错误、permission；
- 800×600、1200×800、窄窗口和 ultrawide。

视觉回归应截 Gallery 或指定 DeepX 窗口，不截整个桌面。

## 7. 安全截图协议

用户正在游戏、演示或处理敏感内容时：

1. 不调用全桌面截图，不使用会捕获当前前台窗口的快捷键；
2. 不启动、不激活、不置顶 DeepX，也不模拟 Alt+Tab；
3. 只有用户明确表示方便后，才启动视觉 Gallery 或 DeepX；
4. 优先按已确认的 DeepX HWND/进程进行窗口级捕获；捕获前验证标题和进程；
5. 若窗口最小化或 API 只能回退到桌面捕获，则停止，不生成截图；
6. 截图完成后先检查画面只包含目标应用，再用于视觉评审。

本轮没有截图，也没有启动应用窗口。

## 8. Microsoft winui-design skill

设计审查基线采用
[microsoft/win-dev-skills 的 winui-design](https://github.com/microsoft/win-dev-skills/tree/v0.5.0/plugins/winui/skills/winui-design)，
并固定到 `v0.5.0`。升级 skill 时先阅读 release/tag diff，再审查本文件中的资源、控件、
accessibility 和测试约束，不能把新示例语法机械复制到 Rust。

该 skill 面向通用 WinUI 3，并不限定 C++：ThemeResource、XAML 控件选择、Fluent
Design、键盘、UI Automation 和 High Contrast 规则对 C#、C++/WinRT、Rust/windows-rs
都适用。它的示例和 `winui-search` 工具主要是 C#/XAML / microsoft-ui-reactor 写法；
DeepX 只移植设计语义与控件映射，具体 API 使用 `windows-reactor` 投影。

仓库提供的 `winui-search.exe` 未签名，不纳入 DeepX 构建或 CI；需要检索时优先查官方
文档与可审计源码。

## 9. 验收门槛

- ChatView 生产代码不再用 literal RGB 或 emoji 表达状态；
- light/dark/high-contrast 下信息层级和状态均可辨认；
- 200% 文本缩放不截断主消息和关键命令；
- 键盘可到达所有动作，icon-only 动作有 Tooltip/AutomationName；
- streaming、跟尾、上滚分页和虚拟化测试不回归；
- 新视觉 primitive 必须进入 `deepx-fluent`，页面不得复制同类样式。
