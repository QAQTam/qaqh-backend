# WinUI ChatView 渲染维护契约

> 适用范围：`deepx-domain` / `deepx-client` / `apps/winui` /
> `markdown-core` / `markdown-winui` / `windows-reactor` 同步与重构。
>
> 目标：上游发生较大变化时，维护者能从协议事实逐层推导本地改动，避免重新
> 引入 Web 兼容协议或两套相互矛盾的增量渲染机制。

## 1. 唯一渲染路径

```text
Ringing typed event
  → BridgeCore（按 seed 隔离、异步 I/O）
  → chat_adapter（领域事件 → 展示事件）
  → Transcript（唯一可变展示模型）
  → TurnView / RoundView / AnswerView
  → chat_view 声明 Element 树（稳定 key）
  → windows-reactor reconciler
  → WinUI 3 XAML 控件
```

必须保持以下边界：

- `Transcript` 更新 Rust 状态，不创建或保存 XAML 控件；
- `chat_view` 只声明当前状态对应的 `Element`，不重放命令式 patch；
- `windows-reactor` 是唯一的控件树 diff/patch 层；
- Bridge 承担 HTTP/SSE、后台任务、seed 隔离，渲染 crate 不持有 client；
- `TranscriptChange` 只描述 `None / Live / Structural` 和 extent 是否可能变化，
  不携带 RichText、工具卡或整段正文副本。

禁止重新引入 `RenderCommand` 一类“第二 reconciler”。如果 profiling 证明 reactor
无法满足某个热点，应先优化稳定 key、控件粒度或 reactor backend；只有决定完全
替换声明式路径时，才允许设计命令式路径，不能让两者并存。

## 2. Ringing 事件语义

| 事件 | 合并语义 | Transcript 行为 | 失效等级 |
|---|---|---|---|
| `RoundDelta` | append | 只追加目标 `(turn, round, kind)` | `Live`，工具卡为 `Structural` |
| `BlockCheckpoint` | replace | 覆盖目标块完整值；相同值为 no-op | `Live` / 工具卡 `Structural` |
| `ProviderToolStatus` | replace by `call_id` | 同 id upsert，相同状态 no-op | `Structural` |
| `RoundCompleted.answer` | authoritative final | final parse 一次；之后忽略迟到 delta | `Structural` |
| `RoundCompleted.output_ref` | authoritative external final | 保留 live preview，进入 loading，后台下载后 resolve | `Structural` |
| `TurnCompleted/Failed` | terminal status | 只改变状态，不请求跟尾 | `Structural`, extent=false |

相邻 delta 只允许在同一 UI 帧、同一 `(turn_id, round_num, kind)` 内拼接。不得跨
checkpoint、工具状态或 round 边界排序。未知事件必须安全 no-op。

## 3. `output_ref` 闭环

外置正文不是 UI 命令：

1. `Transcript::apply` 把 `PendingOutput` 放入模型队列，并设置
   `RoundView.output_loading`；现有 live preview 继续显示；
2. `ChatView` drain 请求并调用 `BridgeCore::spawn_resolve_chat_output`；
3. `deepx-client::download_content` 请求
   `GET /ringing/v1/content/{content_id}?seed=...`，带 session header，并校验 SHA-256；
4. UI 泵 drain `ChatOutputResolution`，调用 `resolve_output` 或 `fail_output`；
5. 失败必须可见，不能用空 final 静默覆盖 preview。

同步上游时重点核对 `ContentRef` 字段、所有权校验、过期语义、媒体类型和摘要算法。
若 wire schema 改变，先改 canonical domain/client，再改 Bridge 适配，不能在 ChatView
里解析兼容 JSON。

## 4. 上游大改同步顺序

每次同步 Ringing、windows-rs、Windows App SDK 或 windows-reactor：

1. 固定基线：记录 upstream commit、Windows App SDK 版本和本地 fork commit；
2. 对照 canonical 类型：事件 variant、字段 optionality、append/replace/final 语义；
3. 更新 `deepx-client` transport，并在 client 层完成鉴权、游标、校验和错误分类；
4. 更新 `chat_adapter`，确保它只做窄映射，不维护第二份会话状态；
5. 更新 Transcript 状态转移与纯 Rust contract tests；
6. 更新 `chat_view` 的控件映射、key 和无障碍语义；
7. 最后更新 windows-reactor fork/backend，避免用底层补丁掩盖上层模型错误；
8. 运行下方门禁并更新本文件中的契约差异。

发生冲突时按“协议事实 → 模型不变量 → 声明式控件能力”的方向解决，不能从旧 UI
形状反推协议。上游新增能力优先封装在 `deepx-fluent` 或 reactor 的通用 API，页面
不直接依赖实验 XAML 类型。

## 5. 性能判断边界

- `scale.rs` 只测 Transcript 寻址/回放，不证明 XAML 或 reactor 性能；
- 16ms frame coalescing 减少同一活尾的重复解析，但当前行内 markdown 仍会对
  可见活尾整体重解析；这是明确、可测的实现，不宣称 O(1)；
- `LiveTableTracker` 只对新增表格行增量扫描；
- 真正的 UI 性能要分别记录：model apply、Element 声明、reactor reconcile、XAML
  layout/realize、跟尾滚动；不能把总耗时归因给某一层；
- 在 profiling 证明瓶颈前，不增加块哈希缓存、Run 级 patch 或另一套生命周期。

## 6. 变更门禁

```powershell
$env:CARGO_INCREMENTAL='0'
cargo fmt --all -- --check
cargo check -p markdown-winui -p deepx-client -p deepx-winui
cargo test -p markdown-core -p markdown-winui -p deepx-client --lib --tests
rg -n "RenderCommand|StreamingMarkdown|XamlFrameUpdate" crates apps/winui
```

最后一条应无生产命中。Golden reference 文档可描述旧 Web 实现，但必须明确它是
能力对照，不是当前 WinUI 架构。

代码评审至少回答：

- 新状态的唯一所有者是谁？
- 重放相同事件是否 no-op？
- seed 切换后异步结果会不会串会话？
- 是否保留 checkpoint 与 authoritative final 的覆盖语义？
- 是否改变内容 extent，跟尾请求是否必要？
- 稳定 key 是否来自领域 identity，而不是数组瞬时位置？
- 测试断言的是模型事实，还是复制了一套实现细节？
