# Handoff:把压缩三态镜像进 v2(上下文窗口面板 / 压缩 UI 的后端联动)

日期:2026-10-09
提出方:桌面壳(qaqh-desktop-app)前端
范围:仅后端仓 `qaqh-backend`。宿主(`src-tauri`)侧我自己落地,不在本单。

## 1. 目标

桌面端要做三件事,都需要"压缩进行中"的过程数据:

1. 输入区上下文面板里的「压缩上下文」按钮 → 点了要能看到**进行中**;
2. chatview 里显示 `正在压缩…` 分隔条,并带 `turns_keeping / turns_total` 进度;
3. 把压缩时模型的流式摘要**渲染成一张工具卡片**(不是真工具,是压缩产物),结束后显示 `已压缩`。

## 2. 现状(已核实,含行号)

**已具备**

| 能力 | 位置 |
|---|---|
| 手动压缩命令 `ConversationCommand::ConversationCompact{turn_id}` | `crates/qaqh-domain/src/command.rs:289`(注释已写明"accepted 不代表成功;`CompactFinished` 才是终态") |
| 三个过程事件(域事件) | `crates/qaqh-domain/src/event.rs:355-374`: `CompactStarted{compact_id,turns_total,turns_keeping}` / `CompactProgress{compact_id,delta}`(**replaceable 流式摘要**)/ `CompactFinished{compact_id,status,summary_chars,turns_compacted,turns_removed}` |
| 终态事实 + v2 投影 | `ConversationDelta::CompactionApplied{checkpoint_id,replaces_through_fact_seq,summary,context_revision}`;快照 `ConversationCompactionState`(`crates/qaqh-session/src/projection/conversation.rs:55`) |
| 发射点 | Started: `crates/qaqh-runtime/src/agent/plugins/engine_compact.rs:152`;Progress: `crates/qaqh-runtime/src/agent/engine_turn.rs:1246`;Finished: `engine_compact.rs:359`(`publish_compaction_fact`)+ `engine_turn.rs:1327` |

**缺口**

- 三个过程事件都是 **`ConversationEvent`(Ringing 控制频道)**,而 v2 两个 delta 频道都没有 compact:
  `ConversationDelta` 8 个变体(assistant_block_sealed / compaction_applied / input_accepted / tool_call_declared / tool_finished / turn_finished / turn_interrupted / turn_started)、`ControlDelta` 12 个变体(activity / driver_changed / interaction_* / round / session_* / subagent_* / tool_*)。
- 桌面宿主的 `ClientHandlers`(`qaqh-desktop-app/src-tauri/src/events.rs`)**没有订阅 Ringing 控制事件**的回调,只有 `on_timeline_*` / `on_v2_event` / `on_v2_reset` / `on_liveness`。所以走 Ringing 这条路 = 宿主还要新增一条事件转发,且前端要认第二条流。

**结论:把三个瞬态事件镜像进 v2 的 conversation 频道**,前端只认一条流(与既有"单一流"原则一致)。

## 3. 接口契约(前端已按此实现)

在 `ConversationDelta` 增加三个变体,字段与域事件逐字段同构,并沿用其余变体的 `revision`:

```rust
// crates/qaqh-session/src/projection/conversation.rs(or qaqh-types 的 ConversationDelta 定义处)
CompactStarted {
    revision: u64,
    compact_id: String,
    turns_total: u32,
    turns_keeping: u32,
},
/// replaceable:同一 compact_id 覆盖合并
CompactProgress {
    revision: u64,
    compact_id: String,
    delta: String,
},
CompactFinished {
    revision: u64,
    compact_id: String,
    status: CompactStatus,          // Completed | Skipped | Failed | Cancelled
    #[serde(default, skip_serializing_if = "Option::is_none")]
    summary_chars: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    turns_compacted: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    turns_removed: Option<u32>,
},
```

线形状(双层 tag/content,与其余 delta 一致):

```json
{"kind":"conversation_delta","data":{"kind":"compact_progress","data":{"revision":7,"compact_id":"01J...","delta":"…"}}}
```

**契约约束**

- 这三个**必须走 conversation 频道**,不能走 control:摘要卡片与回合的先后顺序要和 transcript 同序(前端按到达顺序插卡片)。
- **不能走 fact 链**。它们是瞬态事件,不是事实;v2 的 fact 投影是重放确定性的,把瞬态塞进 fact 会破坏重放语义。已有的 `CompactionApplied` 才是事实,它继续管"已压缩"的持久水位与快照恢复。
- `delta` 允许重复推送,前端按 `compact_id` 覆盖(与 `Delivery::Replaceable` 语义一致)。
- 兼容性:前端投影表是**字符串键**的(`qaqh-desktop-app/src/session/projection.ts:59`),未知 kind 直接忽略 → 后端先发、前端后跟也安全,无需版本协商。

**放置锚点(暂不需要,勿实现)**

若将来要把压缩卡片**嵌进时间线**(插在"被压缩的第 N 个回合之前"),需要一个前端可用的锚点:`replaces_through_fact_seq` 是 canonical fact seq,而前端 turn 的稳定 key 是全局序号 `#i`(canonical turn_id 会复用,`qaqh-desktop-app/src/session/reducer.ts:561-572`),两套计数对不上;现状只能靠"当前最后一个 slot"这种启发式,重载后必然丢。

**本期不做**:卡片是实时专属的(刷新即消失,产品上已接受),所以上述锚点(如 `turns_removed` / `first_kept_ordinal` 同时进 `compaction_applied` 与持久化压缩条目)**不要求实现**。等真要嵌入时间线时再补,不影响本单其余部分。

## 4. 实现落点

1. **类型**:在 `ConversationDelta` 定义处加三个变体;跑 `cargo test -p qaqh-domain -p qaqh-session`。
2. **发布**:照 **`crates/qaqh-runtime/src/ringing/v2.rs:442 publish_title_changed`** 这个既有模板写一个 hub helper(它已经是"瞬态 delta 直发"的范例:`ProjectionEvent{ stream_key: Channel(Control), delivery: Replaceable{revision}, payload: ProjectionPayload::MetaDelta(...) }`)。建议新增 `publish_compact_started/progress/finished`,把 `stream_key` 设成 `Channel(Conversation)`、`payload` 设成 `ProjectionPayload::ConversationDelta(...)`。
3. **接发射点**:
   - `engine_compact.rs:152`(Started)与 `:359`(Finished,failed/cancelled 分支也要发);
   - `engine_turn.rs:1246`(Progress,每个流式 delta;建议沿用该处已有的节流节奏,避免把 conversation 频道打成高频)。
4. **快照/重连边界(顺手补)**:`qaqh-domain/src/state.rs:49-50` 已有 `compact_status/compact_id` 字段,但快照里的 `ConversationCompactionState` 只表达终态。若希望"刷新/重连后仍显示进行中",需要让快照带上 running 态;这一条**可选**,前端在拿不到时会退回"按钮点击后本地乐观 running"。
5. **类型导出**:`just ts-export`(在 desktop 仓跑),生成物 `src/api/qaqh/*.ts` 会有新变体。

## 5. 已/未按此契约实现的桌面侧

- 已实现(我这边):`ConversationCompact` 的宿主命令、压缩状态机、`正在压缩…` 分隔条、流式摘要卡片、`已压缩` 终态、面板按钮。它们在**收不到新 delta 时也能工作**(点按钮 → 乐观 running;收到 `CompactionApplied` → 结束)。
- 未实现(等本单):过程事件一到,`正在压缩` 的进度数字与摘要流式就会从"乐观占位"变成真实数据。

## 6. 验收

- `cargo test --workspace --lib` 全绿;
- 手动压缩一轮,`tcpdump`/日志确认 conversation 频道出现 `compact_started → compact_progress* → compact_finished`,且顺序在摘要卡片之前/之后与 transcript 一致;
- 桌面端联调:点压缩 → 进度条动 → 摘要卡片逐字出现 → 卡片收起为 `已压缩`;
- 重连后不发生"卡片重复出现"(以 `compact_id` 去重)。

## 7. 附录:附件链路(同属后端仓,但 daemon 侧无需改动)

结论:**daemon 面已齐,只缺 client 的一次封装。**

- 上传面已存在:`POST /ringing/v2/content`(`crates/qaqh-daemon/src/axum_server/axum_impl/mod.rs:204` → `content.rs:194 handle_content_upload`),multipart 字段固定为 `session_id` / `media_type` / `content`,鉴权走 `Scope::Interact` + lease 归属,返回 `{content_id, media_type, sha256, size, truncated}`;`content_id` 即 canonical `ContentRef.content_id`。
- 读取面已存在:`GET /ringing/v2/content/{content_id}`(`content.rs:12`),带归属校验与 Range,未命中回落每会话 `blobs/`。
- **缺**:`qaqh-client` 只有下载(`client.rs:672 download_content` / `:701 download_content_by_id`),没有上传。请在 `client.rs` 补 `upload_content(session_id, media_type, bytes) -> ContentRef`,内部照 `handle_content_upload` 的 multipart 形状组包,并对回包校验 `sha256` 与字节数。

宿主侧(`tauri-plugin-dialog` 选文件 → 读字节 → 调 `upload_content`)与前端(附件入口、上传态、发送带 refs)我在 desktop 仓做,依赖上面这个 client 方法。

## 8. 后端落地记录(2026-10-09,本仓已实现)

**先更正 §2 的现状**:v1 Ringing 广播面已在 hub-fact-bus 阶段 3d 删除(`crates/qaqh-runtime/src/actor.rs:70-85`,worker 域事件只剩副作用迁移 + activity 状态机)。所以三个 `Compact*` 域事件在落地本单前**到不了任何客户端**,桌面宿主"没订阅 Ringing 控制事件"不是缺口而是既成事实——本单是**唯一出口**,不是双发。

**已按契约实现**

- 类型:`qaqh-session/src/session_fact_v2/types.rs` 新增 `CompactStatus{completed|skipped|failed|cancelled}`;`projection_event.rs` 的 `ConversationDelta` 新增 `CompactStarted/CompactProgress/CompactFinished`,字段与本单 §3 逐字段一致,线形状仍是双层 tag/content。
- 发布:`ringing/v2.rs` 新增 `publish_compact_started/progress/finished`(共用 `publish_compact_delta`),`stream_key = Channel(Conversation)`、`delivery = Replaceable{revision}`,照 `publish_title_changed` 模板。不进 fact 链、不进快照,§3 的"不能走 fact 链"保持。
- 接点:`ringing/compact_mirror.rs`(新)+ `actor.rs` 的 `WriterEvent::Ringing` 分支。**引擎发射点一行未改**——自动压缩、手动压缩、`loop_outcome` 的 cancelled/skipped/failed 兜底因此自动全覆盖。

**两处契约澄清(前端需对齐)**

1. `CompactProgress.delta` 是**累积全文快照**,不是 provider 增量块。这是 §3"前端按 `compact_id` 覆盖"唯一能成立的读法:丢帧自愈、重复推送幂等。桥侧把 provider 分块按 **256 个字符**合并后再发一帧(hub 的 live broadcast 每会话只有 1024 槽,逐 token 发会把所有订阅者打成 `ReplayOverflow` reset)。一次 6KB 摘要 ≈ 24 帧。
2. 重连语义由 hub 的 current-value 槽 `conversation:compact:{compact_id}` 承担:压缩进行中重连 → 拿到最近一帧(含 `turns_total/turns_keeping` 与已流出的摘要),即 §4.4 想要的"刷新后仍显示进行中"**无需改快照**;终态帧发完即清除该槽,所以不会重放出常驻的"正在压缩"卡片(§6 验收的"卡片重复出现"从后端侧堵住)。

**验证口径**:`cargo test --workspace --lib` 全绿;新增单测 `ringing::v2::compact_deltas_publish_on_the_conversation_stream`(频道/交付/双层 tag/revision 单调)、`compact_slot_replays_while_running_and_clears_on_terminal`(重连与清理)、`ringing::compact_mirror::*`(合并阈值/累积/尾帧/状态映射)。桌面联调与 §7 的 `qaqh-client::upload_content` 仍未做。

**壳层注意**:`just ts-export` 后 `ConversationDelta.ts` 多三个 kind;Rust 宿主若对 `ConversationDelta` 做**穷尽 match**,新增变体是编译期破坏,需补分支(`qaqh-client` 已导出 `ClientV2CompactStatus`)。
