# Ringing v2 单流硬切 Handoff（2026-09-24）

状态：**后端 + 客户端已落地（v1 三频道 SSE 路由删除，客户端每 seed 一条 v2 单流）；
TUI 侧改动在 `qaqh-tui-app` 分支 `feat/v2-bootstrap-cutover`（需一次 rev bump）。**

## 1. 这次修的是什么

纯 v2 化最大的一块：`GET /ringing/v1/events/{channel}`（三条全局频道流）→
`GET /ringing/v2/sessions/{seed}/events`（每 seed 一条 canonical 单流）。删掉 v1 路由后，
`/ringing/v1/*` 只剩 approvals 一条。

## 2. 复核前提：三个「想当然」都不成立

按纪律先验证前提，发现三个必须先修的真问题：

1. **「无 cursor 订阅」会静默丢事实。** 服务端把 `since_cursor=None` 解释成
   「只看 live」（`since_fact_seq = last_fact_seq`），snapshot 与订阅之间的所有事实
   被跳过。客户端必须**先 bootstrap 拿 snapshot cursor 再订阅**（规范顺序
   `open -> bootstrap -> subscribe -> replay -> live`）。
2. **新建会话的 canonical 目录是懒创建的。** `session_create` 之后、首个 canonical
   事实落盘之前，`/ringing/v2/sessions/{seed}/{bootstrap,events}` 返回 **404/409**
   （`resolve_identity` 要求 identity + commit marker 都在）。这是**正常瞬态**，
   不能抬成流告警，否则新建会话永远显示 degraded。
3. **快照本身不进流。** 流只从 snapshot cursor 起放 replay；「流建立前写入的交互请求」
   （例如新建会话第一回合里立刻出现的 exec 授权）只能靠**流建立后再 bootstrap**
   补齐——否则权限 modal 永不出现，工具永远挂起。

第 3 条是实测挖出来的：删掉 v1 流后 `e2e-alpha1-basic` 稳定挂在 exec（
`sealing orphan tool call_alpha1_3 (no ToolFinished)`）。

## 3. 另一个真缺口：permission 答复的 id 对不上

v2 投影只暴露 **canonical** `call_id`（`call_<ULID>`，由 wire id 哈希而来），
而运行时的挂起表（`ApprovalRegistry` / `saved.pending_permission_ids`）按 **wire**
`call_id` 记账。壳层拿 v2 的 id 去答复 → `unknown permission response` → 工具挂起。

修法：运行时**同时接受两种形态**（`permission_id_matches`：wire id 或它的 canonical
形式），壳层因此可以只用 v2 的 id 答复。canonical 由 `ulid_from_text` 单向哈希得到，
不可反推，故只能在运行时侧归一。

## 4. 改动

| 面 | 改动 |
|---|---|
| daemon 路由 | 删 `GET /ringing/v1/events/{channel}`（404）；v2 单流不变 |
| daemon sse.rs | 删 `handle_events` + `ShardedChannelStream` + `SubscriptionLease` + v1 专用 helper/测试（−851 行） |
| daemon test_hooks | 删 channel 作用域的终止注入（只留 timeline） |
| qaqh-client | 新增 `v2_stream.rs`（每 seed 常驻读循环：bootstrap→subscribe→replay→live、cursor、reset、退避）；`ClientHandlers` 的 `on_batch`/`on_status`/`on_reset` → `on_v2_event`/`on_v2_reset`/`on_v2_status`；`activate_timeline` 起流、`deactivate_timeline` 停流；`connect_async` 不再起三条全局流 |
| qaqh-client error | `is_session_not_ready()`（404/409/`session_not_found`/`snapshot_missing`） |
| qaqh-runtime | permission 答复接受 canonical id（`permission_id_matches` + `ApprovalRegistry::resolve_key`） |
| TUI（未提交） | `RuntimeMsg::V2Event`；`handle_v2_event` + control/conversation/meta/resource 四个 delta 分派；删 v1 `handle_control`/`handle_conversation`/`handle_tool`；v2 流 Open → 重新 bootstrap；ask/plan 正文从 content store 取回后按 `interaction_body` 构造面板 |

### TUI 装配要点

- **timeline 家族**仍由独立的 per-seed timeline 流承载（transcript 权威），v2 单流里
  的 `TimelineDelta` 在 app 层忽略，避免双写。
- **permission 面板**详情来自 timeline 上同一 `call_id` 的工具卡（v2 交互正文不含
  permission 详情）；`call_id` 是 canonical 形态，与工具卡的 wire id 不直接相等，
  故当前可能退化成「（恢复中）」占位——**待办**：把 wire id 也带进 v2 交互。
- **dashboard / skills** 没有 v2 增量：dashboard 走 `session.dashboard` RPC（本来就
  有兜底），`skills` 字段是死状态，已删。

## 5. 验证

```text
cargo test --workspace -- --test-threads=1              PASS（0 failed）
cargo clippy --workspace --all-targets -- -D warnings   PASS
cargo fmt --all -- --check                              PASS
scripts/v2-smoke.sh <data-root>                         PASS（含 v1 events/{channel} 404）
scripts/v2-content-probe.sh <data-root>                 PASS
```

**真机 e2e（TUI 用本 rev 重建 + 本仓 daemon）**：

| e2e | 结果 |
|---|---|
| `e2e-alpha1-basic.sh`（write/read/edit/exec + 权限批准） | PASS（4/4） |
| `e2e-new-session.sh` | PASS（4/4） |
| `e2e-history.sh` | PASS（6/6） |
| TUI `cargo test` | PASS（399 passed） |

## 6. 剩余 v1 面

| 端点 | 现状 |
|---|---|
| `GET /ringing/v1/sessions/{seed}/approvals` | 只读审批查询，v2 无对应端点（需设计：并入 bootstrap 或新端点） |

## 7. alpha 未决清单（本次新增/更新）

1. **v2 交互缺 wire call id**：permission 面板详情无法与 timeline 工具卡精确关联
   （现在靠 canonical↔wire 的运行时归一兜底，UI 侧可能显示占位）。建议
   `ControlDelta::InteractionRequested` 增一个 `wire_call_id`。
2. **TUI v2 e2e harness 仍打 v1**：`e2e-v2-interactions.sh` / `e2e-v2-alacritty.sh` /
   `e2e-v2-faults.sh` / `e2e-v2-real-terminal.sh` / `e2e-v2-tmux.sh` /
   `e2e-v2-wezterm.sh` / `e2e-lease-expiry.sh` 直接打 `/ringing/v1/clients/open`，
   v1 硬切后 404，必须同批改成 v2 open + `session_attach`。
3. **新建会话的 canonical 目录懒创建**：客户端已按瞬态处理（`Connecting` 不抬告警 +
   短退避），但更干净的做法是 `session_create` 即物化 identity/commit。
4. **TUI bootstrap 重试窗口 60s**：新建会话若长期不发消息，60s 后会弹一次
   「bootstrap 失败」toast（会话此时仍未物化，属预期）。
5. `ClientHandlers` 是破坏性改名（`on_batch`→`on_v2_event` 等）：本仓 examples/tests 已
   同步，其它壳层需同批。

## 8. 仍是 v1 的面（承 #351）

`GET /ringing/v1/sessions/{seed}/approvals` 一条；`RingingSessionBootstrap` 类型仍留
（v1 hub 的产出方往返测试还在用），approvals 迁完可一并删。
