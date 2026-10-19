# spec-deepseek-oauth-surfaces-v1

> 状态：T0/T1/T2/T3/T4/T5 已落地并实测（T4 于 2026-10-19 完成）；T6（Files API）已决策不做。
> 落地计划：[`plan-deepseek-oauth-surfaces-v1.md`](plan-deepseek-oauth-surfaces-v1.md)。
> 日期：2026-10-19　关联：[`analysis-deepseek-oauth-surfaces-2026-10-19.md`](analysis-deepseek-oauth-surfaces-2026-10-19.md)
> 外部依据：`api-docs.deepseek.com/zh-cn/guides/{kv_cache,search,tool_calls,responses_api,anthropic_api,files_api}`。
> 施工纪律：本文所有 `file:line` 是**当前快照**的锚点；行号漂移时以符号为准（AGENTS-x P3）。

---

## 0. 术语

| 词 | 含义 |
| --- | --- |
| 请求体 | gate 序列化后真正发往 provider 的 HTTP body 字节（不是 `Vec<Message>`） |
| 缓存前缀 | provider 侧按请求体**前缀**匹配的 KV 缓存单元 |
| 账号额度 | `credential = deepseek_account`，本机 DeepSeek 授权 grant，走裸 `x-dsh-auth-token` |
| 官方面 | DeepSeek 官方 Search/Fetch 端点（`/v1/tools/*`）与 Files API（`/files`） |

---

## 1. 不变量（本 spec 的实现约束）

| 编号 | 约束 | 落点 |
| --- | --- | --- |
| S1 | 请求体跨重启必须**逐字节**可复现；上下文级等价不算数 | §4 T1 |
| S2 | 缓存前缀的变化只允许两来源：用户追加历史、或配置/工作区**显式**变更 | `agent.rs:552-561` 的工作区失效判定 |
| S3 | provider 差异只能来自配置（`EndpointSpec/EndpointCompat`），禁止按模型名/URL 特判 | `types.rs:194-254`、`provider.rs:133` |
| S4 | 会话级差异不得放进程全局；工具要按调用拿后端 → 走 `ToolCallContext` 显式注入 | `context.rs:99`、AGENTS I10 |
| S5 | 官方 Search/Fetch 复用账号凭据通道，不新开第二条凭据路径 | `client.rs:113` `headers()` |
| S6 | 账号 token 只发往绑定 origin；换端点 ≠ 转发凭据 | `gate.rs:981-988`、`client.rs:25` |
| S7 | 计划中的 `file_id` 引用必须能解析（I5），否则不落 fact | §4 T6 |

---

## 2. 一次模型请求的代码流水线（坐标）

```
配置/兼容               运行时上下文                       gate 序列化                         transport
─────────              ─────────────                     ───────────                         ────────
Config                 AgentState::build_context()        project_chat_messages()             EndpointAdapter
config.rs:91  ───────▶ agent.rs:539            ────────▶ chat_sdk.rs:94              ──▶ stream_with()
  :97 wire             │  ├ frozen annotation            │    build_options()  :399
  :99 compat           │  │   agent.rs:563-597            │      include_stream_usage :423
  :101 credential      │  ├ PrefixShape  :68              │    build_chat_request() :435
                       │  └ workspace 失效判定 :552-561   │      metadata :449 / max_tokens :453
EndpointCompat  :133   │                                  │
  responses_web_search │  MessageStore                    └ project_responses_messages() responses_sdk.rs:302
  :191                 └▶ build_context_for_gate store.rs:1041    派生 function_call :426-451
                         flat_in_write_order    :1006            build_request() :676 / server_tools :702
prompt.rs:38                                                    store=false :723
```

请求体组装**唯一**入口：`crates/qaqh-runtime/src/agent/turn_lap/gate.rs:418 gate_request`
（`:474` 注入 `RequestContext`，`:475` 调 `qaqh_gate::chat_stream`）。
wire 分派：`crates/qaqh-gate/src/lib.rs:42 chat_stream`。
provider 构造**唯一**入口：`crates/qaqh-runtime/src/agent/turn_lap/gate.rs:790 provider_for`
（`:813-830` Responses 的 compat 映射，`:820-829` 逐字段镜像）。

重启恢复路径（= daemon 重启）：

| 步骤 | 坐标 |
| --- | --- |
| resume 入口 | `crates/qaqh-runtime/src/agent/state/lifecycle.rs:154 init_session` |
| 注解回放（P0） | `lifecycle.rs:339` → `agent.rs:522 restore_frozen_annotation` |
| 工具清单热载（顺序稳定） | `lifecycle.rs:345` → `agent.rs:443 apply_tool_mode` |
| 注解落盘 | `manager.rs:687 persist_frozen_annotation` |
| system prompt（无日期，含 OS/壳/工具链） | `prompt.rs:38 full_system_prompt_with_env` |
| 新建会话 | `lifecycle.rs:430 create_session` |

---

## 3. 能力矩阵（事实，来自官方文档）

| 能力 | Chat | Responses | Anthropic `/messages` |
| --- | --- | --- | --- |
| 中间插入工具调用 | ❌ | ✅ | ✅ |
| 中间插入 system | ✅ | ✅ | ✅ |
| `reasoning` 回放 | `reasoning_content` | `reasoning` item | ✅ |
| 缓存命中字段 | `prompt_cache_hit_tokens` | `input_tokens_details.cached_tokens` | `cache_read_input_tokens` |
| 服务端 `web_search` | ❌ | ✅（仅 v4-pro） | ❌ |
| `apply_patch` custom | ❌ | ✅ | ❌ |
| 账号裸头可用性 | 已实测 | 已实测（T0 ③：200，`cached_tokens` 字段在） | 已实测 401（T0 ⑤，见 T5） |

---

## 4. 任务

### T0（验证前置）账号凭据各面可用性（DONE，2026-10-19 真机）

- 入口：`crates/qaqh-dsaccount/examples/probe_surfaces.rs:30`（只读、脱敏）。
- 覆盖：`/v1/tools/search`、`/v1/tools/fetch`、`/responses`（账号头 + Bearer 对照）、`/anthropic/v1/messages`（`x-api-key`）、`GET /files?limit=1`。
- 跑法：`cargo run -p qaqh-dsaccount --example probe_surfaces`（模型 `deepseek-flash`，`:28`）。

结果（全部实测，凭据脱敏）：

| # | 面 | 结果 | 判定 |
| --- | --- | --- | --- |
| ① | `POST /v1/tools/search` | 200，1 条结果 | 账号裸头可用 |
| ② | `POST /v1/tools/fetch` | 200，`Example Domain` / 931 字符 Markdown | 账号裸头可用 |
| ③ | `POST /responses`（`x-dsh-auth-token`） | 200，`input_tokens_details.cached_tokens` 字段在 | 可用 |
| ④ | `POST /responses`（`Bearer` 对照） | 401 | 证明 ③ 的成功来自裸头 |
| ⑤ | `POST /anthropic/v1/messages`（`x-api-key`） | 401 | 结构性不通，维持不迁移 |
| ⑥ | `GET /files?limit=1` | 200，0 个对象 | 可用（T6 仍决定不做） |

由此定稿：迁移表 deepseek 的 `openai`/`responses` 两端点均置 `official_web_tools = true`；
新配置默认 `responses` 不变；Anthropic 面不采用。

### T1 请求体跨重启逐字节稳定（DONE）

实现：

- chat：`crates/qaqh-runtime/tests/restart_request_body.rs:43`
- Responses：`crates/qaqh-runtime/tests/restart_request_body_responses.rs:46`
- 装置：`crates/qaqh-runtime/tests/common/model_wire_capture.rs`
  - `:24` CHAT_SSE、`:34` RESPONSES_SSE、`:50` `CaptureProvider`、`:166` `provider_for`、`:187` `capture_body`
- 既有上下文级测试：`crates/qaqh-runtime/tests/restart_prefix_cache.rs:107`（保留，不替代）

验收（全部已通过）：

| # | 断言 | 坐标 |
| --- | --- | --- |
| 0 | body 里真有 system / `[Environment]` / tool result / 非空 `tools` | `restart_request_body.rs:70-88` |
| 1 | 同上下文两次请求字节相同 | `restart_request_body.rs:98` |
| 2 | 重启后字节相同（+ `tools` 不变） | 同上 `:108`、`:114` |
| 3 | 追加后 `messages` 逐元素相同 + 序列化字节前缀 | 同上 `:139-158` |
| 4 | 再重启仍相同 | 同上 `:167` |
| 5 | 截断 `tool_defs` 必须改变 body | 同上 `:180` |
| R1 | `function_call` → `function_call_output` → 后续 user（历史中间） | `restart_request_body_responses.rs:86-93`（`:98` 工具非空） |
| R2 | `store=false` 且无 `previous_response_id` | 同上 `:73-74` |
| R3 | Responses 版的 2/3/4/5 同口径 | 同上 `:110-163` |

不变式：S1、S2。

### T2 缓存命中回传（DONE）

现状（三条 wire 均已解析并累计）：

| 关注点 | 坐标 |
| --- | --- |
| chat 字段选择（`CacheTokenField` 三态） | `crates/qaqh-gate/src/chat_sdk.rs:499`（`:512-538` 分支） |
| Responses `input_tokens_details.cached_tokens` | `crates/qaqh-gate/src/responses_sdk.rs:747` |
| Anthropic `cache_read_input_tokens` | `crates/qaqh-gate/src/messages_sdk.rs:372` |
| 统一归一化（`cache_usage_reported`、extras 白名单） | `crates/qaqh-gate/src/usage.rs:56` |
| wire 类型 | `crates/qaqh-types/src/api_types.rs:18 UsageInfo` |
| 会话累计（单一聚合点） | `crates/qaqh-types/src/session.rs:172 record_usage`、`:157 effective_cache_reported_requests`、`:131 usage_totals` |
| DeepSeek chat 端点声明 | `assets/legacy-providers.toml:20-21`（`PromptCacheHitTokens` + `include_stream_usage=true`） |
| 流式 usage 开关落点 | `crates/qaqh-gate/src/chat_sdk.rs:423` |

结论：DeepSeek 不需要额外回传字段；需要保证的是 `cache_field` 配对 + 流式要 usage + **请求体前缀稳定**（T1）。
新增回归建议：在 `crates/qaqh-gate/tests/gate_test.rs` 为 oauth profile 的 chat 流加一条「末帧 usage → `cache_reported_requests==1`」端到端断言（现有单测只覆盖解析函数）。

### T3 官方 Search / Fetch 客户端（DONE）

| 关注点 | 坐标 |
| --- | --- |
| 端点常量 | `crates/qaqh-dsaccount/src/protocol.rs:43` `/v1/tools/search`、`:45` `/v1/tools/fetch` |
| 推理 origin URL | `crates/qaqh-dsaccount/src/client.rs:108 inference_url` |
| 凭据/身份头（裸 `x-dsh-auth-token`） | `client.rs:113 headers` |
| 失败分类（401/40003 → 重新登录） | `client.rs:128 ensure_success` |
| 请求类型 + 区间校验 | `web_tools.rs:37 WebSearchRequest`、`:185 search_body`、`:219 validate_fetch_url` |
| 调用 | `web_tools.rs:146 search_web`、`:167 fetch_web` |
| 测试 | `crates/qaqh-dsaccount/src/e2e_tests.rs:755`；纯函数单测 `web_tools.rs:248/259/296/314` |

不变式：S5、S6。验收：`cargo test -p qaqh-dsaccount`（82 passed）。

### T4 官方工具面接线（`web_fetch` + `web_search`）（DONE，2026-10-19）

目标：在声明了官方工具面的 profile 上，`web_fetch` 调 `POST /v1/tools/fetch` 拿 Markdown、
新增的 `search` 工具调 `POST /v1/tools/search`；其余 profile 保持自实现且**不暴露** `search`。
T0 已实测两面对账号裸头均 200（§4 T0）。

命名约束：工具必须叫 canonical `search`，不能叫 `web_search` —— Responses 的服务端内置
工具类型已占用 `web_search`（`responses_sdk.rs:699-703`）；改名机制已存在
（`responses_sdk.rs:66-82` 的 `provider_function_name`，DeepSeek responses 行已配
`qaqh_search`，`assets/legacy-providers.toml:66`）。

改动点（按依赖方向从上到下）：

1. **端口（下层）**：`crates/qaqh-tool-core/src/tool_api/context.rs:99 ToolCallContext` 增字段
   `official_web: Option<Arc<dyn OfficialWebTools>>`；newtype 手写 `Debug`，trait 定义放同目录新文件。
2. **执行点**：`crates/qaqh-workspace/src/web.rs:98 WebFetchTool::run` 先看 `ctx.official_web`：
   有值 → 官方 fetch（返回 `{title, content}`，`content` 已是 Markdown，跳过 `html2text`）；
   无值 → 现路径 `:125 fetch_content`。`output` 落盘/`journal::record_change` 语义不变（`:173-195`）。
3. **注入点（上层）**：`crates/qaqh-runtime/src/agent/context.rs:115 tool_call_context` 从
   `RuntimeContext`（`:49`）取后端；`RuntimeContext` 新增字段并由 `from_legacy_ambient`（`:62`）从
   `AgentState`/`Config` 取值。适配器实现放 `qaqh-runtime`（已依赖 dsaccount：`crates/qaqh-runtime/Cargo.toml:36`），
   每次调用现取 `qaqh_dsaccount::manager().token()`，不缓存 token。
4. **开关（配置）**：`crates/qaqh-types/src/provider.rs:133 EndpointCompat` 增 `official_web_tools: bool`
   （默认 false，缺省处 `:276` 附近同批更新）；`crates/qaqh-config/src/registry.rs:134/197` 映射迁移表；
   `assets/legacy-providers.toml:31/61` 对 deepseek 的 openai/responses 端点置 true。
5. **测试**：
   - `crates/qaqh-workspace/src/web.rs` 单测：有后端走官方、无后端走自实现、`output` 仍落盘；
   - `crates/qaqh-runtime` 集成测试：注入后 `chat` 不触发网络（后端是 stub）；
   - 探针：T0 的 ①②。

6. **`search` 工具（同一文件 `web.rs`）**：`ToolCategory::Net` + `ToolRisk::ReadOnly`；
   入参 `query` / `count` / `need_content`；`ctx.official_web == None` 时返回
   `official_search_unavailable`（防御分支，正常不会触发）。
7. **暴露过滤**：`AgentState::refresh_tool_defs()` = `runtime::all_tools()`，
   且 `!compat.official_web_tools` 时摘掉 `search`；替换 6 处赋值点
   （`agent.rs:434/466`、`lifecycle.rs:345`、`engine_turn.rs:1301/1309`、`spawn.rs:124`），
   保证 `false` 的 profile 请求体与今天逐字节一致（S2）。

不变式：S2、S3（开关来自配置）、S4（per-call 注入，不进全局）、S5。回滚：配置置 false 即回自实现。

落地坐标（实际）：

| 关注点 | 坐标 |
| --- | --- |
| 端口 | `qaqh-tool-core/src/tool_api/official_web.rs`（`OfficialWebTools` / `OfficialPage` / `OfficialHit` / `OfficialWebBackend`） |
| 上下文字段 | `tool_api/context.rs` `ToolCallContext::official_web`（另 26 处字面量补 `None`；两处 `..` 更新语法无需改） |
| 抓取分派 | `qaqh-workspace/src/web.rs` `fetch_official` / `persist_output`（与自实现共用落盘 + journal） |
| 搜索工具 | 同文件 `SearchTool` + `render_hits`；canonical 名 `search`，`web::register` 注册 |
| 暴露过滤 | `qaqh-runtime/src/agent/state/agent.rs` `refresh_tool_defs`（6 处赋值点改走它） |
| 注入 | `qaqh-runtime/src/agent/official_web.rs`（`official_web_enabled` / `backend_for`）+ `agent/context.rs` `from_legacy_ambient` + `engine_tool.rs` UI 直调 |
| 开关 | `qaqh-types/src/provider.rs` `EndpointCompat::official_web_tools`（默认 false）；`qaqh-config/src/registry.rs`；`assets/legacy-providers.toml` deepseek 两端点 `true` |
| 工具表 | `qaqh-tool-core/src/tool_capabilities.rs` `("search", READ_ONLY_UNBOUND)`；`qaqh-workspace/src/registration.rs` 正式词汇表 |

**开关的存量迁移**（`qaqh-config/src/config.rs` `migrate_official_web_tools`）：加载时若某
profile 的 `credential = deepseek_account` 且文件里**没有**写 `official_web_tools`，则补
`true` 并写回一次；显式 `false` 原样保留。判据必须读**原始 TOML**——`EndpointCompat` 的
`#[serde(default)]` 让"键缺失"与"显式 false"在类型层不可区分，而迁移不能覆盖用户的
显式选择。判据是凭据来源而不是 base_url（后者会变成第二真相源，I15）。

验收（2026-10-19 实测）：`cargo test -p qaqh-workspace --lib` 154 passed、
`cargo test -p qaqh-tool-core` 69 passed、`cargo test -p qaqh-config` 全绿、
`-- refresh_tool_defs official_web` 3 passed、T1 两条字节级测试仍 4/4、
`cargo check --workspace --all-targets` 干净。

### T5 历史中间工具调用 + wire 迁移（DONE 部分）

已改：

| 改动 | 坐标 |
| --- | --- |
| 保存期值域 `{chat}` → `{chat, responses}` | `crates/qaqh-config/src/dto.rs:146-163`（`apply_patch` 在 `:108`） |
| 运行期同口径 + 文档化理由 | `crates/qaqh-runtime/src/agent/turn_lap/gate.rs:968 apply_deepseek_account`（`:951-967` 注释） |
| Responses 投影已支持中间插入 | `crates/qaqh-gate/src/responses_sdk.rs:372-475`（assistant 派生 `function_call`）、`:476-523`（tool → `function_call_output`） |
| Anthropic 面不通的结构原因 | `crates/qaqh-gate/src/types.rs:200-203`（`auth_header` 只被 openai/responses 消费） |
| 迁移表里 DeepSeek 的 Responses 兼容位 | `assets/legacy-providers.toml:38-67` |

验收：`restart_request_body_responses.rs:86-98`（R1）+ dto/gate 单测。

结论：账号额度**新配置默认 responses**，chat 保留兼容，`messages` 不采用。

### T6 Files API（不做，2026-10-19 决策）

**决策：不做。** T0 ⑥ 显示账号凭据在 `/files` 上可用（200），所以这不是「不可用」而是
「不值得」：收益只有同图复用与 >32 MiB 单图，代价是引入远端引用生命周期（I5）与一次
multipart 能力改造（`qaqh-dsaccount/src/http.rs:104/113/139` 只有 `get`/`post_json`/`post_empty`）。
下列坐标保留，仅供将来重启议题时参考。

现状坐标（将来要动的面）：

| 关注点 | 坐标 |
| --- | --- |
| 内容块类型 | `crates/qaqh-types/src/message.rs:48 ContentBlock`、`:98 ImageRef` |
| 图片 blob 取回 | `crates/qaqh-types/src/image_store.rs:41 load_image_b64` |
| chat 图片投影 | `crates/qaqh-gate/src/chat_sdk.rs:153/243`、`:310 MEDIA_ATTACHMENT_TEXT` |
| Responses 图片投影 | `crates/qaqh-gate/src/responses_sdk.rs:530-537 response_media_parts` |

设计约束：`file_id` 只能作为**会话级短缓存**（不落 fact），否则违反 I5（远端文件会过期/被删）。
触发条件：单图 >32 MiB，或同一图片第二次出现。`expires_after` 建议 1 天。

---

## 5. 代码地图（一页速查）

| 关注点 | 坐标 | 说明 |
| --- | --- | --- |
| 会话配置 | `crates/qaqh-config/src/config.rs:91`（`:97 wire`、`:99 compat`、`:101 credential`） | 单一配置真相源 |
| wire 枚举 / 缓存字段 / 兼容位 / 凭据来源 | `crates/qaqh-types/src/provider.rs:29/97/133/243` | provider 差异只在配置 |
| 配置补丁校验 | `crates/qaqh-config-api/src/lib.rs:200 ConfigPatch`、`:290 validate` | 保存期拦截 |
| 兼容位迁移表 | `crates/qaqh-config/src/registry.rs:27 legacy_preset`、`:134/197` | 旧 `(provider_id, endpoint)` 坐标 |
| 会话用量累计 | `crates/qaqh-types/src/session.rs:131/157/172` | 缓存命中率的唯一计数点 |
| 上下文构建 | `crates/qaqh-runtime/src/agent/state/agent.rs:539`、`store.rs:1041` | 注解冻结 + 写序合并 |
| 前缀自检 | `agent.rs:68 PrefixShape`、`:912/918` 工作区标签 | 变更原因诊断 |
| provider 构造 | `turn_lap/gate.rs:790` | 唯一入口 |
| gate 请求/事件聚合 | `turn_lap/gate.rs:418` | 流式与 usage 归一 |
| 请求序列化 | `chat_sdk.rs:435`、`responses_sdk.rs:676` | 两套 wire 的 body |
| 传输/重试/头 | `crates/qaqh-gate/src/sdk_common.rs:529 apply_management_headers` | 管理头与 UA |
| 官方搜索/抓取 | `qaqh-dsaccount/src/web_tools.rs:146/167` | 账号面客户端 |
| 工具层 fetch（现状自实现） | `qaqh-workspace/src/web.rs:82/125` | T4 要接的调用点 |
| 工具类别/副作用 | `permission/permission.rs:1117/1139`、`tool_runtime.rs:1167` | `web_fetch` 属 Net |
| 测试装置 | `tests/common/model_wire_capture.rs:50/166/187` | 真 HTTP 捕获 |

---

## 6. 验收矩阵

| 命令 | 期望 | 当前 |
| --- | --- | --- |
| `cargo test -p qaqh-runtime --test restart_request_body` | 4 passed | ✅ |
| `cargo test -p qaqh-runtime --test restart_request_body_responses` | 4 passed | ✅ |
| `cargo test -p qaqh-runtime --test restart_prefix_cache` | 1 passed | ✅ |
| `cargo test -p qaqh-runtime --lib` | 288 passed | ✅ |
| `cargo test -p qaqh-dsaccount` | 82 passed | ✅ |
| `cargo test -p qaqh-config` | 全绿 | ✅ |
| `cargo test -p qaqh-workspace --lib` | 154 passed | ✅ |
| `cargo test -p qaqh-tool-core` | 69 passed | ✅ |
| `cargo check --workspace --all-targets` | 干净 | ✅ |
| `cargo check --workspace` | 干净 | ✅ |
| `cargo fmt --all -- --check` | 全绿 | ❌ 基线红（`qaqh-client/examples/workspace_audit_e2e.rs`、`qaqh-config/src/secrets.rs`、`qaqh-config/tests/permission_level_fail_closed.rs`） |
| `cargo clippy --workspace --all-targets -- -D warnings` | 全绿 | ❌ 基线红（`qaqh-memwatch/src/lib.rs:247/314`、`qaqh-types/src/config.rs:387`、`qaqh-config/src/config.rs:820`、`qaqh-runtime` 6 处、`qaqh-dsaccount/examples/probe_models.rs:7`） |

本 spec 触碰的文件单独 `rustfmt --check` 干净（T4 改动亦然）；`qaqh-dsaccount` 的 lib+tests clippy 干净。
**更正**：`cargo fmt --all -- --check` 的基线红远大于此前记录（实测约 100 个文件，
含 `sbx-win`、`qaqh-gate`、`qaqh-siwc`、`qaqh-daemon` 等），所以本 spec 只承诺
「触碰的文件干净」，不承诺全仓。
**T0 已真机实测**（2026-10-19，模型 `deepseek-flash`）：`/v1/tools/search` 200、
`/v1/tools/fetch` 200、`/responses`（账号裸头）200 且 `cached_tokens` 字段在、
`/responses`（Bearer）401、`/anthropic/v1/messages`（`x-api-key`）401、`GET /files` 200。
明细见 §4 T0。

---

## 7. 风险与回滚

| 风险 | 影响 | 回滚 |
| --- | --- | --- |
| 账号 token 在 `/responses` 不被接受 | 迁过去的 profile 首发 401 | 配置改回 `wire=openai`（代码无需回滚） |
| 官方 Search/Fetch 不认账号 token **（已排除：T0 ①② 均 200）** | — | 保留开关：配置 `official_web_tools=false` 即回自实现 |
| `ToolCallContext` 扩字段遗漏构造点 | 编译失败（显式字段，不会静默） | 编译器强制，无运行时风险 |
| 请求体新增不可复现字段 | 缓存命中率下降 | T1 测试立刻红 |
| Files API 的 `file_id` 过期 | 历史图片不可读 | 不适用（T6 不做）；若重启议题则只做会话级短缓存、不落 fact |
