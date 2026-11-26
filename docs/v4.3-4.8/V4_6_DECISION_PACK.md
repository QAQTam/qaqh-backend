# QAQ-Harness `v4.6` 架构决策包（二审已定案）

> **基线时间**：2026-11-26 · **代码基线**：`main @ fc1ed287`（已合入 `v4.5` PR #39–#44、`v4.5.1` PR #45、`wip/prompt-cache` PR #46；`../qaqh-gpui/Cargo.toml` 已摘除 `qaqh-config-api`；`../qaqh-gpui/crates/qaqh-gpui-wire/src/bridge.rs` 现 **1,316 行**）
> **裁决状态**：**`已二审定案`**（2026-11-26，采纳独立红蓝对抗二审意见并完成 `fc1ed287` 代码实证复核）
> **Companion 文档**：[AGENTS.md](../../AGENTS.md)（《项目宪法 v2 · 高质量发展修宪版》）· [ADR-0042 r3](../adr/0042-ringing-v3-rust-native-ipc-and-quic.md)（状态：`已采纳 · 未实施`）· [ADR-0035](../adr/0035-exec-flood-model-spill-gate.md)（状态：`已采纳 · 未实施`）· [V4_6_EXECUTION_PLAN.md](./V4_6_EXECUTION_PLAN.md)

---

## 一、已冻结前提（本次未开放重审）

1. **战略路线**：`v4.5` → `v4.12`（稳定封板）→ `v5.0`（内核大重写）。
2. **消费端形态**：纯血 Rust Native（桌面 `../qaqh-gpui` + 移动 `qaqh-client` FFI），冻结 `ADR-0008` C# 投影演进；`../qaqh-gpui` 在 `v4.7`、移动端在 `v4.8` 各自单 PR 原子切换至 RingingV3（零回退开关），**`v4.9` 首张 PR 整目录物理立斩 V2**。
3. **本机前后端通信机制**：走 RingingV3 本地 IPC（Linux UDS / Windows Named Pipe），**否决「跨进程共享内存直读后端可变消息上下文」**（收益低于渲染预算 2–3 个数量级，却破坏 Rust 内存安全并在 `truncate_from_turn`/扩容时引入 `SIGBUS`/读撕裂与双轨分叉）。
4. **三大硬约束**：
   - `v4.6`–`v4.9` 全程复用 `SCHEMA_VERSION == 13` SQLite WAL，不动存储引擎、不新增磁盘 `migrate-on-read`（宪法 #6）；
   - `session_loop.rs`、`store.rs`、`owner.rs` 等 >1,000 行生产文件严守宪法 #8 净行数只减不增，大重写锁定 `v5.0`；
   - 剥离重型 PTC 沙箱运行时至 `v4.11` 选型、`v5.0` 接入，不在 `v4.6` 接入 `session_loop.rs`。
5. **`v4.6` 既定范围**：在先置清债 `P3-0` 之后按序推进 `P3-1`（`ADR-0035` 泄洪闸 + `ADR-0034` 冒烟）→ `P3-2`（`core-vocab` + `tool_mode.rs` + `ToolCatalog`）→ `P3-3`（冻结 `qaqh-ringing-v3`）→ `P3-4`（`v3_server` + `client/v3` + `duplex` + 6 条不变量 + 2 份前置 ADR）。

---

## 二、5 项架构抉择（含 `fc1ed287` 代码实证与修正）

### D1（P0 · 编译期硬冲突）：`qaqh-ringing-v3` 类型归属、`postcard` 兼容性与零 `serde_json` 铁律

#### 代码实证事实（`fc1ed287`）
1. **无条件 `serde_json` 传递依赖**：`crates/qaqh-types/Cargo.toml:11` 与 `crates/qaqh-ringing-contract/Cargo.toml:18,25` 均依赖 `serde_json = "1"`，且 contract 直接依赖 `qaqh-types`。
2. **生产裸 `Value` / `Error`**：`qaqh-ringing-contract` 含 **4 处 `serde_json::Value`**（`commands.rs:87` `model_content`、`events.rs:132` `args`、`state.rs:214` `opaque`、`state.rs:267` `args`）及 **1 处 `serde_json::Error`**（`team/error.rs:10`）；`qaqh-types` 含多处 `serde_json` 使用（`arg.rs`、`memory.rs`、`message.rs`、`platform.rs`、`tool_def.rs:24`、`tool_result.rs`）。
3. **`postcard` 致命不兼容属性（共 257 处）**：
   - `qaqh-ringing-contract` 含 **13 处 `#[serde(tag = ...)]`（其中 6 处为 adjacently tagged `tag + content`）+ 1 处 `#[serde(flatten)]`**；`qaqh-types` 含 **5 处 `#[serde(tag = ...)]` + 1 处 `#[serde(flatten)]`**；
   - **更隐蔽的高危雷区**：两 crate 合计含 **237 处 `#[serde(skip_serializing_if = ...)]`**。`postcard` 是位置编码，序列化跳过字段会导致反序列化端**静默错位读垃圾**（比 `tag` 直接报错更危险）。
   - V2 HTTP/SSE 与 SQLite `blocks.payload_json` / `turns.outcome_json` 在 `v4.6`–`v4.8` 期间仍依赖既有 JSON 形态，冰封期不能改旧 crate 的 `serde` 属性。
4. **原语与目录现实状态**：
   - `SessionId`/`TurnId`/`BlockId`/`ToolCallId`/`InputId`/`InteractionId` 现有 newtype 由冰封的 `qaqh-ringing-contract/src/fact/types.rs:57-66` 经 `string_id!` 宏生成；`ActivityState`/`TurnTerminal` 亦在 `fact/types.rs:905,961`，仅 `UsageInfo` 在 `qaqh-types/api_types.rs:18`；核心业务链路大量传 `&str`/`String`；
   - `ToolCatalog` 尚未在代码库存在，为 `P3-2` 全新产物。

#### 方案选型与定案：**1C′（二审修正版）**
- **否决 1A**（完全不共享底层 vocab 导致 `P3-2` `ToolCatalog` 与 V3 边界出现重复 ID/枚举定义）与 **否决 1B**（CBOR/MsgPack 退让违背宪法 #6“依赖级封杀 `serde_json`”）。
- **定案 1C′**：
  1. 在 **`P3-2`** 新建零 `serde_json` 的 `crates/qaqh-core-vocab`（放强类型 ID newtype、`ActivityState`、`TurnTerminal`、`UsageInfo`、`ToolCatalog`（不带 JSON Schema 或以 `JsonText(String)` 封装）等纯值原语），仅给 `P3-2`、`qaqh-ringing-v3`、`v3_server`、`client/v3` 使用；
  2. **冰封中的 `qaqh-types`、`qaqh-ringing-contract` 及 `../qaqh-gpui` 在 `v4.6` 一行不动**（绝不违反宪法 #7 V2 冰封，亦避免留 `pub use` 空壳）；`v4.7` GPUI 切换后断除对旧 contract/`qaqh-types` 依赖，`v4.9` 处决 V2 时整包退役 `qaqh-ringing-contract` 并按需收编 `qaqh-types`；
  3. **`v3_server` 单向投影层**：`v3_server` 将后端内部/存储态单向投影为 `core-vocab` / `qaqh-ringing-v3` 强类型 DTO（在 `ADR-0042 r3` 显式界定「单向后端→V3 投影」≠「被禁 V2↔V3 转换层」）；
  4. **机器硬红线**：`qaqh-core-vocab` 与 `qaqh-ringing-v3` 对 `skip_serializing_if`、`#[serde(tag`、`untagged`、`flatten`、`serde_json` **恒为 0 命中**，且 `cargo tree -p <crate> -e normal,build -i serde_json` **输出为空**，配合全量 DTO（含 `Option::None`/空 `Vec`）`postcard` 往返属性测试。

---

### D2（P1 · 前端初心与协议冻结）：`SessionSnapshot` 尾窗分页与大载荷边界

- **D2.1 向上翻页原语（定案：A 加强版）**：
  - 延续 `store.rs:862`（`read_transcript_page(before_ord, limit)`）与 `view.rs:35`（`load_session_state_tailed`），在 V3 首版冻结：
    1. 首屏 `GetSessionSnapshot` 支持 `tail_blocks`（回退回合边界），**把保留的最近 compaction 摘要拆入独立 `pinned_compaction: Option<CompactionSummary>` 字段**，不混入 `timeline`，消除前端翻页重复去重逻辑；同时给 `turns` 列表设明确尾部窗口上限与分页支持；
    2. 向上翻页走强类型 `GetTranscriptPage { session_id, before_ord, limit, timeline_epoch }`，一次同时返回该页 `blocks` 与关联 `ToolCallView`（明确跨页工具调用的归属规则）；`truncate_from_turn` 递增 `timeline_epoch`，游标失配时 Fail-Closed 返回 `StaleCursor` 令前端重拉快照。
- **D2.2 按需只读 `InspectModelContext`（定案：否决，推迟至 `v5.0` 之后）**：
  - `fc1ed287` 下拉取真实上下文需在 2,887 行的 `session_loop.rs` 接入 `assemble_context()`（违背宪法 #8），且暴露明文 system prompt 与 PR #46 无明文 `cache_diag` 方向相悖、需额外鉴权与走 Blob 流。记入 `ADR-0042 r3` 已否决（`v4.6`）。
- **D2.3 `BlobInfo` 预留 `local_readonly_path`（定案：否决）**：
  - 与 §一否决共享内存的理由一致（收益低于渲染预算 2–3 个数量级），破坏跨端一致性，且 `ADR-0035` 日志运行期仍在追加并存在 GC 竞态。本地与远端统一走 `BlobTransfer` 流。

---

### D3（P1 · 传输与双栈退场）：本地 IPC 多路复用栈 & 服务发现策略

- **D3.1 本地 IPC 多路复用（定案：A · `yamux`）**：
  - UDS / Named Pipe 之上叠 **`yamux`**（提供每逻辑流独立流控窗口与调度公平性，与 `quinn` 四流同构）；注意 Rust `yamux` crate 基于 `futures::io`，需 `tokio-util::compat` 与独立 driver task（开工时按宪法 #16 查最新官方 API）。
  - 修正不变量②表述：单连接字节流上不存在传输层物理消除队头阻塞，不变量②以**可量化延迟指标**验收（大 Blob 满载 + PTY 刷屏时，Control RPC 往返 P99 < 50ms）。
- **D3.2 双栈期服务发现（定案：A · 无版本最终名 `daemon.endpoint`）**：
  - 独立于现 `crates/qaqh-types/src/discovery.rs:15` 的 `DaemonDiscovery`（含 `endpoint`/`token`/`pid`/`server_epoch`/`protocol_version`/`daemon_version`/`build_id`/`channel`/`executable`/`lan_endpoint`/`tls_fingerprint` 及 `ts` 导出）；
  - 第一天即使用永久无版本文件名 **`daemon.endpoint`**（固定单一轻量非 JSON 格式如 `postcard` 或定长二进制头，避免 `qaqh-client` V3 路径引入 `serde_json`），内置 `build_id` / `channel` Fail-Closed 校验；`v4.9` 处决 V2 时整块删除旧 `daemon.json` 与 `DaemonDiscovery`（含 `ts` 导出）。

---

### D4（P1 · 与 `v5.0` 衔接）：`v3_server` 纯 `async` 门面拆分粒度

- **现状修正**：`crates/qaqh-runtime/src/service/handlers/` 实为 **7 个业务文件 + `mod.rs`**（`gitx`/`goal`/`providers`/`session`/`system`/`task`/`workspace`）。
- **定案 4A（细化版）**：
  - 不把 9 个域塞进单个 `SystemControlFacade`，按锁与生命周期细分为多个纯粹 `async` Facade Trait（可组合为顶层 `V3Backend` bound）：
    - `SystemFacade` / `ConfigAccountFacade` / `MarketplaceFacade` / `GitxFacade` / `FsFacade`
    - `SessionFacade`（列表/快照/分页/命令/订阅 `OwnerAttach`）
    - `ShellFacade`（配合独立并发句柄表，完全绕开中央 `registry` 锁，消除 `L-11`；注：`v5.0` 切 Per-Root Actor 时 Shell/Exec 归属可能随 root 同步调整）
    - `BlobFacade`（全异步读写与流式分片，消除 `L-12`）
  - **分发方式**：明确采用**泛型静态分发**（`V3Server<B: ...>`）以支持 `tokio::io::duplex` 零装箱测试注入，避开原生 `async fn in trait` 非 `dyn`-safe 返工。

---

### D5（P2 · 施工排期）：前置收债与 `v4.6` 切片次序

- **D5.1 Fast-Follow（`session_loop.rs` 16 处 `.await.ok()` + 2 处回滚泄漏）排期（定案：A）**：
  - **先清 Fast-Follow（记为前置切片 `P3-0`），再开 `P3-1`（`ADR-0035`）**；`session_loop.rs` 在 `fc1ed287` 已缩至 **2,887 行**，所有锚点行号与每类 Fail-Closed 策略已按 `fc1ed287` 刷新至 [V4_5_1_PR45_REMEDIATION_GUIDE.md](./V4_5_1_PR45_REMEDIATION_GUIDE.md) §二，且通过拆出独立子模块保证 `session_loop.rs` 净行数 `≤ 0`。
- **D5.2 `tool_surface_schema_shape` 基线红灯（定案：B）**：
  - **并入 `P3-0` 同 PR 一并修绿**，确保 `v4.6 P3-1` 是在 100% 全绿基线上开工（宪法 #12）。

---

## 三、二审盲区补丁定案（强制进 `ADR-0042 r3` 与 `v4.6`–`v4.7` 门禁）

1. **Windows Named Pipe 安全红线**：Windows 默认 Named Pipe DACL 会授予 `Everyone` 读权限；本地管道监听必须显式构造当前用户 SID 的 **Owner-Only Security Descriptor**，并开启 `reject_remote_clients(true)`。
2. **`postcard` 协议演进与机器指纹**：`PROTOCOL_REV` 之外的**协议指纹必须由机器从 `qaqh-ringing-v3` 类型定义生成**（严禁手改）；同时在 `ADR-0042 r3` 明确 `v4.7`–`v4.12` 的版本配对与移动端最低支持版本守卫。
3. **`P3-2` 先依赖 D1**：`qaqh-core-vocab` 在 `P3-2` 开工建 crate 承载 `ToolCatalog` 与底层原语，`P3-3` 直接消费。
4. **`v4.7` GPUI 切换后依赖红线**：切完 GPUI 后必须在 CI 强制 `../qaqh-gpui/Cargo.toml` 对 `qaqh-ringing-contract` / `qaqh-types` 的引用恒等于 0。

---

## 四、最终裁决汇总表

| 编号 | 议题 | 最终定案 | 关键约束/备注 |
|---|---|---|---|
| **D1** | `qaqh-ringing-v3` 类型归属与 `postcard` 零 `serde_json` | **1C′（已采纳）** | `P3-2` 建零 JSON `qaqh-core-vocab`（不碰 V2 crates）；两新 crate 禁用 `skip_serializing_if`/`tag`/`untagged`/`flatten`/`serde_json`（0 命中 + `cargo tree` 为空）+ 补 `ADR-0042 r3` |
| **D2.1** | `SessionSnapshot` 尾窗与向上翻页 | **A 加强版（已采纳）** | 独立 `pinned_compaction` 字段 + `turns` 窗口上限 + `GetTranscriptPage(before_ord, limit, timeline_epoch)` 含关联 `ToolCallView` + `StaleCursor` Fail-Closed |
| **D2.2** | 按需只读 `InspectModelContext` RPC | **已否决（推迟至 `v5.0` 后）** | 记入 `ADR-0042 r3` §四 |
| **D2.3** | 不可变大载荷 `local_readonly_path` | **已否决** | 本地/远端统一走 `BlobTransfer` 流，记入 `ADR-0042 r3` §四 |
| **D3.1** | 本地 IPC 多路复用栈 | **A · `yamux`（已采纳）** | `tokio-util::compat` + 独立 driver task；不变量②改量化 P99 < 50ms |
| **D3.2** | 双栈期服务发现文件 | **A · `daemon.endpoint`（已采纳）** | 第一天即无版本最终名 + 单一非 JSON 格式 + `build_id`/`channel` 校验；`v4.9` 整删旧 `daemon.json` |
| **D4** | `v3_server` 异步门面粒度 | **4A 细化版（已采纳）** | 按域拆分细粒度 trait + `ShellFacade` 独立并发句柄表 + 泛型静态分发 `V3Server<B>` |
| **D5.1** | Fast-Follow 16 处 `.await.ok()` 排期 | **A（已采纳，立为 `P3-0`）** | 按 `fc1ed287` 新行号 + 先定 Fail-Closed 策略，优于 `P3-1` 前清完（净 `≤ 0`） |
| **D5.2** | `tool_surface_schema_shape` 基线失败 | **B（已采纳，并入 `P3-0`）** | 不带已知红灯进入 `P3-1` |
