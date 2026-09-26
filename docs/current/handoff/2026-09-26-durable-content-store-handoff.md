# Durable Content Store / 大正文 content_ref Handoff

> 日期：2026-09-26
> 基线：`d83ef13`（`main`）
> 实现提交：`2480575`（`feat(content): persist externalized canonical content`）
> 状态：durable content store、InputAccepted / InterAgentCommunication 大正文外置、
> permission 正文 pin/unpin 已落地并通过全 workspace 门禁
> 范围：`qaqh-runtime`、`qaqh-daemon`、`qaqh-runtime/tests`

## 1. 一句话状态

content plane 已从「进程内、重启即丢、大正文跳过/拒绝」升级为：

```text
content store
  + 磁盘 metadata/body（懒加载 body）
  + 按内容 sha256 引用计数
  + TTL / 容量淘汰 / pinned 额度
  + 重启后按 pin_key unpin
  + 大 InputAccepted / InterAgentCommunication 自动外置为 content_ref
```

runtime 的 canonical 边界没有变化：小正文继续 inline，超过 8 KiB 的正文写
`content_ref`，不再出现「跳过 canonical fact」或「直接拒绝 inter-agent message」。

## 2. 已完成

### 2.1 Durable ContentStore

- 持久化路径：`<ringing_root>/content/<content_id>.json` + `<content_id>.bin`。
- 启动只加载 metadata；body 首次 `get` / `get_any` 时懒加载，并用 sha256 复核。
- `content_id` 是正文 sha256；相同正文由多个会话共享，`owners` 做引用计数。
- 最后一个 owner 释放时才删除文件；`release_session` 不会误删其他会话的内容。
- 未 pin 条目保留 TTL；pinned 条目不吃 TTL / 容量淘汰。
- 崩溃窗口产生的无 metadata `.bin` 在下次启动扫描时回收。
- 重启后 metadata 里的 `pin_key` 仍在，`release_interaction_content` 可按交互 id
  解除 pin；不依赖进程内 `live_interaction_content` 表。

### 2.2 大正文外置

- `InputAccepted`：
  - `inline_text.len() <= 8 KiB`：保持 inline；
  - 超过 8 KiB：写入 content store，fact 携带 `content_ref = sha256:<hex>`；
  - 不再 log-and-skip canonical fact。
- `InterAgentCommunication`：
  - 小正文：`InterAgentContent::Inline`；
  - 大正文：`InterAgentContent::ContentRef`；
  - `host_impl::send_agent_message` 的 8 KiB 直接拒绝已删除。
- `Loop` 现在持有 `Option<Arc<RingingHub>>`（由 `AgentRegistry` spawn 时注入），
  作为 canonical 大正文写入 content store 的入口。
- v2 content 端点继续按 `content_ref` 取正文；归属校验改为检查条目的
  `owners`，而不是单个 `seed`。

### 2.3 Permission 正文 pin / unpin

- ask / plan / permission 三类交互正文都进入 content store 并 pin。
- permission 的 pin_key 是 `canonical_interaction_id(tool_call_id)`。
- hub 在发布同一 `tool_call_id` 的 `ToolFinished` 时解除 pin；grant / reject /
  cancel 的终结路径都会落到该终态。
- 重启后 `live_interaction_content` 为空时，`unpin_key` 仍能释放磁盘上的 pin。

## 3. 验收证据

本次实现提交后执行：

```text
cargo fmt --all -- --check                                              PASS
cargo clippy --workspace --all-targets --offline -- -D warnings         PASS
cargo test --workspace --offline -- --test-threads=1                    PASS
```

新增/更新的定向覆盖：

- `content_store` 单测：
  - durable put/get round trip；
  - 重启后 pinned 条目仍可读、可按 `pin_key` unpin；
  - 相同正文跨会话引用计数；
  - 重启时清理过期未 pin 条目。
- `hub` 单测：
  - `RingingHub::with_persistence` 重启后 content 仍可读；
  - 重启后 live 表为空仍能按 pin_key 解 pin。
- `input_accepted_producer`：
  - 超过 8 KiB 的 inter-agent 正文写 `InterAgentContent::ContentRef`；
  - 对应 `InputAccepted` 写 `content_ref`、`inline_text = None`；
  - 两个 ref 都能从 hub 读回原文。
- 既有 `interaction_body_content_id` / `interaction_body_permission_content_id` /
  `permission_lifecycle` 全部通过。

## 4. 仍未完成 / 明确边界

### 4.1 Interaction 跨重启继续执行（未做产品裁决）

durable store 解决了「重启后正文丢失」，但没有改变「重启后挂起 turn 无法继续」
的既有语义：

- `seal_orphan_channel_state(force=false)` 在重启后仍会把 pending interaction
  收成 `Dismissed`；
- 原因是 turn actor / tool engine 的挂起状态在内存中，当前没有 turn resume 路径；
- 现在正文在 dismissal 后按 `InteractionResolved` unpin，并保留 30 min TTL，
  客户端仍可在窗口内读到正文，但交互本身不会恢复为 pending。

下一步需要在两个方向中选一个：

1. 明确产品语义为「daemon 重启即 dismiss pending interaction」，并删除
   debug backlog 里的「跨重启继续」表述；
2. 实现 turn resume：从 canonical facts 重建挂起 tool batch / interaction，
   再恢复 actor。这是独立的大切片，不应只靠 content store 解决。

### 4.2 容量与 I/O

- `ContentStore` 仍是全局 `max_entries = 256`，未改为按字节预算；
  大正文虽持久化，但未 pin 条目在容量压力下仍可能被淘汰。
- `put` / `unpin` 的磁盘写是同步 I/O，持 content_store 锁；大输出路径暂未
  异步化。常规 8 KiB~百 KiB 正文可接受，超大输出需要后续压测。
- 当前持久化以 body 先写、metadata 后写；崩溃窗口的孤儿 body 在下次启动回收。

### 4.3 Permission unpin 的边界

- 依赖 `ToolFinished` 作为 permission 终结信号；
- 如果未来出现「permission resolved 但不产生 ToolFinished」的新路径，需要
  在 hub 增加对应 unpin 事件，否则 pin 会保留到 session close。

## 5. 关键文件

- `crates/qaqh-runtime/src/ringing/content_store.rs`
- `crates/qaqh-runtime/src/ringing/hub.rs`
- `crates/qaqh-runtime/src/agent/loop_dispatch_conversation.rs`
- `crates/qaqh-runtime/src/agent/loop_core.rs`
- `crates/qaqh-runtime/src/actor.rs`
- `crates/qaqh-runtime/src/agent/spawn.rs`
- `crates/qaqh-runtime/src/registry.rs`
- `crates/qaqh-runtime/src/host_impl.rs`
- `crates/qaqh-daemon/src/axum_server/axum_impl/content.rs`
- `crates/qaqh-runtime/tests/input_accepted_producer.rs`

## 6. 接手注意事项

- canonical 小正文阈值仍是 8 KiB；不要为了「统一」把小正文也外置，这会增加
  UI 取正文的 RTT。
- `content_ref` 的 wire 形态是 `sha256:<64 hex>`；content store 内部 id 是裸
  hex，端点负责 strip 前缀。
- content store 是「展示面/大正文旁路」，不是 canonical fact log 的替代；
  canonical fact 仍必须先于依赖它的 turn 写入。
- 不要把 `owners` 改回单 `seed`：相同正文跨会话去重是持久化层的正确性前提。
- 不要在没有 turn resume 设计的情况下把重启后的 pending interaction 强行
  保活；那会留下「可点击但无执行者」的幽灵 modal。
- 不要恢复 `send_agent_message` 的 8 KiB 硬拒绝；大正文走 content_ref 是本轮
  已冻结的契约。
