# ChatView V4 改进计划（ToolView 聚合 + 状态设计）

> 状态：**已落地**（2026-08-12 完成代码，待人工视觉验证；原型见 `docs/nextdev/chat-live-prototype.html`）
> 日期：2026-08-12

## 1. 背景

DeepSeek 的实际交互模式是「思考链 → 工具1 → 工具2 → … → 答案」——每次工具调用后模型**不再输出 assistant 文本**，导致旧版按 round 三通道渲染时：
- 连续工具调用被判断为"不连贯"，呈现为单卡散落；
- 工具调用与思考链的视觉层级混乱。

方案：参考 OpenAI Codex 桌面版（Electron/Web）的工具调用展示设计，做「ToolView 聚合」改造。
逆向对象：`C:\Program Files\WindowsApps\OpenAI.Codex_26.803.10989.0_x64__2p2nqsd0c76g0`（app.asar）。

## 2. Codex 逆向研究结论

| 发现 | 说明 |
|---|---|
| `ToolCall`/`ToolResult` 组件族 | 工具调用渲染为对话流内**紧凑胶囊行**：图标 + 工具名 + 活动标签 + 状态，点击展开参数/结果 |
| `CodexAgentActivityGroupExpanded`/`State` 事件 | 存在**活动组聚合折叠**概念——多工具调用聚合、头部状态、整体折叠（ToolView 的雏形） |
| 活动标签人性化 | `HYs()` 根据 toolName/arguments/result 生成动词短语（"Used Chrome"、"运行命令"），**状态不用文字前缀表达** |
| `latestToolMarker` 投影 | turn 状态维护"最新工具标记"，用于进行中指示 |
| 无输入框 shimmer | 工作状态直接内嵌在对话流的胶囊行里（spinner 动画）；loading 用字符级灰阶光带（background-clip:text + steps(120) 离散跳动） |

逆向工具：`scripts/asar-list.js` / `asar-debug.js` / `asar-extract.js`（自研 asar header 解析，header 在文件开头偏移 16，JSON 长度在偏移 12）。

## 3. V4 设计决策

### 3.1 ToolView（对话流内的工具聚合区段）

- **连续工具块自动聚合为一个区段**（工具块之间无文本块时天然成组；有文本块则保序分段显示——顺序优先于聚合）。
- 区段头部 = 工作状态 + 折叠：
  - 进行中：`⏳ 正在运行 2 个工具…`（spinner）
  - 有失败：`⚠ 工具调用 (4) · 1 个异常`（黄色警示 chip）
  - 全成功：`✓ 工具调用 (4)`
- 行级渲染（胶囊行）：
  - 文案**固定为动作短语**（`写入 src/file_edit.rs`），不随状态变化——状态由左侧图标表达（spinner / ✓ / ✕ 红）
  - 完成且带 diff：`+120 −30`（彩色）+ 耗时 `1.8s`
  - 失败：✕ 红色 + 错误摘要一行截断，点击行展开完整错误（红色详情框）
  - 详情默认收起，用户手动展开
- 头部状态优先级：`running > error > done`（不掩盖失败）。
- turn 结束自动折叠为一行。

### 3.2 错误态

- 失败工具行：`✕` 红色 + `失败：找不到符号 restored_turns`（截断）+ 展开完整错误。
- 区段头部按最差状态显示（见 3.1），避免"轮次内其它工具正常 → 整体显示已完成"掩盖失败。
- **答案流应主动提及失败项**，让模型自纠（错误进入对话上下文而非被 UI 隐藏）。
- 数据来源：block_transcript 中每个工具块自带终态（成功/失败），渲染层读块状态即可，无需新状态机。

### 3.3 shimmer 状态条（保留，不退役）

- 目的：**存活指示**——动画在跑 = 后端活着；静止 = 用户可感知 API/本地卡住。
- 现有实现已存在且设计优良（`apps/winui/src/composer_bar.rs` `work_status_bar`，见 §4），**保留并扩展**，不新造。

### 3.4 耗时

- 后端能力已有：block_transcript 的 block 带开始/结束时间戳，前端算 `end − start` 即得耗时。
- 完成行右侧显示（如 `1.8s`），灰字。

## 4. 现有 shimmer 状态栏确认（composer_bar.rs L976-1047）

- 位置：composer 卡片内、TextBox 之上；空闲零高占位。
- 结构：`hstack(ProgressRing 14px + RichTextBlock(shimmer_runs))`。
- 文案（俏皮系）：`飞速思考中…` / `奋力回答中…` / `探索中…正在调用「{name}」` / `等待你确认…`。
- shimmer 机制：**字符级灰阶光带**（非背景渐变）——`deepx_fluent::shimmer_runs` 逐字符 Run + 高斯衰减灰阶（基色 72 → 峰值 155 微蓝），90ms/步离散推进（仿 Codex steps(120)），`label_len + 8` 周期回绕。同一光带被 chat_view 加载覆盖层复用（`loading_overlay`）。
- 性能边界：tick 线程封装在独立组件内（`use_async_state` + cleanup stop），只重渲染子树；历史教训——挂在 composer 渲染链上每 tick 全量 diff 会拖慢 ChatView 16ms 泵。
- 数据源：bridge.rs 从 round_delta kind 推导 WorkPhase，250ms 轮询。

## 5. XAML 能力对照（Codex=Web vs 我们=WinUI 3 + Rust）

| Codex 效果 | WinUI 3 对应 | 能力 |
|---|---|---|
| 胶囊行（圆角/hover/展开） | `Border`(CornerRadius) + PointerEntered/Exited + Tapped | ✅ 原生 |
| spinner | `ProgressRing` | ✅ 原生 |
| 折叠 | 显示/隐藏 + Composition 动画 | ✅ |
| 自动追尾 | `ScrollViewer.ChangeView()` | ✅ |
| 打字机光标 | DispatcherTimer + TextBlock 追加 | ✅ |
| 图标 | `FontIcon`（Segoe Fluent Icons） | ✅ |
| 彩色 ±N | SolidColorBrush TextBlock | ✅ |
| 毛玻璃 | AcrylicBrush / MicaBackdrop | ✅ |
| 虚拟化长列表 | ItemsRepeater | ✅ |
| shimmer 流动高光 | **无内置控件**：① ProgressRing+静态卡（零成本，推荐起步）② CompositionLinearGradientBrush+Offset 表达式动画（~60-80 行）③ Lottie/AnimatedVisualPlayer（见 §6） | ⚠️ 需自研 |

## 6. AnimatedVisualPlayer 调研（已完成）

### 结论：未引入，可引入，但对 shimmer 场景不划算

**现状**：`F:\deepx-winui-reactor`（官方 windows-rs 的 DeepX durable fork，基线 #4824）中：
- `bindings.rs`（936KB 手生成 WinUI 3 绑定）**无** AnimatedVisualPlayer / IAnimatedVisualSource
- `tools/reactor/src/base.txt`（类型过滤清单）无相关条目；`generated.rs` 无控件绑定

**生成链路**（引入入口）：
```
winmd（WinAppSDK 2.3.1 固定，含 Microsoft.UI.Xaml.winmd）
  + base.txt（类型清单，如 Microsoft::UI::Xaml::Controls::AnimatedVisualPlayer）
  + winui.toml（控件属性清单，如 [ProgressRing] IsActive/IsIndeterminate…）
  → crates/tools/reactor 生成器 → bindings.rs / generated.rs（+ selftest，gen.yml zero-diff 门禁）
```

**成本分层**：
1. 绑定层：base.txt 加 ~4 行 + winui.toml 加控件段 + 跑生成器 → 机械产出（全量 diff）
2. reactor 层：element.rs/widgets 暴露 builder（手动 ~几十行，参照 ProgressRing 样板）
3. **Lottie 源（真正瓶颈）**：AnimatedVisualPlayer 只是播放器，内容 = IAnimatedVisualSource 实现；WinUI 3 **无内置 Lottie JSON 解析**，Rust 生态无现成实现 → 自研 = JSON 解析 + Composition 场景图 + 时间轴驱动，大工程

**shimmer 推荐路径（不进 AnimatedVisualPlayer）**：
补 Composition 绑定（Compositor / CompositionLinearGradientBrush / CompositionVisual / ExpressionAnimation，base.txt ~8 行），经已绑定的 `ElementCompositionPreview.GetElementVisual` 挂到元素上做渐变偏移动画——成本远低于 Lottie，直击背景流动需求。现有 `CompositionObject`/`CompositionTarget` 已绑定，缺口仅几个 Composition 类。

**官方对照**：官方 windows-rs `Microsoft_UI_Xaml_Controls` feature 有完整 AnimatedVisualPlayer 绑定，可作 API 形状参考。

**决策**：shimmer 背景流动 → 走 Composition 绑定补充（单独提交，改 fork）；AnimatedVisualPlayer 仅当出现 Lottie 图标/Logo 需求时再引入（届时 Lottie 源仍需外部方案）。

### 6.1 dotlottie-rs / dotlottie-io 调研（2026-08-12，gh CLI 确认）

| | dotlottie-rs | dotlottie-io |
|---|---|---|
| 定位 | **播放器引擎**（Lottie/dotLottie 解析 + ThorVG 渲染 + 状态机/主题/音频） | **文件格式 I/O 库**（创建/读取/修改 .lottie ZIP 容器，Node N-API + WASM 绑定） |
| 创建/成熟度 | 2023-09，v0.1.58，273★，MIT | 2026-04，v0.1.0，3★，MIT（作者 Sam Osborne） |
| 核心依赖 | ThorVG（C++ 渲染，`links="thorvg"`） | 纯 Rust（zip/serde/serde_json） |
| 关系 | 两者同属 LottieFiles 官方，**非新旧版**，是"渲染播放" vs "文件读写"的分工；io 是 2026 年拆出的格式层 | 同左 |

**结论**：两个都用不上——dotlottie-rs 自带 ThorVG 渲染管线，与 AnimatedVisualPlayer 的 `IAnimatedVisualSource`（要求 Composition 场景图）**不兼容**，桥接需 SurfaceImageSource + 编译 C++ 依赖，成本远超收益；dotlottie-io 纯格式读写，对 UI 播放无帮助。Rust 生态无现成 Lottie→Composition 实现（CommunityToolkit.Lottie 是 C#）。

**自研路线（待排期）**：最小实现 = ① Lottie JSON 核心子集解析（图层/形状/变换/位置/缩放/旋转/透明度 + 关键帧线性&贝塞尔插值，裁剪 mask/matte/表达式/文本/渐变）→ ② 软件渲染验证正确性 → ③ fork 补 Composition 类型绑定（Compositor/ShapeVisual/CompositionSpriteShape/CompositionPathGeometry/CompositionColorBrush/KeyFrameAnimation 等 ~30-50 类型）→ ④ `windows_core::implement` 实现 IAnimatedVisualSource → ⑤ 接入 AnimatedVisualPlayer。风险点：Lottie JSON schema 细节（AE 导出格式）与 IAnimatedVisualSource 契约需查资料；图形学基础（插值/仿射/贝塞尔）现成。

## 7. 落 Rust 实施清单（~半天，待确认后开工）

1. `chat_view.rs`：ToolView 渲染——连续工具块聚合为区段；头部状态机（running/error/done + 折叠）；行级胶囊（动作短语 + ✓/✕ + ±N + 耗时 + 点击展开详情）。
2. `composer_bar.rs`/bridge：WorkPhase 扩展并行计数（`Tool { name, running: usize }`），状态条文案 + 计数；保持独立组件边界。
3. 失败态数据流：block 终态 → 行级 ✕ + 摘要；答案提及失败项（模型侧已有上下文，确认 UI 不吞错误）。
4. 耗时：block 时间戳 → 完成行显示。
5. 原型与实现对齐：statusbar 改字符级光带 + 俏皮文案（对齐现有实现而非 v4 原型背景渐变）。
6. 验证：真实 daemon 流式会话（交错块序/并行工具/失败工具/撤回/分页）+ 历史会话恢复。

### 实施结果（2026-08-12 完成代码，待人工视觉验证）

- ✅ **1. ToolView 聚合**（chat_view.rs）：Tool 块**不占 block 原位，集中渲染在 turn 底部**（答案之后，对话流 = 思考 → 答案 → [工具工作区]，V4-C）；`ToolSectionProps` 聚合 memo（内部任一块 mutation_rev 变化才重建）；`tool_section` Border 分组容器 + 状态头（非折叠，V4-B 修订去掉外层区段折叠），头部状态机 running>error>done（spinner / ⚠+异常数 / ✓）；工具行保留单层 Expander 展开详情。
- ✅ **2. 并行计数**：按 §8 决策**不改 WorkPhase**——toolview 头部自算 running/failed 计数（`正在运行 N 个工具…`），状态条保持单阶段文案与独立组件边界。
- ✅ **3. 失败态**：`ToolCardView` 新增 `failed: bool` + `failure: Option<String>`（协议 `TimelineToolState::Failed` + `TimelineFailure` 透传，`parse_tool` 实现）；行级 `✕` 红 + 错误摘要置顶，展开显示完整错误。
- ✅ **4. 耗时**：`BlockView` 新增 `opened_at: Option<Instant>`（live 块创建时记录；restore 块为 None）+ `duration_ms`（seal 时计算）；行尾 `1.8s` 灰字。
- ✅ **5. 对齐**：行文案固定动作短语（沿用 `tool_action`/`tool_argument_hint`/`ChangeStats::label`），状态由图标表达；composer 状态条保留现状（字符级光带 + 俏皮文案）。
- ✅ **6. reasoning 折叠块**：`reasoning_view`——头部（状态点 spinner/✓ + 标题 + meta）+ 固定高 176px **尾部 8 行窗口**（语义等价自动追尾；fork 未暴露 ScrollViewer 编程滚动，尾部窗口行为确定）；sealed 且未手动操作自动折叠。
- ✅ **7. 双栏 diff**（V4-C，tool_content.rs）：edit/apply_patch 类工具展开区显示 **side-by-side diff**——`pair_side_by_side` 配对算法（删除/添加按序对齐、单边补空、Context 双栏同行、Hunk/Meta 跨栏合并），行视图 `[旧行号|旧内容(红底)|新行号|新内容(绿底)]`；删除单栏 unified 视图（-150 行，零警告）。
- ✅ **8. 单列 unified diff（定稿）**：双栏太占空间 + 行号错位 → 改回单列：`[旧行号 44px|新行号 44px|marker 18px|文本 STAR]`，行号列灰底右对齐、删除红底红字/添加绿底绿字（主题感知）、Hunk 跨行灰条、**vstack spacing 0**（弃用 ListView——ItemContainer 默认样式导致行间空隙）、NoWrap + 横纵滚动、Cascadia Mono。demo：`diff_demo` bin。
- ✅ **9. 按文件合并 + diff 抽屉（V4-D）**：
  - `parse_unified_diff` 内 `merge_diff_by_file`：多 op 同文件 diff 合并为单块（rows + 统计）；+3 测试
  - `diff_file_view` pub 化（re-export）
  - turn 末尾**总结行**「已修改 N 个文件（+X −Y）· 查看详情 ›」（chat_view `collect_diff_drawer` 按文件合并 + failed OR；`diff_summary_row` 点击写槽）
  - **diff 抽屉**（新模块 `diff_drawer.rs`）：静态槽 + 80ms 轮询覆盖层（同 diagram_zoom 模式）、右滑入（`AnimationConfig.translation` 520px → 0，220ms ease-out，无需改 fork）、左文件列表（✓/✕ + 路径 + ±N + 选中高亮 AccentSecondary）/ 右单列 unified diff、关闭 ✕/遮罩/Esc；main.rs 覆盖层挂载
  - demo：`diff_drawer_demo` bin（总结行 → 滑入 → 切换 → 关闭）
- ✅ **10. 总结行数据源修复（V4-D 链路）**：实测「查看详情」按钮不出现——`collect_diff_drawer` 只认 `ToolBody::Diff`，而 edit_file/write 结果**从不携带 diff 正文**（紧凑摘要行是刻意设计，省模型上下文；`data` 又会被 `project_for_model` 投射给模型）。修复：新增**展示平面** diff 通道，全程不进模型投影——
  - `ToolResult.diff`（deepx-types，`project_for_model` 排除）+ `with_diff()`
  - workspace：edit_file 的 `FileReport.display_diff`（`#[serde(skip)]`，常规执行也计算）；write 改结构化返回并算 `unified_diff`
  - 模型路径：engine_turn 内联执行拿到 `_canonical_result.diff` → `push_tool_result_direct_with_diff` → 消息存储 ToolResult.diff（`last_step_tool_results` 带出）；UI 路径：`result.diff` → timeline
  - `TimelineTool.diff`（deepx-domain + markdown-winui timeline_protocol 双 mirror，serde default/skip）→ `tool_body_from_timeline(…, diff)` 显式优先解析
  - **抽屉灰屏修复**：`transition` 走 `ElementCompositionPreview::SetImplicitShowAnimation`，只在 Visibility 切换时播放；抽屉是新建控件挂载进视觉树 → 动画不触发但元素停在起始帧（偏移 520px 屏幕外 / opacity 0）→ 遮罩灰屏无抽屉（diff_drawer_demo 注释已实证）→ **移除 transition，与 demo 对齐**
- ✅ **11. V5 子窗口式弹层 + 帧驱动动画**：灰屏修复后改为正式弹层设计——
  - **子窗口**：主窗口 70% 尺寸（`DRAWER_RATIO=0.70`，宽高同比例保持纵横比，clamp 640..1280 × 480..920）、居中、圆角 12 + 边框 + 底部合计条
  - **动画**：reactor 隐式动画全路径证伪（transition 不触发、animate 无起点帧）→ **DispatcherTimer 16ms 帧驱动 state**（rAF 式）：progress 放 use_ref + tick state 触发重渲染，ease-out cubic 映射，每帧更新 `canvas_left`（右缘外→居中）/ `opacity` / 遮罩 alpha
  - 生命周期：打开动画完成清静态槽（防轮询重播）；关闭 ✕/遮罩/Esc 反向动画，完成清 state+槽；动画期间轮询暂停（防打断）
  - **0xc0000409 崩溃修复**：timer 回调 `anim.borrow_mut()` 借用存活到回调末尾，动画静止时轮询分支再次 `borrow_mut` → RefCell 双重借用 panic → `abort_on_panic("timer")` → fail-fast abort；修复：借用限定块作用域（demo 同款坑一并修复）
  - **关闭失效修复**：close 条件 `a.active && !a.closing` 在打开动画完成（active=false）后永假 → ✕/Esc/遮罩无响应；改为 `!closing && progress>0`（可见即响应）+ 静止态激活反向推进
  - **弹出卡顿修复**：帧驱动每帧重建 file_rows + diff 行子树 → 内容抽为 `drawer_content` memo 组件（props 忽略回调字段，动画期间不变 → reconciler 复用输出），每帧只提交外层 border 的 canvas_left/opacity + 遮罩 alpha 两个属性
- ✅ **12. V6 原生 ContentDialog 弹层（方案 C 落地）**：软件动画 15fps 卡顿感 → 换 WinUI 原生模态：
  - **reactor 扩展**（deepx-winui-reactor fork）：`Widget::content_element()` slot（仿 header/pane/flyout 全套：mounted_tree contents 存储、widget_dispatch mount/update 四分支、backend `set_content_element` → `IContentControl.SetContent`）；`ContentDialog.content_element(el)` 与 text 模式互斥
  - **弹层**：系统级弹出/关闭动画（Composition 合成层，零卡顿、零帧驱动）；Esc/返回手势/✕ 原生关闭（on_closed 收口）；模态遮罩原生；尺寸 70% 主窗口（覆盖模板默认 MaxWidth 548）
  - 架构简化：静态槽 + 80ms 轮询保留（写端 chat_view 零改动），删掉 AnimState/帧 timer/Canvas/遮罩自绘
  - demo 同步 V6（ContentDialog 常驻 + is_open 驱动，✕ 走 Hide 动画）
  - 测试：reactor 38 全绿；deepx-winui 编译 + demo 构建通过
- ✅ **13. V6.1 修复两个实测回归**：
  - **面板变小**：ContentDialog 模板内容区是 Auto 高度，控件级 `Height` 不参与模板布局 → 内容 STAR 行无约束退化 → 面板被压小。修复：**尺寸设在内容根 grid**（`width/height` 固定 70% 窗口，rows=[Auto, STAR, Auto]）
  - **关闭后阴影残留拦截输入**：关闭动画期间卸载销毁 ContentDialog（on_closed → set_req(None) → unmount destroy）→ WinUI 模态遮罩残留。修复：**ContentDialog 常驻**（永不卸载），is_open 驱动显隐；on_closed 只清 open/槽，内容保留复用
- ✅ **14. V6.2 根治关闭残影（面板形状阴影拦截点击）**：实测常驻后输入拦截消失但**视觉残影仍在**（对话框内容+投影形状残留，覆盖屏幕中央，可 hit-test）。机制：WinUI 3 ContentDialog 关闭动画（Composition）在特定条件下中断，popup 视觉层滞留。修复（backend 层透明，app/demo 零改动）：
  - bindings：补 `ContentDialogClosingEventArgs` + `IContentDialog::Closing`（vtable 补齐）
  - **Closing 事件**（动画播放前同步触发，✕/Esc/系统关闭统一路径）：清空 Content + 缩小到 1x1 → 关闭动画只播空壳 → 无视觉残影
  - **重新打开前恢复**：`dialog_state` 表（set_prop 记录 Width/Height、set_content_element 记录 content_id）→ IsOpen(true) 时回写尺寸+内容再 ShowAsync
  - reactor 38 测试全绿；demo 构建通过
- ✅ **15. V6.3 ✕ 关闭残影的最终根因与修复**：实测 ✕ 有残影、**Esc 无残影** → 差异不在关闭动画（Closing 清场对两者都生效），而在**关闭调用时机**：✕ 在 **Click 事件栈内**调用 `Hide()`，WinUI 在 pointer/焦点事件处理中关闭承载 Popup 会清理失败（视觉残留）；Esc 是系统路径（对话框自己处理键事件后关闭）→ 干净。修复（backend 层）：
  - `IsOpen(false)` 不再直接 `Hide()`，改为 `DispatcherQueue.TryEnqueue` **下一周期**执行（事件链完成后，与系统路径等效；零用户感知延迟）
  - `Rc<AtomicBool>` 取消标志：排队期间 `IsOpen(true)` 置 false，防止快速重开被排队的 Hide 误关
  - 兜底：队列不可用时同步 Hide
  - demo 同步 V5（纯 bin crate 无法共享模块，内联同构实现；数据解析走 markdown-winui）
  - 测试：`non_dry_run_edit_attaches_display_diff`、`display_diff_flows_to_last_step_results_but_not_model_projection`、`timeline_explicit_diff_wins_over_compact_output`、`tool_diff_from_timeline_becomes_diff_body`
- ✅ **验证**：`cargo check --workspace` 零错误；deepx-workspace 191 / deepx-message 23 / markdown-winui 60 / deepx-domain 56 / deepx-runtime 103 / deepx-daemon 4 全绿。
- ⏳ **待人工**：真实 daemon 流式会话视觉验证 + 抽屉在真实数据下的表现。

## 8. 开放问题

- [ ] AnimatedVisualPlayer/Lottie 是否引入 Rust 侧（见 §6 调研结果）
- [ ] 并行计数落点：WorkPhase 扩展 vs ToolSection 自算（倾向后者——toolview 头部从 timeline 快照推导 running 数，状态栏保持单阶段文案）
- [ ] 状态栏与 ToolSection 职责分工最终确认：状态栏=全局存活指示；ToolSection=对话流内明细
- [ ] shimmer 背景流动 vs 字符级光带：已定字符级（对齐现有实现）
