# Ringing v2 P0-4 / P0-5 落地 + 实机调测 Handoff（2026-09-24）

状态：**P0-4 全部落地、P0-5 daemon 侧落地、实机 smoke 通过**。承接
`docs/handoff/2026-09-24-ringing-v2-p0-3-p0-4-handoff.md` 的「下一步顺序 1–2」。

## 1. 当前基线

- 集成分支：`main` @ `59cb30d`
- 本分支：`feat/ringing-v2-typed-existing-result`
- 前序 commit：`a47504f`（typed existing result 第一刀）
- TUI anchor 不变：`b77c251` / tag `tui-ringing-v2-interaction-causation-2026-09-24`

## 2. 本次落地

### 2.1 P0-4a：canonical interaction 结构化裁决

`InteractionResolved` 之前只有 `decision_ref`（内容 hash），读不出裁决语义。
新增：

- `qaqh-session::session_fact_v2::InteractionDecision`
  （`approved` / `rejected` / `answered` / `dismissed`），作为
  `InteractionResolved.decision: Option<...>` **增量字段**（老 fact 反序列化为
  `None`，不 bump payload_version）。
- `ControlInteractionResolution.verdict` + `ControlDelta::InteractionResolved.verdict`，
  canonical → projection 全程透传。
- `ToolLedger::interaction_resolution_matches` 对 `decision` 做兼容比较：
  老 fact（`None`）仍幂等，新 fact 不同裁决即 `InteractionTerminalConflict`。
- runtime `record_interaction_resolution` 把 decision 字符串映射成枚举写盘。

### 2.2 P0-4b：v2 ack typed existing result

- `RingingV2CommandAck.existing: Option<RingingV2ExistingResult>`（`source` 标签）：
  - `command_receipt`：同 `command_id` 重放，带 receipt 的 state/terminal_event_id/
    error_code/result；
  - `interaction_resolved`：second-answer 路径，带**获胜裁决**。
- daemon 在 v2 command 入口同步判定：
  - 命中 TTL 内 receipt → 直接回 `existing`，不下发 worker；
  - 命中 canonical 已解决 interaction（`V2ProjectionHub::control_interactions`，
    只 clone control projection）→ 200 `rejected` + `interaction_already_resolved`
    + `existing.interaction_resolved`。
  - HTTP 保持 200，避免 client 的 `api_error` 把 typed payload 压成
    `{code,message}`。
- `RingingV2CommandStatus.result` + `GET /ringing/v2/commands/{id}` 同步带 typed result。
- `qaqh-client` 增量 API：`send_command_v2_typed` / `command_status_v2_typed`；
  旧 `send_command_v2` / `command_status_v2` 签名不变。

### 2.3 P0-4c：V2-R1..R4

- `v2_pending_interactions_survive_reconnect`：permission / ask / plan 三种
  pending interaction 在两次 bootstrap（重连）后 `interaction_id` 稳定。
- `v2_second_answer_returns_winning_verdict`：ask 的 first-answer-wins typed 裁决。
- 老 fact 兼容 + 冲突检测由 `qaqh-session` 单测覆盖。

### 2.4 P0-5：driver capability（daemon 侧）

- 新增 `qaqh-runtime::ringing::RingingDriverStore`：
  per-seed holder + 单调 `driver_epoch`；claim 幂等、busy 拒绝、lease 失效可接管、
  release 只允许 holder。
- 新增 v2 端点（真实 HTTP 路由）：
  - `POST /ringing/v2/sessions/{seed}/driver/claim`
  - `POST /ringing/v2/sessions/{seed}/driver/release`
- bootstrap 输出真实 `driver { holder, driver_epoch, can_claim }`。
- command 准入：
  - 非 driver 的 Conversation / session 控制命令 → `not_driver`；
  - `driver_epoch` 与当前不一致 → `stale_driver_epoch`；
  - **interaction 回答不 gated**（非 driver 仍可回答 permission/ask/plan）；
  - **无 holder 时放行**（未 claim 的会话保持可用，兼容尚未 claim 的 TUI）。
- `RingingV2CommandEnvelope.driver_epoch: Option<u64>`（增量字段）+
  `CommandOptions.driver_epoch`。

### 2.5 P0-6（部分）

- 新增 reset fixture：`log_id_mismatch`、`unknown_fact`（`V2ProjectionHub` 单测）。
- open/bootstrap golden 已有：wire 侧 `bootstrap_json_matches_frozen_shape`、
  daemon 侧 `v2_open_bootstrap_uses_canonical_projection`。
- driver claim/busy/handover 由 daemon 路由测试覆盖。

### 2.6 实机调测

- 新增 `scripts/v2-smoke.sh`：构建 daemon → 独立 data root 起进程 →
  `crates/qaqh-session/examples/e2e_seed.rs` 播一个含已解决 ask 的 canonical
  session → 走真实 HTTP/SSE 全链路。
- 覆盖：open、bootstrap、driver claim/busy/release、`not_driver`、
  first-answer-wins typed 裁决、command replay typed receipt、command status、
  SSE subscribe。

## 3. 验证证据

```text
cargo test --workspace -- --test-threads=1        PASS
cargo clippy --workspace --all-targets -- -D warnings  PASS
scripts/v2-smoke.sh                               PASS（可重复运行）
```

实机关键响应（原文节选）：

```json
{"accepted":true,"holder":"c98b…","driver_epoch":1,"reason":"claimed"}
{"accepted":false,"holder":"c98b…","driver_epoch":1,"reason":"driver_busy"}
{"command_id":"cmd-b-cancel","status":"rejected","code":"not_driver"}
{"command_id":"cmd-second-answer","status":"rejected",
 "code":"interaction_already_resolved",
 "existing":{"source":"interaction_resolved",
             "result":{"kind":"ask_resolved","interaction_id":"int_…","outcome":"answered"}}}
{"command_id":"cmd-replay-1","status":"accepted",
 "message":"duplicate command_id (already completed)",
 "existing":{"source":"command_receipt","state":"succeeded"}}
```

## 4. 未决事项（alpha 迭代清单）

### A. 架构 / 契约（需要决策，不宜直接做）

1. **canonical `DriverChanged` reliable 事件**：driver holder 是 daemon lease
   概念，写 canonical fact 需要决定单一写者（daemon 直写 vs 转 worker 命令）。
   当前 driver 状态只在 bootstrap 快照里，没有 reliable 增量事件。
2. **driver 身份与 lease 续期**：holder lease 过期后当前按“视为空位”处理，
   epoch 只在下次 claim 时 +1；是否需要 daemon 周期任务主动 bump + 广播待定。
3. **`driver_epoch` 是否进 command 指纹**：当前作为准入 guard、不参与指纹；
   若前端需要「同 id 不同 epoch」区分重放，需重新定契约。
4. **driver gate 集合**：当前 gate = Conversation 全部 + session/skill/tool-mode
   控制；workspace 类命令尚未纳入（需确认 workspace 命令面）。

### B. P0-6 剩余 fixture

1. reliable / replaceable / ephemeral transcript fixture（顺序、去重、不回放）。
2. `ResetRequired` 剩余 reason：`cursor_expired` / `snapshot_missing` /
   `cross_session` / `v1_epoch_mismatch` / `replay_overflow`。
3. concurrent answers 的 permission / plan 变体（当前只有 ask 路由级用例）。
4. driver handover 的 reliable 事件断言（依赖 A-1）。
5. **v1 `Last-Event-ID` → v2 cursor 映射 fixture**（服务端映射表尚未实现）。
6. Windows alpha 共用同一 fixture（当前无 Windows 实机）。

### C. P1 未开工

1. `/ringing/v2/service/{method}`。
2. `/ringing/v2/content`（当前 v2 路由未挂 content）。
3. timeline v2 完整分页与重连（client 有 `timeline_v2`，daemon 侧仍是 v1 路径）。

### D. 落地细节

1. permission 的 `trust_folder` 未进 canonical decision（当前只存 approved/rejected）。
2. `InteractionResolved.decision` 为 `Option`，老日志裁决不可恢复（只有 hash）；
   alpha 可考虑一次性回填或标记只读。
3. `CommandOptions` 新增 `driver_epoch` 是**破坏性结构体字面量变更**：TUI pin bump
   时需同步（用 `..Default::default()` 可规避）。
4. 实机调测需要真实 LLM key 才能让 canonical log 由正常 turn 产生；
   当前 smoke 用 seeder 直接写 canonical session。
5. CNB 流水线 Prepare 阶段 CPU 配额问题未解决，仍以本地全量门禁为准。

## 5. 接手注意

- 不要移动已有 v2 tag。
- v1 `RingingCommandAck` / `RingingCommandStatus` / `InteractionResolved` 的既有字段
  不得原地改语义；新增一律走增量字段 + `serde(default)`。
- 不要 bump `SESSION_FACT_PAYLOAD_VERSION`（会让老日志校验失败）。
- 新 interaction 结果类型要同步补 `qaqh-client` 别名与 `v2_public_api` 断言。
- 改 driver/interaction 必须同时覆盖 bootstrap、live event、reconnect 三条路径。
