# 客户端服务面缺口：`qaqh-client` 触达不到的 18 个 admin 方法、wire 拼写非对称、缺失的类型出口

> 日期：2026-10-19　范围：`qaqh-client` / `qaqh-config-api` / `qaqh-config` / `qaqh-siwc` / `qaqh-dsaccount`
> 来源：`qaqh-tui` 于同日适配上游变动（`reasoning_kind` / `UsageSampled` / `credential` / `wire` 改名）时逐条实测，
> 并与 `qaqh-desktop-app` 已上线的同类实现对照。
> 结论：5 项请求，其中 **R1 是硬阻塞**（不是"没 UI"，是"调不到"）；每项都给了验收标准。

---

## 0. 结论摘要

| # | 请求 | 阻塞什么 | 优先级 |
| --- | --- | --- | --- |
| **R1** | `qaqh-client` 暴露服务面调用（白名单式 `service_call`） | TUI 的套餐（SIWC）/DeepSeek 账号登录、余额、模型目录**完全不可达** | **P0** |
| **R2** | `siwc.*` / `dsaccount.*` 的 typed DTO（当前 `status()` 直接返回 `Value`） | 每个客户端各自手抄 wire 形状（桌面端已手抄一份，TUI 将被迫再抄一份） | P1 |
| **R3** | `ConfigPatch::validate` 接受规范 wire 拼写（`chat`/`messages`） | 每个客户端都得写一层"读规范名 / 写旧拼写"的桥；写错就是 400 | P1 |
| **R4** | `qaqh-client` 出口补 `ReasoningKind` / `Wire` / `CredentialSource` | 客户端被迫直接 path 依赖 `qaqh-types`（TUI 已这么做） | P2 |
| **R5** | `dsaccount.verify` 未进 `qaqh_dsaccount::methods::ALL`；反向穷举闸是空转 | 客户端按 `ALL` 生成白名单会漏掉"凭据是否还有效"这一条 | P1 |

已确认**不需要**后端改动的 4 项见 §6。

---

## 1. R1：`qaqh-client` 触达不到 18 个 admin 方法

### 1.1 现状（证据）

daemon 侧**已经就绪**：`qaqh-runtime/src/ringing/service_methods.rs:119-140` 收录 18 个方法，全部
`READ_ADMIN` / `WRITE_ADMIN`，并**声称**有正反两向的穷举测试（`:194-232`）——但那条"反向"
实际是空转，见 R5。

| 组 | 方法 | 档 |
| --- | --- | --- |
| SIWC（8） | `siwc.status` / `siwc.profiles` / `siwc.models` / `siwc.sign_in.start` / `siwc.sign_in.status` / `siwc.sign_in.cancel` / `siwc.profile.select` / `siwc.sign_out` | READ/WRITE_ADMIN |
| DeepSeek 账号（10） | `dsaccount.status` / `.balance` / `.bonuses` / `.sign_in.start` / `.sign_in.status` / `.sign_in.cancel` / `.sign_out` / `.bonus.ack` / `.models` / `.verify` | READ/WRITE_ADMIN |

> `dsaccount.verify` 只存在于方法表与 `service.rs:947`，**不在** `qaqh_dsaccount::methods::ALL`
> （该 `ALL` 只有 9 条）——见 R5。客户端若按 `ALL` 生成白名单，会漏掉它。

客户端侧**一个都调不到**：`crates/qaqh-client/src/endpoint.rs` 只有两个**封闭枚举**
`QueryRequest`（`:36`）与 `ActionRequest`（`:130`），方法名映射在 `into_parts`（`:88` / `:228`）
里穷举，且是 `pub(crate)`——没有任何泛化入口。`Client` 的公开面（`client.rs:385-901`）只有
`send_command` / `query` / `action` / `bootstrap` / `attach` / … ，没有 `service_call`。

也就是说：**daemon 端能力齐了、权限也对了，但 Rust 客户端库没有对应出口**。TUI 侧只能看到
`ConfigDto.credential` 这个字段值，无法发起登录、读余额、取模型目录。

### 1.2 对照：桌面端为什么能调

桌面端不是 Rust 客户端库的用户，它有自己的壳层：

- `qaqh-desktop-app/src-tauri/src/commands.rs:20` 的 `SERVICE_METHODS` 白名单，
  `:557+` 的 `service_rpc(method, params)` 直连；
- `:104 service_requires_session` 把登录类方法从"必须注入 seed"里**豁免**——否则零会话时
  （引导页第一件事就是登录）会因注入失败报 `no_active_seed`，把登录入口直接堵死。

这条豁免语义是 daemon 侧契约的一部分，任何客户端实现都必须复现（见 §1.3 验收 4）。

### 1.3 方案 A（推荐）：白名单式通用调用

```rust
// crates/qaqh-client/src/service.rs（新）

/// 客户端放行的服务面方法。**单一真源**是各 crate 的方法表
/// （`qaqh_siwc::methods::ALL` ∪ `qaqh_dsaccount::methods::ALL`）。
///
/// 写法二选一：
///  1) 逐项引用常量（编译期就能发现拼写错/方法被删）：
///
///     pub const SERVICE_METHODS: &[&str] = &[
///         qaqh_siwc::methods::STATUS,
///         qaqh_siwc::methods::PROFILES,
///         // … siwc 共 8 项
///         qaqh_dsaccount::methods::STATUS,
///         // … dsaccount 共 9 项（+ R5 补齐后的 VERIFY）
///     ];
///
///  2) 运行时合并两个 `ALL`（`OnceLock`），R5 修好前显式补一条 `verify`。
///
/// Rust 常量不能跨 crate 拼接切片，所以 `const X: &[&str] = &[A..., B...]` 不能
/// 直接写成 `ALL ∪ ALL`——上面的形态是有意为之，不是偷懒。

impl Client {
    /// 服务面直调：方法名必须落在 [`SERVICE_METHODS`]，越界**在客户端就拒**（不发请求）。
    pub async fn service_call(&self, method: &str, params: Value) -> Result<Value>;
}
```

理由：

- 审计故事不降级：白名单常量就是"前端能调哪些 daemon 方法"的可审计面（与桌面端同款思路），
  且比 18 个 typed 变体更难漏改——新方法进 daemon 时，穷举闸会先红；
- 不需要 `qaqh-client` 为两家登录流程建模（flow_id / attemptId、轮询、取消、`superseded` 判定
  都是**客户端**的事，见桌面端 `src/lib/siwc.ts:134` / `src/dsaccount/flow.ts:51`）；
- 未来新增 admin 方法（如设备管理类）不必再改客户端库版本。

**验收标准**

1. `siwc.sign_in.start` → 轮询 `sign_in.status` → `completed`，以及 `dsaccount.sign_in.start`
   → `status` → `signed-in` 两条全流程可由 TUI 走通（无需改 daemon）；
2. 非白名单方法（如 `diagnostics.memory.snapshot`，若未纳入）在客户端即被拒，且错误可区分
   "不存在/不允许"与"daemon 报错"；
3. 穷举闸测试：`qaqh_siwc::methods::ALL ∪ qaqh_dsaccount::methods::ALL ⊆ SERVICE_METHODS`
   且反向无死条目。**注意**：`service_methods.rs:194-232` 现存的"反向"是空转（见 R5），
   新写的这条闸请真的从方法表遍历回来，别照抄；
4. 登录类方法**不要求会话**：`service_call` 不得在任何路径上注入 seed 或要求 `attach`。

### 1.4 方案 B（不推荐）：18 个 typed 变体

给 `QueryRequest` / `ActionRequest` 各加若干变体即可编译通过，但代价是：

- `all_query_requests` / `all_action_requests` 穷举闸（`endpoint.rs:30` 的注释）维护面直接翻倍；
- 每个变体都要建模请求参数（`flow_id` / `attempt_id` / `profile_id` / `new_profile` / `label` /
  `reconsent` / `port` / `locale` …），而这些字段**已经**由服务面 JSON 定义；
- 返回仍是 `Value`（见 R2），typed 变体只 typed 了壳。

若最终选 B，请至少保留"参数为 `Value`"的形态，不要为 18 个方法各造一套 `*Params`。

### 1.5 不需要改的部分

`service_methods.rs` 的表、`READ_ADMIN`/`WRITE_ADMIN` 档位、设备（view/interact）403 的
`authz_matrix_tests` 都不用动——TUI 与桌面端一样持 admin token。

---

## 2. R2：`siwc.*` / `dsaccount.*` 的返回是 `Value`，没有 typed DTO

**证据**

- `crates/qaqh-siwc/src/manager.rs:175 pub fn status(&self) -> Value`、`:240 pub fn profiles(&self) -> Value`；
- `crates/qaqh-dsaccount/src/manager.rs:183 pub fn status(&self) -> Value`；
- 同 crate 里其实已有 Rust 类型（`qaqh-siwc/src/manager.rs:92 SignInStart`、`:101 FlowState`；
  `qaqh-dsaccount/src/manager.rs:33 FlowState`、`:70 SignInStart`），但**没有一个是客户端可见的出口**。

**后果（已经发生的）**

桌面端只能手抄：`qaqh-desktop-app/src/siwc/brand.ts::SiwcSessionStatus`、`src/lib/siwc.ts:16-52`
（`SiwcModelEntry` / `SiwcSignInStart` / `SiwcFlowState` / `SiwcSignOutResult`），
其 `docs/siwc-brand-posture.md` 待办 #5 自己也记了这笔：
"`siwc.*` 的类型化 DTO（当前 FE 用结构对齐的本地类型）"。
TUI 已就 R1 提出需求；一旦 R1 落地，**第二份手抄**立刻出现。

**建议（按成本从低到高）**

1. 先出 fixture：在 `docs/` 放一份 `spec-service-surface-siwc-dsaccount.md`，逐方法列请求参数
   与响应 JSON 形状（含 `error { code, message, retryable?, requiresReauth? }` 与 `cancelled` 终态），
   并附 `crates/qaqh-runtime/tests/` 里可执行的真实响应样例；
2. 把 `manager.rs` 里已存在的 `SignInStart` / `FlowState` / status 结构提升为 `pub` DTO，
   或新建 `qaqh-client/src/service_types.rs` 定义 serde DTO，由 `client` 出口；
3. 服务面 JSON 的 camelCase/snake_case 口径要在文档里钉一句（桌面端按 camelCase 解析：
   `flowId` / `expiresAtMs` / `authorizeUrl` / `usageUrl` / `attemptId` / `inferenceOrigin`）。

**验收标准**：客户端能只依赖 `qaqh-client` 就把 `siwc.*` / `dsaccount.*` 的响应反序列化成
结构体，不再出现"结构对齐的本地类型"。

---

## 3. R3：wire 拼写非对称（读规范名 / 写只收旧拼写）

**证据**

- 读：`crates/qaqh-config/src/dto.rs:27` → `wire: cfg.wire.as_str()`，HEAD 上 `Wire::as_str()`
  返回 `chat` / `messages`（`qaqh-types/src/provider.rs:56-61`）；
- 写：`crates/qaqh-config-api/src/lib.rs:314` 的值域仍是 `openai|responses|anthropic|gemini`，
  直发 `chat` = 400 `action_failed`；
- 解析器其实已经两边都认：`qaqh-types/src/provider.rs:66-80` 的 `Wire::parse` 同时接受
  `chat` 与 `openai`、`messages` 与 `anthropic`。

**后果（每个客户端各写一套桥）**

- 桌面端：`src/oauth/provider-state.ts:81-83` 明确记录"读模型回显规范名，但 `ConfigPatch::validate`
  目前只收旧拼写；桥在 `../settings/patch::wireForDaemon`（直发规范名 = 400）"；
- TUI：为同样的原因新加 `src/app/settings.rs:53 wire_daemon_spelling`（读侧折回写拼写），
  并专门加了漂移护栏测试，防止上游再改名时"点一下方向键跳两格"；
- 第三个客户端会再写第三套。

**建议（二选一）**

- **a（推荐）**：把 `validate` 的 wire 值域改为 `Wire::parse` 判定——它已经是唯一的拼写解析器且
  向后兼容旧值；错误文案随之给出规范名。这样读写同一套 canonical，客户端零桥。
- **b**：维持值域不变，但把 `ConfigDto.wire` 的 doc（现在仍写四个旧名）与错误文案改成
  "读返回规范名、写只收旧拼写"的显式说明，并在 `qaqh-config-api` 导出一个**唯一**的
  `wire_for_patch(&str) -> Option<&str>`，客户端直接复用而不是各写一份。

无论选哪个，`qaqh-config-api` 里 `wire: String` 的字段注释（写的是 `openai | responses |
anthropic | gemini`）都已经与实际输出不一致，需同笔修正。

**验收标准**：`ConfigPatch { wire: Some("chat") }` 被接受且落成 `Wire::Chat`；`ConfigDto` 读回
`"chat"`；客户端代码里不再有 wire 拼写映射函数。

---

## 4. R4：`qaqh-client` 出口缺 `ReasoningKind` / `Wire` / `CredentialSource`

**证据**

- `crates/qaqh-client/src/lib.rs:48-63` 的 `pub use types::{…}` 有 `UsageInfo`、`TimelineBlock`、
  `TimelineEvent`…，但没有 `ReasoningKind` / `Wire` / `CredentialSource`；
- 而 `TimelineBlock.reasoning_kind: Option<qaqh_types::ReasoningKind>` 是**公开字段**
  （`qaqh-domain/src/timeline.rs:380`），`ConfigDto.credential` 的取值也由 `CredentialSource` 定义。

客户端因此"看得见字段、拿不到类型名"：要么 match 时不写类型名，要么直接 path 依赖
`qaqh-types`。TUI 选了后者（`Cargo.toml` 里新增 `qaqh-types` 依赖并注明原因，因为
`ReasoningKind::is_displayable()` 是显示规则的单一真源，不能手抄）。

**建议**：补进 `pub use types::{…}` 或 `pub use qaqh_types::{ReasoningKind, Wire, CredentialSource};`
——`qaqh-types` 本来就是 `qaqh-client` 的依赖，零成本、零新代码。

**验收标准**：`qaqh-tui` 可以删掉 `qaqh-types` 直接依赖，且 `cargo check` 通过。

---

## 5. R5：`dsaccount.verify` 不在 `methods::ALL` 里，且"反向穷举闸"是空转

**证据**

- 方法表：`qaqh-runtime/src/ringing/service_methods.rs:140` `"dsaccount.verify" => Some(WRITE_ADMIN)`；
- 实现：`qaqh-runtime/src/service.rs:947` `"dsaccount.verify" => Ok(self.dsaccount()?.verify())`；
- 清单：`qaqh_dsaccount::methods::ALL`（`qaqh-dsaccount/src/lib.rs:99`）只有 **9** 条，
  **没有 verify**，全仓也没有 `VERIFY` 常量（对比 `qaqh-siwc/src/lib.rs:62-81`：8 个常量与 `ALL` 完全对齐）；
- 测试：`service_methods.rs:217-234` 的两段循环**方向相同**（都是 `ALL` → `lookup`），
  所以"表里多一条不在 `ALL` 的方法"永远发现不了。`siwc` 那条（`:198-215`）注释写着"反向"，
  但 `:209-214` 遍历的仍是 `ALL`——同样空转。

**后果**

`qaqh-siwc/src/lib.rs:60` / `qaqh-dsaccount/src/lib.rs:79` 都把 `methods` 称为
"服务面方法名（daemon 方法表与 FE 共用的单一真源）"。现在这个真源少了 `verify`：
客户端若按 `ALL` 生成白名单（R1 方案 A 的推荐做法），"这把凭据还有效吗"这个动作会被漏掉
——而它恰恰是账号模式下最需要的一问（`service_methods.rs:139` 的注释自己也这么写）。

**建议**

1. 补 `pub const VERIFY: &str = "dsaccount.verify";` 并加入 `ALL`（与 siwc 的形状对齐）；
2. 把两条穷举测试的"反向"真正写出来：遍历 `lookup` 能解析、且前缀为 `siwc.` / `dsaccount.` 的条目，
   断言它们都在对应 `ALL` 里——**这才是**注释所声称的"防止表里留了没有实现的死条目 / 漏项"；
3. `verify()` 的返回形状一并进 R2 的 fixture。

**验收标准**：`qaqh_dsaccount::methods::ALL` 为 10 条；从 `ALL` 里删掉任意一条，测试必须红
（当前删了也不会红，因为反向闸没生效）。

---

## 6. 已确认**不需要**后端改动的

| 项 | 结论 |
| --- | --- |
| `TimelineEvent::UsageSampled` | 契约可用。TUI 已接线（逐字段"有才更新"，缺报字段不清零）。请**保持**"不要求回合/块已开"这一条：TUI 依靠它在该事件上不进 `dropped_missing_turn`（否则每帧采样都会被记成契约异常）。 |
| `reasoning_kind` | 一条链已通：wire → 客户端模型 → adapter → 渲染（`Encrypted`/`Redacted` 依 `is_displayable()` 丢弃，`Summary` 在头部标注）。 |
| `ToolProgress.stream` / `bytes_total` | 后端已在发（`engine_tool.rs:959` 等），TUI 侧是**自己的** `TODO(M3)`，不在本单。 |
| `qaqh-ui v=0.2` 围栏 | 渲染是客户端职责。仅记一句现状：后端 prompt 已默认 `v=0.2`，未实现渲染的客户端会退化成代码块（TUI 当前即如此），属已知且可接受。 |

---

## 7. 复现与验证入口

```powershell
# 1) 客户端出口现状（应看到 only query/action，且 into_parts 是 pub(crate)）
git grep -n "pub(crate) fn into_parts" -- crates/qaqh-client/src/endpoint.rs

# 2) 18 个方法 + 档位 + 双向穷举闸
git grep -n "siwc\.\|dsaccount\." -- crates/qaqh-runtime/src/ringing/service_methods.rs

# 3) 返回形状仍是 Value
git grep -n "pub fn status\|pub fn profiles" -- crates/qaqh-siwc/src/manager.rs crates/qaqh-dsaccount/src/manager.rs

# 4) wire 非对称
git grep -n "wire 仅允许" -- crates/qaqh-config-api/src/lib.rs
git grep -n "wire: cfg.wire.as_str" -- crates/qaqh-config/src/dto.rs

# 5) R5：verify 在方法表里、却不在 methods::ALL
git grep -n "dsaccount.verify" -- crates
git grep -n "pub const ALL" -- crates/qaqh-dsaccount/src/lib.rs crates/qaqh-siwc/src/lib.rs
```

落地 R1 之后，可用桌面端已跑通的两条流程做交叉验证：
`qaqh-desktop-app/src/lib/siwc.ts:134`（`runSignInFlow`）与 `src/dsaccount/flow.ts:51`
（`runDeepSeekSignIn`）——两者对"超时也必须 cancel""被新登录顶掉 → superseded""终态后不再轮询"
的处理，是服务面契约的既有事实，客户端实现应保持一致。
