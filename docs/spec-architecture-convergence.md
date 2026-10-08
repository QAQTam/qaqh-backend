# 架构收敛施工单（architecture-convergence）

> **当前执行入口（2026-10-07 owner 更新）**：clean 分支只做 RC 前架构收敛、砍刀和 crate 减负，
> 任务顺序、删除要求与派工口径以 **§10 CLEAN-0–CLEAN-7** 为准。§1–§9 保留证据与旧任务追溯，
> 不再独立派发被 §10 接管的任务。职责图和现状/目标区分见 [ARCHITECTURE.md](ARCHITECTURE.md)。
> owner 明确要求大砍、事后回归；取消生产 shadow 双写、长期兼容层和正常读取 fallback。

> **状态**：施工单。规则见根目录 [`AGENTS-x.md`](../AGENTS-x.md)；本文件只列"要建什么、按什么顺序、怎么算完成"。
> **来源**：2026-10-06 一次架构评审会话（session `01a11215-7ee1-7615-aa6d-92f5575ddf5a`）给出的裁决。
> 该会话在动笔写施工单时被 429 中断，本文由后续会话从它的 messages.jsonl 转录，
> 并对**全部** `file:line` 于 2026-10-07 逐条复核（漂移处已按当前代码为准，见 §2 复核表）。
> **性质**：转录 + 复核，不含新裁决。凡本文与代码事实冲突，按 `AGENTS-x.md` P3 停下来记进 §9。
> **不变量编号**（I1–I21 / P1–P6）在 `AGENTS-x.md`，本文只引用不复制，避免两处漂移。

---

## 1. 目标与范围

一句话：**协议已经换过三次（ws+json-lp → Ringing v1 → Ringing v2），但该换的不是协议。**
Wire 层是整个系统里最容易改的部分，所以它被改了三遍；真正没定下来的是两件事——

1. **事实源到底是谁**（现在是两套并存、各写各的）；
2. **内部事件模型到底长什么样**（现在有三套词汇，其中一套发出去没人收）。

这两件事不解决，Ringing v3 只是时间问题。**本 spec 的范围 = 把这两件事定下来并做完**，
不是加功能、不是改 wire。

**明确不做**：改 wire 语义（Ringing v2 冻结，只允许加 `#[serde(default)]` 字段，违反要 ADR）；
全异步重写 agent loop（线程-每-会话模型保留）；拆/改 crate 名字（除 `ToolLedger` → `SessionLedger`
这一处命名订正，见 D6）。

---

## 2. 现状证据（全部于 2026-10-07 复核）

> 复核口径：以符号为准。下表"当前锚点"为 2026-10-07 复核值，与 2026-10-04 审计稿的差值列在
> 最后一列——**行号漂移本身就是这套文档的问题之一**，引用时优先看符号。

| # | 事实 | 当前锚点 | 与旧文档差 |
|---|---|---|---|
| E1 | canonical store 自认未接管 legacy 写 | `crates/qaqh-session/src/canonical/store.rs:27`（"not yet route legacy message/journal writers through this owner"） | 同 |
| E2 | legacy 侧写者 `LegacyWriterFacade`，**10 个活跃调用点** | `wal.rs:149/180/200/729`、`timeline_store.rs:98/175/278`、`session/manager.rs:241/808/1327` | 审计稿 timeline 记 105/182/285、manager 记 242/809/1346 → 已漂 |
| E3 | WAL 头 `qaqh-wal-v1`，文件名 `messages.wal` | `crates/qaqh-message/src/wal.rs:44,51` | 同 |
| E4 | `emit_domain` 汇点：只做副作用然后丢弃事件本体 | `crates/qaqh-runtime/src/actor.rs:59-79`（Ringing 臂） | 同 |
| E5 | 桥只匹配三类做副作用 | `stash_interaction_body` `registry.rs:2115`、`apply_interaction_side_effects` `registry.rs:2198`（匹配 `PlanReviewResolved`、`ToolEvent::ToolFinished` `:2219`） | 审计稿记 2202-2244 |
| E6 | **8 个 fact 变体零构造点**（只有消费/校验/投影臂） | `TurnStarted`/`ModelRoundStarted`/`AssistantBlockSealed`/`ToolCallDeclared`/`TurnInterrupted`/`SessionTitleChanged`/`SessionDeleted`/`WorkspaceResourceChanged` | **本文首列** |
| E7 | `TurnFinished` 唯一构造点在子代理恢复 | `crates/qaqh-runtime/src/subagent_recovery.rs:358` | **本文首列** |
| E8 | 回执靠 TTL 兜底而非 fact 折叠 | 注释 `crates/qaqh-runtime/src/ringing/pending_store.rs:310-314`；`RECEIPT_TTL:552`，使用点 `:130/:162/:220/:285/:421/:472/:491/:511` | 交接文档已记 |
| E9 | ContentStore 有 30 分钟 TTL + 256 上限，会真删盘 | `content_store.rs:37`（`DEFAULT_CONTENT_TTL`）、`:120/:137`（`max_entries: 256`）、`:559`（`sweep_expired`）、`:43`（pin 上限 64） | **本文首列** |
| E10 | 部分 `ContentRef` 只是哈希，正文从未落盘 | `engine_compact.rs:238`（`summary_ref`）、`tool_runtime.rs:518`（`effective_args_ref`） | **本文首列** |
| E11 | 字面量哈希当 ref 用 | `crates/qaqh-daemon/src/axum_server.rs:1156/1171/1494/1509/1996`（`sha256_content_hash(b"ask-request")` 一类） | **本文首列** |
| E12 | `ToolFinished.output_ref` 生产侧几乎全 `None`（10 处），只有 2 处会填 | `None`：`util/format.rs:168`、`engine_tool.rs:762`、`tool_recovery.rs:390/443`、`tool_runtime.rs:531/597/1160`、`qaqh-types/src/tool_result.rs:400`、`qaqh-session/src/actor.rs:297`、`tool_ledger.rs:1478`；会填：`host_impl.rs:1647`、`recovery_executor.rs:314` | **本文首列** |
| E13 | 零生产者变体的消费方仍在（删了会编译错但不会少功能） | `projection/conversation.rs:157`、`projection/timeline.rs:111`、`session_fact_v2/projection.rs:56` | **本文首列** |
| E14 | 磁盘格式被既有 spec 冻结 | `docs/spec/hub-fact-bus-refactor.md:247`（§6"不动 canonical log 磁盘格式"）、`:124` | 交接文档已引 |
| E15 | 会话级全局/TLS 状态 | `crates/qaqh-permission/src/lib.rs:42`(`CANCEL`)/`:43`(`CURRENT_SESSION`)/`:54`(`CURRENT_WORKSPACE`)/`:56`(TLS)/`:158`(`SESSION_CANCELS`)/`:190`(resolver 钩子) | AGENTS-x.md 记 :187 → 已漂 |
| E16 | 取消机制两套并存 | `crates/qaqh-runtime/src/agent/engine_turn.rs:1442`（`cancellation().is_set() \|\| qaqh_workspace::is_cancel()`） | 同 |
| E17 | 进程级字符串键全局表 | `process_registry.rs:195`、`fs-core/file_state.rs:25/42/55`、`file-tools/read_image/mod.rs:47`、`file-tools/pending.rs:29`、`workspace/audit/v2.rs:45` | **本文首列** |
| E18 | gate 同步面走**进程级 current-thread** runtime | `crates/qaqh-gate/src/transport.rs:29-37`（`FALLBACK_RT` = `new_current_thread`） | **本文首列** |
| E19 | lap 之间是递归不是循环 | `crates/qaqh-runtime/src/agent/loop_outcome.rs:384` `apply_outcome` → `:565` 再调 `turn.run(...)` | **本文首列** |
| E20 | Registry 一把大锁 | `crates/qaqh-runtime/src/service.rs:22`（`Arc<Mutex<AgentRegistry>>`） | 同 |
| E21 | 交互请求永不过期 | `InteractionExpiryReason` 生产侧只有一处写入且恒为 `TurnCancelled`：`crates/qaqh-session/src/actor.rs:628` | 同 |
| E22 | 文档与代码漂移 | `README.md:8`（"alpha / debug"）`:10`（`2.0.0-alpha2`）vs `Cargo.toml:43`（`2.0.0-beta.2`）；`architecture-report.md` 记 20 crate，实际 **29** | **本文首列** |
| E23 | 按 URL 子串判供应商 | `crates/qaqh-gate/src/types.rs:388`（`base_url.contains("opencode.ai/zen")`） | 同 |

### 2.1 核心发现：canonical log 现在**无法**重建 LLM 上下文

这是 E6+E7+E9+E10+E11+E12 合起来才成立的结论，也是 2026-10-06 那次评审推翻自己方案前提的那条：

- 定义的 `AssistantBlockSealed`（assistant 内容）**没有生产者** → log 里根本没有 assistant 正文；
- `TurnFinished` 只在子代理恢复路径构造 → **主会话的 turn 生命周期事实不存在**；
- 于是 `pending_store` 折叠 `TurnFinished` 的路径对普通会话永不触发，回执只能靠 `RECEIPT_TTL`（300s）过期（E8）——
  正是 `AGENTS-x.md` I9"每条客户端命令都必须有终态"要禁的那件事；
- 而 fact 里**已有的** `ContentRef` 又不可解析：指向 30 分钟就会被 `sweep_expired` 删盘的 ContentStore（E9），
  或者干脆是"只算了哈希、正文从未存过"的指纹（E10/E11），`ToolFinished.output_ref` 则几乎全是 `None`（E12）。

**推论**：把 `events.jsonl` 定为唯一事实源**不是改读路径能达成的**。前置条件是
（a）一个无 TTL、写后 fsync 的持久 CAS blob 存储，（b）给缺失变体补生产者。
`docs/spec/hub-fact-bus-refactor.md` §6 冻结磁盘格式的约束（E14）因此必须被修订（见 §4）。

---

## 3. 裁决（D1–D10）

> 以下为 2026-10-06 评审给出的定论，本文原样转录并复核其证据。术语与 `AGENTS-x.md` I7 对齐：
> **fact**（持久，经 `SessionLedger`）/ **live 帧**（易失，经 `Emitter`）。

- **D1 wire 冻结。** Ringing v2 只允许新增带 `#[serde(default)]` 的字段；不改语义、不删字段、不加路由。
  任何 wire 变更先写 `docs/adr/`。beta 期间不出现 v3。
- **D2 唯一事实源。** 每会话 **`events.jsonl` 是唯一的持久事实源**；`messages.jsonl`、`meta.json`、
  `index.jsonl`、timeline 快照、`messages.wal` 全部降级为**可删除重建的投影/缓存**。
  命令回执属于 daemon 层运行态，允许保留，但**终态必须能由 fact 折叠得出**（不得再靠 TTL 兜底）。
- **D3 持久 CAS blob 存储。** 新增每会话 `blobs/`（sha256 内容寻址、**无 TTL**、
  仅在会话删除或显式 GC 时回收）。**写入顺序**：blob 写 + fsync → 再追加引用它的 fact（`AGENTS-x.md` I2）。
  8 KiB 以内内联，超过走 blob。ContentStore 保留但**降级为 wire 传输缓存**，fact 不得直接指向它。
- **D4 上下文即投影。** LLM 上下文 = 从 fact 折叠出来的 `ContextView`（archive / active /
  covered_through / next_msg_id）。为此新增追加型 fact（`ContextRewound`、`MessageCommitted`、
  `ContextInjected`、`CompactionApplied` 补字段），**undo 不再是重写**（E7/`SaveFull` 那条路要删）。
  为保 provider 保真（thinking signature / 加密 reasoning / 工具调用顺序），
  assistant 侧以**整轮模型消息**为单位落 fact，block 级事实（`AssistantBlockSealed`）不再有生产者、删除。
- **D5 判据：单一 `apply(ContextOp)`。** `ContextOp`（AppendUser/AppendAssistant/AppendToolResult/
  Compact/Revert）由 `qaqh-message` 拥有；live 路径与重放路径**共用同一个函数**，
  用确定性测试保证"live store == replayed store"。
- **D6 事件词汇收敛到两种。** 会话 actor 只允许输出 **fact** 与 **live 帧**。
  删除 `DomainEvent`（含 `ToolEvent`/`ControlEvent`/`ConversationEvent`）与 `Emitter::emit_domain`；
  原挂在模式匹配上的副作用改为**显式调用**（`InteractionPort` 一类）。保留 `DomainCommand`（wire 仍用命令）。
  `ToolLedger` 改名 `SessionLedger`（它写的是全部 fact，不只是工具）。
- **D7 会话上下文显式传递。** 引入 `SessionScope`（`session_id` / `workspace_root` / `CancelToken` /
  沙箱标志 / blob 句柄），经 `ToolCallContext` 显式下发；拆除 `qaqh-permission` 的全局与 TLS，
  取消统一到**一棵 `CancelToken` 树**。会话键的进程级注册表逐项审计：能进 `SessionScope` 的进，
  必须留全局的在 `docs/ARCHITECTURE.md` 登记理由（每个 global 一行决策）。
- **D8 并发模型保留但修 runtime。** 保留线程-每-会话（同步落盘显式、易推理）；
  `FALLBACK_RT` 由 current-thread 换为 **daemon 持有的共享 multi-thread runtime**，
  gate/LSP/MCP 在构造期拿 `Handle`。理由：current-thread runtime 上多线程 `block_on` 会串行化 IO 驱动，
  会话间互相拖慢延迟——**这是怀疑项，实施前后都要压测取证**（`AGENTS-x.md` P5：跑不了要写明）。
  全异步重写不在本 spec 范围。
- **D9 交互必须有终态。** 权限类交互：daemon 重启后不可恢复的挂起 turn 一律
  以 `InteractionExpired(RestartPolicy)` 收口（现在 `/approvals` 重启后会留下永不可答的僵尸条目，是真 bug）；
  会话关闭/卸载走 `SessionClosed`。**默认不设墙钟超时**（等人本身就是正确产品行为），
  可选 `interaction_timeout_secs` 默认关闭。
- **D10 lap 改循环 + 锁纪律。** `apply_outcome` 的递归改成"跑到终态为止"的循环（十几行）。
  Registry 不拆，但立规：**持 registry 锁期间不做 IO、不做阻塞 send**，25 处调用点逐一审计。

---

## 4. 与既有文档的关系（谁接管谁，别两份清单打架）

本仓库的**既有病**就是"两份并行清单"（E1）。本文与现存文档的分工如下，**不允许再出现第二份任务清单**：

| 既有文档 | 处置 |
|---|---|
| `docs/archive/audit-legacy-protocol-2026-10-04.md` §7 的 **P0 / P1 / P2** | **保持原样**，仍由 `docs/plan-legacy-protocol-cleanup.md` + `docs/archive/legacy-protocol-post-merge-2026-10-06.md` 两条台账推进（PR #10 已合并 12 提交）。本 spec 不重列。 |
| 同上 **P3「独立工程」** | **并入本 spec**：`LegacyWriterFacade` 双栅栏收敛 → D2/T3.x；BETA-01 目录名 = canonical id → D2/T3.x（同一次迁移）；`to_tool_result()` 下游改吃 `ToolOutcome` → T4.x；`spec-file-mutation-delta` 开工或归档 → §9。 |
| `docs/spec/hub-fact-bus-refactor.md` §6"不动 canonical log 磁盘格式" | **被本 spec 修订**：D3/D4 要求动 fact 生产面与 schema（带一次性迁移与版本标记）。该 spec 的其余约束（wire 冻结）继续有效。 |
| `docs/archive/legacy-protocol-post-merge-2026-10-06.md` §四"三项回执靠 TTL 兜底" | 结论仍对（因为 fact 侧无对应物），但**归因被本文扩大**：不是"缺三个变体"，而是 turn 生命周期事实整体缺生产者（E7）。根治路径 = D2/D4，**不得**在补 fact 之前先删那三个事件（那是行为回退）。 |
| `docs/plan-context-ownership-v2.md` | 被 **D4 取代**：`ContextService` 变成 fact 上的投影，而非旧 message/session backend 上的服务。 |

---

## 5. 阶段与任务

任务 ID = `T<阶段>.<序号>`，一个 PR 对应一个任务 ID（`AGENTS-x.md` P2）。
**依赖**：T3 必须等 T1 完成（blob 存储先于 cutover）；T2 与 T3 互不依赖，**可并行**（分 worktree，各自 rebase）。

### P0 卫生（低风险，先做；纯机械）
- [ ] **T0.1** `apply_outcome` 递归改循环（E19）。验收：新增测试——小栈线程上跑大量 lap 不溢出。
- [ ] **T0.2** 文档与现实对齐（E22）：修 `README.md` 状态/版本；`architecture-report.md` 改为归档或重写基线；
      建 `docs/ARCHITECTURE.md` 骨架（含存储表：每个持久文件登记"投影/缓存/诊断/运行态"+ 重建来源；见 `AGENTS-x.md` I1/I10/I13/P6）。
- [ ] **T0.3** 重启僵尸交互取证：daemon 重启后 `/approvals` 是否仍展示不可答条目（D9 的现场证据）。
- [ ] **T0.4** 量尺基线（§8）：测出各项 ratchet 计数的当前值并落配置。

### P1 唯一事实源（最大一笔，最后收益最大）
- [ ] **T1.1** 持久 CAS blob 存储（D3）：写入用 tmp + fsync + rename；`SessionLedger::append` 校验
      "payload 引用的 blob 必须已存在"（`AGENTS-x.md` I5）。
- [ ] **T1.2** 补 fact 生产者：turn 生命周期（`TurnStarted`/`TurnFinished`/`TurnInterrupted`）、
      整轮模型消息（取代 `AssistantBlockSealed`）、`ContextInjected`、`ContextRewound`、`CompactionApplied` 补字段。
- [ ] **T1.3（由 CLEAN-3 接管）** 离线导入/重放对比后一次切换：不在生产 live 路径双写。
      新上下文与离线基线要求 **零 diff**（金标语料：工具/并行工具/图片/压缩/undo/steer/子代理注入/取消中工具/溢出恢复/流续写）。
- [ ] **T1.4** 收口内容读取面：wire 的内容端点先读 ContentStore 缓存，未命中回落 blob（wire 不变）。

### P2 事件词汇收敛（与 T3 独立，可并行）
- [ ] **T2.1** 删 `DomainEvent` 的 fire-into-void 路径（E4/E5）：副作用改显式调用。
      **先剔除不能删的**：`RoundDelta`/`RoundCompleted` 有集成测试钉住（`docs/archive/legacy-protocol-post-merge-2026-10-06.md` §三.1）。
- [ ] **T2.2** `Emitter` 收敛为 `emit_live(LiveFrame)`；`TimelineIntent` 名保留（wire 兼容）。
- [ ] **T2.3** `ToolLedger` → `SessionLedger` 改名；`PersistOp` → `ContextOp`（D5）。

### P3 cutover + 存储布局迁移
- [ ] **T3.1** 读路径切到 fact 折叠（D4/D5）；`build_context` 改读 `ContextView`。
- [ ] **T3.2** 删 legacy 写路径：`messages.jsonl` 直写、`messages.wal`、`LegacyWriterFacade`（E2/E3）。
- [ ] **T3.3** 一次性迁移（daemon 启动、幂等、写 `data_version`，`AGENTS-x.md` I6）：
      旧 session 的 `messages.jsonl` 以 `LegacyTranscriptImported` 引用入 blob；**同一次**把
      `seed` 目录名迁到 canonical id（BETA-01）与 `ringing-driver-watch.json` 的 seed 键，迁移前备份。

### P4 显式上下文 + 收尾
- [ ] **T4.1** `SessionScope` + 拆全局/TLS（D7），global 逐项审计表落 `docs/ARCHITECTURE.md`。
- [ ] **T4.2** `CancelToken` 单树（E15/E16）。
- [ ] **T4.3** runtime 换 multi-thread + 压测取证（D8）。
- [ ] **T4.4** 交互终态策略 + 重启过期（D9）。
- [ ] **T4.5** 清 migrate-on-read（旧格式识别只留在 migrate 模块）+ ts-rs 导出面按白名单继续收窄。

### P5 按会话选 profile（用户请求，2026-10-07）
现状：`active_profile` 是 config.toml 的**单个全局值**，`profile.apply` 经
`notify_config_changed()` → `broadcast_ringing(AgentReloadConfig)` 广播给**所有**会话
（`service.rs:774`）；`ProfileConfig` 不含 key，密钥全局单份。→ 无法"这个对话用 A、那个用 B"。

接缝已定：`EngineSession::apply_config`（`engine_session.rs:95`）是唯一把配置灌进运行中 agent 的口，
`AgentState.config` 是每会话快照（`agent.rs:161`），gate 从它读 `base_url`/`model`（`agent.rs:368/370`）
——**不需要动 gate**。合成逻辑已存在：`Config::apply_profile`（`config.rs:1401`）。

- [x] **T5.1** per-profile api_key：`ProfileConfig` 加 `api_key` **标记**（只存 `"set"`，永不落明文）；
      `SecretStore` 加 `[secrets.profiles]` 命名 map（照 `[secrets.mcp]` 的形状与字符集校验）+
      `load/set/delete/has_profile_key`；`save_profile` 记标记、`apply_profile` 解析 key
      （自带则用，**无则继承 main**——保住"同 key 换端点/模型"的既有用法）、`delete_profile` 连 secret 删。
- [x] **T5.2** 会话级选择：`SessionMeta` 加 `profile`（**走 meta.json，照 `mode`/`tool_mode` 先例**，
      不经 canonical fact——fact 侧要动被 §6 冻结的 schema，属 D3/D4 的范围，不在本任务）；
      `SessionManager::session_profile` 回读（照 `workspace_cwd:569`）+ `persist_profile`（照 `persist_mode`）；
      `Config::for_session(global, profile, own_key)` 合成（`profile=None` 跟随全局 `active_profile`）；
      `reload_config` 用 `agent.session_manager` 回读后合成；新增 service 方法
      `session.set_profile { session_id, name }`（走既有 catch-all
      `POST /ringing/v2/service/{method}`，**不新增 wire 类型**），落 meta 后
      `registry.send_ringing(session_id, AgentReloadConfig)` **只重载该会话**（`registry.rs:1406`，不是 broadcast）。

#### T5 落地记录（2026-10-07）

实现点：`qaqh-types/src/config.rs`（`ProfileConfig.api_key`）、`qaqh-types/src/session.rs`（`SessionMeta.profile`
+ wire 键锁同步）、`qaqh-config/src/secrets.rs`（`[secrets.profiles]` + 5 个方法 + 名校验）、
`qaqh-config/src/config.rs`（`apply_profile`/`save_profile(name, api_key_saved)`/`profile_carries_key`/`for_session`；
`save_with` 保留 profile 密钥标记）、`qaqh-session/src/manager.rs`（`session_profile`/`persist_profile`）、
`qaqh-runtime/src/agent/engine_session.rs`（`resolve_profile_key` + `session_effective_config` 共用合成函数）、
`qaqh-runtime/src/agent/spawn.rs`（首启/恢复同走该函数）、
`qaqh-runtime/src/service.rs`（`profile.save_current`/`profile.delete` 接 secret、新增 `session.set_profile`）、
`qaqh-runtime/src/ringing/service_methods.rs`（`session.set_profile` = `WRITE_SEEDED`）、
`qaqh-client/src/endpoint.rs`（`SessionSetProfile` 变体）。

**实现期补掉的一个真缺口**：生产装配原来在 `agent/spawn.rs:84` 直接
`AgentState::new(watch::authoritative())` —— **不过会话 profile**。于是"重启后恢复的会话"会先跑在
全局 profile 上，直到下一次 `AgentReloadConfig` 才纠正。已抽出 `session_effective_config(manager, session_id)`
让 **spawn 与 reload 共用同一份合成**，避免两条路各算一份。

验证：
- `cargo check --workspace --all-targets` → 0 error。
- **`cargo test --workspace --no-fail-fast` → exit 0：172 个测试目标 / 1842 passed / 0 failed / 6 ignored**
  （连文档里记的"已知基线红" `agent::prompt::tests::prompt_and_tool_defs_char_budget` 这次也是 ok）。
- 单箱：`qaqh-types` 34、`qaqh-config` 61、`qaqh-session` 34、`qaqh-client` 55 全绿；含 3 个新增测试
  （`apply_profile_leaves_main_key_untouched`、`profile_key_marker_survives_save_and_secret_round_trips`、
  `for_session_overrides_endpoint_and_resolves_key`）。
- 端到端 A（隔离数据根真 daemon + 真 HTTP，热重载分支）：`config.save` 设主密钥 → `profile.save_current alt` →
  切回 `default` 活跃并改全局 model → 两个会话只对 S1 调 `session.set_profile alt`。结果：
  `config.toml` 的 `[profiles.alt]` 有 `api_key = "set"`、明文只在 `secrets.toml` 的 `[secrets.profiles]`；
  `sessions/<S1>/meta.json` `profile = "alt"`，**S2 无 profile**；`session.set_profile` 前后
  `config.toml` **逐字节不变**（证明是会话级、无广播）。
- 端到端 B（spawn 分支）：设好 S 的 profile 后**重启 daemon**，再 attach 恢复该会话。daemon 日志：
  `[ACTOR] session=<S> profile=Some("alt") model=model-alt`（全局则是 `model-global`）——
  恢复路径确实取到会话 profile。
- 闸门事件：`qaqh-client` 的 `routes_are_pairwise_distinct_and_well_formed` 把**服务面路由总数**
  写死（34）。新增 `session.set_profile` 后该锁按设计报红（"确认是新增而非改错"），已在同一改动内
  同步为 35 并注明来源。新增的 `session.set_profile` 传空 `name` = **清除**该会话的选择、
  回到跟随全局 `active_profile`。

- **生效时机（契约，2026-10-07 定）**：`session.set_profile` 的重载在 **turn 边界**生效。
  命令排在 actor 命令通道里，等当前 turn 跑完、主循环回到 `drain_pending()`
  （`loop_core.rs:439`）才被处理（`loop_dispatch_control.rs:144` → `reload_config`）。
  turn 内部**没有**第二条命令处理路径（`turn_lap/*` 与 `engine_turn.rs` 对 `cmd_rx` 零引用）；
  空闲期则几乎即时（`recv_timeout` 一到即返回）。由此产生的三个窗口，前端必须按契约处理：
  1. `session.set_profile` 的 ack 是**乐观的**——`send_ringing_cmd` 只是非阻塞投递
     （`service.rs:142-151`），返回 ok 时重载尚未发生；
  2. `meta.json` 写得比生效早 → `session.meta` / `session.list` 读到新 profile 时，
     活着的 agent 可能仍在旧端点/旧 key 上（窗口长度 = 当前 turn 剩余时长）；
  3. 本任务**不写 fact、不发 `ProjectionEvent`** → 前端只能重读 `session.meta`，
     无法从事件流得知它何时真正生效。
- **明确不做（owner 决定，2026-10-07）**：**不做 turn 中途生效**（不在 lap 边界抽命令）。
  理由：中途换会带来次生问题——本轮已发出的请求不受影响、工具轮次之间的端点语义要重新定义、
  因果链会跨端点；收益不值。要加这个能力，先回来改这一条。
- 范围：**只覆盖端点/模型**（model/base_url/wire/max_tokens/effort/context_length/compat），不含
  permission_level / exec / subagent（后者要扩 `apply_config` 字段清单与 `applies_all_hot_fields` 守卫）。
- wire 合规（I14）：新服务方法名是加法，不新增路由、不改字段 → 不需要 ADR；若要把"当前 profile"
  显示到前端，`ControlState` 只能加 `#[serde(default)]` 字段。

---

## 6. 完成定义（DoD）与探针

每个任务的 PR 必须全绿（`AGENTS-x.md` P5）：

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace -- --test-threads=1
# 触到 wire/derive(TS) 时必跑（生成物与漂移闸已随前端拆到 qaqh-desktop-app）：
#   cd ../qaqh-desktop-app && just ts-check
bash scripts/v2-legacy-compat-probe.sh                 # 触到 migrate-on-read 时必跑
```

外加本 spec 各任务验收条款里的探针/测试。**跑不了的必须写"未验证：<原因>"，禁止声称通过。**
已知基线红：`agent::prompt::tests::prompt_and_tool_defs_char_budget`（设计如此）。

---

## 7. 禁止动作（对执行模型）

- 不许为了掩盖行为回退改测试断言；不许加 `#[ignore]`；不许留"临时兼容桥"或生产 shadow 双写。
  只验证旧结构/源码字符串的测试随 CLEAN 任务删除或改为真实行为验证，不能锁死待删除实现。
- 不许在 wire 类型上动手（`AGENTS-x.md` I14）。
- 不许用"运行时注册 `fn` 钩子"绕依赖方向（I13）。
- 不许对**别人**的未提交改动动刀；工作区里并行的 webui 线不属于本 spec。
- 不许把 `to_tool_result()` 折回层当"旧协议残留"删——它是当前所有工具的共同收敛面（审计稿 §1.3）。

---

## 8. 量尺（ratchet 基线，T0.4 测）

CI 只做"计数不许增加"，不要求立刻清零（改法见 `AGENTS-x.md` I16/I17/I18/I19）：

| 项 | 模式（在仓库根执行） | 基线值（35fa46e 复测） |
|---|---|---|
| 会话级 static / thread_local | `grep -rnE "pub static .*AtomicBool\|thread_local!" crates --include=*.rs \| wc -l` | **8** |
| 变更日志式注释 | `grep -rnE "BUG-20[0-9]{2}\|阶段 [0-9]\|PR-[0-9]+-[0-9]+\|§[0-9]+\.[0-9]+\.[0-9]+" crates --include=*.rs \| wc -l` | **329** |
| 超参函数 | `grep -rc "#\[allow(clippy::too_many_arguments)\]" crates --include=*.rs` 求和 | **48**（旧记 50 是按 `too_many_arguments` 字样数行，含注释与 expect 变体） |
| 身份残留 | `grep -rnE "\bseeds?\b" crates --include=*.rs \| wc -l` | **765**；排除 `/tests/` 为 **657**。原表标注"排除 tests"与所记 765 不一致，已按两种口径同时登记 |
| 裸字符串错误 | `grep -rnE "Result<[^>]*, *String>" crates --include=*.rs \| wc -l` | 信息值 **310**；仍由 review 守新增 pub API |

---

## 9. 发现记录

> `AGENTS-x.md` P2/P3 指定的落点：发现的问题写这里，不要就地改。

1. **【需裁决】阶段顺序有两版，属转录内的不确定。** 2026-10-06 会话先在推理里给出
   "P0 卫生 → P1 事件词汇 → P2 SessionScope → P3 唯一事实源 → P4 删 legacy"，
   后在动笔时改成"P0 卫生 → P1 blob 存储 + 补生产者 → P2 shadow → P3 cutover"。
   本文按**依赖关系**合并（见 §5）：事实源链条（T1→T3）与事件词汇/上下文（T2、T4）互不依赖。
   若执行方认为必须单选一版顺序，请在此签名裁决。
2. **~~`docs/ARCHITECTURE.md` 不存在但被 `AGENTS.md` 引用~~ 已闭合（2026-10-07，提交 `ee40274`）**：
   `docs/ARCHITECTURE.md` 已入库（29 crate 职责表 + 存储权威表 + 依赖目标），I1/I10 恢复可验收。
   **新增事实**：宪法文件已改名入库为根目录 `AGENTS-x.md`（内容与 `git show HEAD:AGENTS.md` 逐字节一致）。
   本文与 ARCHITECTURE 里所有"见 `AGENTS.md` I-x"引用在本机解析到 `AGENTS-x.md`；派工时要点名这一条，
   否则执行模型会以为宪法丢失。
3. **`architecture-report.md` 会误导人**：它写 20 crate / 有 `qaqh-webui-gateway`，
   实际 29 crate、gateway 已随 Tauri 化删除，gate 已换 `*_sdk.rs`。它自称"只读探索、不修改代码"，
   但作为事实报告已过期 → T0.2 里决定归档还是重写。
4. **`docs/spec-file-mutation-delta-v2.md` 零实现**（审计稿 §4.6、`plan-legacy-protocol-cleanup.md` §4）：开工或归档，别长期空转。
5. **`ToolCapabilities.streaming` 无生产读取点**（审计稿 §4.5）：纯声明字段，改名/删除零风险，待表态。
6. **`docs/current/*.md` 悬空引用 41 处**：整仓文档清理事项，已有豁免记录
   `docs/archive/handoff-permission-three-tiers.md:107`，非单文件注释问题（`draft-comment-conflicts-2026-10-05.md` §6）。
7. **未实测项（评审自己标注的）**：E18 的"多会话共用 current-thread runtime 互相拖慢"是**怀疑**，
   不是结论；E19 的栈溢出取决于线程栈大小（`tool_runtime.rs:654/677/939` 是 4 MiB，
   其余线程用默认值），都必须在 T4.3/T0.1 里取证。
8. **`draft-comment-conflicts-2026-10-05.md` 的 189 条注释改动当时未提交**——动手前先确认是否已落库，
   否则会与 T0.x 的注释类改动互相覆盖。
9. **【探针可靠性】`scripts/v2-legacy-compat-probe.sh` 的 02 项有假阳性模式。**
   2026-10-07 实测该脚本对 `[COMPACT` 报 `hits=1`，实为该串出现在某条消息的**正文里**
   （工具结果中粘贴的源码讨论文本），并非消息开头的压缩水位标记——脚本对整文件 `grep -rl`，
   不看位置。判"零命中"前必须改成"只看消息正文起始处"。同次运行 11 项（明文 api_key）
   `hits=1`，数分钟后同表达式复跑变 0，期间 `config.toml` 被活进程重写（mtime 变动）——
   **该项在本机不可稳定判定**，删除前置条件不成立。这条同时说明 P2 的"探针当闸门"结论
   （`docs/archive/legacy-protocol-post-merge-2026-10-06.md` §四"本机全部零命中"）
   **在 2026-10-07 已不成立**，续做 P2 前要重跑并逐项判定真假。

### T5 实现期发现（2026-10-07）

10. `ProfileConfig` 的文档注释早就宣称 *"only a `"set"` marker is written here"*，但 `api_key`
    字段**从未存在**（注释领先代码）。T5.1 补字段时一并改写了该注释。
11. **【危险点，已加回归测试】** `Config::save_with` 会把 `self.api_key` 写进 `SecretSlot::Main`。
    因此 `apply_profile` **绝不能**改 `api_key`——否则"切 profile"会在下一次存盘时把该 profile
    的密钥复制成主密钥。守卫：`apply_profile_leaves_main_key_untouched`；profile 的有效密钥改由
    `for_session` 做**只读合成**（永不落盘）。
12. **【既有语义，勿踩】** `save_profile` 会把新 profile 设为全局 `active_profile`，且 `save_with`
    每次存盘都用当前扁平值覆盖 `[profiles.<active>]`。于是"在 profile X 活跃时改全局配置"会写进 X
    （扁平值即活跃 profile，是既有设计）。`session.set_profile` 不触发存盘，无此问题；但客户端若先
    `profile.apply X` 再 `config.save`，改的就是 X。
13. **【工具链漂移，非本次引入】** `cargo fmt --all -- --check` 在 **57 个未改动文件**上失败
    （rustfmt 版本差异）；clippy（1.99）报 54 条既有 warning，例：`qaqh-types/src/config.rs:386`
    的 `needless_borrows_for_generic_args`。本次改动的 10 个文件 fmt 干净、未新增 clippy warning。
    → 这两条闸门在本机需要先降噪，否则 `AGENTS-x.md` P5 的 DoD 无法真正执行。
14. **【未做】** `ConfigDto.profiles` 仍是纯名字列表，未暴露"哪些 profile 自带 key"；设置页要显示
    该标记需要新增字段。但 `ConfigDto` 现由 `dto_rejects_a_partial_payload` 锁定"缺字段即失败"，
    加必填字段会破坏旧载荷解析，需先定策略（加 `#[serde(default)]` 还是另立只读方法）。
15. **【并发修改】** 2026-10-07 11:37–11:40（本 spec 编写期间）有**另一条线**在执行 T0.2 的文档归档：
    `docs/audit-legacy-protocol-2026-10-04.md`、`docs/handoff/*`（含
    `legacy-protocol-post-merge-2026-10-06.md`）、`docs/handoff-permission-three-tiers.md`、
    `docs/spec/windows-sandbox-*.md`、`docs/plan-beta-readiness.md` 等被 `git mv` 进 `docs/archive/`
    （已暂存），`docs/handoff/` 现已空。本文所有引用已同步改为归档路径。**未动**这条线的任何文件。
    仍然留在原地的：`docs/plan-legacy-protocol-cleanup.md`、`docs/spec/hub-fact-bus-refactor.md`、
    `docs/draft-comment-conflicts-2026-10-05.md`、`docs/audit-security-2026-10-01.md`。

### T6 webui 拆仓（owner 决定，2026-10-07）

16. **`webui/` 已拆成独立仓 `qaqh-desktop-app`。** 决定：**留 Tauri**（换 Electron 会让 `src-tauri/`
    + capabilities + sidecar + NSIS 全套作废，而前端只用纯 JS 库、无 Node 主进程诉求）、
    仓名 `qaqh-desktop-app`（框架中性，与已有的 `qaqh-tui-app` 对称）、**先把在途改动提交再拆**。
    - 迁法：`git subtree split -P webui` → 24 个提交的完整历史导入 `E:/qaqh-desktop-app`，
      后端历史不重写。（本地保留 `webui-export` 分支作安全网，可随时删。）
    - 新仓修配：独立 `Cargo.toml` workspace（提供 `[workspace.package]` / `[workspace.lints.clippy]`
      供 `src-tauri` 的 `version.workspace` / `[lints] workspace = true` 继承）；
      `qaqh-client` / `qaqh-types` 的 path 依赖改 `../../qaqh-backend/crates/*`（注意 src-tauri 上移后多一层 `..`）；
      搬入并改造 `place-sidecar.ps1`（daemon 的权威构建仍在后端仓，本脚本只搬运）；
      新建 justfile（renderer-build / ts-export / ts-check / place-sidecar / desktop-dev / desktop-build）；
      README 补「与后端仓的关系」。
    - 后端解耦：members 去掉 `webui/src-tauri`、`webui/` 与 `scripts/place-sidecar.ps1` 移除、
      justfile 的 webui/desktop/ts 段改为指向、README 改写、`.gitignore` 清理。
      **crate 代码零改动**——`rg webui` 在 `crates/` 零命中，`Cargo.lock` 仅收窄（tauri 树退出，−3014/+116）。
    - 验证：新仓 `cargo check --workspace --all-targets` = 0 error（**需先 `just place-sidecar`**，
      否则 tauri-build 会因 externalBin 资源缺失而失败——Tauri 固有行为）；
      `bun install --frozen-lockfile` + `typecheck` + `test`（117 pass / 0 fail）+ `build` 全 0；
      后端移除后 `cargo check --workspace --all-targets` = 0 error。
    - **跨仓类型链实测通过**：在新仓跑 `just ts-export`（cd 到 `../qaqh-backend` 跑 cargo test，
      `TS_RS_EXPORT_DIR` 指回本仓 `src/api/`）→ exit 0；重新生成的 129 个 `.ts` 与已入库版本
      **逐字节一致（零漂移）**。注：首次 `git status` 会把 130 个文件报成 `M`——那是 EOL 噪声
      （导出时没有 `.gitattributes`），`git add` 后即归零；`* text=auto` 现已入库，不再复发。
    - 遗留（未做，需 owner 定）：① `scripts/smoke-g1.ps1` 引用的 `qaqh-webui-gateway` 是早已删除的 crate
      （既有腐化，非本次引入）；② 后端根 `package.json` 描述仍写 "desktop layer lives in qaqh-winui-app"（过时）；
      ③ `docs/` 下仍有多份文档按 `webui/...` 路径描述前端，本次只改了 README 与 justfile。
17. **【T6 收尾，2026-10-07】** owner 定了三件事，均已执行：
    - **后端 388M 残留已删**（`webui/` 整目录，全是未跟踪的构建产物 + `.zcode/`；真源码已在
      `qaqh-desktop-app` 与后端 git 历史里，删除无损失）。
    - **新仓改用 pnpm**（`bun.lock` 删除，只认 `pnpm-lock.yaml`）。连带把测试运行器从
      `bun test` 换成 **vitest**——`bun:test` 只在 9 个测试文件里各 import 一次
      `describe/expect/test`、无 bun 专有 API、且测试不碰 DOM，所以迁移是 9 行 import + 一个
      `test: { environment: "node" }`（vitest 5 默认环境会去找 jsdom 而拒绝启动 worker）。
      `devDependencies` 去掉 `@types/bun`（src 不用 bun 运行时，tsconfig 只取 `vite/client`）。
      `tauri.conf.json` 的 `beforeDevCommand`/`beforeBuildCommand` 也必须一起换——否则 Tauri 仍去调 bun。
      验证：`pnpm install --frozen-lockfile` / `typecheck` / `test`（117 pass）/ `build` 全 0。
    - **`AGENTS-x.md` 与本文已入库**（提交 `3a66f97`），收敛了 AGENTS-x.md 那条悬空引用。
    - **发现（未处置）：根 `prompt.md` 是 `crates/qaqh-runtime/src/agent/prompts/backend_prompt.md`
      的过期副本**——探针：`diff <(tr -d '\r' < prompt.md) <(tr -d '\r' < backend_prompt.md)` 报 4 处漂移
      （bash 参数说明、子代理白名单 `skills` vs `the skill_* tools`、`skills{action:"list"}` vs
      `skill_list`/`skill_activate`/`skill_resource`）。它无任何代码引用、从未入库。
      → **建议删除**（留着就是第二份会漂的系统提示词，正是 `AGENTS-x.md` I16 禁的"并存"）；
      本次未提交、也未删，等 owner 表态。
    - 新仓远端：`https://github.com/QAQTam/qaqh-desktop-app`（public，MIT，署名年份 2027）。
      推送前已扫全历史：严格凭据模式、敏感文件名均零命中。
18. **【2026-10-08 施工记录】审计优先级 1/2/3 的实施边界。** 按桌面仓
    `docs/backend-tool-upgrade-audit.md` 落地：① Todo canonical 资源 fact（工具路径经
    `tool_runtime::backfill_executed_result` / UI 直调 / goal 转换统一走
    `agent/resource_publish.rs`；service 路径经新 `ControlCommand::PublishResourceChanged`，
    ADR：`docs/adr/2026-10-08-publish-resource-changed.md`）；② `read_image::store_image`
    改返回 `Result`（I20）；③ `tool_search` 收敛 typed Args/schema/能力单源；
    ④ CLEAN-3 的 `effective_args_ref`/`output_ref` 接 `SessionBlobStore`（ARCHITECTURE
    存储表已更新）。**仍未覆盖**：会话未加载时 service 直写无 fact（无 worker 可投递）；
    wire 内容端点未接 blob 回落（T1.4）；`RoundDelta` 等其余 fire-into-void 的
    DashboardUpdated（`turn_lap/backfill.rs:98`、`engine_misc.rs:82`）留给 CLEAN-2；
    Deferred/MCP 聚合仍未在生产接线（P3 接入，原审计优先级 5）。

## 10. clean：RC 前砍刀与 crate 减负

### 10.1 owner 范围与实施规则

2026-10-07：owner 要求先更新主线到 2.0.0-beta.3，再建 clean 本地/远端分支与 PR。
规划模型负责审计、拆任务与验收设计；owner 自行安排执行模型。本轮不启动执行代理。
clean PR 是整项动作的 draft 集成 PR；下面的任务 ID 是交付/提交单元。
本轮按 owner 授权采用一项集成 PR，不为满足旧 P2 机械拆成多个并行清单。

* 一个任务改完完整链路并删除旧路径，然后做回归；失败修新实现或整体回滚该任务。
* 不留 deprecated/shim/no-op feature、生产双写、旧存储回落、迁移开关和备用执行入口。
* 旧数据转换只在一次性 migrate 入口，正常读取只认当前格式。禁止损坏/缺失时猜出成功终态。
* 开工前保留 Git/数据快照及测试样本，不先给旧壳大量补测试；崩溃接缝需要专项验证。
* 每个迁移任务包括调用者、exports、Cargo 依赖、真实行为测试和 ARCHITECTURE 更新。
* 被其他仓使用的 Rust API 同步改实际调用者；不为潜在消费者维持兼容层。
* 不加新产品功能、v3、远端执行协议、不全异步重写、不凭行数制造新 crate。
* 不动用户已有未提交项：prompt.md、.zcode/。它们不属于本轮提交。
  （`AGENTS.md` 的本地删除与 `AGENTS-x.md` 改名已于 `9d0a26c` 入库，不再属未提交项。）

### 10.2 依赖和派工顺序

```text
CLEAN-0 基线（本轮）
  -> CLEAN-1 空转删除
  -> CLEAN-2 一次收敛内部事件
  -> CLEAN-3 事实/正文/上下文完整 cutover
  -> CLEAN-4 显式状态与执行生命周期
  -> CLEAN-5 crate 职责迁移与依赖减负
  -> CLEAN-6 文件/API/文档收尾
  -> CLEAN-7 完整回归与 RC 判定
```

CLEAN-2 只能删除已在该任务中由事实或显式动作完整承接的事件；其余生产者随 CLEAN-3
补齐后删除。这是一项有界依赖，不允许提前加一个 DomainEvent 内部桥再做第二次迁移。
CLEAN-5 的 session-api 抽取可提前完成准备，但 contract 的最终删除/导出与 CLEAN-3 对齐；
不在事实 schema 正在变化时让两个模型同时改同一套类型。

派工以当前顺序串行合入 clean。确需并行，仅允许不共享写集的子任务（如 provider
内部模块整理与文件工具内部模块整理）；registry、service、事实 schema、workspace 执行入口
各自同一时间只有一个修改者。每次派工给出任务 ID、起始 SHA、允许修改的路径、依赖任务
的交付 SHA。其他发现记回本文，不顺手扩范围。

### 10.3 可直接派发的任务

#### CLEAN-0 — 基线与规划（本轮完成）

交付：版本 2.0.0-beta.3；main 同步；clean 分支与 draft PR；ARCHITECTURE 29 crate 职责表；
metrics/clean-baseline.json 的物理行数、最大文件、普通直接依赖与默认 features；本节派工单。
版本更新包含 26 个 qaqh-* workspace 包，sbx-* 三个独立 0.1.0 不改。
源码职责结论是已核对模块的分析，不宣称全仓行为审计完成。

#### CLEAN-1 — 一批砍掉无独立语义的外壳

写集：workspace/tool_api/boundary.rs、workspace/execution.rs 与旧入口调用者；runtime/registry.rs；
旧订阅 service 链、启动调用者及对应测试/exports/Cargo features。
起始 SHA = `bb2973b`（其 `crates/` 内容与 `35fa46e` 逐字节相同，量尺快照按 `35fa46e` 测，两者可互换引用）。

2026-10-07 复核的删除清单与证据：

* `ExecuteBatch/BatchOutcome/ResumeInteraction`（`workspace/src/tool_api/boundary.rs:24/51/90`）：
  全仓无构造点（`ExecuteBatch {` 只命中结构体定义本身），仅 `tool_api/mod.rs:11-12` re-export。
  `tool-core/src/tool_api/mod.rs:21` 的文档把 boundary 归给 tool-core，实际文件在 workspace，随本次删除一并清。
* 旧订阅桥：`registry.rs:264 subscription_actor`（自述 "P2-2d-b migration bridge"）+
  `apply_subscription:1995` + `subscribe_channel:1507`/`unsubscribe_channel:1528`/`connection_closed:1545`
  + `session/src/actor.rs:335-460` 的 `SubscriptionCommand/SubscriptionEffect`。
  生产调用者为零，只剩 registry 自测（2503/2636）。
* `AgentTransport`（`registry.rs:242`）只有 `InProcess` 单变体，9 处 match 全为单臂解构 → 展平成直接字段。
* `spawn_with`（`registry.rs:1120`）纯转发；两个调用点（782/1117）都传 `&[]`，
  下游 `spawn_session_inprocess` 收 `_extra_args` 后丢弃 → 删参数与转发层。
* runtime `memory` feature：`default = ["memory"]` 且 `memory = []` 为 no-op，
  唯一消费者是 `daemon/Cargo.toml:43` 的 `memory = ["qaqh-runtime/memory"]`，两处同删。
* 旧工具入口：`execution.rs:27 execute_authorized` 自述为 "Legacy adapter … pre-P2-4d entry point"；
  runtime 侧 `tool_runtime.rs:943` 已在用 `execute_authorized_with_context`，其余调用点均在 execution.rs 自测内。

**`subscribe` 同名保护名单（按字样清扫会误删活路径）**：真正的 V2 SSE 是
`ringing/v2.rs:318 subscribe`（调用者 `daemon/axum_server/axum_impl/v2.rs:576`、`host_impl.rs:535`）；
`host_impl.rs` 的 board subscription 是团队看板产品功能；`daemon/server.rs` 的
`shutdown.subscribe()` 与 `config/watch.rs:33 subscribe()` 是 tokio/watch 原语。这四者不属于删除范围。

删除 ExecuteBatch/BatchOutcome/ResumeInteraction 未接线协议；subscription_actor 与旧订阅 API；
单分支 AgentTransport；spawn_with 无效参数与转发层；runtime memory no-op feature。
把旧工具入口调用者一次迁到 ToolCallContext 后删旧入口，保留解析/准入/审计语义。
manager/fold policy 的剩余 TLS 不在这里裸删，留给 CLEAN-4 完整迁移。

验收：上述符号/feature 无生产残留；仅构造旧壳、扫描函数名的测试删除；实际 V2 SSE、授权拒绝、
会话/workspace 绑定、取消、线程退出/join、尾部排空回归。删除项逐条附搜索结果。
接管：旧工具入口 → `ToolCallContext` 的调用者迁移（= 部分 T4.1）。
注：本条原文写"旧 Next 3 Actions 的第一项"，该说法在现库与 `docs/archive/` 中查无出处，已删（见 §10.6）。

#### CLEAN-2 — 事件直接收敛，副作用显式化

写集：runtime/actor.rs、agent/paced_emitter.rs、types.rs、engine_title.rs、engine_compact.rs；
registry 交互副作用、activity；domain/event.rs、ringing/event.rs/worker.rs 及生产调用者。

先列出每种旧事件的构造点、消费者和真实动作。actor 输出职责收敛为持久 fact/易失 live，
交互内容/pin/activity 改显式动作或由提交事实触发。跨线程需要排队仍用有类型的内部队列，
不再把 wire envelope 当内部语义。对外序列化只发生在传输边界。
无独立行为的旧事件与桥接删除；缺事实的生命周期事件必须在删除前承接，必要时与 CLEAN-3 同交付。
保留真实流式顺序，不因 RoundDelta/RoundCompleted 有旧名字就忽略其实际调用。

验收：ask/plan 正文、pin 释放、权限交互、工具结果、activity、流式次序、SSE 重连；
证明事件路径不再通过 wire 往返触发业务。最终 DomainEvent/emit_domain 及转换实现为零。
接管：T2.1/T2.2。命令模型不随事件删除。

#### CLEAN-3 — 一次完整切换事实源与正文

写集：session/canonical、session_fact_v2、projection、manager、team/store；message/store、effect、wal；
runtime 上下文/持久化/恢复/timeline；daemon 内容读取；types/image_store 调用者与 migrate 模块。
这是完整垂直任务，可多个提交，不能以只接通新 writer 为完成条件。

实施顺序：
1. 持久 blob 落盘与引用校验；事实引用不能指向 TTL 缓存或未落盘哈希。
2. 完整 turn 生命周期/整轮模型消息/工具结果/压缩/撤回/注入生产者；类型改名必须匹配真实职责，
   先审 ToolLedger 的全部写入与消费者，再确定 SessionLedger 的公开入口。
3. 单一 apply(ContextOp)，live 与 replay 共用；保留 thinking signature/加密 reasoning/工具顺序。
4. 在静态样本上离线导入并比对，上下文和人类归档分别验证，不启动生产双写。
5. 停写、备份、一次性幂等 migrate、版本标记、切换全部正式读写入口。
6. 删除 messages.wal、LegacyWriterFacade、消息独立直写、SaveFull/重写归档、migrate-on-read、
   timeline 推算 turn ID 与统一 Completed 补偿、以 TTL 代替业务终态的路径。
7. 补齐 ARCHITECTURE 存储表：包含 team/board、meta 的配置字段、索引、recovery intent，
   不把独立配置/密钥/设备登记误称会话缓存。

历史旧数据没有的终态记录为未知/历史导入，不伪造成功。文件 undo journal 保留必要职责。
迁移故障不能默默跳过；备份和恢复入口由离线运维控制，正常 reader 没有旧格式 fallback。

验收：删可重建投影后完整恢复；live==replay；正文可解析；各落盘/执行接缝崩溃注入；
副作用工具不重复执行；不确定执行明确拒绝盲重放；取消/失败/重启中断终态正确；
重启后不可答交互明确收口；重复迁移幂等且数据不丢。历史终态未知单独断言。
接管：T1.1–T1.4、T2.3、T3.1–T3.3、T4.4/T4.5 的恢复部分；禁止旧 T1.3 shadow。

#### CLEAN-4 — 状态显式、取消单树、锁与生命周期

写集：permission/lib.rs、fs-core/file_state.rs、file-tools/pending/read_image；workspace/runtime、
tool_side_fold、audit；process registry；runtime/types、loop_outcome、registry/service；
MCP/LSP/gate runtime 装配，daemon composition。

建立由会话/工作区拥有的显式状态，工具仅拿调用需要的句柄。进程共享资源与会话状态分别列所有者。
删除 session/workspace/cancel 全局/TLS 与 resolver fn 钩子、双取消机制；取消树覆盖工具和子代理。
lap 递归改循环，按职责拆函数；Registry 锁内不 IO/block_on/阻塞 send/回调。
runtime 共享方式先取证；没有性能问题证据也要消除隐藏全局构造入口，但不声称提速。

验收：两会话同时读写/取消不串状态；子代理取消传播；小栈大量 lap；退出无悬挂线程；
并发工具和锁顺序探针；若改 runtime，记录前后同负载延迟及阻塞栈。
接管：T0.1、T4.1–T4.3，T4.4 剩余运行态部分。

#### CLEAN-5 — crate 职责迁移，拆完旧门面一起删

写集与目标由 ARCHITECTURE §3/§4 决定，先提交符号/消费者迁移清单再动代码。

* 抽 session-api 纯契约，client 不再依赖 session 存储；清理旧路径导出与 DTO 重复镜像。
* 抽 platform 数据根/bootstrap；图片 IO 进入 session blob；tokenizer/上下文计数脱离 types
  默认依赖。纯 types 不初始化环境、不读写磁盘。
* runtime 的 device/lease/driver/wire hub 归 daemon 模块；纯 timeline 折叠/重建归 session。
  只迁其职责内代码，不能将全部 ringing/ 机械搬到 daemon 而产生循环依赖。
* service 初始化/全局安装归 daemon composition；workspace/git 转导出删，服务直接依赖 git。
* 工具登记契约归 tool-core；MCP/LSP/subagent 只依赖登记契约，不依赖 workspace 具体 manager。
  登记抽象只覆盖真实使用的 register 操作，不把授权/执行/会话状态搬进 SDK。
* Skill effect 契约与实现解耦；domain 展示适配退出文件工具；process→file-tools 边按调用点削减。
* types/tool-core 的 ToolResult/ToolOutcome/display 转换逐消费者收敛，provider 与 UI 各自
  在边界取所需投影，删中间往返与旧 helper；不复制第二套同义类型。
* title 的 session 依赖按实际调用者决定是否删除；小 crate 不为数量好看强行合并。

验收：普通生产依赖图无环；client 不带 session writer；MCP/LSP/subagent 不依赖 workspace；
tool-core 不依赖 skills 实现；types 无环境 IO/默认 tokenizer；旧 shim、exports、Cargo 依赖为零。
desktop/TUI 实际外部调用者与 TS 导出同步验证，提交 SHA 写回这里；外部未验证就不能称 RC 可用。
两个新增 crate 各有纯契约/平台接缝测试；不要给机械搬动逐函数造镜像测试。

#### CLEAN-6 — 文件、公共 API 和文档收尾

将超过 1500 行的热点按现有职责拆模块：registry/timeline/engine_turn/manager/store/
axum_server/config/subagent lib。先删重复逻辑，再拆留存行为。tests 从大文件分离时分别报告行数。
拆模块不扩大 pub，优先 pub(crate)；公共错误改具名类型；过多参数收敛为职责明确的输入结构。
删除迁移历史注释、无消费导出、重复 schema/声明、无用 features/依赖、已失效工具脚本。
清理 version 脚本的旧仓名字、root package 描述、无消费者版本锁等须附引用核对。
architecture-report.md 明确归档历史，所有当前说明只引用 ARCHITECTURE 与本节。
不删除用户未跟踪文件，不把批量格式化混到职责迁移里。

验收：无正常读取兼容分支、无旧符号 shim；修改的大文件不增肥、新文件 <=1000 行；
职责/依赖/存储表与代码一致；记录真实净删、搬移、新增生产代码、测试代码和第三方依赖变化。
接管：T0.2/T0.4 余项与旧注释/文档收尾。

#### CLEAN-7 — 完整回归与 RC 判定

每刀完成后已做专项回归；本任务在最终 clean HEAD 上跑：

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked -- --test-threads=1
```

补充：新显式状态测试正常并发运行，不能靠单线程掩盖串会话；跨平台构建/CI、daemon/client
冒烟、desktop/TUI、迁移/重启/断线、长会话压缩/undo/steer/并行工具/图片/子代理/文件变化。
不依赖外部模型服务的确定性接缝用 mock；在线模型探针单独记录环境和结果。
现有 fmt/clippy 基线失败重新实测定位，不用永久白名单、放宽 lint 或删行为测试解决。
无法完成的项目列“未验证 + 原因”，RC 判定保持未通过；不以文档或行数降幅替代行为验证。

### 10.4 执行模型交付模板

```text
任务：CLEAN-N；起始/结束 SHA：
本次解决的职责冲突：
删除的符号/路径/依赖（附搜索证据）：
迁移到的唯一所有者与正式调用链：
净删 / 搬移 / 新增代码（生产与测试分别说明）：
专项回归命令、结果与未验证项：
异常/崩溃/数据迁移结果（如涉及）：
同步的外部消费者与 SHA（如涉及）：
ARCHITECTURE 更新：
新发现但未扩大实施的问题：
```

### 10.5 本轮验证记录

版本来源 version.txt/Cargo.toml/package.json 一致，Cargo.lock 的 26 个 qaqh-* 已同步。
cargo metadata --locked 完整依赖解析通过；只解析 metadata，没有编译或运行测试。
metrics 中登记了 29 个 crate 的源码体量与普通直接依赖（含 target 条件、不含 dev/build 边）。
本轮不改 Rust 行为，因此不执行全工作区编译/回归；CLEAN-1–CLEAN-7 均未实施。
原 main 领先远端的 6 个提交已由 owner 明确授权一并同步。

### 10.6 基线与 worktree 变更（2026-10-07 晚）

clean 工作树此前混着**两条无关线**的未提交改动（+477/−197，9 个 tracked 文件 + 1 新测试）。
按文件与 hunk 内容分组后分别落库；`prompt.md`、`.zcode/` 按 §10.1 保持在未提交状态、未进任何提交
（`AGENTS.md` 的本地删除与 `AGENTS-x.md` 改名已于 `9d0a26c` 入库）：

* **`35fa46e`（在 clean）** 交互终态收敛（幽灵审批）：`runtime/agent/engine_turn.rs`(+153)、
  `turn_actor.rs`(+21)、`turn_lap_test_api.rs`(+66)、新测试 `ask_resolution_projection_fold.rs`(296 行)、
  `session/projection/control.rs`(+40)、`session/tests/control_projection.rs`(+101)、
  `daemon/axum_server/axum_impl/v2.rs`(+14 模态优先级)。
  这是 **D9/E21 的运行态部分**，也是 §5 T0.3（重启僵尸交互取证）的实施证据：`InteractionExpired` 的
  生产点不再只有 `session/actor.rs:628` 一处，**E21 的记录已过期**，CLEAN-2/3 复核时要按现状重数。
* **`feat/lan-pairing`（从 main 切，不在 clean）** LAN 双 listener + `discovery` 的
  `lan_endpoint`/`tls_fingerprint`（`daemon/main.rs`、`daemon/server.rs`、`types/discovery.rs`）。
  属 `docs/plan-mobile-remote-access.md` 产品线；§1 明确 clean 不加产品功能，故不入 clean 历史。

**约束（对后续派工有效）**：CLEAN-3/CLEAN-4 **不得另起第二套 expiry 生产者**。turn 终态与挂起表
清理由 `35fa46e` 承接后，事件词汇收敛（CLEAN-2）与事实 schema 迁移（CLEAN-3）必须复用
`canonical_interaction_id` 的 id 对齐口径（wire id 与 `int_<ULID>` 两侧都认），不得再引入第三种 id 形态。

验证（在 clean HEAD `35fa46e` 上实测，非声称）：`cargo check --workspace --all-targets` = 0 error；
`cargo test -p qaqh-session -p qaqh-runtime -p qaqh-daemon -p qaqh-types` = **766 passed / 0 failed**
（76 个目标）；新回归 `ask_resolution_projection_fold` 3 passed、`control_projection` 6 passed。
**未验证项**：全工作区 `cargo test --workspace`（DoD 要求的 172 目标/1842 测试那次是在 T5 上跑的，
本提交未重跑）、`cargo clippy -D warnings`、v2-smoke/v2-legacy-compat-probe、`just ts-check`（在
`qaqh-desktop-app` 仓）。CLEAN-1 完成后须在施工 worktree 内补齐这几项。

**量尺基线**：新增快照 `docs/metrics/clean-start-35fa46e.json`（29 crate 合计 **152,594** 行；
runtime 38,533 / session 19,063 / daemon 9,655，相对 `ebcf0f8` 各 +194/+40/+4）。
固定基线 `clean-baseline.json`（`ebcf0f8`）**未被覆盖**，净删一律以 `clean-start-35fa46e.json` 为分母。
§8 的 ratchet 计数已填实测值并固化测量命令；同时更正两处：原"765 行（排除 tests）"实为**含 tests** 口径
（排除 `/tests/` 是 657），原"too_many_arguments 50"按属性字面计数应为 **48**。
另更正 §10.3 CLEAN-1 的"旧 Next 3 Actions"悬空引用（现库与 `docs/archive/` 均查无出处）。

**worktree 拓扑（施工期固定）**：

```text
E:/qaqh-backend        -> main         可用基线；qaqh-tui-app 的 path 依赖指向这里，保持可编译
E:/qaqh-backend-clean  -> clean        CLEAN-1…7 唯一施工处；独立 ./target
E:/qaqh-backend-v2.1   -> feat/profile-p1  原样保留：8 个未推送提交 + 9 个脏文件（含 config/migrate.rs、
                                    permission/tier_file.rs 两个新源文件），从未 push，不做任何移动或删除
```

已移除 `C:/Users/tsy3m/.qoder/worktrees/app/8ebe21/qaqh-backend`（refactor/sdk-gate-and-byok：
工作树零脏文件、零 stash、分支已并入 main 且与 origin 同步）及其**本地**分支；
`origin/refactor/sdk-gate-and-byok` 未动。`v2.1`、`fix/hotfix`、`webui-export`、
`research/tool-system-modernization` 全部保留未删——删除属不可恢复动作，未获逐条确认。
`origin/clean` 现落后本地 1 个提交（`35fa46e`）；推送会改动共享分支与 PR #13，等 owner 点头。
