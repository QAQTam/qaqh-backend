# ChatView 渲染参考实现规格（Web 端）

> **定位**：Web 端 ChatView（`renderer/src`）的富文本/代码块渲染实现规格——作为
> WinUI 侧移植的 **golden reference**。终局宗旨：**移除 WebView**；策略：先在
> Web 端把渲染能力固化（本规格即固化产物），WinUI 侧待上游 WinUI3/reactor
> 富文本能力（重构/补丁）就绪后对照本规格移植样式与布局。
>
> 维护约定：Web 端渲染代码演进时**同步更新本规格**（特性矩阵/样式 token/
> 语义章节），保持"规格 = 实现"。

---

## 1. 渲染栈总览

```
raw markdown（turnProjection 产物，纯字符串）
  │
  ▼
MarkdownBody（组件，按块拆分 + 缓存）
  ├─ 流式 live 块 → marked.parseInline（仅已闭合内联语法，低开销）
  ├─ final 块   → Web Worker：marked(GFM) + shiki(代码高亮) → HTML
  │               （worker 失败/无 Worker → 主线程回退，内容永不丢失）
  ├─ katex 收尾（主线程，需 DOM）
  └─ mermaid 占位 → hydrateMermaidPlaceholders（主线程）
  │
  ▼
innerHTML 挂载 → markdown.css 样式 → 用户可见
```

**依赖**：`marked`（GFM）+ `shiki@4`（oniguruma wasm）+ `katex`（auto-render）
+ `mermaid` + `@antv/g6`（图 DSL 渲染）。

**线程策略**：marked + shiki 是纯字符串管道（耗时大头）→ Web Worker；
katex/mermaid/G6 需要 DOM → 主线程收尾。

## 2. 组件树与职责

```
ConversationTranscript        // 滚动容器：跟随尾部(120px 阈值)、锚点补偿、虚拟高度估算
└─ TurnGroup（keyed by turnId）// 会话 turn 分组：UserPromptBubble + AssistantAnswer
   ├─ UserPromptBubble         // 用户消息气泡
   └─ AssistantAnswer          // 助手回答：多轮(rounds) + 流式 live/final 状态
      └─ MarkdownBody          // 核心渲染组件（见 §4-§6）
VirtualTurn                    // 虚拟占位（高度估算 ESTIMATED_TURN_HEIGHT=120）
ProcessTimeline / ProcessDetail // 进程时间线（独立渲染族，无 markdown 依赖）
```

**滚动契约**（ConversationTranscript）：
- `BOTTOM_THRESHOLD = 120px`：距底部 ≤120px 时跟随尾部（自动滚底），否则保持阅读位置
- 锚点补偿：上方 turn 高度变化（流式展开）时，用 `[data-turn]` 锚点修正滚动位置
- 高度估算：`ESTIMATED_TURN_HEIGHT = 120`，`measuredHeights` 实测后替换占位

## 3. Markdown 特性支持矩阵（marked GFM）

| 特性 | 状态 | 实现 | 流式期间 |
|---|---|---|---|
| 段落/换行 | ✅ | marked `breaks: false` | inline 预览 |
| 标题 h1-h3 | ✅ | marked（h4+ 降级为粗体文本） | inline 预览 |
| **加粗** / *斜体* / ~~删除线~~ | ✅ | marked | ✅ inline 预览（已闭合语法） |
| 行内代码 `` `code` `` | ✅ | marked + 样式 | ✅ inline 预览 |
| [链接](url) | ✅ | marked | ✅ inline 预览 |
| 列表（有序/无序/嵌套） | ✅ | marked | inline 预览（未闭合由 marked 字面输出） |
| 引用块 | ✅ | marked | inline 预览 |
| 表格 | ✅ | GFM | final 全量 |
| 代码块（围栏 + 缩进） | ✅ | shiki 高亮（§4） | **等 final**（流式期间不 lex） |
| 任务列表 `- [ ]` | ✅ | GFM | final |
| 图片 | ✅ | marked（远程 URL） | final |
| 数学 `$..$` `$$..$$` `\(..\)` | ✅ | katex 主线程收尾 | final（含 `$` 检测快速跳过） |
| mermaid 图 | ✅ | 占位 → hydrate（§6） | final |
| G6 图 DSL（`graph-dsl`） | ✅ | 代码块 → 图数据 → G6 v5 | final |
| 原始 HTML | ⚠️ 有限 | marked 默认透传 | — |

**关键语义（移植时不可丢）**：
1. **未闭合语法字面输出**：流式期间 marked 对未闭合的 `**`/`` ` ``/`[` 按字面文本输出，
   不产生破损 HTML——这是"流式期间也能看到加粗/链接"的基础
2. 代码块/图表**等 final**：流式 live 只做内联解析（低开销），块级/高亮延迟到
   producer 封块（`final=true`）

## 4. 代码块规格（shiki）

**结构**（`buildMarkdownRenderer.renderer.code` 产出）：

```html
<div class="code-block-wrapper">
  <button class="code-copy-btn" aria-label="Copy code">…svg…</button>
  <span class="code-lang-label">rs</span>          <!-- 无 lang 时不输出 -->
  <pre class="shiki" style="…theme vars…"><code>…高亮 token…</code></pre>
</div>
```

- **语言表**（27 种）：ts/tsx/js/jsx/json/yaml/toml/rs/rust/py/python/go/java/kt/
  css/scss/html/bash/sh/shell/sql/graphql/md/markdown/diff/c/cpp/zig/nim
- **别名归一**：`h`→`c`、`hpp`→`cpp`；未知 lang → shiki 原样尝试，失败回退
  字面 `<pre><code>`（内容永不失真）
- **主题**：`github-light` / `github-dark`（显式传入，worker 无 document 不检测）；
  跟随 `data-theme` 属性切换
- **清洗**（`cleanMarkedHTML`）：剥掉 shiki 的 `background-color`（由 CSS 主题接管）
  与 `tabindex="0"`（焦点框污染）
- **复制按钮**：wrapper 内绝对定位右上角，复制原始文本（非高亮 HTML）

## 5. 流式渲染管线（MarkdownBody）

```
projectBlocks(text, final):
  final → [{ key:"f", hash, raw, stable:true }]          // 全量渲染一次
  live  → [{ key:"l0", hash, raw, stable:false }]        // cheap inline preview
```

- **块哈希缓存**（`blockHash`：`len:head…tail`）：内容未变不重渲染
- **渲染代次**（`renderGeneration`）：旧代次结果到达时丢弃（乱序保护）
- **final 渲染**：`renderMarkdownInWorker(raw, theme)` → HTML → `renderMath`(katex)
  → 挂载；worker 不可用回退主线程（`getShiki` 失败 → 无高亮纯 marked）
- **live 渲染**：`marked.parseInline`（rAF 节流，`livePreviewFrame`），只渲染已闭合内联语法
- **dispose 保护**：组件卸载后丢弃未完成渲染结果

## 6. 扩展渲染

| 扩展 | 机制 |
|---|---|
| **katex 数学** | 主线程 `renderMathInElement`；分隔符 `$$`/`\[ \]`（display）、`$`/`\( \)`（inline）；`ignoredTags` 含 pre/code（代码块内 `$` 不误渲染）；`throwOnError: false`；快速路径：HTML 无 `$`/`\(` 直接跳过 |
| **mermaid** | worker 产出 `createMermaidPlaceholder` 占位 → 主线程 `hydrateMermaidPlaceholders` 渲染；语言 tag = `MERMAID_LANG` |
| **G6 图 DSL** | `graph-dsl.ts` 解析特定代码块 → `graph-renderer.ts`（G6 v5）渲染 |

## 7. 样式 token（markdown.css，WinUI 移植基准）

| Token（CSS 变量） | 用途 |
|---|---|
| `--font-mono` | 代码字体族 |
| `--bg-secondary` / `--bg-tertiary` | pre 背景 / 行内 code 背景 |
| `--border-card` | pre/code 边框（0.5px） |
| `--radius-sm` / `--radius-md` | 圆角（行内 code / pre） |
| `--text-muted` | 语言标签与复制按钮颜色 |

**排版基准**：
- 正文：14px / line-height 1.75，段落间距 0.6em（首尾 0）
- 标题：500 字重，h1=1.2em / h2=1.1em / h3=1em，1em 上距
- 行内 code：0.88em、`--font-mono`、`--bg-tertiary` 底、0.5px 边框、1px 5px 内边距
- pre：`--bg-secondary`、12px 14px 内边距、`overflow-x: auto`、0.8em 上下距
- pre code：13px / line-height 1.6、无背景无边框
- 语言标签：10px / 700 / 大写 / 0.04em 字距，右上角定位（圆角 0 8px 0 6px 半透明底）
- 复制按钮：12px、`--text-muted`、右上角 z-index 2

## 8. 性能契约（WinUI 移植时必须保留的语义）

1. **渲染不阻塞交互线程**：markdown 全量渲染在 worker；主线程只做 DOM 收尾
2. **流式期间 O(1) 成本**：live 只做内联解析；块级 lex/高亮延迟到 final
3. **内容永不丢失**：worker 失败 → 主线程回退 → 无高亮回退（三级降级）
4. **稳定 key 防抖动**：turn/round/块哈希缓存，未变化内容零重渲染
5. **滚动锚定**：上方内容高度变化不打断阅读位置（锚点补偿）

## 9. WinUI 移植映射（建议，待上游富文本就绪）

| Web 概念 | XAML 建议对应 |
|---|---|
| `MarkdownBody`（块拆分/缓存/代次） | Rust 侧块拆分 + 渲染缓存（语义同 §5） |
| `marked`（GFM 解析） | Rust markdown 解析（`pulldown-cmark` 等）或上游能力 |
| `shiki`（27 语言高亮） | 上游 WinUI3 代码高亮 / `syntaxhighlight` 移植（同一 token 结构） |
| `katex` 数学 | 上游能力或 `katex` Rust 端口（语义同 §6） |
| `mermaid` / G6 | 保留 WebView 片段（若上游无等价物）或降级为图片 |
| `markdown.css` token | XAML `ThemeResource`（§7 对照表） |
| worker 线程隔离 | Rust 渲染天然不阻塞 UI 线程（tokio/线程池） |
| 滚动锚定 | `ScrollViewer` 变化事件 + 锚点偏移补偿 |

**降级阶梯**（上游能力未就绪时的 ChatView 渲染终局）：
`纯文本（保底）→ 简化 markdown（段落/标题/行内 code/代码块，无行内样式）→ 完整
markdown（对照本规格全特性）`——每级可独立上线，均以本规格为基准。

## 10. 已知缺口 / 待补

- 原始 HTML 透传策略未显式收紧（marked 默认行为，安全面待评估）
- 图片懒加载/失败占位未实现（远程 URL 直接渲染）
- `h4+` 标题降级行为未在组件层显式声明（依赖 marked 默认）
- 复制按钮无"已复制"反馈态（aria-live 缺失）
- 表格无横向滚动容器（长表格溢出行为依赖父容器）
