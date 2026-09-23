# Ringing v2 daemon 最小闭环 Handoff（2026-09-24）

状态：**P0-3 第一刀已实现**，待 PR 合并与新 tag 固化。  
目标 tag：`tui-ringing-v2-daemon-2026-09-24`。

## 1. 本次闭环

已实现：

```text
open
  -> bootstrap
  -> since_cursor subscribe
  -> canonical committed replay
  -> live
```

端点：

```text
POST /ringing/v2/clients/open
POST /ringing/v2/leases/renew
GET  /ringing/v2/sessions/{seed}/bootstrap
GET  /ringing/v2/sessions/{seed}/events/{channel}?since_cursor=...
POST /ringing/v2/commands/{channel}
GET  /ringing/v2/commands/{command_id}
```

命令面先复用 v1 command dispatcher；v2 envelope 在 daemon 入口转换为内部
v1 envelope，后续 P0-4 再切 canonical interaction 事实。

## 2. 核心实现

### 2.1 canonical projection sink

`qaqh-session` 新增可选 `ProjectionSink`：

- `ToolLedger` durable append 成功后发布 `ProjectionEvent`；
- 发布失败不影响 canonical commit；
- 无 sink 时为零开销 no-op；
- 覆盖 tool intent/finished、interaction requested/resolved/expired、recovery
  marker 等全部 canonical append 路径。

`ProjectionEvent` 新增：

- `causation_id`，从 canonical `SessionFact` 透传；
- `ProjectionPayload::revision()`，供 v2 envelope 填 `revision`。

### 2.2 `V2ProjectionHub`

`qaqh-runtime::ringing::V2ProjectionHub`：

- 每 canonical session 一份 `ProjectionSet`；
- 在同一个 per-session 锁内完成 snapshot clone + live subscribe；
- replay 从 `events.jsonl` committed prefix 生成；
- reliable cursor 为 `(log_id, fact_seq, projection_index)`；
- `ResetRequired` 覆盖 `log_id_mismatch`、`unknown_fact`、`replay_overflow`、
  `per_connection_overflow`。

### 2.3 bootstrap 类型调整

`RingingV2Bootstrap<C, V, T>` 改为泛型 wire 外壳。`qaqh-client` 绑定：

- `ClientV2ControlState`：canonical `ControlSnapshot` + `driver`
- `ClientV2ConversationState`：canonical `ConversationSnapshot`
- `ClientV2ToolState`：canonical tools

这保证 bootstrap 与 v2 event replay 同源于 canonical projection，不再把
v1 `ControlState` 伪装成 canonical state。

## 3. 验证

```text
cargo test -p qaqh-runtime --lib ringing::v2 -- --test-threads=1 PASS
cargo test -p qaqh-daemon --bin qaqh-daemon -- --test-threads=1 PASS
cargo test -p qaqh-client --all-targets -- --test-threads=1 PASS
cargo clippy -p qaqh-session -p qaqh-runtime -p qaqh-daemon -p qaqh-ringing -p qaqh-client --all-targets -- -D warnings PASS
```

daemon 路由测试覆盖：

```text
POST /ringing/v2/clients/open
GET  /ringing/v2/sessions/{seed}/bootstrap
```

并断言 bootstrap 来自 committed canonical projection。

## 4. 仍未完成

- interaction command 的 canonical `causation_id = command_id` 闭环；
- canonical interaction registry 与 bootstrap pending set 的 P0-4 接线；
- driver claim/release 与 `DriverChanged`；
- v2 timeline/service/content 端点；
- v1 `Last-Event-ID` -> v2 cursor 映射；
- v2 fixture 与 V2-C/R/D/V1/T/W 验收矩阵；
- TUI SessionModel reducer 接线。

## 5. 下一步

1. P0-4：interaction canonical registry、pending replay、first-answer-wins；
2. P0-5：driver capability；
3. P0-6：fixture 与故障钩子；
4. P1：timeline/service/content 完整面与 v1 映射。
