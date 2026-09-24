# 已冻结决策

> 日期：2026-09-25
> 基线：`2.0.0-alpha2`
> 状态：accepted

以下决策是当前实现与后续 debug 的前提；不得在没有新裁决文档的情况下回退。

## D1. 纯 v2，不做 v1 兼容

- `/ringing/v1/*` 路由已删除。
- v1 三频道全局流已删除。
- `Last-Event-ID → v2 cursor` 映射已作废。
- 仍存在的部分 v1 类型/命名只服务内部历史兼容或测试，不得重新暴露为 wire 契约。

## D2. Canonical facts 是事实源

- session 事实以 `events.jsonl` + commit marker 为准。
- timeline、conversation、control、resource 等 projection 都是派生数据。
- 不允许新增第二份可写事实源来绕过 canonical log。

## D3. messages.jsonl 是消息归档

- 消息归档 append-only。
- 工具结果、assistant、user、trailing 注入都按写入序持久化。
- 读取端按 `msg_id` 重建顺序，不按 role 重排协议关键消息。

## D4. compact 由归档水位推导

- 摘要作为普通消息 append 到 `messages.jsonl`。
- `meta.compact_covered_through_msg_id` 是覆盖水位。
- 活跃视图 = system + 最新摘要 + 水位后的非摘要消息。
- `compact-context.json` 和旧 checkpoint effect 不再存在。
- undo / 图片修复的全量重写必须保留或显式清除 compact watermark。

## D5. Ringing v2 单流

- 客户端只订阅单一 v2 流。
- bootstrap 提供 typed 三频道快照。
- reliable replay 与 replaceable current-value mirror 分离：
  - reliable 推进 cursor；
  - replaceable 不带 cursor，只提供当前值。

## D6. Interaction first-answer-wins

- ask / plan / permission 的有效回答只有一个。
- 重复回答返回稳定 `interaction_already_resolved` 和既有结果。
- 不产生第二次工具执行或第二个终态。

## D7. Driver seat 是 canonical 事实

- `DriverChanged` 进入 canonical projection。
- daemon lease 决定 holder 存活。
- 已认领的 seat 对写入命令生效；未认领时保持兼容放行。
- 过期回收、显式 release、daemon 启动轮转均以 canonical 事实收敛。

## D8. 工具终态结构化

- `ToolResult` 状态是终态唯一真相。
- 展示层使用结构化 `outcome`。
- exec stdout/stderr 分离展示，不伪装交织顺序。
- 不从 summary 文本反推执行结果。

## D9. Gate 边界

- `qaqh-gate` 只负责 provider HTTP/协议/流式事件边界。
- agent loop、tool execution、permission、timeline、canonical persistence 不属于 gate。
- 未来接入 `mutilAI-SDK` 必须先经 bridge 和 parity 验收，不能直接替换 runtime 契约。

## D10. 文档权威

- `docs/current/` 是唯一当前权威文档区。
- `docs/archive/` 只用于历史追溯。
- 旧文档中的“未完成/待办/计划”不能直接当作当前状态。
