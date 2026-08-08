# WinUI 原生化架构审计（Ringing 2026-08-01 基线）

> 状态：设计基线（2026-08-08）
> 范围：`deepx-domain`、`deepx-ringing`、`deepx-runtime`、`deepx-client`、
> `apps/winui` 与 `windows-reactor` 的桌面数据链。
> 目的：区分“使用原生 WinUI 控件”和“采用原生桌面应用架构”，并给出可逐步
> 回滚、可随 Ringing 演进同步的改造顺序。

## 1. 结论

当前实现不是 WebView，也不是 HTML/CSS 伪装的桌面程序。`windows-reactor`
最终创建的是 WinUI 3 控件，输入、焦点、可访问性和合成均由 Windows App SDK
承担；项目不必为了“原生”而改写为 C# 或重新引入传统 `.xaml` 标记文件。

但“控件是原生的”不等于“应用架构已经原生化”。目前桌面主链仍明显保留了
Electron/SolidJS renderer 的状态模型：

```text
Ringing SSE / query
  -> JSON envelope / JSON snapshot
  -> 4098 行 BridgeCore（全局缓存、revision、active_seed）
  -> DispatcherTimer 轮询 / 16ms drain
  -> 再次解析 JSON
  -> Transcript / 各视图局部 state
  -> WinUI 控件树
```

因此更准确的判断是：

| 层 | 当前性质 | 判断 |
| --- | --- | --- |
| WinUI 控件树与窗口 | 原生 | 已完成 WebView 移除，方向正确 |
| 领域模型与命令 | 大部分强类型 Rust | `deepx-domain` / `deepx-ringing` 已具备原生化基础 |
| timeline 生产、持久化、SSE | 原生且强类型 | 已是 transcript 的正确权威来源 |
| client / query / snapshot 边界 | JSON、字符串方法名、TS 兼容形状 | 仍是 Web/RPC 移植边界 |
| 壳层状态同步 | 全局 bridge 缓存、revision、定时轮询 | 仍是 browser store / preload bridge 思路 |
| transcript 消费 | JSON conversation + JSON timeline snapshot 双路径 | 尚未真正切到 native timeline |
| 导航与页面状态 | `current_view: String` + 定时检查 | 可工作，但不是强类型、事件驱动的桌面导航 |

用户关于“现在是 Web 移植方案”的判断在**应用状态与渲染数据流**上成立；若把
整个后端一概称为 Web 移植则不准确。后端领域核心和 timeline 已经原生化，真正
需要收口的是 shell-facing SDK、查询/快照投影和 WinUI 消费方式。

## 2. 判定标准

这里的“更贴近 XAML/WinUI 原生应用”不是要求使用某种源文件后缀，而是要求：

1. UI 消费强类型 ViewModel/状态，不在视图中遍历任意 JSON。
2. 状态变化主动推送到 UI `DispatcherQueue`，而不是所有视图各自轮询 revision。
3. 集合通过可观察变更或虚拟化数据源增量更新，不周期性重建整个快照。
4. 导航、命令、选择、焦点和生命周期是有类型的应用状态。
5. transcript 只由一个权威时间线 reducer 驱动，实时、恢复和向前分页共享语义。
6. WinUI 资源、键盘、可访问性、激活和窗口生命周期成为架构的一部分，而不是
   Web 行为的逐项复刻。

这与 Windows App SDK 的推荐模型一致：数据绑定用于将视图与数据源解耦；后台
状态通过 `DispatcherQueue.TryEnqueue` 切回 UI 线程；`ListView`/`GridView` 可使用
增量加载和集合通知；复杂可变高度集合也可用 `ItemsRepeater` 配合虚拟化布局。

参考：

- [Data binding overview](https://learn.microsoft.com/en-us/windows/apps/develop/data-binding)
- [Data binding in depth](https://learn.microsoft.com/en-us/windows/apps/develop/data-binding/data-binding-in-depth)
- [NavigationView](https://learn.microsoft.com/en-us/windows/apps/design/controls/navigationview)
- [Attached layouts and ItemsRepeater](https://learn.microsoft.com/en-us/windows/apps/design/layout/attached-layouts)
- [ListView and GridView data optimization](https://learn.microsoft.com/en-us/windows/apps/develop/performance/listview-and-gridview-data-optimization)

## 3. 证据清单：哪些部分仍是 Web 移植形态

### P0：typed timeline 已接入却没有驱动实时 UI

`deepx-domain` 已定义强类型 `TimelineEntry`、`TimelineEvent`、
`TimelineSnapshot`；runtime 也已经生产、持久化并通过独立 timeline SSE 提供。
然而 `apps/winui/src/bridge.rs` 的 `on_timeline_entry` 回调明确丢弃每个实时 entry，
只缓存 `serde_json::Value` 形式的 snapshot。实时 ChatView 仍从 conversation 频道
取得 JSON，再由 `chat_adapter::internal_event` 转成另一套 `ConversationEvent`；恢复
时又由 `timeline_turns` 手工解析 JSON。

这会形成两套 transcript 真相：

```text
实时：conversation JSON -> chat_adapter -> Transcript
恢复：timeline snapshot JSON -> chat_adapter -> RestoredTurn -> Transcript
```

风险是跨频道顺序、block seal、fragment gap、工具状态和恢复语义逐渐分叉。它也绕过
了 `docs/timeline-protocol-design.md` 已确定的原则：renderer 应只消费 timeline
entries/snapshots 决定 transcript 布局。

**原生目标**：`deepx-client` 直接暴露 domain 中的 typed timeline；每个 session
由一个 `TimelineReducer` 同时处理 snapshot、live entry 和 prepend page；ChatView
只观察 reducer 产出的 typed block collection。

### P0：UI 使用定时轮询模拟响应式 store

当前多个视图创建 `DispatcherTimer`，以 250/500ms 比较 revision；ChatView 另有
16ms drain 泵，`main.rs` 还有 50/250/500ms 的壳状态检查。`shell::poll_rev` 本身
就是“轮询快照 rev、变化才 set_state”的通用适配器。

这不是 WinUI 必需条件，而是将 Web store 缺失的订阅机制用 timer 补回。代价包括：

- 空闲窗口仍持续唤醒；
- 用户动作到显示之间存在固定延迟；
- 每个视图分别管理 timer 生命周期，容易在卸载、重连和窗口关闭时竞态；
- revision、snapshot 与 drain 三种同步方式同时存在，难以证明一致性。

**原生目标**：后台 reducer 更新 typed store 后，通过统一 UI marshaller/
`DispatcherQueue` 发出精确变更；reactor hook 订阅 store，组件卸载即取消订阅。
timer 只保留给动画、显式 debounce、超时和连接健康检查。

### P0：shell-facing 协议仍是 stringly JSON

`crates/deepx-client/src/types.rs` 仍以兼容旧 TS 形状为目标：

- `RingingEventEnvelope.event: serde_json::Value`；
- `TimelineEntry.kind: serde_json::Value`；
- `CommandRequest.command: serde_json::Value`；
- timeline snapshot callback 接收 `Value`；
- channel/timeline status 注释仍以 TS 类型为参照。

runtime 的只读 query 入口以 `"session.list"`、`"config.load"` 等字符串分发并返回
`Value`；`RingingChannelSnapshot.state` 和 `SnapshotProjector` 也仍是通用 JSON
对象。它们是合理的 wire/debug 边界，但不应继续成为 WinUI 的应用 API。

**原生目标**：wire 可以保留 JSON，反序列化必须在 client/repository 边界完成。
WinUI 只看到 `SessionSummary`、`SettingsSnapshot`、`DomainEvent`、
`TimelineSnapshot`、`RingingCommand` 等强类型；查询由 typed method 封装，字符串
router 留在 HTTP 服务器内部。

### P1：`BridgeCore` 是 browser preload/store 的集中复刻

`bridge.rs` 已增长到 4098 行，同时承担：连接、重连、command/query、所有页面
projection、全局缓存、revision、会话切换、timeline/chat 队列、文件对话框和 UI
facade。字段和注释中仍存在：

- `mirrors Electron ringing.status`；
- `same as Web projection`、`ignoring Web projection`；
- `direct mode` / fallback 标志；
- `apply_header`、`apply_composer`、`apply_interaction` 等投影入口；
- UI `Bridge` 对 `BridgeCore` 的大量重复转发方法。

即使 WebView 已删除，这个对象仍扮演 `window.deepx + renderer store`。一个状态变更
需要跨越 JSON、bridge cache、rev、timer、view state 多层，维护成本随协议字段数
近似成倍增长。

**原生目标**：拆成职责单一、可独立测试的服务：

```text
ConnectionService        Ringing 连接/恢复/健康
CommandService           typed commands
SessionRepository        会话集合与元数据
TimelineRepository       每 seed 的 timeline reducer/store
InteractionStore         ask/plan/permission
SettingsRepository       配置读取、编辑、提交
ShellState               Route、选中会话、窗口级状态
```

`Bridge` 名称和 Web compatibility 开关最终应消失；若为渐进迁移暂时保留，必须只做
composition root，不再拥有业务投影。

### P1：`active_seed` 单例导致后台会话数据被丢弃

`BridgeCore` 用全局 `active_seed` 过滤 chat 队列；注释明确说明非活动事件丢弃，切回
后依靠权威快照恢复。为解决旧投影回写错误，又引入 `resume_target` 和双 seed 仲裁。

这符合单页 Web chat 的活动 store，却不适合有 tab、多窗口潜力和后台通知的桌面
应用。它让“切换会话”同时成为数据生命周期切换，并迫使恢复与渲染耦合。

**原生目标**：`HashMap<Seed, SessionViewModel>` 或等价 repository 保留每个打开/
活跃会话的轻量状态；UI 的 selected seed 只决定显示哪个 ViewModel，不决定哪些
事件可以存在。内存压力通过 LRU、sealed block paging 和显式释放策略控制。

### P1：ChatView 是 Web golden reference 的二次实现

`chat_adapter.rs` 明确以 `serde_json::Value` 为输入，手工识别 `type`、tool payload
和 timeline turns。`chat_view.rs` 通过 16ms pump 把这些值喂给 `Transcript`；已有
迁移文档又把滚动和流式语义描述成对齐 Web golden reference。

行为对齐对迁移期很重要，但不应长期成为架构权威。native timeline 已经定义统一
排序、稳定 block id、fragment sequence 和 seal 边界，继续维护 conversation JSON
适配器会削弱这些保证。

**原生目标**：渲染输入是 `TimelineBlockViewModel`；未 sealed 文本使用轻量流式
呈现，sealed 后进行 Markdown materialization；tool 更新原地修改同一个稳定 block，
不得重建或移动。恢复和实时路径共用一个 reducer 和一组契约测试。

### P1：导航是字符串状态加轮询

当前 shell 使用 `current_view: String`，`main.rs` 定期检查后切换 view。字符串无法
穷举、参数无法建模，也难以保存每页导航状态。

**原生目标**：使用 `Route` enum（例如 `Home`、`Chat { seed }`、`Skills`、
`Settings`），由单一 `ShellState` 推送选择变化；`NavigationView` 的 items/selection
与 typed route 对齐。windows-reactor 不要求强行套用 C# 的 `Page + Frame`，但必须
保留类型、选择、返回和页面状态语义。

### P1：页面 DTO 在 UI 包内手工解析 JSON

`shell_store.rs` 明确写着“直接解析 `serde_json::Value`”，session、settings、
dashboard、skills 等页面各自把 query 返回值转为局部结构。这会使 daemon 字段变化
在编译期不可见，并把错误处理分散到视图层。

**原生目标**：DTO 位于 client API 或独立 application-model crate；所有 wire
兼容、默认值和版本迁移在该边界测试，视图只接收成功状态或结构化错误。

### P2：WinUI 平台能力尚未形成系统层

当前原生控件已经提供了基础输入和布局，但仍需把这些能力纳入后续验收：

- 键盘加速键、command routing、焦点恢复和 default/cancel action；
- UI Automation 名称、角色、live region 与高对比度；
- AppLifecycle 激活、协议/文件激活、单实例重定向；
- 窗口/导航/草稿状态恢复；
- 原生通知、任务栏/标题栏状态和多窗口策略；
- XAML resources/theme tokens、样式和模板复用，而不是组件内重复属性。

这些是“像 Windows 应用”的重要部分，但优先级低于 P0 数据链，因为先优化视觉
而保留双重状态源只会扩大返工面。

## 4. 哪些东西不应因为“原生化”而重写

以下内容本身没有问题：

- HTTP/SSE 和 JSON wire 格式：跨进程协议使用 JSON 是合理选择；问题只在 JSON
  穿透到 UI。
- Rust + windows-reactor DSL：它创建真实 WinUI 对象，不需要为了形式改成 C# 或
  `.xaml` 文件。
- `ListView`：它本身是原生虚拟化控件。只有当可变高度 transcript、精细元素复用
  或布局控制确实受限时才评估 `ItemsRepeater + VirtualizingStackLayout`。
- `DispatcherTimer`：动画、节流和超时可以用；禁止的是用它作为常态状态总线。
- `serde_json::Value`：允许存在于 HTTP/wire、日志、未知插件 payload 等真正不透明
  边界；不应存在于 WinUI 页面 ViewModel 和核心 command API。

## 5. 目标架构

```text
deepx-domain / deepx-ringing（权威类型）
                    |
                    v
deepx-client typed transport
  - wire deserialize / schema validation / reconnect
  - typed query + typed command + typed timeline
                    |
                    v
application repositories（后台线程）
  - per-seed TimelineReducer
  - Session / Interaction / Settings stores
  - immutable snapshot + precise collection changes
                    |
          DispatcherQueue / UiMarshaller
                    |
                    v
reactor typed subscription hooks
                    |
                    v
WinUI ViewModel / observable item source
  - Shell Route
  - virtualized TimelineBlock collection
  - native commands, focus, accessibility, lifecycle
```

建议的代码所有权：

```text
crates/deepx-client/
  typed/{command,query,event,timeline}.rs

apps/winui/src/application/
  connection_service.rs
  command_service.rs
  session_repository.rs
  timeline_repository.rs
  interaction_store.rs
  settings_repository.rs

apps/winui/src/view_model/
  shell.rs
  chat.rs
  sidebar.rs
  settings.rs

apps/winui/src/view/
  ... windows-reactor 控件组合，仅保留展示与 UI 行为
```

## 6. 分阶段升级计划

### Phase 0：冻结 Ringing 2026-08-01 行为基线

先补齐契约测试，不改变 UI：

1. snapshot + watermark + replay 后的 reducer 结果等于连续 live replay；
2. fragment gap/duplicate、epoch reset、reconnect 和 prepend 的确定性测试；
3. conversation/tool 现有可见能力对 timeline 的 cutover matrix；
4. 记录 WinUI 现有 session 切换、滚动、interaction 和 settings 行为。

**退出条件**：协议升级后能用测试回答“语义是否改变”，而不是靠手工比较界面。

### Phase 1：建立 typed client facade

1. `deepx-client` 依赖权威 domain/ringing 类型或一个无 runtime 依赖的 contract crate；
2. typed `TimelineEntry`/`TimelineSnapshot` 替换 client 内的 flattened `Value`；
3. 为 query/command 提供 typed methods，保留底层通用接口仅供诊断/兼容；
4. 在 client 边界完成 wire version 和结构验证。

**退出条件**：`apps/winui` 的新代码不需要了解 method string 或 envelope JSON。

### Phase 2：用 push store 替代 revision polling

1. 在 windows-reactor 增加或复用 external-store subscription hook；
2. repository 在后台 reducer 完成后，通过一个 UI marshaller 投递变更；
3. 逐页迁移 header、composer、interaction、sidebar、home、settings；
4. 删除对应 revision、snapshot getter 和 `DispatcherTimer`。

**退出条件**：稳态状态同步无 polling timer；timer 仅用于有说明的 UI/网络时序。

### Phase 3：ChatView 切换到权威 native timeline

1. 实现 per-seed `TimelineRepository`；
2. 启用当前被忽略的 `on_timeline_entry`；
3. snapshot、live、prepend 全部进入同一个 `TimelineReducer`；
4. ChatView 订阅 typed block collection；
5. 完成能力矩阵后删除 `chat_adapter::internal_event` 和 conversation 渲染路径。

**退出条件**：transcript 的布局与顺序只由 timeline 决定；恢复与实时结果字节级或
结构级等价；后台 session 事件不会因未选中而丢弃。

### Phase 4：原生导航、集合和资源

1. `Route` enum + ShellState 替换 `current_view: String`；
2. NavigationView selection 与 route 单向映射；
3. sidebar/session/chat 使用 observable/incremental item source；
4. 对 ChatView 做实测后在 `ListView` 与 `ItemsRepeater` 中择优；
5. 统一 theme resources、styles、data templates 和 visual states。

**退出条件**：页面切换不依赖轮询；大 transcript 的创建数、滚动帧耗时和内存满足
基准；主题和高对比度不靠逐组件补丁。

### Phase 5：Windows 应用行为收官

补齐 activation、窗口恢复、快捷键/command、焦点、UI Automation、通知和多窗口
策略，并在打包版本上做键盘、屏幕阅读器、DPI、主题、睡眠恢复和重连测试。

## 7. 量化验收门槛

完成原生化不以“看起来一样”为标准，而以以下可检索、可测试条件为准：

- `apps/winui` 中 `serde_json::Value` 只出现在明确登记的 opaque/wire 边界；
- 常态 UI 状态同步不再有 16/250/500ms drain/revision timer；
- `on_timeline_entry` 不再为空实现；
- transcript 不再调用 `chat_adapter::internal_event`；
- command/query 的业务调用点没有裸 method string 和任意 JSON params；
- 不再存在 `direct mode`、`Web projection fallback`、`resume_target` 兼容状态；
- 一个 session 的 snapshot + replay 与 uninterrupted live replay 结果相同；
- 切到后台再返回的 session 不依赖丢事件后全量重建才能正确；
- 所有可交互控件可仅用键盘完成，关键动态状态有 Automation 属性；
- 建立启动时间、首帧、流式更新 CPU、滚动帧时间、长会话内存的固定基准。

## 8. Ringing 上游发生大改时的同步流程

每次 Ringing schema、sequence 或恢复语义变更，都按以下顺序处理：

1. 对比 `deepx-domain` / `deepx-ringing` 的类型与不变量，先更新协议契约测试；
2. 更新 daemon producer、journal、snapshot 与 SSE，验证 replay 等价；
3. 更新 `deepx-client` 的 wire decoder，禁止 UI 直接兼容新旧 JSON；
4. 更新 application reducer/ViewModel，记录用户可见语义变化；
5. 最后更新 WinUI view；若只是字段新增，理想状态下 view 无需变化；
6. 运行原生化验收 grep、协议测试、WinUI 构建与长会话性能基准。

版本迁移应维护一个短期兼容矩阵：

| 项目 | 必须记录 |
| --- | --- |
| wire schema/version | daemon 与 client 接受的版本范围 |
| cursor/watermark | 重连从哪里继续，何时必须 reset |
| ordering | 哪个 sequence 决定 transcript 顺序 |
| snapshot | materialized state 的字段和默认值 |
| terminal/seal | turn、round、block 何时不可再更新 |
| capability delta | 新旧版本对用户可见功能的增删 |
| cutover/remove | 兼容代码删除的明确版本和测试门槛 |

## 9. 下一批可直接执行的工作

按风险和收益排序，建议下一次实现从以下四个独立提交开始：

1. `deepx-client`: 使用权威 typed timeline 并为 snapshot callback 定型；
2. `apps/winui`: 新建 per-seed `TimelineRepository`，先并行校验、不切 UI；
3. `windows-reactor`: 提供可自动取消的 typed external-store subscription hook；
4. `apps/winui`: 先迁移 header/interaction 两个小视图，验证 push 模型后再切 ChatView。

不要把这次升级做成一次性大爆炸重写。先建立 typed facade 和双跑一致性检查，再逐条
切换读路径；每切完一条就删除对应 Web 兼容层，避免形成第三套长期状态模型。
