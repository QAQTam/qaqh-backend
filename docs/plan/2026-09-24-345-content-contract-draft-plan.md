# #345 内容契约草案（DRAFT · 待裁决）

状态：**草案，未冻结、未动代码。**
目的：把「pending interaction 正文怎么让 v2 客户端拿到」的契约先定下来，再决定动手面。
关联：issue #345、基线 spec `2026-09-23-TUI-Ringing-v2冻结语义-spec.md` §1/§2/§7、
`docs/handoff/2026-09-24-323-typed-payload-gap1-handoff.md` §3。

---

## 0. 复核结论（先说结论，再给证据）

1. **端点面不是新设计，是已冻结未实现。** 基线 spec §1 已列
   `GET /ringing/v2/content/{content_id}` 与 `POST /ringing/v2/content`，
   §2 的 open 响应已承诺 `capabilities.content = true`，§7 的 reason 集合里已有
   `content_quota_exceeded`。而 daemon 只注册了 v1 路由（`mod.rs:196-197`），
   `qaqh-client::content_v2`（`v2.rs:479`）**已经存在且会 404**。
   ⇒ 现状是「对客户端说了谎」的假广告，本身就该修。
2. **存储已存在，但是内存态。** `ringing/content_store.rs`：`HashMap` +
   `Instant`、TTL 30 min、`max_entries = 256`、超限**静默淘汰**最早过期项、
   `forget_seed` 时整会话释放。进程重启即丢。
3. **正文在 durable 路径上确实存在，只是不在 canonical log 里**：
   - v1 control 域事件带全文（`ControlEvent::InteractionRequested{questions}` /
     `PlanReviewRequested{plan_content,todo_items}`）；
   - 它被**持久化进 v1 ringing journal**：
     `$DATA/ringing/journal/control/{seed}.jsonl` 里能直接读到 `questions` 正文；
   - 另外 timeline 工具卡的 `args_json` 也是 durable 的
     （`$DATA/ringing/ringing-timeline/{seed}.json`）。
4. **canonical fact 里的 `request_ref` 是身份三元组的 hash，不是正文 hash**
   （`engine_turn.rs:325-334`）⇒ 即便实现了 content 端点，今天也取不到东西。
5. **跨 daemon 重启不需要重建 modal。** 重启后 `live_interactions` 为空，
   `seal_orphan_channel_state` 会把 pending interaction 收尾成 `Dismissed`
   （`orphan_seal.rs:200-243`）。⇒ 需求是「**同进程**内断线重连 / 换新客户端实例
   能重建 modal」，**不是**「跨重启」。这条把 B 路线最大的顾虑（内存态存储不够
   durable）直接消掉了。
6. 交互生命周期内 worker 不会被回收（`liveness.rs::suspend_pending` 阻止
   idle-unload）⇒ 交互「活着」的时间由用户决定，可能远超 30 min。
   **30 min TTL 会在交互还活着的时候把正文丢掉**，这是必须处理的点。

---

## 1. 三条候选路线

| 路线 | 做法 | 改动面 | 代价 / 风险 |
|---|---|---|---|
| **B（推荐）** | 交互创建时把正文写进 content store，`request_ref` 指向它；补 v2 content GET | 引擎 1 处 producer + store 加 pin + daemon 加 2 条路由（upload 可延后） | 需要定 pin / 配额语义；沿用已冻结 wire |
| **D″** | v2 bootstrap 读侧 join v1 控制投影 / v1 journal 的正文，直接内联 `ContentValue::Inline` | 只改 daemon 的 bootstrap（+ 可能的 delta 路径） | **wire 零改动**、零新存储；但让 v1 journal 成为 v2 客户端的载重面，与「v2 canonical 才是权威、v1 是过渡映射层」的方向冲突 |
| **D** | 客户端自己从 timeline 卡 `args_json` 重建 modal | 只改 TUI 仓 | 无需后端改动；但要把 ask 参数归一化逻辑复制到客户端，跨仓漂移 |

**建议 B。** 理由：① 它是已冻结 wire 的设计意图（端点 + capability + client 方法都在），
补齐它同时修掉「假广告」；② 正文属于**展示面**，进 canonical 会破坏「canonical 只存事实」
的取向，而 content store 正是为此存在的通道；③ D″/D 都要把 v1 journal / timeline 变成
v2 客户端的隐式依赖，长期是负债。D″ 可作为**不引入新存储的备选**，若你想把 #345 压到最小
改动面，它是唯一能做到「wire 零改动」的路线。

---

## 2. 若走 B：契约草案

### 2.1 端点

| 用途 | 方法 | 路径 | 备注 |
|---|---|---|---|
| 读取 | `GET` | `/ringing/v2/content/{content_id}` | **不带 seed 查询参数**，与已存在的 `qaqh_client::content_v2(content_id)` 签名一致 |
| 上传 | `POST` | `/ringing/v2/content` | multipart，字段 `seed` / `media_type` / `content`，与 v1 同形 |

- 读取：服务端用 `content_id` 查出条目 → 取条目的 `seed` → 校验调用方
  `client_session_id` 对该 seed 有**活跃 lease 归属**（`owns_seed`）。
  - 归属不符 → `403 {"code":"content_forbidden"}`
  - 不存在 / 已过期 / 已释放 → `404 {"code":"content_not_found"}`
  - 无 token → 401；无 client session → `lease_required`
  - `media_type` 出站前仍走 `is_valid_media_type` 兜底（BUG-2026-09-13-03 的
    存储型 DoS 防线不能只在 v1 生效）
- 上传：**alpha 不阻塞 #345**，可与读取同批做，也可延后；若延后，需裁决
  `capabilities.content` 的含义（见 §3.4）。
- **range / 分页延后**（`content_store.rs` 文档里提到 range，但 alpha 先做整体 GET）。

### 2.2 媒体类型与正文 schema

交互正文统一 `media_type = application/vnd.qaqh.interaction-request+json`，body：

```json
{ "kind": "ask",  "mode": "single|batch", "questions": [ ... ] }
{ "kind": "plan", "plan_content": "...", "review_type": "plan", "todo_items": [ ... ] }
```

- 字段与 v1 域事件同源，客户端**不需要**二次归一化（单/批 ask 的归一化在服务端
  `normalize_ask_args` 已经做过一次，不能在客户端再复制一份）。
- 正文由**服务端写**：客户端只读，不提供「客户端上传交互正文」的路径。

### 2.3 生命周期：pin 到交互终态

- 交互正文条目以 **pinned** 方式入 store：**不受 30 min TTL、不受 256 条上限淘汰**。
- 解除 pin 的时机：`InteractionResolved` / `InteractionExpired` 落盘后，
  或 `forget_seed`（会话关闭）。会话关闭时 `release_session` 照旧整体释放。
- 非 pin 条目（工具输出外置、附件、图片）保持今天的 TTL + 上限淘汰行为不变。
- 这样「交互活多久，正文就活多久」，不需要给交互额外发明 TTL，也不需要在
  `expires_at_ms` 上加定时器（今天全仓没有任何东西在读它，它恒为 `None`）。

### 2.4 配额与 fail-closed

- pin 条目单独计配额（建议：**每 seed ≤ 32 条 pinned 且 ≤ 1 MiB 合计**；交互正文
  实测是几百字节量级，这个额度只用于防跑飞）。
- 超配额时**不得静默淘汰、也不得创建没有正文的交互**：交互创建路径 fail-closed，
  回合以错误码 `content_quota_exceeded` 收敛。
- 需要裁决：冻结 spec 把 `content_quota_exceeded` 列在 **`ResetRequired.reason`** 里，
  但「写路径失败」不是「流需要重同步」的条件。**建议**：它作为回合/命令错误码使用，
  spec 需要一条小修订把它从 reset reason 列表里挪走（或明确它只用于「store 整体不可
  用时要求客户端 rebaseline」这种极端情形）。这条不定，代码里就没有正确的落点。

### 2.5 与 canonical 的关系

- content 是**展示面旁路**：canonical fact 只保留 `ContentRef`，正文永不进 canonical。
- 因此 `request_ref` 必须从「身份三元组 hash」改成「正文 hash」＝store 返回的
  `content_id`（`sha256(bytes)`）。**推荐做法**：在交互创建处**一次**序列化正文、
  经一个内容出口（`Emitter` 上加 `put_content` 之类的默认空实现）写入 store，
  用 store 返回的 id 直接构造 `ContentRef`——**只有一个 producer，ref 与 bytes
  不可能不一致**。
  - 备选：hub 侧写（它天然持有 store 且能看到 v1 事件），但那样「算 ref 的地方」和
    「写 bytes 的地方」是两处序列化，必须共享同一个序列化函数，否则 ref 解析不出来。
- 非 Ringing 出口（测试 mock emitter）没有 store 时，退化为今天的行为（身份 hash ref），
  保证既有测试面不变。
- **不做**：正文持久化 / 跨重启重建。理由见 §0.5。

### 2.6 客户端行为

- 拿到 `ContentValue::Ref` → `GET /ringing/v2/content/{id}` → 按 `kind` 渲染 modal。
- **404 必须当成「该交互正文不可用」**：本地把该 modal 收掉（可提示一句），
  不得重试风暴、不得卡在空 modal。服务端已经把这种交互收尾成 `Dismissed`，
  客户端的 404 处理是最后一道兜底。
- 客户端不得缓存超过交互生命周期（交互 resolved 后即可丢弃正文）。

---

## 3. 需要你裁决的点（其余我按上面推荐值执行）

1. **路线**：B（推荐）/ D″（wire 零改动，但 v1 journal 变载重面）/ D（改 TUI 仓）。
2. **`content_quota_exceeded` 的定位**：作为回合错误码（推荐，需一条 spec 小修订）
   还是严格留在 `ResetRequired.reason` 集合里（那要先定义触发场景）。
3. **配额数值**：每 seed 32 条 / 1 MiB（推荐）还是别的口径。
4. **v2 上传**：与 GET 同批做，还是先 GET、upload 留 v1（若留 v1，需定
   `capabilities.content` 是「读可用」还是拆成 `content_read` / `content_write`）。
5. **顺带修不修这两个假广告**（建议开成独立 issue，不塞进 #345）：
   - `capabilities.content = true` 但 `/ringing/v2/content/*` 无路由；
   - `capabilities.timeline = true` 但 `/ringing/v2/sessions/{seed}/timeline` 无路由
     （TUI 现在实际走的是 v1 timeline 路由）。

---

## 4. 证据索引

| 事实 | 位置 |
|---|---|
| v2 端点表（含 content） | `docs/spec/2026-09-23-TUI-Ringing-v2冻结语义-spec.md:44-45` |
| `content_quota_exceeded` 在 reset reason 列表 | 同上 `:330` |
| daemon 只注册 v1 content 路由 | `crates/qaqh-daemon/src/axum_server/axum_impl/mod.rs:196-197` |
| v2 open 承诺 `content/timeline = true` | `crates/qaqh-daemon/src/axum_server/axum_impl/v2.rs:93-100` |
| client 已有 `content_v2`（无 seed 参数） | `crates/qaqh-client/src/v2.rs:479-492` |
| content store 实现（TTL/上限/淘汰） | `crates/qaqh-runtime/src/ringing/content_store.rs` |
| v1 正文入 journal（真机样本） | `$DATA/ringing/journal/control/{seed}.jsonl` |
| timeline 卡 `args_json` durable（真机样本） | `$DATA/ringing/ringing-timeline/{seed}.json` |
| `request_ref` 现状＝身份三元组 hash | `crates/qaqh-runtime/src/agent/engine_turn.rs:325-334` |
| 重启后 pending interaction 被收尾 | `crates/qaqh-runtime/src/ringing/orphan_seal.rs:200-243` |
| 交互挂起时 worker 不被回收 | `crates/qaqh-runtime/src/agent/liveness.rs:7-27` |
