# Steer / Interject Safe-Point Delivery

> 日期：2026-09-26
> 基线：`a154242`（`main`）
> 状态：accepted；SUBV2-11 实施中
> 上游：`2026-09-25-subagent-v2-rewrite-spec.md` Phase 7 / SUBV2-11
> 范围：`qaqh-domain`、`qaqh-session`、`qaqh-runtime`、`qaqh-subagent`

## 1. 定位

Steer / Interject 是 mailbox delivery 的第二阶段语义。它们不替代
`queue` / `trigger` / `interrupt`，也不引入 actor handle 直连。

```text
queue      = 普通下一回合/下一 lap 消息
steer      = 当前 turn 下一 safe point 优先合并的指导
interject  = 当前 turn 下一 safe point 最高优先级合并的纠正
trigger    = idle 时启动 turn
interrupt  = 显式中止当前 turn
```

冻结决策：

- `interrupt` 仍是唯一会中止当前 turn 的 delivery；
- `steer` / `interject` 不取消 turn、不中止正在执行的工具批；
- `steer` / `interject` 只在 safe point 合并到当前 turn；
- idle 时 `steer` / `interject` 只进 mailbox，不启动 turn；
- 所有 delivery 仍先写 canonical `InterAgentCommunication`，再写
  `InputAccepted`；
- 正文接受不等于模型已读。

## 2. Safe point

V1 的 safe point 定义为：

```text
一次工具批已经完成并已写回 tool results
且 Loop 尚未进入下一次模型请求
```

对应现有 `Outcome::ContinueTurn` 边界。约束：

- 不在 assistant(tool_call) 与其 tool_result 之间插入消息；
- 不在正在执行的工具中途插入消息；
- 不在 turn 已产生可见 final answer 后继续合并；此时延期到下一 turn；
- safe point 合并后，消息必须在下一次模型请求的 context 中可见；
- 一个 safe point 最多合并有限条 steer / interject，剩余记录留到下一
  safe point，避免单次 context 被洪水淹没。

## 3. Delivery 扩展

```rust
InterAgentDelivery {
  Queue,
  Trigger,
  Interrupt,
  Steer,
  Interject,
}
```

| Delivery | idle | running | 是否取消 turn | 优先级 |
|---|---|---|---|---|
| `queue` | 只入 mailbox | 下一 safe point | 否 | normal |
| `steer` | 只入 mailbox | 下一 safe point | 否 | steer |
| `interject` | 只入 mailbox | 下一 safe point | 否 | interject |
| `trigger` | 启动 turn | 由现有 mailbox 规则处理 | 否 | normal |
| `interrupt` | 保留身份 | 中止当前 turn | 是 | interrupt |

同一 safe point 内排序：

```text
interject -> steer -> queue
```

每个 priority 内保持 FIFO。

## 4. Canonical purpose

`ConversationInputPurpose` 与 canonical `InputPurpose` 扩展为：

```text
trigger_turn
queue_only
steer
interject
```

映射规则：

| delivery | domain input purpose | canonical InputPurpose |
|---|---|---|
| `queue` | `queue_only` | `queue_only` |
| `steer` | `steer` | `steer` |
| `interject` | `interject` | `interject` |
| `trigger` | 调用方显式 purpose | 对应 purpose |
| `interrupt` | 不产生新 turn 的输入 | 由 interrupt 路径处理 |

## 5. 工具面

新增：

| 工具 | delivery |
|---|---|
| `steer_agent` | `steer` |
| `interject_agent` | `interject` |

工具参数与 `send_message` 相同：

```json
{
  "to": "/root/<agent>",
  "message": "..."
}
```

不接受正文 regex 解析 `@`；target 必须是结构化 AgentPath。

## 6. Root 安全

- 非 root agent 不得向 root 发送 `steer` 或 `interject`；
- root 可以 steer/interject 同一 tree 内的 child；
- child 可以在同一 tree 内 steer/interject peer 或 descendant；
- `interrupt_agent` 仍拒绝 root 和 self；
- steer/interject 不绕过现有 root-tree ownership、depth 和 outbound 配额。

## 7. 配额与防互喷

除现有 `messageInFlightPerPair` 和 `messageOutboundPerSender` 外，V1 增加
safe-point 单 lap 上限：

| 类型 | 单 safe point 上限 |
|---|---:|
| `steer` | 8 |
| `interject` | 4 |

规则：

- 超出的记录留在 InjectionBus，下一 safe point 继续；
- 不静默丢弃，不覆盖旧消息；
- 同一 priority 保持 FIFO；
- 单条正文上限继续由 canonical content 规则约束；
- `interject` 不得被滥用为 cancel；需要中止 turn 时必须使用
  `interrupt_agent`。

## 8. 验收矩阵

| ID | 场景 | 必须成立 |
|---|---|---|
| SA2-S1 | steer running | 下一 safe point 合并，不取消 turn |
| SA2-S2 | interject running | 同一 safe point 排在 steer/queue 前，不取消 turn |
| SA2-S3 | idle | steer/interject 只入 mailbox，不启动 turn |
| SA2-S4 | ordering | interject -> steer -> queue，priority 内 FIFO |
| SA2-S5 | safe point | 不在 tool_call/tool_result 之间插入 |
| SA2-S6 | quota | 单 lap 超过上限的记录保留到下一 lap |
| SA2-S7 | root safety | child 向 root steer/interject 稳定拒绝 |
| SA2-S8 | canonical | 每个 delivery 都写 `InterAgentCommunication` + `InputAccepted` |

## 9. 实施切片

1. SUBV2-11a：spec + domain/session delivery 与 purpose 扩展；
2. SUBV2-11b：InjectionBus 优先级、safe-point 排序与单 lap 配额；
3. SUBV2-11c：`steer_agent` / `interject_agent` runtime 工具与 host 安全门；
4. SUBV2-11d：TUI/WinUI 显式展示 steer/interject 与 interrupt 的区别。
