# Bug：Ringing v2 命令全部卡在 Running（worker 从不产出终局事件）

> 状态：**已修复（fact 因果链重接）**，2026-10-05；遗留面见文末"未覆盖"
> 复现环境：Windows 11，qaqh-daemon v2.0.0-beta.2，`qaqh server --bind 0.0.0.0`，TUI + webui + 安卓端三客户端并存。
> 关联：`plan-mobile-remote-access.md` §9（第二审批者竞态）、§4.5（归因穿线）。

## 现象

任何客户端通过 `POST /ringing/v2/commands/{channel}` 提交的命令（含审批应答、会话消息等）在
daemon 侧**受理成功**（进入 pending store / receipts），但 **worker 侧从不产出终局事件**，回执永远停在
`Running`。表现：

- 手机端提交 `tool_permission_respond` → ack accepted → TUI 审批面板毫无反应，工具一直挂起；
- TUI 内本地批准（进程内直通路径）不受影响，因此症状呈现为"远程批准全部无效，只认 TUI"；
- `/approvals` 与 bootstrap 快照中的挂起交互不被这些命令清掉。

daemon 自身日志即已给出诊断关键词：`possible frozen worker/seal path`。

## 证据（2026-10-05 15:00–15:30 本地时间窗口）

1. **pending_store 大规模告警**（`~/.qaqh/qaqh-daemon.log`，同类告警共 **2487 条**）：

```
[1791185346] WARN qaqh_runtime::ringing::pending_store: [ringing] command receipt
stuck in Running for 1029s (no terminal event): 01M45EFDSEEDDSM92CFEMA7Y56
client_session=Some("97783ec2...") — possible frozen worker/seal path
```

   横跨 **至少 5 个不同 client_session_id**（`195c9f3c…` / `97783ec2…` / `c3db87ba…` /
   `12564e44…` / `6e66976f…`），且命令 id 同时存在 **UUID 形态**（安卓端生成）与 **ULID 形态**
   （桌面侧客户端生成）——排除单一客户端问题。

2. **回执账本**（`~/.qaqh/ringing-command-receipts.json`）：所有记录均停在受理态，
   `accepted_at_ms` 时间戳与告警窗口吻合（15:22–15:27），无对应 terminal 事件。

3. **命令确实被 daemon 校验放行**：同样的信封（`channel:"tool"` +
   `type:"tool_permission_respond"` + canonical `call_*` id + `session_id`=seed）若格式不合法
   会在 HTTP 层被 400/ack rejected 挡下，不会进入 pending store。安卓端 wire 格式另有
   MockWebServer 回环测试对齐（`qaqh-android` 仓库 `RingingRoundTripTest`）。

4. 对照组：同一时段 TUI 本地批准（不走 ringing 命令通道）一切正常，审计账本
   （`audit.csv`）中该时段存在 `user_approved` 记录——worker 进程本身存活，
   卡的是 **ringing 命令 → worker 的消费/seal 路径**，不是 worker 整体冻结。

## 复现步骤

1. `qaqh server --bind 0.0.0.0 --port 64413 --token <token>`，TUI 打开某会话；
2. 触发一次需要批准的工具调用（TUI 面板挂起）；
3. 另一客户端（webui 或任意 ringing v2 client）`POST /ringing/v2/commands/tool`：
   ```json
   {"schema":"qaqh.Ringing","version":2,"channel":"tool","command_id":"<uuid>",
    "client_instance_id":"<uuid>","client_session_id":"<lease>",
    "session_id":"<seed>",
    "command":{"channel":"tool","type":"tool_permission_respond",
               "tool_call_id":"call_...","approved":true,"trust_folder":false}}
   ```
   → 200 ack `accepted`；
4. 观察：TUI 面板不收敛；`pending_store` 在 ~60s 后开始对该 command_id 持续告警
   `stuck in Running`；`GET /approvals` 仍返回该挂起项。

**预期**：worker 消费命令 → 产出 `interaction_resolved`（reliable，`causation_id == command_id`）→
TUI 面板收敛、`/approvals` 清空、回执进入终态。

## 排查建议（供认领者参考）

- `qaqh_runtime::ringing::pending_store` 的告警点即观测位：命令受理后如何交付 worker
  （mailbox/seal？），交付失败/丢弃时是否有日志；
- 命令按 `session_id` 路由到对应 worker 进程的路径——多会话并存时是否路由到了不存在的
  worker（观察样本中多个不同 seed 的命令同时卡住）；
- worker 侧对 `InteractionRespond` 类命令的消费条件（是否要求提交方 attach / owns_session /
  driver epoch——注意审批应答按设计**免 driver 门控**，`axum_impl/v2.rs:798-820`）；
- 时间线上命令批量卡死早于安卓端介入（`c3db87ba` 租约的 ULID 命令更早），**怀疑与移动端无关**，
  webui 的远程审批可能同样受影响。

## 影响面

- 远程审批（webui / 移动端）全部失效，阻塞 `plan-mobile-remote-access.md` 的 M2 审批器验收；
- 命令账本被无终局回执持续污染（`receipts stuck` 告警每 10s 刷屏）；
- 不阻塞 M0 安全前置（TLS/pair/scope 与该路径无交集）。

## 修复验收

1. 上述复现步骤中，远程提交的 `tool_permission_respond` 在 TUI 收敛、`/approvals` 清空；
2. `pending_store` 不再出现无终局回执告警；
3. `v2-smoke.sh` 增加"第二客户端审批 → 第一客户端面板收敛"用例（建议稿 §9 的竞态验收顺带覆盖）。

## 修复记录（2026-10-05）

根因**不在投递路径**：daemon 日志里每条卡死命令都有 `[AGENT] received worker command frame`，
`~/.qaqh/sessions/*/events.jsonl` 也证实审批确实落成了 `interaction_resolved` fact 并让回合续跑。
断的是**归因（causation）→ 回执折叠**这一条腿。

hub-fact-bus 阶段 2.3（`bfbf74b`，2026-09-29）把回执折叠从 v1 事件链（`observe_terminal_event`，
事件信封带 `causation_id`）整体迁到 canonical fact 链（`observe_projection_events`），
但 fact 写侧从未接上：

| 断点 | 位置 | 后果 |
| --- | --- | --- |
| fact 的 `causation_id` 硬编码 `None` | `qaqh-session/src/canonical/tool_ledger.rs` 五个 builder 中的三个（`build_fact` / `build_conversation_fact` / `append_compaction_applied`） | tool/input/compact 类命令永无终态 |
| 非 ULID 命令 id 被就地丢弃 | `engine_turn.rs:422` `filter(is_ulid)`、`loop_dispatch_control.rs:428` | 安卓端（UUID）审批应答全量失效 |
| 折叠臂缺 `DriverChanged` | `pending_store.rs::observe_projection_events` | driver claim/release 的内部回执永挂 Running（v1 臂里有这条） |
| 巡检不看 TTL | `pending_store.rs::warn_stale_running` | 过期条目不被摘除，告警按 10s 无限重刷（实测 2487 条） |

本机全部 9 个会话的 fact 类型实测分布：`session_created 5 / driver_changed 13 /
input_accepted 42 / tool_intent 338 / tool_finished 338 / compaction_applied 1 /
interaction_requested 10 / interaction_resolved 9`，其中**带 causation 的只有 1 条**
（driver_changed，桌面 ULID）。

修法：

- `qaqh-session::canonical::FactCausation`——一个会话 actor 的"在飞命令"作用域格。
  `PacedEmitter` 本就持有这个作用域（v1 事件用它），现把它同时交给 actor 的
  `ToolLedger`（`loop_core.rs::from_channels` 绑定 → `AgentState::tool_ledger_mut` 注入），
  于是一次 dispatch 写出的 fact 与发出的事件归因**同源，不可能分叉**；
  显式传 causation 的 append 仍以显式值为准。
- `ledger::envelope()` 收口五个 append 的 fact 构造（原来五处各自 `SessionFact{}` 字面量，
  是这次漂移的土壤）。
- `canonical::causation_for_command(command_id)`：客户端命令 id 归一进 ULID 因果通道——
  原生 ULID 原样透传（与既有落盘形态一致），其余（UUID / 任意自由值）走
  `ulid_from_text` 派生，与 `turn_*` / `call_*` / `int_*` 同一套约定。
  **canonical log 磁盘格式与 v2 wire 契约都不动**（spec §6），客户端选定的字符串永不原样落盘，
  也不给 16MB body 留下把超长 id 写进每条 fact 的面。回执侧按同一函数反查
  （`PendingCommandStore::by_causation` 副索引，随 TTL 一起摘除）。
- 折叠臂补 `DriverChanged`；`warn_stale_running` 只覆盖 `warn_after..RECEIPT_TTL` 窗口，
  过期条目当场从两个索引摘除。

测试：
- `qaqh-runtime/src/agent/paced_emitter.rs`——作用域格与 emitter 的进入/嵌套恢复同源
  （fact 写侧绑的就是这一格，分叉即回执永不终态）。
- `qaqh-runtime/tests/input_accepted_producer.rs`——真 `Loop` dispatch 一条**非 ULID** 命令，
  断言落盘 fact 带归一后的 causation（改前 `left: None` 实测红）。
- `qaqh-session/tests/tool_ledger.rs`——作用域内每条 append 都带 causation、出作用域即清；
  `causation_for_command` 的 ULID 透传 / UUID 派生 / 超长 id / 空值四态。
- `qaqh-runtime/tests/interaction_request_ledger.rs`——UUID 命令 id 的审批应答在真实
  actor 写侧走通，且整段 log 仍通过 `SessionFact::validate`。
- `qaqh-runtime/src/ringing/pending_store.rs`——折叠经映射找回回执；过期条目转为静默摘除。
- `scripts/v2-smoke.sh` 新增"回执折叠到终态"相位：driver claim/release 之后轮询
  daemon 自己的 `ringing-command-receipts.json`，账本里不得残留 `accepted`/`running`。

### 未覆盖（遗留，需单独决策）

1. **纯对话回合仍无 fact 终态**：生产侧根本没有任何 `TurnStarted` / `TurnFinished` 的
   生产者（上面 fact 类型分布里为零），所以"发一条不带工具调用的消息"这类命令
   依旧只能靠 TTL 过期——现在过期即静默摘除，不再刷日志，但 `command_status` 在
   这 5 分钟内仍报 `running`。补 turn fact 会同时改变 v2 conversation/timeline 投影的
   输出面，属于 spec §6 冻结面的决策，不该塞进 bugfix。
2. **UUID 命令 id 的 smoke 用例未建**：脚本里没有"免模型即可挂起审批"的确定性路径
   （`e2e_seed` 的交互是已 resolved 的，第二客户端只会撞 first-answer-wins 被 400 挡下，
   根本进不到 worker），硬造会是假覆盖。该 lane 由上面 Rust 测试锁住。
3. **"TUI 面板不收敛"未复现于 daemon 侧**：本次证据显示审批确实落成了 fact 且回合续跑，
   回执只是没折叠。若面板仍不收敛，那是客户端读流的问题（v2 单流 / 本地状态），
   需另开单在 TUI 侧查。
4. 存量非绿：`qaqh-runtime --lib` 的 `agent::prompt::tests::prompt_and_tool_defs_char_budget`
   在本机失败（`backend_prompt.md` 10041 字符 vs 断言 128），该文件 HEAD 干净，与本修复无关。
