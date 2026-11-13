# Widget Directive 协议（QAQH Gen-UI v1）

状态：定稿待实施。逆向依据：Codex 桌面端 `739301/255119/774133` chunk 实测原文 + zcode Gen UI 契约。
决策（用户已拍板 2026-11-12）：A2UI/render_surface 即时移除；后端改动在本仓（LTS）；冒烟用当前模型配置。

## 1. 标记文法（wire format）

单行 block marker，出现在 assistant 正文中"希望渲染出现的位置"：

```
\uE200 visualize \uE202 {"path":"<绝对路径>.html"[,"mode":"wide","title":"…"} \uE201
```

- `U+E200` 起始、`U+E202` 分隔 type 与 JSON payload、`U+E201` 结束。三个 PUA 码位（Private Use Area），不可能与正常文本冲突。
- **解析规则**（仿 Codex markdown-it codexDirective，本项目落到 marked extension）：
  - block：标记必须独占一行（前后同范围内无非空白字符）才挂载 widget；
  - inline：`\uE200…\uE201` 出现在行内 → **静默吞成空文本**（渲染为空，绝不显示原始串，绝不进剪贴板复制源）；
  - JSON 解析失败 → 整行降级为普通文本占位（一行 `widget: <title|path basename>`），不报错、不阻断回合；
  - type 白名单：v1 仅 `visualize`。未识别 type 的 marker → 按 inline 规则吞掉（多端前向兼容）。
- 无 `::visualize` ASCII 兼容层（宪法 4：不做兼容性套壳）。

## 2. Fragment 目录与文件契约

- 会话级可视化目录：`<workspace>/.qaqh/widgets/<session_id>/`（session 数据根 `.qaqh` 已是事实标准，见 qaqh-runtime service/fs_git.rs）。daemon 在 session bootstrap 时创建；路径写入 system 环境块（与 tool result `[#ctx …]` 同层），字段名 `widgets_dir`。
- 模型侧纪律（写进 skill）：
  - 只写 **HTML fragment**：无 `<!doctype>/<html>/<head>/<body>`；根节点带唯一 id，`document.getElementById` 选取，禁 `document.currentScript`；
  - 字面 markup（禁转义地狱）；CSS/JS 内联仅在基础类不够时；禁 `fetch/XHR/WebSocket`；单文件 ≤1MB；
  - 文件名 ASCII 小写连字符 `<title>.html`；更新=重写同一路径 + 同一 turn 重发 marker。
- 前端读取：daemon `FsRead` 放行 `.qaqh/widgets/**`（工作区外仍拒绝）。**实施裁定（2026-11-12 会话2）：`qqh-widget://` 特权协议裁撤**——宿主本来就要拿到 fragment 文本做状态占位注入（§3 srcdoc 装配），通道直接用 daemon `fs.read`（scope_session 锁会话工作区，天然白名单）；再注册一条协议是同一目标两条路，属冗余攻击面。Electron 侧零新增 scheme。

## 3. 宿主桥 `window.qqh`（iframe 内注入）

| 成员 | 语义 |
|---|---|
| `window.qqh.widgetState` | 初始态（可 null），来自占位 script |
| `setWidgetState({modelContent, privateContent})` | 整快照替换；**TextEncoder UTF-8 字节数 ≤16KiB**（超限抛 TypeError）；乐观更新，本地先写后 ack；保存**永不**触发 turn |
| `sendFollowUpMessage({prompt, title})` | title ≤250 字符=确认弹窗标题；用户确认后经 RingingV2 user-send 发出；只读/失效会话拒绝 |
| `openExternal({href})` | 绝对 http(s)；fragment 内普通 `<a>` 直接放行新窗 |

- 状态注入：宿主把 fragment 包进沙箱文档时替换占位符
  `<script type="application/json" id="qqh-widget-state">__QQH_WIDGET_STATE__</script>`，桥脚本 JSON.parse 后即 `remove()`；序列化时 `<` 一律 `\u003c` 转义。
- 加固（照抄 Codex）：在不受信 fragment 执行**之前**，用 `Object.getOwnPropertyDescriptor` 捕获 `MessagePort.prototype.postMessage/addEventListener/start`、`MessageEvent.prototype.data/ports/source`，一律 `Reflect.apply` 调用；`stopImmediatePropagation` 同理。
- 通道：宿主 `new MessageChannel()`，port2 经 postMessage 移交（targetOrigin 校验），port1 收 `setWidgetState`/`sendHeight`/`sendFollowUpMessage` 帧。iframe `sandbox="allow-scripts allow-forms allow-modals allow-popups"`，**禁 allow-same-origin**。
- 高度：桥脚本 ResizeObserver → `sendHeight` → 宿主设 iframe 高（初值 240px，上限 10000px）。

## 4. modelContent → 模型上下文回流（RingingV2 接线）

- RingingV2 user-send 命令（daemon `axum_impl/commands.rs` 入口 + `qaqh-ringing-contract` DTO）新增可选字段：
  ```
  widget_context: { session_id, widget: {path, title}, model_content: Json } | null
  ```
- 语义：前端在用户点击 `sendFollowUpMessage` 确认时携带**当下最新** `modelContent` 快照；daemon 把它作为结构化 input metadata 注入 agentloop（渲染进 system/reminder 块），**绝不拼进用户可见文本**；`privateContent` 永不出前端存储（Electron userData，按 fragment 绝对路径键——**实施裁定：`workspace|session|path` 三段键收缩为 path**，绝对路径唯一编码 workspace+session，语义等价且渲染层少一个易变句柄）。
- best-effort：注入失败（字段缺失/版本不匹配）→ 回合照常，仅少上下文。
- 此为 LTS 用户空间小破坏（新增可选字段），符合宪法 15。

## 5. skill：`qaqh-visualize`（bundled）

`crates/qaqh-skills/assets/bundled/qaqh-visualize/SKILL.md`，纪律条款直抄 Codex SKILL.md 并本地化：
- 触发边界（对话内可视化请求 vs 项目文件请求；表格用 Markdown、静态结构图用 Mermaid）；
- 第 2 节文件契约全文；
- marker 只在**最终回复**发射、每 turn 对新建/更新的每个 widget 各一行、绝不宣告实现细节（不出现 HTML/文件/iframe 字样）、静默工作；
- 主题变量表（前端提供的 CSS token）与排版底线（后续可扩 charts/）；
- 冒烟门（本仓 `probes/widget-marker-smoke.mjs`，DPAPI 读当前配置凭据、带真
  write 工具回路）：qwen 当前配置 10 轮，marker 合规率 <80% 时收紧措辞（示例先行 + 反例列举）。
- **实施修订（2026-11-12 冒烟实证）**：qwen3.8-flash 对 Windows 路径的 JSON 转义
  三连败（单反斜杠非法 JSON → 过度转义 `\\\\`），反斜杠文法不可收敛。裁定
  **marker path 一律正斜杠形态**（全链消费方天然接受），且 system 环境块的
  `widgets_dir` 直接以正斜杠注入（模型零转换原样复制）。skill 全文已零反斜杠。

## 6. A2UI 移除清单（即时执行，不留兼容）

后端本仓：
- 删 `crates/qaqh-genui/` 整 crate + workspace Cargo.toml members；
- `crates/qaqh-workspace/src/registration.rs`：摘 render_surface 注册；
- `crates/qaqh-tool-core/src/lib.rs` + `src/tool_api/display.rs`：摘 genui 依赖与 display 工具项；
- `crates/qaqh-types/src/api_types.rs`：删 `surface_rendered` ToolEffectInfo 变体及关联类型；
- `crates/qaqh-daemon/src/axum_server/v2_storm_tests.rs`：摘相关断言；
- `docs/genui-surface-tool-contract-2026-11-12.md` 顶部加 superseded-by 指向本文档。

前端仓（qaqh-electron）：
- 删 `webui/src/genui/`（v1/v2 编译器、store、SurfaceView2、dataModel、eval2）；
- `webui/src/tools/StepRow.tsx`：删 surface_rendered 分支与 lazy import；
- `webui/src/genui-check.tsx` + `genui-check.html`：替换为 `widget-check`；
- 全局搜 `surface` 残留样式（styles/components）清灰；
- 新增：`webui/src/widget/`（directive tokenizer for marked、widget-host、bridge 注入）、Electron `qqh-widget://` 协议注册、发送通道 `widget_context` 接线、`api/qaqh` 重新生成（含新字段、无 surface_rendered）。

## 7. 实施顺序（跨会话可恢复）

1. 本仓：§6 移除 → 编译绿 → §2 widgets_dir + FsRead 放行 → §4 DTO（含 contract 类型再生成）→ §5 skill 落 bundled；
2. 前端仓：§1 tokenizer(marked extension) → §3 iframe+桥+协议 → widget-check.html 静态用例 → §4 发送接线；
3. 冒烟 §5 → 手测 → 双方各自 typecheck/cargo check 通过即完工。不做全局测试（宪法 10）。
