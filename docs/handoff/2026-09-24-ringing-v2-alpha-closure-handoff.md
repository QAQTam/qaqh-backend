# Ringing v2 alpha 未决收口 Handoff（2026-09-24）

状态：**已落地，工作区全量测试 + clippy + fmt + 真实 daemon smoke + ask/permission
content probe 全绿。** 契约修订见
`docs/spec/2026-09-24-TUI-Ringing-v2-alpha修订-spec.md`。

## 1. 本次收口范围

本轮只处理 alpha 未决里已有明确裁决、且不需要新架构的项：

1. bootstrap 与 SSE delta 的 interaction kind 统一为 `plan`；
2. permission 正文也进入 canonical content ref，bootstrap 不再返回 `None`；
3. approvals 的 `pending_interaction` 增加 `details`；
4. v2 content GET 支持 RFC 9110 单区间 Range；
5. `session.new` 即物化 canonical `SessionCreated` / commit marker；
6. v2 command fingerprint 纳入 `driver_epoch`；
7. `content_quota_exceeded` 的双重语义写入修订 spec。

## 2. 具体改动

### 2.1 interaction kind 统一

`RingingV2InteractionKind::PlanReview` 的 Rust 变体名保留（避免 TUI 无意义
source break），但 serde wire 值改为 `plan`：

```text
ask | plan | permission
```

`qaqh-ringing`、`qaqh-client` 的序列化断言、daemon reconnect 用例、TUI 公共 API
注释均已更新。

### 2.2 permission 正文

- daemon bootstrap 对 permission 也返回 `request: ContentValue::Ref`；
- `qaqh_domain::interaction_body::permission_body` 继续作为唯一序列化点；
- 客户端不再需要把 canonical `call_id` 与 timeline 工具卡 wire id 对齐；
- 正文 404 时仍保留 interaction id/call id，按详情不可用降级。

TUI 侧要跟读：`restore_pending_interaction` 的 `K::Permission` 分支应像 ask/plan
一样消费 `interaction.request` 并下载正文；旧 timeline 工具卡兜底可以移除。

### 2.3 approvals details

`GET /ringing/v2/sessions/{seed}/approvals` 的 `pending_interaction` 现在为：

```json
{
  "id": "int_...",
  "kind": "ask | plan",
  "details": { "kind": "ask", "questions": [] }
}
```

正文取不到时 `details = null`。webui gateway 已有 `details` 透传逻辑，不需要改
challenge 形状。

### 2.4 content Range

`GET /ringing/v2/content/{content_id}`：

- 无 Range：`200` 全量；
- `bytes=start-end` / `bytes=start-` / `bytes=-suffix`：`206` + `Content-Range`；
- 多区间 / 语法错误 / 越界：`416` + `Content-Range: bytes */{total}`；
- 始终返回 `Accept-Ranges: bytes`。

`qaqh-client` 新增 `content_v2_range(content_id, range)`；`content_v2` 保持原签名。

### 2.5 `session.new` canonical 物化

`QaqhService::handle("session.new")` 在 worker spawn 前：

1. 建 canonical identity；
2. 初始化空 commit marker；
3. 追加首个 `SessionCreated`；
4. release writer lease；
5. 再 spawn worker。

因此新建会话从创建那一刻起 bootstrap/events 就可用，不再依赖首个工具事实。
`crates/qaqh-session/examples/e2e_seed.rs` 同步改成「已有 SessionCreated 则跳过」，
并使用当前时间戳，避免 smoke seeder 与 daemon 抢 writer lease。

### 2.6 driver_epoch fingerprint

v2 command fingerprint 现在包含 `envelope.driver_epoch`。同一 `command_id` 在不同
driver epoch 下提交会被判为不同 payload，不会重放旧 ACK；v1 面固定传 `None`。

## 3. 验证

```text
cargo test --workspace -- --test-threads=1              PASS（0 failed）
cargo clippy --workspace --all-targets -- -D warnings    PASS
cargo fmt --all -- --check                               PASS
QAQH_SMOKE_LEASE_TTL_MS=30000 scripts/v2-smoke.sh ...    PASS
scripts/v2-content-probe.sh ...                          PASS（ask）
QAQH_CONTENT_PROBE_MODE=permission scripts/v2-content-probe.sh ...
                                                         PASS（permission）
```

permission probe 的真实输出：

```text
kind=permission
request.ref=sha256:b770072767e4fd6eff8274e58ecc8bbf092c6623fa2e8feb74af3b6959b0b8ec
body={"kind":"permission","tool_name":"exec",
      "action_summary":"command: \"echo permission-probe\"",
      "reason":"Level 2: 'exec' (write/exec/net) requires confirmation.",
      "category":"exec","level":2,"risk":"high",...}
第二个 client session 重连 ref_match=True status=200
```

## 4. 仍未决（下一阶段）

这些不是本轮“已既定”项，已从 alpha 收口范围显式排除：

1. **V2-C3 replaceable 无生产 producer**：需要先冻结 fact → replaceable 的映射；
2. **interaction 正文跨 daemon 重启持久化**：与 pending interaction 跨重启存活绑定；
3. **permission 正文 pinned 与终结 unpin**：需要稳定的权限终结域事件；
4. **driver 侧**：回收延迟（3s 巡检）、`not_eligible`/优先级、workspace command
   gate 集合；
5. **崩溃路径 writer fence 轮转**：`ToolLedger::Drop` 只覆盖有序退出；
6. TUI 侧 6 个 e2e harness 仍打 v1（他们的仓，勿在后端仓改）。

## 5. 接手注意

- 不要改回 bootstrap permission `request = None`；TUI/webui 的授权详情现在依赖
  canonical content ref。
- `RingingV2InteractionKind::PlanReview` 的 Rust 名不等于 wire 值；wire 永远是
  `plan`。
- `session.new` 已经会写 `SessionCreated`；任何 seeder 都不得再无条件追加第二个。
- content Range 只支持单区间，这是刻意限制；多区间返回 416，不做 multipart/byteranges。
