# Spec：daemon 侧设备鉴权（token 表 + scope + 会话归属）— 移动端 M0 前置

> 状态：可实现（spec 定稿待评审）。作用域：**`qaqh-daemon` HTTP 面的鉴权重设计**。
> 关联：`docs/plan-mobile-remote-access.md`（D1 扫码配对 / D3 安全硬前置 / §4 前置清单）、
> `docs/audit-security-2026-10-01.md`（H3 传输 / M7 信任文件夹）。
> 前置事实基于对当前 worktree 源码的实证核对，非采信计划文档自述。
>
> **关键约束（本 spec 的定调）**：安卓/鸿蒙作为**原生 wire 客户端**直连 daemon 的
> HTTP+SSE+JSON，**不内嵌 `qaqh-client`、不走 UniFFI/NAPI**。后果：daemon 的 HTTP 面
> 就是移动端的完整 API，安全边界 100% 落在服务端；原生 app 视同**半可信**（可改、可
> root）。这把三件事从"可延后"抬成 M0 硬前置（见 §3）。

---

## 0. 范围 / 非范围

**范围（本 spec）**
- 身份模型：admin token vs device token（`scope` + `device_id`）。
- 鉴权收口：单次解析的中间件取代散落的逐 handler 检查。
- scope 裁决 + 会话归属（owns_session）统一。
- 配对签发 + 设备注册表（持久化、可吊销、跨重启存活）。
- 服务端归因穿线（resolved_by 携带设备身份）。
- canonical id 不出 daemon（challenge 下沉）——因客户端半可信而抬为前置。
- 跨语言契约 / 越权测试矩阵。

**非范围（另立）**
- 原生 app 的 UI、扫码、Keystore/HUKS、后台续期、通知——移动端 M1/M1'。
- rustls 传输层详设（本文按依赖项 P1 引用，完整设计在 plan §4.1）。
- 厂商推送（云依赖，plan §6 独立决策点）。

---

## 1. 经核对的现状（含对 plan 的三处修正）

| 事实 | 源码位置 | 判定 |
|---|---|---|
| 单一全局 token，明文比对 | `AppState.token: String` `mod.rs:98`；`is_authorized` 对 `"Bearer {token}"` 做定长 SHA256 常数时间比对 `auth.rs:7-18`；无显式 token 时每次重启 `random_hex()` 轮换 `server.rs:169` | ✅ |
| 无鉴权中间件，逐 handler 手检 | `build_router` 只挂 `log_http_errors` 一个 `from_fn` `mod.rs:195`；**18 处** handler 各重复 `if !is_authorized(...) return unauthorized()`（control ×3、sse/content/service/timeline ×5、v2 ×10） | ✅ |
| `open` 采信客户端声明的 `client_instance_id` | `handle_open_v2` 用 `request.client_instance_id` 给 lease 建键 `v2.rs:98-103`；wire 侧 `client_instance_id`+`client_session_id` 均来自请求体 `types.rs:391-392` | ✅（§3.1 要改） |
| `/control/v1/stop` 仅凭 token，无 scope | `control.rs:39-51` | ✅ |
| 非回环明文 HTTP、无 TLS | 无服务端 TLS 依赖（daemon `Cargo.toml:25` 仅 axum）；非回环须显式 token `server.rs:117-125`；横幅自述"no transport security" `server.rs:200-205` | ✅ |
| `resolved_by` 硬编码 | `engine_turn.rs:414`（恒 `ActorKind::User`/`"user"`）；`ActorKind` 已含 `Api` 变体 `types.rs:715-721`，`ActorRef{kind,id,display_name}` + `validate_actor_ref` 已就位 `validation.rs:280` | ✅（§8） |

**修正 1 — owns_session 是"已实现、部分已接线"，不是"全局缺失"。**
`RingingLeaseStore` 早已实现 `owns_session/attach_session/detach_session/owned_sessions`
`lease_store.rs:68-113`，且**读面已有一半在用**：`timeline_api.rs:92`、`sse.rs:119/152`、
`service_api.rs:61`、`content.rs:39/220`。缺的只是 4 个只读面 `bootstrap/team/approvals/events`
（它们走 `require_v2_lease`，后者**只调 `is_active_session`** `v2.rs:1177-1184`）与命令目标会话
（`execute_command` `command.rs:77-91` 只验活跃、dispatch 到 `envelope.session_id` `command.rs:556`
时不验归属）。→ §7 是**照搬现成模式**，非新建机制。

**修正 2 — 归因不必触碰 v2 冻结契约。**
命令下发走的是**内部** `RingingWorkerCommandEnvelope`（`v2.rs:600`、`command.rs:550`），不是
客户端可见的 `RingingV2CommandEnvelope`（`types.rs:385`，冻结线）。给内部信封加 `actor` 字段即可
归因（§8），与 `plan-permission-extraction-v2` 的 wire 冻结解耦。

**修正 3 — challenge 下沉是移植，不是重写。**
宿主 `ChallengeStore`（`webui/src-tauri/src/challenge.rs`）已是完整的一次性签发器：TTL 5min
`challenge.rs:19`、`consume` 一次性 + active-seed 校验 `challenge.rs:134-142`、canonical→opaque
`issue_views` `challenge.rs:149`、反向映射 `command_for` `challenge.rs:215`，且自带单测。搬到
daemon 即可（§9）。

---

## 2. 目标鉴权架构

**三段式，admin token 全程豁免（桌面壳 / daemon-CLI / TUI / 探针行为零变化）：**

```
Authorization: Bearer <t>
   │
   ├─ authenticate 中间件（单次解析，取代 18 处手检）
   │     命中 admin token ─→ Identity{Admin}
   │     SHA256(t) 命中 DeviceRegistry 且未吊销 ─→ Identity{Device{id,scope}}
   │     否则 ─→ 401 unauthorized
   │     注入 request.extensions，供 handler 读取
   │
   ├─ require_scope(identity, min)          — scope 裁决（view<interact<admin）
   │
   └─ require_lease_on_session(...)          — 会话归属（device 需 owns_session；admin 豁免）
```

信任边界三段（原生无 Rust 中介 → 中段并入原生层，故服务端必须自足）：
- **daemon**：配对签发、设备注册表、scope 裁决、会话归属、challenge 签发、归因落账——**全部**在此层。
- **原生客户端**：保管 `device_token`、TLS pinning、lease 循环、challenge 展示；**视同半可信**，
  其自报的一切身份字段（`client_instance_id` 等）**不作信任**。

---

## 3. 因"原生不用 Rust 绑定"抬升的三条设计原则

1. **身份由 token 反推，绝不采信客户端自报字段。** `handle_open_v2` 现用请求体
   `client_instance_id` 给 lease 建键（`v2.rs:103`）。设备场景改为：daemon 依 Bearer
   `device_token` → `DeviceRegistry` 反查 `device_id`；lease 以 **device_id 派生身份**登记，
   客户端的 `client_instance_id` 降级为纯诊断、不参与任何信任判定。
2. **canonical id 不出 daemon（challenge 下沉 M0 必做）。** 没有可信 Rust 核心在 app 里握
   canonical id；若 `/approvals` 把 `call_*`/`int_*` 直接交给半可信原生 app（`v2.rs:346-410`
   现状即返回 canonical），等于把"能答任意交互"的裸 id 交出去。→ §9。
3. **契约测试替代共享 core 的收敛作用。** open→attach→bootstrap→cursor→重协商这套语义由
   Kotlin/ArkTS **各写一遍**，漂移风险大。在 daemon 侧建**跨语言一致 + 越权矩阵**测试
   （§11），对任意语言客户端跑同一 fixture。

---

## 4. 凭证体系与生命周期

| 凭证 | 存储 | 生命周期 | 能力 | 来源 |
|---|---|---|---|---|
| **admin token** | `AppState`（现状，可改名 `admin_token`） | 未显式配置则每次重启轮换 `server.rs:169` | 全权，owns/scope 全豁免 | 现状不变，桌面壳 / CLI / 探针 |
| **pairing_token** | daemon 内存（短寿） | 一次性、TTL 120s、**仅**可调 `/pair` | 换取 device_token | `/pairing/tokens` 签发 |
| **device_token** | `DeviceRegistry`（**持久**，存 SHA256 摘要） | 长期、per-device、**跨 daemon 重启存活**、可吊销 | `scope` ∈ view/interact(/admin) | `/pair` 一次性下发 |

要点：
- device_token **只在 `/pair` 响应中出现一次**，注册表只存摘要，不可再取回；丢失 = 重新扫码。
- **daemon 重启不得失效 device_token**（与 admin token 的每次轮换行为**显式区分**）。
- 吊销：删注册表项 → 摘要不再命中 → 后续请求 401；并**顺带强制失效该设备的活跃 lease**
  （见 §6，堵住"已建立的 SSE 流在吊销后仍读"）。

---

## 5. 设备注册表（`DeviceRegistry`）

新增 **`crates/qaqh-runtime/src/ringing/device_registry.rs`**，与 `driver_watch` 同层、同持久化
范式（`driver_watch.rs:42-86`：`data_dir()` 下 JSON、`.json.tmp`+rename 原子写、损坏即空启动）：

```rust
pub enum Scope { View, Interact, Admin }            // Ord：View < Interact < Admin
impl Scope { fn at_least(self, min: Scope) -> bool { (self as u8) >= (min as u8) } }

#[derive(Serialize, Deserialize)]
pub struct DeviceRecord {
    pub device_id: String,          // pair 时服务端生成（ULID/hex）
    pub name: String,               // 仅展示 + 归因，非信任
    pub platform: String,           // "android" | "harmonyos" | ...
    pub scope: Scope,               // 实际授予（可与请求不同，桌面端可降档）
    pub token_digest: [u8; 32],     // SHA256(device_token)，绝不含明文
    pub created_at_ms: u64,
    pub last_seen_ms: u64,          // 供设备管理 UI；每次鉴权命中时惰性更新
}

pub struct DeviceRegistry {
    by_digest: HashMap<[u8;32], String>,   // 摘要 → device_id
    by_id: HashMap<String, DeviceRecord>,
    persistence_path: Option<PathBuf>,     // <data_dir>/ringing-devices.json
}
```

API：`issue(&mut self, name, platform, scope) -> (device_id, device_token /*明文仅此一次*/)`、
`lookup(&self, digest) -> Option<&DeviceRecord>`、`revoke(&mut self, device_id)`、`list()`。
持有方式：`AppState` 增 `devices: Arc<Mutex<DeviceRegistry>>`，`server.rs` 装配处
`DeviceRegistry::new_persistent()`（对齐 `driver_watch` `server.rs:237`）。

**pairing_token 短寿表**（同模块或 `authz.rs`，仅内存）：
`HashMap<[u8;32], PairingGrant { scope_request, name, platform, expires_at_ms, consumed: bool }>`。
`/pair` 命中即校验未过期未消费 → 置 `consumed`（一次性）→ 调 `DeviceRegistry::issue`。

---

## 6. 身份 → lease 绑定与吊销联动

现状 lease 键 = 客户端自报 `client_instance_id`（`lease_store.rs:24-31`、`v2.rs:99-103`）。改造：
- `handle_open_v2` 从 `Extension<Identity>` 取身份，**忽略**请求体 `client_instance_id` 作信任：
  - admin：维持旧行为（键用自报 `client_instance_id`，兼容桌面/CLI/TUI）。
  - device：lease 记录 `device_id`（`LeaseEntry` 增 `owner_device: Option<String>`），
    `client_instance_id` 用 `device_id` 派生的稳定串覆盖。
- `revoke(device_id)`：删注册表 + 遍历 `leases` 摘除 `owner_device == device_id` 的条目
  （`RingingLeaseStore` 增 `revoke_device(&mut self, device_id)`）。

> 决策：吊销要能杀掉在途 SSE（不因短 TTL 自愈而留窗口）。`revoke_device` 是显式代价最小的做法。

---

## 7. 会话归属（owns_session）补齐 — 修正 1 的落地

新增单一 helper（放 `v2.rs`，复用现成 `owns_session`）：

```rust
fn require_lease_on_session(
    state: &AppState, headers: &HeaderMap, identity: &Identity, path_seed: &str
) -> Result<String, Response> {
    let caller = require_v2_lease(state, headers).ok_or_else(lease_required_v2)?; // 401 无 lease
    if matches!(identity.actor, Actor::Admin) { return Ok(caller); }              // admin 豁免
    if !state.leases.lock()...owns_session(&caller, path_seed) {
        return Err(forbidden_not_owner(path_seed));                                // 403 非归属
    }
    Ok(caller)
}
```

**接线点**（把 `require_v2_lease` 换成 `require_lease_on_session`，对照 `timeline_api.rs:92`）：
- `handle_bootstrap_v2` `v2.rs:162`
- `handle_team_snapshot_v2` `v2.rs:292`
- `handle_pending_approvals_v2` `v2.rs:354`
- `handle_events_v2` `v2.rs:465`
- 命令目标：`execute_command` 于 `command.rs:77-91` 的活跃检查后、dispatch 前，对
  `envelope.session_id` 加非-admin owns 检查（403）。
- **已接线**的 `timeline/content/service/sse` **不动**（已是 owns_session）。

状态码区分（写进测试矩阵）：无 lease → **401** `lease_required`；有 lease 但非归属 →
**403** `forbidden_not_owner`；scope 不足 → **403** `insufficient_scope`。

> `SessionAttach` 是建立归属的唯一入口（`command.rs:480-529`，仅 attach 无 actor 副作用）。
> 移动端读别人已 attach 的 seed 前须先 attach；视察 app 走 `session.list` 挑选后 attach。

---

## 8. 服务端归因穿线（resolved_by）— 修正 2 的落地

- 内部 `RingingWorkerCommandEnvelope`（`v2.rs:600`、`command.rs:550`）增
  `actor: Option<ActorRef>`（**客户端 wire 不变**）。
- daemon 依 `Identity` 填：Admin → `ActorRef{kind:User, id:"user"}`（保持现状语义）；
  Device → `ActorRef{kind:Api, id:device_id, display_name:Some(name)}`。
- `engine_turn.rs:414` 用它替硬编码，缺省（`None`）回退现状。→ 桌面/手机并发审批时账本可区分设备。
- **不新增 `ActorKind` 变体**（复用 `Api` `types.rs:717`），避免 TS 导出与 wire 破坏。

---

## 9. canonical id 不外泄（challenge 下沉）— 修正 3 的落地

- `ChallengeStore` 从 `webui/src-tauri/src/challenge.rs` **移植**进 daemon
  （建议 `axum_impl/challenge.rs`，逻辑与单测原样搬，TTL/上限/去重/一次性/scope 校验全保留）。
- `handle_pending_approvals_v2`（`v2.rs:346`）：**device 身份**返回 opaque `challenge_id` +
  有界展示 details（现 `issue_views` 产物），canonical `call_*`/`int_*` 不出 daemon；
  **admin 身份**维持现状透传（桌面宿主已在壳侧做映射，零变化）。
- 审批应答：device 走 `{challenge_id, decision, payload}` → daemon `consume`（一次性 + seed
  scope）→ `command_for` 映射回 canonical → 进 `execute_command`。应答命令**刻意不受 driver 门控**
  的语义保留（`v2.rs:802-820`）。
- 桌面宿主 `challenge.rs` 退役为薄适配（或 admin 路径继续用，M2 定）。

### 9.5 远程 `trust_folder` 默认拒（M7，plan §4.7）

`ToolPermissionRespond { approved, trust_folder }`（`command.rs:167`、宿主
`challenge.rs:222` "trust"→`(true,true)`）里 `trust_folder=true` 是**永久扩写信任边界**。
规则：非-admin `Identity` 提交的 `trust_folder=true` **降级为 `false`**（放行单次审批但**不**永久
扩界），并在审批卡片上不暴露 "trust" 选项；如确需远程永久扩界，要求 driver seat 或桌面端二次确认
（M2 定案）。落点：`execute_command` 于 dispatch 前依 `Identity` 规范化 `trust_folder`；admin 不变。
守门用例进 §13（"interact token 提交 trust_folder=true → 实际 false"）。

---

## 10. 传输安全（P1 前置，引用非详设）

- rustls + 自签证书生成并持久于 daemon 数据目录；`axum::serve`（`server.rs:345`）包 TLS
  acceptor；非回环 bind 改为"**要求 TLS 就绪**"（替换 `server.rs:117-125` 的门禁语义），
  横幅 `server.rs:200-205` 文案更新。
- 证书指纹（SPKI sha256）经 `/pairing/tokens` 响应回给桌面壳 → 进二维码 → 原生端 pinning。
- HTTP/SSE 语义不变；`v2-smoke` 在 `https://` 下跑通。
- 详设见 `plan-mobile-remote-access.md` §4.1；本 spec 视其为 device_token 生效的**硬依赖**
  （明文 LAN 下发 Bearer 无意义）。

---

## 11. 配对协议（wire，追加不改既有帧）

路由前缀沿用 `RINGING_V2_BASE_PATH`；`RingingV2Capabilities` 增
`#[serde(default)] pub pairing: bool`（对齐 `single_stream` 的兼容范式 `types.rs:51-52`），
open 响应置 `pairing: true`。

```text
1) POST /ringing/v2/pairing/tokens          [Admin]
   body: { "scope_grant": "view"|"interact", "device_name": str, "platform": str }
   resp 200: { "pairing_token": str, "expires_in_ms": 120000, "tls_fp": "sha256:<hex>" }
   注：scope 由**桌面端**决定并写入 grant；pairing_token 仅一次性、仅可换 /pair。

2) 桌面壳组 QR payload（daemon 不参与）：
   { "v":1, "kind":"qaqh-pair", "base_url":"https://<lan-ip>:64413",
     "pairing_token":"<one-time>", "tls_fp":"sha256:<hex>", "host_name":str }

3) POST /ringing/v2/pair                     [无 Bearer；消费 pairing_token]
   body: { "pairing_token":str, "device_name":str, "platform":str }
   resp 200: { "device_id":str, "device_token":str /*仅此一次*/, "scope":"interact",
               "daemon_version":str, "protocol_version":num }
   err 401/403: { "code": "pairing_invalid" | "pairing_expired" | "pairing_used" }

4) 之后一切请求 Authorization: Bearer <device_token>；lease 语义照 §6。

5) 设备管理（均 [Admin]，供桌面壳设备管理 UI）：
   GET  /ringing/v2/devices               -> { "devices": [ {device_id,name,platform,scope,created_at_ms,last_seen_ms} ] }  // 不含任何 token 材料
   POST /ringing/v2/devices/{id}/revoke   -> 204   // 删表项 + revoke_device 强失效其 lease
```

要点：配对是**唯一**依赖桌面在线的时刻；日常重连只靠 device_token（daemon 在跑即可）。
证书轮换 = 全部设备重扫码（写进配对文档）。

---

## 12. 分步实施（每步收尾 `cargo check --workspace --all-targets`）

| 步 | 内容 | 主改文件 | 风险 |
|---|---|---|---|
| S0 | `authenticate` 中间件 + `Identity`/`Scope`，注入 `Extension`，删 18 处手检（admin 路径行为不变） | `auth.rs`、`mod.rs:143`、各 handler 签名 | 低 |
| S1 | owns_session 补齐 4 读面 + 命令目标（§7），admin 豁免 | `v2.rs`、`command.rs` | 低 |
| S2 | `DeviceRegistry` + device token 反查 + scope 裁决 + open 派生身份（§5/§3.1/§6） | 新 `device_registry.rs`、`mod.rs`、`server.rs` | 中 |
| S3 | 配对端点 + 短寿表 + `capabilities.pairing` + 设备列表/吊销（§11） | 新 `pairing.rs`、`qaqh-ringing/v2/types.rs` | 中 |
| S4 | rustls + 自签持久 + bind 门禁 + 指纹（§10 / P1） | `server.rs`、`Cargo.toml` | 中 |
| S5 | challenge 下沉（§9），approvals 对 device 返 opaque | 新 `axum_impl/challenge.rs`、`v2.rs` | 中高 |
| S6 | 归因穿线（§8） | `command.rs`、`qaqh-ringing`、`engine_turn.rs` | 中 |
| S7 | 越权矩阵 + 跨语言一致性测试（§13）、`v2-smoke` device 变体 | `crates/qaqh-daemon/tests/`、`scripts/` | — |

建议顺序 **S0→S1→S2→S3→S4**；S5/S6/S7 可在 S4 后并行。**S0+S1 为最小可交付**（纯收敛 +
关越权，不引新子系统、不碰 wire），是其余项地基。

---

## 13. 验收 / 测试矩阵（device 视角，替代共享 core 的收敛作用）

`cargo test -p qaqh-daemon` + `scripts/v2-smoke.sh`（admin）+ 新增 device 变体，覆盖：

| 用例 | 期望 |
|---|---|
| pair e2e → device_token → open → attach seed → bootstrap | 200，能读 |
| view token，GET 自己已 attach 的 seed 的 bootstrap/team/approvals/events/timeline | 200 |
| view token，GET **未 attach** 的 seed 的 bootstrap | **403** `forbidden_not_owner` |
| view token，POST commands（消息/审批应答/driver claim） | **403** `insufficient_scope` |
| interact token，审批应答 | 200（不受 driver 门控，`v2.rs:802-820`） |
| 吊销后该 token 一切请求 | **401**；在途 SSE 被 `revoke_device` 切断 |
| 客户端伪造 `client_instance_id` 换 seed 归属 | 归属不变，他人 seed 仍 403 |
| pairing_token 重放 / 过期 | **403** `pairing_used` / `pairing_expired` |
| daemon 重启后旧 device_token | 仍有效（跨重启存活）；admin token 仍可轮换 |
| admin token 全路径 | 与改造前逐字节一致（owns/scope/challenge 全豁免） |
| 归因：手机答 vs 桌面答 `InteractionResolved.resolved_by` | 账本区分 `Api:device_id` / `User` |
| 跨语言一致：Kotlin/ArkTS 各对同一 wire fixture 解 open/attach/bootstrap/cursor | 断言同构（无 Rust core 兜底） |

---

## 14. 守门（不可回归）

- `/health` 维持免鉴权（`control.rs:9`）；`/ringing/v2/pair` 无 Bearer（consume pairing_token）。
- 所有 scope/owns/challenge 降级**必须豁免 admin** → 桌面壳（`webui/src-tauri` 经
  `qaqh_client` `daemon.rs:15`）、daemon-CLI（`main.rs` 手搓 HTTP `main.rs:397`）、TUI、
  `examples/remote_fs.rs` **零改动**。
- `capabilities` 新字段一律 `#[serde(default)]`（旧 client 反序列化不误伤）。
- 新端点是 v2 冻结线**外追加**，不改既有帧格式（`types.rs:3-5` additive 原则）。
- 128 连接 / 16MB body 上限不变（`mod.rs:71-72`）。
- driver 门控语义不变；首答即终（`interaction_already_resolved`）竞态下，原生 UI 按 typed
  verdict 收敛、不重试。
- discovery（`daemon.json`）只写 admin token，**不含任何 device_token**（`server.rs:187`）。

---

## 15. 文件改动清单

**新增**
- `crates/qaqh-runtime/src/ringing/device_registry.rs`（`DeviceRegistry`/`Scope`/`DeviceRecord` + 持久化 + 单测）
- `crates/qaqh-daemon/src/axum_server/axum_impl/authz.rs`（`authenticate` 中间件、`Identity`、`Actor`、`require_scope`、短寿 pairing 表）
- `crates/qaqh-daemon/src/axum_server/axum_impl/pairing.rs`（`/pairing/tokens`、`/pair`、`/devices`、`/devices/{id}/revoke`）
- `crates/qaqh-daemon/src/axum_server/axum_impl/challenge.rs`（移植自宿主）
- `crates/qaqh-daemon/tests/authz_scope_matrix.rs`（§13 越权矩阵）

**修改**
- `qaqh-daemon/.../auth.rs`：保留 `is_authorized`（admin 内部用），加 `resolve_identity`
- `qaqh-daemon/.../mod.rs`：`AppState.token`→`admin_token` + `devices: Arc<Mutex<DeviceRegistry>>`；`build_router` 挂 `from_fn_with_state(authenticate)`、加配对路由、`capabilities.pairing`
- `qaqh-daemon/.../v2.rs`：`require_lease_on_session`（§7）、`/approvals` device opaque（§9）、open 依 Identity 派生（§3.1/§6）
- `qaqh-daemon/.../command.rs`：命令目标 owns（§7）、worker 信封 `actor`（§8）
- `qaqh-daemon/.../control.rs`：stop/stop-if-idle 归 Admin scope
- `qaqh-runtime/src/ringing/mod.rs`：导出 `device_registry`；`RingingLeaseStore` 增 `revoke_device`
- `qaqh-daemon/src/server.rs`：`DeviceRegistry::new_persistent` 装配、TLS(P1)、bind 门禁、discovery 不含 device
- `qaqh-ringing/src/v2/types.rs`：`Capabilities.pairing`；配对 wire 类型（PairTokenRequest/Response、PairRequest/Response、DeviceRecordWire）
- `engine_turn.rs:414`：`resolved_by` 取 worker 信封 `actor`，缺省回退

---

## 16. 与 plan §4 的对应

plan §4 清单 1-7 → 本 spec：1=§10(S4)、2=§5/§11(S2/S3)、3=§3.1/§5/§6(S2)、
4=§7(S1)、5=§8(S6)、6=§9(S5)、7=§9.5（trust_folder 远程默认拒，M7）