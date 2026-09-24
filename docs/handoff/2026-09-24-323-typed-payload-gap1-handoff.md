# #323 TUI v2 typed payload 消费面 Handoff（2026-09-24）

状态：**缺口 ① 已落地；缺口 ③ 复核后发现比原描述更深，需裁决。**

## 1. 三条缺口现状

| 缺口 | 状态 |
|---|---|
| ① `ClientV2Payload` 内部类型不可命名 | ✅ **本次落地** |
| ② `DriverChanged` 未进 canonical projection | ✅ 已落地（MR #330） |
| ③ pending interaction 的 request 载荷不足 | ⚠️ **复核后改判**（见 §3） |

## 2. 缺口 ①：typed control delta 现在可从 `qaqh-client` 命名

### 改动

`crates/qaqh-client/src/v2.rs` 新增别名导出（`lib.rs` 同步 re-export）：

| 别名 | 源类型 | 用途 |
|---|---|---|
| `ClientV2ControlDelta` | `session_fact_v2::ControlDelta` | **match 枚举变体必须写枚举名**，这是缺口的核心 |
| `ClientV2DeltaInteractionKind` | `session_fact_v2::InteractionKind` | `InteractionRequested.kind` |
| `ClientV2InteractionId` | `session_fact_v2::InteractionId` | `interaction_id` |
| `ClientV2ToolCallId` | `session_fact_v2::ToolCallId` | `call_id` |
| `ClientV2ContentValue` | `session_fact_v2::ContentValue` | `request` / `decision` |
| `ClientV2InteractionDecision` | `session_fact_v2::InteractionDecision` | `verdict` |
| `ClientV2ActorRef` | `session_fact_v2::ActorRef` | `resolved_by` |
| `ClientV2InteractionExpiryReason` | `session_fact_v2::InteractionExpiryReason` | `InteractionExpired.reason` |

**只导出 `ClientV2Payload` 不够**：`match ClientV2Payload::ControlDelta(d) => match d { … }`
里内层 match 必须写出 `ControlDelta::` 路径，壳层拿不到这个名字就只能退回字符串比较。

### 顺带发现：两套 interaction kind 的 wire 拼写不一致

做 ① 时发现同一交互在两条路径上拼写不同：

| 路径 | 类型 | wire 值 |
|---|---|---|
| bootstrap `control.state.interactions[].kind` | `RingingV2InteractionKind`（`qaqh-ringing`） | `ask` / `plan_review` / `permission` |
| SSE `ControlDelta::InteractionRequested.kind` | `InteractionKind`（`qaqh-session`） | `ask` / `plan` / `permission` |

即 **plan 在 bootstrap 是 `plan_review`、在 delta 是 `plan`**。壳层必须自己映射。

本次**不改 wire**（那是独立决策），只把两个枚举都做成可从 `qaqh-client` 命名，
并在 `v2_public_api.rs` 的测试里把该差异**写成断言注释**，避免它继续隐形。

### 测试

`crates/qaqh-client/tests/v2_public_api.rs::control_delta_interaction_branches_are_matchable_from_the_client_root`

只依赖 `qaqh-client`，覆盖：

- `classify(&ClientV2ControlDelta)` 对 `InteractionRequested` / `InteractionResolved` /
  `InteractionExpired` / `DriverChanged` 四个分支做 match，并把每个字段绑到对应的
  `ClientV2*` 类型上（证明可命名）；
- 用真实 wire JSON 反序列化三种 interaction delta，断言 `kind` 取值
  （`"plan"` / `"approved"` / `"timeout"`）。

## 3. 缺口 ③：复核后改判 —— request 内容**根本不在 canonical log 里**

原 issue 给的两个选项是：

1. bootstrap 的 `control.state.interactions` 携带 typed request payload；或
2. 保证 bootstrap 之后 reliable replay 对应 `InteractionRequested`。

**复核结论：两个选项都不够。** 因为：

1. `ControlInteractionState.request` 由 `content_ref_value(request_ref)` 得到，恒为
   `ContentValue::Ref { content_ref }` —— **只是一个 hash 引用**。
2. 而那个 `request_ref` 的内容是**身份三元组的 hash**，不是 modal 正文：
   `crates/qaqh-runtime/src/agent/engine_turn.rs:331`
   ```rust
   let request_bytes = serde_json::to_vec(&serde_json::json!({
       "interaction_id": …, "call_id": …, "kind": format!("{kind:?}"),
   }));
   request_ref: ContentRef::new(sha256_content_hash(&request_bytes)),
   ```
3. **没有任何 content store 持久化这些字节**：真机会话目录只有
   `canonical-identity.json / events.jsonl / events.commit.json / messages.jsonl /
   messages.wal / meta.json / writer-fence.json`，**没有内容目录**。
4. 真正的 modal 正文（ask 的 `questions`/`AskMode`、plan 的 `plan_content`/`todo_items`）
   只存在于**内存中的 turn 状态**，经 v1 domain event（control 频道）广播：
   `crates/qaqh-domain/src/event.rs:594 InteractionRequested { mode, questions }`、
   `:606 PlanReviewRequested { plan_content, todo_items }`。

⇒ 在 v2 上「bootstrap + reliable replay」**无论如何都拿不到 modal 正文**——它在
durable 路径里不存在。spec §0.6「pending interaction 以 `interaction_id` 重放」
目前只重放了**身份**，没重放**内容**。

### 可落地的三条路线（需要裁决）

| 路线 | 做法 | 代价 |
|---|---|---|
| **A. 把 request 写进 canonical fact** | `InteractionRequested` 增加真实 payload（inline 或真 content ref） | **canonical fact schema 变更**（payload_version 提升）+ 兼容矩阵 |
| **B. 内容落盘 + `/ringing/v2/content/{id}`** | 把 request 字节写进内容寻址存储，实现 P1 的 content 端点 | 新增存储 + 端点；P1 范围 |
| **C. 让 bootstrap 直接内联正文** | bootstrap 时从**内存/registry**取正文塞进 `control.state.interactions` | 重启后内存没了 → 只在同进程内有效；不满足「断线重连」 |

**倾向 B**：它同时服务 ask / plan / permission 三类，且是 P1 已登记项
（`/ringing/v2/content`）；而 A 会让 canonical fact 承载展示面正文，与
「canonical 只存事实、不存展示」的既有取向相冲突。

**C 单独不可用**，但可以作为 B 落地前的临时缓解（同进程重连可用）。

## 4. 验证证据

```text
cargo test --workspace -- --test-threads=1              PASS（138 suites，0 failed）
cargo clippy --workspace --all-targets -- -D warnings    PASS
cargo fmt --all -- --check                               PASS
```

## 5. 接手注意

- `ClientV2InteractionKind`（bootstrap）与 `ClientV2DeltaInteractionKind`（SSE delta）
  **不是同一个枚举**，wire 拼写也不同（`plan_review` vs `plan`）。要统一必须先定
  wire 归属，不要在下游悄悄映射。
- 缺口 ③ 不要在没裁决路线前动手：三个选项的改动面差一个数量级。
- `ClientV2ControlDelta` 是全量导出（不只 interaction 分支），后续 canonical 新增
  control delta 变体会自动出现在壳层可见面——这是刻意的（避免每加一个变体就补一次
  导出），但也意味着 **canonical delta 的变体名属于壳层可见契约**，改名要按破坏性
  变更处理。
