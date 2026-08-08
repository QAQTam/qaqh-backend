# ChatView 性能修复记录（2026-08）

> 症状：ChatView 内大量文字/markdown（长回答流式、大代码块）时 UI 冻结，
> 滚动条卡死，其它区域全部无响应。
> 根因分析见会话记录：全链路 UI 线程 + 视口内行每帧全量重建 + RichTextBlock
> 无 diff 重建 + syntect 高亮无缓存。

## 改动清单

### 1. syntect 高亮产物缓存 — `crates/markdown-winui/src/highlight.rs`

`highlighted_code_block` 按 `(lang, text, dark, font_family)` memo 化（thread_local
LRU，容量 64）。视口内行 rebuild 时内容未变的代码块不再重复 lexing——
大代码块单次 syntect lex 数十毫秒且发生在 UI 线程，这是"大量代码块卡顿"
的首要热点。

### 2. 窗口快照 Rc 缓存 — `crates/markdown-winui/src/round_renderer.rs` + `apps/winui/src/chat_view.rs`

- `Transcript` 新增 `rev`（`mutation_rev()`）：所有变更入口无条件 +1；
- `chat_view` 缓存 `(rev, Rc<Vec<TurnView>>)`：rev 未变时复用同一 Rc；
- 配合 vendor 补丁 `list_view_rc`，`same_items_as` 按 `Rc::as_ptr` 命中 →
  reconciler **完全跳过已 realize 行的 rebuild 与 diff**。

效果：流式期间每帧不再深拷贝 30 turns、不再重建视口内全部行的 Element 树，
只 diff 真正变化的行。这是"每 50ms 卡一下"的主因。

### 3. 节流回退 — `apps/winui/src/chat_view.rs`

`SCROLL_REQUEST_THROTTLE` 50ms → 100ms（滚动请求与 live 渲染共用）：
断开"滚动 → 行 realize → 渲染 → 再滚动"反馈循环（原注释即警告过该现象）。

### 4. RichTextBlock 段落级 diff — vendor 补丁 `backend/winui/mod.rs`

`set_rich_text_paragraphs` 增加按 ControlId 的段落快照缓存：内容未变直接跳过
`Blocks.Clear()` + 全量 Run 新建（该操作会触发整段文本重新布局）。
流式期间未变化的富文本块不再反复重建。

### 5. `list_view_rc` API — vendor 补丁 `reactor/src/widget.rs`

`TemplatedListBuilder::new_rc` + `list_view_rc`：接收 `Rc<Vec<T>>`，
数据未变时复用同一 Rc（见 #2）。

## 依赖管理（最终方案：git rev 指向 downstream snapshot）

- 三处 manifest（`apps/winui`、`crates/deepx-fluent`、`crates/markdown-winui`）
  以 `rev = "88c68602d3e79c0acb95085da68144be219d49ad"` 消费
  `QAQTam/windows-rs` 的 `deepx/snapshot-v4` 分支（无父提交快照，含本批
  ChatView 性能补丁）；
- downstream 维护仓库：`F:\DeepX-windows-rs`（`DEEPX-DOWNSTREAM.md` 为
  维护手册）。升级流程：该仓库 `codex/chatview-perf` 集成分支 rebase 上游 →
  生成新 snapshot 分支 → 推送 fork → DeepX 三处 rev 改新 commit；
- 补丁清单见 `DEEPX-DOWNSTREAM.md`（Streaming list perf / RichText commit perf）。

## 协作者 / LLM 上下文（改 downstream 前先读）

本地 downstream 仓库：**`F:\DeepX-windows-rs`**（QAQTam/windows-rs 的本地工作树，
维护手册见其根目录 `DEEPX-DOWNSTREAM.md`）。切换脚本：`scripts/dev-downstream.ps1`
（`on` / `off` / `status`）。

给 LLM 的标准上下文模板（可整段粘贴）：

> DeepX 的 windows-reactor / windows / windows-reactor-setup 依赖指向
> `QAQTam/windows-rs` 的 immutable snapshot commit `88c68602d3e79c0acb95085da68144be219d49ad`
> （无父快照分支 `deepx/snapshot-v4`，含 ChatView 流式性能补丁）。
> 本地下游工作树在 `F:\DeepX-windows-rs`（集成分支 `codex/chatview-perf`，维护手册
> `DEEPX-DOWNSTREAM.md`）。需要改动下游代码时：
> 1. 在 `F:\DeepX-windows-rs` 的集成分支上改（遵守 DEEPX-DOWNSTREAM.md 的补丁清单与
>    生成绑定规则：`bindings.rs` 不许手改，改 `base.txt` 跑 `cargo run -p tool_reactor`）；
> 2. 跑 `pwsh scripts/dev-downstream.ps1 on` 让 DeepX 指向本地 → `cargo check` 验证；
> 3. 验证后 `off` 恢复 git rev；提交 downstream → 按 runbook 生成 `deepx/snapshot-vN`
>    并推送 → DeepX 三个 manifest 的 rev 改新 commit。
> 注意：DeepX 工作区的 `composer_bar.rs` / `deepx-fluent/src/lib.rs` 有历史未提交
> 改动，提交时勿混入本次变更。

## 验证

- `cargo check -p markdown-winui` / `-p deepx-winui`：通过；
- `cargo test -p markdown-winui`：10 passed；
- `cargo test -p deepx-winui`：60 passed；
- 实机验证建议：长回答流式 + 500 行以上代码块滚入视口 + 滚动跟手性。

## 后续可选项（本次未做）

- markdown 解析 / syntect / mermaid SVG 生成移后台线程（UI 只做 diff+提交，
  对齐 CHATVIEW-RENDERING-REFERENCE §8"渲染不阻塞交互线程"）；
- `answer_delta` 增量解析（跨 delta 边界语法需保留扫描状态机，风险较高）；
- turn 内部子虚拟化（超大 round 拆 realize 单元）。
