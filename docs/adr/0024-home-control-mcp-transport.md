# ADR-0024 硬件控制 MCP：Rust 独立进程 + stdio + 2026-07-28 无状态协议

状态: `已采纳（部分实施）` · 决策日期 2026-11-16 · 决策人：仓库所有者
关联：ADR-0001（kernel/user-space）、ADR-0003（路由双表）、ADR-0004（strict-input）、ADR-0009（三类分类）、ADR README #1/#3

> **状态说明（2026-11-17 更新）**：server 侧已实施并在真机闭环——
> `D:\mijia-ohos\home-control-mcp` 四工具接线 + `mijia-core` 云侧读写 + 策略层
> （allowlist / 封闭动作表 / 每会话授权开关 / 静默时段 / 空调硬闸 / 会话真探活与
> passToken 静默续期（默认关）/ plan 一次性句柄 /
> 冷却 / 审计 JSONL / approvals 带外批准文件 / `reason`→pending 待审批队列）；
> `cargo test` 35 项 + 集成 3 项全绿、
> `cargo build` 零 warning，stdio JSON-RPC 真机冒烟 50 项断言全绿；另用与 `auto_lifecycle()` 逐字同构的 rmcp Auto 客户端做了 3 项集成测试（modern 零回退 + legacy 被 -32022 拒）（含一次对落地扇的
> 真实下发 + 实时回读）。**仍未做**：`qaqh-mcp` 端到端连通（§8 客户端侧）、
> `check --against docs/api-surface/v3.0` 零 wire 变化验证、era 反证与 CI 配对检查。
> 实现规格与逐项证据见同目录 `home-control-mcp/DESIGN.md`（§10/§11）。

## 背景（只写已核实的事实）

### 1. 上游协议能力（`D:\mijia-ohos`）

`com.xiaomi.smarthome` v11.9.705 的登录面与设备控制鉴权已由本仓 Python 原型 + Rust
`mijia-core` 还原并实测闭环（`协议面分析报告.md` §2/§3/§10、`mijia_proto.py`、
`mijia-core/src/{auth,cloud,crypto,session,device,mi_home}.rs`）：

- 登录：`sid=xiaomiio` → `serviceLoginAuth2`（`hash=MD5(密码).upper()`）→ `ssecurity`/
  `nonce`/`location` → `clientSign=b64(SHA1("nonce=<十进制>&"+ssecurity))` →
  `serviceToken` 在响应头 `xiaomiio_serviceToken`；`securityStatus=16` 需手机通知确认。
- 控制鉴权：`POST api.io.mi.com/app/...` 带 Cookie `userId+serviceToken`，
  `sessionSecurity=b64(SHA256(ssecurity‖nonce))` 派生 RC4 密钥，`signature=b64(SHA1(
  "POST&<path>&<字典序k=v>&sessionSecurity"))`；`/home/rpcv2/<did>` 的
  `set_properties/get_properties(siid,piid,value)`、`/device/batchdevicedatas` 为读值源。
- **鉴权模型是「账号即边界」**：持会话者拥有全账号设备完全控制权，协议层无 per-action
  能力收窄 / 审批 / 审计；`ssecurity`、`serviceToken`、`passToken` 长期明文落盘
  `session.json`。
- `mijia-core`：`Cargo.toml` package `mijia-core`，edition 2021，纯 Rust（reqwest 0.12 +
  rustls），`MiHome` 门面（`new/with_http/session/is_logged_in/reload_session/
  login_password/finish_notification`）+ `DeviceClient`；不依赖鸿蒙 API，可在 PC 编译。

### 2. 本仓 MCP 客户端现状（决定性事实）

- `Cargo.lock:3773-3774`：`rmcp 3.9.0`；`Cargo.toml:53` workspace 声明 `rmcp = "3.2"`
  （`qaqh-mcp/Cargo.toml:29-33` 注释里的 3.5.1 已过时——三处版本戳不一致）。
- `crates/qaqh-mcp/src/adapter.rs:102-107`：
  `auto_lifecycle()` = `ClientLifecycleMode::Auto { preferred_versions:
  vec![ProtocolVersion::V_2026_07_28], legacy_version: Some(V_2025_11_25) }`。
- `adapter.rs:209`（stdio）/`:239`（unix-socket）/`:287`（streamable HTTP）**三条路径都**用
  `serve_with_lifecycle(transport, auto_lifecycle())`。
- `projection.rs:35-36`：stdio → `ToolCategory::Exec`，http → `Net`；
  `adapter.rs:201-207` 有 process-wrap 进程组/Job 隔离 + `record_spawn_pid` 组杀。
- `qaqh-mcp/Cargo.toml:17-25,50-55`：生产 deps 只有 **client** features；
  `server` / `transport-streamable-http-server` 仅在 dev-deps（mock server 用）。

⇒ **客户端是 dual-era 且优先 Modern（2026-07-28），不是 legacy。** 这一条推翻了
「先做 legacy、架构为 modern 留路」的初判，直接决定了下面决策 3。

### 3. MCP spec `2026-07-28`（联网核实 modelcontextprotocol.io）

`Key Changes` 第 1/2 条：

- 移除 protocol-level sessions 与 `Mcp-Session-Id` 头（SEP-2567）；`tools/list` 等不再
  per-connection 变化；**跨调用状态改用 server-minted handles 作为普通工具参数**。
- **Make MCP stateless**：移除 `initialize`/`notifications/initialized` 握手；每请求在
  `_meta` 带 `io.modelcontextprotocol/protocolVersion` / `clientCapabilities`；版本不匹配
  返回 `UnsupportedProtocolVersionError` (-32022)。
- 新增 `server/discover`（server MUST）；移除 HTTP GET 端点 → `subscriptions/listen`；
  移除 SSE 可恢复性（`Last-Event-ID`）⇒ **断流即丢在途请求，客户端 MUST 用新 request id 重发**。
- Streamable HTTP POST 必带 `MCP-Protocol-Version` / `Mcp-Method` / `Mcp-Name`；
  安全：校验 `Origin`（无效→403）、本地只绑 127.0.0.1、认证。
- `Versioning` 术语：**Modern = 2026-07-28+；Legacy = 2025-11-25 及更早（initialize 握手）**；
  兼容矩阵 **「Modern client + Legacy server = Fails」**，且「legacy clients have no
  fall-forward mechanism」。era 是 **server 的属性**，客户端会缓存探测结果。
- 纠偏：该修订由 SEP（PR-based）/ 社区流程产出，非 Anthropic 单方提案；TS SDK v2
  （2026-07-27）与 rmcp 3.x 均已实现。用户所说「Anthropic 新版无状态」方向正确，
  仅治理归属需精确。

### 4. SDK 能力（find-docs / Context7 + 官方 README 核实）

- `rmcp` README：实现稳定 `2026-07-28`，同时完全兼容 `2025-11-25` 及更早；
  `ClientLifecycleMode::{Initialize, Discover, Auto}`；`cargo add rmcp --features server`。
- docs.rs `StreamableHttpServerConfig`：`legacy_session_mode=false` + `NeverSessionManager`
  = stateless；`stateless_protocol_metadata_required` 要求每请求带 `MCP-Protocol-Version`
  与 `_meta.io.modelcontextprotocol/protocolVersion`；建议 override
  `ServerHandler::supported_protocol_versions` 只宣告 2026-07-28+。
- TS SDK v2（`@modelcontextprotocol/server`/`client`，Node/Bun/Deno）同样实现 2026-07-28
  ⇒ 选 Node 在技术上可行，被否理由见下表（是架构判断，不是能力判断）。

### 5. 「无状态」三种含义必须分清

| # | 含义 | 是否本次采用 |
|---|---|---|
| (a) | `2025-06-18` Streamable HTTP `Session Management` 是 MAY：不发 `Mcp-Session-Id` 的「无 session HTTP」，**仍要 initialize 握手** | 否 |
| (b) | `2026-07-28` Modern：完全移除 initialize/session，per-request `_meta` | **是** |
| (c) | 应用层不缓存设备状态、每次从云端读（观测值带 source+ts） | 是（与协议无关） |

### 6. stdio 与 stateless 正交，不是二选一

2026-07-28 的 stateless 是**协议层**改动，对 stdio / Streamable HTTP 两个 binding 同样生效
（stdio 侧用 `server/discover` 探测，HTTP 侧用 modern POST）。因此「做 stdio」与
「做新版无状态」可以同时成立，本次正是二者组合。

## 决策

1. **语言 = Rust**，做成**独立进程**（crate `home-control-mcp`）。实现落在
   `D:\mijia-ohos\home-control-mcp\`，与已实测闭环的 `mijia-core` 同仓、path 依赖
   `../mijia-core`；**完全不进入** `qaqh-backend` 的 workspace 目录，因此既不改 `members`
   也不改 `exclude`，`Cargo.lock` 与 ADR-0001 §后果的五处版本戳一个字节都不动。
   （`Cargo.toml:47-49` 的 `exclude` 惯例说明「未接线 crate 不参与构建」在本仓是被认可的
   模式；若日后要把骨架移入本仓，按该惯例处理。）
2. **传输 = stdio（第一期）**。凭据（`ssecurity`/`serviceToken`）+ 物理执行不应监听端口，
   规避 spec 两版 Security Warning 点名的 DNS rebinding / `Origin` / `0.0.0.0` 绑定；
   且 stdio 路径白拿 `qaqh-mcp` 的进程组隔离、组杀、`connection.rs` idle 回收/崩溃标记/
   5s 冷却、`${secret:name}` 插值脱敏。HTTP / unix-socket 留二期。
3. **协议 era = `2026-07-28` Modern stateless only**。实现 `server/discover`、
   `supported_protocol_versions` 只宣告 `2026-07-28`；不实现 `initialize`/
   `notifications/initialized`、不发 `Mcp-Session-Id`。依据决策背景 §2：
   `auto_lifecycle()` 已把 `V_2026_07_28` 列为 preferred，Auto 客户端**直连零回退**。
4. **不写任何 era 转换层 / 兼容双路径**（宪规 9、ADR README #3）。
5. **跨调用状态用 server-minted `plan_id` 作为普通工具参数**（SEP-2567）。工具集封闭为
   `read_environment` / `list_capabilities` / `propose_action` / `execute_action`；
   `propose` 返回计划，`execute` 只执行已授权计划。与决策 3 的 stateless 天然吻合。
6. **「低风险自动执行」由策略层封闭枚举白名单判定 + 每会话用户授权开关（默认关）**：
   模型只产出结构化意图、**不判风险**；其余动作走显式审批（用户口径 q3 的严格收口）。
   工具入参 closed-set + `deny_unknown_fields`（ADR-0004 `strict-input`）与值域校验
   （`target_temp_c` 16..=30）；allowlist 把友好名映射到 `did/siid/piid`，模型永不看到 did。
7. **凭据隔离**：`ssecurity/serviceToken/passToken` 只在 server 进程内，不进工具 schema /
   日志 / 上下文，错误不回显；每次执行写审计（who/what/value/when/result/plan_id），
   幂等键 + 冷却；`set_properties` 后 `get_properties` 回读确认。因 2026-07-28 无传输重传，
   **幂等键必须自实现**，不得依赖断流重发。
8. **取值源 = 小米云** `/home/rpcv2/<did>` `get_properties` 与 `/device/batchdevicedatas`
   （用户口径 q2）。
9. **命名避开 `device`**：`RingingV2DeviceWire`（`v2/types.rs:543`）/ `/ringing/v2/devices`
   已指**配对客户端设备**；硬件侧一律用 `entity` / `actuator` / `home`。
10. **契约分类 = 零 wire 改动**：MCP 是外部能力，只在 `[mcp.servers.*]` 配置接线；
    `RINGING_V2_HTTP_ROUTES` / `CONTROL_PLANE_ROUTES` 两张表不动、`docs/api-surface/*`
    不动 ⇒ 不需 `[contract-refreeze]`、不需 bump revision，可进 LTS（ADR-0009）。
    本文按 ADR README #1/#3 留痕。

## 被否选项

| 选项 | 否决理由 |
|---|---|
| Node / TS SDK v2 | 能实现 modern（能力上可行），但要重写 RC4/nonce/签名（已有 `mijia-core`），或引 npm `mijiaAPI`（GPL-3.0 强传染、第三方逆向实现不受控）；凭据进 npm 依赖树 / Node heap；双运行时、双 SDK 版本面。官方 reference servers README 亦自述「教学用、非生产」 |
| `2025-06-18` 无 `Mcp-Session-Id` 的 HTTP（弱 stateless，(a)） | 仍要 initialize 握手；Auto 客户端会多做一次 `server/discover` 失败探测再回退；且违反决策 4 |
| legacy-only server 配 Auto 客户端 | 兼容矩阵「Modern client + Legacy server = Fails」；会诱导写转换层 |
| dual-era server（同时实现 modern + initialize） | 宪规 9 点名的「为声称兼容写转换层」，懒政；且 spec 明说 era 是 server 属性、客户端缓存探测结果 |
| 第一期就做 Streamable HTTP 无状态 | 凭据 + 物理执行不应开端口；stdio 同样可跑 modern，无收益 |
| server 加入 `qaqh-backend` workspace members | 把逆向私有协议依赖拖进 LTS `Cargo.lock`；且 `mijia-core` reqwest 0.12 与 `qaqh-mcp` 0.13 会出现双实例 |
| RingingV2 加 `/ringing/v2/hardware/*` 路由 / 新 typed method / 新 command 变体 | `[contract-refreeze]`（wire 实变），禁入冻结线，须开新线 bump revision + 反证闸门会红（宪规 18）；让 RingingV2 承载 IoT 语义是宪规 9 的「拧巴设计」 |
| 暴露通用 `call_miio` / `raw_rpc` 给模型 | 等价于交出账号全权（米家「账号即边界」弱点），违反最小权限 |

## 后果

- **`qaqh-mcp` 一行不改** —— 本选型最大收益：`Auto` 已优先 Modern。
- 脆弱点：`auto_lifecycle()` 一旦被降级 `preferred_versions`、或 rmcp 降到不含 modern 的
  版本，本 server 立刻连不上（legacy 客户端无 fall-forward）⇒ 需一条 CI 配对检查。
- 版本对齐坑：`Cargo.toml:53` 写 `3.2`、注释写 3.5.1、`Cargo.lock` 锁 **3.9.0**；
  新 crate 必须锁 3.9.0，否则出现两个 `ServiceExt`/`ProtocolVersion` 类型
  （`qaqh-mcp/Cargo.toml:29-33` 的 process-wrap 同类事故）。
- 2026-07-28 移除 SSE 可恢复性 ⇒ 物理动作幂等键与冷却必须自实现。
- 仍是逆向私有协议 + 长期明文凭据；`securityStatus=16` 通知确认需手机点「允许」
  ⇒ **不可能完全自主**，server 必须做成显式 pending 态，不得自动重试。
- 本机 4C/8T、8 GB（约 2 GB 空闲）：不跑 `--release` 全量构建，交 CI（宪规 7/10/20）。

## 落地清单

- [x] 本 ADR（选型决策固化）
- [x] `home-control-mcp` crate + `DESIGN.md` 严格设计
      （`Cargo.toml` / `lib.rs` / `actuate.rs` / `main.rs` / `cloud.rs` / `DESIGN.md`
      + `policy.json`/`policy.example.json`）；骨架轮的「未接线」状态已被下一轮取代
- [x] 锁 `rmcp =3.9.0` 并 `cargo check` 通过（2026-11-16 本机实测、零 warning）：
      首次 `version = "3.9.0"` 被 `^` 解析为 3.10.0，已改精确锁 `=3.9.0` 重编译；
      `#[tool_handler]` 显式 impl 块 override `supported_protocol_versions` 与宏不冲突
      已由编译证实（非仅文档推断）
- [x] 实现 `server/discover`、override `supported_protocol_versions` 只宣告 2026-07-28
      （真机 `server/discover` 响应 `supportedVersions=["2026-07-28"]`，无 `initialize` 路径）
- [x] 四工具接线 + strict-input DTO（`deny_unknown_fields`，实测多塞 `did` 被拒）
      + allowlist + 策略层封闭动作白名单 + 每会话授权开关（默认关）
      + 静默时段 + 空调代码级硬闸（违规配置拒绝启动）
- [x] 接 `mijia-core`：`MiHome::get_properties` / `get_properties_realtime` /
      `set_properties`（`/device/batchdevicedatas` **未采用**：body 形状未实测，
      `get_properties` 已覆盖全部取值需求，故从 `HomeCloud` trait 删除该空接口）
      + 回读比对 + plan 一次性幂等 + 冷却 + 审计 JSONL + approvals 带外批准通道
- [ ] `${secret:name}` 配置 + `qaqh-mcp` 冒烟（tools/list → read_environment →
      propose → execute）——本 ADR 唯一未动的半边：**客户端/daemon 侧尚未接线**
- [ ] `check --against docs/api-surface/v3.0` 确认零 wire 变化
- [ ] 反证：临时把 server 降级 legacy，Auto 客户端应回退探测失败（看 CI 步骤明细、
      不看 job 绿灯；宪规 18）
- [ ] 二期：HTTP / unix-socket（`legacy_session_mode=false` + `NeverSessionManager`
      + 只绑 127.0.0.1 + Origin 校验 + 认证 + POST-only，无 GET、无 `Mcp-Session-Id`）
