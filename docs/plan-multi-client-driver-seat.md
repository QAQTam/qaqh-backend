# 多客户端与 driver seat —— 现状评估与改造提案

> 2026-10-08 记录。基于当前 `main` 源码逐条核对；被删的设计文档语义回收自
> `8860285`（`docs/spec/2026-09-23-TUI-Ringing-v2冻结语义-spec.md`，2026-09-25 由
> `5b1324f` 删除）。本文只做评估与提案，不含实现。

## 0. 结论速览

- Ringing v2 **确实为多客户端设计**，但职责划分是「**观测/交互多路，驾驶单路**」。
- **6 个监听者不会被单 seat 独占**——观测走广播扇出，互不影响；被独占的是**写权**。
- 对 desk+mobile **真正不友好的**不是"单席位"本身，而是：**接管路径缺失 + `can_claim`
  语义误导 + 无空闲让位 + 席位上不带可读身份**。

## 1. 设计本意（回收自 8860285 §9）

三能力模型（§9.1）：`subscription`（能收哪些事件）/ `interact`（能答 permission/ask/plan）/
**`drive`**（能提交输入、取消、undo、workspace/session 控制 —— 即 driver seat）。

- 冻结语义第 7 条：**"driver 是显式能力，不是'谁先连上谁就是 driver'"**。
- driver 身份是 `client_session_id`，**不是** `client_instance_id`（§9.1）。
- 席位是权威事实：canonical `DriverChanged` + 单调 `driver_epoch`；bootstrap 回吐
  `{holder, driver_epoch, can_claim}`（§9.2）；claim/release（§9.3）；旧 epoch 回
  `stale_driver_epoch`。
- §9.4 约定：非 driver 时 composer/cancel/undo/workspace 进只读；**收到 `not_driver`
  须刷新 driver 状态、不得重试命令**；epoch 变化后在途旧命令不得再显示可成功。

## 2. 现状：三条路径

### 2.1 观测（subscription）—— 多路、无排他 ✔
- per-seed SSE（`axum_server/axum_impl/v2.rs:547-551`），底层 `live_tx.subscribe()` 是
  tokio `broadcast`（`qaqh-runtime/src/ringing/hub.rs:112`，容量 1024/订阅者）→ 每订阅者
  独立 receiver，**无"每 seed 单流"排他**；慢订阅者 `Lagged` 后按 `since_cursor` 重放。
- 归属记在 `session_leases: HashMap<client_session_id, HashSet<seed>>`
  （`qaqh-runtime/src/ringing/lease_store.rs`）→ 同一 seed 可被任意多个客户端 attach。
- 未见客户端数上限闸（`challenge.rs` 的 `approval_limit` 是审批挑战，非订阅）。

### 2.2 交互（interact）—— 多路、first-wins 幂等 ✔
非 driver 也能答 permission/ask/plan；抢答第二发返回 `interaction_already_resolved`
并回既有结果（`qaqh-daemon/src/axum_server.rs:1226/1598`）。

### 2.3 驾驶（drive）—— 单席位，活持有者不可抢 ✖
- `claim_driver`（`qaqh-session/src/canonical/tool_ledger.rs:346-376`）+
  `v2.rs:801-824`：holder==我 → `already_holder`；**holder 活着且 ≠ 我 → `driver_busy`
  硬拒**；仅 `stale_holder`（租约已过期）可接管。
- 命令门控 `driver_admission`（`v2.rs:945-987`）：无 holder / holder 租约已死 → **放行**；
  仅"holder 活着且 ≠ 你"拒 `not_driver`。
- 租约：TTL **30s**（`lease_store.rs:10`、`axum_impl/mod.rs:76`）、续租 **10s**
  （`mod.rs:77`）；过期惰性判定。
- 自动移交：维护循环每 **3s** 扫描（`server.rs:379,393` → `v2.rs:689-745`），仅在
  `!holder_is_live` 时发带 `expected_epoch` CAS 的 `DriverRelease`；每 seed 冷却 **15s**
  （`driver_watch.rs:23`）。

## 3. 缺口（反直觉/不利点）

1. **活持有者能永久挡位**：桌面端着但用户切到手机 → 桌面 lease 一直续 → 手机 claim
   **永远** `driver_busy`，只能读。**没有"空闲让位"**（lease 只证明进程活着，不证明在驾驶）。
2. **`can_claim` 语义误导**：bootstrap 里 `can_claim = effective_holder != caller`
   （`v2.rs:226`）→ 别人活着持有时仍返回 **true**，但 claim 必被 `driver_busy` 拒。
3. **拒绝是硬失败且无恢复接口**：`not_driver`/`driver_busy` 直接打回，spec §9.4 要求
   "刷新状态、不重试、进只读"，但**没有"请求让位/接管"接口**供前端恢复。
4. **无 latest-wins 抢占**：现状是"先到先得 + 活租约保位"，不存在"只锁最新"。
5. **席位上无身份归因**：后端**知道是谁**（`Identity`：admin/device + `device_id` +
   `scope`；`DeviceRecord.name/platform` 见 `device_registry.rs:33-48`；lease 经
   `caller_lease_bound_to_identity` 与 `device_id` 强绑定，`v2.rs:1320-1339`），但
   **席位/事件只挂不透明 `client_session_id`**，前端说不出"被谁占"。
6. **`Identity::Admin` 太粗**：桌面壳 / TUI / CLI / 探针共用 admin token，**互相不可区分**；
   `platform` 字段存在但未参与任何判定。

## 4. 提案（分步，按性价比）

### P0（纯加法，不改语义）—— 归因
在 driver 状态 / `DriverChanged` 里补 `holder_identity { kind: admin|device, device_id,
name, platform, scope }`，替代/伴随裸 `cs`。前端即可显示"被『MacBook 桌面壳』占用"。
`can_claim` 改为"claim 会被接受"的语义（或另加 `claim_blocked_by`）。

### P1 —— 同设备/管理员强夺（复用现成件）
同 `device_id` 或 `Scope::Admin` 可直接罢免：先 `revoke_device(old)`
（`lease_store.rs:188`）摘旧租约，再走已支持的 `DriverClaim{ stale_holder: Some(old) }`
CAS 接管（`v2.rs:826-840`）。解决"我的另一窗口/另一台设备接管"，不跨用户乱抢。

### P2 —— 跨端"请求让位"
新增 `driver.request`（或 claim 带 `intent=takeover`）→ 给当前 holder 推 reliable delta
`DriverTakeoverRequested` → holder 选择 release 或拒绝；可配"N 秒未响应默认让位"。

### P3 —— 空闲让位（治本）
给 holder 增加"活跃证据"（最近命令/回合/心跳），超阈值视为空闲，可被接管或自动释放。
把 lease 语义从"进程活着"升级到"在驾驶"，改动最大。

### P4 —— Admin 细分
open 时由**服务端**按入口签发 `client_kind`（`local-desktop|tui|cli|probe`），并让身份
参与接管规则（admin 罢免 device、同 device 强夺、按 platform 定制策略）。

## 5. 铁律

- **身份必须由服务端从 token 反推，绝不采信客户端自报**（`axum_impl/authz.rs:5`）。
  `name`/`platform` 仅展示归因；任何用于信任/接管判定的字段都必须服务端签发。
- 不引入裸的 last-claim-wins 抢占——两端互抢会切碎回合、抖动 epoch、交错时间线。

## 6. 未做

未跑多客户端压测；未在真实 macOS/移动端联调。以上为源码与语义核对。
