# 研判：Sign in with ChatGPT 接入（后端 + Tauri 前端标识）

> 日期：2026-10-14。状态：**研判稿**（未实施）。
> 依据（全部为公开资料，核对于 2026-10-14）：
> - DevKit 仓库 `https://github.com/openai/sign-in-with-chatgpt-devkit`（`@siwc/local` / `@siwc/react` / Paste Perfect 示例）
> - Cookbook：`https://developers.openai.com/cookbook/articles/sign-in-with-chatgpt`（追加 `.md` 取纯文）
> - 开发者文档：`/siwc/quickstart`、`/siwc/website`、`/siwc/ui-ux-guidelines`、
>   `/siwc/token-sharing-open-source`(+`/sign-in`、`/token-reference`、`/profiles-and-sessions`、
>   `/models-and-inference`、`/errors-and-recovery`、`/preview-limitations`)
> - 本仓库源码实证位置逐条标注为 `file:line`。
>
> **本文件不复制任何 OpenAI 代码或设计资产**：DevKit 代码/资产是
> *Sign-in with ChatGPT DevKit Noncommercial License v1.0*（明禁商用，且不含商标授权），
> 本仓库是 MIT（`Cargo.toml` `workspace.package.license = "MIT"`）。协议事实按公开文档自实现。

---

## 0. 结论速览

1. **只有一条现在能走的路**：面向「开源 / 用户本机运行」应用的 **ChatGPT plan usage 动态注册流**
   （`client_id=dynamic_agent_client` + `agent_name_hint` + `ext_agent_host_id`，回环 `127.0.0.1` 回调，
   PKCE，**无 client secret**，无需合作方审核）。拿到 OAuth access token 后**只**打
   `POST https://api.openai.com/v1/responses`（`store:false` + `stream:true`），用量计入用户自己的 ChatGPT 套餐。
2. **identity-only（`/siwc/website` 那条）当前不对我们开放**：需要 OpenAI 下发 `oaiapp_…` client ID +
   逐环境登记回调，且文档明写「limited trial / 联系 OpenAI 代表」。它解决的是「用 ChatGPT 账号登录
   *你的网站*」，与 qaqh 的场景（本地 harness 借用用户套餐跑推理）不是同一件事。**M0 不依赖它**，
   但账户抽象要预留，将来若开放可平移（见 §4 G）。
3. **定位必须摆正：SIWC 不是「另一种登录 qaqh」，而是「另一种给 qaqh 的推理付费与身份展示口径」。**
   - daemon 的 Identity 域（`admin_token` / `device_token` + `Scope`）**不动**：那是「谁在跟我说话」。
   - SIWC 落在 **Provider/Profile 域**：那是「我用谁的额度、以谁的名义调上游」。
   把两者混起来（例如「用 ChatGPT 登录 daemon 拿 device_token」）会同时破坏本地信任模型与 OAuth 语义。
4. **前端 Tauri 要的「标准 ChatGPT signin 标识」= 后端能提供一套可判定的连接状态 DTO**：
   `connected / sharing / identity{name,email} / label`，加上模型目录与一个固定深链
   `https://chatgpt.com/settings/usage`。FE **永远不接触 token**（现有 shell→daemon 的
   `invoke(...)` 通道天然满足，见 `E:\qaqh-desktop-app\src\lib\transport\tauri.ts`）。
5. **最大工程量不在 OAuth，而在 Responses 变体的兼容收口**：plan usage 这条路禁用了一批字段、
   禁用了 hosted 工具、禁用了会话持久化（`store:false`，不能 `previous_response_id`），
   并把一批错误码提升为「产品状态」（限流/不适用/路由不支持）。详见 §4 D、§6。

---

## 1. 两条能力的可用性对照

| 维度 | identity-only（`/siwc/website`） | **ChatGPT plan usage（OSS/本地 app）** |
|---|---|---|
| 面向 | 商业合作方站点，用 ChatGPT 账号登录你的产品 | 开源 / 用户自己机器上跑的应用，借用用户套餐做推理 |
| 当前可用性 | **limited trial**，需 OpenAI 代表 / 表单 | **对所有开源伙伴开放**（文档口径），无需 secret、无需 partner key |
| client | OpenAI 下发 `oaiapp_…`，逐环境登记回调 URL，可 confidential(+secret) | 首次用 `client_id=dynamic_agent_client` 动态注册，回调里**下发** `client_id`（形如 `oaiapp_…`），此后复用；公开客户端，无 secret |
| 回调 | 你的 HTTPS 域名 | **必须** `http://127.0.0.1:<port>/auth/callback`（禁 `localhost`，path 固定） |
| scope | `openid profile email` | `openid profile email offline_access resource.invoke chatgpt.tokens.use.direct` |
| 产物 | 只认 `id_token`（文档明确：不要依赖 access/refresh token） | `access_token` + `refresh_token` + `id_token`，打 `https://api.openai.com/v1` |
| 后端职责 | 换码→验 ID token→关联本地账户→**自建会话** | 换码→验 ID token→验 scopes→**本地加密存凭据**→刷新/撤销→代理推理 |
| 对 qaqh | 需要公网回调与事务存储，daemon 现在是本地回环（`axum_server.rs`） | 与 daemon 形态天然契合；**选它** |

---

## 2. 协议事实（实现清单，逐条来自官方文档）

### 2.1 discovery 与端点

- discovery：`https://auth.openai.com/.well-known/openid-configuration`；取 `issuer`、
  `authorization_endpoint`、`token_endpoint`、`jwks_uri`、`revocation_endpoint`。
  **必须校验 `data.issuer === "https://auth.openai.com"` 且各 endpoint 的 origin 同为该 issuer**
  （DevKit `packages/local/src/oauth.ts` 就是这么做的；它是防「discovery 被污染的 SSRF/降级」的硬检查）。
- 生产值：authorize `…/api/accounts/authorize`、token `…/api/accounts/oauth/token`、
  JWKS `…/.well-known/jwks.json`。

### 2.2 authorize（浏览器跳转）

| 参数 | 取值 / 说明 |
|---|---|
| `client_id` | 首次注册 `dynamic_agent_client`；**回访必须用已下发的 `client_id`**（绝不落盘 `dynamic_agent_client`） |
| `agent_name_hint` | 应用真名（如 `QAQ-Harness`），**仅首次注册带**；展示元数据，用户可改 |
| `ext_agent_host_id` | 本 host 稳定持久标识（见 2.6），**必带** |
| `id_token_hint` | 回访可用（上次成功的 `id_token`，可过期，仅用于定位账户 + 跳过账户选择器） |
| `login_hint` | 回访可用（保存的邮箱），预选账户 |
| `response_type` | `code` |
| `redirect_uri` | `http://127.0.0.1:<port>/auth/callback`；**只允许端口变**；同一次尝试内 authorize 与 token 交换必须逐字一致；`localhost` 不允许 |
| `scope` | `openid profile email offline_access resource.invoke chatgpt.tokens.use.direct` |
| `resource` | `https://api.openai.com/v1` |
| `state` / `nonce` | 每次全新随机；state 回程常数时间比对且**一次性消费** |
| `code_challenge_method` / `code_challenge` | `S256` / base64url(SHA-256(verifier))，无 padding |
| `prompt` | 仅在「用户主动去设置里开启套餐用量」时用 `prompt=consent`；`force_reconsent=true` 待 OpenAI 确认后再用；**普通登录不得强制同意** |

### 2.3 callback

`?code=…&state=…&scope=…&client_id=<ISSUED_CLIENT_ID>`（**新注册**才带 `client_id`；回访可能不带）。
- 先验 `state`，再处理 `error`（`error=access_denied` = 用户拒授权 → 不得换码，保留其他付款路径）。
- 回调给的 `client_id` 与本次尝试已存的 `client_id` 不一致 → **拒绝**，不得覆盖既有注册。
- 新注册拿不到 `client_id` → 视为注册未完成。

### 2.4 token 交换 / 刷新 / 撤销

- 换码：`POST …/api/accounts/oauth/token`，`application/x-www-form-urlencoded`：
  `grant_type=authorization_code`、`client_id`(下发的)、`code`、`code_verifier`、`redirect_uri`、`resource`。无 secret。
- 响应字段：`access_token`、`refresh_token`、`id_token`、`token_type`、`expires_in`、`scope`、`earliest_refresh_at`。
- 寿命：access **1h**；refresh **30 天**，每次刷新**轮换**并重置 30 天，无固定次数上限。
- 刷新：`grant_type=refresh_token` + 该 profile 的 `client_id` + `refresh_token` + `resource`；**省略 `scope` 以保留原授权**。
  **必须串行化**同一会话的刷新（旋转式 token，两处并发刷新会互相作废），且**先把待用凭据落盘再使用**
  （DevKit 的 `pendingRefresh` checkpoint 语义：崩溃/取消不得丢掉唯一的后继 refresh token）。
- 撤销：`POST` discovery 的 `revocation_endpoint`，form：`token=<refresh_token>`、`token_type_hint=refresh_token`、`client_id`；
  空 200 即成功（含已失效 token）。登出流程：先尝试撤销，网络失败则本地清除并**明确告知未确认远端撤销**。

### 2.5 ID token 校验（安全关键）

- RS256 + JWKS（按 `kid` 解析，未知名刷新 JWKS；JWKS 拉取失败 = **可用性故障，不等于身份无效** ——
  DevKit 专门把这类错误分类为 `identity_verification_unavailable` 并保留连接，值得照搬）。
- 必验：`iss` == `https://auth.openai.com`；`aud` == 本次的 `client_id`；`exp/iat`（允许小 skew）；
  `nonce` == 本次尝试；多 `aud` 或存在 `azp` 时必须 `azp == client_id`。
- **`sub` 是账户身份**；`email` 只作展示。`sub`/`email` **都不是 workspace 标识**，
  同一邮箱的两次注册必须保持独立（注册身份 = `sub` + 下发的 `client_id`）。
- 回访：新 `id_token` 的 `sub` 必须与选中 profile 一致，否则 `account_mismatch`，不得覆盖凭据。

### 2.6 host ID（`ext_agent_host_id`）

- 每个「运行实例」（本机安装 / 自托管 VM）一个，**首次登录前生成并持久化**，跨重启/换账户复用。
- 格式（按推荐序）：`urn:ietf:params:oauth:jwk-thumbprint:…`（RFC 9278，从公钥派生，**当前不校验私钥持有**）
  / `urn:uuid:<uuidv4>`（支持）/ `did:key:…`。不透明，**不是凭据**，禁含邮箱/用户 ID。
- 语义边界：**一个 client（用户授权的注册）可对应同用户同 workspace 的多个 host**；
  设置/限额跟着 client 走，host 只是归属维度。

### 2.7 推理面（`https://api.openai.com/v1`）

- 模型目录：`GET /v1/models`（同一 access token），取 `models[]` 中 `visibility == "list"` 的
  `{slug, display_name}`，**保持服务端顺序**；切账户后必须重取。
- 推理：`POST /v1/responses`，`Authorization: Bearer <access_token>`，**`store:false` + `stream:true` 每个请求都要带**，
  `input` 传每次请求所需的完整上下文；system 只能走顶层 `instructions` 或 developer message
  （显式 `{type:"message",role:"system"}` item **会被拒**）。
- **禁用字段**：`background`、`conversation`、`max_output_tokens`、`max_tool_calls`、`metadata`、`moderation`、
  `multi_agent`、`prompt`、`prompt_cache_retention`、`safety_identifier`、`temperature`、`top_logprobs`、`top_p`、
  `truncation`、`user`；HTTP 下 `previous_response_id` 也必须省略（无会话持久化）。
- **工具**：客户端 function/custom 工具需放进 **namespaces** 或经 `additional_tools` input item 提供；
  **hosted** 工具不可用：image generation、file search、Code Interpreter、native computer use、hosted MCP/connectors、
  Responses `tool_search`（客户端执行不代表支持）；`programmatic_tool_calling` 不能出现在顶层 `tools`。
  web search 保留但受账户/workspace 策略约束。
- **输入**：文本/图片/文件（模型支持时）；音频视频、Files 上传 API、转录 API 不支持。
- **完成判定**：只有读到 `response.completed` 才算成功；`response.failed`（限流两类 code 可能在流开始后才到）、
  `response.incomplete`、流中断都要单独处理。
- **错误码 → 产品状态**（`error.code`，不是 HTTP 状态）：

| code | HTTP | 处理 |
|---|---|---|
| `subscription_sharing_user_not_eligible` | 403 | 该用户/workspace/策略不可用；**不要重试、不要反复走 OAuth** |
| `subscription_sharing_usage_limit_exceeded` | 429 | 暂停 plan 请求，引导 `https://chatgpt.com/settings/usage`；不要推断额度空或重置时间 |
| `subscription_sharing_usage_unavailable` | 503 | 保凭据，退避重试 |
| `subscription_sharing_unsupported_capability` | 400 | 看 `error.param`，**移除我们发错的字段**（视为我方 bug，不重试同体） |
| `subscription_sharing_route_not_supported` | 403 | 路由/方法不对 |
| `subscription_sharing_invalid_user` | 401 | 订阅上下文校验失败；留 request id，必要时重登 |
| `chatpass_v2_scope_not_authorized` / `chatpass_v2_invalid_authorization_context` | 403 | 授权上下文不成立，查 client/授权配置 |
| `subscription_sharing_user_unavailable` | 503 | 用户/workspace 信息暂不可用，退避 |

  刷新错误分类：`invalid_grant` / `invalid_refresh_token` / `token_expired` / `refresh_token_expired` /
  `refresh_token_invalidated` / `refresh_token_reused` → 清不可用 token 后用**已下发的 client_id** 重走 OAuth；
  `invalid_client` → 配置错。**网络/5xx 不得清凭据**。OpenAI **不会**主动通知用户已断开。

### 2.8 计费与限额口径（影响产品行为）

- Plus 用户的 **5 小时限额在所有使用其套餐的 app 间共享**（含私有与 OSS），没有单独额度；Pro 不适用该 5 小时限制。
- 用户可在 ChatGPT 设置里按 app 设周限额（10%~100%）与是否可用 credits。→ **限额是常态，不是异常**。

---

## 3. 与 qaqh 现状的差距（实证）

| 事项 | 现状 | 缺口 |
|---|---|---|
| daemon 身份域 | admin token + device token（`Scope` ∈ view/interact/admin），`require_scope`（`crates/qaqh-daemon/src/axum_server/axum_impl/authz.rs:65`），配对与设备表已接线（`mod.rs:214-215`） | **与 SIWC 无关，保持不动**；只需新增「SIWC 方法必须 admin_only」这一条守卫 |
| 服务面 RPC | `POST /ringing/v2/service/{method}`，方法表带 `admin_only`/`requires_session` 标志，未知名 404（`axum_impl/service_api.rs:19-31`） | 这是 `siwc.*` 的自然落点 |
| 上游凭据 | `ProviderConfig{kind, base_url, api_key, model, …}` 里 `api_key: String` 静态字符串（`crates/qaqh-gate/src/types.rs:161-168`） | OAuth bearer 是**每请求可能刷新**的动值 → 需取值间接层（§4 C） |
| 配置模型 | `[profiles.*]` 带 `api_key/base_url/wire/compat/model/max_tokens`（`crates/qaqh-config/src/config.rs:93-103`）；密钥进 `secrets.toml`（Windows DPAPI，`secrets.rs:1-16`）；DTO 走 `qaqh-config-api`（`lib.rs:32-70`，ts-rs 生成给 FE） | 需要 `authMode` 维度（apikey / chatgpt）与「凭据在 daemon、不落 config」的表达 |
| Responses 分支 | 已发 `store:false`（`responses_sdk.rs:603`）；`max_output_tokens` 仅在 `max_tokens>0` 时发（`:586`，单测 `:1434`）；首条 system → `instructions`、后续 system/developer 原位（`:260`、`:505` `FirstToTopRestInPlace`） | 差距集中在：**工具形态（namespaces/additional_tools）**、**禁用字段清单**（`user`/`temperature`/`prompt_cache_retention`…）、**错误码映射**、**`response.completed` 判定已有但需与 plan 语义对齐** |
| 兼容位表达 | `EndpointCompat`/`ResponsesCompat` 已是「端点差异用配置声明，不加 gate 代码」的既定范式（`crates/qaqh-types/src/provider.rs:118+`，`responses_sdk.rs:199-201` 注释） | 新增一枚 plan 位即可，**不要新 ProviderKind** |
| 依赖可用性 | `Cargo.lock` 已有 `reqwest`、`ureq`、`tiny_http`、`sha2`、`base64`、`rand`、`subtle`、`ring`、`url`；daemon 已有 `axum`+`rustls`+`rcgen` | **无 JWT 库**：RS256 校验需自实现（`ring` + base64url，约百行）或新增依赖；`tiny_http`/`axum` 都能当回环回调监听器 |
| 参考实现 | `E:\codex\codex-rs\login\`（Rust：discovery、PKCE、回环 server、JWKS 校验、凭据存储、refresh、`revoke.rs`） | **可读不可抄**（那是 Codex 自家 client，client_id 不适用第三方）；但结构、测试范式（wiremock + e2e server）值得对照 |

---

## 4. 后端设计

### A. 新增叶子 crate `qaqh-siwc`

职责（**不依赖 runtime/daemon/config 引擎**，风格对齐 `qaqh-config-api` 的 K1 纪律）：

1. `host_id`：生成/持久化 `urn:uuid:`（或 jwk-thumbprint），写 daemon data dir。
2. `pending`（OAuth 事务）：`state`/`nonce`/`PKCE verifier`/`redirect_uri`/`issued_client_id`/`expires_at`(10min)，一次性消费。
3. `loopback`：`127.0.0.1:<port>`（默认 1455，占用则选可用端口）的 `/auth/callback` 监听器。
   守卫照抄 DevKit 的实证要点：校验 `Host` 头 == `127.0.0.1:<port>`、`state` 常数时间比对且唯一、
   非本次事务的回环请求**不得消费**待处理事务、响应带 `Cache-Control: no-store` + 严格 CSP。
4. `token`：换码/刷新/撤销；**刷新单飞 + 先落盘 checkpoint**。
5. `identity`：JWKS 拉取 + RS256 验签 + claims 校验；把「JWKS/端点不可用」与「token 无效」分成两类错误。
6. `store`：profile 持久化（DPAPI，复用 `qaqh-config` 的加密范式），记录
   `{label, client_id, ext_agent_host_id, subject, issuer, email, id_token, access_token, refresh_token, expires_at, scopes[]}`。
   **一个 profile 一个文件/一条记录**；跨账户绝不串 token。
7. `models`：`GET /v1/models` 过滤 `visibility=="list"`，返 `{slug, display_name}`。
8. `usage_url()`：常量 `https://chatgpt.com/settings/usage`（唯一真源，FE 用它，不各自硬编码）。

### B. daemon 服务面（全部 `admin_only`）

在 `service_methods` 方法表新增（沿用 `admin_only` 标志，`service_api.rs:27-31` 已有裁决）：

| 方法 | 类别 | 语义 |
|---|---|---|
| `siwc.status` | read | `{status: disconnected/connecting/connected, sharing, profileId, label, identity{name,email}, error?}`；**永不含 token** |
| `siwc.profiles` | read | 登录 profile 列表（展示信息 + sharing 位） |
| `siwc.sign_in.start` | write | 起事务 → 返回 `{flowId, authorizeUrl, expiresAt}`；**浏览器由宿主打开**（daemon 无 UI 语境；Tauri 已有 `open_external`） |
| `siwc.sign_in.poll` / `siwc.sign_in.cancel` | read/write | 轮询结果 / 取消（或改走事件推送） |
| `siwc.select_profile` | write | 切换活动 profile（并触发模型重取） |
| `siwc.sign_out` | write | 先 revoke 再清凭据 |
| `siwc.list_models` | read | 模型选择器数据源 |
| `siwc.use_plan` | write | 把 plan 绑定到全局 active 或指定 session（决定「本会话用谁的额度」） |

**安全矩阵（必须进 `authz_matrix_tests`）**：
- device token（view / interact）调任何 `siwc.*` → 拒绝。理由：pair 出来的远端设备若能触发登录，
  就等于「远程把新账户接进用户机器并写凭据」。
- 非回环 HTTP 面永不下发 token；`siwc.*` 响应里出现 `access_token`/`refresh_token`/`id_token` 字样应视为测试失败。
- 主机名/端口变更、并发两次 `sign_in.start` → 第二次拒绝（`connection_busy`）。

### C. 推理面接线（gate 取值间接层）

问题：`ProviderConfig.api_key: String` 是静态的，而 SIWC 的 bearer 会滚动。

- **M1 最小侵入（推荐起步）**：runtime 在每次 turn 构造 provider 时调用
  `siwc.access_token(profile)`（内部完成「快过期→刷新→单飞→落盘」），把结果填进 `api_key`。
  **gate 不改一行**，OAuth 知识不下沉到协议层。
- M2 正规化（可选）：`ProviderConfig` 引入
  `credential: ApiKeySource::{Static(String), Resolver(Arc<dyn Fn() -> Result<String>> + Send + Sync)}`，
  由 gate 在 build 请求前解析。收益有限，代价是把同步 trait 与生命周期掺进 gate；**建议延后**。

### D. Responses 兼容位（工程量主体）

新增 compat 位（命名建议 `siwc_plan` / `subscription_sharing`，落在 `ResponsesCompat`，默认 false）：

开启后 gate 必须：

1. **不透传被禁字段**：`temperature`、`top_p`、`metadata`、`user`（现由 `user_id_mode` 控制 → 该模式下强制不下发）、
   `prompt`、`truncation`、`background`、`conversation`、`previous_response_id`、
   `prompt_cache_retention`、`safety_identifier`、`multi_agent`、`moderation`、`max_tool_calls`。
2. **强制不发 `max_output_tokens`**（与现有 `max_tokens==0` 分支同形），并把本地侧上限改成「本地截断/压缩」语义。
3. **system 处理**：保持首条 → `instructions`；确认后续 system 降级为 developer item（文档禁的是
   `{type:"message",role:"system"}` item，developer 允许）。
4. **工具形态**：客户端 function/custom 工具改为 namespaces 或 `additional_tools` input item；
   关掉 server_tools 中的 `tool_search`/hosted 系列；web_search 由 compat 位单独控制。
5. **错误面**：`StreamEvent::Error` 带上 `code`/`param`/request id；runtime 映射为 UI 语义
   （`usage_limit` / `not_eligible` / `unsupported_capability` / `route_not_supported` / `transient`），
   并驱动已有的 recovery/交互面显示「Manage usage」。
6. **CI 哨兵**：一条 canary 请求体的快照测试，断言「被禁字段一个都不在」，把
   `subscription_sharing_unsupported_capability` 挡在发版前。

### E. 配置与 DTO 面

- `ConfigDto`/`ConfigPatch`（`qaqh-config-api`）新增 `authMode`（`"apikey" | "chatgpt"`）与
  `siwc: { status, sharing, label, identity }` 只读投影；**`apiKey` 的掩码语义对 chatgpt 档不适用**，
  不要让 FE 以为「填 key 就能用套餐」。
- profile 维度：一个 profile 要么 BYOK（key 在 `secrets.toml`），要么 chatgpt（凭据在 siwc store）——
  二者互斥，切换走既有 `profile.apply`（`crates/qaqh-client/src/endpoint.rs:253`）。
- **凭据绝不进 `config.toml`**（本仓库既有红线，`secrets.rs:1-16` 的由来），SIWC 凭据同样独立存储。

### F. 观测与凭据卫生

- 日志/诊断：token 与 `id_token_hint` 必须脱敏（authorize URL 含 hint 时整体打码）；
  错误上报保留 `x-request-id` / `openai-request-id`（文档点名要保留）。
- 不得把 token 放进任何 URL、界面、崩溃报告；`siwc.*` 响应结构在类型层就不含 token 字段（编译期保证好过 review 纪律）。

### G. 预留：identity-only

若将来 OpenAI 开放或我们拿到 `oaiapp_…`：同一 crate 里的「authorize + 换码 + 验 ID token」逻辑可直接复用，
新增的只是「HTTPS 回调 + 事务存储 + 自建会话（对应 daemon 的 admin/device token 签发）」。
因此 M0 的模块边界要保证：**`qaqh-siwc` 不假设回调一定是回环**（redirect_uri 从配置注入）。

---

## 5. 前端 Tauri 的「标准 ChatGPT 标识」——后端要供给什么

FE 只是消费方；**合规与语义的锚点在文档 `/siwc/ui-ux-guidelines`**：

1. **按钮**：文案只用 **Continue with ChatGPT**（黑/白两种官方样式之一），与其它登录方式同权重；
   登录页与设置页都可达。
2. **连接态**：登录后显示「已连接账户」（`identity.name/email` 来自 `siwc.status`），
   并明确套餐用量**已启用**；`sharing=false`（只登录未授权套餐）必须显示为「已登录、套餐未启用」，
   而不是「已连接」——这个区分直接映射到 `siwc.status.sharing`。
3. **用量标识**：使用套餐的请求附近显示 **Using ChatGPT plan** + **Manage usage**（深链 `usage_url`）。
4. **首次成功登录**：一次性欢迎卡「You're using your ChatGPT plan」+ Got it；此后不再出现。
5. **限流态**：收到 `usage_limit` 语义时，弹窗/紧凑卡把 **Manage usage** 作为主行动；
   若 qaqh 自己的额度存在则作为次级行动。
6. **已有用户**：对 BYOK 用户显示引导（Use your ChatGPT plan in this app + Continue with ChatGPT），
   并说明「切换需重新登录」。
7. **资产与商标**：官方 SVG/字体在 DevKit 内，**许可为 Noncommercial 且不含商标授权**。
   若要商业化发布，必须另签协议或采用自有视觉 + 仅保留合规文案（**这是产品/法务决策**，
   不是工程决策）。M0 建议：先只做协议与状态，不引入任何 OpenAI 图形资产。
8. **术语**：SIWC 的 host（安装实例）≠ daemon 的 device。UI 里不要出现「host」；
   诊断页可显示 host id 的存在与轮换策略，但不鼓励用户操作。

---

## 6. 硬约束与风险清单

| # | 约束 | 影响 |
|---|---|---|
| 1 | 只允许 `POST /v1/responses`（+`GET /v1/models`）；不得打 `chatgpt.com/backend-api` | plan 只能挂 Responses wire；Anthropic/Gemini/Chat-Completions 端点无法用套餐 |
| 2 | `store:false` + 无 `previous_response_id` | **会话历史必须本地全量回放**；压缩/裁剪必须在本地完成，且与 context 预算口径一致 |
| 3 | hosted 工具全禁（file search / code interpreter / computer use / hosted MCP / tool_search） | qaqh 自带工具（read/exec/edit/MCP 客户端/LSP）走客户端 function/custom tools **不受影响**；但任何「依赖服务端执行」的能力在 plan 档必须关闭 |
| 4 | 一批字段被禁（含 `temperature`、`user`、`prompt_cache_retention`） | 需 compat 位收口 + 哨兵测试，否则线上直接吃 400 `unsupported_capability` |
| 5 | Plus 5 小时限额跨 app 共享；用户可设 app 周限额 | **必须有降级路径**（切回 BYOK profile），否则用户体验是「随机不可用」 |
| 6 | refresh token 旋转 | 刷新必须单飞、串行、先落盘；daemon 与 Tauri 宿主**绝不能各持一份凭据** |
| 7 | 网络失败≠凭据失效 | 只有明确 `invalid_*`/`*_reused` 才清；其余保凭据 + 退避 |
| 8 | 仍处 preview | `sendHostId` 被 DevKit 标注为「仅兼容 deployment 开启」、`force_reconsent` 待确认 → 端点/参数/scope 集中常量表，便于跟随 |
| 9 | 许可 | DevKit 非商用 + 无商标授权；本仓库 MIT → 自实现协议，不拷代码/资产 |
| 10 | 可用性前提 | plan usage 面向**开源/本机运行**应用；若 qaqh 提供托管多租户服务，需走 commercial 协议 |

---

## 7. 分阶段落地与验收

**M0（不依赖任何 OpenAI 审批即可开发/验证）**
- `qaqh-siwc`：host id、事务、回环回调、换码、JWKS 校验、DPAPI 存储、刷新单飞、撤销。
- daemon：`siwc.status/profiles/sign_in.start/poll/cancel/select_profile/sign_out/list_models`（全 `admin_only`）。
- 验收：mock OIDC（本地起 authorize/token/jwks 三个桩，风格可参照 `E:\codex` 的 wiremock + e2e 范式）跑通
  「注册 → 回访 → 刷新 → 撤销」；authz 矩阵用例（device token 调 `siwc.*` 必拒）；
  响应体断言不含 token；并发 `sign_in.start` 必拒第二次。

**M1（协议兼容）**
- compat 位 `siwc_plan`：禁用字段、工具形态、错误码映射、`response.completed` 判定对齐。
- runtime：每次 turn 取 token（§4 C 方案 1）；plan 档 session 的模型来自 `siwc.list_models`。
- 验收：canary 快照（无被禁字段）；限流/不可用/不支持三类错误的语义档位单测。

**M2（真实端到端 + 前端标识）**
- 真机跑通：登录 → `listModels` → 一次 `streamResponse` 读到 `response.completed`；
- Tauri：按钮/连接卡/Using ChatGPT plan/Manage usage/首次欢迎/限流恢复；
- 验收：断开授权（用户在 ChatGPT 设置里撤销）后，daemon 首次请求失败即转入 `reauth_required` 且提示重登，不误删凭据。

---

## 8. 待决策（阻塞点）

1. **qaqh 的发行形态**：开源 + 用户本机运行？（决定能否走 OSS flow，以及能否使用官方标识资产）
2. **是否申请 identity-only（`oaiapp_…`）**：需要公网回调与事务存储 → 与「daemon 只在回环」的现状冲突，
   若要做，是另起服务还是由 daemon 反代？
3. **plan 与 BYOK 的关系**：默认口径？限额触发时是否自动回落 BYOK？同一会话能否中途切换？
4. **是否引入 JWT 校验依赖**：自实现（`ring` 已在树内，零新依赖）还是引入成熟 JWT 库？
5. **标识资产**：M0/M1 一律不引 OpenAI 图形资产是否可接受？
---

## 附：M0 实施记录（2026-10-14 落地）

决策：**OSS plan-usage 动态注册流** + 落 M0 骨架 + 前端按官方标识接入（FE 阶段另行）。

### 新增 `crates/qaqh-siwc`（叶子 crate，10 个模块）

| 文件 | 职责 | 关键不变量 |
|---|---|---|
| `protocol.rs` | 端点/scope/HOST 常量集中一处；discovery 严格校验 | issuer 精确匹配 + **每个 endpoint 的 origin 必须同 issuer**（防 discovery 被替换后凭据外送） |
| `util.rs` | 随机量、`urn:uuid` host id、常数时间比较、base64url | 随机数走 `ring::rand::SystemRandom`；PKCE 对 RFC 7636 附录 B 向量 |
| `host_id.rs` | `<dir>/chatgpt-host.json` 生成/复用 | 形态非法的既有值**不沿用**（文档：host id 不可是邮箱/用户 ID） |
| `loopback.rs` | 回环回调监听器 | 只监听 `127.0.0.1`、`Host` 头必须精确匹配、state 常数时间且唯一、**无关请求不消费事务**、响应无脚本 + 严格 CSP |
| `oauth.rs` | authorize URL、换码、刷新、撤销 | `redirect_uri` 由监听器产出（无「另拼一个」的机会）；刷新省略 `scope`；token 响应按文档字段严格校验 |
| `identity.rs` | RS256 + JWKS 校验（`ring`，零 JWT 依赖） | `aud` 对**下发的 client_id**、多 `aud` 必须有 `azp`、nonce 必等；未知 kid 判完整性失败，JWKS 拉取失败判**可用性**失败 |
| `store.rs` | 凭据/profile 持久化 | token 三件套仅以 DPAPI 密文落盘；`sharing` = 已连接 ∧ 有凭据 ∧ 有 `chatgpt.tokens.use.direct`；损坏文件不删 |
| `models.rs` | `GET /v1/models` | 只留 `visibility=="list"`、保服务端顺序、缺字段回退 slug |
| `manager.rs` | 登录流程、profile、刷新单飞、登出 | **注册先落盘再换码**；**刷新先落盘再使用**（旋转式 token 的检查点）；只有 `invalid_grant` 家族清凭据 |
| `error.rs` | 机器可读 code + `retryable`/`requiresReauth`/`needsUserAction` | 5xx/网络**绝不清凭据**；限流等 8 个上游 code 各自成档 |

### daemon 接线

- `service_methods.rs`：8 条 `siwc.*`，**全部 `admin_only`**（`qaqh_siwc::methods::ALL` 为清单真源）。
- `service.rs`：`QaqhService` 增 `siwc: OnceLock<Arc<SiwcManager>>`（**懒装配**到 `<data_dir>/siwc`，不用该能力则零磁盘副作用）+ 8 个 handle 分支。
- 越权用例（`authz_matrix_tests.rs`）：view/interact 设备对**每个** `siwc.*` 都得 403 `insufficient_scope`，且**从未触达凭据存储**；admin 路径返回的状态投影不含 token 字段。

### 验证（全部实测通过）

```
cargo test -p qaqh-siwc                63 passed（含 12 项对本地 mock OIDC 的端到端）
cargo test -p qaqh-daemon              90 passed（含新增越权用例）
cargo test -p qaqh-runtime --lib ringing  75 passed（含 siwc 方法表 admin 对齐）
cargo test -p qaqh-config               9 passed（DPAPI 往返 + 迁移）
```

端到端覆盖的真实路径：注册（下发 client_id 并用它换码）→ 回访（复用注册、不带 `agent_name_hint`）→ 刷新（旋转 + 落盘检查点 + 省略 scope）→ `invalid_grant`（清凭据保注册）→ 5xx（**不清**凭据）→ `earliest_refresh_at` 未到（可重试档，不发请求）→ 拒绝授权（不换码）→ 纯身份登录（`sharing=false`）→ 登出（远端撤销 + 保留注册）→ 切到别的账户（拒绝）。

### 顺带修掉的既有隐患

`crates/qaqh-config` 用 `Win32::Storage::FileSystem`（`MoveFileExW`）却没声明 `Win32_Storage_FileSystem` feature——全量构建时靠工作区其它 crate 的 feature 合并掩盖，**单独构建该 crate 必编译失败**（`cargo test -p qaqh-siwc` 实证）。已按「用到什么就声明什么」补上。

### 尚未做（下一步）

1. **M1 · gate 侧 Responses 兼容位**（本 M0 未动 `qaqh-gate`）：禁用字段清单、工具改 namespaces/`additional_tools`、`store:false`+`stream:true` 强化、错误码语义化、canary 快照测试。
2. **runtime 取值接线**：每次 turn 调 `SiwcManager::access_token` 填进 `ProviderConfig.api_key`（本 M0 只到服务面，未接推理路径）。
3. **FE（Tauri）**：`siwc.status/profiles/sign_in.*` 消费 + `Continue with ChatGPT` 按钮 + `Using ChatGPT plan`/`Manage usage`（`https://chatgpt.com/settings/usage`）；官方资产许可口径见 §5 第 7 条。
4. **真机联调**：用真实 ChatGPT Plus/Pro 账户跑通一次 `response.completed`；在此之前以上全部结论都只在 mock 上验证过。
5. profile 与 `ConfigDto.authMode` 的映射（把「用套餐」做成一种 profile 形态）。
---

## 附：M1 实施记录 —— gate 侧 Responses 兼容收口（2026-10-14 落地）

目标：让「套餐用量」这条路在**协议层**可跑——被禁字段、hosted 工具、流式要求、
prompt 缓存族全部收口，且**不靠人工 review 守住**。

### 改动面

| 位置 | 改动 |
|---|---|
| `crates/qaqh-types/src/provider.rs` | `EndpointCompat` 新增 `responses_siwc_plan: bool`（`#[serde(default)]`，**默认关**）+ `Default` 同步 |
| `crates/qaqh-gate/src/types.rs` | `ResponsesCompat` 新增 `siwc_plan: bool`（默认关） |
| `crates/qaqh-gate/src/responses_sdk.rs` | `SIWC_PLAN_FORBIDDEN_EXTRA_FIELDS`（16 个字段）+ `apply_siwc_plan`（**不可绕过**收口）；`max_output_tokens`/`web_search`/`user` 三处提前阻断；`build_options` 不下发 `prompt_cache_key`；`chat_sync_responses` 在 plan 模式改走流式（`collect_plan_stream`） |
| `crates/qaqh-config/src/registry.rs` | 旧平铺迁移表的构造点补齐（取缺省，不编造开关） |
| `crates/qaqh-runtime/src/agent/turn_lap/gate.rs` | 配置 → gate 的映射加一行 |

### 两条设计决定（值得记住）

1. **收口放在 `build_request` 的最后一步，而不是「记得别发」。**
   被禁字段一旦上线就是 400 `subscription_sharing_unsupported_capability`，
   而该错误码按文档意味着**我方发错了字段**——所以它必须由「任何路径塞进来都会被摘掉」
   来保证，而不是靠 review 纪律。对应的用例是 `siwc_plan_strip_cannot_be_bypassed`
   （先注入 16 个字段，再断言全被摘除）。
2. **非流式入口在 plan 模式改走流式，而不是报「不支持」。**
   文档要求**每个** HTTP 推理请求都带 `store:false` + `stream:true`；而压缩（compact）
   走的正是 `chat_sync`。直接拒绝会平白丢一个功能（上下文满了就报错），
   所以 `chat_sync_responses` 在 plan 模式下驱动流式接口并收集文本。

### 验证（实测）

```
cargo test -p qaqh-gate --lib                 67 passed（含新增 3 项）
cargo test -p qaqh-gate --test gate_test siwc_plan_sync_runs_over_the_streaming_route
                                              1 passed（HTTP 级：stream:true / store:false / 无被禁字段 / sync 走流式）
cargo test -p qaqh-types -p qaqh-config        11 / 4 / 6 / 9 passed
cargo check --workspace --all-targets          无错
```

### 顺带发现的既有红灯（**非本次引入**，已 A/B 实证）

`cargo test -p qaqh-gate --test gate_test` 里有 1 条失败：

```
default_provider_tool_history_is_unchanged  (gate_test.rs:591)
assertion failed: request["messages"][1].get("content").is_none()
```

- **证据**：`git stash push` 只回退本次改动的两个 gate 文件后，该用例**同样失败**
  （`0 passed; 1 failed`），与本次改动无关。
- **性质**：断言与实现口径矛盾。`openai_sdk.rs:396-402` 的注释写明「SDK 默认序列化
  本就把空文本写为 `"content": null`」，而用例期望的是**根本没有 `content` 键**；
  最近一次提交正是 `fix(gate): tool_call_content 语义回归`。二者需要择一对齐
  （要么用例改成断言 `content` 为 `null`，要么实现改为省略该键）。
  **本次未擅自改**——它落在另一条语义链上，且改哪边是产品判断。

### 还没做（下一步）

1. **没有人打开 `siwc_plan`。** 目前它只能由配置声明
   （`[profiles.X.endpoint.compat] responses_siwc_plan = true`）。
   真正的下一步是 **runtime 取值接线**：SIWC profile 激活时自动置位该 compat，
   并在每个 turn 调 `SiwcManager::access_token()` 填进 `ProviderConfig.api_key`。
   在那之前，套餐用量**仍不能真正跑推理**。
2. 错误码尚未提升为类型化档位（`ErrorKind::SubscriptionSharing{code}`）：
   当前 `error.code` 随 raw body 带出，runtime/FE 只能按字符串匹配。
3. 真机联调未做；迄今全部结论只在 mock 上验证。
---

## 附：M2 实施记录 —— runtime 取值接线（2026-10-14 落地）

目标：让请求**真的用上套餐额度**——SIWC profile 激活时自动启用该路由的请求契约，
并在每个 turn 现取 access token。做完这条，「能不能真跑」只剩真机联调。

### 改动面

| 位置 | 改动 |
|---|---|
| `qaqh-types` | 新增 `CredentialSource { ApiKey（缺省）, ChatgptPlan }`；`ProfileConfig.credential` |
| `qaqh-config` | `Config.credential` + 由活动 profile 合并 + `Default`；4 处 `ProfileConfig` 字面量补齐 |
| `qaqh-siwc` | `global.rs`：`install_manager` / `manager`（与 `qaqh_mcp::install_manager` / `qaqh_lsp::install_manager` 同范式） |
| `qaqh-runtime` `turn_lap/gate.rs` | `provider_for` → `Result<ProviderConfig, String>`；新增 `apply_chatgpt_plan` |
| 三个调用点 | 回合 → `Outcome::Error`；标题 → 保留截断标题；压缩 → 跳过本轮 |
| `service.rs` | 懒装配管理器时就地安装到全局槽位 |

### 三个设计决定（都值得记住）

1. **凭据来源是显式字段，不是推导。**
   若用「api_key 为空 + 某个 compat 位」隐式触发，一个配置错误就会变成
   「静默用了别的额度」——最难查、也最伤用户信任的一类问题。
2. **管理器走进程级装配。** `provider_for(ctx, …)` 的 `RingContext` 里没有 service 句柄，
   而往 `RingContext` 加字段会波及所有 engine 构造点。沿用本仓库既有的
   「基础设施管理器」装配方式：单点装配、深层取用（`global.rs` 里写明了这个取舍）。
3. **三条校验一律「拒绝」而不是「回退」**（`apply_chatgpt_plan`）：
   - wire 非 `responses` → 拒（该路由只提供 Responses API）；
   - `base_url` 非官方 → 拒（否则等于把套餐 bearer 发往用户没预期的端点）；
   - 未登录 / 未授权 → 拒并给可读原因（回落成空 key 只会打出一个上游 401，把真因埋掉）。

   顺带两个**静默降级陷阱**在配置层被堵住：`save_current` / `materialize` 重建 profile 时
   必须**保留** `credential`，否则「用套餐」会被悄悄改回 BYOK（与既有「保留 api_key 标记」
   是同一类陷阱，注释已写在各自的位置）。

### 验证（实测）

```
cargo test -p qaqh-runtime --lib chatgpt_plan     4 passed
cargo test -p qaqh-runtime --lib                  265 passed, 1 failed（既有红灯，见下）
cargo test -p qaqh-siwc                           63 passed
cargo test -p qaqh-gate --lib                     67 passed
cargo test -p qaqh-gate --test gate_test          40 passed, 1 failed（既有红灯，见下）
cargo test -p qaqh-daemon                         90 passed
cargo test -p qaqh-config -p qaqh-types           11/12/11/4/6/9/1/36 全过
cargo check --workspace --all-targets             无错
```

### 两个既有红灯（**均与本次改动无关**，已各自取证）

| 用例 | 证据 |
|---|---|
| `gate_test.rs::default_provider_tool_history_is_unchanged` | `git stash` 只回退本次改动后**同样失败**（0 passed; 1 failed）。断言要求 assistant 消息**没有** `content` 键，而 `openai_sdk.rs:396-402` 写明「SDK 默认把空文本写为 `content: null`」——测试与实现口径需要择一对齐（改哪边是产品判断） |
| `qaqh-runtime --lib agent::prompt::tests::prompt_and_tool_defs_char_budget` | 断言 `identity_chars <= 12_000`，其**唯一输入**是 `include_str!("prompts/backend_prompt.md")`；该文件与 `prompt.rs` 在 `git status` 里均未修改，实测 **13,221** 字符 → 在 HEAD 上就已超标 |

### 还没做

1. **`credential` 尚未进 `ConfigDto` / 设置页**：今天要用套餐，得手改
   `config.toml`（`[profiles.X] credential = "chatgpt_plan"` + `wire = "responses"`）。
   把它做进设置页（含「用 ChatGPT 账户登录」入口）是 FE 阶段的活。
2. **FE 尚未桥接** `siwc.*`：Tauri 宿主需要把 `siwc.sign_in.start` 的 `authorizeUrl`
   用系统浏览器打开（`open_external`），并把 `siwc.status` 接到账户卡/套餐标识。
3. **真机联调**：迄今全部结论只在 mock 上验证过。
