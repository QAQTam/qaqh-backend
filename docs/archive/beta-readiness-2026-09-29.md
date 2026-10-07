# Handoff — beta 就绪快照（2026-09-29 晚）

> 取代 `docs/handoff/hub-fact-bus-stage-1.md`（已删除；hub-fact-bus spec 阶段 1-3 的
> 逐条执行记录随其留档于 git 历史，本文件只保留仍然有效的事实与挂点）。
> 任务来源：`docs/spec/hub-fact-bus-refactor.md`（领取自 `docs/archive/legacy-compat-cleanup-draft.md` H1 条目）
> + `docs/plan-beta-readiness.md`（排期权威）。
> 当前状态：**beta 门禁 G1-G5 全部完成**；W1-W6 全部完成（见「二·五」节）；
> hub-fact-bus 主体收尾完成，遗留项已挂 P4。

## 二·五、第二批（2026-09-29 深夜）：W 类全清 + BUG-01 回归锁

裁决：WinUI **暂不包含 beta 交付面**（W5 不升级门禁、W4 留 backlog）；
daemon/后端一律按 v2 严格语义演进、不做 v1 兼容层。

- **BUG-01 回归测试**（`crates/qaqh-daemon/src/axum_server.rs`
  `session_create_over_commands_channel_attaches_created_seed_to_the_lease`）：
  真实路径锁（session.new 落盘 + in-process worker spawn，同 smoke-g1 流程），
  断言 ack accepted + `owned_sessions(lease)` 恰含新建 seed，结束 close+delete
  清理。旧代码下该测试必红（attach 恒 false → 401）。
- **W1（C3）**：`ProjectionEvent` 加 `ts_ms: Option<i64>`（源 fact 提交时间，
  合成 ephemeral 为 None）→ 信封 `ts_ms: Option<u64>`（serde default，向后兼容）。
  回归断言挂 `hub_bootstraps_from_committed_canonical_facts`。
- **W2（D4/D5/D8）**：webui 消费 `before_index`（回合 `turn_index` 全局序号）
  触顶懒加载 + scrollHeight 锚点补偿；页淘汰上限 400 回合（运行中回合不淘汰，
  尾部被淘汰后触底自动重拉最新页）；单测 `webui/tests/transcript-pagination.test.ts`。
  ⚠️ 随此发现并修复后端缺口：**归档深翻页此前不回填 `turn_index`**（只有常驻
  窗口分支回填），跨窗口边界翻页即断链；`timeline_api.rs` 归档分支已补齐。
- **W3（D10，严格 v2）**：调查结论——**`CompactionApplied` fact 零产生点**
  （conversation/meta/timeline 三个投影与 pending_store 回执折叠全是死链；
  handoff P4 的"fact 产生侧缺口"判断在压缩面同样成立）。本轮补齐：
  - 后端：`ToolLedger::append_compaction_applied`（`replaces_through_fact_seq` =
    追加前 canonical head，空日志校验拒绝）；engine_compact（手动）与
    engine_turn（自动）两条成功路径在 `persist_compaction` 后、
    `CompactFinished` 域事件前 durable append；失败降级记日志不拖垮 turn。
    `summary_ref` = 摘要文本 sha256（与 args_ref/request_ref 同口径，正文
    本就在 messages.jsonl）。session 侧 2 条 ledger 测试。
  - 前端：conversation 流 `compaction_applied` → 「此前已压缩」分隔
    （锚定事件时刻最后回合后；锚点淘汰后渲染顶部；快照重载清除）。
- **W6**：session-forensics 删除 `compact-context.json` 死解析链
  （`Session.compact`/info 四死字段/evidence 死分支/selftest 样例），换 route 1
  活口径（meta `compact_covered_through_msg_id` + messages.jsonl 内
  `[Compacted N turns]` 摘要识别）；README 同步。`meta.compact_skip` 核实为
  活字段（resume 路径在用），保留。
- **v1 兼容残留检查**：`RingingEvent` 为 worker 事件泵活跃别名（非残留）；
  `v2/mod.rs` 过期"兼容窗口"注释已修；command 信封 version=1 是活跃合同，
  硬切留 P 阶段评估。
- **回归基线（全绿）**：runtime --lib 265 / session 全套（含新 2 条）/
  daemon --bins 66（+1）/ gateway --lib 21 / v2 验收矩阵 9 / host_direct 6 /
  cancel 4 / webui tsc 0 错 + bun test 11（+6）。workspace 全量与 daemon 集成
  测试仍按磁盘预算暂缓，随 beta 前最后一次构建补跑（G5 口径不变）。

## 一、本轮（2026-09-29 晚）完成的改动

### 协议裁决表缺口补齐

- **B9 前端断线重连**（G3）：
  - `crates/qaqh-webui-gateway/src/lib.rs`：`proxy_events`/`proxy_timeline_events`
    白名单透传查询参数（`since_cursor` / `last_event_id`），新增
    `forward_query_param` + 回归测试 `forward_query_param_whitelists_reconnect_cursors`；
  - `webui/src/state.ts`：自管重连（指数退避 500ms×2^n 封顶 30s + 半程抖动），
    重连 URL 携带 cursor（events 记 `envelope.cursor`，timeline 记 SSE 帧
    `lastEventId`）；连续失败 >1 次丢弃 cursor 降级纯实时（防 cursor_expired
    死循环）；切 seed 清空游标；`onopen` 重置退避计数；closeStreams 清 timer。
- **W1 前置**：未动（信封 `ts_ms` 仍待加，见 plan W1）。

### 缺陷修复（冒烟揪出，均有端到端证据）

- **BUG-2026-09-29-01**（`crates/qaqh-daemon/src/axum_server/axum_impl/command.rs`）：
  SessionCreate 分支 `if let Some(session_id)` 遮蔽外层 `session_id`（header 的
  client_session_id），`attach_session(&seed, &seed)` 恒 false → commands 通道上的
  SessionCreate 必然 401 回滚（会话已创建却被拒归属）。修复：`if let Some(created)`，
  attach 宿主改为发起命令的 lease（与 SessionResume 分支同语义）。
  ⚠️ 残留风险：修复前经此路径创建的会话处于「无归属」态，如 beta 数据中存在
  不可 attach 的孤儿会话，优先排查此因。
- **BUG-2026-09-29-02**（`crates/qaqh-webui-gateway/src/daemon.rs`）：
  `DaemonClient` 的 reqwest client 级 `timeout(REQUEST_TIMEOUT=15s)` 覆盖整个请求
  周期（含响应体流）→ SSE 长连接 15s 整被网关掐断，浏览器每 15s 断流重连、
  B10 keep-alive 永远到不了前端。修复：新增无总超时的 `stream_http` client，
  `get_stream` 专用。

### 测试修复

- `crates/qaqh-runtime/tests/host_direct.rs`（§5.3/§5.4 遗留项，6/6 绿）：
  重写至 v2 语义——注入路径对齐 `v2_acceptance_matrix` fixture
  （`CanonicalSessionStore::append` + `ProjectionSink::publish`）；订阅用独立
  分配的 seed（不与子代理 actor 的 writer 租约竞争）；先 append SessionCreated
  建立提交基线（空日志 session 的 v2 subscribe 返回 SnapshotMissing，桥会断）；
  断言 batch.session_id 过滤 + `CollectorEvent::TurnFinished` 映射。
- `crates/qaqh-runtime/tests/cancel_keeps_tool_results.rs`（G4，4/4 绿）：
  `cancel_mid_batch_keeps_executed_tool_results` 环境失败定性 = PATH 无 `sh`
  （工具 spawn 失败，非取消时序）；加 `sh_available()` skip 守卫。

### 端到端冒烟（G1）

- 新增 `scripts/smoke-g1.ps1`（可重复执行）：daemon（真实 `~/.qaqh` 数据根，
  `--bind 127.0.0.1`）→ gateway → 浏览器 HTTP 全流程：
  bootstrap nonce → gateway session（cookie+CSRF）→ sessions →
  daemon lease open → **SessionCreate ack accepted** → 会话入列 → gateway attach →
  **三条 SSE keep-alive 均到达**（daemon 直连 / gateway events / gateway timeline）→
  approvals → SessionDelete 清理。
- 已知设计行为（非缺陷）：webui 无从零建会话流（网关命令要求 active seed，
  白名单仅 SendMessage/Cancel，审批走一次性 challenge）；网关对
  session_delete 返回 403 `command_not_allowed`。
- SSE 帧验证口径：安静会话以 keep-alive 注释帧为准（15s 节奏，脚本窗口 18s）；
  真实 turn 的事件到达由 v2 验收矩阵 + 首次真实使用覆盖。

## 二、回归基线（全绿）

| 套件 | 结果 |
|---|---|
| `cargo test -p qaqh-runtime --lib` | 265 passed |
| `cargo test -p qaqh-daemon --bins` | 65 passed |
| `cargo test -p qaqh-webui-gateway --lib` | 21 passed |
| runtime `--test host_direct` | 6 passed |
| runtime `--test cancel_keeps_tool_results` | 4 passed |
| webui `tsc --noEmit` / `bun test` | 0 错 / 5 passed |

未跑（磁盘预算，beta 前最后一次构建补跑）：workspace 全量、daemon 集成测试。

## 三、遗留 / 下一步（按 plan-beta-readiness 挂点）

- **W1-W6：已全部完成**（第二批记录见「二·五」；plan 已勾）。W4/W5 裁决 =
  WinUI 暂不交付，不升级门禁。
- **P4（hub-fact-bus 遗留债，from 旧 handoff，仍然有效）**：
  - §5.4：hub.rs 21 个退役锁 v1 语义测试的重挂 + lease_store 1 个（需按 fact 面
    重新表述，不阻塞 beta）；
  - §4.0.4：orphan_seal 4 类补终态的 fact 补写（canonical ledger 句柄在
    agent/session 侧，RingingHub 不持有；需先做归属裁决）；
  - SessionActivityChanged 的 fact 产生侧（活动推送现为查询轮询）；
  - typed `existing` replay 的 fact 侧重建（可选）；
  - §6：timeline 归属决策（`timeline_hub` 是否并入 fact 总线，与 P2 协同裁决）。
  - （本轮新证据：压缩面 fact 产生侧缺口已由 W3 补齐，同类缺口的
    TurnStarted/TurnFinished/AssistantBlock fact 产生侧仍缺——v2 events 流上
    `turn_finished`/`assistant_block_sealed` 等 kind 目前实际不可达，与 P2/P4
    协同裁决是否补齐。）
- **BUG-2026-09-29-01 回归测试：已完成**（第二批，daemon --bins）。

## 四、已知存量问题（HEAD 基线，非本轮引入）

- `cancel_keeps_tool_results::cancel_mid_batch_*`：无 `sh` 环境跳过（已加守卫）。
- `qaqh-sandbox` unused import / `qaqh-mcp` unused variable 警告（存量）。
- 文档勘误：`docs/archive/legacy-compat-cleanup-draft.md` G 节勾选已改回未执行
  状态（crates/qaqh-tui 实际不存在）。

## 五、文件清单（本轮全部改动）

- `crates/qaqh-daemon/src/axum_server/axum_impl/command.rs` — BUG-01 修复
- `crates/qaqh-webui-gateway/src/daemon.rs` — BUG-02 修复（stream_http）
- `crates/qaqh-webui-gateway/src/lib.rs` — 查询透传 + 回归测试
- `webui/src/state.ts` — 自管重连 + cursor 记录
- `webui/src/lib/ringing.ts` — eventsUrl/timelineSseUrl 支持游标参数
- `crates/qaqh-runtime/tests/host_direct.rs` — v2 语义重写
- `crates/qaqh-runtime/tests/cancel_keeps_tool_results.rs` — skip 守卫
- `scripts/smoke-g1.ps1` — 新增端到端冒烟
- `docs/plan-beta-readiness.md` — 排期权威（G1-G5 已勾）

第二批（W 类 + BUG-01 锁）新增改动：

- `crates/qaqh-session/src/session_fact_v2/projection_event.rs` — ProjectionEvent.ts_ms
- `crates/qaqh-ringing/src/v2/types.rs` — 信封 ts_ms（C3）
- `crates/qaqh-ringing/src/v2/mod.rs` — 过期兼容窗口注释修正
- `crates/qaqh-runtime/src/ringing/v2.rs` — 信封透传 ts_ms + 回归断言
- `crates/qaqh-session/src/canonical/tool_ledger.rs` — append_compaction_applied
- `crates/qaqh-runtime/src/agent/plugins/engine_compact.rs` — 压缩 fact 产生侧（共享 helper）
- `crates/qaqh-runtime/src/agent/engine_turn.rs` — 自动压缩成功路径接线
- `crates/qaqh-session/tests/tool_ledger.rs` — 压缩 fact 2 条测试
- `crates/qaqh-runtime/tests/v2_acceptance_matrix.rs` — 信封字面量补 ts_ms
- `crates/qaqh-daemon/src/axum_server.rs` — BUG-01 回归锁
- `crates/qaqh-daemon/src/axum_server/axum_impl/timeline_api.rs` — 归档页 turn_index 补回填
- `webui/src/lib/transcript.ts` — oldestIndex/tailTruncated/compactMarker + 前插/淘汰
- `webui/src/state.ts` — loadOlderTurns/reloadLatestTurns + compaction_applied 分派
- `webui/src/App.tsx` — 触顶加载 + 锚点补偿 + 压缩分隔渲染
- `webui/src/styles.css` — .compact-divider
- `webui/tests/transcript-pagination.test.ts` — 翻页/淘汰/分隔 6 条单测
- `tools/session-forensics/session_forensics.py` + `README.md` — 死解析清理（W6）
- `docs/archive/legacy-compat-cleanup-draft.md` — 已归档（G 节勘误后留档）
- 本文件 — 取代 hub-fact-bus-stage-1.md
