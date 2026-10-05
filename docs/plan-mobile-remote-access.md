# 鸿蒙/安卓远程接入计划（扫码配对 + 双端原生 app）

> 状态：提案——平台决策已定（见 §0），排期未定。前置事实基于 2026-10-04 代码实证。
> 关联：`docs/audit-security-2026-10-01.md`（H3 传输安全 / M7 信任文件夹）、
> `docs/plan-permission-extraction-v2.md`（交互域合并 = 客户端归因的接缝）、
> `docs/plan-webui-tauri.md`（qaqh-client 定位为可共享客户端核心）。

## 0. 已定决策（2026-10-04）

| 编号 | 决策 | 含义 |
|---|---|---|
| D1 | **连接方式 = 扫码配对** | 桌面端出二维码，移动端扫码换取设备凭证；不做手动输 token、不做局域网广播发现。 |
| D2 | **双端原生 app** | 安卓（Kotlin）与鸿蒙（ArkTS）各为原生应用，共享 Rust 核心（qaqh-client）。不走 Tauri 移动复用，不走 Web/PWA。 |
| D3 | **安全协议是硬前置** | 安卓侧要求补齐传输安全与设备凭证体系（TLS、per-device token、scope）。前置未落地前不交付任何移动端；该前置在 daemon 侧实现，两端一体受益。 |
| D4 | **桌面端是锚点** | 两个原生 app 各自独立连接桌面侧 daemon；桌面端承担配对签发、设备管理与吊销。移动端之间不互联。 |

## 1. 现状实证（接入点已就绪，缺的是安全边界）

### 1.1 已就绪的接入点

| 层 | 位置 | 事实 |
|---|---|---|
| 协议面 | `crates/qaqh-daemon/src/axum_server/axum_impl/mod.rs:143-197` | Ringing v2 全套路由：open/leases/bootstrap/events(SSE)/commands/{channel}/timeline/content/service。纯 HTTP+SSE+JSON，握手 open→attach→bootstrap→subscribe 四步。 |
| 跨机直连 | `crates/qaqh-client/src/client.rs:60-83`、`examples/remote_fs.rs` | `RemoteEndpoint { base_url, token }` 手动直连模式已存在并被示例验证；这就是移动端连接形态的原型。 |
| 观察者语义 | `qaqh-domain/src/command.rs:70-78`、`axum_impl/v2.rs:465-467` | `SessionAttach` 显式只读 attach；`/events` 只要求 lease 不要求 attach。远程视察所需原语齐备。 |
| 远程审批 | `axum_impl/v2.rs:798-820` | 审批应答（permission/ask/plan）**刻意不经 driver 门控**：第二客户端审批是设计内行为。 |
| 客户端核心 | `crates/qaqh-client`（~5k 行） | lease 续期/重协商、SSE 解码 + cursor 跟踪、typed commands、`pending_approvals`。plan-webui-tauri.md:24-31 明确其"可共享给未来客户端"定位。 |
| UI 契约 | `webui/src/lib/transport/backend.ts:70-105` | 前端只见"typed 请求/响应 + 推流帧"抽象，payload 与 SSE 帧同形——原生 app UI 层照此契约实现即可。 |
| 多端并存 | `crates/qaqh-runtime/src/ringing/lease_store.rs` | lease 层支持任意多客户端、多 session；单 session 多客户端 attach 是既有语义。 |

### 1.2 缺口（远程控制的前置，即 D3 的内容）

1. **无传输安全**：全仓无 TLS 代码路径，明文 HTTP + Bearer；非回环 bind 需显式 token 且横幅警告"临时构建，无传输安全"（`crates/qaqh-daemon/src/server.rs:117-125, 200-205`）。这是审计 H3 收紧后的刻意姿态。
2. **单一全局 token**：无 per-client 凭证、无 scope、无吊销、无轮换；`/control/v1/stop` 仅凭 token 即可停掉 daemon（`axum_impl/control.rs:39-51`）。
3. **授权作用域不一致**：bootstrap/team/approvals 只查"任意活跃 lease"，不查 owns_session——任何 lease 可拉**任意 session** 完整 transcript（`axum_impl/v2.rs:289-306`）；命令路径同样无 owns_session 检查。本地单用户可容忍，远程场景是漏洞。
4. **归因缺失**：`InteractionResolved.resolved_by` 硬编码 `"user"`（`crates/qaqh-runtime/src/agent/engine_turn.rs:410-421`）；审计账本 Actor 只有 Agent/OS-user，无任何 client/设备字段。远程动作在全部持久化证据上不可归因。
5. **审批加固在宿主侧**：challenge 语义（opaque 一次性 id、5min TTL、active-seed scope，`webui/src-tauri/src/challenge.rs`）是 Tauri 宿主私有物，直连 daemon 的客户端拿裸 canonical id。
6. **移动生命周期**：lease TTL 30s / 续期 10s（`axum_impl/mod.rs:69-70`），手机进后台必掉 lease。

## 2. 目标架构

```
┌─ 桌面端（锚点）──────────────┐        ┌─ 安卓原生 app ──────────────┐
│ Tauri 壳（admin token）       │        │ Kotlin/Compose UI           │
│  ├ 出二维码 / 设备管理 UI      │        │  ↑ UniFFI 绑定（只见 opaque  │
│  └ qaqh-client ──┐            │  TLS   │  challenge id）             │
├──────────────────┼────────────┤  LAN   ├─────────────────────────────┤
│ qaqh-daemon      │◀───────────┼────────┼─ Rust core（qaqh-client，   │
│  ├ rustls + 自签证书           │        │  device_token + pinning，   │
│  ├ 配对端点 + 设备注册表        │        │  lease/SSE/cursor/审批代理） │
│  ├ device token 表 + scope     │        └─────────────────────────────┘
│  └ challenge 签发/归因          │        ┌─ 鸿蒙原生 app ──────────────┐
└──────────────────────────────┘        │ ArkTS UI                    │
                                        │  ↑ NAPI（napi-rs，ohos 目标） │
                                        │  └ 同一 Rust core            │
                                        └─────────────────────────────┘
```

信任边界三段式（延续桌面壳教义"token 不进 webview"）：

- **daemon**：配对签发、设备注册表、scope 裁决、challenge 签发、归因落账。
- **Rust core（qaqh-client）**：device_token 保管、lease 循环、SSE 重连、challenge 代理。token 只存在于这一层。
- **原生 UI 层**：只见 opaque challenge id 与展示字段，永不接触凭证。

## 3. 扫码配对协议设计（D1）

### 3.1 凭证体系

| 凭证 | 生命周期 | 能力 |
|---|---|---|
| pairing_token | 一次性、TTL 120s、仅可调 `/pair` | 换取 device_token |
| device_token | 长期、per-device、可吊销、带 scope | `view` / `interact` / `admin` |
| 本地 admin token | 现状不变（桌面壳、CLI、探针脚本） | 移动端永不接触 |

scope 语义：

- `view`：bootstrap / events / timeline / team / approvals(GET) / service 只读方法（session.list、meta、git.diff、todo.* 等）。
- `interact`：+ 全部 commands（会话消息、审批响应、driver claim/release）。移动端默认申请此档。
- `admin`：+ pairing token 签发、设备吊销、`/control/v1/stop`、config 写。仅桌面端持有。

### 3.2 配对流程

```text
1. 桌面壳 → daemon  POST /ringing/v2/pairing/tokens   （admin 鉴权）
   daemon 签发 pairing_token（一次性）+ 返回 TLS 证书指纹
2. 桌面壳渲染二维码，payload：
   { "v": 1, "kind": "qaqh-pair", "base_url": "https://<lan-ip>:64413",
     "pairing_token": "<one-time>", "tls_fp": "sha256:<hex>",
     "host_name": "<桌面主机名>" }
3. 移动端扫码 → 校验 kind/v → TLS 连接并 pinning 校验 tls_fp
   → POST /ringing/v2/pair  { pairing_token, device_name, platform, scope_request }
   ← { device_token, scope, device_id, daemon_version, protocol_version }
4. 移动端持久化 device_token（Android Keystore / 鸿蒙 HUKS）
5. 之后一切请求 Authorization: Bearer <device_token>；lease 语义照旧
   （clients/open 用 device_token 鉴权，client_instance_id = device_id 派生）
6. 吊销：桌面壳设备管理 UI → 删除注册表项 → 该 device_token 全部请求 401
```

要点：

- **TLS 指纹进二维码**：自签证书 + 移动端 pinning，防 LAN 内 MITM。证书持久化于 daemon 数据目录，轮换证书 = 重新扫码（文档明示）。
- 配对是**唯一**依赖扫码的时刻；日常重连只靠 device_token，无需桌面端在线配合（daemon 在跑即可）。
- daemon 重启不影响 device_token（持久化于设备注册表）；现全局 token 每次重启轮换的行为仅适用于 admin token。

## 4. 安全前置清单（M0，daemon 侧，D3 的落地项）

按风险升序，每步收尾 `cargo check --workspace --all-targets`：

1. **rustls 内建**：自签证书生成 + 持久化；`server --bind` 语义扩展为"非回环 bind 要求 TLS 就绪"；横幅文案更新。HTTP/SSE 语义不变。
2. **配对端点 + 设备注册表**：`POST /ringing/v2/pairing/tokens`、`POST /ringing/v2/pair`、设备列表/吊销（service RPC 或 control 面）。注册表持久化于数据目录。
3. **device token 验证 + scope**：`axum_impl/auth.rs` 从"单一哈希比对"扩为 token 表（admin token + N 个 device token），每个 token 携带 scope；scope 裁决做成中间层，路由标注所需最低 scope。
4. **owns_session 补齐**：bootstrap/team/approvals/commands 路径加 lease-owns-session 检查（移动端照常走 `SessionAttach` 建立 attach）。admin token 豁免（保持桌面/探针行为不变）。
5. **归因穿线**：`client_instance_id`/`device_id` 进 `InteractionResolved.resolved_by` 与审计账本 Actor。**挂靠 plan-permission-extraction-v2 Step 4（交互域合并）同批做**——触碰 v2 冻结契约，单做代价高。
6. **challenge 下沉 daemon**：`/approvals` 签发 opaque 一次性 challenge id（TTL 5min、active-seed scope），canonical `call_*`/`int_*` id 不出 daemon。桌面宿主 challenge.rs 退役为薄适配。
7. **远程审批约束**：远程 scope 来源的 `trust_folder=true` 默认拒绝（M7：永久扩写边界）；如需放开，要求 driver seat 或桌面端二次确认——执行时定案。

验收基线：`scripts/v2-smoke.sh` 在 TLS + device_token 下绿；新增越权矩阵测试（view token 拉未 attach session 的 transcript 应 403；吊销后 token 应 401；无 interact scope 的审批应答应 403）。

## 5. 移动端架构（D2）

### 5.1 共享 Rust core（qaqh-client 演进）

core 承担：open/续期/重协商（掉 lease 后 re-open + attach 重放 + bootstrap + since_cursor，现有路径）、SSE 解码 + cursor + 指数退避、typed commands、approvals/challenge 代理、TLS pinning、device_token 持久化交接。

需新增：

- `pair` 流程客户端（扫到的 QR payload → device_token）。
- 传输层从"全局 token + 明文"扩展为"device_token + pinned TLS"（`RemoteEndpoint` 演进，不另起炉灶）。
- FFI 边界：`uniffi` feature（Kotlin 绑定）与 `napi` feature（ArkTS 绑定）共用同一 core API 面；两绑定层只暴露异步方法 + 事件回调（推流帧、challenge、lease 状态）。

### 5.2 各平台原生件

| 件 | 安卓 | 鸿蒙 |
|---|---|---|
| UI | Kotlin + Compose | ArkTS + ArkUI |
| 绑定 | UniFFI → Kotlin | napi-rs → ArkTS（`aarch64-unknown-linux-ohos` Tier 2 目标） |
| 凭证存储 | Android Keystore | HUKS |
| 扫码 | CameraX + ML Kit 二维码 | Scan Kit |
| 后台续期 | 前台服务（审批器场景） | 长时任务（continuous task） |
| 通知 | NotificationManager | Notification Kit |

### 5.3 UI 契约

与 webui 的 `TransportBackend` 同构：typed 请求/响应 + 推流帧 + opaque challenge id。桌面端已验证的状态管理语义直接移植（watermark 去重、seq 缺口→快照纠偏、审批面板以 `/approvals` RPC 为唯一事实源）。

## 6. 移动生命周期策略

- **前台**：正常续期（10s 间隔）。
- **后台**：视场景二选一——视察 app 允许冻结（回前台重协商即可，重协商路径已有）；审批器 app 用前台服务保 lease。
- **应用被杀时的审批推送**：需要厂商推送（FCM / Push Kit）或常驻连接，与"局域网自持"姿态冲突。**M1/M2 不做推送**，用户进前台拉取 `/approvals`；推送列为独立决策点（涉及云依赖，另立项）。
- **多会话**：单活跃流 + 后台 15s 轮询 `session.list`（照搬桌面 tabs 模式），不为手机开并行多流（daemon 128 连接上限不必消耗）。

## 7. 里程碑

| 阶段 | 内容 | 验收 |
|---|---|---|
| M0 安全前置 | §4 清单 1-6（daemon 侧，不含移动端） | TLS+device_token 冒烟绿；越权矩阵测试绿 |
| M1 安卓视察 | 扫码配对、会话列表、只读 timeline、git/todo 视图 | 真机连真 daemon：看列表、看流、翻页、断网重连续传 |
| M1' 鸿蒙视察 | 同 M1（可与 M1 并行，Rust core 复用） | 同上（OHOS 真机） |
| M2 审批器 | approvals 面板 + 三类卡片应答 + 前台服务；trust_folder 约束落地 | 手机与桌面各答一单，归因记录区分设备 |
| M3 完整控制 | driver seat、消息输入、会话创建/关闭 | 与桌面端抢写位 UX 验收（not_driver/接管提示） |

## 8. 对现有面的改动清单

- **qaqh-daemon**：rustls、pair/pairing 路由、设备注册表、auth token 表 + scope 中间层、owns_session、challenge 签发、归因字段。
- **qaqh-ringing**：pairing/pair 端点契约（v2 冻结线**外新增**，不动既有帧格式）；capabilities 通告 `pairing: true`。
- **qaqh-client**：pair 客户端、pinned TLS 传输、uniffi/napi feature。
- **qaqh-session / audit**：`resolved_by` 扩展（随交互域合并）、Actor 增 client/device 字段。
- **webui/src-tauri**：出二维码宿主命令、设备管理 UI（挂 settings）。
- **docs**：Ringing v2 协议文档补配对章节；`docs/audit-security-2026-10-01.md` 的 H3 处置结论更新（从"关闭暴露面"改为"受控暴露面 + 设备凭证"）。

## 9. 风险与守门点

- **归因触碰 v2 冻结契约**：`resolved_by` 扩展必须与 plan-permission-extraction-v2 Step 4 同批对齐 wire 契约，勿单点先行。
- **trust_folder 远程扩界（M7）**：远程来源默认拒绝，守门测试进 M0 验收。
- **自签证书可用性**：证书轮换 = 全部设备重扫码；证书持久化路径与轮换文档写进 M0。
- **owns_session 是硬前置**：移动端把"任意 lease 看任意 session"从可容忍变成实际漏洞；该项不绿不出 M1。
- **OHOS 工具链投入**：`aarch64-unknown-linux-ohos` + napi-rs 的 CI 矩阵是新增维护面，M1' 排期前先做工具链冒烟（core 编译过 + NAPI 双向调用通）。
- **归因之外的第二审批者竞态**：首答即终（`interaction_already_resolved`）桌面手机并发审批时，输方 UI 要按 typed verdict 收敛，不重试。
- **推送**：列为独立决策点（云依赖 vs 局域网自持），不阻塞 M0-M2。
