# TUI Ringing v2 最小锚点 Handoff（2026-09-24）

状态：**代码已实现，待 PR 合并与 tag 固化**。  
目标 tag：`tui-ringing-v2-types-2026-09-24`。  
范围：TUI 冻结 spec §14 步骤 1–2，即 `qaqh-ringing` v2 wire 契约与
`qaqh-client` v2 typed 消费面。daemon v2 端点不在本锚点内。

## 1. 已提供

### 1.1 `qaqh-ringing`

v1 常量与类型未修改，新增独立的 `qaqh_ringing::v2` 面：

- `RINGING_V2_VERSION = 2`
- `RINGING_V2_BASE_PATH = "/ringing/v2"`
- `CanonicalCursor`
- `CursorToken`
- `END_OF_FACT`
- `RingingV2EventEnvelope<P>`，含 `causation_id` / `correlation_id` 可选字段，
  用于 `causation_id = command_id` 的 interaction 完成判定
- `RingingV2Bootstrap`
- `RingingV2ResetRequired` / `RingingV2ResetReason`
- `RingingV2PendingInteraction` / `RingingV2PendingSet`
- `RingingV2DriverState`
- `RingingV2OpenRequest` / `RingingV2OpenResponse`
- `RingingV2CommandEnvelope`
- `RingingV2DriverClaimResponse` / `RingingV2DriverReleaseResponse`

`CursorToken` 使用：

```text
v2.<base64url-no-pad(compact-json)>
```

并校验 canonical 编码、可靠 cursor 的 `projection_index <= 65534`、snapshot
cursor 的 `END_OF_FACT`、跨 `log_id` 不可比较。

### 1.2 `qaqh-client`

TUI 只需依赖 `qaqh-client`，可从 crate root 命名：

- `ClientV2Cursor`
- `ClientV2CursorToken`
- `ClientV2Event`
- `ClientV2EventEnvelope`
- `ClientV2Payload`
- `ClientV2Bootstrap`
- `ClientV2Reset`
- `ClientV2ResetReason`
- `ClientV2PendingInteraction`
- `ClientV2PendingSet`
- `ClientV2DriverState`
- `ClientV2Subscription`
- `ClientV2SubscriptionEvent`
- `ClientV2SessionState`

新增 `Client` 方法：

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

`ClientV2Subscription::next()` 返回：

```text
ClientV2SubscriptionEvent::Event(Box<ClientV2Event>)
ClientV2SubscriptionEvent::Reset(ClientV2Reset)
```

事件 payload 是 `qaqh_session::session_fact_v2::ProjectionPayload`，壳层不需要
直接依赖 `qaqh-ringing`、`qaqh-domain` 或 `qaqh-session`。

结构化错误通过 `ClientError::Api { status, code, message }` 暴露，
`ClientError::code()` 返回稳定 code，包括：

```text
interaction_already_resolved
stale_driver_epoch
not_driver
unsupported_version
cursor_expired
snapshot_missing
```

## 2. 已锁验收

- v1 常量和类型测试零回归。
- cursor 往返、非法字符、非 canonical padding、跨 log 拒绝。
- reliable / replaceable / ephemeral 字段约束。
- v2 bootstrap 三频道形状。
- v2 command envelope 的 v2 identity 校验。
- `qaqh-client` 公共 API 编译测试。
- `qaqh-client` 结构化 error code 测试。

聚焦验证：

```text
cargo test -p qaqh-ringing --lib -- --test-threads=1 PASS
cargo test -p qaqh-client --all-targets -- --test-threads=1 PASS
cargo check -p qaqh-daemon --all-targets PASS
cargo clippy -p qaqh-ringing -p qaqh-client --all-targets -- -D warnings PASS
```

## 3. 本锚点不包含

- daemon `/ringing/v2` 路由。
- `open -> bootstrap -> since_cursor replay -> live -> reset` 原子闭环。
- interaction canonical registry 的 daemon 接线。
- driver `DriverChanged` 的 canonical projection 与 daemon 实现。
- v1 `Last-Event-ID` -> v2 cursor 映射。
- v2 fixture / Windows acceptance matrix。

这些进入下一步 P0-3 及后续。TUI 现在可以开始写 SessionModel reducer、
cursor/rebaseline 状态机和 typed interaction/driver 适配层，不需要等 daemon
端点全部落地。

## 4. TUI 使用建议

优先使用：

```rust
let client = qaqh_client::Client::connect_v2_async(options).await?;
let bootstrap = client.bootstrap_v2(seed).await?;
let mut subscription = client
    .subscribe_v2(seed, qaqh_client::Channel::Control, Some(&bootstrap.snapshot_cursor))
    .await?;
while let Some(frame) = subscription.next().await? {
    // ClientV2SubscriptionEvent::Event / Reset
}
```

若壳层已有 v1 `Client` 生命周期，也可保留现有连接，只额外调用 `open_v2()`；
v1/v2 lease identity 分开保存，不会互相覆盖。
