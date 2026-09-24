# TUI Ringing v2 alpha 修订 spec（2026-09-24）

> 本文只记录对已冻结 v2 契约的 alpha 修订，不追改日期快照
> `2026-09-23-TUI-Ringing-v2冻结语义-spec.md`。实现与壳层以本文为准。

## 1. interaction kind 统一为 `plan`

修订前：

| 路径 | wire `kind` |
|---|---|
| bootstrap `control.state.interactions[].kind` | `plan_review` |
| SSE `ControlDelta::InteractionRequested.kind` | `plan` |

修订后两条路径统一为：

```text
ask | plan | permission
```

Rust 侧 `RingingV2InteractionKind::PlanReview` 变体名暂保留，避免无意义的
source-breaking rename；其 serde 值固定为 `plan`。壳层按 wire 值匹配。

## 2. permission 正文也走 canonical content ref

修订前 permission 的详情只存在于 v1 tool 频道快照 / timeline 卡；纯 v2 单流下
没有 tool 频道快照，授权面板会退化成占位。

修订后：

- `RingingV2PendingInteraction.request` 对 permission 也携带
  `ContentValue::Ref`；
- 正文由 `qaqh_domain::interaction_body::permission_body` 单点构造，字段为
  `tool_name` / `action_summary` / `reason` / `paths` / `category` / `level` /
  `risk` / `consequence`；
- 客户端按 canonical `content_ref` 调
  `GET /ringing/v2/content/{content_ref}`，不再尝试关联 timeline 工具卡的
  wire call id；
- 正文 404 时仍保留可答复的 `interaction_id` / `call_id`，面板按详情不可用降级。

## 3. approvals 的 ask/plan 增加 `details`

`GET /ringing/v2/sessions/{seed}/approvals` 的 `pending_interaction` 保持
`id` / `kind` 兼容字段，并新增：

```json
{
  "id": "int_...",
  "kind": "ask | plan",
  "details": { "kind": "ask", "questions": [] }
}
```

正文取不到时 `details = null`。浏览器网关继续负责把 details 放进 opaque approval
challenge；daemon 不暴露 challenge id。

## 4. content GET 支持 RFC 9110 单区间 Range

`GET /ringing/v2/content/{content_id}` 新增：

- `Accept-Ranges: bytes`；
- 合法单区间返回 `206 Partial Content` + `Content-Range`；
- `bytes=start-`、`bytes=start-end`、`bytes=-suffix` 均支持；
- 多区间、语法错误、越界返回 `416 Range Not Satisfiable` +
  `Content-Range: bytes */{total}`；
- 不带 `Range` 仍是 `200` 全量。

`qaqh-client` 新增 `content_v2_range(content_id, range)`；原
`content_v2(content_id)` 等价于不带 Range。

## 5. `content_quota_exceeded` 的定位

保留在 `RingingV2ResetReason` / session-fact `ResetReason` 中，语义限定为
**会话级内容总量 hard watermark**。

交互正文 pinned 准入配额（每 seed 64 条 / 4 MiB）超限时：

- 不写正文、不静默淘汰；
- 客户端对 `request` ref 取到 404，按正文不可用降级；
- 不作为流 `ResetRequired.reason` 发送，因为 reset 不能解决写路径配额。

## 6. `session.new` 即物化 canonical 基线

`session.new` 在 worker spawn 前写入：

```text
canonical-identity.json
events.jsonl (SessionCreated)
events.commit.json
```

因此 bootstrap / events 从 session 创建后即可用，不再把
`snapshot_missing` 当作新建会话的正常瞬态。首个 canonical fact 固定为
`SessionCreated`；后续工具 ledger 复用同一 identity/log。

## 7. v2 command fingerprint 包含 `driver_epoch`

`driver_epoch` 是 v2 command envelope 的 CAS 输入。同一 `command_id` 在不同
epoch 下提交属于不同 payload，不得重放旧 ACK；v1 面没有该字段，指纹仍以
`driver_epoch = null` 计算。

## 8. 仍未决（不在本次 alpha 修订）

- V2-C3 replaceable 生产 producer；
- interaction 正文跨 daemon 重启持久化（与 pending interaction 跨重启存活绑定）；
- permission 正文 pinned 与终结 unpin（需要一条稳定的权限终结域事件）；
- driver 回收延迟 / `not_eligible` 优先级 / workspace command gate 集合；
- 崩溃路径 writer fence 轮转。
