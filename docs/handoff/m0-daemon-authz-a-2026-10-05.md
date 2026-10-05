# Handoff A：移动端 M0 daemon 设备鉴权（S0–S6 + S7 daemon 侧矩阵）

> 2026-10-05。状态：**S0–S6 实现完成 + S7 daemon 侧越权矩阵完成；e2e 25/25；已提交于
> `m0-daemon-authz` 分支、未合并进 main**。
> 代码在独立 worktree **`E:/qaqh-m0-authz`**，分支 **`m0-daemon-authz`**（基于 main `56a8e40`；
> main 已前进到 `6d1c129`，与本分支改动**零文件重叠**，合并无冲突）。
> 关联：`docs/spec-daemon-auth-devices.md`（本工作的 spec）、`docs/plan-mobile-remote-access.md`。

---

## 0. 一句话

把 daemon 的 HTTP 面从「单一全局 token + 逐 handler 手检 + 任意 lease 可读任意会话」
改造成 **admin token / device token 双身份 + scope 裁决 + 会话归属 + 扫码配对 + TLS +
challenge 下沉 + 归因穿线**。桌面壳 / daemon-CLI / TUI / 探针行为零变化（admin 全程豁免）。

---

## 1. 已定决策（勿回退）

| 编号 | 决策 | 含义 |
|---|---|---|
| A1 | **客户端 = 原生 wire** | 采 `spec-daemon-auth-devices §0` 姿态：安卓/鸿蒙原生 app 直连 daemon HTTP+SSE，**不内嵌 qaqh-client、不走 UniFFI/NAPI**。与 `plan §5` 的「共享 Rust core」路线**冲突**——本工作采 spec。后果：S5(challenge 下沉)/S7(跨语言测试) 是 M0 硬前置。 |
| A2 | **admin 全程豁免** | scope / owns_session / challenge 降级一律豁免 admin；桌面壳(经 qaqh_client)、CLI、TUI、探针零改动。 |
| A3 | **身份由 token 反推** | 设备身份 = `SHA256(device_token)` 反查注册表；**绝不采信**客户端自报 `client_instance_id`。 |
| A4 | **v2 冻结线外追加** | 新端点/新字段一律 additive；既有帧格式不动；新 `capabilities` 字段带 `#[serde(default)]`。 |

### 1.1 spec 空白/偏离的三处自定决策（需评审确认）

1. **S5 审批应答端点**：spec §9 描述了 device 应答流 `{challenge_id, decision, payload}`，
   但 §11 **未定义 wire 端点**。本工作新增
   `POST /ringing/v2/sessions/{seed}/approvals/respond`。admin 不走此端点（维持 canonical 透传命令通道）。
2. **TLS pin 指纹口径**：spec §10 字面为「SPKI sha256」。本实现取**叶证书 DER 的 sha256**
   （`sha256:<hex>`），不引 x509 解析器；对原生端 pinning 等价可用。
3. **§9.5 远程 `trust_folder` 默认拒**：非 admin 提交的 `trust_folder=true` 在应答时**降级为 false**
   （放行单次、不永久扩界）。
4. **命令 scope 分级**（spec §4 与 §7 的矛盾）：spec §4 说「interact = 全部 commands」，
   但 §7 又要求 **view 档视察 app「先 attach 再读」**；若 attach 也要 interact，则 view 档永远
   无法建立归属、等于废档。本工作按 §7 判：**`SessionAttach` 只需 `View`**（其文档明确
   「仅建立归属、不触碰 actor」），其余命令仍需 `Interact`。落点
   `command.rs::required_scope_for_command`。

---

## 2. 文件改动清单

**新增**
- `crates/qaqh-daemon/src/axum_server/axum_impl/authz.rs` — `Identity`/`Scope`/`authenticate` 中间件/`require_scope`。
- `crates/qaqh-daemon/src/axum_server/axum_impl/pairing.rs` — `/pairing/tokens`、`/pair`、`/devices`、`/devices/{id}/revoke` + 短寿令牌表。
- `crates/qaqh-daemon/src/axum_server/axum_impl/challenge.rs` — 自宿主 `webui/src-tauri/src/challenge.rs` 移植的一次性 challenge（canonical id 不出 daemon）。
- `crates/qaqh-daemon/src/tls.rs` — 自签证书生成/持久化 + `TlsListener`（rustls）。
- `crates/qaqh-runtime/src/ringing/device_registry.rs` — 设备注册表（持久化、摘要、吊销）。
- `crates/qaqh-daemon/src/axum_server/authz_matrix_tests.rs` — **S7 daemon 侧越权矩阵**（10 个 in-crate 用例，走 `build_router` 真实中间件+handler）。
- `scripts/e2e-m0.mjs` — e2e 复跑脚本（真 daemon + 真 TLS，25 断言）。

**修改**
- daemon：`axum_server/axum_impl/{mod,command,content,control,v2,service_api,sse,timeline_api}.rs`、
  `axum_server.rs`、`server.rs`、`main.rs`（`mod tls`）、`Cargo.toml`（+rustls/tokio-rustls/rustls-pemfile/rcgen）。
- runtime：`ringing/{lease_store,mod}.rs`（`revoke_device`、导出）、
  `agent/{engine_turn,loop_core,loop_dispatch_control,loop_dispatch_tool,turn_lap_test_api}.rs`（S6 归因穿线）。
- ringing：`v2/{types,mod}.rs`（配对 wire 类型 + `Capabilities.pairing`）、`worker.rs`（内部信封 `actor`/`WorkerActor`）、`lib.rs`。
- client：`src/session.rs`（构造 `RingingV2Capabilities` 补 `pairing: false`）。

---

## 3. 验证证据

- `cargo check --workspace --all-targets`（主树时）绿；worktree 因缺 Tauri sidecar 不走全量 workspace，改用：
  `cargo check/test -p qaqh-daemon -p qaqh-runtime -p qaqh-ringing -p qaqh-client`。
- 单测：**daemon 85/85**（含 `authz_matrix_tests` 10 个）、**ringing 19/19**、**client 全过**；
  runtime **258 通过 + 1 已知基线失败**（`agent::prompt::tests::prompt_and_tool_defs_char_budget`，
  按设计恒失败，非本改动引入）。
  > 提交前复测（2026-10-05）：`cargo check -p qaqh-daemon -p qaqh-runtime --all-targets` exit 0；
  > 分 crate `cargo test` = daemon **86 passed / 0 failed**、ringing **19/19**、client **62/62**
  > （daemon 比上文记的 85 多 1，为 `authz_matrix_tests` 后续补的用例）。
- **e2e（真 daemon + 真 TLS）：25/25**。脚本 `scripts/e2e-m0.mjs`。
  复跑：
  ```bash
  # 隔离数据目录（数据根受守卫限制必须是 <USERPROFILE>\.qaqh）
  USERPROFILE='<tmp>' QAQH_DATA_DIR='<tmp>\.qaqh' ./target/debug/qaqh-daemon.exe \
      server --bind 0.0.0.0 --port 64777 --token e2e-admin-token &
  BASE=https://127.0.0.1:64777 ADMIN_TOKEN=e2e-admin-token NODE_TLS_REJECT_UNAUTHORIZED=0 node scripts/e2e-m0.mjs
  ```
  覆盖：health 免鉴权、pairing/tokens admin/401、pair 下发、令牌重放/伪造 403、device open、
  `capabilities.pairing`、devices 列表无 token 明文、view 越权(403)/admin 豁免(404)、
  attach 建归属、吊销后 401。

### 3.1 本轮发现并修复的缺口

1. **spec §4 与 §7 矛盾**（命令 scope 分级）：见 §1.1-4。
2. **v2 单流 SSE 不复查租约存活**（spec §6「吊销杀掉在途 SSE」实际不成立）：
   `handle_events_v2` 原实现只处理 `V2StreamItem`，订阅生命周期与 lease 无关——设备被
   `revoke_device` 摘除租约后，**已建立的 `/sessions/{seed}/events` 流仍继续收事件**；
   而 `sse.rs::handle_timeline_events` 每事件复查 `is_active_session`（两处不一致）。
   已按同款模式修复：v2 流循环每事件前复查租约存活，失效则下发
   `ringing.stream_terminated{code:"revoked"}` 并断流。
   **测试受阻**：难以低成本造 v2 事件（`v2_hub.subscribe/publish_*` 需真实 canonical 会话），
   故该修复目前靠「同款既有模式 + 代码走查」保证，未加自动化用例（与归因落账同一缺口）。
3. **身份↔lease 未绑定**（A3 硬化）：原 `require_lease_on_session` / `execute_command` 只校验
   调用方 `client_session_id` 活跃，未校验该 lease 是否由**本设备**建立——设备 D2 若拿到 D1 的
   cs 即可借其 lease 读写/下令。cs 不可猜且只回给所有者，风险低但违反 A3。已加
   `caller_lease_bound_to_identity`（device 的 lease 必须 `instance == device_id`），
   S1 四读面 + 命令面均接线；矩阵加 `device_cannot_impersonate_other_lease` 用例。

### 3.2 数据根污染修复（烟测/测试不再往真实 `.qaqh` 塞会话）

现象：`.qaqh/sessions` 累积一批同秒创建、`model:""`、0 消息的空会话，进入 `session.list`
干扰前端（Android 列表）。根因：Windows 数据根守卫（`platform.rs::validate_data_root_location`）
强制数据根必须是 `<USERPROFILE>\.qaqh`，把**所有**测试/烟测赶到真实根；泄漏即污染。

修复：
- `qaqh-types/src/platform.rs`：新增 `QAQH_ALLOW_TEST_DATA_ROOT` 逃生口——显式设非空且非 `0`
  时放行任意数据根（仅测试/烟测；生产不设，守卫语义不变）。
- `qaqh-daemon/src/axum_server.rs::init_session_manager()`：测试默认把数据根指向
  `%TEMP%\qaqh-daemon-test-<pid>`（调用方显式设 `QAQH_DATA_DIR` 时不覆盖）——`cargo test
  -p qaqh-daemon` 不再碰真实根（实证：跑前/跑后真实 `.qaqh/sessions` 均为 0）。
- `scripts/{smoke-g1.ps1,v2-smoke.sh,v2-compact-probe.sh,v2-content-probe.sh}`：改用隔离
  数据根 + `QAQH_ALLOW_TEST_DATA_ROOT=1`；`smoke-g1.ps1` 原先"故意跑真实根、只删自建会话、
  中途失败即泄漏"的写法已废除。

> 注：`session.list`（`QaqhService::list_sessions`）是**唯一后端源**，但它不做测试/空会话过滤
> ——各前端自行过滤导致标准不一。真正的修法是"不产生垃圾"，而非后端猜哪些是垃圾。

---

## 4. 未完成

- **S7 剩余项**：
  - **跨语言一致性测试**（Kotlin/ArkTS 对同一 wire fixture 解 open/attach/bootstrap/cursor）——
    需原生 app 侧 fixture，当前不可行。
  - **e2e 未覆盖（需真实 agent 会话）**：在途 SSE 被 `revoke_device` 切断；归因落账 `Api:device_id`。
- **审计账本 Actor 扩容**：`plan §8` 要求「audit Actor 增 client/device 字段」，`spec` 未提。
  `crates/qaqh-workspace/src/audit/v2.rs` 的 `ActorKind` **无 `Api` 变体**，且 hash 链
  （`sha256(prev_hash + canonical_json)`）对字段增删敏感——**必须 additive + `skip_serializing_if`**，
  绝不可删字段，需单列决策。
- **证书轮换文档**：`plan §10` 要求写「证书轮换 = 全部设备重扫码」；本工作只在 banner/代码注释提及。

---

## 5. 合并须知（重要）

- 主树当前有**同事的未提交改动**（agent 文件、`pending_store.rs`、`.cargo` OHOS 交叉编译、`qaqh-session/*`）。
  本工作已隔离在 worktree，主树**未被我改动**。
- **FF-only 不成立**：`engine_turn.rs` 的 `resolved_by` 改动与同事的 **causation** 改动**相邻**；
  `loop_core.rs` / `loop_dispatch_*.rs` 也在同事手里。合并需**手工解冲突**，非零重叠。
- 合并时只 stage 本清单（§2）的路径；勿 `git add -A`。
- worktree 里 `cargo check --workspace` 会卡在缺 Tauri sidecar 的 `qaqh-webui-app`（见 worktree 注记）。

---

## 6. 守门（不可回归）

- `/health` 免鉴权；`/ringing/v2/pair` 无 Bearer（自带一次性 pairing_token）；`/pairing/tokens`、`/devices*` 需 admin。
- 所有 scope/owns/challenge 降级**豁免 admin** → 桌面壳 / CLI / TUI / 探针零改动。
- `capabilities` 新字段一律 `#[serde(default)]`；新端点在 v2 冻结线**外追加**。
- 128 连接 / 16MB body 上限不变；driver 门控语义不变；首答即终（`interaction_already_resolved`）。
- discovery（`daemon.json`）只写 admin token，**不含任何 device_token**。
- 命令 scope **按命令分级**：`SessionAttach` 需 `View`（仅建立归属），其余命令需 `Interact`；
  在 `execute_command` 内裁决（见 §1.1-4）。admin 全豁免。
