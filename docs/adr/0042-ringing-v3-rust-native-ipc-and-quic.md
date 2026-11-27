# ADR-0042 RingingV3：Rust Native 二进制多路流（本地 IPC + 远端纯 QUIC）与反腐迁移铁律

状态: `已采纳 · 部分实施` · 初始决策 2026-11-25 · r2 修订 2026-11-26（纳入 5/14–7/31 创世初心与 9/21–10/30 V2 历史考古、七条零遗留设计铁律、六条反腐迁移纪律）· r3 修订 2026-11-26（采纳 `V4_6_DECISION_PACK` 二审定案：1C′ 纯原语拆分、`skip_serializing_if` 零命中红线、Windows Named Pipe Owner-Only DACL、独立无版本 `daemon.endpoint`、尾窗 `pinned_compaction`/`timeline_epoch`、`yamux` 量化延迟与细粒度静态分发异步门面）· **r4 修订 2026-11-26（协议深度复核定案：对外由裸 `quinn` 升级为 `iroh` 纯 QUIC + `EndpointId` 公钥寻址/NAT 打洞/加密 Relay；对内坚持 `UDS/Pipe + yamux` 并经薄 `MuxTransport` 与 `iroh::Connection` 同构、否决 `remoc`；桌面与移动端统一坚持裸 `postcard` + 机器指纹严格同版 Fail-Closed；显式钉死四流因果分流与跨流时序铁律）** · **r5 修订 2026-11-27（本地传输路线复核定案：澄清 r4 为“本地 `yamux` / 远端 `iroh` 平行互斥、零嵌套”而非 `yamux over QUIC`；维持本地 `UDS/Pipe + yamux`，将“本地亦全走 `iroh` 回环”记为附切换触发条件的被否方案；新增 `yamux` 工程约束、`MuxTransport` 双后端不变量矩阵与 PTY 一帧延迟门槛）** · **r6 修订 2026-11-27（P3-5c 实施补充：本地 UDS/Named Pipe 传输、`V3Client::connect_local` 与三后端不变量矩阵落地；生产 daemon 接线仍待 P3-5d，整体状态保持“未实施”）** · **r7 修订 2026-11-27（P3-5d 实施补充：`v3_bridge/RealBackend`、SQLite 持久回执、会话投影、Shell/Config/Account/Pairing/Workspace/Marketplace/Gitx/Fs/System 真门面与 daemon 本地 IPC 生产启动/停机收摊已落地；整体状态改为“部分实施”，广域 QUIC、GPUI 原子迁移与 Windows 真实运行仍待后续验收）** · **决策人：仓库所有者**

> 一句话：随着默认桌面前端切换为纯原生 `../qaqh-gpui`（零 WebView、10k 时间线实测 RSS ~153 MiB）、移动端保持 Rust `qaqh-client` 核心 + FFI 薄壳，本仓决定在 **`v4.6`–`v4.9`** 落地 **RingingV3**——以零 `serde_json` 依赖的纯 Rust 强类型二进制多路流（`postcard`）彻底取代 HTTP + SSE + WebSocket 三拼及所有 `/v1` 孤岛端点；本地走 **Named Pipe（Windows）/ UDS（Linux）+ `yamux`**，远端与移动端走 **`iroh` 纯 QUIC（`EndpointId` 公钥寻址 + 自动 NAT 打洞 + 端到端加密 Relay 兜底）**，两者在协议层经极薄 `MuxTransport` 100% 同构；冻结 `ADR-0008` C# 投影演进，并以硬性物理隔离与死刑倒计时在 **`v4.9` 物理立斩旧 V2 控制面**，为 `v4.12` 稳定封板与 `v5.0` 内核革命扫清一切协议债。

---

## 一、背景、动机与 9/21–10/30 V2 迁移历史考古（防重蹈覆辙）

### 1. 现状痛点
1. **旧传输层的结构性拧巴**：RingingV2 为兼容浏览器/C# 客户端，把一个天然双向的 Agent 会话协议强行拆成了三套传输——HTTP POST（JSON RPC）、SSE（单向纯文本事件推流）、WebSocket（Shell PTY），导致 `qaqh-daemon/src/axum_server`（7,413 行）与 `qaqh-client`（5,651 行）堆积了约 13,000 行协议胶水、鉴权包装与 JSON/Base64 序列化开销。
2. **Linux 8 GiB 宿主机下 Electron 不可用**：Electron（Chromium 多进程 + V8 + daemon）在 8 GiB RAM Linux 宿主机上严重吃 Swap 卡顿；消费端已全面收敛为 Rust Native（桌面 `../qaqh-gpui` + 移动 Android `qaqh-ffi` / OHOS `qaqh-harmony-napi` 均消费 `qaqh-client`）。继续维护 `qaqh-contract-export` 与跨语言投影（`ADR-0008`）属纯沉没成本。
3. **内核底盘已就绪**：`v4.3`–`v4.4` 已完成单一 Tokio Runtime、SessionOwner Tokio Actor、SQLite WAL 单权威与 `service_methods` 强类型收口；领域类型可直接复用，**无需在切协议期间重做存储层**。

### 2. 血泪历史考古（2026-09-21 → 10-30 历时 40 天、687 提交 + 11-10 P0 事故）
对 `git log` 的实证回溯揭示了团队害怕协议升级的真实根因——V1→V2 表面在 **9/24** 一天连合 8 个硬切 PR（#347–#355），实际却因“**只切外皮、不斩内脏、遍地兼容口子**”拖成 40 天拉锯战并引爆 11 月事故：

| 阶段 | 实证提交与代码锚点 | 暴露的结构性谬误 |
|---|---|---|
| **假硬切留内脏（9/24 → 10/30，拖 36 天）** | `acaea559`（删 7,942 行死 V1 执行器 + 3,693 行孤儿 crate）、`ca880251`（删 ~6,000 行 `qaqh-message` 与双存储探测）、`e2d457c4`（删 `meta.json`）、`7e4c8185`（10/05 探针盘点出 **13 条磁盘 `migrate-on-read` + 4 条 wire 别名**） | **谬误 1 & 2**：把“协议换代”与“存储 3 次换代”混在同一波，且旧内脏“等稳了再删”，导致新代码被迫长期双探测。 |
| **边切边改模型 + 别名/Value 逃生口** | `745510a7`（9/26 `seed→session_id`）、`b94bb75e`/`3f20fdd2`（10/26 推翻 driver seat 与 lease 归属）、`7a174c95`（10/30 删 40 个 `ClientV2*` 重命名墙）、`ea5e0f1f`（删 wire 别名）；**至今（11/26）仍活着的伤疤**：`qaqh-client/src/v2.rs:224-230`（`pending_approvals` 裸 `Value`“与旧 v1 保持一致”）、`service_methods.rs:651-654`（“remaining legacy RPCs untyped”） | **谬误 3**：协议未冻结就强切端，并用 `serde_json::Value` 与别名墙缓冲，导致编译器失效、临时妥协永久固化。 |
| **遗留 `/v1` 孤岛** | `qaqh-ringing/src/v2/mod.rs:35`（`/control/v1/*`、`/marketplace/v1/*`、`/health`、`/activity` 至今漂在 V2 之外） | **谬误 4**：只迁主干放过边角端点，客户端被迫维护多套 URL/鉴权/错误模型。 |
| **盲拆误伤核心（11/03 → 11/10 P0）** | 11/03 P3-D（`70d6beb8` / 报告记 `32f84029`）大扫除误删 9/20 建好的 `replay_window.rs`（-677 行），7 天后引爆 [11/10 P0 attach 活锁](file:///home/qaqtam/项目/qaqh-backend/docs/p0-2026-11-10-ringingv2-attach-livelock-report.md)（前后端同轮崩溃） | **谬误 5**：没有先用独立的“跨代协议不变量守卫集”钉死生命线语义，清理历史债时一刀误杀真需求。 |

### 3. 创世初心回溯（2026-05-14 → 07-31 RingingV1 问世与 8 月前端体验攻坚）
对更早期历史（[prehistory-continuity-S5.md](../history/prehistory-continuity-S5.md)、`d3adddbb`、`f78fd926`、`19c09ee8:PLAN.md`、`49a253a6`、`9767bb11`）的考古揭示了 Ringing 协议最初的灵魂：
- **RingingV1 之前的痛点**：从 `DSX` 单行 JSON-LP 管道（`Ui2Agent`/`Agent2Ui`）演进到 `07-26` 的单条全局 `/control/v1` WebSocket 后，10 MiB 工具洪峰、流式增量、审批、错误与连接态全堵在同一队列（触发“compact 假断联”与“前端点击发送后端阻塞”），且靠“重放 UI 事件数组”恢复状态（8/08 实测单会话 5.6 MB JSON 全量解析导致秒级卡顿）。
- **7/31 创世初心（`19c09ee8:PLAN.md`）**：明文确立“**不把 Ringing 绑定到某一种传输**”“**区分 `reliable`/`replaceable`/`ephemeral` 保护前端渲染预算**”“**大内容外置 + 权威 `Snapshot` 直供渲染**”“**尾窗分页 + `server_ts` 端到端延迟诊断**（8/08 `9767bb11`）”。
- **V2 时代的初心偏航**：HTTP+SSE+WS 三拼与跨语言金标准仪式反客为主，逼得纯原生 `../qaqh-gpui` 写下 **56 KB 的 `bridge.rs`** 去擦重连/游标/缓冲的屁股；`pending_approvals` 甩裸 `Value`、工具目录靠前端硬编码、图片走 Base64——**协议偏离了“服务前端开发与最终用户体验”的初心**。RingingV3 必须以铁律 7 把这份初心彻底请回来。

---

## 二、核心决策：RingingV3 七条零遗留设计铁律

### 铁律 1：`1C′` 纯原语拆分 + 依赖级封杀 `serde_json::Value` 与 `postcard` 禁区属性
- **严禁直接依赖带有 `serde_json`、`#[serde(tag)]`、`#[serde(flatten)]`、`#[serde(skip_serializing_if)]` 的旧契约 crate**：实查确认 `qaqh-types` 与 `qaqh-ringing-contract` 含无条件 `serde_json`、18 处 `tag`（含 6 处 adjacently tagged）、2 处 `flatten` 及 **237 处 `skip_serializing_if`**（`postcard` 为位置编码，跳过字段会导致静默错位读垃圾）。因 V2 HTTP/SSE 与 SQLite JSON 列在双栈期必须保持既有 JSON 形状，采取 **`1C′` 方案**：
  1. 在 `v4.6 P3-2` 新建零 `serde_json` 的底层纯原语 crate **`crates/qaqh-core-vocab`**（承载强类型 ID newtype、`ActivityState`、`TurnTerminal`、`UsageInfo`、`ToolCatalog` 等纯值类型），仅给 `P3-2`、`qaqh-ringing-v3`、`v3_server`、`client/v3` 使用；**冰封中的 `qaqh-types` 与 `qaqh-ringing-contract` 一行不动**（待 `v4.9` 处决 V2 时退役旧 contract 并按需收编）；
  2. 新建独立二进制协议 crate **`crates/qaqh-ringing-v3`**，仅依赖 `qaqh-core-vocab`、`serde`、`postcard`、`bytes` 等纯粹基础 crate，所有 DTO 使用干净的 externally-tagged `enum`/`struct`（线上传输长度前缀 + `postcard`；图片/附件/PTY 透传原始 `Bytes`/`Vec<u8>`，零 JSON 转义与 Base64 膨胀）；极少数动态载荷以强类型 `JsonText(String)` / `Bytes` 表达，剥离内部 `opaque` 锚点不上客户端 wire；
  3. **明确架构边界**：`v3_server` 将后端内部/存储态单向映射为 V3 DTO 属于**「单向后端→V3 投影层」**，**绝不等同于被宪法 #2/#5 禁止的「V2↔V3 转换层」**；
  4. **机器编译与静态扫描红线**：`qaqh-core-vocab` 与 `qaqh-ringing-v3` 对 `serde_json`、`skip_serializing_if`、`serde(tag`、`untagged`、`flatten` 必须 **恒为 0 命中**，`cargo tree -p <crate> -e normal,build -i serde_json` 输出为空，且对全量 DTO（覆盖 `Option::None`/空 `Vec`）强制 `postcard` round-trip 属性测试。
- 彻底取消字符串方法表与 `Value` 中转，从编译期物理封死 `pending_approvals() -> Value` 这类历史妥协口子。

### 铁律 2：协议一网打尽，零协议外 `/v1` 孤岛
- V3 单一强类型 `ControlRequest` / `ControlResponse` 必须从第一天覆盖全部 daemon 能力：
  - `System`（`Health` / `Activity` / `Version` / `Stop` / `StopIfIdle`）
  - `Session` / `Command` / `Shell` / `Marketplace` / `Config` / `Account` / `Pairing` / `Gitx` / `Fs` / `ToolCatalog`
- `v4.9` 处决 V2 后，daemon 对外**零 HTTP/SSE/WS 端口、零 `/control/v1`、零 `/marketplace/v1`**。

### 铁律 3：单连接四流多路复用（按因果独立域分流）+ `MuxTransport` 同构 + 原生多端联动一等语义
一条物理连接上按**因果独立域**划分四类独立逻辑流（`v3_server` 与 `client/v3` 经极薄 `MuxTransport` trait 统一抽象 `open_bi`/`accept_bi`/`open_uni`/`accept_uni`：远端直连 `iroh::Connection` 原生 QUIC 流；本地 IPC 与 `duplex` 测试在字节流上经 `tokio-util::compat` 叠加 **`yamux` + 独立 driver task**，单向流以半关闭双向流同构，接入前按宪法 #16 核查最新官方 API）：
- **控制流（BiStream）**：强类型 RPC 请求与响应；
- **会话事件流（每订阅会话一条 UniStream）**：按 `session_id` 隔离，消除跨会话队头阻塞。**因果定序铁律**：同一会话内的全部业务事实（含 `DeliveryClass::Reliable` 与 `Replaceable`）**必须死守同一条 FIFO 流并由 `OwnerPump` 单点分配单调递增 `seq`**，严禁将 `Reliable` 与 `Replaceable` 拆成两条独立流；合帧/节流仅在同流发送缓冲侧执行，遇到任意 `Reliable` 帧前必须先 flush 已合并的 `Replaceable` 增量再按序发出；`SessionEvents` 流是前端状态的唯一真相源，客户端严禁假设 `ControlResponse(CommandAck)` 与 `SessionEvents` 的跨流到达先后；
- **Shell PTY 流（BiStream）**：原始终端字节按单调 `offset` 透传 + 极简 `Resize`/`Closed` 控制帧，支持多端同时观看；
- **二进制 Blob 流（UniStream / BiStream）**：不可变内容寻址（`ContentHash` / `sha256`）附件、截图、大文件二进制分片直传，零 Base64 膨胀（因果靠“先落盘/上传完成、再在事件/控制流引用”保证）。
- **原生多端联动下沉进协议层**：多设备同看同一会话/同一 Shell、Driver 席位变更由 `OwnerPump` 单点裁决并写入单调 `seq` 广播 `DriverChanged`（非 Driver 端发 `SendPrompt` 当场 Fail-Closed 拒绝）、设备吊销即时按 `EndpointId` 拒连并踢流、移动端 `iroh` QUIC 0-RTT 与 Wi-Fi↔5G 连接迁移（彻底断联后按 `since_seq` 追 `ReplayRing`，超环再拉尾窗快照），不再靠 HTTP 轮询打补丁。

### 铁律 4：去版本化命名 + 零别名墙 + Fail-Closed 握手（统一严格同版）与安全发现
- 严禁创建 `ClientV3*` 重命名墙（客户端直接 `pub use qaqh_ringing_v3::*`），严禁任何 `#[serde(alias = ...)]`。
- 业务域 DTO（`TimelineEntry`、`ToolCallView`、`CommandAck`、`ShellEvent`、`ToolCatalog`）**永久无版本前缀**；仅最外层线缆模块带 `v3`（`qaqh-ringing-v3`、`v3_server`、`client/v3`）。
- 传输层抽象与本地安全红线：
  - **本地（`qaqh-gpui` ↔ `qaqh-daemon`）**：Linux/macOS `UnixListener`/`UnixStream`（`$XDG_RUNTIME_DIR/qaqh-daemon.sock`，权限 `0600`）；Windows `tokio::net::windows::named_pipe`（`\\.\pipe\qaqh-daemon-<user>`）**不得依赖默认 DACL（默认会给 `Everyone` 读权限），必须显式构造当前用户 SID 的 Owner-Only Security Descriptor 并开启 `reject_remote_clients(true)`**；
  - **独立无版本发现文件（防 `v4.9` 留尾）**：V3 从第一天启用永久最终名 **`daemon.endpoint`**（固定单一非 JSON 格式避免 V3 客户端引入 `serde_json`），内置 `build_id` / `channel` Fail-Closed 校验，并预留 `iroh_endpoint_id` / `iroh_relay_url` 字段；`v4.9` 处决 V2 时整块删除旧 `daemon.json` 与 `DaemonDiscovery`（含 `ts` 导出）；
  - **远端 / 移动端（PC / Android / OHOS ↔ `qaqh-daemon`）**：**`iroh` 纯 QUIC**（底层 `noq`，内置 TLS 1.3 + Ed25519 `EndpointId` 公钥双向认证 + 自动 NAT 打洞 + 端到端加密 Relay 兜底，默认支持 n0 公共 Relay 并允许配置自托管 Relay；配对二维码携带 `daemon_endpoint_id + relay_url + 一次性 pairing_token`，授权设备表直接绑定对端 `EndpointId` 与 `AuthScope`；`v4.8` 开工前置核验 `iroh` 在 OHOS 交叉目标下与 `ADR-0039` `aws-lc-rs`/`ring` 策略的兼容性）；
  - **测试 / 调试直连**：`tokio::io::duplex` + `yamux` 内存直连，免启进程免占端口。
  - **拓扑澄清（r5）**：本地 `yamux` 与远端 `iroh` 为 `MuxTransport` 下**平行互斥**的两个后端，**任何路径上都严禁 `yamux`/`chmux` 等复用层叠加在 QUIC 流之上（Mux-over-Mux）**；`iroh` 路径直接使用原生 `open_bi`/`open_uni` 与 QUIC 每流独立流控。
  - **本地 `yamux` 工程约束（r5 钉死）**：① 每条物理连接由**唯一 driver task** 持续推进 `yamux::Connection`（`poll_next_inbound` 驱动全部 I/O），driver 退出即整连接 Fail-Closed 关闭全部逻辑流，严禁按需/懒轮询（防 11/10 类活锁复发）；② 显式配置入站流上限，超限当场拒流而非排队；③ `yamux` 无流优先级，`BlobTransfer` 必须按不超过单流接收窗口的分块写入并在块间让出调度，保证 Control/PTY 帧可插队；④ 字节流适配统一使用 `tokio_util::compat::TokioAsyncReadCompatExt::compat()`（`Compat<T>` 同时实现读写），**严禁调用 `compat_write()` 等会命中第三节 `compat_` 反向隔离闸的 API，亦不得为此放宽红线**。
- **统一严格同版 Fail-Closed 握手（桌面与移动端均选方案 A）**：握手通过 **ALPN (`b"qaqh-ringing/3"`) + `PROTOCOL_REV: u32` + 机器从协议类型定义自动生成的契约指纹 `PROTOCOL_FINGERPRINT: [u8; 32]`**（严禁手改）强校验；**无论本地桌面端还是经 `iroh` 接入的移动端，失配当场 Fail-Closed 拒绝（绝不协商降级、绝不为旧客户端维持向后兼容字段解码）**，强制协议改动与端侧升级同步闭环，防止遗忘更新和隐性兼容债。

### 铁律 5：跨代协议不变量守卫先于业务代码（根绝 11/10 P0 复发）
在编写 V3 业务 handler 前，必须先用 `tokio::io::duplex` 建立并锁定 **`ringing_v3_invariants`** 六大红线守卫，且在 `v4.9` 删旧栈后保持 100% 绿灯：
- **`MuxTransport` 三后端矩阵（r6）**：六大守卫按 `MuxTransport` 后端参数化，必须**同时**在 `yamux` 后端（`v4.6` 为 `duplex` 内存，`v4.7` 起追加真实 UDS / Named Pipe）、`local_ipc` 后端（OS 本机字节流 + `yamux`）与 `iroh` 本机回环后端上全绿，任一守卫仅在单一后端变绿即判失败；`iroh` 在 `v4.6`–`v4.7` 仅以 `dev-dependency` 进入测试矩阵、不进入生产依赖图（生产接线仍在 `v4.8`）。以此机器兜底“本地 OS 传输、内存复用与 QUIC 行为分叉”风险，并保证远端路径在本机即被持续验证。

1. **原子订阅 + 有界回放尾**：先 grab 订阅接收器再读回放环，事件洪峰下 `resync_cycles == 0`（直接继承 11/10 P0 守卫语义）；
2. **多流背压隔离与延迟上界**：大 Blob 满载分片 + PTY 刷屏 + 会话 A 洪峰并发时，Control RPC 往返 **P99 < 50ms**、PTY 回显帧 **P99 ≤ 16.6ms**（一帧渲染预算，r5 新增）且会话 B 事件不被饿死；
3. **断联不杀进程 + 幂等回执**：断开 UDS/QUIC 不杀 exec/shell，同 `command_id` 重放返回原回执；
4. **身份单权威不可伪造**：鉴权身份唯一来自 OS Socket/Owner-Only Pipe 凭据或握手期 Device Token；**所有业务请求体禁止携带自报身份字段**；
5. **多端联动原语**：双端同看会话/Shell、Driver 交接广播、设备吊销即时断流；
6. **前端渲染预算保护（初心守卫）**：高频 `Replaceable` 增量洪峰经后端合帧/节流后不得拖慢同流 `Reliable` 审批/终态交付，大输出自动转 `BlobRef`/泄洪引用，快照尾窗分页可无全量加载拉取历史。

### 铁律 6：存储隔离 + 细粒度泛型静态分发异步门面（为 `v5.0` 铺路）
- **严禁把协议升级与存储换代混做**：`v4.6`–`v4.9` 全程复用 `v4.4` 已落地的 SQLite WAL 单权威，禁止新增磁盘格式或 `migrate-on-read`。
- **细粒度并发隔离与静态分发**：
  - `v3_server` 不得把 9 个域塞进单个“上帝 Facade trait”，必须按锁边界与生命周期细分为可组合的纯 `async` Facade Trait（`SystemFacade` / `ConfigAccountFacade` / `MarketplaceFacade` / `GitxFacade` / `FsFacade` / `SessionFacade` / `ShellFacade` / `BlobFacade`）；
  - 采用**泛型静态分发**（`V3Server<B>`，配合 `duplex` 零装箱注入）避开原生 `async fn in trait` 非 `dyn`-safe 返工；
  - `ShellFacade` 配合独立并发句柄表直接持有 `Arc<ShellHandle>`（消灭 `L-11`；注：`v5.0` 切 Per-Root Actor 时 Shell/Exec 归属将按需随 root 同步调整），`BlobFacade` 走全异步 `Db::write`（消灭 `L-12`）。

### 铁律 7：前端第一与用户体验至上（不忘 7/31 创世初心，绝不折磨前端开发）
- **开箱即渲染（View-Ready Snapshot + 严格无重复分页）**：
  - `SessionSnapshot` 从 V3 第一天起原生内置尾部窗口（`tail_blocks` 回退回合边界），**保留的最近 compaction 摘要放入独立 `pinned_compaction` 字段**（绝不混入 `timeline`，免前端翻页重复去重），同时为 `turns` 列表设明确尾部窗口上限；
  - 向上翻页走独立强类型 **`GetTranscriptPage { session_id, before_ord, limit, timeline_epoch }`**（一次性返回该页 `blocks` 与关联 `ToolCallView`，明确跨页工具调用归属；`truncate_from_turn` 递增 `timeline_epoch`，游标失配 Fail-Closed 返回 `StaleCursor` 触发重拉快照）；
  - `ToolCatalog`、`PendingApprovals`、`Diff` 与会话检索一次给齐强类型视图，禁止让前端拼多接口或硬编码规则。
- **交付分级保护前端 60/120fps 帧率预算**：V3 事件帧显式区分 `DeliveryClass::Reliable`（必达终态/审批/错误）与 `DeliveryClass::Replaceable`（流式文本/思考/exec 进度/用量，允许后端按 16–33ms 预算合帧或尾窗截断），大输出统一转 `BlobRef`/权威日志引用，绝不把高频洪峰直砸 UI 主线程。
- **消费端零胶水硬标尺**：`qaqh-client::v3` 吸收全部发现（`daemon.endpoint`）、重连、QUIC 迁移、回放尾追帧与命令幂等逻辑，对外只暴露极简原生 async Stream/Handle；`v4.7` 验收 `../qaqh-gpui` 切 V3 时，以 **`qaqh-gpui-wire/bridge.rs`（现 1,316 行）体积腰斩以上** 与 **GPUI `Cargo.toml` 对 `qaqh-ringing-contract`/`qaqh-types` 引用清零** 为硬判据（沿用 `ADR-0028` 净行数判定纪律——让前端更难写的协议一律判失败）。
- **顺应最先进原生前端范式**：原始 `Bytes` 图像直喂 GPU 纹理（零 Base64）、原样 PTY 字节直喂 `alacritty_terminal`、结构化原生组件取代 HTML iframe。
- **内置轻量端到端延迟标尺**：事件帧头保留 `server_ts_ms: u64`（`postcard` varint 极简开销，继承 8/08 诊断初心），支撑前端精确观测模型/daemon/传输/渲染四段耗时。

---

## 三、反腐迁移纪律（6 条机器可执行红线）

1. **物理隔离建新厂 + CI 反向隔离闸**：
   - 新建独立目录 `crates/qaqh-core-vocab/`、`crates/qaqh-ringing-v3/`、`crates/qaqh-daemon/src/v3_server/`、`crates/qaqh-client/src/v3/`；
   - CI/本地提交前强制校验：上述目录内 `rg 'axum_server|RINGING_V2|v2_stream|serde_json|skip_serializing_if|serde\([^)]*(tag|flatten|untagged)|serde\(alias|legacy_|compat_'` **必须恒等于 0 命中**，且 `cargo tree -p qaqh-core-vocab -p qaqh-ringing-v3 -e normal,build -i serde_json` **输出为空**。
2. **先冻协议（`v4.6`），再切端（`v4.7`–`v4.8`）**：`v4.6` 完成全量 V3 枚举 + `duplex` 闭环 + 6 条不变量绿灯后，迁移期禁止再推翻核心归属/席位/鉴权模型。
3. **双栈期 V2 绝对冰封 + 2 版本死刑倒计时**：
   - 自 `v4.6` 起 V2 只读冰封，所有新功能只进 V3、严禁倒灌 V2；严禁写任何 `V2-over-V3` / `V3-to-V2` 转换层（宪法 #2/#5）；
   - 双栈仅允许在 `v4.7`（切 GPUI）与 `v4.8`（切移动端）两版存在，**绝不拖延到 `v4.12`**。
4. **消费端单 PR 原子切换，零回退开关 + GPUI 零旧契约门禁**：`v4.7` 切 `../qaqh-gpui`、`v4.8` 切 `qaqh-ffi`/`qaqh-harmony-napi` 时，同 PR 物理删除旧 V2 调用代码，禁止留 `QAQH_USE_V2=1` 类回退开关；`v4.7` 合入后强制校验 `../qaqh-gpui/Cargo.toml` 对 `qaqh-ringing-contract` / `qaqh-types` 引用为 0。
5. **`v4.9` 首张 PR 整目录处决 V2**：物理 `git rm -rf` 删除 `crates/qaqh-daemon/src/axum_server/`、`crates/qaqh-ringing/src/v2/`、`crates/qaqh-ringing-contract/`、`crates/qaqh-client/src/{v2.rs,v2_stream.rs,sse_decoder.rs,shell_stream.rs}`、`tools/qaqh-contract-export/` 与 `daemon.json`/`DaemonDiscovery`；验收以两道机器闸为准：① `rg 'axum_server|RINGING_V2|/ringing/v2|/control/v1|/marketplace/v1|v2_stream' crates/` **0 命中**；② `ringing_v3_invariants` 全绿。
6. **冻结 `ADR-0008` 与旧跨语言仪式**：不再重写 `tools/qaqh-contract-export/src/cs_backend.rs`；旧前端留在既有冻结线（宪法 #11）。

---

## 四、被否方案记录（防重提，宪法 #10）

1. **在 QUIC 之上再套一层 HTTP/3（`h3`）**：两端均为自有 Rust 代码，套 HTTP/3 引入冗余 Header/QPACK/状态码转换层，违背宪法 #2/#5。
2. **保留 HTTP+SSE+WS 与 RingingV3 长期双栈并存至 `v4.12`**：直接重演 9/21–10/30 的 40 天泥潭，制造双维护与行为分叉；双栈上限锁死为 2 个版本并在 `v4.9` 处决。
3. **继续完成 `ADR-0008` 第 3 步重写 C# 投影机**：主前端已切 `qaqh-gpui`，属沉没成本陷阱。
4. **在 `v4.6` 硬将 PTC 沙箱运行时接进旧 `session_loop.rs`（`fc1ed287` 实测 2,887 行）**：违背宪法 #8（上帝文件止血），且 `v5.0` 重写 Effect 状态机与全异步工具时必 100% 返工；改为 `v4.11` 独立选型验证、`v5.0` 一次性接入。
5. **本机跨进程共享内存直读后端可变消息上下文**：本机 UDS+`postcard` 为 10–50 µs 级，远低于渲染预算（8.3–16.6ms），直读内存堆指针却引入高危 `unsafe`、布局耦合与 `truncate_from_turn`/扩容 `SIGBUS` 撕裂风险，且与移动端双轨分叉。
6. **在 `v4.6` 首版加入明文 `InspectModelContext` Control RPC**：调 `assemble_context()` 须在 2,887 行上帝文件 `session_loop.rs` 接线，且对外暴露明文 system prompt 与 PR #46 无明文 `cache_diag` 方向相悖；推迟至 `v5.0` Effect 状态机落地后再评估。
7. **在 `BlobInfo` / `ExecLogRef` 预留仅本地生效的 `local_readonly_path: Option<PathBuf>` 后门**：同一字段语义随传输分支、破坏跨端一致性，且 `ADR-0035` exec 日志运行期持续增长并受 GC 竞态影响；本地与远端统一走 `BlobTransfer` 流。
8. **对内采用 `remoc`（`chmux` + 过程宏远程 trait/channel 全家桶）替代 `UDS/Pipe + yamux`**：对外引入 `iroh` 后，`remoc` 要么导致内外分叉两套协议、要么在 QUIC 流内再叠 `chmux`（Mux-over-Mux）废掉 QUIC 原生独立流控；且远端 trait/channel 句柄绑定单次连接生命周期，不利于移动端断线重连追帧、FFI 边界控管与未来协议平滑演进。
9. **为移动端引入可跳未知字段的宽松跨小版本编解码（如 `postbag` / 整数 tag CBOR）**：虽然允许移动端滞后发版，但容易在演进中遗忘同步更新客户端并滋生隐性兼容债；所有者明定桌面与移动端统一坚持裸 `postcard` + 源码级 `PROTOCOL_FINGERPRINT` 严格同版 Fail-Closed（方案 A）。
10. **本地与远端统一全走 `iroh`/QUIC（本机回环）、砍掉 `UDS/Pipe + yamux`（r5 · 附切换触发条件的被否方案）**：先澄清该方案 ≠ `yamux over QUIC`（r4 拓扑为平行互斥两后端、零嵌套；Mux-over-Mux 已由第 8 条否决）。**否决理由**：① 默认用户为 8 GiB 级桌面宿主机，本地高频 PTY / 时间线流量每包须承担 UDP 协议栈 + TLS 1.3 加解密 + ACK/拥塞控制状态机 + `iroh` 路径探测后台任务开销，而 UDS/Pipe 仅一次内核拷贝；② 本机无丢包，QUIC“丢包不致队头阻塞”的核心红利在本地为零，本地真实需求仅为跨流公平与每流独立窗口，`yamux` 足以覆盖；③ Windows 本机 UDP 回环易受防火墙/安全软件干扰，属 CI 不可验的真机风险（宪法 #13）；④ UDS/Pipe 发现为固定路径，无动态端口与端口占用竞争。**如实记录非决定性因素**：安全性不构成决定理由——`iroh` `EndpointId` 双向公钥认证 + `0600` 私钥 + 授权白名单的强度不弱于 OS 凭据；“省胶水代码”亦不构成决定理由——薄 `MuxTransport` 在两方案下均需实现。该方案“单引擎、内外行为零分叉”的核心价值改由铁律 5 双后端矩阵机器兜底。**切换触发条件**（任一触发即整体改走本方案，同 PR 物理删除 `yamux` 及其适配代码，严禁以调参/补丁续命，并另立本 ADR 修订留痕）：(a) `v4.6` `P3-4` 量化基准中 `yamux` 后端未达铁律 5 第 2 条门槛（大 Blob 满载下 Control RPC P99 < 50ms、PTY 回显 P99 ≤ 16.6ms）；(b) 双后端矩阵出现仅 `yamux` 后端失败、且根因归属 `yamux` 自身语义（非本仓代码缺陷）的守卫红灯。

---

## 五、与 `v4.5`–`v4.12` → `v5.0` 路线图的衔接

- **`v4.5` / `v4.5.1`（已完成）**：内核清债（#39–#44）、SQLite 盘满防丢/瘦身/并发减负（#45）、前缀缓存修复与无明文 `cache_diag`（#46）已全量合入 `main`。
- **`v4.6`（`P3-0` 清债 → 安全阀 + V3 协议冻结）**：`P3-0` 清剿 `session_loop.rs` 剩余 16 处 `.await.ok()` 与 2 处回滚泄漏 + 修绿 `tool_surface_schema_shape`；`P3-1` 落地 `ADR-0035` 泄洪闸（`spill_gate.rs`）+ `ADR-0034` 冒烟；`P3-2` 立 `qaqh-core-vocab`（0 `serde_json`）+ 收口 `tool_mode.rs` + `ToolCatalog`；`P3-3` 冻结 `qaqh-ringing-v3`；**`P3-4` 落地 `v3_server`/`client/v3` 骨架 + `MuxTransport`（对齐 `yamux` 与 `iroh::Connection`）+ `tokio::io::duplex` 闭环 + 6 条跨代不变量守卫（`yamux`/`iroh` 回环双后端矩阵 + `yamux` 量化切换门禁，见第四节第 10 条）+ 2 份前置 ADR**。
- **`v4.7`（V3 本地 IPC + GPUI 首切 + 依赖收口）**：落地 Linux UDS + Windows Named Pipe（Owner-Only DACL + `reject_remote_clients(true)`）+ `yamux` + `daemon.endpoint`；`../qaqh-gpui` 原子切换至 RingingV3（`bridge.rs` 腰斩、CI 强制零旧 contract/`qaqh-types` 依赖、解决 `L-11`/`L-12`）；拔除 `v4.4` 临时 `sidecar.rs` 并统一 `ureq → reqwest` 与 SHA-256。
- **`v4.8`（V3 `iroh` 纯 QUIC + 原生多端联动 + 移动端切流）**：落地 **`iroh` 纯 QUIC**（`EndpointId` 公钥寻址 / 自动 NAT 打洞 / 端到端加密 Relay 兜底与自托管配置 / 0-RTT / 连接迁移 / ALPN + 严格同版指纹校验；前置核验 OHOS `ADR-0039` TLS 栈兼容）与多端同看会话/Shell/Driver 单点裁决广播；Android `qaqh-ffi` 与 OHOS `qaqh-harmony-napi` 原子切换至 V3。
- **`v4.9`（物理立斩 V2 · 纯血 Rust Native 里程碑）**：整目录物理删除 `axum_server`、`qaqh-ringing-contract`、旧 V2 路径、`daemon.json` 与 `qaqh-contract-export`；补齐 `../qaqh-gpui` Linux + Windows 双桌面端一等交付。
- **`v4.10`（纯原生富体验 & 历史 ADR 收账）**：无 WebView 原生可视化（取代 HTML iframe `widget-directive`）；接线 `ADR-0024` 硬件 MCP；落定 `audit`/`JOURNAL_IO` 归属；突破 256 条会话检索限制。
- **`v4.11`（`rust-nightly` 烧机 + 8 GiB 级压测 + `v5.0` 边界预演）**：切换 `rust-nightly` 留足 2 版烧机期；8 GiB RAM 全链路压测；独立验证 PTC 沙箱选型（`rquickjs`/`Starlark`/`Monty`，不碰 `session_loop.rs`）；冻结 `v5.0` 内核架构 ADR。
- **`v4.12`（4.x 终极稳定封板）**：全面功能冻结，全平台打包验收，确立 `v5.0` 内核大重写前的纯血 Rust Native 稳固基线。
- **`v5.0`（内核革命）**：一次性完成 `SessionLoop` Effect 驱动状态机 + 全异步工具 + Per-Root Actor + PTC 正式接入。

---

## 六、实施进度（按切片滚动登记；状态字段为 `已采纳 · 部分实施`）

> **纪律提醒（宪法 #10）**：协议**契约**冻结 ≠ 协议**已实施**。本地生产接线已落地，但广域 QUIC、GPUI 原子迁移与 Windows 真实运行尚未完成；以下逐条只登记已冻结/已落地的事实与其验证命令。

### `v4.6 P3-2`（2026-11-27）：原语底座

- `crates/qaqh-core-vocab` 落地：依赖面 `serde`/`thiserror`/`hex`；
  `cargo tree -e normal,build -i serde_json` → `did not match any packages`。
- 三档 `ToolMode`（`Core`/`Standard`/`Custom`）+ 压测档 `minimal:b/c` 当场拒绝；
  只读 `ToolCatalog`（**不带任何 JSON Schema**，分组＝权限类别，禁用原因判定顺序固定）。
- 详见 [ADR-0045](0045-core-vocab-tool-mode-and-catalog.md)。

### `v4.6 P3-3`（2026-11-27）：协议契约冻结

- `crates/qaqh-ringing-v3` 落地，依赖面仅
  `qaqh-core-vocab` + `serde` + `postcard` + `bytes` + `thiserror`（+ build-dep `sha2`）：
  ```
  cargo tree -p qaqh-ringing-v3 -e normal,build --depth 1
  ├── bytes v1.12.1   ├── postcard v1.2.0   ├── qaqh-core-vocab
  ├── serde v1.0.229  └── thiserror v2.0.21  [build] sha2 v0.11.1
  ```
  **无** `serde_json` / `axum` / `qaqh-ringing` / `qaqh-types` / `qaqh-ringing-contract`。
- **线缆帧**：`u32_le(len) ++ postcard(payload)`，单帧上限
  `MAX_FRAME_BYTES = 16 MiB`（超限在**分配前**拒绝）；一帧恰好一载荷
  （尾随字节＝`LengthMismatch`，杜绝"一帧走私两帧"）。
- **机器指纹**：`build.rs` 读 `protocol_rev.txt` 得 `PROTOCOL_REV`，并对
  `src/**/*.rs`（**排除测试文件**）的归一化文本（剥注释 + 折叠空白）算 sha256 →
  `PROTOCOL_FINGERPRINT: [u8; 32]`。源码里没有可手改的字面量。
  **反证实测**：新增一个 `struct` → 指纹变化（`3a5e87d4…` → `cbcfbc50…`）；
  只改注释 → 指纹不变（回到 `3a5e87d4…`）。
- **握手**：`ClientHello`（`client_instance_id`/`ClientKind`/凭据/`protocol_rev`/指纹）
  ↔ `ServerHello`；版本或指纹任一失配 → `HandshakeReject`（`RevMismatch` /
  `FingerprintMismatch`），**桌面与移动端同一口径，无跨小版本宽松解码**。
  凭据只有三类：本地 OS 用户 / `iroh` EndpointId / 已配对设备令牌——DTO 里没有
  "自报 client_id" 字段（铁律 5）。
- **四流**：`StreamKind::{Control, SessionEvents{session_id,since_seq},
  ShellPty{shell_id,from_offset}, BlobTransfer}`；`DeliveryClass` 是**帧属性**
  （同会话单流单 `seq`，绝不按分级拆流）；`FrameMeta{seq, server_ts_ms, delivery}`
  的服务端时钟是唯一时间权威。
- **多端联动**：`SessionEvent::DriverChanged{session_id, driver_client_instance_id, ordinal}`
  ——单点定序，`delivery()` 恒为 `Reliable`（丢了就是两台设备同时发指令）。
- **一网打尽 11 域**：`System` / `Session` / `Command` / `Shell` / `Marketplace` /
  `Config` / `Account` / `Pairing` / `Gitx` / `Fs` / `ToolCatalog`；
  `ControlCall.command_id` 是幂等键（断线重放返回原回执，铁律 4）。
- **显式排除**：无 `InspectModelContext`，无 `local_readonly_path`，无后端
  `opaque` 锚点；动态参数只有 `JsonText(String)` 与 `Bytes` 两条出路。
- **会话首屏/翻页**：`GetSessionSnapshot{tail_blocks}` 带
  `pinned_compaction: Option<CompactionSummary>`（**独立于 `turns` 窗口**）与
  `turns` 窗口；`TranscriptPage{before_ord, limit, timeline_epoch}` 一次带齐
  关联 `tool_calls`（跨页归属规则＝同 `tool_call_id` 同视图），
  `timeline_epoch` 失配一律 `ErrorCode::StaleCursor`。
- **验证**：`cargo test -p qaqh-ringing-v3` 17/17 绿
  （全量 DTO round-trip、`Option`/`Vec` 混合序列位置漂移反证、超限帧/失配版本/
  陈旧代际 Fail-Closed、`daemon.endpoint` 往返与过期守护进程拒绝）；
  `cargo clippy --no-deps -p qaqh-ringing-v3 --all-targets` 零告警。

### `v4.6 P3-4`（2026-11-27）：隔离骨架 + 双后端（`yamux`/`iroh`）闭环 + 六大不变量守卫

> 仍然**不是**「协议已实施」：V3 还不是生产线缆（生产切换在 `v4.7`）。本段只登记
> 契约两端骨架与双后端不变量守卫的实测事实（`status` 维持 **已采纳 · 未实施**）。

- **传输层独立成 crate**：`crates/qaqh-v3-transport`（`MuxTransport` trait + 单一真源
  `framing` 编解码 + `yamux 0.14.1` 后端 + `tokio::io::duplex` 闭环）。两端（`v3_server`
  与 `client/v3`）共用同一份长度前缀与 `FrameIoError::Eof` / `TruncatedPrefix` /
  `TruncatedPayload` 区分语义（消除两份重复 `framing.rs` 与干净 EOF 被误判截断问题）；
  依赖面实测仅 `log` / `thiserror` / `tokio` / `tokio-util` / `yamux` / `qaqh-ringing-v3`，
  `cargo tree -i serde_json` 无匹配。
  - 唯一 driver task（`Connection` 只活在里面）+ 入站**活流**上限（累计计数会在 64 条流后
    永久封锁，已作为反例写进注释）+ `ConnectionError::TooManyStreams` 不拖死整条连接
    + 有界接受队列；
  - `yamux` 把 SYN 挂在**第一帧**上（流在写出首帧前对端不可见），这条使用前提写进
    `open_bi` 文档并由全部守卫遵守；
  - `open_uni`/`accept_uni` 在 `yamux` 下如实 `Unsupported`，保留签名供 `iroh` 与 `v4.8` 复用。
- **`crates/qaqh-daemon/src/v3_server/`**（物理隔离目录，`#[allow(dead_code)]` 待 `v4.7` 接线；
  全部生产源码拆分为 `< 1000` 行职责单一模块）：
  - **8 个 RPITIT 门面 Trait + `V3Backend` + `SkeletonBackend`**：`SystemFacade`、
    `SessionFacade`、`ShellFacade`、`ConfigAccountFacade`、`MarketplaceFacade`、`GitxFacade`、
    `FsFacade`、`BlobFacade`，与十一域静态分派 `V3Server<T, B>::dispatch` 解耦，方便
    `v4.7` 零返工挂接真实 `QaqhService`；
  - **状态模块清晰拆分**：`state.rs`、`session_runtime.rs`（`VecDeque` 回放历史、原子回合
    占坑、原子驱动权切换与事件发射）、`shell_table.rs`（单锁原子回放+直播附着、多端同看）、
    `receipts.rs`（`(DeviceKey, command_id)` 设备级隔离 + 4096 上限 FIFO 淘汰）、
    `blob_store.rs`（256 MiB 字节上限 FIFO 淘汰）；
  - **原子订阅**：同一把锁里读回放环 + 注册 receiver（反证原语 `replay_after` /
    `register_only` 让「先读环后注册必丢帧」可确定性复现）；
  - **同流合帧**：`DeltaCoalescer` 最多持一帧未写出的 `Replaceable`，`Reliable` 到达或
    同类增量的 `offset` 断裂 / `spilled: true` 变化时先 flush（结构上排除「终态早于尾增量」
    及缺口/外置覆盖）；合帧帧带**最大 `seq`**，回放环存未合帧权威帧；
  - **身份单权威与推送鉴权**：授权键是凭据推导的 `DeviceKey`（`client_instance_id` 不参与
    授权）；`blob_pump` 仅向已认证、未吊销、拥有会话权限且广播模式下已附着该会话的连接推送
    blob；`Control` 流为连接主锚点（只允许一条且断开即触发连接收摊），吊销经 `watch` 哨兵
    `wait_for` 无竞态即时踢流；
  - **`daemon.endpoint`**：支持 `local_ipc` + `iroh` 双传输并行发布，启动期按 `PID + 序号`
    唯一临时文件原子写入与 Unix `0o600` 权限收敛，并按 `build_id` / `channel` / `rev` /
    `protocol_fingerprint` 四重 Fail-Closed 校验。
- **`crates/qaqh-client/src/v3/`**：`V3Client`（handshake + UUID 幂等键生成 + 一问一答 +
  `upload_blob`）+ `SessionEventsStream`（前端唯一真相源，无重复本地状态缓存，`into_frame_stream`
  在 `StreamRejected`/EOF 后正常结束）+ `ShellOutputStream`（起始偏移对齐 `from_offset`，非零
  偏移续读 `gaps == 0`）+ `BlobInbox`（有界 `mpsc(32)` 背压、SHA-256 完整性与单块 128 MiB
  上限 Fail-Closed）。
- **`iroh` 1.5 本机回环双后端矩阵（`ADR-0042 r5`）**：仅作为 `qaqh-daemon` 的 `[dev-dependencies]`
  引入（`default-features = false, features = ["tls-aws-lc-rs"]`，`cargo tree -p qaqh-daemon -e normal,build -i iroh`
  实测生产/构建依赖为 0，且未启用 `iroh/tls-ring`），在 `127.0.0.1:0` 以 `presets::Minimal` + `RINGING_V3_ALPN` 实例化
  `IrohTransport` 并与 `YamuxFactory` 同体参数化运行全部 6 大守卫（12 条守卫测试全绿，
  `v3_server` 37 项单测与守卫全绿，8 核 / 7876 MiB / debug 实测）：
  ① **洪峰 3000 帧中段 `attach`（`attach_seq >= 200`）**：`yamux` `attach_seq=930` / 收到 72 帧、
     `iroh` `attach_seq=1442` / 收到 47 帧，均 `resync=0`、字节守恒 21000 B、破序反证必丢 1 帧；
  ② **并发洪峰背压隔离**：
     - `yamux`（会话A 10790 帧 / PTY 974848 B / blob 24 MiB / 会话B 43 心跳）：控制面 RTT
       p99 **1.393 ms**（< 50）、PTY 回显 p99 **1.875 ms**（≤ 16.6）、会话B 帧间隔 p99 **7.779 ms**（< 50）；
     - `iroh`（会话A 59200 帧 / PTY 1951744 B / blob 40 MiB / 会话B 625 心跳）：控制面 RTT
       p99 **42.269 ms**（< 50）、PTY 回显 p99 **14.984 ms**（≤ 16.6）、会话B 帧间隔 p99 **17.474 ms**（< 50）；
  ③ **断联与幂等**：`yamux` / `iroh` 断联均即时释放驱动权、shell 存活、同 `command_id` 重放回执
     21 B 逐字节相同、回合数=1、从偏移 18 续读 17 B（`gap=0`）；
  ④ **身份单权威**：管理面 9 条对已配对设备全拒、设备间同 `command_id` 互不串话、`blob_pump`
     不向未授权设备泄密、冒充实例无效、未知令牌被拒；
  ⑤ **多端协同与吊销**：双端同看同一 Shell 一致、驱动权序号 `1 → 2 → 3` 两端一致、非驱动端两次
     被拒、吊销 2 s 内踢流并清附着/驱动权；
  ⑥ **前端渲染预算与有序性**：`yamux` 发布 2021 帧 → 收 115 帧（合掉 1906），`iroh` 发布
     2021 帧 → 收 167 帧（合掉 1854），增量字节 14000 B 全对齐，8 MiB 外置（预览 1024 B），
     代际 bump 后 `StaleCursor` 生效。
- **P3-5c 本地 IPC 与三后端矩阵（`ADR-0042 r6`）**：`qaqh-v3-transport` 新增 Unix UDS 与 Windows Named Pipe 本机传输——Unix 新建父目录 `0700`、socket `0600`、拒绝非 socket 路径、拒绝抢占活跃 socket、`peer_cred` 同 UID 校验与 drop 清理；Windows 从当前进程令牌取得用户 SID 构造 Protected Owner-Only DACL、`reject_remote_clients(true)`、首实例标志与有界 `ERROR_PIPE_BUSY` 重试。`V3Client::connect_local` 完成 `daemon.endpoint` 发现、build/channel 校验、按需自启、15 秒启动/握手超时与严格 V3 握手；六大守卫扩展为 `yamux` / `local_ipc` / `iroh` 共 18 条。Linux 实测 transport 12 项、client V3 12 项、daemon `v3_server` 43 项（含 18 条守卫）全绿；Windows MSVC transport 测试代码交叉编译通过，真实运行待 CI。
- **P3-5d 真后端与生产本地 IPC（`ADR-0042 r7`）**：`v3_bridge/RealBackend` 已接入真实 `QaqhService`、SQLite、ShellRegistry、Config/Account/Pairing/Workspace/Marketplace/Gitx/Fs/System；会话投影按需启动并以 readiness barrier 保证首事件不丢，Owner 序号从 0 开始严格校验，快照先取回放水位再读库；回执指纹改为 SHA-256，一次性 pairing 凭据不进入持久回执；生产 daemon 启动本地 IPC、发布 `daemon.endpoint`，停机统一取消并等待连接/逻辑流任务后清理端点与 socket。Linux 实测 `v3_bridge` 10 项、`v3_server` 44 项通过，iroh backpressure 守卫在全量串行运行中 PTY P99 偶发超线、隔离重跑通过；生产 daemon 启动/停机烟测通过，端点与 socket 均为 `0600` 且退出后清理。Windows 真实运行仍待 CI。
- **反腐隔离闸**：`rg 'axum_server|RINGING_V2|v2_stream|serde_json|skip_serializing_if|serde\((tag|flatten|untagged|alias)|legacy_|compat_'`
  扫描 `qaqh-core-vocab`、`qaqh-ringing-v3`、`qaqh-v3-transport/src`、`v3_server/`、`client/v3/` → **字面 0 命中**（注释内禁词示例也已清除）。
- **两份前置 ADR**：[0046](0046-native-structured-visualization-no-webview.md)（无 WebView
  原生结构化可视化，HTML iframe widget 退役，**已采纳 · 未实施**）、[0047](0047-audit-and-journal-consolidation.md)
  （`audit v1/v2` 与 `JOURNAL_IO` 保留显式单写者，不收编 `qaqh.db`，**已采纳 · 未实施**）。

### 仍未实施（留给后续切片，保持诚实）

- **前端原子切端（`v4.7 P3-5e`）**：`v3_server` 与 `v3_bridge` 本地 IPC 生产路径已于 `P3-5c`/`P3-5d` 落地 daemon，但 `../qaqh-gpui` 消费端尚未切至 `V3Client`，旧 `bridge.rs` 仍保留在前端仓库。
- **广域 QUIC / 中继链路与 V2 物理退场（`v4.8` / `v4.9`）**：`P3-4` 已通过 `iroh 1.5` 本机回环验证 QUIC 多流与反压，但中继发现、公网打洞、密钥持久化、真实移动端集成留待 `v4.8`，`axum_server` / V2 整目录物理删除留待 `v4.9`。
- **Windows / macOS 分支**：Windows Named Pipe 的当前用户 SID Owner-Only DACL、`reject_remote_clients(true)` 与测试代码已通过 MSVC 交叉编译，真实运行验收交由 Ubuntu/Windows CI；本机仅 Linux 宿主，未实测 Windows 运行行为，也未编译或运行 macOS 分支。
