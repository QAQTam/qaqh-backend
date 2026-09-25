# Subagent V2 Team Projection Handoff

> 日期：2026-09-25
> 基线：`0349f7c`（`main`）
> 状态：Team projection 后端核心完成
> 范围：`qaqh-session`

## 1. 本次结论

Phase 4 的前端数据源已落地：

```text
canonical facts
  -> TeamProjection
  -> TeamSnapshot / TeamDelta
  -> ProjectionSlot::Team
```

`TeamSnapshot` 包含 root session、roster、inbox 和 revision；roster 保留
unloaded agent，不以 runtime handle 是否存在作为逻辑身份依据。

## 2. 已落地

### 2.1 Slot 与 wire 类型

- `ProjectionSlot::Team = 6`；
- `TeamAgentStatus` / `TeamAgentResidency`；
- `TeamAgentSnapshot`、`TeamInboxSummary`；
- `TeamDelta`：
  - `AgentJoined`
  - `AgentStatusChanged`
  - `AgentResidencyChanged`
  - `AgentMessageQueued`
  - `AgentMessageDelivered`
  - `AgentInterrupted`
  - `AgentCompleted`

### 2.2 Reducer

- `SessionCreated` 建立 `/root` roster entry；
- `SubagentSpawned` 建立 child entry，保留 parent path、role、AgentPath；
- `TurnStarted` 更新 running/current turn；
- `TurnInterrupted` 更新 interrupted；
- `SubagentFinished` 收敛 completed/errored/interrupted 与 unloaded；
- `InterAgentCommunication` 进入 inbox；
- 匹配 `InputAccepted.client_request_id` 后移出 inbox 并产生 delivered delta。

### 2.3 ProjectionSet

- `ProjectionSetSnapshot` 新增 `team`，旧 snapshot 通过 serde default 兼容；
- `ProjectionSet::apply` 输出 Team delta；
- replay stream 将 Team 归入 Control channel。

## 3. 验收证据

```text
cargo test -p qaqh-session --test team_projection --test projection_slots --offline -- --test-threads=1
cargo check --workspace --all-targets --offline
```

覆盖：

- roster 从 SessionCreated/SubagentSpawned 重建；
- path、parent path、role 保留；
- status/running/interrupted/completed 与 residency/unloaded 收敛；
- inbox queued/delivered；
- ProjectionSet 暴露 Team slot 与 snapshot。

## 4. 未决项

- TUI/WinUI 尚未消费 TeamSnapshot/TeamDelta；
- `TeamDelta::AgentResidencyChanged` 当前作为类型契约保留，运行时 unload 尚无独立 canonical fact；
- task board / message board 尚未开始；
- steer/interject 与大正文 content_ref 外置仍未完成。

## 5. 接手注意事项

- frontend roster 不得从 `spawn_subagent` 工具卡 JSON 推导身份；
- unloaded 不能显示成 deleted；
- `SubagentSpawned` 应先于工具卡可见；
- `SubagentFinished` 必须能独立收敛状态，不能依赖 timeline。
