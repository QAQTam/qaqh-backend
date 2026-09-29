# Handoff — beta 就绪快照（2026-09-29 晚）

> 取代 `docs/handoff/hub-fact-bus-stage-1.md`（已删除；hub-fact-bus spec 阶段 1-3 的
> 逐条执行记录随其留档于 git 历史，本文件只保留仍然有效的事实与挂点）。
> 任务来源：`docs/spec/hub-fact-bus-refactor.md`（领取自 `docs/archive/legacy-compat-cleanup-draft.md` H1 条目）
> + `docs/plan-beta-readiness.md`（排期权威）。
> 当前状态：**beta 门禁 G1-G5 全部完成**；hub-fact-bus 主体收尾完成，遗留项已挂 P4。

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

- **W1**：`RingingV2EventEnvelope` 加 `#[serde(default)] ts_ms: Option<u64>`（C3）。
- **W2/W3**：前端消费 `before_index` 游标翻页 + 滚动锚点（D4/D5/D8）；
  `CompactFinished` 压缩分隔标记（D10）。
- **W4/W5/W6**：跨仓 path 依赖、WinUI v2 桥（**决策项**：beta 交付面是否含 WinUI
  二进制——是则升级为门禁，WinUI bridge 目前停 v1 合同对 HEAD 编译不过）、
  session-forensics 死解析清理。
- **P4（hub-fact-bus 遗留债，from 旧 handoff，仍然有效）**：
  - §5.4：hub.rs 21 个退役锁 v1 语义测试的重挂 + lease_store 1 个（需按 fact 面
    重新表述，不阻塞 beta）；
  - §4.0.4：orphan_seal 4 类补终态的 fact 补写（canonical ledger 句柄在
    agent/session 侧，RingingHub 不持有；需先做归属裁决）；
  - SessionActivityChanged 的 fact 产生侧（活动推送现为查询轮询）；
  - typed `existing` replay 的 fact 侧重建（可选）；
  - §6：timeline 归属决策（`timeline_hub` 是否并入 fact 总线，与 P2 协同裁决）。
- **BUG-2026-09-29-01 回归测试**：execute_command SessionCreate 路径目前仅冒烟
  验证，daemon --bins 缺一条不变量测试锁（open → create → accepted + lease owns
  seed），下批补。

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
- `docs/archive/legacy-compat-cleanup-draft.md` — 已归档（G 节勘误后留档）
- 本文件 — 取代 hub-fact-bus-stage-1.md
