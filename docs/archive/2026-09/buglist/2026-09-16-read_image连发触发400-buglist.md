# buglist（2026-09-16）— 一个回合里连发两次 `read_image`：图片降级出的合成 user 消息插进 tool 消息串 → HTTP 400

> 登记规则：一行一个缺陷；**详情进 `docs/report/`**，本文件只做索引与状态跟踪。
> 状态口径：`open` / `fixed（工作区，待提交）` / `fixed @{commit}` / `verified` / `wontfix`。
>
> 本条目**未单开 report**（证据自足并逐条内联于下）。
> 发现来源：**机主真机报告**——「连续两次 read_image 会报错」，附 TUI 截图
> `屏幕截图_20260916_001353.png`：
> `model_request_failed: OpenAI API HTTP 400 (Bad Request - 格式错误): … An assistant message …`。

## 缺陷

| ID | 状态 | 项 |
|---|---|---|
| BUG-2026-09-16-01 | `fixed @1c13662` | **gate 的 chat-completions 降级把 `read_image` 的图片写成合成 `user` 消息并「就地」插入**。一个 assistant 消息里的多个 tool_call（并行批）会产生**连续多条** `tool` 消息，合成 user 被插在中间 ⇒ 违反「assistant(tool_calls) 之后必须紧跟全部对应的 tool 消息」⇒ 上游 400，且 400 不可重试 ⇒ 整个回合 Fatal。触发与图片内容无关，**两条带图 tool 结果就够了**。 |

## 事实与证据

| # | 事实 | 位置 |
|---|---|---|
| 1 | `convert_messages` 在 `"tool"` 分支里边遍历边落盘：`ToolResult` → `role:"tool"`，图片块 → **紧跟**一条合成 `role:"user"`（含 `image_url` data URI） | `crates/qaqh-gate/src/chat_completions_api.rs:843-885`（修复前） |
| 2 | 并行批里一个 assistant 消息带多个 tool_call ⇒ 消息串退化成 `assistant → tool(c1) → user(img1) → tool(c2) → user(img2)` | 同上 |
| 3 | OpenAI 兼容端点要求 tool 消息紧贴 assistant(tool_calls)，**不得被其它角色打断** ⇒ 400 `An assistant message with 'tool_calls' must be followed by tool messages responding to each tool_call_id` | 机主截图（当时 provider = `opencode-go`，`endpoint = "openai"`，即本路径） |
| 4 | 该 400 被 transport 判为不可重试 ⇒ 回合 Fatal，用户只能看到 `model_request_failed` | `qaqh-gate/src/transport.rs` 错误分级 |

## 复现（修复前实测，先红后绿）

`cargo test -p qaqh-gate --lib parallel_tool_result_images`

```
left:  ["assistant", "tool", "user", "tool", "user"]
right: ["assistant", "tool", "tool", "user", "user"]
```

测试构造：`assistant(2 × tool_call)` + 两条 `tool` 消息，各带 `ContentBlock::ToolResult` + 一张 `ContentBlock::image`。

## 修复

`convert_messages` 引入 `pending_media`：合成 media 消息**不在遍历中就地落盘**，改为入队，等这串 tool
消息走完（遇到非 `tool` 消息，或消息列结束）再统一 `out.append`。

- `chat_completions_api.rs:746` 声明队列；`:748-750` 非 tool 消息前先冲刷；`:867`/`:880` 图片块改为入队；`:900` 收尾冲刷。
- 修复后角色序列：`assistant → tool → tool → user(img1) → user(img2)`，且后面接普通消息时合成消息仍留在 tool 段内。
- 回归测试：`chat_completions_api::skill_envelope_tests::parallel_tool_result_images_do_not_split_the_tool_run`
  （覆盖「消息列尾」与「后面还有 assistant」两种落盘时机）。
- `chat_sync_openai`（compact / title 的同步路径）复用同一 `convert_messages`，一并修好。

## 同类路径核查

| 协议 | 图片投放 | 结论 |
|---|---|---|
| Anthropic（`message_api.rs`） | **真图**：`tool_result` 内部附 `{"type":"image","source":{"type":"base64"}}`；用户上传的图仍是 `[Image #N]` 文本占位 | **无此缺陷**：图片跟 `tool_result` 同属一条 user 消息，随后「Merge consecutive same-role messages」把同一 assistant 的多条 tool_result 合成**一条** user ⇒ 符合 Anthropic 的严格交替要求。已用 `parallel_tool_result_images_merge_into_one_user_turn` 锁住：**变异验证**（把合并条件改为 `false`）后该测试转红（`["user","assistant","user","user"]`），确认锁的是合并行为本身 |
| Responses（`responses_api.rs`） | **真图**：合成 `message` item 里的 `input_image` + data URL；用户上传的图同样只是占位文本 | **形态同源但本次不改**：合成 `message` item 仍就地插在 `function_call_output` 之间。Responses 协议按 `call_id` 配对、不要求「assistant 后紧跟全部 output」，判断不会触发；**本机没有 Responses 端点可实测**，故不动，登记为观察项 |

## 验证

- `cargo test -p qaqh-gate`：108（lib）+ 37（integration）passed。
- `cargo clippy -p qaqh-gate --all-targets`：零 warning。
- 修复前该测试先运行并确认失败（输出见「复现」），非事后补测。

## 未做 / 待办

- 运行中的 daemon 是**预编译二进制**（`~/Desktop/ws/qaqh-daemon`，build_id `6b105bf5`，落后 main 一个提交）。
  本修复要真机生效需重新构建并重启 daemon；本次**未重启**（会中断机主正在用的会话）。

---

## ⚠️ 结论修正（2026-09-16，见 `2026-09-16-anthropic-400-真因-上下文超限-buglist.md`）

本文把「anthropic 路径 400」归因于 tool_result 形态是**不完整**的，请以新文档为准：

- 形态修复本身是对的（`[tr,tr,img,img]` 实测 200、`[tr,img,tr,img]` 400），**但它不是
  线上 400 的原因**。真因是 **`context_limit` 配成 1,000,000（端点实为 ~245k）⇒ 
  自动压缩永不触发，会话涨过上限后每个请求都被 400**，错误体只有 `{"model":...}`。
- 「双 read_image 触发 400」的相关性来自：它恰好是那几轮的新增内容，把请求推过了上限；
  图片形态/尺寸/数量、body 体积、消息条数、`max_tokens`、stateful 均已逐一实测排除。
- 新增次生问题：**工具图被投影两遍**（内联 `result.images` + 兄弟 `ImageRef`，16 张图
  → 32 个 image 块），已在 `message_api.rs` 去重并加锁。
