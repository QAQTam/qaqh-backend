# 服务面规格：`siwc.*` / `dsaccount.*`（18 个方法）

> 日期：2026-10-19　真源：`crates/qaqh-runtime/src/service.rs`（转发层）+
> `qaqh-siwc` / `qaqh-dsaccount`（投影层）　客户端出口：`qaqh-client`
> （`Client::service_call` + `qaqh_client::service_types::*`）
>
> 本文档回答三个问题：**每个方法收什么参数、回什么形状、失败长什么样**。
> 形状的 typed 版本在 `crates/qaqh-client/src/service_types.rs`，两边必须同步改。

---

## 0. 通用规则

### 0.1 端点与身份

```
POST /ringing/v2/service/{method}
Authorization: Bearer <admin token>
X-QAQH-Client-Session-Id: <client session id>
Content-Type: application/json
```

- 18 个方法**全部 `admin_only`**：配对出来的设备（view/interact）一律 403。
  理由：它们是「本机身份」——谁调 `sign_in.start`，谁就能把新账户接进用户机器。
- `X-QAQH-Client-Session-Id` 是 daemon 定位 lease 的依据，**登录类方法也要带**
  （零会话时它指的是刚协商出来的 lease，不是某个 attach 过的会话）。
- **不需要 seed、不需要 `attach`**。不要把「登录」错接到会话作用域上：
  引导页的第一件事就是登录，那时还没有任何会话。

### 0.2 成功响应

方法自己的形状，直接是响应体（2xx 即成功）。没有统一信封。

### 0.3 方法级失败（HTTP 非 2xx）

```json
{ "code": "…", "message": "…" }
```

| `code` | 状态码 | 含义 |
| --- | --- | --- |
| `unknown_method` | 404 | 方法不在 daemon 方法表里 |
| `invalid_body` | 400 | 请求体不是合法 JSON |
| `invalid_envelope` | 400 | 缺 `session_id`（本组方法都不会） |
| `lease_required` | 401 | 缺 client session 头 / 会话不归属本 lease |
| `forbidden` | 403 | 缺 admin scope，或 `FORBIDDEN:` 前缀的边界拒绝 |
| `not_driver` | 403 | 会话被另一客户端持有 driver 席位（本组方法都不会） |
| `query_failed` | 400 | 读类方法执行失败 |
| `action_failed` | 400 | 写类方法执行失败（`message` 是给用户看的原因） |

> `qaqh-client` 的 `Client::service_call` 会把这份 JSON 翻成
> `ClientError::Api { status, code, message }`；白名单外的名字**在客户端就拒**
> （`ClientError::ServiceMethodNotAllowed`，一个请求都不发）。

### 0.4 成功响应**内部**的错误槽

`siwc.status.error` 与 `siwc.sign_in.status.error` 是**另一个形状**（不是 0.3 那个）：

```json
{
  "code": "sign_in_required",
  "message": "…",
  "retryable": false,
  "requiresReauth": true,
  "requestId": "可选"
}
```

对应 DTO：`qaqh_client::service_types::ServiceError`。

### 0.5 命名口径（**已钉死**）

| 范围 | 口径 |
| --- | --- |
| `siwc.*` 全部字段 | camelCase（`usageUrl` / `expiresAtMs` / `clientId` / `savedAtMs` / `displayName` / `profileId`） |
| `siwc.*` 枚举值 | snake_case（`reauth_required` / `disconnected`） |
| `dsaccount.*` 外层字段 | camelCase |
| `dsaccount.status.account` **嵌套层** | **snake_case**（`user_id` / `saved_at_ms` / `has_credential`）——原样转发 `StoredAccount::to_public_json` |
| `dsaccount.status.flow` | camelCase（`attemptId` / `expiresAtMs`） |
| `dsaccount.sign_in.status` | **snake_case**（`attempt_id` / `expires_at_ms`）——与上面同一状态机的另一套拼写 |
| `dsaccount.status.revoke` | `state` / `attempts` / `message`（无大小写差异） |
| 金额（`balance` / `amount`） | **服务端原串**（big.js 语法：`0E-16` 就是 0）。不要转 `f64` 存权威值 |

最后两行的不一致是**历史事实**，不是笔误；DTO 已按各自拼写分开定义，
并有测试（`dsaccount_flow_and_sign_in_status_keep_their_own_spelling`）挡住「顺手统一」。

---

## 1. `siwc.*`（Sign in with ChatGPT / 套餐用量，8 个）

方法名常量：`qaqh_siwc::methods::{STATUS, PROFILES, MODELS, SIGN_IN_START,
SIGN_IN_STATUS, SIGN_IN_CANCEL, PROFILE_SELECT, SIGN_OUT}`。

### 1.1 `siwc.status` → `SiwcStatus`

请求：`{}`

```json
{
  "status": "connected",            // connecting | connected | reauth_required | disconnected
  "sharing": true,                   // 套餐用量是否真的可用（另判，见下）
  "profiles": 2,                     // 已登记 profile 的**条数**（不是数组）
  "usageUrl": "https://…",           // 「管理用量」深链，FE 不得自己拼
  "apiBase": "https://api.openai.com/v1",
  "profileId": "p_1",                // 以下四个仅在有活动 profile 时出现
  "label": "工作",
  "email": "a@b.c",
  "name": "A",
  "error": { … }                     // 仅 reauth_required 时出现（§0.4 形状）
}
```

- `connected` 但 `sharing == false` 是**真实状态**（登录了、但没授予直连套餐用的
  scope）：UI 必须显示成「已登录 · 套餐未启用」，不能显示成「已连接」。
- `connecting` 只在**流程未终结且未过期**时出现；终态流程不会把状态钉在「连接中」。

### 1.2 `siwc.profiles` → `SiwcProfiles`

请求：`{}`

```json
{
  "active": "p_1",                   // null = 无活动 profile
  "profiles": [{
    "id": "p_1", "label": "工作", "status": "connected", "sharing": true,
    "clientId": "oaiapp_…", "scopes": ["openid", "chatgpt.tokens.use.direct"],
    "savedAtMs": 1728000000000,
    "email": "…", "name": "…"        // 可选
  }],
  "pending": [{
    "id": "p_2", "label": "待完成", "status": "disconnected",
    "sharing": false, "pending": true
  }]
}
```

`pending` 是**注册中途**的账户：能显示、不能选（选它会回 `profile_not_found`
配一条「注册尚未完成，请重新登录」的文案）。

### 1.3 `siwc.models` → `[SiwcModel]`

请求：`{ "profile_id": "可选" }`（缺省 = 活动 profile）

```json
[{ "slug": "gpt-5", "displayName": "GPT-5" }]
```

### 1.4 `siwc.sign_in.start` → `SiwcSignInStart`

请求：

```json
{ "profile_id": "可选", "new_profile": false, "label": "可选", "reconsent": false }
```

- `profile_id` 与 `new_profile: true` **不能同时给**（回 `action_failed`）。
- 已有一次**未过期且 pending** 的流程时，再开会回 `action_failed`
  （「已有一次登录在进行中」）——先 `sign_in.cancel` 再开。

响应：

```json
{
  "flowId": "f_1",
  "authorizeUrl": "https://auth.openai.com/…",   // 交给宿主用系统浏览器打开
  "redirectUri": "http://127.0.0.1:1455/callback",
  "expiresAtMs": 1728000000000,                  // 毫秒
  "usageUrl": "https://…"
}
```

### 1.5 `siwc.sign_in.status` → `SiwcSignInStatus`

请求：`{ "flow_id": "f_1" }`

```json
{ "flowId": "f_1", "state": "pending",   "expiresAtMs": 1728000000000 }
{ "flowId": "f_1", "state": "completed", "profileId": "p_1" }
{ "flowId": "f_1", "state": "failed",    "error": { … §0.4 … } }
```

- **终态（`completed` / `failed`）后停止轮询。**
- `flow_id` 与当前流程不符 → `action_failed`（`profile_not_found` 语义，文案
  「登录流程 id 不匹配（可能已被新的登录取代）」）。这就是 **superseded**：
  被新流程顶掉时，旧页面应停止轮询并提示，**不要**去 cancel（旧 id 也取消不了）。

### 1.6 `siwc.sign_in.cancel` → `null`

请求：`{ "flow_id": "f_1" }`

- 幂等：没有进行中的流程时返回 `null`；id 不符 → `action_failed`。
- **超时必须取消**（宿主关闭授权页 / 用户放弃）：不取消的话下一次
  `sign_in.start` 会被「已有一次登录在进行中」挡住，直到流程超时。

### 1.7 `siwc.profile.select` → `SiwcStatus`

请求：`{ "profile_id": "p_1" }`

切换活动 profile 并**返回切换后的状态**（省一次 `siwc.status` 往返）。
选 `pending` 里的条目 → `action_failed`（「注册尚未完成」）。

### 1.8 `siwc.sign_out` → `SiwcSignOutResult`

请求：`{ "profile_id": "可选" }`（缺省 = 活动 profile）

```json
{ "profileId": "p_1", "revoked": true, "hadRefreshToken": true, "usageUrl": "https://…" }
```

- 顺序是**先撤销远端、再清本地**；远端失败**不算登出失败**：本地已清，
  `revoked: false` 时 UI 提示用户去 ChatGPT 设置里手动断开。
- `hadRefreshToken: false` 说明本来就没有可撤销的凭据（诊断用）。

---

## 2. `dsaccount.*`（DeepSeek 账号，10 个）

方法名常量：`qaqh_dsaccount::methods::{STATUS, BALANCE, BONUSES, SIGN_IN_START,
SIGN_IN_STATUS, SIGN_IN_CANCEL, SIGN_OUT, ACK_BONUS, MODELS, VERIFY}`。

### 2.1 `dsaccount.status` → `DsAccountStatus`

请求：`{}`

```json
{
  "signedIn": true,
  "account": {                       // null = 未登录（**snake_case**）
    "user_id": "u_1", "label": "…", "provider": "WECHAT", "currency": "CNY",
    "inference_origin": "https://api.deepseek.com",
    "saved_at_ms": 1728000000000, "has_credential": true
  },
  "storageError": null,              // 非 null = 本地凭据读取失败（**不是**未登录）
  "inferenceOrigin": "https://api.deepseek.com",
  "usageUrl": "https://…", "topUpUrl": "https://…",
  "flow": {
    "attemptId": null,
    "state": "idle",                 // idle | authorizing | exchanging | signed-in | failed | cancelled
    "authorizeUrl": null, "expiresAtMs": 0, "error": null
  },
  "revoke": { "state": "idle", "attempts": 0, "message": null }
}
```

- `signedIn: false` + `storageError: "…"` 是「本地凭据损坏」，要引导重登，
  而不是显示成一个干净的空态。
- `revoke` 只在**本进程内存**里（撤销的前提是本地凭据已删）：进程重启即回 `idle`。

### 2.2 `dsaccount.balance` → `DsAccountBalance`

请求：`{}`

```json
{
  "recharge": [{ "currency": "CNY", "balance": "5.0000000000000000", "tokenEstimation": "…" }],
  "bonus":    [{ "currency": "CNY", "balance": "0E-16", "tokenEstimation": null }],
  "totalCost":[{ "currency": "CNY", "amount": "1.2500000000000000" }],
  "hasUsableBalance": true
}
```

充值 / 赠金**分开**展示（促销额度不得冒充充值）。金额保留原串。

### 2.3 `dsaccount.bonuses` → `[DsAccountBonus]`

请求：`{}`

```json
[{ "orderId": "o_1", "campaign": "welcome", "amount": "1.00", "currency": "CNY",
   "grantedAt": "…", "expiresAt": "…", "message": "服务端文案" }]
```

### 2.4 `dsaccount.sign_in.start` → `DsAccountSignInStart`

请求：`{ "locale": "zh", "port": 0 }`（都可选；`port` 被占用时 daemon 内部回退临时端口）

```json
{
  "attemptId": "a_1",
  "authorizeUrl": "https://…",          // 交给宿主用系统浏览器打开
  "redirectUri": "http://127.0.0.1:PORT/oauth/callback",
  "expiresIn": 600                       // **秒**（siwc 是毫秒，别混）
}
```

- 已有进行中的尝试时，daemon **先收掉它**（用户连点两次不该把自己卡住）：
  旧尝试的 worker 因 attempt id 不匹配而无法覆盖新状态。

### 2.5 `dsaccount.sign_in.status` → `DsAccountSignInStatus`

请求：`{}`（**无参数**：一个进程只有一个当前尝试）

```json
{ "attempt_id": "a_1", "state": "authorizing",
  "authorize_url": "https://…", "expires_at_ms": 1728000000000, "error": null }
```

- **snake_case**（见 §0.5）。
- `state ∈ {idle, authorizing, exchanging, signed-in, failed, cancelled}`；
  `authorizing` / `exchanging` 之外即终态 → 停止轮询。

### 2.6 `dsaccount.sign_in.cancel` → `null`

请求：`{ "attempt_id": "a_1" }`

- id 必须是**当前**尝试，否则 `action_failed`（「这次登录尝试已经不是当前尝试」）。
  旧页面不得取消新登录。
- 超时同样要 cancel（与 siwc 一致）。

### 2.7 `dsaccount.sign_out` → `null`

请求：`{}`　本地先清，远端撤销在后台有界重试；远端失败**绝不**恢复本地登录态。

### 2.8 `dsaccount.bonus.ack` → `null`

请求：`{ "order_id": "o_1" }`（确认这笔赠额已展示）

### 2.9 `dsaccount.models` → `[DsAccountModel]`

请求：`{}`

平台条目**原样转发**（比 OpenAI 标准形态多：上下文窗口、输出上限、能力位、
`effort.supported_levels`）。DTO 只钉 `id`，其余进 `extra`（`#[serde(flatten)]`）——
在转发层或客户端「猜」完整键名就是造第二个真相源。

### 2.10 `dsaccount.verify` → `DsAccountVerify`

请求：`{}`

```json
{ "valid": true,  "reason": null, "cleared": false }
{ "valid": false, "reason": "…", "cleared": true }
```

- 只有它回答「这把凭据现在还有效吗」。
- `cleared: true` = 判定失效且**本地凭据已被清** → UI 立刻切未登录态。
- 方法表把它标为**写类**（可能清凭据），失败报 `action_failed`。
  它在 `qaqh_dsaccount::methods::ALL` 里（2026-10-19 补入；此前只在 daemon 方法表，
  客户端按 `ALL` 生成白名单会漏掉它）。

---

## 3. 客户端实现契约（与桌面端已跑通的两条流程一致）

1. **超时也要 cancel**：浏览器授权页被关掉、用户放弃、流程超时——三条路径都要
   发 `*.sign_in.cancel`，否则下一次 `start` 会被「已有一次登录在进行中」挡住
   （siwc）或被先收掉（dsaccount，语义不同但同样要清）。
2. **superseded 不是错误**：siwc 的 `sign_in.status` 在 id 不符时报错、dsaccount 的
   `cancel` 在 id 不符时报错——这都意味着「我不是当前尝试了」，调用方应**停止轮询**，
   而不是重试或提示失败。
3. **终态后不再轮询**：`completed` / `failed`（siwc）、非 `authorizing`/`exchanging`
   （dsaccount）。
4. **登录类方法不要求会话**：不注入 seed、不要求 `attach`（见 §0.1）。
5. **浏览器属宿主**：daemon 不 spawn 进程、不内嵌 webview；客户端拿 `authorizeUrl`
   用系统浏览器打开，**永不接触 token**（投影里也没有 token 字段）。

---

## 4. 复现入口

```powershell
# 方法表 + 白名单的两向闸
cargo test -p qaqh-runtime service_methods::

# 客户端白名单 / DTO 形状
cargo test -p qaqh-client service

# 转发层（形状的真源）
git grep -n '"siwc\.\|"dsaccount\.' -- crates/qaqh-runtime/src/service.rs
```
