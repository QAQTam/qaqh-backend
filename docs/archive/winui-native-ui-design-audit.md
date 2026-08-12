# DeepX WinUI 原生 UI 设计审计

> 状态：初步盘点  
> 日期：2026-08-11  
> 范围：`apps/winui` 的页面结构、控件选择、响应式布局、键盘与无障碍语义  
> 暂不纳入：正在修改的设置页、近期 WinUI 3 动画问题、motion token 与 Composition 实现

## 1. 目标

识别从 Web UI 直接移植后仍保留的设计思维，并将其映射为更符合 XAML、WinUI 3 和 Fluent Design 的原生实现。

这里关注的不是“圆角是否好看”，而是以下结构性问题：

- 是否用 `Border + Grid + Pointer` 模拟平台已有控件；
- 是否绕开平台的选择、焦点、键盘、UI Automation 和高对比度语义；
- 是否使用固定桌面宽度，而没有面向可缩放窗口的响应式状态；
- 是否在页面内容树里手工实现弹层、菜单和滚动容器；
- 是否把后端协议事件一对一映射成 UI 卡片，而不是按用户任务组织信息。

## 2. 结论摘要

当前实现已经不是 WebView，也大量使用了原生 WinUI 控件，但仍有几处明显的 Web 移植遗留。

最高优先级是：

1. 将自制交互遮罩迁移为 `ContentDialog`；
2. 重构左侧会话栏的选择与滚动模型；
3. 为侧栏、正文和信息栏建立窗口宽度断点；
4. 消除首页、技能页和侧栏中的 Pointer-only 可点击容器；
5. 将技能页和 Slash 菜单迁移到平台集合与建议控件。

不建议全面重写。会话 `TabView`、ChatView 虚拟化 `ListView`、原生表单控件、删除确认和主题资源体系已经走在正确方向上。

## 3. 审计优先级

| 优先级 | 区域 | 当前形态 | 推荐 WinUI 形态 |
|---|---|---|---|
| P0 | 交互请求 | 全窗 `Grid` + 固定宽卡片 + 自绘遮罩 | `ContentDialog` |
| P0 | 会话侧栏 | 手写导航外观、禁用选择、Pointer 点击 | `SplitView` + 单个分组 `ListView` |
| P0 | Shell 响应式 | 固定侧栏和信息栏宽度 | Visual State / Adaptive Trigger + Overlay pane |
| P1 | 技能页 | 固定四列、每列独立滚动、Pointer 展开 | `GridView` / `ItemsView` / `ItemsRepeater` + `Expander` |
| P1 | 首页和卡片 | `Border` 承担按钮职责 | `Button` / `ListViewItem` / `Expander` |
| P1 | Slash 菜单 | 页面树内 `Border + VStack<Button>` | `AutoSuggestBox` 或 `Flyout + ListView` |
| P2 | 信息栏仪表 | Border 宽度模拟进度条 | `ProgressBar` |
| P2 | 标题栏命令 | 固定 `HStack` 图标按钮 | `CommandBar` 或窄窗 Overflow 策略 |
| P2 | 状态同步 | 多视图 250/500ms revision 轮询 | 集中状态订阅或 Reactor 通知 |

## 4. 详细发现

### 4.1 P0：交互请求仍是 Web modal 的原生复刻

证据：

- [`apps/winui/src/interaction_overlay.rs`](../apps/winui/src/interaction_overlay.rs) 明确以 Web `PermissionPrompt` / `AskUserPrompt` 为参考；
- 卡片固定为 `420 DIP`；
- 使用全窗 `Grid` 和硬编码半透明黑色背景模拟 scrim；
- 弹窗动作、焦点和关闭行为均由页面状态自行管理。

问题：

- 没有天然的焦点圈定；
- Esc、默认按钮、取消按钮和关闭后焦点恢复需要自行维护；
- Narrator 不一定能获得正确的模态窗口通告；
- 固定宽度在窄窗口、文本缩放和本地化环境下容易溢出；
- 自绘遮罩需要额外验证高对比度主题；
- 页面级覆盖层容易与 splash、其他 overlay 和 z-order 互相干扰。

建议：

- permission、ask、plan 统一使用 `ContentDialog`；
- 对话框正文继续复用现有 `RadioButton`、`TextBox`、`CheckBox` 和风险信息；
- 多题 ask 可在 `ContentDialog` 内容区使用受限高度的 `ScrollViewer`；
- 将批准动作设为 Primary，拒绝或跳过设为 Close/Secondary；
- 不在业务视图里重新实现 scrim、焦点恢复和键盘关闭。

验收条件：

- Tab 焦点不能逃离对话框；
- Esc 行为明确且可测试；
- Primary/Close 按钮有正确默认语义；
- 对话框关闭后焦点回到此前控件；
- 200% 文本缩放下内容可访问；
- Light、Dark、High Contrast 均可读。

### 4.2 P0：侧栏绕开了 ListView 的原生选择模型

证据：

- [`apps/winui/src/sidebar.rs`](../apps/winui/src/sidebar.rs) 文件头明确说明“仿 NavigationView，但容器用手写 Grid”；
- 活动和归档分别使用一个 `ListView`；
- 两个列表都使用 `SelectionMode::None`；
- 会话标题通过 `TextBlock.on_pointer_pressed` 切换；
- 两个 `ListView` 外又包了一层 `ScrollViewer`。

问题：

- 外层 `ScrollViewer` 会破坏或削弱集合控件自身的虚拟化和滚动行为；
- 禁用选择后需要手工重建选中背景、选中竖条和状态同步；
- TextBlock 不是可聚焦按钮，键盘用户无法自然使用方向键、Enter 和 Space；
- 选择、当前项和活动状态被混在视觉样式里，UI Automation 语义不足；
- 活动、归档两个滚动集合会产生不一致的焦点和滚动体验。

建议：

- 保留会话侧栏作为文档工作区的一部分，不强制把所有会话塞进 `NavigationView`；
- 外层改用 `SplitView` 管理 pane 的 Inline/Overlay 状态；
- 会话数据合并为单个分组 `ListView`；
- 恢复 `Single` 选择模式和 `SelectionChanged`；
- 活动/归档作为分组头或可折叠分组；
- 删除仍保留行内原生 `Button` 和 `ContentDialog` 确认；
- 拖拽调整宽度可以保留，但必须有键盘或命令式替代路径。

验收条件：

- 上下方向键可移动会话选择；
- Enter 可打开当前会话；
- 当前会话通过 Selection 语义暴露给 UIA；
- 列表内部自行滚动，不再套外层 `ScrollViewer`；
- 归档组不需要第二个独立滚动容器。

### 4.3 P0：Shell 缺少真正的响应式状态

证据：

- [`apps/winui/src/main.rs`](../apps/winui/src/main.rs) 使用固定侧栏宽度；
- [`apps/winui/src/info_panel.rs`](../apps/winui/src/info_panel.rs) 使用固定 `320 DIP` 右栏；
- 主布局没有按窗口有效像素切换 pane 模式；
- 当前缩窗行为主要依赖 `GridLength::Star` 被动挤压正文。

推荐断点：

| 窗口宽度 | 侧栏 | 信息栏 | 主内容 |
|---|---|---|---|
| `>= 1280 epx` | Inline，允许调整宽度 | Inline，320 DIP | 完整阅读宽度 |
| `1008–1279 epx` | Inline 或 CompactInline | Overlay，按需打开 | 优先保证正文 |
| `640–1007 epx` | CompactOverlay | 默认隐藏 | 单主列 |
| `< 640 epx` | Overlay | 隐藏，通过命令打开 | 单列，标签与命令进一步压缩 |

实现路径：

- 平台层优先 `VisualStateManager + AdaptiveTrigger`；
- 如果当前 `windows-reactor` 尚未包装 AdaptiveTrigger，可在壳层订阅尺寸变化并维护明确的 `ShellLayoutState`；
- 右栏用 `SplitView`，避免用 `GridLength` 在 `0` 和 `320` 之间手工切换；
- 标题栏和命令区必须有 Overflow 路径，不能只做隐藏。

验收条件：

- 640、1008、1280 epx 附近缩放不会跳变或裁切；
- 主输入框始终可用；
- 侧栏或信息栏隐藏后仍有明确打开入口；
- 200% 文本缩放不导致关键命令消失。

### 4.4 P1：多处可点击卡片仍是 Pointer-only

已发现位置：

- 侧栏会话标题和品牌文字；
- [`apps/winui/src/home_view.rs`](../apps/winui/src/home_view.rs) 的最近任务卡片；
- [`apps/winui/src/skills_view.rs`](../apps/winui/src/skills_view.rs) 的技能卡片主行；
- Composer 和侧栏的拖拽手柄。

问题：

- `Border`、`Grid` 和 `TextBlock` 不会自动获得按钮或选择项语义；
- PointerPressed 不等价于 Click，容易忽略键盘、触摸、笔和辅助技术；
- 缺少默认 hover、pressed、disabled 和 focus visual；
- 需要手工补充 AutomationName、AutomationId 和角色。

推荐映射：

| 当前交互 | 推荐控件 |
|---|---|
| 打开会话 | `ListViewItem` 选择或 `Button` |
| 展开技能详情 | `Expander` |
| 首页任务卡 | `Button`、`GridViewItem` 或 `ListViewItem` |
| 品牌返回首页 | `Button` 或导航项 |
| 可拖拽宽度 | 保留指针捕获，同时增加重置命令和键盘调整路径 |

### 4.5 P1：技能页仍是固定 Web Kanban 布局

证据：

- [`apps/winui/src/skills_view.rs`](../apps/winui/src/skills_view.rs) 固定四个等宽列；
- 每列内部使用 `ScrollViewer + VStack<Card>`；
- 卡片使用 `Border`，主行通过 Pointer 展开；
- 搜索使用普通 `TextBox`，建议结果和过滤语义由页面自行管理。

问题：

- 固定四列无法适应窄窗和文本缩放；
- 多个独立滚动区增加滚轮、触摸和键盘焦点复杂度；
- 卡片列表没有集合控件的虚拟化、选择与容器语义；
- Pointer-only 展开不符合原生键盘模型。

建议方案：

优先方案：

- 顶部使用 `AutoSuggestBox` 搜索；
- 使用 `SelectorBar` 切换 catalog/requested/active/unavailable；
- 主区域使用单个 `ListView` 或 `GridView`；
- 技能详情使用 `Expander`；
- 操作继续使用 `ToggleSwitch` 和 `Button`。

如果必须保留看板：

- 使用响应式 `ItemsRepeater`/`GridView`；
- 宽窗四列、中窗两列、窄窗一列；
- 每列使用真正的集合控件，而不是 `ScrollViewer + VStack`；
- 避免四个彼此竞争的垂直滚动区。

### 4.6 P1：Slash 命令菜单不是平台 Popup

证据：

- [`apps/winui/src/composer_bar.rs`](../apps/winui/src/composer_bar.rs) 在 Composer 内容树中渲染 `Border + VStack<Button>`；
- 菜单作为普通布局元素占据输入框上方空间；
- 选中态通过按钮样式切换模拟。

风险：

- 菜单会改变 Composer 高度和对话视口；
- 焦点、Esc、上下方向键、PageUp/PageDown 和关闭规则需要手工维护；
- 容易受到父级裁切、z-order 和窗口边缘空间影响；
- 屏幕阅读器无法自然识别建议列表。

建议：

- 首选 `AutoSuggestBox` 的 suggestion 列表；
- 如果需要保留当前 TextBox，则使用锚定 `Flyout + ListView`；
- 当前命令匹配、选中索引和回车确认逻辑可以继续复用；
- 菜单显示不应改变 Composer 的布局高度。

### 4.7 P2：信息栏自绘了平台已有的进度控件

证据：

- [`apps/winui/src/info_panel.rs`](../apps/winui/src/info_panel.rs) 使用两个 `Border` 和 Star 列宽模拟进度条。

问题：

- 没有 `Value`、`Minimum`、`Maximum` 等进度语义；
- Narrator 无法直接读出百分比；
- 高对比度和系统主题适配需要手工验证；
- 后续增加 indeterminate、暂停或错误状态会继续扩大自定义实现。

建议：

- 上下文占用、缓存命中率统一使用原生 `ProgressBar`；
- 百分比文本继续保留，避免只依赖颜色和图形；
- 设置明确的 AutomationName，例如“缓存命中率 95.6%”。

### 4.8 P2：标题栏命令缺少 Overflow 策略

[`apps/winui/src/header.rs`](../apps/winui/src/header.rs) 的图标按钮已具备 Tooltip 和 AutomationName，基本交互质量较好。目前主要问题是固定 `HStack` 在窄窗口下没有命令优先级和 Overflow。

建议：

- 保留自定义标题栏拖拽区域；
- 命令区域改用 `CommandBar`，或实现等价的 Primary/Secondary 命令分层；
- 窄窗优先保留高频命令，其他进入 `More` 菜单；
- 不允许关键命令仅通过 `Collapsed` 消失。

### 4.9 P2：视图局部轮询仍带有 Web 投影架构痕迹

侧栏、标签、首页、信息栏和交互层分别维护 250/500ms revision 轮询。它不属于控件规范错误，但会产生：

- 同一状态存在多个刷新节拍；
- UI 最多延迟一个轮询周期；
- 不可见视图仍可能持有计时器；
- 测试需要处理额外的时间与生命周期状态。

长期建议：

- Bridge 提供集中、类型化的 snapshot subscription；
- Reactor 状态由事件或订阅推送，而不是页面自行轮询；
- 视图只投影状态，不持有 transport 刷新节奏；
- 保留低频健康检查，但不用于常规 UI 同步。

这项工作应独立于 UI 控件迁移，避免同时扩大改动范围。

## 5. 已经符合原生方向的部分

以下内容不建议因为本次审计而重写：

- [`apps/winui/src/session_tabs.rs`](../apps/winui/src/session_tabs.rs) 已使用原生 `TabView`；
- ChatView 已使用虚拟化 `ListView` 和稳定 key；
- 会话删除已经使用 `ContentDialog`；
- Composer 使用原生 `TextBox`、按钮、快捷键和 AutomationProperties；
- permission/ask 正文已经使用 `CheckBox`、`RadioButton`、`TextBox` 等原生表单控件；
- 技能操作已经使用 `ToggleSwitch`、`ProgressRing` 和 `Button`；
- 颜色主要来自 `ThemeRef` 语义资源，而不是硬编码 Light/Dark 色值；
- 工具与思考活动已经开始按用户可理解的活动组聚合；
- 设置页正在转向 `NavigationView + SettingsCard` 语义，本审计不干预其进行中的实现。

## 6. 推荐实施顺序

### 阶段 A：模态与键盘语义

1. 将 interaction overlay 迁移到 `ContentDialog`；
2. 将首页、技能页和侧栏的 Pointer-only 点击替换为原生可交互控件；
3. 添加键盘遍历和 UIA 回归测试。

收益：立即改善安全交互、键盘可用性、Narrator 和高对比度行为。

### 阶段 B：侧栏与响应式 Shell

1. 合并侧栏会话集合；
2. 恢复 ListView 选择模型；
3. 移除 ListView 外层 ScrollViewer；
4. 用 SplitView 管理侧栏和信息栏；
5. 建立 640/1008/1280 epx 响应式状态。

收益：解决缩窗、滚动、选择和工作区空间分配问题。

### 阶段 C：集合与弹出界面

1. 重构技能页集合；
2. 将 Slash 菜单迁移到 AutoSuggestBox 或 Flyout；
3. 首页任务卡改为 GridView/ItemsView；
4. 信息栏自绘进度条替换为 ProgressBar。

收益：减少自定义交互代码，提升虚拟化与响应式能力。

### 阶段 D：状态架构收敛

1. 设计集中 snapshot subscription；
2. 逐步移除视图局部轮询；
3. 将健康检查与 UI 更新解耦。

收益：减少延迟、计时器和生命周期复杂度。

## 7. 回归检查清单

每个迁移项至少覆盖：

- [ ] 鼠标点击、悬停和滚轮；
- [ ] 键盘 Tab、Shift+Tab、方向键、Enter、Space、Esc；
- [ ] 触摸和高 DPI；
- [ ] Light、Dark、High Contrast；
- [ ] 100%、150%、200% 文本缩放；
- [ ] Narrator 的名称、角色、状态和值；
- [ ] 小于 640、641–1007、1008 以上有效像素窗口；
- [ ] Loading、Empty、Error、Offline/permission denied；
- [ ] 选中项与焦点项区分；
- [ ] 命令隐藏后仍有 Overflow 或替代入口；
- [ ] 集合内部滚动，不在 ListView/GridView 外嵌套 ScrollViewer。

## 8. 本轮边界

本文件仅记录审计与后续方向，不授权或实施以下改动：

- 不修改正在进行中的设置页面；
- 不调整近期 WinUI 3 动画问题；
- 不修改 `deepx-fluent::motion`、Composition animation 或全局动画 token；
- 不在同一批次同时进行状态架构和 UI 控件迁移；
- 不为追求“原生”而重写已经正确使用的 `TabView`、`ListView` 和标准输入控件。

