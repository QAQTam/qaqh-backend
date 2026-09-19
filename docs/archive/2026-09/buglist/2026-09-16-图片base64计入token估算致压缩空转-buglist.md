# BUG-2026-09-16-05：图片 base64 被当普通文本计入 token 估算 → 带图会话每轮触发 auto-compact（压缩空转 + prompt cache 反复失效）

> 状态：**已定位、已取证，未修**（用户决策：本条目只登记，另开任务修）。
> 关联：`2026-09-16-anthropic-400-真因-上下文超限-buglist.md`（BUG-2026-09-16-04）、
> `2026-09-16-read_image连发触发400-buglist.md`。

## 症状

带图会话里，`auto-compact preflight` **几乎每轮都判定超阈值**并执行压缩，压完之后
`post-compact preflight` 仍报超阈值；随后进入「压缩 → prompt cache 失效 → 再压缩 → 压不动」
的空转循环。请求本身仍是 200（真实 token 未超端点上限），所以**不表现为 400**。

```
[1789494061] auto-compact preflight: source=estimate, decision=321950, raw=348545, predicted=304522, upper=321950/240000 tokens (12 samples, 80% threshold)
[1789494077] auto-compact done: 26200 → 9066 tokens
[1789494077] [PREFIX] cache key changed: message[1] — expect cache miss
[1789494077] post-compact preflight remains above threshold: source=estimate, decision=302274/240000, upper=302274
[1789494085] auto-compact preflight: decision=305554, raw=330795, ...
[1789494127] auto-compact preflight: decision=315781, raw=341867, ...
[1789494131] auto-compact preflight: decision=316410, raw=342548, ...
[1789494154] auto-compact preflight: decision=319194, raw=345561, ...
[1789494181] auto-compact preflight: decision=321861, raw=348449, ...
[1789494206] auto-compact preflight: decision=324302, raw=351092, ...
[1789494220] auto-compact produced no change; suppressing retry until context changes
```

配置：`context_limit=240000`、`auto_compact_threshold=0.8` ⇒ 阈值 `0.8 × 240000 = 192,000`。

**关键自相矛盾帧**：同一帧里 `auto-compact done: 26200 → 9066 tokens`（文本确实压掉了 65%）
却紧接着 `post-compact ... decision=302274`。差额不可能来自文本，只能来自图片字节。

## 结论（先给答案）

**`prepared_request_metrics()` 把 `(messages, tools)` 整体 `serde_json::to_string()` 后直接
交给 `count_tokens()`，而 `ToolResult.images[].data` 的内联 base64 就在里面 —— 于是 1MB 的
base64 按"普通文本"被计成 ~29–31 万 tokens，而该端点（anthropic 口径）按像素计费只要 ~3.2k，
高估约 90 倍。**

估算里**没有任何图片感知**（`rg image` 在 `token_calibration.rs` / `engine_compact.rs` 零命中）。
`ContentBlock::ImageRef.bytes_len` 的文档注释写着「占位符显示与 **token 估算**用」，但实际只有
展示路径消费它（`message_api.rs:110`、`chat_completions_api.rs:785`、`responses_api.rs:302`），
**估算路径从未接上**。

## 证据链

1. **代码链（估算）**
   `engine_turn.rs:875-877` `estimate_prepared_request(&messages, Some(&tool_defs))`
   → `token_calibration.rs:153-168 prepared_request_metrics()`
   ```rust
   let serialized = serde_json::to_string(&(messages, tools)).unwrap_or_default();
   let raw_tokens = u64::from(qaqh_types::count_tokens(&serialized)).max(1);
   ```
   → `token.rs:19-28 count_tokens()`（`tokenizers` 特性默认开启 → 真实 BPE；关闭时退化为
   `count_tokens_heuristic()`，非 CJK 按 `len / 3.3`，`token.rs:38-`）。
   `raw_tokens` 再经 `token_calibration.rs:48-72 estimate()` 得 `predicted/upper_bound`，
   最终由 `engine_turn.rs:882-883` 与阈值比较。

2. **膨胀源**
   本会话两张实测图（`sessions/9aebf274/messages.jsonl` 的 `msg 1072` / `msg 1073`，
   每条的 `tool_result` 各带 1 张内联图）：
   | 图 | 尺寸 | 内联 base64 字符数 | anthropic 像素计费 `w×h/750` |
   |---|---|---|---|
   | `屏幕截图_20260915_231748.png` | 761×454 | 126,348 | 461 |
   | `屏幕截图_20260915_232627.png` | 1920×1080 | 894,592 | 2,765 |
   | 合计 | — | **1,020,940** | **3,226** |

   同 1,020,940 字符经 BPE（~3.5 字符/token）≈ **292k**，启发式（/3.3）≈ **309k**
   ⇒ **高估 ≈ 90×**。与 `post-compact decision=302274` 同量级，吻合。

3. **量化对照（日志）**
   加图前 `[1789493916] preflight: decision=203176, raw=184705`；两张图进入上下文后
   `raw` 涨到 348,545，`decision` 越过 192,000 阈值 → 触发压缩，且**压完仍 302,274**。
   即"文本可压、图片字节压不掉"，估算被图片钉死在阈值之上。

4. **空转闭环**
   `engine_turn.rs:904` `run_auto_compact` → `:906-925` 重新估算并 warn
   → 下一轮 `auto_compact_allowed()` 因 `context_revision` 变化再次放行
   → 每次压缩都改 `message[1]` ⇒ `[PREFIX] cache key changed … expect cache miss`
   → 直到 `auto-compact produced no change; suppressing retry until context changes`。

5. **对照：本次连发 read_image 全部成功（无 400）**
   同一会话并发两次 `read_image`（761×454 与 1920×1080），两条 `tool` 消息各挂 1 张图
   （`msg 1072/1073` = `tool_result(imgs=1)` + 兄弟 `image_ref`），`t44` 各轮均
   `gate succeeded`，日志无 400。
   ⇒ **BUG-04 的修复有效；本 bug 不影响请求成功，只造成压缩/缓存空转。**

6. **同一盲点还有第二处（顺带记录，未验证影响）**
   `tool_result.rs:433 estimate_tokens()` = `chars/4`，用于 `ToolModelPayload.total_tokens`
   （`:231/:318/:328/:340`）——同样把图片 base64 当文本，只是那条链路当前不参与压缩决策。

## 影响面

- **闸门对带图会话失效**：任何含图会话的估算恒 > 阈值，`context_limit` 形同虚设；
  方向是**高估 → 过早压缩**（安全但不经济），而不是低估漏拦。
- **成本/延迟**：每轮压缩 ⇒ 每轮 `message[1]` 变化 ⇒ prompt cache 稳定失效
  （缓存命中率归零，输入按未缓存计价）。
- **触发条件**：只要上下文里存有内联 base64 图片（`ToolResult.images[].data`），该字节数
  就按普通文本计入估算。本次两张中等截图（合计 1.02M 字符）即让 `raw` 从 ~18 万跳到 ~35 万。
- **不影响**：请求成功与否（真实 token 远低于端点上限）、BUG-04 的 400 修复。
- 另注：`request_key` 是对含 base64 的整串取 hash，图片一变 key 就变 ⇒
  `api_context_by_request`（精确请求绑定）很难命中，日志里 `source=` 始终是 `estimate`
  而未见 `api`。此点仅作观察记录，未单独取证。

## 复现步骤

1. 配置 `context_limit=240000`、`auto_compact_threshold=0.8`（阈值 192,000），
   端点用 anthropic 兼容路径。
2. 会话中连续 `read_image` 两张真图（例：761×454 与 1920×1080 各一）。
3. `tail ~/.config/qaqh/qaqh-daemon.log`，观察 `auto-compact preflight` 的
   `decision` 跃过 192,000 且接近 `raw × 0.87`。
4. 核对同帧 `auto-compact done: A → B`（B 远小于 A）与
   `post-compact … decision` 仍 >192,000 —— 两者矛盾即为本 bug。
5. 旁证：`python3` 统计 `messages.jsonl` 中带图消息的 base64 字符数，
   按 `/3.5`（或 `/3.3`）折算即可复现 `decision` 量级。

## 修复方向（**未实施**，供另开任务选择）

落点建议统一在 `prepared_request_metrics()` 序列化之前：对消息做一趟「图片摘要化」——
把 `ToolResult.images[].data` / `ContentBlock::Image.data` 换成等价计费长度的占位符，
再交给 `count_tokens`。此方案同时让 hash 对 base64 抖动免疫（见「影响面」末条）。

- **A. 按像素计费（最准）**：图片按 `ceil(w × h / 750)`（anthropic 口径）或 `ceil(w × h / 750)`
  上限截断计费。需要 `ImageRef` / `ToolImage` 增加 `width` / `height`（**契约变更**，
  且要兼容旧会话缺字段的情况，或从 base64 头解析尺寸）。
- **B. 保守固定上限（改动最小）**：剔除 base64，每张图按固定上限计
  （如 `MAX_DIMENSION = 2000` → `2000×2000/750 ≈ 5334`；或取 1600）。消除爆炸，
  不引入契约变更，代价是偏保守（仍可能过早压缩）。
- **C. 完全剔除、靠 provider usage 校准**：估算里图片计 0，依赖 `api_context_tokens`
  修正。最小，但**可能低估至超限**，与 240k 闸门冲突，不建议单独使用。

无论选哪种，都建议补一条回归锁：构造含 1MB base64 图片的消息，
断言 `prepared_request_metrics` 的 `raw_tokens` 不随图片字节线性增长。

## 复盘

- 「`auto-compact done: 26200 → 9066`」与「`post-compact decision=302274`」同帧并存，
  是**两个不同度量的同一份上下文**被并列打印造成的假象：前者只计文本，后者把图片字节
  当文本。两条日志并排看反而互相证伪，是这次能定位的关键线索。
- 之前把此现象记成「post-compact 剩余计数偏高」的**现象描述**；真正根因（图片 base64
  计入）由这两行矛盾 + `prepared_request_metrics` 的实现共同确定，不是估算器的精度问题。
- `ImageRef.bytes_len` 的注释早已写明用途包含「token 估算」，但估算路径没有实现它 ——
  **注释里的意图若没有对应调用点，等于没有**；排查时应以 `rg` 实际消费方为准。
