# QAQH v2 canonical fact 迁移消费者与 writer/reader 盘点

> 基线：`origin/betav2 @ 66539a0`
> 日期：2026-09-20
> Issue：[#117](https://cnb.cool/QAQ-Harness/qaqh-backend/-/issues/117)
> 范围：只读源码盘点，不修改运行时行为。

## 0. 方法与结论边界

本报告以当前仓库源码中的实际符号、路径和锁/持久化调用为依据，不把文档中的目标状态当作已实现事实。

当前 v2 canonical fact 类型仍在 PR #115 分支 `feat/111-session-fact-types @ 80928e9`，尚未进入 `betav2`；`events.jsonl`、`SessionStore`、`SessionActor`、ProjectionSet 和 recovery batch 也尚未实现。因此本报告把现有链路按“可保留的派生读侧”“必须等待 canonical fact”“必须等待 SessionActor/ToolRuntime”三类登记。

## 1. 执行摘要

当前没有一个单一的 v2 canonical fact store。实际持久化事实分散在：

1. `sessions/{seed}/messages.jsonl`：消息归档，当前 resume、conversation snapshot 和 timeline rebuild 的主要输入。
2. `sessions/{seed}/messages.wal`：message persist op 的 L2 WAL，只负责把未 drain 的写操作补回 `messages.jsonl`。
3. `sessions/{seed}/compact-context.json`：模型可见上下文检查点，是模型 resume 的优先视图，不是 canonical session history。
4. `sessions/{seed}/meta.json` 与 `sessions/index.json`：session metadata 和列表索引。
5. `ringing-timeline/{seed}.json`、`ringing-offload/{seed}.jsonl`、`timeline-audit/{seed}.jsonl`：timeline 物化快照、已 seal turn 的 offload sidecar、轻量 timeline 审计。
6. Ringing V1 内存 `RingingHub`/`TimelineAppender`：每频道 `stream_seq`、可靠 replay journal、snapshot baseline；进程内 journal 不跨 daemon 崩溃持久化。
7. `tool-outbox`：工具已执行的 call_id 记录，用于恢复时区分“执行过但结果丢失”和“从未执行”。
8. `audit.csv` + `audit/v2.jsonl`：工具执行审计双写账本，独立于 session projection。

v2 canonical fact 落地后，现有链路应被重新定位为：

- `messages.jsonl`：在 canonical fact 存在期间可保留为兼容读侧/迁移输入，但不能再作为唯一 session history。
- `compact-context.json`：继续是模型投影/上下文检查点，不升级为 canonical source。
- timeline snapshot/offload：继续是 derived projection，必须能从 canonical fact 重建；不能反向成为事实源。
- Ringing `stream_seq` journal：继续是 wire replay 机制，不是 canonical ordering。
- tool outbox：继续是 ToolRuntime 的执行恢复辅助账本；不能替代 `ToolIntent/ToolFinished`。
- audit v2：继续是全局审计账本，不从 session fact 重建，也不承载 session projection。

## 2. 分类定义

| 分类 | 含义 | 迁移动作 |
|---|---|---|
| `read-first` | 当前读侧可从 canonical fact 双读/切读，不改变现有写路径 | 先加 read adapter，保留旧路径 fallback |
| `canonical-wait` | 语义上必须等待 canonical fact/`events.jsonl` 才能切换 | 保留旧 writer/reader，直到 P1 store 完成 |
| `actor-wait` | 需要 `SessionActor` 成为唯一 writer 后才能改变所有权 | 不得在 P1 前改运行时写路径 |
| `tool-runtime-wait` | 需要 `ToolRuntime`/Tool SDK 的 typed output、intent、policy、recovery 边界 | 只做兼容映射，不改工具执行契约 |
| `keep-derived` | 永久保留为派生投影或 sidecar，不作为事实源 | 只保证可从 canonical fact 重建/对账 |

## 3. 逐层盘点

### 3.1 `messages.jsonl`

**写入生产者**

- `crates/qaqh-session/src/store/mod.rs:47-59`：`append_one` 直接 append 一行并 `flush + sync_all`。
- `crates/qaqh-session/src/store/mod.rs:63-77`：`append_messages` 批量 append 后 `flush + sync_all`。
- `crates/qaqh-session/src/store/mod.rs:81-96`：`rewrite_messages` 通过 temp + rename 重写整个文件，用于 undo/compact。
- `crates/qaqh-session/src/manager.rs:891-960`：`SessionManager::save_one`。
- `crates/qaqh-session/src/manager.rs:962-1023`：`SessionManager::save_full`。
- `crates/qaqh-session/src/manager.rs:1024-1060`：`SessionManager::save_append`，是 live append 的主入口。

**读取消费者**

- `crates/qaqh-session/src/manager.rs:255-257`：`SessionManager::load`。
- `crates/qaqh-session/src/manager.rs:272-305`：`load_for_resume`，先 `replay_message_wal`，再加载 archive 与 compact context。
- `crates/qaqh-session/src/manager.rs:321-335`：`load_recent_for_projection`，compact 优先，否则读 archive tail。
- `crates/qaqh-session/src/manager.rs:356-364`：`load_archive_tail`，只读 append-only archive，无视 compact。
- `crates/qaqh-runtime/src/ringing/conversation_snapshot.rs:12-43`：`persisted_conversation_state`，从 `load_for_resume` 构建 conversation snapshot。
- `crates/qaqh-runtime/src/ringing/timeline_rebuild.rs:33-52`：`rebuild_timeline_snapshot`，通过 `load_archive_tail` 重建 timeline。

**当前事实源关系**

`messages.jsonl` 是当前消息归档的 append-only 文件；`compact-context.json` 是模型可见视图；timeline snapshot 是从 messages 重建的派生读侧。三者语义并不相同。

**迁移分类**

- 写入路径：`canonical-wait`。在 `SessionActor` 成为唯一 writer 前，不应把 messages 写路径直接替换成 canonical fact 写路径。
- 读取路径：`read-first`。可以为 conversation snapshot、timeline rebuild 增加 canonical fact 优先的 read adapter；没有 canonical log 时继续读 messages/compact。
- 风险：`messages.jsonl` 不持久化 turn 终态；timeline rebuild 目前把重建 turn 统一标为 `Completed`（`timeline_rebuild.rs:58-60`），切到 canonical fact 后必须改为读取真实 `TurnFinished/TurnInterrupted`。

### 3.2 `messages.wal`

**定义与写入**

- `crates/qaqh-message/src/wal.rs:3-31`：WAL 布局说明。
- `crates/qaqh-message/src/wal.rs:42`：`WAL_FILE_NAME = "messages.wal"`。
- `crates/qaqh-message/src/wal.rs:72-198`：`WalWriter`、`open`、`log_op`、`sync`、`checkpoint`。
- `crates/qaqh-message/src/store.rs:309-368`：`MessageStore::flush_meta` 在 op 入 drain queue 前写 WAL。
- `crates/qaqh-message/src/store.rs:373-397`：`enable_wal` 与 `wal_checkpoint`。

**恢复消费者**

- `crates/qaqh-session/src/manager.rs:437-534`：`SessionManager::replay_message_wal`，读取、重放、去重并 checkpoint。
- `crates/qaqh-message/src/wal.rs:230-273`：`open_reader`、损坏/不可读尾部隔离。
- `crates/qaqh-message/src/wal.rs:762-780`：`checkpoint_file`。

**迁移分类**

`canonical-wait` / `keep-derived`。WAL 当前解决的是 message persist op 的崩溃窗口，不是 canonical fact 的 durable ordering。P1 引入 `events.jsonl` 后，WAL 可继续作为旧 message 写路径的兼容恢复机制，直到该写路径退役；不能用 WAL 代替 canonical fact 的 fsync/commit marker。

### 3.3 `compact-context.json`

**生产者与消费者**

- `crates/qaqh-session/src/manager.rs:539-592`：`save_compact_context`。
- `crates/qaqh-session/src/manager.rs:1397-1433`：`compact_context_path`、`read_compact_context_checked`。
- `crates/qaqh-session/src/manager.rs:272-305`：resume 优先读取 compact context。
- `crates/qaqh-session/src/manager.rs:321-335`：projection 路径 compact 优先。
- `crates/qaqh-runtime/src/ringing/conversation_snapshot.rs:16-23`：conversation snapshot 使用 compact 优先的 active view。

**迁移分类**

`keep-derived`。compact context 是模型可见面/检查点，不是 canonical history。v2 后它应可从 canonical fact + compaction checkpoint 重建，但当前恢复路径仍可保留。

**风险**

`timeline_rebuild.rs:26-32` 明确 timeline 不能读 compact 视图，否则会抹掉人类 transcript；v2 canonical fact 切读后必须保持这条“模型面 ≠ 人类 transcript”边界。

### 3.4 Session metadata、index 与 legacy migrate

**Session metadata**

- `crates/qaqh-session/src/session_meta.rs:1-5`：`SessionMeta` 目前只是 `qaqh_types::SessionMeta` 的 re-export。
- `crates/qaqh-session/src/store/mod.rs:21-33`：`write_meta`，temp + flush + `sync_all` + rename。
- `crates/qaqh-session/src/store/mod.rs:37-41`：`read_meta`。
- `crates/qaqh-session/src/manager.rs:594-628`：`load_meta`。
- `crates/qaqh-session/src/manager.rs:630-685`：mode/tool mode/skills/frozen annotation 的 meta 写路径。
- `crates/qaqh-session/src/manager.rs:782-889`：新 session、usage 等 meta 写路径。

**Index**

- `crates/qaqh-session/src/store/mod.rs:408-430`：`read_index`。
- `crates/qaqh-session/src/manager.rs:1369-1395`：index sync/持久化边界说明。

**Legacy migrate**

- `crates/qaqh-session/src/migrate.rs:1-10`：旧 TOML → JSONL 布局说明。
- `crates/qaqh-session/src/migrate.rs:45-92`：`run`，启动时发现 legacy session。
- `crates/qaqh-session/src/migrate.rs:94-120`：`migrate_one`，写 `messages.jsonl` 与 `meta.json`。

**迁移分类**

`keep-derived` / `read-first`。meta 和 index 是 session 元数据/列表投影，不是 canonical fact；v2 迁移应把 `SessionCreated`、`SessionMetadataChanged`、`SessionTitleChanged` 等作为事实源，并让 meta/index 可由事实重建。legacy migrate 是一次性输入适配，不能扩展成新的 canonical writer。

### 3.5 Timeline snapshot、journal、offload 与 timeline audit

**Timeline 持久化**

- `crates/qaqh-runtime/src/timeline_store.rs:1-19`：模块注释明确 `ringing-timeline/{seed}.json` 是快照，内存 journal 只负责进程内 replay，崩溃恢复走 `timeline_rebuild`。
- `crates/qaqh-runtime/src/timeline_store.rs:38-55`：`TimelineStore` 路径与水位状态。
- `crates/qaqh-runtime/src/timeline_store.rs:64-85`：`TimelineStore::new`，创建 `ringing-timeline` 与 `timeline-audit`。
- `crates/qaqh-runtime/src/timeline_store.rs:174-205`：`persist`，原子替换 snapshot + journal。
- `crates/qaqh-runtime/src/timeline_store.rs:237-267`：`list_seeds`、`load_seed`、`path_for`。

**Offload sidecar**

- `crates/qaqh-runtime/src/timeline_store.rs:87-121`：`ringing-offload/{seed}.jsonl`，`append_offloaded_turn`。
- `crates/qaqh-runtime/src/timeline_store.rs:124-172`：`load_offloaded_turn` 与 offset 索引。

**内存 timeline**

- `crates/qaqh-runtime/src/timeline.rs:339`：`TimelineAppender`。
- `crates/qaqh-runtime/src/timeline.rs:721`：`apply_intent`。
- `crates/qaqh-runtime/src/timeline.rs:888-931`：offload enable/candidate/mark。
- `crates/qaqh-runtime/src/timeline.rs:940-963`：`replay_since` 与 `snapshot`。

**TimelineHub 读写边界**

- `crates/qaqh-runtime/src/ringing/timeline_hub.rs:427-455`：`publish_timeline`，发布后请求异步持久化。
- `crates/qaqh-runtime/src/ringing/timeline_hub.rs:459-511`：`persist_timeline_sync`，offload → snapshot → audit → persist 的顺序。
- `crates/qaqh-runtime/src/ringing/timeline_hub.rs:515-539`：enable/offload。
- `crates/qaqh-runtime/src/ringing/timeline_hub.rs:734-785`：`rehydrate_timeline_page` 与 `subscribe_timeline`。

**Timeline audit**

- `crates/qaqh-runtime/src/timeline_store.rs:280-337`：`append_audit`。
- `crates/qaqh-runtime/src/timeline_store.rs:341-370`：audit watermark 与 rotate。
- `crates/qaqh-runtime/src/timeline_store.rs:38-39`：路径 `timeline-audit/{seed}.jsonl`。

**迁移分类**

`keep-derived` / `read-first`。timeline snapshot 是 canonical fact 的派生投影，必须能从 canonical fact 重建；offload 是 timeline 的 sidecar，不是事实源；timeline audit 只用于近期事故定位，不应成为 canonical ordering 或 session history。

**风险**

- `TimelineStore::persist` 目前使用 temp + rename，但 `append_offloaded_turn` 只 `flush`，没有显式 `sync_all`（`timeline_store.rs:110-116`）。
- timeline 重建只覆盖最近 40 轮（`timeline_rebuild.rs:14-17`），canonical fact 切读后要重新定义“完整历史”与分页边界。

### 3.6 Ringing V1 wire、replay 与客户端消费

**服务端 wire 与 replay**

- `crates/qaqh-ringing/src/snapshot.rs:13-27`：`RingingChannelSnapshot`，包含 `baseline_stream_seq`、`state_revision`、`snapshot_version`。
- `crates/qaqh-ringing/src/snapshot.rs:50-80`：`RingingSessionBootstrap`，三频道快照。
- `crates/qaqh-ringing/src/event.rs:25`：`RingingEvent`。
- `crates/qaqh-runtime/src/ringing/hub.rs:299`：`RingingHub`。
- `crates/qaqh-runtime/src/ringing/hub.rs:862-929`：`publish`/`publish_with_causation` 与 `stream_seq`。
- `crates/qaqh-runtime/src/ringing/hub.rs:1118-1202`：`replay_since` / `replay_channel_since`。
- `crates/qaqh-runtime/src/ringing/hub.rs:1207-1242`：`snapshot` 与 `checkpoint`。
- `crates/qaqh-runtime/src/ringing/router.rs:256-277`：内存 reliable journal 的 `replay_since` / `last_stream_seq`。
- `crates/qaqh-runtime/src/ringing/outbox.rs:116`：replaceable 事件按 `stream_seq` 清理。

**客户端消费**

- `crates/qaqh-client/src/sse.rs:84`：`ChannelStatus::Reconnecting`。
- `crates/qaqh-client/src/sse.rs:129`：发送 `Last-Event-ID`。
- `crates/qaqh-client/src/sse.rs:221-259`：`ringing.stream_terminated` 与 envelope/cursor 校验。
- `crates/qaqh-client/src/timeline.rs:8-9`：timeline gap recovery 重新拉 snapshot 并以 watermark 推进 cursor。
- `crates/qaqh-client/src/timeline.rs:153`：`TimelineStatus::Reconnecting`。
- `crates/qaqh-client/src/timeline.rs:228`：timeline `Last-Event-ID`。
- `crates/qaqh-client/src/timeline.rs:350-378`：snapshot 重基线化。

**迁移分类**

`canonical-wait`。Ringing 的 `(epoch, channel, stream_seq)` cursor 与 v2 canonical `(log_id, fact_seq, projection_index)` 不是同一坐标系。P1 必须先完成 v1 adapter 的 1:N mapping、snapshot/replay window 和 cursor 映射，再允许 wire 读侧切到 canonical fact。

**风险**

客户端当前把 `ringing.stream_terminated` 压成 `ClientError::Transport` 字符串（`sse.rs:224-231`），这正是 issue #112 要结构化 `reason` 的原因；在 canonical cursor 切换前，不应把 wire `stream_seq` 当作 canonical fact sequence。

### 3.7 Tool outbox

**写入与 flush**

- `crates/qaqh-runtime/src/agent/tool_outbox.rs:310-322`：`OutboxRecord` 与 `outbox_path`。
- `crates/qaqh-runtime/src/agent/tool_outbox.rs:345-401`：`record_in`，工具执行完成后写入 outbox，记录 `call_id`、工具名、status、ts。
- `crates/qaqh-runtime/src/agent/tool_outbox.rs:413-435`：`flush_in` / `flush`。
- `crates/qaqh-runtime/src/agent/tool_outbox.rs:438-448`：`executed_call_ids`。

**恢复与对账**

- `crates/qaqh-runtime/src/agent/tool_outbox.rs:549-586`：`reconcile_store` / `reconcile_store_in`，把 outbox 与恢复后的 `MessageStore` 对账。
- `crates/qaqh-message/src/store.rs:399-426`：synthetic `[RESTORE]` repair 与 `amend_synthetic_repair`。
- `crates/qaqh-runtime/src/agent/loop_core.rs:484-488`：worker teardown 前 `flush`。

**迁移分类**

`tool-runtime-wait`。outbox 是当前 ToolRuntime 的“已执行”辅助证据，不是 canonical `ToolIntent/ToolFinished`。v2 后应：

- `ToolIntent` durable 后再执行；
- `ToolFinished` 成为唯一 call 终态；
- outbox 作为迁移期对账输入，不能继续承担 canonical terminal 语义。

### 3.8 Audit v1/v2

**写入**

- `crates/qaqh-workspace/src/audit/mod.rs:1-8`：v1 CSV + v2 JSONL 双写说明。
- `crates/qaqh-workspace/src/audit/mod.rs:237-260`：`append_audit` 与显式路径版本。
- `crates/qaqh-workspace/src/audit/v2.rs:290-347`：`append_event` / `append_event_with_limit`，补信封、链哈希、append + 权限设置。
- `crates/qaqh-workspace/src/audit/v2.rs:33`：`SCHEMA = "qaqh.audit/v2"`。

**迁移分类**

`keep-derived` / `tool-runtime-wait`。audit 有独立全局 seq/hash chain，不属于 session projection，也不能从 `events.jsonl` 重建。它需要与 `ToolRuntime` 的 canonical tool result 建立引用/对账，但不能被 canonical fact 取代。

## 4. P1 可立即处理项

以下工作不依赖 `SessionActor` 或 canonical store，可先做：

1. **canonical types 与 golden fixtures**
   - 当前由 PR #115 承载；只提供类型、serde 和校验。
2. **只读消费者盘点与接口草案**
   - conversation snapshot、timeline rebuild、timeline page rehydrate 的 read adapter 接口草案。
3. **v1 cursor → v2 cursor 映射测试设计**
   - 基于现有 `RingingHub` replay、client `Last-Event-ID` 和 snapshot baseline，先写映射/反证测试，不改生产写路径。
4. **tool outbox → ToolIntent/ToolFinished 对账字段映射**
   - 只新增映射表/报告/测试设计，不改变 outbox 写入。
5. **meta/index → SessionCreated/SessionMetadataChanged 映射**
   - 只读盘点，不改现有 `meta.json` 写路径。
6. **timeline/offload/audit 的可重建性检查**
   - 为每个派生文件建立“从 canonical fact 重建/对账”的验收条目。

## 5. 必须等待 canonical fact / SessionActor / ToolRuntime 的项

| 项 | 等待对象 | 原因 |
|---|---|---|
| `messages.jsonl` 写路径切换 | `SessionActor` + canonical store | 当前 writer 分散在 SessionManager/MessageStore，必须先有唯一 writer |
| `events.jsonl` / writer lock / commit marker | canonical store | 当前不存在 |
| projection rebuild / composite cursor | canonical fact + ProjectionSet | 当前 Ringing cursor 与 fact cursor 坐标系不同 |
| `ToolIntent/ToolFinished` canonical lifecycle | ToolRuntime | 当前只有 tool outbox 与消息结果 |
| audit → tool result 引用 | ToolRuntime + canonical typed output | audit 当前独立账本 |
| TUI reducer/wire boundary | canonical projection + fixed TUI rev | 跨仓依赖未固定 |
| cutover/rollback CLI | canonical store + legacy adapter | 当前无 canonical log 可切换 |

## 6. 风险、feature gate 与 rollback 边界

1. **双写期间不能把两个事实源都当权威**
   - messages/compact/timeline/ringing 都必须明确是“旧源”还是“派生投影”。
2. **timeline 不能从 compact 视图重建**
   - 该边界已在 `timeline_rebuild.rs:26-32` 记录，迁移时必须保留。
3. **Ringing `stream_seq` 不得冒充 canonical `fact_seq`**
   - 两者是不同坐标系，必须通过 adapter 做 1:N 映射。
4. **offload sidecar 当前不是 durable canonical evidence**
   - `append_offloaded_turn` 只有 `flush`，迁移期不能把它当 commit marker。
5. **rollback 需要同时覆盖**
   - canonical writer 关闭；
   - projection read adapter 回退；
   - legacy messages/compact 读侧恢复；
   - tool outbox/audit 对账不重复执行。
6. **feature gate 建议**
   - `session_fact_v2_read`
   - `session_fact_v2_write`
   - `session_fact_v2_projection`
   - `session_fact_v2_cutover`
   - 四个 gate 分开，避免 read/write 同时切换。

## 7. 尚未确认项与所需证据

1. `messages.jsonl` 的 append-only 历史是否必须逐条映射为 canonical facts，还是只保留迁移快照。
2. compact context 的 `archive_message_count` 与 canonical `last_good_fact_seq` 的稳定映射公式。
3. timeline snapshot watermark 与 canonical composite cursor 的分页/过期边界。
4. offload sidecar 是否纳入 v2 content GC 的 retention/legal-hold 范围。
5. tool outbox 的 `call_id/status/ts` 如何映射到 `ToolIntent/ToolFinished`，尤其是 `denied/cancelled` 无 intent 路径。
6. audit v2 的 `seq/hash` 与 `ToolFinished.evidence_*` 是否需要显式交叉引用。
7. `RingingHub` 进程内 journal 跨重启丢失的现有降级是否继续保留，还是由 canonical replay window 替代。
8. TUI pin bump 的具体 backend rev 与 reducer/wire boundary 验收时机。

## 8. 参考路径索引

- Session store：`crates/qaqh-session/src/store/mod.rs`
- Session manager：`crates/qaqh-session/src/manager.rs`
- Message store/WAL：`crates/qaqh-message/src/store.rs`、`crates/qaqh-message/src/wal.rs`
- Session meta/migrate：`crates/qaqh-session/src/session_meta.rs`、`crates/qaqh-session/src/migrate.rs`
- Timeline store：`crates/qaqh-runtime/src/timeline_store.rs`
- Timeline runtime：`crates/qaqh-runtime/src/timeline.rs`
- Timeline hub/rebuild：`crates/qaqh-runtime/src/ringing/timeline_hub.rs`、`crates/qaqh-runtime/src/ringing/timeline_rebuild.rs`
- Conversation snapshot：`crates/qaqh-runtime/src/ringing/conversation_snapshot.rs`
- Ringing hub/router/outbox：`crates/qaqh-runtime/src/ringing/hub.rs`、`crates/qaqh-runtime/src/ringing/router.rs`、`crates/qaqh-runtime/src/ringing/outbox.rs`
- Ringing wire：`crates/qaqh-ringing/src/event.rs`、`crates/qaqh-ringing/src/snapshot.rs`
- Client consumption：`crates/qaqh-client/src/sse.rs`、`crates/qaqh-client/src/timeline.rs`
- Tool outbox：`crates/qaqh-runtime/src/agent/tool_outbox.rs`
- Audit：`crates/qaqh-workspace/src/audit/mod.rs`、`crates/qaqh-workspace/src/audit/v2.rs`
