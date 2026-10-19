# plan-deepseek-oauth-surfaces-v1：spec 的落地计划

> 日期：2026-10-19　上游：[`spec-deepseek-oauth-surfaces-v1.md`](spec-deepseek-oauth-surfaces-v1.md)（下称 spec）、
> [`analysis-deepseek-oauth-surfaces-2026-10-19.md`](analysis-deepseek-oauth-surfaces-2026-10-19.md)（下称 analysis）。
> 纪律依据：`AGENTS-x.md` §1（I5 / I10 / I15）、§2 P3（行号漂移以符号为准）。
> 本文所有 `file:line` 是 **2026-10-19 实测快照**；与 spec 冲突处已在文中标注。
>
> **范围裁定（用户 2026-10-19）**：T6 Files API **不做**；T4 除 `web_fetch` 外还要把
> **`web_search`** 接到官方面（DeepSeek 侧优先，ChatGPT 侧随后）。
>
> **实施状态（2026-10-19）**：T0、T4a（DeepSeek 侧 `web_fetch` + `search`）已落地并通过
> 回归；T4b 仍未排期（SIWC 无搜索端点事实）。本文保留为设计记录，实际坐标以
> `spec-deepseek-oauth-surfaces-v1.md` §4 T4 的「落地坐标」表为准。

---

## 0. 前提纠正：spec 不是"零起步"

spec 头部写「T1/T2/T3/T5 已落地」，本次逐条核过代码与测试，**确实已落地**：

| 任务 | spec 标注 | 本次实测 | 证据 |
| --- | --- | --- | --- |
| T1 请求体逐字节稳定 | DONE | ✅ | `cargo test -p qaqh-runtime --test restart_request_body` → 4 passed；`--test restart_request_body_responses` → 4 passed |
| T2 缓存命中回传 | DONE | ✅ 三条 wire 均已解析 | `chat_sdk.rs:499`、`responses_sdk.rs:747`、`messages_sdk.rs:372`、`usage.rs:56`、`session.rs:172` |
| T3 官方 Search/Fetch 客户端 | DONE | ✅ | `qaqh-dsaccount/src/web_tools.rs:146/167`；`cargo test -p qaqh-dsaccount` → 82 passed |
| T5 中间插工具调用 + wire 迁移 | DONE | ✅ | `dto.rs:146-163`、`gate.rs:968-1004`、`responses_sdk.rs:372-523` |
| T0 探针 | 未跑 | ✅ **本次已跑，六面全绿** | 见 §1 |
| **T4 官方工具面接线** | PLANNED | ✅ 已落地（2026-10-19） | 端口 `tool_api/official_web.rs`；`ToolCallContext::official_web`；`web.rs` 的 `fetch_official`/`SearchTool`；`AgentState::refresh_tool_defs`；`EndpointCompat::official_web_tools` |
| T6 Files API | P2 | ⛔ **不做（决策）** | 见 §7 |

构建基线：`cargo check -p qaqh-tool-core -p qaqh-workspace -p qaqh-runtime --all-targets` 干净（exit 0）。

---

## 1. T0 结论（已执行：`cargo run -p qaqh-dsaccount --example probe_surfaces`）

凭据：本机已登录（脱敏 `Cuj7…`，64 字符）；模型 `deepseek-flash`（`probe_surfaces.rs:28`）。

| # | 面 | 结果 | 判定 |
| --- | --- | --- | --- |
| ① | `POST /v1/tools/search` | **200**，1 条结果，首条标题正常 | 账号裸头可用 ✅ |
| ② | `POST /v1/tools/fetch` | **200**，`Example Domain` / 931 字符 Markdown | 账号裸头可用 ✅ |
| ③ | `POST /responses`（`x-dsh-auth-token`） | **200**，`usage.input_tokens_details.cached_tokens` 字段在 | Responses 面可用 ✅ |
| ④ | `POST /responses`（`Bearer` 对照） | 401 `Your api key ... is invalid` | 证明 ③ 的成功来自裸头，不是别的原因 |
| ⑤ | `POST /anthropic/v1/messages`（`x-api-key`） | 401 | 结构性不通，**维持不迁移 messages** |
| ⑥ | `GET /files?limit=1` | 200，0 个对象 | 可用（但 T6 已决定不做） |

**由此定稿的配置决策（不再需要探针）**：

1. `assets/legacy-providers.toml` 里 deepseek 的 `openai`（`:10-36`）与 `responses`（`:38-67`）两个端点
   都置 `official_web_tools = true`。
2. 新配置默认 `wire = responses` 的结论不变（③ 已实测）。
3. Anthropic 面维持"不采用"（⑤ 401）。
4. spec §3 能力矩阵末行、§6「未验证」段已按本表更新。

---

## 2. 范围与阶段

```
P0 基线（已做）  →  P2 T4a：web_fetch + web_search（DeepSeek 侧，2~2.5d）  →  P4 收口（0.5d）
                              ↓ 端口 provider-agnostic，adapter 可加
                     T4b：ChatGPT（SIWC）侧（阻塞于"它到底提供什么搜索端点"）
```

- T6 已砍，从关键路径上消失（决策与备查见 §7）。
- T4b **现在无法排期**：`grep -ri search crates/qaqh-siwc/src` 无任何匹配，SIWC 侧目前
  没有任何搜索/抓取端点。要接必须先确认它提供什么（新探针 + 官方文档），
  在此之前只保证端口不把 DeepSeek 形态写死。

---

## 3. P0 基线（已完成，留作每阶段回归底座）

| 命令 | 期望 | 实测 |
| --- | --- | --- |
| `cargo check -p qaqh-tool-core -p qaqh-workspace -p qaqh-runtime --all-targets` | 干净 | ✅ exit 0 |
| `cargo test -p qaqh-runtime --test restart_request_body` | 4 passed | ✅ |
| `cargo test -p qaqh-runtime --test restart_request_body_responses` | 4 passed | ✅ |
| `cargo test -p qaqh-dsaccount` | 82 passed | ✅ |
| `cargo test -p qaqh-workspace --lib web` | 全绿 | 待跑（T4 的回归底座） |
| `cargo test -p qaqh-config` | 全绿 | 待跑（T4 改 registry） |

已知**基线红**（既有债，本计划不修，只要求"不新增"）：`cargo fmt --all -- --check`、
`cargo clippy --workspace --all-targets -- -D warnings`。每阶段只对本阶段触碰的文件跑
`rustfmt --check` 与 `clippy -p <crate>`。

---

## 4. T4a：官方工具面接线（`web_fetch` + `web_search`）

目标：声明了官方工具面的 profile 上，`web_fetch` 走 `POST /v1/tools/fetch`，
`search` 走 `POST /v1/tools/search`；其余 profile 与所有 legacy/ambient 调用保持现状。

### Step 1 端口（下层，`qaqh-tool-core`）

新文件 `crates/qaqh-tool-core/src/tool_api/official_web.rs`：

```rust
pub struct OfficialPage { pub title: String, pub content: String }
pub struct OfficialHit  { pub title: String, pub url: String, pub summary: String }

pub trait OfficialWebTools: Send + Sync {
    fn fetch(&self, url: &str, timeout: Duration) -> Result<OfficialPage, String>;
    fn search(&self, query: &str, count: Option<u32>, need_content: bool)
        -> Result<Vec<OfficialHit>, String>;
}

/// Arc 包装 + 手写 Debug（`dyn` 不能 derive；Debug 里不出现 URL 正文与凭据）。
pub struct OfficialWebBackend(Arc<dyn OfficialWebTools>);
```

- 端口**不依赖** `qaqh-dsaccount`（`tool-core/Cargo.toml` 只有 types/policy/skills）。
- 错误统一 `String`（上层映射成 `ToolError` 的 `code`/`kind`），不跨层透传 `DsError`。
- `OfficialHit` 有意**不**照搬官方 10 个字段：只留模型真会读的三项，其余（`site_icon`/
  `authority`/`rerank_score`）留在 dsaccount 侧，避免端口被单一 provider 的形状绑死。

### Step 2 `ToolCallContext` 扩字段

`tool-core/src/tool_api/context.rs:99-133` 增 `pub official_web: Option<OfficialWebBackend>`。

- `#[derive(Clone)]` 保留（`Arc` 可 Clone），`Debug` 由 newtype 手写。
- 全部结构体字面量补 `official_web: None`，共 **29 处**（编译器强制，不会静默漏）：
  - `qaqh-workspace`：`authorization.rs:314/542`、`execution.rs:49/771/1015/1068/1090/1112`、
    `manager.rs:1008`、`web.rs:259`；
  - `qaqh-tool-core`：`context.rs:157/182`、`dynamic.rs:104`、`typed.rs:263`；
  - `qaqh-runtime`：`agent/context.rs:122`、`tests/{cancel_keeps_tool_results:40,
    tool_ledger_finish_failure:96, tool_ledger_open_intent:80, tool_ledger_wiring:123,
    tool_ordering_contract:47, workspace_change_injection:160}`；
  - `qaqh-file-tools`：`file_glob.rs:265`、`file_mutate.rs:844/903`、`file_query.rs:537`、`grep_tool.rs:476`；
  - `qaqh-process-tools`：`exec/tests.rs:56/1272`；`qaqh-subagent/src/lib.rs:1831`。
- 不用 `..Default::default()`：显式 `None` 本身就是"这里没有官方后端"的审计点。

### Step 3 `web_fetch` 执行点（`qaqh-workspace/src/web.rs`）

- `WebFetchTool::run`（`:98-114`）分派：`ctx.official_web` 有值 → `fetch_official`；无值 → 现
  `fetch_content`（`:125-197`）**一行不改**。
- `:173-195` 的「落盘 + `journal::record_change` + `[saved to …]` 前缀」抽成
  `persist_output(ctx, output, text)`，两条路径共用（journal 语义只能有一份实现）。
- 官方 `content` 已是 Markdown → 跳过 `html2text`；仍需按 `MAX_WEB_BODY_BYTES` 同口径截断并标
  `truncated`，防止一次请求打爆上下文。
- 错误映射：`web_error(ToolErrorKind::Recoverable, "official_fetch_error", …)`，
  文案提示"可换 URL 或改用 BYOK 自实现"，不回显 token。

### Step 4 新增 `search` 工具（同一文件）

**命名必须是 canonical `search`，不是 `web_search`**：

- Responses 面的服务端内置工具类型就叫 `web_search`（`responses_sdk.rs:699-703` 注入
  `ServerTool::WebSearch`）。再加一个同名 function 工具会在 `tools` 数组里撞名。
- 仓里**已经**备好改名机制：`responses_sdk.rs:66-82` 的 `provider_function_name` /
  `canonical_function_name` 把 canonical `search` 映射成 `compat.responses_search_function_alias`，
  且 DeepSeek 的 responses 迁移行已写死 `qaqh_search`（`legacy-providers.toml:66`）。
  也就是说**只要工具叫 `search`，这条链立即生效**（授权/执行/历史仍用 canonical 名）。

实现要点：

- `SearchTool`（`TypedTool`）：`meta()` 用 `ToolCategory::Net` + `ToolRisk::ReadOnly`
  （与 `web_fetch` 同档，`web.rs:92`），超时默认 30s。
- 入参（`schemars`）：`query`（必填，1～128 字符）、`count`（可选 1～50）、
  `need_content`（可选，默认 false）。**不**暴露 `localization` / `ori_question`
  （前者是本机 IP，属于不该让模型决定的东西；后者当前没有可信来源）。
- `run`：`ctx.official_web` 为 `None` → `Recoverable` + `code = "official_search_unavailable"`
  + hint「当前 profile 未启用官方搜索」。这条分支正常**不会**被触发（Step 5 会把它从
  工具清单里摘掉），保留它只是防御。
- 输出投影：每条 `title` / `url` / `summary`（`need_content` 时附 `content` 截断段），
  逐条按 `clamp_display_body` 口径截断，块间空行分隔；`display()` 用 `ToolHeader::Query`。
- 注册：`web.rs:249 register()` 里加 `mgr.register_typed(SearchTool)`，并确认
  `agent_tool_registrars()` 已经覆盖 `web::register`（`agent.rs:432`）。

> `web.rs:1-7` 模块注释现在写着"web search 已经不是本地工具"，实现后要改写为
> "本地 `search` 在官方工具面 profile 上执行，其余 profile 不暴露"。

### Step 5 暴露过滤（按会话，按配置）

工具清单是**每条会话**从 `runtime::all_tools()`（`workspace/src/runtime.rs:389` →
`filtered_defs()`，`manager.rs:451`）重建的，所以可以按 profile 收窄：

- 新增 `AgentState::refresh_tool_defs(&mut self)`：
  `self.tool_defs = runtime::all_tools();` 然后
  `if !self.config.compat.official_web_tools { self.tool_defs.retain(|d| d.function.name != "search"); }`
- 把 **6 处**直接赋值换成它：`agent.rs:434`（`init`）、`agent.rs:466`（`apply_tool_mode`）、
  `lifecycle.rs:345`（恢复）、`engine_turn.rs:1301/1309`、`spawn.rs:124`。
- 顺序要求：`engine_session.rs:148` 把 `cfg.compat` 写进 `agent.config` 之后必须调用一次，
  否则首次请求的 `tools` 与后续不一致（T1 的逐字节断言会立刻红）。
- 语义：`official_web_tools = false` 的 profile **请求体与今天逐字节一致**（S2：不因升级
  被动失效缓存前缀）；`true` 的 profile 多一个 `search`，这是显式配置变更，允许。

### Step 6 注入点（`qaqh-runtime`）

- `RuntimeContext`（`agent/context.rs:49-57`）增 `official_web: Option<OfficialWebBackend>`；
  `from_legacy_ambient`（`:62-80`）增参数；唯一生产调用者 `TurnContext::from_legacy`（`:154`）
  按
  ```
  official_web_enabled(cfg.credential, &cfg.compat)
      = cfg.credential == CredentialSource::DeepseekAccount && cfg.compat.official_web_tools
  ```
  算一次（抽成纯函数，便于四象限单测）。
- `tool_call_context`（`:115-140`）透传进 `ToolCallContext`。
- 适配器 `crates/qaqh-runtime/src/agent/official_web.rs`：`DsOfficialWeb { client: DsClient }`，
  构造 `DsClient::new(manager.config().clone())`（`client.rs:93`、`manager.rs:175`）；
  **每次调用现取** `manager.token()`（`manager.rs:213`），不缓存 token（同 `gate.rs:998-1002`）；
  `fetch` → `DsClient::fetch_web`（`web_tools.rs:167`），`search` → `search_web`（`:146`）。
- **legacy/ambient 路径保持 `None`**（`authorization.rs:305-336`、`execution.rs:49-75`）：
  那里拿不到会话 profile，按 I10 不去读线程局部，落回自实现。注释写明这是**有意**的。

### Step 7 配置位（S3 / I15：开关只来自配置）

1. `qaqh-types/src/provider.rs:132-227` 增 `#[serde(default)] pub official_web_tools: bool`；
   `Default`（`:259-286`）补 `false`。旧配置无此字段 → false，向后兼容。
2. `qaqh-config/src/registry.rs`：`LegacyEndpoint`（`:100-149`）增 `Option<bool>`，
   `compat()`（`:158-215`）按 `flag()` 口径映射（与 `include_stream_usage` 同形）。
3. `assets/legacy-providers.toml`：deepseek `openai`（`:10-36`）与 `responses`（`:38-67`）
   各加 `official_web_tools = true`（依据 §1 的 ①②）。
4. `qaqh-config-api` 的 `validate` 不改（纯 compat 位，无跨字段约束）；
   `qaqh-config/src/config.rs:641-660` 的示例注释补一行。

### Step 8 测试

| 层 | 测什么 | 位置 |
| --- | --- | --- |
| `qaqh-workspace` 单测 | stub 端口：`web_fetch` 走官方、**不触网**、`output` 仍落盘且 `journal::record_change` 一条 | `web.rs` 的 `mod tests`（现 `:253`） |
| 同上 | `official_web: None` → `web_fetch` 走自实现分支；`search` → `official_search_unavailable` | 同上 |
| 同上 | `search` 结果投影：截断、URL/标题顺序、`need_content=false` 不带正文 | 同上 |
| `qaqh-gate` 单测 | `search` 在 Responses 面被改成 `qaqh_search`，回程反转成 canonical（现 `provider_function_name` 无覆盖用例） | `responses_sdk.rs` 测试段 |
| `qaqh-runtime` 单测 | `official_web_enabled` 四象限；`refresh_tool_defs` 在 `official_web_tools=false` 时摘掉 `search`、`true` 时保留 | `agent/official_web.rs`、`agent/state/agent.rs` |
| `qaqh-config` 单测 | 迁移表新字段映射；serde 缺省 = false | `registry.rs` 测试段 |
| 端到端 | T0 探针 ①②（已跑）；不新增联网测试 | — |

现有 `web.rs` 测试全 hermetic（`web.rs:276-320` 只测 `missing_url` 与输出投影），
T4 不需要联网夹具，**不要**引入。

### Step 9 回滚

配置置 `official_web_tools = false` → 立即回自实现，且 `search` 从清单消失；无代码回滚、无数据迁移。

**不变式自查**：S2（false 时请求体不变）、S3（开关来自 `EndpointCompat`）、S4（per-call 注入，
不进 `OnceLock`/TLS）、S5（复用 `DsClient::headers` 的裸 `x-dsh-auth-token`，`client.rs:113`）、
I15（无 URL/模型名特判）。

---

## 5. T4b：ChatGPT（SIWC）侧（未排期）

- 现状：`crates/qaqh-siwc/src` 里**没有任何** search/fetch/tools 端点（grep 零匹配）。
  「ChatGPT 这边也需要」目前缺少可对接的事实。
- 排期前置：先确认 SIWC 套餐路径到底有没有官方搜索能力（文档 + 探针），
  再决定是走官方端点还是复用 Responses 的服务端 `web_search`。
- 代码上已无阻碍：`OfficialWebTools` 端口与 provider 无关，届时只需再加一个
  adapter + 一个 `official_web_enabled` 分支（`CredentialSource::ChatgptPlan`）。
- 注意：Responses 的**服务端** `web_search`（`responses_web_search` 位，`provider.rs:189-191`）
  与本地 `search` 工具是两条不同路径，不要混为一谈；前者已可用。

---

## 6. P4：收口

1. spec：状态行、§3 能力矩阵、§4 T4 的改动清单（含新增的 `search`）、§6 验收矩阵
   「当前」列替换为本计划实测数。
2. `docs/ARCHITECTURE.md` §3（crate 职责）补一条「工具层如何按调用拿到 provider 后端」：
   `qaqh-tool-core` 只定义端口，`qaqh-runtime` 是唯一注入点；工具清单按 profile 收窄
   也写在这里（P6 同步）。
3. 行号漂移按符号更新（AGENTS-x P3）。
4. `rustfmt --check` / `clippy -p <本阶段触碰的 crate>` 不新增告警。

---

## 7. T6 Files API：不做（决策记录）

- 决策：**不做**（用户 2026-10-19）。探针 ⑥ 显示账号凭据在 `/files` 上可用（200），
  所以这不是"不可用"而是"不值得"：收益仅限"同一图片复用"与">32 MiB 单图"，
  代价是引入远端引用生命周期（I5）与一次 multipart 能力改造
  （`dsaccount/src/http.rs:104/113/139` 只有 `get/post_json/post_empty`）。
- 若将来重启此议题，约束不变：`file_id` 只做会话级短缓存（TTL 1 天、容量 64、不进
  `static`/`thread_local`），触发条件是单图 > 32 MiB 或同一 `sha256` 第二次出现，
  上传失败一律回退内联 base64。
- spec §4 T6 段落已标记为「不做」，保留坐标仅供将来参考。

---

## 8. 风险与回滚

| 风险 | 影响 | 处置 |
| --- | --- | --- |
| `search` 与 Responses 内置 `web_search` 撞名 | 请求被拒/行为不可预期 | 工具名用 canonical `search`，走已有 alias 机制（`responses_sdk.rs:66-82`） |
| `refresh_tool_defs` 漏改某处赋值点 | 首次与后续请求 `tools` 不一致 → 缓存前缀失效、T1 断言红 | 6 处清单见 Step 5；T1 测试是兜底 |
| `official_web_tools=true` 但未登录 | 每回合构建 provider 就报错 | 已有行为（`gate.rs:995-1001` 报"先登录"），不是新风险 |
| 官方 Fetch 返回超长正文 | 单轮 token 超限 | Step 3 的截断 + `truncated` 标记 |
| legacy 路径拿不到官方后端 | 权限复核/旧调用面走自实现 | 有意为之（I10），注释写明 |
| 官方端点计费（2026-11-01 起按次） | 用户成本 | `need_content` 默认 false、`count` 不主动放大；文案不提"免费" |
| T4b 无事实可依 | 排期空转 | 先探针/文档确认，不猜端点 |

## 9. 剩余待确认

1. **T4b 的前置事实**：SIWC 侧有没有官方搜索端点？（没有就只能等/或不做）
2. `OfficialHit` 只保留三项字段是否符合你对 `search` 输出的预期（要不要带 `date`/`site_name`）。

### 已决（2026-10-19）

- 开关的存量迁移选**方案 2**：加载时按「`credential = deepseek_account` 且文件里没写这个
  键」补 `true` 并写回一次（`qaqh-config/src/config.rs` `migrate_official_web_tools`）。
  不采用方案 1（在 provider 构造里强置)，因为那会覆盖用户显式 `false`。
