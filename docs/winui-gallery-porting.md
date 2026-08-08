# WinUI Gallery / Community Toolkit 视觉移植

DeepX 不把 WinUI Gallery 当作运行时样式依赖。Gallery 是微软官方的控件与
Fluent Design 样例应用；实际 WinUI 控件与默认样式来自 Windows App SDK。
DeepX 将经过筛选的视觉语义移植到 `deepx-fluent`，缺少的原生能力补到
`windows-reactor`。

## 首批移植：SettingsCard

来源与基线：

- WinUI Gallery：<https://github.com/microsoft/WinUI-Gallery>
- Windows Community Toolkit SettingsControls：
  <https://github.com/CommunityToolkit/Windows/tree/main/components/SettingsControls>
- 两个仓库均采用 MIT 许可证；本实现重新组合 reactor 原生元素，没有复制
  Toolkit 的完整 `ControlTemplate`。

本地 API：

- `deepx_fluent::settings_card(header, description, content)`
- `deepx_fluent::settings_section_header(title, description)`
- `tokens::SETTINGS_CARD_WRAP_THRESHOLD`（476 DIP，上游自适应阈值）
- `tokens::SETTINGS_CARD_ACTION_MIN_WIDTH`

映射原则：

| 上游语义 | DeepX 实现 |
|---|---|
| Header | 14 DIP、Semibold、可换行的 TextBlock |
| Description | SecondaryText、12 DIP、UIA help text |
| Content | 右侧原生 WinUI 控件，不包装输入行为 |
| Surface | CardBackground + CardStroke + 8 DIP radius |
| Row | 最低 64 DIP，16×12 DIP padding |
| Section | Level 2 automation heading |
| Theme | 全部使用 ThemeRef，继承 Light/Dark/High Contrast |

## 当前边界

Community Toolkit 在宽度小于 476 DIP 时把 header/content 改为上下堆叠，小于
286 DIP 时进一步隐藏 header icon。当前 `windows-reactor` 尚未投影
`AdaptiveTrigger` / XAML VisualState，因此首版使用桌面双列布局并保留官方阈值。
不得通过硬编码窗口宽度或在页面复制两棵控件树模拟响应式；应在 reactor 获得通用
adaptive primitive 后集中补齐。

首批真实接入点是 `apps/winui/src/settings_view.rs`：所有标准字段行使用
`settings_card`，分类标题使用 `settings_section_header`。页面业务状态、输入事件和
Bridge 命令没有移动到视觉库。

## 后续移植门禁

1. 先确认是 WinUI 平台控件、Community Toolkit 控件还是 Gallery 应用私有组件；
2. 平台已有控件时优先补 reactor binding，不重新实现模板；
3. 组合范式进入 `deepx-fluent`，不得依赖 Ringing、Bridge 或页面 ViewModel；
4. 使用语义 ThemeResource，覆盖 High Contrast，不复制固定色值；
5. 保留键盘、焦点、UI Automation、loading/empty/error/disabled 状态；
6. 记录上游文件和阈值，更新时按语义 diff，而不是整段覆盖本地实现。

验证：

```powershell
$env:CARGO_INCREMENTAL='0'
cargo test -p deepx-fluent
cargo check -p deepx-winui
cargo test -p deepx-winui --bin deepx-winui
```
