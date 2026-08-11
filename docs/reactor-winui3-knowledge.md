# Reactor/WinUI3 开发知识库（实战踩坑与当前基线）

> 2026-08-11 更新：补丁已重放到 windows-rs #4824。**任何 LLM 会话在改 `apps/winui` 或 fork 前必读本节"致命陷阱清单"**。
>
> 配套文档：[windows-reactor-skill.md](./windows-reactor-skill.md)（心智模型/骨架/错误模型）、[PERF-FIX-NOTES.md](../apps/winui/PERF-FIX-NOTES.md)（chat 性能）、[winui-native-migration.md](./winui-native-migration.md)（迁移史）。

---

## 1. 当前基线（2026-08-11，重要！）

| 项 | 值 |
|---|---|
| reactor 来源 | 本地 fork `F:\deepx-winui-reactor`（独立 git 仓库），**开发期 path 依赖** |
| 上游基线 | `c318f55a2`（PR #4824，2026-08-11） |
| DeepX 补丁 | `42e951803` 文件级重放 + `d57da0ccc` 语义融合；包含 on_frame、RichText 增量 diff、虚拟滚动、Element 修饰等能力 |
| 补丁清单 | `F:\deepx-winui-reactor\DEEPX-DOWNSTREAM.md` |
| release 打包 | `apps/winui/Cargo.toml` 的 path 依赖通过 `scripts/dev-downstream.ps1 off` 切到 `QAQTam/deepx-winui-reactor` immutable rev |
| 上游状态 | #4815–#4824 已融合；后续更新从最新 `master` 重放 `deepx-winui` |

旧 shallow fork `F:\deepx-winui-reactor-1` 不再参与构建、发布或补丁审计。

---

## 2. 架构速览（写代码前 30 秒）

```
声明式层（apps/winui/src/*）  fn app(cx) -> Element   ← hooks + Element 树（纯 Rust 数据）
        │
reconciler（fork reconciler.rs）  diff 新旧树：新建/复用/卸载/属性更新（keyed: compute_lis）
        │
backend（fork backend/winui/mod.rs）  真实 WinUI 控件：创建、Set*、add_* 事件 → 闭包
        │
windows-rs bindings（fork bindings.rs）  裸 COM/WinRT 调用（Microsoft.UI.Xaml）
```

- **无 XAML**：Element 树是运行时构建的 Rust 数据，diff 后才碰真实控件
- **无人工持有控件**：ElementRef（`reference.rs`）是唯一的命令式句柄
- 渲染循环：`use_state` → `set_state` → 组件子树重渲染 → diff → patch

---

## 3. 致命陷阱清单（LLM 必读，全部实战验证）

### 3.1 `Element::Component` 无 modifiers —— 链式修饰静默忽略 ⚠️最高危

- **现象**：`component(x, ()).grid_row(1).grid_column(1)` 的 grid/margin/padding/alignment **全部不生效**，且不报错。
- **机制**：fork `element.rs` `modifiers_mut()` 对 `Element::Component`/`Empty` 返回 `None`（L366-378 注释：上游 capability 模型设计如此，DeepX downstream 扩展选择了"静默忽略"）。`component()` 返回的 `ComponentElement` 结构体**没有 modifiers 字段**（`engine.rs` L136）。
- **`with_key` 例外**：key 有效（`ComponentElement.key` 字段，`element.rs` L356）。
- **修复**：用**普通元素**包裹承载修饰符（普通 widget 的 modifiers 正常应用）：
  ```rust
  let info_el: Element = if chat_visible {
      grid((component(info_panel_component, ()).with_key("shell-info"),))
          .with_key("shell-info-wrap")          // wrapper 加 key：视图切换按 key 匹配
          .rows([GridLength::STAR])
          .columns([GridLength::STAR])
          .grid_row(0)
          .grid_column(1)
          .into()
  } else { Element::Empty };
  ```
- **历史案例**：解绑（9503dc0）把视图从函数调用改为 component 后，`info_el.grid_column(1)` 静默丢失 → info 面板落到 chat 区（1300px 宽）、composer 被覆盖、主区全空。像素分析 + `element.rs` 源码定位。
- **排查**：布局错位先搜 `component(` 后面是否链了 `grid_*`/`margin` 等修饰符。

### 3.2 `Element::Empty` 会被 children 过滤 → 视图切换 = 卸载/重挂载

- `into_children()` 会 `retain` 掉 Empty（fork `element.rs` L148）；reconciler 对空出的 key 执行**卸载**（`reconciler.rs` L1298/L1364）。
- 非当前视图返回 `Element::Empty` = 子树完全卸载：`use_effect` cleanup、`DispatcherTimer` drop、`EventRevoker` drop 自动执行（停止轮询/帧回调）。
- **不要**用"零高度常驻组件 + active prop"替代卸载（PERF-FIX-NOTES 已定论）。
- 视图切换时 children 数量变化，**无 key 元素按 index 匹配、有 key 按 key 匹配**——wrapper 一律加 key 避免歧义。

### 3.3 Grid 缺维度定义 = 隐式 Auto → 填满必须显式 STAR

- **WinUI Grid 未定义 rows/columns 时，隐式行/列为 Auto**（按内容测量）；Stretch 子元素只覆盖到内容尺寸，**不填满**。这是本项目实证过的（main.rs 覆盖层注释 L376-378："Grid 默认 Auto 会让 Stretch 子元素只覆盖内容大小"）。
- **需要"填满"的维度必须显式 STAR**；覆盖层/全窗遮罩必须 rows+columns **都** STAR。
- 症状链：父级缺维度 → 测量退化（ListView 无界高度全量 realize、scroll_viewer 视口=内容高无法滚动、STAR 链塌缩、composer 被推出可视区）。
- 历史案例：壳层 6 个 Grid 各缺一个维度（chat_area/right_content/right/base 缺 columns；right_body/content 缺 rows）→ 长会话/大窗口时布局崩坏。
- **检查法**：每个 grid 自问——子元素是否需要"撑满"某维度？需要就显式声明。

### 3.4 `text_block` 默认 Wrap —— 单行省略要 `max_lines(1)`

- fork `widgets/text_block.rs` L87：`text_wrapping` 默认 `Wrap`。`TextTrimming` 在 Wrap 下**不生效**（trim 只对不换行的单行文本）。
- 长文本在窄列（侧栏标题等）会换行成多行、行距紧贴视觉像"重叠"。
- **单行省略的正确写法**：`.max_lines(1).text_trimming(TextTrimming::CharacterEllipsis)`。
- reactor 没有 `no_wrap()` builder（`wrap()` 是 0 参数设置 Wrap）。

### 3.5 hooks 必须在组件函数顶层无条件调用

- `use_state`/`use_ref`/`use_effect` 不能在闭包/条件分支里调用（违反 hooks 顺序规则）。
- 案例：footer 的版本标识本想放在 `let footer: Element = { ... }` 闭包内用 `use_ref` 缓存——错误；必须把 ref 提升到组件函数顶层。

### 3.6 其它确认过的语义

- `component` 的 props 用 `()` + `Bridge::shared()` 全局单例（解绑模式）；props 相等 → 子树不重渲染。
- 事件（`on_click`/`on_text_changed`/`on_pointer_*`）回调里 `set_state` 正常触发重渲染。
- `ThemeRef` 语义色令牌自动适配深浅主题；RichTextBlock 不继承全局字体，需显式 `font_family`（chat_view.rs L947-949 案例）。

---

## 4. 高频更新路径（chat 性能，改 chat_view 前看）

- 事件泵：`CompositionTarget::Rendering` vsync 帧回调（`on_frame`，fork 扩展 API）→ `chat_drain_limit` 限额取事件 → 跨帧 `deferred_events` 合并（同 turn/round/kind 的 RoundDelta 合并、BlockCheckpoint 替换）→ `CHAT_REDUCER_BUDGET`（4ms 墙钟）reducer。
- live 正文提交节流 ~33ms；结构变化（新 turn/工具卡/final）立即提交。
- **组件隔离是性能根基**：chat/composer/info 各自独立 component，高频 set_state 只重渲染自身子树，不波及壳层（header/sidebar/tabs）。
- 数据规模：窗口化裁剪（restore）+ 会话缓存驱逐（SESSION_CACHE_CAPACITY）+ 主题纳入窗口缓存键。

---

## 5. 排查方法论（本项目实战验证过）

1. **先 `cargo check -p deepx-winui`**——编译器是最大的安全网；release 全量构建确认零 warning。
2. **运行日志**：`.deepx-winui.log`（`DEEPX_WINUI_LOG` 可改路径）；启动/异常/`log_diag` 都写这里。
3. **布局问题**：先查 Grid 维度定义（3.3）和 component 修饰符（3.1），再怀疑视觉元素本身。
4. **像素级分析**（无多模态时）：Python `PIL + numpy` 做行/列投影找内容带、文本行间隙检测（两行文字粘连 = 行距异常/换行）、色相分类（进度条蓝/绿、错误红）。示例脚本在 `.deepx-tmp/`（已 gitignore）。
5. **视觉模型反馈**：截图 + 描述，与代码结构对照；"重叠/错位"优先怀疑布局维度与修饰符传递，而非元素样式。

---

## 6. 打包与版本标识

- 打包链：`just winui-package` = build-daemon + build-winui + prepare-daemon + assemble-winui + build-installer/updater + collect-payload + finalize。
- **版本标识**：`collect-payload-winui.ps1` 生成 `build-info.json`（`version`/`commit`/`timestamp`/`build_id`，与 staging 目录名同源：`<版本>-<git12>-<UTC yyyyMMddHHmmss>`，秒粒度）→ 安装根目录 → 前端侧栏 footer 读取显示（`sidebar.rs::build_id_label`，fallback `CARGO_PKG_VERSION`）。
- dev 模式（cargo run）无 build-info.json → 只显示 `v1.0.0-rc.x`。
