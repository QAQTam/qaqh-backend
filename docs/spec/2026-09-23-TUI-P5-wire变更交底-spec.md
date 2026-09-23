# TUI P5 wire 变更交底（规划稿，2026-09-23）

> 状态：**已被 [`2026-09-23-TUI-Ringing-v2冻结语义-spec.md`](2026-09-23-TUI-Ringing-v2冻结语义-spec.md)
> 取代**。本文保留为排期背景，不再作为 wire 契约来源。正式语义以冻结 spec 和
> tag `tui-ringing-v2-frozen-2026-09-23` 为准。

## 1. P5 的目标

P5 不是 additive 小改，而是 Ringing 重连语义的代际切换：

- v1 的权威游标是 `Last-Event-ID` / 三频道 journal；
- v2 的权威游标是 canonical fact log 投影出的 `since_cursor`；
- 三频道仍是 wire 过滤视图，不删除；
- replaceable 事件不推进 cursor；
- 重连后先恢复权威快照，再原子订阅 live，避免 gap/dup；
- 未决 permission / ask / plan 交互支持重放。

来源：`docs/plan/2026-09-20-qaqh-v2.0-总架构设计-plan.md` §11 P5、
`docs/plan/2026-09-19-qaqh-v2.0-前瞻设计-plan.md` §2.4。

## 2. 已知变更面

| 面 | v1 现状 | P5 方向 | TUI 影响 |
|---|---|---|---|
| 订阅端点 | v1 SSE + `Last-Event-ID` | 新增 Ringing v2 端点 + `since_cursor` | client 需要 capability/版本分支 |
| 游标 | channel `stream_seq` / timeline seq | canonical projection cursor | reducer 不能继续把旧 seq 当权威 |
| replaceable | 与 reliable 共享流 | replaceable 不推进 cursor | 重连后可能重放覆盖态，需要幂等 |
| reset | `ringing.reset_required` | 语义保留并进入 v2 client API | 重置/rebuild 路径必须统一 |
| 未决交互 | 主要依赖当前连接 | pending interaction replay | modal 必须 first-answer-wins |
| timeline wire | `Ringing_timeline_intent_v1` | 明确保持兼容 | 不应因 P5 重写 timeline 渲染器 |
| service response | legacy `Value` 与 typed 混合 | typed response，旧 Value 只做 adapter | TUI 不应继续手解 JSON |
| 存储 | journal/checkpoint 目录 | canonical log + 可重建 projection | TUI 不得引用任何存储路径 |

## 3. 兼容策略

架构稿的既定原则：

- Ringing core：v2 并行端点，v1 提供**一个发布周期**映射层；
- additive/Unknown 兼容覆盖 wire、tool display、resource/state event、service
  response；
- canonical fact 不做跨版本 additive 读取；未知 kind/payload version 必须
  fail-closed 到 read-only/upgrade-required；
- 旧字段保留期由兼容矩阵明确，不以“以后可能需要”为由永久双写。

因此 TUI 的排期建议：

1. v2 alpha 尽量在 P5 cutover 前收口；
2. v2 alpha 的 parity matrix 固定 backend rev；
3. P5 开工时另建一套 v2 wire fixture/reducer 验证，不把旧协议矩阵当新协议
   证据；
4. 兼容期结束时，TUI 应能删除 v1 cursor 映射，而不是长期维护双 reducer。

## 4. Cutover 前必须冻结的 wire 细节

以下内容本文**不能替 P5 spec 决定**：

- v2 端点路径与版本协商字段；
- `since_cursor` 的编码、类型、reset 语义；
- v1 `Last-Event-ID` → v2 cursor 的映射表；
- `ResetRequired` 在 v2 的 payload 与错误码；
- pending interaction replay 的批次边界和去重键；
- driver capability 的字段与协商方式；
- service typed response 的 schema 与 legacy adapter 退出时间；
- 兼容窗口从哪个 release 开始、到哪个 release 结束。

## 5. TUI 侧验收预留

P5 开工后，TUI 至少需要新增：

- v2 cursor 单调性、reset、gap/dup 测试；
- replaceable 重放不推进游标的回归；
- pending permission/ask/plan 断线重连重放；
- interaction first-answer-wins；
- v1/v2 fixture 各跑一次 parity matrix；
- 静态检查：TUI 不引用 journal/checkpoint/offload/messages 路径。

## 6. 当前排期结论

- P5 时间点：**未冻结**；
- 兼容窗口：**原则是一个发布周期，具体版本待定**；
- TUI 当前不需要为 P5 提前改 wire，但应避免把 v1 cursor 语义写进新的
  SessionModel 核心；
- P5 spec 冻结后，本文应升级为正式交底并附完整字段/端点表。
