# Ringing v2 P0-3 / P0-4 汇总 Handoff（2026-09-24）

## 1. 当前基线

- 集成分支：`main`
- 当前 `main`：`0b55f0f1464868546a2ce8516020c319cd999afb`
- TUI 当前 anchor：`b77c2519f06f66c084bcb29d234e4a9c008777c5`
- TUI anchor tag：`tui-ringing-v2-interaction-causation-2026-09-24`
- TUI 侧验证：`cargo check --manifest-path /home/qaqtamsy/项目/qaqh-tui-app/Cargo.toml` PASS

本文件是 P0-1 到 P0-4 第一刀的 consolidated handoff。分阶段细节仍可查阅：

- `docs/handoff/2026-09-24-tui-ringing-v2-min-anchor-handoff.md`
- `docs/handoff/2026-09-24-ringing-v2-daemon-min-loop-handoff.md`
- `docs/handoff/2026-09-24-ringing-v2-interaction-causation-handoff.md`

## 2. 已合并 PR 与 tag

| 阶段 | PR | Tag | 指向 |
|---|---|---|---|
| P0-1/P0-2 v2 wire + client | #322 | `tui-ringing-v2-types-2026-09-24` | `a43a8bc` |
| P0-3 daemon 最小闭环 | #324 | `tui-ringing-v2-daemon-2026-09-24` | `d9a9abe` |
| TUI control shape 修复 | #325 | `tui-ringing-v2-daemon-2026-09-24-r2` | `289872e` |
| P0-4 interaction causation 第一刀 | #326 | `tui-ringing-v2-interaction-causation-2026-09-24` | `b77c251` |
| 原计划执行记录 | #327 | 无 | `0b55f0f` |

不可移动 tag 不要重打或 force-push。

## 3. 已完成能力

### 3.1 P0-1 / P0-2：wire 与 client

`qaqh-ringing` 新增独立 v2 面，v1 常量/类型未原地修改：

- `RINGING_V2_VERSION = 2`
- `RINGING_V2_BASE_PATH = "/ringing/v2"`
- `CanonicalCursor` / `CursorToken`
- `RingingV2EventEnvelope<P>`
- `RingingV2Bootstrap<C, V, T>`
- `RingingV2ResetRequired` / `RingingV2ResetReason`
- interaction / driver / command / open 类型

`qaqh-client` 提供：

```text
connect_v2_async
open_v2
renew_lease_v2
bootstrap_v2
subscribe_v2
send_command_v2
command_status_v2
claim_driver
release_driver
timeline_v2
service_v2
content_v2
```

TUI 只依赖 `qaqh-client` 即可命名 v2 类型；`ClientV2Event` 的 payload 是
canonical `ProjectionPayload`，不再需要 TUI 自己解析 wire JSON。

### 3.2 P0-3：daemon canonical 最小闭环

已实现：

```text
open
  -> bootstrap
  -> since_cursor subscribe
  -> canonical committed replay
  -> live
```

核心结构：

- `qaqh-session` 新增可选 `ProjectionSink`。
- `ToolLedger` 在 canonical append 成功后统一发布 projection events。
- `qaqh-runtime::ringing::V2ProjectionHub`：
  - per-session canonical `ProjectionSet`
  - bootstrap snapshot
  - committed `events.jsonl` replay
  - live broadcast
  - `ResetRequired`
- daemon v2 路由：
  - `POST /ringing/v2/clients/open`
  - `POST /ringing/v2/leases/renew`
  - `GET /ringing/v2/sessions/{seed}/bootstrap`
  - `GET /ringing/v2/sessions/{seed}/events/{channel}?since_cursor=...`
  - `POST /ringing/v2/commands/{channel}`
  - `GET /ringing/v2/commands/{command_id}`

daemon 路由测试覆盖真实 canonical 文件：

```text
open -> bootstrap -> append canonical fact -> since_cursor replay -> SSE frame
```

### 3.3 TUI control shape

TUI reducer 已经接入 anchor，要求：

- `ClientV2ControlState.interactions`
- `ClientV2ControlState.driver`
- pending interaction 含 `interaction_id` / `call_id` / `turn_id` / `kind`

backend 已对齐该形状；canonical `ControlInteractionState` 增加 `turn_id`，
daemon bootstrap 输出 TUI 可直接消费的 control state。

### 3.4 P0-4 第一刀：interaction causation

已完成：

- `InteractionResolved` canonical fact 支持 `causation_id = command_id`。
- v2 `send_command_v2` 默认生成 ULID command id。
- permission / ask / plan resolution 都透传 command id。
- 重复 ask / plan / permission 稳定返回 `interaction_already_resolved`。
- first-answer-wins 继续由 `ApprovalRegistry` + TurnActor suspended state 保证。
- `interaction_request_ledger` 断言 `causation_id == command_id`。

## 4. 验证证据

本地全量验证：

```text
cargo test --workspace -- --test-threads=1 PASS
cargo check --workspace --all-targets PASS
cargo clippy --workspace --all-targets -- -D warnings PASS
```

TUI 验证：

```text
cargo check --manifest-path /home/qaqtamsy/项目/qaqh-tui-app/Cargo.toml PASS
```

当前 anchor worktree：

```text
/home/qaqtamsy/项目/qaqh-backend-anchor
HEAD = b77c2519f06f66c084bcb29d234e4a9c008777c5
```

CNB 流水线仍可能在 Prepare 阶段因根组织 CPU 配额不足失败；这不是代码阶段失败。
最近几次 PR 均以本地全量门禁为准合并。

## 5. 仍未完成

### P0-4 剩余

- v2 command ack 携带 typed existing-result payload；当前只有稳定 code/message。
- permission request 全面进入 canonical `InteractionRequested` 统一路径。
- amend rule。
- sandbox denial escalation。
- 完整 V2-R1..R4 Gate matrix。

### P0-5

- driver claim/release。
- `DriverChanged` reliable event。
- `driver_epoch` 单调递增。
- `stale_driver_epoch` / `not_driver` / `driver_busy`。
- 非 driver 的 composer / cancel / undo / workspace 只读。

### P0-6

- open / bootstrap golden fixture。
- reliable / replaceable / ephemeral transcript。
- ResetRequired 各 reason。
- interaction reconnect + concurrent answers。
- driver claim / busy / handover fixture。
- v1 cursor 映射 fixture。

### P1

- `/ringing/v2/service/{method}`。
- `/ringing/v2/content`。
- timeline v2 完整分页与重连。
- v1 `Last-Event-ID` -> v2 cursor 服务端映射。
- Windows alpha 共用 fixture。

## 6. 下一步顺序

1. 完成 P0-4：typed existing result、permission canonical registry、V2-R1..R4。
2. 做 P0-5 driver capability，并补 `DriverChanged` canonical/reliable 语义。
3. 做 P0-6 fixture 与故障钩子。
4. 再做 P1 service/content/timeline 与 v1 映射。
5. 最后处理 P6 清理和 composition root。

## 7. 接手注意

- 不要移动已有 v2 tag。
- 不要把 v1 `stream_seq` 当 canonical cursor。
- 不要让 bootstrap 与 event replay 使用不同事实源。
- 不要绕过 `qaqh-client` 给 TUI 暴露 `qaqh-ringing` / `qaqh-session` 内部类型。
- 新 driver/interaction 改动必须同时覆盖 bootstrap、live event 和 reconnect。
