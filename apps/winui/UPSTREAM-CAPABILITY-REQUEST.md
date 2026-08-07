# windows-rs fork 上游能力需求单（UPSTREAM-CAPABILITY-REQUEST）

> **读者**：负责维护 `F:/windows-rs`（windows-rs + windows-reactor fork）的开发者/模型。
> **请求方**：DeepX WinUI 壳（`apps/winui`，windows-reactor 声明式 UI）。
> **终局目标**：移除 WebView2，ChatView（聊天视图）全 XAML 原生渲染。
> **参考基准**：`apps/winui/CHATVIEW-RENDERING-REFERENCE.md`（Web 端渲染规格，已固化）。
> 本需求单的每项能力都对应 Web 端**已存在且可对照验收**的实现——上游交付后由请求方
> 逐项对照验收（特性矩阵/样式 token/性能契约），不需要上游猜测行为。

---

## 0. 现状盘点（请求方确认，2026-08-07）

**reactor 已有**（无需重复造）：text_block / text_box / rich_edit_box（编辑控件）/
scroll_viewer / grid / stack_panel / expander / tree_view / navigation_view /
content_dialog / info_bar / teaching_tip / flyout / menu_bar / title_bar /
web_view2 / canvas / shape / image / 等 60+ 控件。

**reactor 缺失（本需求单范围）**：
- RichTextBlock（富文本**显示**）及其 markdown/代码高亮能力
- ItemsRepeater/虚拟化列表绑定
- 托盘（Shell_NotifyIcon）、HWND 暴露、快捷键、背景材质、任意内容 ContentDialog

---

## 1. P0 — 富文本渲染管线（ChatView 全迁的基石）

### 1.1 Markdown 块级渲染

**场景**：ChatView 助手回答 = markdown 字符串 → 原生富文本布局。当前由 Web 端
`marked(GFM)` 完成，迁移后由 reactor 完成。

**功能规格**（对照 CHATVIEW-RENDERING-REFERENCE.md §3 特性矩阵）：
- 块级：段落、标题 h1-h3（h4+ 降级粗体）、有序/无序/嵌套列表、**任务列表**
  `- [ ]`、表格、引用块、代码块、分隔线
- 行内：**加粗**、*斜体*、~~删除线~~、行内代码、[链接](url)
- **流式语义（关键）**：文本增量到达时**追加**渲染（不重建整个文档）；未闭合语法
  （`**` / `` ` `` / `[`）按字面文本输出，不产生破损布局——这是"流式期间也能看到
  加粗/链接"的基础（对应 Web `inlineLiveHTML`）
- API 形状建议（可调整）：
  ```rust
  pub fn markdown_block(content: impl Into<String>) -> MarkdownBlock;  // widget
  // 或 ElementExt 扩展：
  element.markdown(content, MarkdownOpts { streaming: bool, theme: Theme });
  ```

**验收**：REFERENCE §3 矩阵全绿（含流式追加与未闭合语义）；`cargo test` 新增
markdown 解析单测（fixture 从 Web 侧 `markdown-render-core.test.ts` 移植）。

### 1.2 代码块语法高亮

**场景**：markdown 围栏代码块 + 独立代码展示（ProcessDetail）。当前由 shiki
（oniguruma token 化，27 语言，github-light/dark 双主题）完成。

**功能规格**：
- 语言集（27 种）：ts/tsx/js/jsx/json/yaml/toml/rs/rust/py/python/go/java/kt/
  css/scss/html/bash/sh/shell/sql/graphql/md/markdown/diff/c/cpp/zig/nim
- 双主题：light / dark，跟随应用主题切换（reactor 已有点亮/暗色切换机制）
- 结构：`wrapper（右上角语言标签 + 复制按钮）+ 高亮代码区`（横向滚动）
- 语言别名归一：`h`→`c`、`hpp`→`cpp`；未知语言回退字面文本（内容永不失真）
- API 形状建议：`code_block(text, lang, theme) -> Element`（高亮 token 可复用
  RichTextBlock 的 Run/Span 机制）
- 样式：REFERENCE §7（pre 背景 `--bg-secondary`、圆角 8px、代码 13px/1.6 行高、
  语言标签 10px/700/大写右上角）

**验收**：REFERENCE §4 结构一致；27 语言 fixture 渲染无 panic；主题切换即时生效。

### 1.3 数学公式（katex 等价）

**场景**：`$..$` / `$$..$$` / `\(..\)` / `\[..\]` 数学表达式（REFERENCE §6）。

**规格**：块级（display）与行内（inline）两模式；代码块内的 `$` 不误渲染；
渲染失败回退字面文本（`throwOnError: false` 语义）。
**验收**：REFERENCE §6 分隔符矩阵全绿。

### 1.4 富文本交互

- 文本选择/复制（RichTextBlock 选择能力绑定）
- 链接点击事件（Hyperlink 点击 → 应用层回调，打开外部 URL / 应用内路由）
- API：`on_link_click(callback)` / 事件通道

---

## 2. P1 — 列表虚拟化（万级 turn 滚动性能）

**场景**：ChatView transcript 可能万级 turn，当前 Web 端用 DOM 虚拟化
（估算高度 + 实测替换 + 滚动锚定）。WinUI 原生 ItemsRepeater 需要 reactor 绑定。

**功能规格**（对照 `ConversationTranscript.tsx` 滚动契约）：
- **虚拟化绑定**：ItemsRepeater 或等效虚拟化列表，`For` 风格子项渲染
- **估计高度 + 实测替换**：初始 `ESTIMATED_TURN_HEIGHT=120`，渲染后实测高度替换
  （占位避免滚动跳动）
- **跟随尾部**：距底部 ≤120px 时自动滚底，否则保持阅读位置
- **滚动锚定**：上方 turn 高度变化（流式展开）时锚点补偿，不打断阅读位置
- API 形状建议：
  ```rust
  pub fn items_repeater(items: impl IntoIterator<Item = Element>, opts: VirtualizeOpts) -> Element;
  // VirtualizeOpts { estimate_height: f64, on_measured: ..., follow_tail: bool, tail_threshold: f64 }
  ```

**验收**：万级 items 滚动流畅（≥60fps）；流式展开不跳位；锚点补偿正确。

---

## 3. P1 — 壳能力（补缺，非 ChatView 依赖）

### 3.1 托盘 TrayIcon（Shell_NotifyIcon 封装）
- 图标设置（ico 资源）、左键单击（显示/聚焦主窗）、右键菜单（显示/退出）
- 与窗口关闭行为联动（最小化到托盘 vs 退出）——应用层组合
- API：`TrayIcon::new(icon)` + `on_click` / `menu(...)`

### 3.2 窗口 HWND 暴露
- reactor 创建窗口后暴露 `hwnd()`（或 `WindowHandle`）——FileOpenPicker 升级
  （WinUI FileOpenPicker 需 owner HWND）、Win32 对话框、窗口置顶等
- API：`window.hwnd() -> HWND`（在 UI 线程安全调用）

### 3.3 快捷键（KeyboardAccelerator）
- 应用级快捷键：F12（devtools 开关）、Ctrl+W（关窗）等
- API：`window.add_accelerator(Key, Modifiers, handler)` 或声明式
  `keyboard_accelerator(key, modifiers).on_invoked(...)`

### 3.4 背景材质（Mica / Acrylic）
- 窗口背景材质设置（SystemBackdrop 或等效 Win32 组合），跟随主题
- API：`window.set_backdrop(Mica | Acrylic | None)`

### 3.5 ContentDialog 内容槽支持任意 Element
- **历史痛点**：当前 content 槽仅接受 String——请求方被迫自绘覆盖层实现交互弹窗
  （permission/ask/plan 三模板）。期望：`ContentDialog { title, content: Element, buttons: Vec<Button> }`
- 验收：用统一交互弹窗（`interaction_overlay.rs`）验证可替换为原生 ContentDialog 的
  任意内容表单（RadioButton/TextBox/CheckBox 组合）

---

## 4. P2 — 图表（远景，可与 ChatView 渲染同期）

### 4.1 流程图/时序图（mermaid 等价）
- 输入 DSL 文本 → 图形渲染（节点/边/子图），支持缩放与交互（悬停高亮）
- 或：reactor 提供 **SVG 渲染 + 交互基础**，DSL 解析由应用层实现（请求方已有
  `graph-dsl.ts` 解析器可移植）
### 4.2 图可视化（G6 等价）
- 有向图/力导向布局（进程依赖、调用图）——同样可降级为"SVG 基础 + 应用层布局"

---

## 5. 约束与验收总则

1. **增量交付**：每项能力独立 PR/提交，不破坏 reactor 现有 API（存量 60+ 控件
   与 `apps/winui` 现有调用零回归）
2. **性能契约**（REFERENCE §8，迁移时不可丢）：
   - 流式追加 O(1)（不重建文档）
   - 渲染不阻塞 UI 线程
   - 内容永不丢失（渲染失败降级字面文本）
3. **测试**：reactor 仓库 `cargo test` 全绿 + 新增能力单测（fixture 从 Web 侧
   对应测试移植）；请求方集成验证（`cargo check -p deepx-winui` + 可视化 demo）
4. **文档闭环**：能力交付时更新本需求单状态（✅/⏳/❌ + 版本）
5. **对接人**：DeepX 侧（`apps/winui` 维护者）；参考实现文件索引见下表

---

## 6. 参考实现索引（Web 端，验收对照物）

| 能力 | Web 参考文件（apps/winui/renderer/src） |
|---|---|
| markdown 渲染 | `lib/markdown-render-core.ts`（marked GFM + cleanMarkedHTML） |
| 流式 live/final | `components/MarkdownBody.tsx`（块拆分/哈希缓存/渲染代次） |
| 代码高亮 | `lib/markdown-render-core.ts`（shiki 27 语言 + wrapper） |
| 样式 token | `styles/markdown.css`（§7 token 表） |
| 数学公式 | `components/MarkdownBody.tsx`（katex auto-render 分隔符） |
| 图表 DSL | `lib/graph-dsl.ts` + `lib/graph-renderer.ts`（G6 v5） |
| 虚拟化/滚动锚定 | `components/conversation/ConversationTranscript.tsx` |
| 交互弹窗（ContentDialog 验证） | `src/interaction_overlay.rs`（XAML 侧现有实现） |

---

## 7. 交付优先级建议（上游排期参考）

1. **P0-1.1 + 1.2**（markdown + 代码高亮）——ChatView 全迁的主干，先做
2. **P1-2**（虚拟化列表）——与 1 并行，互不依赖
3. **P0-1.3/1.4**（数学 + 交互）——增量
4. **P1-3**（壳能力五件套）——独立小件，随时可插
5. **P2**（图表）——最后
