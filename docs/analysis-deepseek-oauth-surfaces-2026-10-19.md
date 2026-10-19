# DeepSeek 账号（OAuth）的四个面：请求体缓存、缓存命中回传、官方搜索/抓取、wire 研判

> 日期：2026-10-19　范围：`qaqh-gate` / `qaqh-runtime` / `qaqh-config` / `qaqh-dsaccount`
> 外部依据：`api-docs.deepseek.com/zh-cn` 的 `guides/kv_cache`、`guides/search`、
> `guides/tool_calls`、`guides/responses_api`、`guides/anthropic_api`、`guides/files_api`。
> 本机没有已登录的 DeepSeek 账号凭据，凡涉及"账号 token 在那个面上是否被接受"的结论
> 都标了**未验证**，验证入口见 §7。
> 施工用 spec（含逐条代码坐标与验收矩阵）：[`spec-deepseek-oauth-surfaces-v1.md`](spec-deepseek-oauth-surfaces-v1.md)。

---

## 0. 结论摘要

| 议题 | 结论 |
| --- | --- |
| 重启前后请求体 | 新增 chat / responses 两个测试：走真实 HTTP 捕获 gate 发出的字节，断言重启前后**逐字节一致**、追加后旧消息是新请求体的字节前缀、换工具清单必须改变请求体（灵敏度）。Responses 版另锁「历史中间的工具调用」形状。均已通过。 |
| 缓存命中回传 | DeepSeek **会**回传：chat 是 `usage.prompt_cache_hit_tokens/miss_tokens`，Responses 是 `usage.input_tokens_details.cached_tokens`，Anthropic 面是 `usage.cache_read_input_tokens`。三条 wire 本仓都已解析并落到 `SessionMeta.usage_totals`，无需额外协议字段。 |
| 网页搜索/抓取 | 官方是 `POST /v1/tools/search` 与 `POST /v1/tools/fetch`（原始 JSON、同 origin、同账号凭据）。客户端已实现并测好（`qaqh-dsaccount/src/web_tools.rs`）；**工具层接线**（`web_fetch` 走官方面）还需一次 per-call 注入改造，方案见 §3.3。 |
| 对话中间插入工具调用 | **chat completions 不支持**，只有 Responses 与 Anthropic `/messages` 支持。因此账号额度已放开 `responses` wire（`qaqh-config` + `gate.rs`），chat 保留兼容。 |
| wire 研判 | 账号额度**新配置默认走 Responses**（保留 chat）；Anthropic 面不采用——账号 token 是裸 `x-dsh-auth-token`，而 gate 的 `auth_header` 只被 openai / responses 两个 adapter 消费。 |
| Files API | 可用但**不是当前瓶颈**：只解决图片复用与 >32 MiB 图片，且需要把 `file_id` 当持久引用管理（I5 ContentRef 约束）。列为 P2，方案见 §5。 |

---

## 1. 严格测试：重启前后"请求体"逐字节一致

### 1.1 为什么不能只测上下文

`crates/qaqh-runtime/tests/restart_prefix_cache.rs` 已经把 `Vec<Message>` 的逐消息
序列化钉住了。但 provider 吃的是 gate 序列化后的 HTTP body，中间还有一层会改变形状的
投影：

- 第一条 system 提顶、后续 system 变成原位 developer item（`responses_sdk.rs:296-340`）；
- assistant 的 `tool_use` 被派生成 `function_call` item（`responses_sdk.rs:426-451`）；
- chat 侧 `name` 字段被丢弃、`tool_calls` 里的空 content 被省略（`chat_sdk.rs:7-13`、`:419-423`）；
- `tools` / `max_tokens` / `reasoning_effort` / `stream_options` 都在 body 里。

所以"上下文相同"不等于"缓存前缀相同"。新测试断言的是**线上字节**。

### 1.2 测试怎么搭

两个测试二进制（`SessionManager::init` 是进程级 OnceLock，chat / responses 必须分开进程）：

- `crates/qaqh-runtime/tests/restart_request_body.rs`（chat completions）；
- `crates/qaqh-runtime/tests/restart_request_body_responses.rs`（Responses）；
- 共用装置：`crates/qaqh-runtime/tests/common/model_wire_capture.rs`。

1. 本地 `tiny_http` 假 provider 收请求体、回固定 SSE（含正文帧，避免空流重试把请求数打乱）；
2. `capture_body()` 走**生产路径**：`AgentState::build_context()` + `agent.tool_defs`
   → `qaqh_gate::chat_stream()` → 投影 / SDK 归一 / 序列化 / 真发 HTTP；
3. 重启 = `drop(agent)` + `AgentState::init` + `init_session(Some(seed))`，
   即 `lifecycle.rs` 的 P0（`restore_frozen_annotation`）/ P2（`align_skill_injection_watermark`）恢复路径；
4. 断言：

| # | 断言 | 抓的是什么 |
| --- | --- | --- |
| 0 | body 里真有 >1000 字的 system、`[Environment]` 注解、tool result、非空 `tools` | 防止断言空转 |
| 1 | 同进程、同上下文连发两次 → 字节相同 | 隐藏随机源（HashMap 序、时间戳） |
| 2 | 重启后 → 字节相同 | system prompt / 注解 / 工具清单来自持久化面 |
| 3 | 追加一回合后：旧 `messages` 逐元素相同，且**序列化数组是字节前缀**；`tools` 不变 | provider 前缀缓存的 append-only 合同 |
| 4 | 再重启一次 → 与追加后的 body 字节相同 | 追加也持久 |
| 5 | 把 `tool_defs` 截到 3 个 → body 必须变 | 灵敏度：相等断言不是空转 |

> 口径说明：断言 2/4 只要求**同一套配置、同一 workspace** 下的重启。换 workspace 时
> `build_context()` 会主动让 `<workspace_path>` 注解失效（`agent.rs:552-561`），那是一次
> **有意**的缓存失效，不在本测试的承诺范围。

Responses 版额外断言（因为它是账号额度要迁过去的面）：

| # | 断言 | 抓的是什么 |
| --- | --- | --- |
| A | `function_call` 在 `function_call_output` 之前，且二者都在**后续用户消息之前** | 工具调用落在历史中间——chat completions 表达不了这个形状 |
| B | `store == false` 且没有 `previous_response_id` | 无状态请求的 wire 契约 |
| C | 重启 / 追加 / 再重启 / 缩小工具清单 → 与 chat 版同口径的字节断言 | Responses 也要有同样的缓存前缀稳定性 |

### 1.3 这条测试守住的回归面

- `[Environment]` 注解必须冻结并随 meta 落盘（`agent.rs:534-597`、`manager.rs:687-698`），
  重启不得重新生成；
- `tool_defs` 的顺序与内容跨恢复稳定（`lifecycle.rs:344-350`）；
- `build_context_for_gate` 的写入顺序（`store.rs:1006-1093`）不得引入随重启变化的排序；
- 任何在 body 里塞时间戳 / 随机 id / 迭代序的改动，都会在这里红。

---

## 2. 缓存命中回传：DeepSeek 要不要额外做什么？

### 2.1 官方口径

| wire | 缓存命中字段 | 缓存未命中 | 备注 |
| --- | --- | --- | --- |
| Chat Completions | `usage.prompt_cache_hit_tokens` | `usage.prompt_cache_miss_tokens` | 两个都直接给，`hit + miss = prompt_tokens` |
| Responses | `usage.input_tokens_details.cached_tokens` | 需要自己减（`input_tokens - cached_tokens`） | 文档 `guides/responses_api`「响应字段」 |
| Anthropic `/messages` | `usage.cache_read_input_tokens` | `input_tokens` 是未命中部分（Anthropic 口径） | 本仓 `messages_sdk.rs:379-400` |

缓存本身对账号与 API key **一致**：默认开启、按"缓存前缀单元"完整匹配、
构建耗时秒级、几小时到几天不用就清（`guides/kv_cache`）。**没有**需要客户端下发的
缓存键：Responses 明确 `prompt_cache_key` / `prompt_cache_retention` 不支持（静默忽略），
chat 也没有对应字段。所以"缓存命中"完全由**请求体前缀稳定性**决定——这正是 §1 的测试。

### 2.2 本仓现状（三条 wire 都已落地）

- chat：`usage_to_info_chat` 按端点声明的 `CacheTokenField` 取字段
  （`chat_sdk.rs:499-559`）。DeepSeek 的 OpenAI 兼容端点在迁移表里声明
  `cache_field = "PromptCacheHitTokens"` 且 `include_stream_usage = true`
  （`assets/legacy-providers.toml:20-21`）——后者保证流式最后一帧带 `usage`，
  否则缓存芯片不会出现。
- Responses：`usage_to_info_responses` 取 `input_tokens_details.cached_tokens`
  并补出 miss（`responses_sdk.rs:743-763`）。
- Anthropic：`cache_read_input_tokens` / `cache_creation_input_tokens`
  （`messages_sdk.rs:379-406`）。
- 归一化与累计：`usage::normalize`（`qaqh-gate/src/usage.rs`）→
  `SessionMeta::record_usage`（`qaqh-types/src/session.rs:172-208`）→
  `usage_totals` + `cache_reported_requests`；前端"缓存命中率"由这两个数算，
  没有第二套计数。

### 2.3 结论

DeepSeek 账号**不需要**为缓存命中额外回传任何东西；需要保证的是：

1. 该 profile 的 `cache_field` 是 `PromptCacheHitTokens`（旧配置迁移已给出）；
2. 流式 chat 要 `include_stream_usage=true`（旧迁移表已给出；BYOK 手配时由
   `EndpointCompat` 决定）；
3. 请求体前缀稳定（§1 的测试）。

一个已知的用户可配置陷阱：把 DeepSeek 的 compat 手动改成 `PromptDetailsCached`
会让命中数恒为 0（字段名对不上），这是配置错误而不是代码 bug。

---

## 3. 官方网页搜索 / 抓取

### 3.1 官方事实

| API | 端点 | 关键字段 |
| --- | --- | --- |
| Search | `POST https://api.deepseek.com/v1/tools/search` | `query`（必填，1～128 字符）、`count`（1～50，默认 10）、`ori_question`、`localization`、`need_content`（默认 false） |
| Fetch | `POST https://api.deepseek.com/v1/tools/fetch` | `url`（必填，≤8192，http/https + host，不得带用户名密码） |

- 响应：Search 是 `{results:[{rerank_score,title,url,snippet,summary,content,site_name,site_icon,date,authority}]}`；
  Fetch 是 `{title, content}`，`content` 是 **Markdown**。
- 错误用 OpenAI 兼容信封（`error.param` 恒 `null`）；408/429/5xx 可重试，
  402 先充值，其它 4xx 改了再试。
- 计费：当前经服务端工具调用访问这两个接口免费，自北京时间 2026-11-01 00:00 起按次收费。
- 另外一条不同的路径：Responses 的 **服务端** `web_search` 工具（仅 `deepseek-v4-pro`），
  模型自己触发、结果由服务端拼进上下文；回传上一次的 `web_search_call` item 可以还原
  搜索结果（`guides/responses_api`）。本仓 Responses 路径早已支持
  （`responses_sdk.rs:699-703` 下发 `ServerTool::WebSearch`；回放与去重在 `:452-469`）。

### 3.2 已实现：账号面的官方客户端

`crates/qaqh-dsaccount/src/web_tools.rs`（新）：

- `WebSearchRequest` / `WebSearchHit` / `FetchedPage`，`DsClient::search_web` /
  `DsClient::fetch_web`；
- 路径常量收口在 `protocol.rs:41-45`（`WEB_SEARCH_PATH` / `WEB_FETCH_PATH`）；
- 头与凭据复用 `DsClient::headers`：五个客户端身份头 + 裸 `x-dsh-auth-token`，
  **不发** `Authorization: Bearer`；
- 参数区间在**发请求之前**校验，失败是 `DsCode::Protocol`，不会打到线上；
- 响应解不出来只报形状错误，不回显正文（与 `HttpResponse::diagnostic` 同纪律）。

测试：`crates/qaqh-dsaccount/src/e2e_tests.rs::web_tools_use_the_official_paths_and_the_account_header`
（假平台断言路径、方法、凭据头、请求体只带显式字段），加 4 个纯函数单测。
`cargo test -p qaqh-dsaccount` 全绿（82 个）。

### 3.3 还没做：把 `web_fetch` 接到官方面（设计）

**为什么不能简单地"是 DeepSeek 就走官方"**：`qaqh-workspace` 不知道会话用哪个 profile；
`TOOL_MANAGER` 是进程级 `OnceLock`，而 profile 是**会话级**的（`SessionMeta.profile`）。
按 URL/模型名特判又违反 I15。正确形状是**把后端当参数显式传入**：

1. 在 `qaqh-tool-core` 定义端口（下层的 trait，不依赖任何 provider crate）：
   ```rust
   pub trait OfficialWebTools: Send + Sync {
       fn fetch(&self, url: &str, timeout: Duration) -> Result<OfficialPage, String>;
       fn search(&self, query: &str, count: u32) -> Result<Vec<OfficialHit>, String>;
   }
   ```
2. `ToolCallContext` 增一个字段 `web_tools: Option<Arc<dyn OfficialWebTools>>`
   （现 25 处结构体字面量需要补 `None`；`Debug` 用 newtype 手写）。
3. `RuntimeContext::tool_call_context`（`crates/qaqh-runtime/src/agent/context.rs:121-140`）
   从 `AgentState`/`Config` 取"该 profile 是否声明官方面"，需要时把
   `qaqh_dsaccount` 的实现塞进去（runtime 已经依赖 dsaccount，不新增边）。
4. `WebFetchTool::run`：`ctx.web_tools` 有值就调官方 `fetch_web`（拿 Markdown，省掉
   html2text），否则保持现自实现；`output` 落盘语义不变。
5. 开关必须**来自配置**（`EndpointCompat` 新字段，例如 `official_web_tools = true`），
   默认关；DeepSeek 账号 profile 才建议打开。

收益：官方 Markdown、站点策略/反爬由平台处理、与账号计费同一本账；
代价：一次跨 crate 的上下文扩字段 + 一次架构文档同步（P6）。

---

## 4. 对话中间插入工具调用

### 4.1 官方能力矩阵

| 能力 | Chat Completions | Responses | Anthropic `/messages` |
| --- | --- | --- | --- |
| 中间插入工具调用 | ❌ | ✅ `function_call` / `function_call_output` / `custom_tool_call`(+`_output`) | ✅ |
| 中间插入 `system` 消息 | ✅ | ✅（`developer` 视同 `user`） | ✅ |
| 思考内容回放 | 需 `reasoning_content` 原样回传 | ✅ `reasoning` item（`summary`/`encrypted_content` 不支持） | ✅ |
| 缓存命中字段 | `prompt_cache_hit_tokens` | `input_tokens_details.cached_tokens` | `cache_read_input_tokens` |
| 服务端 `web_search` | ❌ | ✅（仅 `deepseek-v4-pro`） | ❌ |
| `apply_patch` custom 工具 | ❌ | ✅（仅 `{"type":"custom","name":"apply_patch"}`） | ❌ |

文献：`guides/tool_calls`「在对话中间插入工具调用」与 `guides/responses_api` 的
「输入 Items / Tools / 响应字段」三张表。

### 4.2 本仓现状

- Responses 投影**已经**按历史顺序派生 `function_call`
  （`responses_sdk.rs:426-451`）与 tool result（`Part::ToolResult` → `function_call_output`），
  与位置无关；`web_search_call` 也会原样回放（`:452-469`）。
  也就是说：**切到 Responses，这个能力立即具备，不需要改投影**。
- chat 投影只能表达"assistant 带 tool_calls + 紧随其后的 tool 消息"
  （`chat_sdk.rs:125-260`），DeepSeek 的 chat 面明确不接受"非模型生成的工具调用
  被插到历史中间"。
- 当前 harness 里确实存在"结果晚到"的路径（`MessageStore::push_tool_result_inner`
  的放置逻辑、`[RESTORE]` 修复），一旦它把 tool 消息放到不紧邻的位置，chat 面就会 400。

### 4.3 已做的代码改动

- `qaqh-config/src/dto.rs:146-163`：`credential=deepseek_account` 的 wire 值域
  从 `{chat}` 扩到 `{chat, responses}`；`anthropic` 仍然拒绝，错误文案改为
  "必须配 chat 或 responses 协议"。
- `crates/qaqh-runtime/src/agent/turn_lap/gate.rs:951-1005`：`apply_deepseek_account`
  同样接受两个面，doc 注释写清"为什么 Responses 在值域里、为什么 Anthropic 不在"。
- 测试：`dto.rs` 的保存期双向拦截用例改成 anthropic 方向；`gate.rs` 新增
  `account_route_accepts_the_responses_wire_and_pins_the_bare_header`。

---

## 5. Files API 研判

官方能力（`guides/files_api`）：`POST /files`（multipart，`purpose=user_data` 必填，
单文件 ≤64 MiB，可选 `expires_after` 1h～30d）、列表 / 查询 / 删除；账号维度
25 GiB / 10000 文件；格式 JPEG/PNG/GIF/WebP。引用方式：

- chat：`{"type":"file","file_id":"file-api-…"}`（还有 `file_data` base64 变体）；
- Responses：`input_image.file_id`（与 `image_url` 互斥；`{"type":"file"}` 不支持）；
- Anthropic 面：必须带 `anthropic-beta: files-api-2025-04-14`，且文件对象形状不同
  （`size_bytes`/`type`/RFC3339 `created_at`）。

评估：

| 维度 | 判断 |
| --- | --- |
| 解决什么 | ① 同一张图片跨请求复用；② 绕过单图 32 MiB 内联上限（file_id 可到 64 MiB） |
| 我们的瓶颈在哪 | `read_image` 目前把图片以 base64 内联进 tool result（`ContentBlock::Image`），大截图/PDF 渲染图才会触上限 |
| 代价 | 需要把 `file_id` 当**持久引用**管理（I5：fact 里出现的引用必须能取回原文；而远端文件会过期/被删）——等于引入第二事实源 |
| 结论 | **P2，暂不单独适配**。若要做：先做"上传即引用"的短生命周期缓存（会话级、不落 fact），只在图片 >32 MiB 或同一图片第 2 次出现时上传；`expires_after` 设 1 天，过期即重传。 |

---

## 6. wire 研判：chat 还是 responses（或 messages）

| 维度 | Chat（现行默认） | Responses（建议的新默认） | Anthropic `/messages` |
| --- | --- | --- | --- |
| 账号 token 头 | `x-dsh-auth-token`，**已实测** | `x-dsh-auth-token`，**未验证** | 文档只认 `x-api-key`；gate 的 `auth_header` 不作用于 anthropic adapter（`types.rs:200-202`）→ 结构上不通 |
| 中间插工具调用 | ❌ | ✅ | ✅ |
| 缓存命中回传 | ✅ 双字段 | ✅ `cached_tokens` | ✅ |
| 有状态 | 无状态 | 无状态（`store:false`、`previous_response_id` 不支持） | 无状态 |
| 我们已有的适配度 | 高（`chat_sdk` 全量） | 高（`responses_sdk` 已含 DeepSeek 的 `effort_max=max`、`web_search`、`reasoning` 回放） | 中（`messages_sdk` 完整，但凭据面不通） |
| 额外收益 | — | 服务端 `web_search`（v4-pro）、`apply_patch` custom 工具 | — |
| 主要风险 | 长历史里出现"非法插入"会 400 | 账号 token 在该面的接受度未验证；`max_tokens` 语义换成 `max_output_tokens` | 凭据 + beta 头 + 文件形状三处差异 |

**决定**：

1. 账号额度的 wire 值域放开到 `{chat, responses}`，**新配置推荐 responses**；
2. 不迁移到 `messages`（凭据头结构性不通，且官方文档没有说账号 token 能当 `x-api-key`）；
3. chat 保留，用于回滚与"只想要最稳路径"的用户；
4. 迁移前先跑 `cargo run -p qaqh-dsaccount --example probe_surfaces`（§7），
   确认 ③ 的 200 与 `cached_tokens` 存在。若 ③ 401 而 ④（Bearer）也 401，
   说明该面不接受账号凭据，把默认改回 chat 即可（只是配置，不是代码回滚）。

---

## 7. 未验证项与验证入口

> 2026-10-19 更新：本机已登录 DeepSeek 账号，六个面已真机实测（`search`/`fetch` 200、
> `/responses` 账号头 200、Bearer 401、messages 401、`/files` 200）。结论以
> [`spec-deepseek-oauth-surfaces-v1.md`](spec-deepseek-oauth-surfaces-v1.md) §4 T0 为准；
> 下面保留的是当时的待验证清单。

本机没有 `deepseek-account.json`（`%USERPROFILE%\.qaqh\` 下未找到），以下都**未验证**：

- 账号 token 在 `POST /responses`、`/v1/tools/*`、`GET /files` 上是否被接受；
- 账号 token 能否当 Anthropic 面的 `x-api-key`；
- Responses 面流式下 `usage` 是否每轮都带 `input_tokens_details.cached_tokens`。

验证入口：`crates/qaqh-dsaccount/examples/probe_surfaces.rs`（新增，只读、脱敏、
不打印 token）。输出逐面给出 HTTP 状态与关键字段，不 dump 正文。

---

## 8. 后续任务（建议按此顺序）

1. **T-DS-1**：跑 `probe_surfaces`，按结果决定新配置默认 wire（responses / chat）。
2. **T-DS-2**：`ToolCallContext` 注入 `OfficialWebTools` 端口 + 配置开关
   （§3.3 的五步），让 `web_fetch` 在官方 profile 上走 `/v1/tools/fetch`；
   `web_search` 保持 Responses 服务端工具，chat profile 维持现状。
3. ~~**T-DS-3**：把 §1 的测试扩到 Responses wire~~ —— 已完成
   （`restart_request_body_responses.rs`）。剩余：reasoning 回放的专门断言（需要
   provider 原生 `ResponseOutputItem` 历史，当前测试只覆盖派生形态）。
4. **T-DS-4**：Files API（P2）——只在 >32 MiB 或重复图片时上传，`file_id` 做会话级短缓存。
