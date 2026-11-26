# ADR-0042 RingingV3：Rust Native 二进制多路流（本地 IPC + 远端纯 QUIC）与反腐迁移铁律

状态: `已采纳 · 未实施` · 初始决策 2026-11-25 · **r2 修订 2026-11-26（纳入 5/14–7/31 创世初心与 9/21–10/30 V2 历史考古、七条零遗留设计铁律、六条反腐迁移纪律，并对齐 `v4.5`–`v4.12` → `v5.0` 路线）** · **决策人：仓库所有者**

> 一句话：随着默认桌面前端切换为纯原生 `../qaqh-gpui`（零 WebView、10k 时间线实测 RSS ~153 MiB）、移动端保持 Rust `qaqh-client` 核心 + FFI 薄壳，本仓决定在 **`v4.6`–`v4.9`** 落地 **RingingV3**——以零 `serde_json` 依赖的纯 Rust 强类型二进制多路流（`postcard`）彻底取代 HTTP + SSE + WebSocket 三拼及所有 `/v1` 孤岛端点；本地走 **Named Pipe（Windows）/ UDS（Linux）+ `yamux`**，远端与移动端走**纯 QUIC（`quinn`）**；冻结 `ADR-0008` C# 投影演进，并以硬性物理隔离与死刑倒计时在 **`v4.9` 物理立斩旧 V2 控制面**，为 `v4.12` 稳定封板与 `v5.0` 内核革命扫清一切协议债。

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

### 铁律 1：依赖级封杀 `serde_json::Value`（Rust 编译器即唯一契约闸门）
- 新建独立契约 crate **`crates/qaqh-ringing-v3`**，**其 `Cargo.toml` 严禁依赖 `serde_json`**。
- 线上传输统一使用 **`postcard`** + 长度前缀帧（直接复用既有 `serde` 强类型领域结构，零手写 schema 文件，二进制紧凑编码）；图片、附件、PTY 终端字节流透传原始 `Bytes`/`Vec<u8>`，彻底消灭 JSON 转义与 Base64 膨胀。
- 彻底取消字符串方法表与 `Value` 中转，从编译期物理封死 `pending_approvals() -> Value` 这类历史妥协口子。

### 铁律 2：协议一网打尽，零协议外 `/v1` 孤岛
- V3 单一强类型 `ControlRequest` / `ControlResponse` 必须从第一天覆盖全部 daemon 能力：
  - `System`（`Health` / `Activity` / `Version` / `Stop` / `StopIfIdle`）
  - `Session` / `Command` / `Shell` / `Marketplace` / `Config` / `Account` / `Pairing` / `Gitx` / `Fs` / `ToolCatalog`
- `v4.9` 处决 V2 后，daemon 对外**零 HTTP/SSE/WS 端口、零 `/control/v1`、零 `/marketplace/v1`**。

### 铁律 3：单连接四流多路复用 + 原生多端联动一等语义
一条物理连接上按用途划分独立逻辑流（QUIC 原生 `open_bi`/`open_uni`；本地 IPC 上叠轻量 `yamux` 多路复用）：
- **控制流（BiStream）**：强类型 RPC 请求与响应；
- **会话事件流（每订阅会话一条 UniStream）**：直推 `SessionEventFrame`，彻底消除跨会话、多端队头阻塞；
- **Shell PTY 流（BiStream）**：原始终端字节透传 + 极简 `Resize`/`Closed` 控制帧，支持多端同时观看；
- **二进制 Blob 流（UniStream / BiStream）**：附件、截图、大文件二进制分片直传，零 Base64 膨胀。
- **原生多端联动下沉进协议层**：多设备同看同一会话/同一 Shell、Driver 席位变更主动广播、设备吊销即时踢流、移动端 `quinn` 0-RTT 与 Wi-Fi↔5G 连接迁移，不再靠 HTTP 轮询打补丁。

### 铁律 4：去版本化命名 + 零别名墙 + Fail-Closed 握手
- 严禁创建 `ClientV3*` 重命名墙（客户端直接 `pub use qaqh_ringing_v3::*`），严禁任何 `#[serde(alias = ...)]`。
- 业务域 DTO（`TimelineEntry`、`ToolCallView`、`CommandAck`、`ShellEvent`、`ToolCatalog`）**永久无版本前缀**；仅最外层线缆模块带 `v3`（`qaqh-ringing-v3`、`v3_server`、`client/v3`）。
- 传输层抽象统一：
  - **本地（`qaqh-gpui` ↔ `qaqh-daemon`）**：Windows `tokio::net::windows::named_pipe`（`\\.\pipe\qaqh-daemon-<user>`）与 Linux/macOS `UnixListener`/`UnixStream`（`$XDG_RUNTIME_DIR/qaqh-daemon.sock`，权限 `0600`），靠 OS 权限天然鉴权，免端口冲突与防火墙弹窗；
  - **远端 / 移动端（PC / Android / OHOS ↔ `qaqh-daemon`）**：**纯 QUIC（`quinn`）**，底层 TLS 复用 `ADR-0039` 既定栈（host/桌面/服务端 `aws-lc-rs`，OHOS 交叉目标保留 `ring`），内置 TLS 1.3 + 证书指纹 pin + 首帧 Device Token 鉴权；
  - **测试 / 调试直连**：`tokio::io::duplex` 内存直连，免启进程免占端口。
- 握手通过 **ALPN (`b"qaqh-ringing/3"`) + 显式 `PROTOCOL_REV: u32` / 契约指纹** 强校验，失配即当场拒绝（Fail-Closed，绝不协商降级）。

### 铁律 5：跨代协议不变量守卫先于业务代码（根绝 11/10 P0 复发）
在编写 V3 业务 handler 前，必须先用 `tokio::io::duplex` 建立并锁定 **`ringing_v3_invariants`** 六大红线守卫，且在 `v4.9` 删旧栈后保持 100% 绿灯：
1. **原子订阅 + 有界回放尾**：先 grab 订阅接收器再读回放环，事件洪峰下 `resync_cycles == 0`（直接继承 11/10 P0 守卫语义）；
2. **四流物理隔离零队头阻塞**：大 Blob 分片 + PTY 刷屏 + 会话 A 洪峰并发时，Control RPC 与会话 B 延迟 < 50ms；
3. **断联不杀进程 + 幂等回执**：断开 UDS/QUIC 不杀 exec/shell，同 `command_id` 重放返回原回执；
4. **身份单权威不可伪造**：鉴权身份唯一来自 OS Socket/Pipe 凭据或握手期 Device Token；**所有业务请求体禁止携带自报身份字段**；
5. **多端联动原语**：双端同看会话/Shell、Driver 交接广播、设备吊销即时断流；
6. **前端渲染预算保护（初心守卫）**：高频 `Replaceable` 增量洪峰经后端合帧/节流后不得拖慢同流 `Reliable` 审批/终态交付，大输出自动转 `BlobRef`/泄洪引用，快照尾窗分页可无全量加载拉取历史。

### 铁律 6：存储隔离 + 异步门面解耦（为 `v5.0` 零返工铺路）
- **严禁把协议升级与存储换代混做**：`v4.6`–`v4.9` 全程复用 `v4.4` 已落地的 SQLite WAL 单权威，禁止新增磁盘格式或 `migrate-on-read`。
- **绕开中央同步大锁**：`v3_server` 面向纯 `async` 服务门面编程，Shell PTY BiStream 直接持有独立 `Arc<ShellHandle>`（消灭 `LOCK_AUDIT_REPORT` `L-11`），Blob 流走异步 `Db::write`（消灭 `L-12`），确保 `v5.0` 切换 `Per-Root Actor` 时 V3 层零返工。

### 铁律 7：前端第一与用户体验至上（不忘 7/31 创世初心，绝不折磨前端开发）
- **开箱即渲染（View-Ready Snapshot + 原生尾窗分页）**：`SessionSnapshot` 从 V3 第一天起原生内置尾部窗口与向上翻页参数（继承 8/08 `9767bb11` 初心，长会话首屏秒开、上滚按需加载）；`ToolCatalog`、`PendingApprovals`、`Diff` 与会话检索一次给齐强类型视图，禁止让前端拼多接口或硬编码规则。
- **交付分级保护前端 60/120fps 帧率预算**：V3 事件帧显式区分 `DeliveryClass::Reliable`（必达终态/审批/错误）与 `DeliveryClass::Replaceable`（流式文本/思考/exec 进度/用量，允许后端按 16–33ms 预算合帧或尾窗截断），大输出转 `BlobRef`/权威日志引用，绝不把高频洪峰直砸 UI 主线程。
- **消费端零胶水硬标尺**：`qaqh-client::v3` 吸收全部发现、重连、QUIC 迁移、回放尾追帧与命令幂等逻辑，对外只暴露极简原生 async Stream/Handle；`v4.7` 验收 `../qaqh-gpui` 切 V3 时，以 **`qaqh-gpui-wire/bridge.rs`（现 56 KB）体积腰斩以上** 为硬判据（沿用 `ADR-0028` 净行数判定纪律——让前端更难写的协议一律判失败）。
- **顺应最先进原生前端范式**：原始 `Bytes` 图像直喂 GPU 纹理（零 Base64）、原样 PTY 字节直喂 `alacritty_terminal`、结构化原生组件取代 HTML iframe。
- **内置轻量端到端延迟标尺**：事件帧头保留 `server_ts_ms: u64`（`postcard` varint 极简开销，继承 8/08 诊断初心），支撑前端精确观测模型/daemon/传输/渲染四段耗时。

---

## 三、反腐迁移纪律（6 条机器可执行红线）

1. **物理隔离建新厂 + CI 反向隔离闸**：
   - 新建独立目录 `crates/qaqh-ringing-v3/`、`crates/qaqh-daemon/src/v3_server/`、`crates/qaqh-client/src/v3/`；
   - CI/本地提交前强制校验：上述目录内 `rg 'axum_server|RINGING_V2|v2_stream|serde_json|serde\(alias|legacy_|compat_'` **必须恒等于 0 命中**。
2. **先冻协议（`v4.6`），再切端（`v4.7`–`v4.8`）**：`v4.6` 完成全量 V3 枚举 + `duplex` 闭环 + 6 条不变量绿灯后，迁移期禁止再推翻核心归属/席位/鉴权模型。
3. **双栈期 V2 绝对冰封 + 2 版本死刑倒计时**：
   - 自 `v4.6` 起 V2 只读冰封，所有新功能只进 V3、严禁倒灌 V2；严禁写任何 `V2-over-V3` / `V3-to-V2` 转换层（宪法 #2/#5）；
   - 双栈仅允许在 `v4.7`（切 GPUI）与 `v4.8`（切移动端）两版存在，**绝不拖延到 `v4.12`**。
4. **消费端单 PR 原子切换，零回退开关**：`v4.7` 切 `../qaqh-gpui`、`v4.8` 切 `qaqh-ffi`/`qaqh-harmony-napi` 时，同 PR 物理删除旧 V2 调用代码，禁止留 `QAQH_USE_V2=1` 类回退开关。
5. **`v4.9` 首张 PR 整目录处决 V2**：物理 `git rm -rf` 删除 `crates/qaqh-daemon/src/axum_server/`、`crates/qaqh-ringing/src/v2/`、`crates/qaqh-client/src/{v2.rs,v2_stream.rs,sse_decoder.rs,shell_stream.rs}`、`tools/qaqh-contract-export/`；验收以两道机器闸为准：① `rg 'axum_server|RINGING_V2|/ringing/v2|/control/v1|/marketplace/v1|v2_stream' crates/` **0 命中**；② `ringing_v3_invariants` 全绿。
6. **冻结 `ADR-0008` 与旧跨语言仪式**：不再重写 `tools/qaqh-contract-export/src/cs_backend.rs`；旧前端留在既有冻结线（宪法 #11）。

---

## 四、被否方案记录（防重提，宪法 #10）

1. **在 QUIC 之上再套一层 HTTP/3（`h3`）**：两端均为自有 Rust 代码，套 HTTP/3 引入冗余 Header/QPACK/状态码转换层，违背宪法 #2/#5。
2. **保留 HTTP+SSE+WS 与 RingingV3 长期双栈并存至 `v4.12`**：直接重演 9/21–10/30 的 40 天泥潭，制造双维护与行为分叉；双栈上限锁死为 2 个版本并在 `v4.9` 处决。
3. **继续完成 `ADR-0008` 第 3 步重写 C# 投影机**：主前端已切 `qaqh-gpui`，属沉没成本陷阱。
4. **在 `v4.6` 硬将 PTC 沙箱运行时接进旧 `session_loop.rs`（3,030 行）**：违背宪法 #8（上帝文件止血），且 `v5.0` 重写 Effect 状态机与全异步工具时必 100% 返工；改为 `v4.11` 独立选型验证、`v5.0` 一次性接入。

---

## 五、与 `v4.5`–`v4.12` → `v5.0` 路线图的衔接

- **`v4.5`（内核清债）**：推进 `P2-0`（已合入 #39）→ `P2-2b`（Crate 合并、消 `OnceLock` 反向槽、清剿手写轮子；`P2-1a` 合并 `qaqh-config-api → qaqh-profile` 时同步摘除 `../qaqh-gpui/Cargo.toml` 旧路径依赖）。
- **`v4.6`（安全阀 + V3 协议冻结）**：落地 `ADR-0035` 泄洪闸（独立 `spill_gate.rs`）+ `ADR-0034` 洪峰冒烟；工具模式三档 + 强类型 `ToolCatalog`（剥离 PTC 沙箱）；**落地 `qaqh-ringing-v3` 强类型协议 + `v3_server`/`client/v3` 骨架 + `tokio::io::duplex` 闭环 + 6 条跨代不变量守卫**。
- **`v4.7`（V3 本地 IPC + GPUI 首切 + 依赖收口）**：落地 Linux UDS + Windows Named Pipe + `yamux`；`../qaqh-gpui` 原子切换至 RingingV3（瘦身 `bridge.rs`、解决 `L-11`/`L-12`）；拔除 `v4.4` 临时 `sidecar.rs` 并统一 `ureq → reqwest` 与 SHA-256。
- **`v4.8`（V3 纯 QUIC + 原生多端联动 + 移动端切流）**：落地 `quinn` 纯 QUIC（0-RTT / 连接迁移 / ALPN / 指纹 pin）与多端同看会话/Shell/Driver 广播；Android `qaqh-ffi` 与 OHOS `qaqh-harmony-napi` 原子切换至 V3。
- **`v4.9`（物理立斩 V2 · 纯血 Rust Native 里程碑）**：整目录物理删除 `axum_server`、旧 V2 路径与 `qaqh-contract-export`；补齐 `../qaqh-gpui` Linux + Windows 双桌面端一等交付。
- **`v4.10`（纯原生富体验 & 历史 ADR 收账）**：无 WebView 原生可视化（取代 HTML iframe `widget-directive`）；接线 `ADR-0024` 硬件 MCP；落定 `audit`/`JOURNAL_IO` 归属；突破 256 条会话检索限制。
- **`v4.11`（`rust-nightly` 烧机 + 8 GiB 级压测 + `v5.0` 边界预演）**：切换 `rust-nightly` 留足 2 版烧机期；8 GiB RAM 全链路压测；独立验证 PTC 沙箱选型（`rquickjs`/`Starlark`/`Monty`，不碰 `session_loop.rs`）；冻结 `v5.0` 内核架构 ADR。
- **`v4.12`（4.x 终极稳定封板）**：全面功能冻结，全平台打包验收，确立 `v5.0` 内核大重写前的纯血 Rust Native 稳固基线。
- **`v5.0`（内核革命）**：一次性完成 `SessionLoop` Effect 驱动状态机 + 全异步工具 + Per-Root Actor + PTC 正式接入。
