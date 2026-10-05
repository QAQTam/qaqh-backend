# 新旧协议并存盘点（legacy-protocol-audit，全层扫描）

> 状态：审计稿，2026-10-04。基准 = 当前工作区（含未提交修改：`spy_tool.rs` 新文件、
> qaqh-spy / qaqh-workspace 一批改动），非 `76f804e`。
> 方法：codegraph 索引 + 四路分区只读审计（wire / 工具 API / 持久化 / 域事件），
> 关键结论已抽样人工复核。
> 目的：为"全面确保新协议、彻底移除旧协议与协议桥"提供移除清单。
> 与 `docs/plan-legacy-protocol-cleanup.md`（基准 76f804e）互为补充；第 6 节给修订。

## 0. 总览

| 层 | 新旧并存程度 | 一句话 |
|---|---|---|
| wire（HTTP/SSE/客户端/Tauri/前端） | **基本干净** | 18 条路由全 `/ringing/v2/*`，v1 只剩死类型与 404 断言测试 |
| 工具调用协议 | **一步之遥** | 内置 20 + 子代理 18 工具已全部 `register_typed`；唯一活桥 = MCP/LSP 动态工具 |
| 持久化 | **最大结构性双轨** | legacy message store（jsonl/wal/meta）与 canonical events.jsonl **双写**，双栅栏互不知晓 |
| 域事件 | **发了没人收** | worker 侧 DomainEvent 整体 fire-into-void；ts-rs 导出面 156/190 零消费 |
| 会话身份 | **wire 已清，内部未清** | 无 wire 残留；内部 `seed` 标识符 ~366 行 + 旧目录未迁移（BETA-01 未完成） |

## 1. 工具调用协议（协议桥本体）

### 1.1 已完成（清理工不要误伤）

- 全部内置 20 工具 + 子代理 18 工具走 `register_typed`（新契约），词汇表由
  `crates/qaqh-workspace/src/registration.rs:85-116` 测试锁定；注册点逐个：
  exec `exec/register.rs:17`、web_fetch `web.rs:271`、edit `edit/handler.rs:324`、
  write/delete `file_mutate.rs:848-849`、read `file_query.rs:518`、glob `file_glob.rs:277`、
  apply_patch `apply_patch.rs:340`、copy_range `copy_range.rs:553`、grep `grep_tool.rs:521`、
  confirm_apply `confirm_apply.rs:286`、todo 三件 `todo/split.rs:177-179`、ask `ask_user.rs:315`、
  read_image `read_image/mod.rs:367`、journal `journal.rs:607`、spy `spy_tool.rs:298`（新增）、
  process `process_inspect.rs:274`、skills `skill.rs:380`；子代理 18 个见
  `qaqh-subagent/src/lib.rs:814-838`。
- v1 `ToolManager::register()` 生产零调用：`manager.rs:236-252` 定义本体，其余全是
  `#[cfg(test)]`（`manager.rs:799-876` 内嵌测试、`runtime.rs:457-460`、
  `tests/dynamic_registration.rs:112,259`、`qaqh-runtime/tests/` 5 处）。

### 1.2 唯一活链：MCP/LSP 动态工具（协议桥的真身）

```
engine_turn.rs:1451-1456 (MCP) / :1459-1464 (LSP)
  → runtime.rs:493-509 replace_dynamic_tools / :514-531 merge_dynamic_tools
  → manager.rs:308-339 register_dynamic  ── LegacyToolAdapter::from_owned(:326) + legacy:Some(:329-336)
  → manager.rs:562-565 prepare 二选一 → execution.rs:321-329 PreparedExecutor::Legacy
  → 裸 fn 指针 = qaqh-mcp/src/bridge.rs:103-111 dispatch（LSP: qaqh-lsp/src/bridge.rs:59-61）
```

- 动态工具签名是 `fn(ToolCallCtx) -> ToolResult`（`DynamicTool.handler_fn`，
  `manager.rs:183`），250ms 轮询/超时/取消在 `mcp/bridge.rs:39-45`。
- `qaqh-mcp/src/adapter.rs` 是 rmcp/进程隔离层，**不属于旧协议**，移除时不动。
- `manager.rs:932-933` 测试断言 dynamic 必须留在 legacy 桥上——移除时要改写该断言。
- 依赖这条链的伴生设施（typed 化后才能删）：
  - `RegisteredTool.legacy` 字段 + `LegacyExecutor` 类型（`manager.rs:47,51-55`）与
    `PreparedExecutor::Legacy` 臂（`execution.rs:322-329`，含死分支 skills 特判 :324-328）；
  - `ToolCallCtx` 兼容字段（`workspace/src/lib.rs:620-633`：`action`、`skill_effects`…）；
  - 线程局部兼容视图 `install_tool_call_context`（`runtime.rs:97-135`，7 个 TLS 槽位，
    每次工具执行都安装；消费者 = dispatcher 读 `ctx.cancel`（`mcp/bridge.rs:108`）+ 测试；
    显式 context 的 `CancellationToken::shared_flag` 在 `tool_api/context.rs:88` 已备）；
  - `{name}_{action}` 后缀解析：唯一生产消费点 `util/format.rs:7-17`（UI 直调路径
    `engine_tool.rs:184`）；admit 侧恒传空串（`authorization.rs:525,550`）。
- `LegacyToolAdapter` 执行面（`execute_legacy`/`ErasedTool::execute`/`map_tool_result`，
  `legacy.rs:189-257,265-322`）**生产不可达**：两个构造点都同时置 `legacy: Some`，执行
  永远绕过适配器；适配器在生产只被当 descriptor 用（`manager.rs:58-68` `tool_def()`）。
  P2.5 副作用丢弃警告（`legacy.rs:249-255`）从未在生产发声。

### 1.3 收敛面 ≠ 旧协议（不要混为一谈）

Typed 工具结果经 `ToolOutcome::to_tool_result()`（`output.rs:175-220`）折回 v1
`ToolResult` 形态（`execution.rs:336`），下游 fold/审计/finalize/timeline/engine_tool
全吃该形态。这是当前所有工具的共同收敛面，只有下游整体改吃 `ToolOutcome` 才能删，
属独立工程，不算"旧协议残留"。

### 1.4 边界外：tool_recovery 的 38 处 legacy

`qaqh-runtime/src/agent/tool_recovery.rs` 全部 legacy 字样指旧 WAL 文件
`tool_outbox.wal`（:25），会话恢复时只读迁移兜底（:81-115 读、:155-199 retirement、
:216-267 canonical 账本优先调和），**不再写入**（`state/lifecycle.rs:240-241` 注释）。
属旧存储格式兼容，与工具协议无关；删除 = 放弃旧会话恢复。

## 2. wire 层（HTTP/SSE/客户端/Tauri/前端）

### 2.1 已干净

- 路由唯一注册点 `axum_impl/mod.rs:143-197`：18 条路由全 `/ringing/v2/*`（另有无版本
  `/health`、`/activity`）；v1 字符串只剩 404 硬切断言测试（`axum_server.rs:584-722`）。
- v1 三频道解析 `parse_channel` 降为 `#[cfg(test)]`（`axum_impl/auth.rs:47-49`、
  `mod.rs:50-51`）。
- SSE 事件名全集 3 流：`ringing.event`（`axum_impl/v2.rs:497`）、`ringing.reset_required`
  （:505）、`timeline.entry`（`sse.rs:86`，data 带 `version: RINGING_V2_VERSION`）+
  `ringing.stream_terminated` 终止帧（`sse.rs:51,235`）。
- gate 三 API 面（`message_api.rs`=Anthropic、`chat_completions_api.rs`=OpenAI Chat、
  `responses_api.rs`=OpenAI Responses）是**对等 provider 适配器**，无新旧之分，按
  `ProviderKind` 分发（`gate/src/lib.rs:76-127`），真实消费方 `turn_lap/gate.rs:611`。
- client 的 `endpoint.rs`（服务 RPC 面）与 `v2.rs`（事件命令面）是分工不是新旧；
  前端零直连 HTTP，全 Tauri IPC（`webui/src/lib/transport/index.ts:23`），Tauri 壳
  commands.rs 全走 v2 面。

### 2.2 死面（零生产调用方，可直接删）

| 对象 | 位置 | 备注 |
|---|---|---|
| `ClientOpenRequest`/`ClientOpenResponse` | `qaqh-ringing/src/capability.rs`；`client/types.rs:33` 再导出 | **文档撒谎**：文件头自称 v2 open payload，实际 handler 用 `RingingV2OpenRequest`（`axum_impl/v2.rs:73`）；ts 绑定 `ClientOpenRequest.ts` 等一并清 |
| `RingingCommandEnvelope`（v1 命令信封） | `qaqh-ringing/src/envelope.rs:18` | 同文件 `RingingCommandAck`/`AckStatus`/`CommandState` 仍活跃（v2 ack 复用，`daemon/command.rs:13-27`），**保留** |
| `RingingEvent` 三频道 wire 联合 | `qaqh-ringing/src/event.rs` | 仅内部 writer 载体（`engine_title.rs:75`、`engine_compact.rs:557`、`paced_emitter.rs:101`）+ 测试；换载体后可删 |
| `RingingChannelSnapshot` + `SnapshotProjector` + `RingingHub::snapshot/conversation_snapshot` | `snapshot.rs`；`runtime/ringing/projection.rs:50-74`；`hub.rs:561-575` | v2 bootstrap 走 `V2ProjectionHub`→`ProjectionSetSnapshot`，此链纯测试维持 |
| `RINGING_VERSION=1` + v1 游标格式文档 | `qaqh-ringing/src/protocol.rs:10,18-19` | 随上行删除后清理 |
| `ClientOpenRequest as OpenRequest` 再导出 | `client/src/types.rs:33` | 零消费者 |
| 前端浏览器模式占位 `bootstrap()` | `webui/src/lib/transport/backend.ts:60-61` | Tauri-only 已定，可删占位与 `GatewaySession` 类型 |
| 过时注释 `/api/config/events` T20 | `qaqh-config/src/watch.rs:11` | 改注释即可 |

### 2.3 活跃兼容（对外红线，移除需先确认旧客户端存活）

- `discovery.rs:22-25` 剥离旧 `ws://`/`wss://` scheme；`:320-368` 两条 pre-0.9
  daemon.json 六字段兼容测试是兼容闸。
- `/control/v1/stop`、`/control/v1/stop-if-idle`（`axum_impl/mod.rs:188-189`）**活跃**
  （调用方 `client.rs:765-770`、`main.rs:167` 自杀信号）——是改名/升版问题，不是删除。

### 2.4 活跃低效（O(n²) 基座，未修）

`StreamEvent::ToolCallProgress { args_so_far }` 每帧 clone 整段累计串：
`chat_completions_api.rs:342-348`、`message_api.rs:506-512`（`responses_api.rs:1111-1116`
只在 done 发一次无此问题）。消费侧已按字节偏移增量绕开（`turn_lap/gate.rs:26-76`，
但 `gate.rs:642` 仍整段 clone 进 `BlockOpened`，每 tool 一次可接受）。

## 3. 持久化与存储

### 3.1 结构性双轨（最大的一块）

**legacy message store 与 canonical fact 流并行双写，双栅栏互不知晓：**

- 现役权威持久层 = `messages.jsonl + meta.json + index.jsonl + messages.wal`
  （`qaqh-message/src/wal.rs` header `qaqh-wal-v1` :51，open/log_op/checkpoint
  :148-204 全活跃；`enable_message_wal` 在 `state/lifecycle.rs:27,237,369,416,444` 全开）。
- canonical `events.jsonl`（schema `qaqh.session-fact/v2`，`session_fact_v2/types.rs:9`）
  在生产并行写入：会话基线 `service.rs:933-1000`、子代理 `registry.rs:397,723,820,880`、
  ringging 交互 `ringing/v2.rs`、tool ledger `canonical/tool_ledger.rs:931,994`。
  **但尚未接管 message/journal 写**——`canonical/store.rs:27` 自认
  "It does not yet route legacy message/journal writers through this owner"。
- 双栅栏：legacy 写走进程内 `LegacyWriterFacade`（`legacy_writer.rs:12-27`），
  **10 个调用点全活跃**：`wal.rs:149,180,200,773`；`timeline_store.rs:105,182,285`；
  `session/manager.rs:242,809,1346`。canonical 写走每会话目录 `events.lock`
  文件锁（`canonical/log.rs:26,669-679`；team store `team/store.rs:22`、
  `team/board/store.rs:22`）。跨系统互斥只靠 daemon 单实例锁兜底
  （`legacy_writer.rs:7-8`）。
- 结论：这条双轨**不能直接删一边**，移除前提是 canonical 接管 message/journal 写，
  是独立工程。

### 3.2 migrate-on-read 清单（活跃兼容，删除前需存量数据审计）

| 兼容点 | 位置 | 审计条件 |
|---|---|---|
| `meta.compact_skip` 旧压缩语义重放 | `state/lifecycle.rs:180-190`（读）；新压缩恒置 0（`store.rs:504-507,1648`） | 确认存量 meta 无 `compact_skip>0` |
| 旧 `[COMPACT` front-of-context 标记清理 | `message/store.rs:1608` | 存量归档不再含旧标记 |
| `index.json` → `index.jsonl` | `session/store/mod.rs:328-389`（迁移后删旧文件） | 存量跑过一次迁移 |
| `workspace.txt` → `meta.cwd` 惰性迁移 | `session/manager.rs:568-592` | 同上 |
| 旧反斜杠 cwd 修复 | `grouping.rs:64` | — |
| `timeline-v3/` 目录改名 | `timeline_store.rs:68-74` | 存量跑过 |
| `PersistedTimeline.journal` 字段 | `timeline_store.rs:33-36`（`serde(default)` 读旧缓存） | 缓存全部重建过 |
| `tool_outbox.wal` 只读兜底 | `tool_recovery.rs:25,81-115,155-199` | 放弃旧会话恢复 |
| DeepX 数据根 marker 改写 | `types/platform.rs:82-113`（启动即跑） | 全机升级过 |
| 旧 provider_id → (id, endpoint) | `config/registry.rs:330` + `config.rs:724-732` | 同上 |
| 明文 api key → secret store | `config.rs:781,847` | 同上 |
| 旧扁平 model 字段 → `[profiles.*]` | `config.rs:1023` | 同上 |
| 权限旧四档 u8(1-4) → 三档 | `policy/lib.rs:111`（`from_legacy_u8`）；wire 上仍是裸 u8（:68 注释） | 确认 wire 升版后可删 |
| `ConversationMode::Code` alias `"normal"` | `domain/command.rs:32-34` | 旧 wire 值消费者消失后 |
| timeline 旧 JSON 兼容槽位 | `domain/timeline.rs:341`（display 缺省回退）、`:400-402`（`created_seq=0` 哨兵）；域事件旧形状 `domain/event.rs:765-814` 测试 | 存量 timeline 重建后 |

### 3.3 死代码 / 预留（可直接删）

- `wal::read_ops` 不可失败旧签名：`message/wal.rs:254-276`（仅测试用）。
- `WorkspaceStore::rename_session`：`grouping.rs:316-320`（无生产调用方，BETA-01 预留）。
- `SessionManager::canonical_identity_for_session`：`manager.rs:1261-1275`（无生产调用方）。
- 过时注释：`manager.rs:138`（"legacy TOML"无其事）、`service.rs:643`（workspace.txt
  spawn 前写入已不走）。
- `flat_in_write_order` 的 turns-先 trailing-后 legacy 顺序（`message/store.rs:971`）仅对
  无 msg_id 的 ephemeral store（子代理）生效——活跃但作用面窄，是行为契约
  （`persist_effects.rs:115,251` 锁定），不是可删残留。

## 4. 域事件层与前端导出面

### 4.1 发了没人收（计划 §3 复核：仍有效）

- `ToolEvent::ToolCallPrepared`（`domain/event.rs:457-463`）：发射点
  `turn_lap/gate.rs:679-690`（每帧整段 args）→ `paced_emitter.rs:76-102` →
  `actor.rs:59-79` **信封 body 被丢弃**（阶段 3d 注释）。
- `ToolEvent::CodeChanged`（`event.rs:512-525`）：发射点 `engine_tool.rs:830-843` +
  `tool_runtime.rs:1219-1233`（backfill）→ 同样在 actor 桥丢弃；载荷另有持久旁路
  `code_stats.jsonl`（`agent/types.rs:501-535`）。
- 两者在 v2 canonical 面 0 命中（`FactPayload` 只有
  ToolCallDeclared/ToolIntent/ToolFinished，`session_fact_v2/types.rs:163`），webui 0 消费。
  **删除不丢数据**：估算职责已由 timeline `ToolEstimated` 通道承接并已被前端消费
  （`timeline.rs:485`；`gate.rs:654-677`；`reducer.ts:322-326`）。

### 4.2 计划外发现：fire-into-void 是整张枚举，不止这两个变体

阶段 3d 后 worker `emit_domain` 的所有 Ringing body（含 `ToolNotice`、`AuditRecorded`、
`DashboardUpdated/Snapshot`、`ProviderToolStatus`…）在 `actor.rs:59-79` 只做副作用判定
（`registry.rs:2202-2244` 只匹配 Interaction*/PlanReview*/ToolFinished）后丢弃——
真正出网的只有 timeline 与 canonical fact 两条通道。**删"发了没人收"的事件时应把
ToolEvent/ControlEvent 整张枚举按"谁还在桥上被匹配"重审**，而不是只看两个变体。

### 4.3 计划外发现：`Delivery` 分类链整体是死代码

`domain/delivery.rs:10` + `DomainEvent/ToolEvent::delivery()`（`event.rs:435-443,528-534`）
+ `qaqh-ringing/src/event.rs:43-45` 转发，生产无调用方；v2 wire 实际用
`session_fact_v2/projection_event.rs:88` 的另一个 `Delivery`。删事件时可一并删分类链。

### 4.4 ts-rs 导出面：156/190 零消费

生成物 190 个（`webui/src/api/qaqh/*.ts` 189 + JsonValue.ts）；app 直接 import 仅 15 个，
传递闭包可达 34 个（全为 Config/Settings/Timeline 系列）→ **156 个零消费**。域事件全家
（ToolEvent/DomainEvent/ControlEvent/ConversationEvent/RingingEvent/Delivery/DomainCommand/
Dashboard*/TeamBoard*/Projection*…）全在死集合。注意：前端 projection 事件消费是
untyped envelope（`backend.ts:67` `Record<string, any>`），`ProjectionEvent/ProjectionPayload`
虽零消费但 SSE 契约仍在线——白名单化时要么转正要么先别删。计划 §3 此条仍有效且比
计划写的更严重。

### 4.5 行数口径（计划 §4 部分过期）

- 聚合徽章已切后端权威（`webui/src/diff/parse.ts:7` 头注释；`StepRow.tsx:162-168`
  注释"不再从 diff 文本统计"）；估算通道 `ToolEstimated` 已接线且前端消费，
  终态强制替换已升级为显式 estimating 状态（`reducer.ts:298,322-326`、
  `StepRow.tsx:170-174`）。
- 残存双口径：diff **展开视图逐文件** `+N −M` 仍前端自算（`DiffView.tsx:222-223`）——
  与后端权威口径并存但服务不同视图，收口与否是 UI 决策。
- `ToolCapabilities.streaming`（`tool_api/capabilities.rs:22-25`）语义已注释钉死
  （计划走了注释路线，可视为已修）。**计划外**：`.streaming` 无任何生产读取点
  （进度通道实际由 `tool_runtime.rs:637` 直连），纯声明字段，改名零风险。

### 4.6 spec vs code

`docs/spec-file-mutation-delta-v2.md` 的 `FileMutationDelta`/`HunkDelta`/`ResolvedHunk`/
`on_hunk` 在 crates/webui/tools/scripts 全文仍 **0 命中**——开工或归档，别让规格长期
空转（计划 §4 仍有效）。

## 5. seed → session_id 身份残留

- **wire 已清**：client 反向断言不发 seed 键（`endpoint.rs:452-457`）；
  `SessionMeta` 序列化不再输出 `seed`（`types/session.rs:456` 守卫测试）。
- **内部标识符未改**（计划 §2 仍有效）：生产代码 `\bseeds?\b` ~366 行。真标识符热区：
  `ringing/hub.rs`（`forget_seed`/`ensure_seed_loaded`/`ChannelShards.sessions` 分片键）、
  `ringing/driver_watch.rs:38-43`（持久化 `<data_dir>/ringing-driver-watch.json`
  seed 清单——**改标识符会动磁盘文件键**）、`session/manager.rs`（目录布局文档
  `{sessions_dir}/{seed}/` :4、错误串 :805,814）。其余多为注释/测试字面量
  （`registry.rs` 日志串、`activity.rs`/`axum_server.rs` 内嵌测试）。
- **旧目录未迁移**：新会话目录名=canonical id（`allocate_session` `manager.rs:873-921`
  + `canonical-identity.json` sidecar `canonical/identity.rs:92-99`）；旧 seed 目录靠
  `session_dir_for_id`（`manager.rs:1388-1420`）扫描 sidecar 解析，注释明说
  "BETA-01 will eventually make the directory name equal the canonical id"（:1387）
  ——**未完成**。仍按旧 id（=seed 字符串）命名的产物：`sessions/{id}/*`、
  `ringing-timeline/{id}.json`、`ringing-offload/{id}.jsonl`、`timeline-audit/{id}.jsonl`、
  `index.jsonl` 旧条目、`.active_session`、CLI `--seed`（`daemon/main.rs:199-240`）、
  HTTP `{seed}` 路径段。
- 例外说明：`gate/types.rs:210` 的 `session.seed` 是 opencode/muse **外部 provider** 的
  请求头兼容，不属本仓协议。

## 6. 对 `docs/plan-legacy-protocol-cleanup.md` 的修订

**已失效 / 需改写：**
- §4 `ToolCapabilities.streaming` 撞车——已按注释路线处理，且字段无生产读取点。
- §4 行数三口径——聚合口径已定后端权威，估算通道已接线被消费；只剩展开视图逐文件
  自算一处，且是分工而非撞车。
- §1 表中 service_methods.rs 计数漂移（5 → 26，几乎全是测试断言）；
  `qaqh-skills/src/session_state.rs` 已 0 处 legacy。
- §3 `ToolCallPrepared` 的处置选项里"接进投影"不再成立——v1 快照投影
  `ringing/projection.rs` 已退化为 orphan_seal 收敛 + 测试入口（`hub.rs:524-577`），
  无 UI 前途；只剩"删"。

**仍有效：** §2 seed 内部改名（热区行号有漂移）；§3 ts-rs 收窄（扩大为 156/190）；
§3 `ToolCallProgress` O(n²)；§4 spec-file-mutation-delta 零实现；§5 发布文档四条
（本轮未复核）。

**计划外新发现（本文档独有）：**
1. MCP/LSP 动态工具旧执行链——**协议桥的真身**，计划完全没提。
2. canonical events.jsonl 与 legacy message store 双写 + 双栅栏互不知晓。
3. fire-into-void 是整张 DomainEvent 枚举，不只两个变体。
4. `Delivery` 分类链死代码。
5. qaqh-ringing v1 类型层五个死面（capability.rs 文档撒谎、RingingCommandEnvelope、
   RingingEvent 载体、RingingChannelSnapshot 链、RINGING_VERSION=1）。
6. `/control/v1/*` 活跃但命名 v1；discovery pre-0.9 兼容红线。
7. `driver_watch.rs` 的 seed 键会落盘（改名牵动数据文件）。
8. `to_tool_result()` 折回 v1 形态是共同收敛面，别当旧协议删。

## 7. 建议移除顺序

**P0 死代码直接删（编译器兜底，一次或两个 PR 可清完）：**
v1 `register()` + 迁移 ~9 处测试到 `register_typed`；`LegacyToolAdapter` 执行面
（`execute_legacy`/`execute`/`map_tool_result`/P2.5 警告）；execution.rs skills 特判；
`ClientOpenRequest/Response`（+修 capability.rs 文档头 + `client/types.rs:33` 再导出 +
ts 绑定）；`RingingCommandEnvelope`（保 Ack 三件套）；`RingingChannelSnapshot`+
`SnapshotProjector`+`hub.rs:561-575`；`RINGING_VERSION=1`；`Delivery` 分类链；
`wal::read_ops`；`rename_session`；`canonical_identity_for_session`（或转正）；
前端 `bootstrap()` 占位；ts-rs 白名单化（156 死类型；`Projection*` 先转正再定）；
两条过时注释。

**P1 换载体后删（每项一个独立 PR + 全量测试）：**
1. MCP/LSP 动态工具 typed 化：`DynamicTool.handler_fn` 改 `TypedTool`/`ErasedTool`
   （dispatcher 内部仍可轮询，外壳换契约）→ 删 `RegisteredTool.legacy` 字段、
   `PreparedExecutor::Legacy` 臂、`ToolCallCtx` 兼容字段、`install_tool_call_context`
   TLS 视图（逐步）、`{name}_{action}` 后缀解析（`util/format.rs:7-17` +
   `ToolInvocation.action` 字段）、改写 `manager.rs:932-933` 断言。
2. 删 `ToolEvent::ToolCallPrepared`/`CodeChanged`（连带重审整张 DomainEvent 枚举
   谁还在桥上被匹配）；`RingingEvent` writer 载体换 timeline intent 后删除。
3. `ToolCallProgress` 改携带片段 + 累计长度（gate 三适配器）。

**P2 存量数据审计后删（每项先写审计查询，确认零命中再删）：**
§3.2 全部 migrate-on-read 项（compact_skip / index.json / workspace.txt /
timeline-v3 / journal 字段 / tool_outbox.wal / DeepX marker / provider_id / 明文 key /
扁平 model / 权限 u8 / `normal` alias / timeline 旧 JSON 槽位）；discovery pre-0.9
兼容（先确认旧客户端存活）；`/control/v1/*` 改名（配合 discovery 版本面）。

**P3 独立工程（是"新协议未完成"，不是"旧协议没删"）：**
canonical 接管 message/journal 写 → 收敛 `LegacyWriterFacade` 双栅栏为单一
`events.lock`；BETA-01 目录名=canonical id（启用 `rename_session`，退役 seed 目录
解析与 `ringing-driver-watch.json` seed 键）；`to_tool_result()` 下游改吃
`ToolOutcome`（若要删折回层）；spec-file-mutation-delta 开工或归档。
