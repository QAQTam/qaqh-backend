# DeepX WinUI 原生控件复用与设计审查

> 状态：设计基线  
> 更新日期：2026-08-11  
> 范围：`apps/winui` 中除 `chat_view.rs` 以外的界面  
> 目标：优先复用 WinUI 3 / Windows Community Toolkit 的真实控件，减少手写交互控件，提升 Windows 11 原生感、动效一致性、键盘体验、可访问性和高对比度兼容性。

## 1. 结论摘要

DeepX 的 WinUI 前端位于 `apps/winui`，使用 Rust + `windows-reactor` 构建真实 XAML/WinUI 控件树，不是 WebView UI。

当前基础控件并不缺失：`TitleBar`、`TabView`、`NavigationView`、`TextBox`、`NumberBox`、`ComboBox`、`RadioButton`、`ToggleSwitch`、`MenuFlyout`、`ContentDialog`、`Expander` 和 `ProgressRing` 已经在使用。

主要问题是部分 `Grid`、`Border`、`StackPanel` 不再只是布局容器，而是承担了以下标准控件的职责：

- 顶级导航与页面路由；
- 模态审批和焦点拦截；
- 列表选择和可点击卡片；
- 展开/折叠；
- 命令栏和切换按钮；
- 进度、错误与警告提示；
- 可拖拽分隔条。

这些手写实现只能复刻静态外观，无法自动获得 WinUI 控件模板提供的 `Normal / PointerOver / Pressed / Selected / Focused / Disabled` VisualState、系统动画、键盘导航、UI Automation 和高对比度行为。

建议按三层推进：

1. 先替换 Reactor 已支持的控件：`GridView`、`ListView`、`Expander`、`ProgressBar`、`InfoBar`、`SelectorBar`、`ToggleButton`、`AutoSuggestBox`。
2. 再补 Reactor 的关键能力：富内容 `ContentDialog`、完整 `CommandBar` 命令状态、`Frame` 与导航过渡、完整 `NavigationView` 内容槽。
3. 最后评估 Windows Community Toolkit：`SettingsCard`、`SettingsExpander`、`GridSplitter`。

## 2. 设计原则

### 2.1 何时必须优先使用真实控件

只要元素承担以下职责，就应先寻找 WinUI/Gallery/Toolkit 控件：

| 需求 | 首选控件 |
|---|---|
| 2–7 个固定页面导航 | `NavigationView` |
| 文档或会话标签 | `TabView` |
| 2–3 个模式切换 | `SelectorBar` |
| 垂直集合 | `ListView` |
| 响应式卡片集合 | `GridView`；必要时 `ItemsRepeater` |
| 展开详情 | `Expander` |
| 搜索和建议 | `AutoSuggestBox` |
| 应用命令 | `CommandBar` + `AppBarButton` / `AppBarToggleButton` |
| 阻塞式决策 | `ContentDialog` |
| 上下文操作 | `Flyout` / `MenuFlyout` |
| 行内错误、警告和重要状态 | `InfoBar` |
| 确定进度 | `ProgressBar` |
| 不确定进度 | `ProgressRing` |
| 可拖拽布局分隔 | Toolkit `GridSplitter` |
| Windows 11 设置行 | Toolkit `SettingsCard` / `SettingsExpander` |

### 2.2 何时可以继续使用 Grid/Border

以下场景保留自定义组合是合理的：

- 非交互式布局和间距组织；
- 数据可视化，例如活动热力图、Token 构成条、统计矩阵；
- 状态点、元数据徽标等简单被动视觉；
- 没有标准控件对应的领域信息卡；
- `DataTemplate` 内部的自定义内容布局。

判断标准不是“有没有 Border”，而是“这个 Border 是否在假扮一个已有的交互控件”。

### 2.3 自定义交互控件的准入条件

只有同时满足以下条件，才应保留手写交互：

- WinUI Gallery 和 Toolkit 均无合适控件；
- 已定义默认、悬停、按压、禁用、选中、焦点和错误状态；
- 支持键盘、屏幕阅读器、UI Automation 和高对比度；
- 支持系统关闭动画；
- 有自动化测试覆盖关键交互。

## 3. 当前已正确使用的原生控件

以下实现方向正确，不建议为了“统一”而重新手写：

| 区域 | 当前原生控件 | 位置 |
|---|---|---|
| 自定义窗口标题栏 | `TitleBar` | `apps/winui/src/header.rs` |
| 会话标签 | `TabView` | `apps/winui/src/session_tabs.rs` |
| 设置分类导航 | `NavigationView`，固定 `PaneDisplayMode::Left` | `apps/winui/src/settings_view.rs` |
| 常规输入 | `TextBox`、`PasswordBox`、`NumberBox`、`ComboBox` | `settings_view.rs`、`composer_bar.rs` |
| 单选/布尔输入 | `RadioButton(s)`、`CheckBox`、`ToggleSwitch` | `settings_view.rs`、`interaction_overlay.rs`、`skills_view.rs` |
| 附件菜单 | `MenuFlyout` | `apps/winui/src/composer_bar.rs` |
| 会话删除确认 | `ContentDialog` | `apps/winui/src/sidebar.rs` |
| Todo 详情 | `Expander` | `apps/winui/src/info_panel.rs` |
| 不确定进度 | `ProgressRing` | 多处 |
| 文件/目录选择 | Windows Shell Picker | `apps/winui/src/bridge.rs` |
| 窗口材质 | Mica/系统主题资源 | WinUI host 与 `deepx-fluent` |

`session_tabs.rs` 的 `TabView` 与 DeepX 的文档/会话编辑器形态匹配，应作为主要会话切换控件继续保留。官方说明：`TabView` 适合动态打开、关闭和重排的文档标签，而较多固定页面应使用 `NavigationView`。

## 4. P0：优先设计与底层补齐

### 4.1 应用壳：NavigationView + Frame

**现状**

- `sidebar.rs` 用手写 `Grid/Border/Button` 复刻 NavigationView 外观；
- `main.rs` 用字符串 `view` 和四组 `GridLength::Pixel(0) / STAR` 控制页面挂载；
- 页面动画由自定义 Composition transition 模拟；
- 首页、技能、设置没有原生导航返回栈和 `RecommendedNavigationTransitionInfo`。

**目标**

- 顶级页面使用 `NavigationView`；
- 内容区域使用 `Frame`；
- `NavigationView.SelectionChanged` 通过 Tag 映射到强类型 Route；
- 使用事件参数提供的 `RecommendedNavigationTransitionInfo`；
- 只使用 `SelectionChanged` 或 `ItemInvoked` 之一，不双重路由；
- 会话 `TabView` 与顶级页面导航职责分离。

**需要先解决的产品设计问题**

- 左侧会话列表和顶部 `TabView` 是否都保留；
- 首页/技能/设置属于顶级路由，聊天是否属于固定工作区或一个可导航页面；
- 页面返回栈是否只覆盖顶级页面，还是也记录设置分类；
- 左侧会话列表是 NavigationView 动态菜单，还是独立的辅助 `SplitView` pane。

**Reactor 缺口**

- 缺少公开的 `Frame` 控件封装；
- 缺少 `NavigationTransitionInfo` / `FrameNavigationOptions` 通道；
- `NavViewItem` 内容仅支持字符串和图标，不能承载状态点、标题和行内删除；
- NavigationView wrapper 尚未公开 `FooterMenuItems`；
- 需要 PaneHeader/PaneFooter/动态菜单项的完整富内容能力。

**验收结果**

- UI Automation 中顶级项类型为 `NavigationViewItem`；
- 鼠标、方向键、Tab、Enter/Space 均可完成导航；
- 选中条和页面过渡由 WinUI 模板/推荐过渡负责；
- 页面切换不依赖手写行高归零；
- 页面路由使用 enum，而不是散落字符串。

### 4.2 交互审批：手写遮罩改为 ContentDialog

**现状**

`interaction_overlay.rs` 用全窗半透明 Grid、实底 Border 和手写按钮行实现 permission、ask、plan 三种模态交互。

**问题**

- 没有平台级焦点陷阱；
- Esc、默认按钮和关闭语义需要自行维护；
- 模态 UI Automation 语义不完整；
- 弹出/关闭动画不由 ContentDialog 模板负责；
- 高对比度、窗口缩放和软键盘场景更难保证。

**目标**

- 外壳使用 `ContentDialog`；
- permission：Primary=批准，Secondary/Close=拒绝或取消；
- ask：Primary=确认，Close=跳过；
- plan：Primary=批准，Secondary=拒绝；
- 内部现有 `RadioButton`、`CheckBox`、`TextBox`、计划列表继续作为富内容；
- 高风险操作使用明确动词，不使用模糊的“确定”。

**Reactor 缺口**

当前 `ContentDialog` wrapper 只支持字符串 `content`。需要像 rich Flyout 一样增加 Element content slot，并由 reconciler 管理内容子树生命周期。

### 4.3 标题栏命令区：CommandBar

**现状**

`header.rs` 在 `TitleBar.footer` 中用 HStack 拼接工作区、打开目录、统计、撤销和压缩等图标按钮。

**目标**

- 使用 `CommandBar`；
- 工作区、打开目录、撤销、压缩使用 `AppBarButton`；
- 统计面板使用 `AppBarToggleButton`；
- 窄窗口时低优先级命令进入 SecondaryCommands；
- 每个图标命令保留 Label、Tooltip、AutomationName 和稳定 ID；
- “会话信息常驻”不再显示一个永远禁用的伪按钮。

**Reactor 缺口**

当前 CommandBar 已可创建 Button/Toggle/Separator，但命令定义缺少：

- `is_enabled`；
- toggle `is_checked`；
- per-command callback 或稳定 command ID；
- tooltip / automation metadata；
- 可选自定义内容，例如压缩中的 ProgressRing。

## 5. P1：现有 Reactor 可直接复用

### 5.1 首页最近任务：GridView

**现状**

`home_view.rs` 把最近 12 个任务固定排成四列 Grid，每个任务用可点击 Border 处理 PointerPressed。

**建议**

- 改用原生 `GridView`；
- 卡片内容继续作为 ItemTemplate；
- 用受控选中或 ItemClick 恢复会话；
- 交给 GridView 提供 hover、pressed、focus、键盘方向导航和响应式换行；
- 不再把列数写死为四列；
- 空态与集合态使用不同稳定 key。

**保留自定义**

活动热力图属于数据可视化，继续使用 Grid/Border 更合适。

### 5.2 技能页：AutoSuggestBox + ListView + Expander

**现状**

- 搜索使用普通 TextBox；
- 四列各自使用 `ScrollViewer + VStack`；
- 技能卡通过 Grid 的 PointerPressed 手写展开；
- 卡片外壳是 Border。

**建议**

- 搜索框改为 `AutoSuggestBox`；
- 每列改为约束高度的 `ListView`，不要在 ListView 外再套 ScrollViewer；
- 技能详情改用 `Expander`；
- 卡片动作继续用 `ToggleSwitch`、Button 和 `ProgressRing`；
- 错误详情可放在 Expander 内容内，必要时使用 Error `InfoBar`；
- 窄窗口下可用 `SelectorBar` 切换四种状态，宽窗口保留四列看板。

### 5.3 信息面板：ProgressBar

**现状**

`info_panel.rs::progress_bar` 用两列 Star Grid 模拟百分比填充。

**建议**

- 使用 `ProgressBar::new(value).range(0, 100)`；
- 通过语义 ThemeResource 设置前景；
- 保留百分比文本；
- 为进度条设置 AutomationName；
- Todo 的 `Expander + ProgressRing` 保持不变。

状态点和 Token 四格统计属于被动数据展示，可以继续自定义，不必强行改成控件。

### 5.4 Composer 控制区

ChatView 本身不在本审查范围内，但 Composer 属于应用命令面，应继续原生化。

| 当前设计 | 建议 |
|---|---|
| “执行/规划”循环按钮 | 两项 `SelectorBar`；空间不足时使用带明确状态的 `ToggleButton` |
| 沉浸式图标按钮 | `ToggleButton`，让 checked 状态进入控件模板 |
| 发送失败红色 TextBlock | Error `InfoBar` |
| 工作区/队列重要状态 | `InfoBar` + `ListView` |
| 附件菜单 | 保留当前原生 `MenuFlyout` |
| 发送/停止 | 保留原生 Button；运行状态注意稳定 UIA 名称 |
| Token 构成条 | 保留自定义数据可视化 |

**斜杠菜单**

当前斜杠菜单是 Border + Button 列表。理想目标是锚定消息 TextBox 的原生 Flyout/Popup，内容使用 `ListBox` 或 `ListView`。`AutoSuggestBox` 默认是单行输入，不能直接替换多行 composer，因此不能为了使用建议列表而牺牲多行编辑能力。

Reactor 需要补充“任意 FrameworkElement 作为 Flyout/Popup 锚点”，而不是只允许 Button 承载 Flyout。

### 5.5 设置页的剩余标准化

当前分类导航已经是真正的 NavigationView，输入控件也大部分正确。剩余问题集中在设置卡和反馈。

**无需 Reactor 扩展即可完成**

- 删除当前预设前使用现有 `ContentDialog` 二次确认；
- 工作区重启提示改为 Warning `InfoBar`；
- 保存失败改为 Error `InfoBar`；
- 导出成功和路径可用 Success/Informational `InfoBar`；
- 操作较多的工作区区块可整理为 `CommandBar`，前提是命令状态能力足够。

**需要 Toolkit**

- `deepx_fluent::settings_card` 替换为真正的 `SettingsCard`；
- 相关设置组使用 `SettingsExpander`；
- 使用 Toolkit 自带布局和 VisualState，而不是复制模板。

注意：`SettingsCard` / `SettingsExpander` 属于 Windows Community Toolkit，不是 WinUI 核心控件。Rust 投影、资源字典加载、NuGet/打包依赖和运行时激活都需要单独验证。

### 5.6 左侧会话列表的选择语义

**现状**

- 已使用原生 ListView 进行虚拟化；
- 但 `SelectionMode::None` 禁用了原生选择；
- 标题 TextBlock 自己处理 PointerPressed；
- 选中背景和强调色竖条全部手写；
- 活动与归档两个 ListView 外部又套了一层 ScrollViewer。

**近期方案**

- 使用 ListView 的受控 `selected_index` 和 `on_selection_changed`；
- 保留状态点、标题和删除按钮作为 ItemTemplate；
- 验证点击行内删除按钮不会误触发会话恢复；
- 去掉 ListView 外层 ScrollViewer；
- 如果两个分组必须共享一个滚动视口，优先增加 grouped ListView/CollectionViewSource 能力，而不是嵌套滚动。

**最终方案**

与应用壳 NavigationView 设计一并决定：会话可能成为富内容 NavigationViewItem，也可能保留在独立 SplitView pane 中。

## 6. P2：Windows Community Toolkit 评估

### 6.1 GridSplitter

当前有两套手写 pointer capture 分隔条：

- `sidebar.rs`：调整左侧栏宽度；
- `composer_bar.rs`：调整消息编辑器高度。

建议接入 Toolkit `GridSplitter`，获得：

- 标准光标和拖拽反馈；
- 键盘调整；
- UI Automation resize 语义；
- 方向、最小/最大尺寸和焦点状态；
- 更少的 pointer capture 边界错误。

在接入前，现有实现必须补齐键盘替代操作；只有鼠标拖拽不满足可访问性要求。

### 6.2 SettingsCard / SettingsExpander

接入前需完成一个独立技术验证：

- Rust WinRT bindings 能否投影 Toolkit 控件；
- Toolkit 资源字典是否随应用正确加载；
- unpackaged/packaged 两种启动方式是否都可激活；
- Light、Dark、High Contrast 是否均能解析资源；
- 安装包是否包含所需 DLL 和 PRI 资源；
- 与 windows-reactor reconciler 的属性更新和生命周期是否兼容。

如果验证失败，现有 `deepx_fluent::settings_card` 可以作为临时实现，但必须补自适应堆叠、完整 VisualState 和高对比度测试，不应继续复制 Toolkit ControlTemplate。

## 7. 应保留的自定义设计

以下部分自定义是有价值的，不应为了控件覆盖率而替换：

- 首页活动热力图；
- Composer Token 构成分段条；
- 信息面板 Token 统计矩阵；
- 会话/任务状态点；
- 模型缓存命中统计卡；
- Mica 窗口承载与标题栏布局；
- 只承担背景、分组、描边和间距的被动卡片；
- 领域特有的工具、证据和计划内容布局。

这些区域仍需遵守：

- 颜色来自 ThemeResource/语义 ThemeRef；
- Light、Dark、High Contrast 都可辨认；
- 文本不依赖颜色表达唯一语义；
- 数据变化时不强制持续动画；
- 自定义动画遵守 Windows client-area animation 设置。

## 8. Reactor 能力建设清单

路径依赖见 `apps/winui/Cargo.toml`，当前指向相邻的 `deepx-winui-reactor` 工作区。

### R0：页面导航

- [ ] 新增 `Frame` Element/Widget；
- [ ] 支持页面 key/route 导航；
- [ ] 支持 back stack；
- [ ] 支持 `NavigationTransitionInfo`；
- [ ] 接收 `RecommendedNavigationTransitionInfo`；
- [ ] 提供重复路由抑制。

### R1：NavigationView 完整内容模型

- [ ] `NavViewItem` 支持 Element content；
- [ ] 支持富 Header、PaneHeader、PaneFooter；
- [ ] 支持 `FooterMenuItems`；
- [ ] 支持动态 MenuItemsSource/稳定 key；
- [ ] 支持 item enabled、visibility 和 automation metadata；
- [ ] 选中状态与受控 route 同步不产生回调循环。

### R2：富内容 ContentDialog

- [ ] Element content slot；
- [ ] DefaultButton；
- [ ] XamlRoot/窗口绑定；
- [ ] Primary/Secondary/Close 按钮 enabled 状态；
- [ ] Opening/Closing/Closed 生命周期；
- [ ] 内容子树卸载和异步关闭安全。

### R3：CommandBar 命令状态

- [ ] 稳定 command ID；
- [ ] per-command callback；
- [ ] enabled/checked；
- [ ] tooltip/automation metadata；
- [ ] Primary/Secondary overflow；
- [ ] 可选自定义 command content。

### R4：集合与弹层

- [ ] grouped ListView / CollectionViewSource；
- [ ] Rich item click/selection 与行内按钮事件隔离；
- [ ] 任意 FrameworkElement Flyout anchor；
- [ ] Popup/ListBox 的焦点返回和 Esc 关闭；
- [ ] GridView 响应式 item sizing。

### R5：Toolkit 可行性

- [ ] `GridSplitter` 技术样例；
- [ ] `SettingsCard` 技术样例；
- [ ] `SettingsExpander` 技术样例；
- [ ] 资源和打包验证；
- [ ] High Contrast 验证。

## 9. 推荐实施阶段

### Phase A：不改 Reactor 的高收益替换

- [ ] 首页最近任务改为 GridView；
- [ ] 技能搜索改为 AutoSuggestBox；
- [ ] 技能列改为 ListView；
- [ ] 技能卡改为 Expander；
- [ ] 信息面板改用 ProgressBar；
- [ ] Composer 模式改为 SelectorBar/ToggleButton；
- [ ] 错误和警告改为 InfoBar；
- [ ] 删除预设增加 ContentDialog；
- [ ] 清理 ListView 外层 ScrollViewer。

### Phase B：小型 Reactor 扩展

- [ ] 富内容 ContentDialog；
- [ ] 完整 CommandBar 命令状态；
- [ ] rich Flyout anchor；
- [ ] grouped ListView 或统一滚动集合；
- [ ] 迁移 interaction overlay；
- [ ] 迁移标题栏命令区。

### Phase C：应用壳导航

- [ ] 定义强类型 Route；
- [ ] 新增 Frame；
- [ ] 补全 NavigationView 内容模型；
- [ ] 决定左侧会话列表与顶部 TabView 的职责；
- [ ] 迁移首页/技能/设置/聊天页面；
- [ ] 删除字符串轮询和 0/STAR 行切换；
- [ ] 使用推荐导航过渡。

### Phase D：Toolkit 控件

- [ ] GridSplitter；
- [ ] SettingsCard；
- [ ] SettingsExpander；
- [ ] 安装包与资源验证。

## 10. 动效规则

原生感不等于“动画越多越好”。建议遵循以下层级：

| 场景 | 动效来源 |
|---|---|
| NavigationView 选中条、hover、pressed | 控件模板 |
| Frame 页面切换 | `RecommendedNavigationTransitionInfo` |
| TabView 选择、关闭、重排 | 控件模板 |
| ContentDialog/Flyout/MenuFlyout | 平台弹层动画 |
| Expander 展开 | 控件模板 |
| ProgressRing/ProgressBar | 控件模板 |
| 新增集合项 | 短 reveal；只在 keyed 新项 mount 时触发 |
| 自定义数据变化 | 默认即时更新，必要时使用短隐式 transition |

所有自定义 Composition 动画必须检查 Windows client-area animation 设置。系统关闭动画时，不应保留持续旋转、位移或 shimmer；仅保留必要状态变化。

## 11. 验收矩阵

每完成一个控件迁移，至少验证以下内容。

### 11.1 输入与焦点

- [ ] 仅使用键盘可以到达并操作；
- [ ] Tab 顺序符合视觉顺序；
- [ ] 方向键符合控件惯例；
- [ ] Enter/Space 行为正确；
- [ ] Esc 关闭 Dialog/Flyout 后焦点回到触发元素；
- [ ] 不出现焦点落在已卸载元素上。

### 11.2 可访问性

- [ ] UI Automation ControlType 正确；
- [ ] Name、HelpText、AutomationId 稳定；
- [ ] 选中、展开、切换、进度等状态可被读取；
- [ ] 不只靠颜色表达 working/error/selected；
- [ ] 200% 文本缩放下不裁切主要操作。

### 11.3 主题与视觉

- [ ] Light；
- [ ] Dark；
- [ ] 至少一种 High Contrast 主题；
- [ ] 系统强调色变化；
- [ ] 非活动窗口状态；
- [ ] 100% / 150% / 200% DPI；
- [ ] 窄窗口和宽窗口；
- [ ] Windows 关闭动画。

### 11.4 运行时稳定性

- [ ] `cargo check -p deepx-winui`；
- [ ] `cargo test -p deepx-winui`；
- [ ] `cargo test -p deepx-fluent`；
- [ ] UI 自动化完成导航、选择、展开和模态操作；
- [ ] 日志无 `reactor fault`、`on_frame failed`、XAML attach/detach fault；
- [ ] 重复切换 50 次不发生控件树错位或假死；
- [ ] 关闭页面后 timer/on_frame/flyout/dialog 正确清理。

## 12. 设计阶段必须先确认的问题

在开始应用壳重构前，应先形成明确决策：

1. 会话左侧列表与顶部 TabView 是否同时保留；如果保留，二者分别解决什么问题？
2. 首页、技能、设置和聊天的 Route 层级是什么？
3. 信息面板常驻还是可以关闭？标题栏是否还需要信息按钮？
4. 设置页是否允许引入 Windows Community Toolkit 运行时依赖？
5. Composer 的执行/规划模式是持久设置、当前会话状态还是单次消息选项？
6. 窄窗口下四列技能看板如何退化：横向滚动、单列 SelectorBar，还是两列布局？
7. 左侧栏可调宽是否属于必要功能；如果是，键盘调整规范是什么？
8. 页面是否需要返回历史；设置分类是否进入 back stack？

这些问题会直接决定 NavigationView、Frame、SplitView、TabView 和 SelectorBar 的职责，不能只靠视觉稿决定。

## 13. 参考资料

- [NavigationView](https://learn.microsoft.com/en-us/windows/apps/design/controls/navigationview)
- [TabView](https://learn.microsoft.com/en-us/windows/apps/design/controls/tab-view)
- [CommandBar](https://learn.microsoft.com/en-us/windows/apps/develop/ui/controls/command-bar)
- [InfoBar](https://learn.microsoft.com/en-us/windows/apps/design/controls/infobar)
- [ContentDialog](https://learn.microsoft.com/en-us/windows/apps/design/controls/dialogs-and-flyouts/dialogs)
- [AutoSuggestBox](https://learn.microsoft.com/en-us/windows/apps/design/controls/auto-suggest-box)
- [ListView and GridView](https://learn.microsoft.com/en-us/windows/apps/design/controls/listview-and-gridview)
- [Expander](https://learn.microsoft.com/en-us/windows/apps/design/controls/expander)
- [Windows Community Toolkit Settings controls](https://learn.microsoft.com/en-us/dotnet/communitytoolkit/windows/settingscontrols/settingscard)
- [Windows Community Toolkit GridSplitter](https://learn.microsoft.com/en-us/dotnet/communitytoolkit/windows/sizers/gridsplitter)

---

本文件是设计和实现边界清单，不要求一次性替换所有自定义布局。每次迁移都应以“平台控件是否真正承担了交互职责”为验收依据，而不是仅检查视觉是否接近 Windows 11。
