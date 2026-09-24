# #345 内容契约（纯 v2，无兼容）

状态：**已按裁决落地（v1 content 路由硬切）。**
裁决（2026-09-24）：**不留 v1，全力纯 v2，不做兼容。**
关联：issue #345、基线 spec `2026-09-23-TUI-Ringing-v2冻结语义-spec.md` §1/§2/§7。

---

## 0. 复核结论（落地前的事实）

1. **端点面不是新设计，是已冻结未实现**：基线 spec §1 已列
   `GET /ringing/v2/content/{content_id}` 与 `POST /ringing/v2/content`，§2 的 open
   响应已承诺 `capabilities.content = true`，§7 的 reason 集合里已有
   `content_quota_exceeded`；`qaqh-client::content_v2` 也早已存在。落地前 daemon 只有
   v1 路由 ⇒ 对客户端说了谎。
2. **存储已存在但是内存态**：`ringing/content_store.rs`（TTL 30 min / 256 条上限 /
   静默淘汰）。交互正文必须**pin**，否则 30 min TTL 会在交互还活着时把正文丢掉。
3. **跨 daemon 重启不需要重建 modal**：重启后 `live_interactions` 为空，
   `seal_orphan_channel_state` 把 pending interaction 收尾成 `Dismissed`。需求是
   「**同进程**内断线重连 / 换新客户端实例能重建 modal」。⇒ 内存态 store 足够，
   **不做**正文持久化。
4. **canonical 的 ref 有格式校验**：`ContentRef` 必须是 `sha256:<64 位小写 hex>`
   （`session_fact_v2/validation.rs::validate_content_hash`），而 content store 的条目
   id 是裸 hex（`qaqh_types::sha256_hex`）⇒ 端点负责归一（见 §2.1）。

---

## 1. 契约总览

| 议题 | 口径 |
|---|---|
| 路线 | 内容外置（B）：canonical 只存 ref，正文进 content store |
| 兼容 | **无**。v1 content 路由删除；客户端 / webui gateway 全部走 v2 |
| 生命周期 | 交互正文 **pin 到交互终态**（resolved / expired / 会话释放） |
| 配额 | pinned 条目按 seed 计：≤ 64 条且 ≤ 4 MiB；超限 fail-closed（不入库） |
| 鉴权 | 读取**不带 seed**：按 id 取条目，再用条目自己的 seed 校验归属 |
| 上传 | v2 multipart（`seed` / `media_type` / `content`），服务端校验归属 |
| 持久化 | 不做（理由见 §0.3） |

---

## 2. 端点

### 2.1 `GET /ringing/v2/content/{content_id}`

- **无 seed 查询参数**（与 `qaqh_client::content_v2(content_id)` 签名一致）。
- `content_id` 两种形态都接受：
  - `sha256:<64 hex>`：canonical `ContentRef` 的形态（schema 强制），端点 strip 前缀；
  - `<64 hex>`：content store 原生 id（工具输出外置等）。
- 归属：取到条目后用 `entry.seed` 校验调用方 `client_session_id` 的活跃 lease
  归属（`owns_seed`）。客户端可能同时 attach 多个 seed，不能反推。
- 状态码：
  - `200` + 原文（`Content-Type` 出站前过 `is_valid_media_type`，非法回退
    `application/octet-stream`，沿用 BUG-2026-09-13-03 的存储型 DoS 防线）；
  - `403 {"code":"content_forbidden"}` 归属不符；
  - `404` 不存在 / 过期 / 已释放；
  - `401` 无 token；无 client session → `lease_required`。
- range / 分页：**未实现**（alpha 整体 GET）。

### 2.2 `POST /ringing/v2/content`

multipart/form-data，字段 `seed` / `media_type` / `content`（与旧 v1 同形，只换路径）。
入库前校验 `media_type`（CRLF / 控制字符拒绝）与 seed 归属；返回
`{content_id, media_type, sha256, size, truncated:false}`。上传条目**不 pin**
（TTL + 容量淘汰），与交互正文区分。

### 2.3 v1 硬切

`/ringing/v1/content/{content_id}`、`/ringing/v1/content` 已删除（404）。
`qaqh-client`（`download_content` / `upload_content`）与 `qaqh-webui-gateway` 的
proxy 已改指 v2；`download_content` 的 `seed` 参数一并删除（端点不再需要）。

---

## 3. 正文与 ref

- media type：`application/vnd.qaqh.interaction-request+json`。
- body（由 `qaqh_domain::interaction_body` **单点**构造）：

  ```json
  { "kind": "ask",  "mode": "single|batch", "questions": [ ... ] }
  { "kind": "plan", "plan_content": "...", "review_type": "plan|todo_activation",
    "todo_items": [ ... ] }
  ```

- **两处序列化必须同源**：引擎（写 canonical `request_ref`）与 hub（写 store）都调用
  `interaction_body::{ask_body, plan_body}`。单测
  `crates/qaqh-runtime/tests/interaction_body_content_id.rs` 把
  `request_ref == sha256(body)` 钉死（这种漂移没有编译期错误）。
- `permission` kind 没有正文可取（详情在 tool 频道快照 / timeline 卡里），
  canonical ref 保持身份摘要；**客户端不得对 permission 尝试取正文**。
- **bootstrap wire 变更（#345）**：`RingingV2PendingInteraction` 新增可选字段
  `request: Option<RingingV2ContentValue>`（`kind`/`data` 判别式，与 canonical
  `ContentValue` 同形）。落地前 bootstrap 的 pending interaction **只有
  id/call_id/turn_id/kind**——正文 ref 在 wire 上根本不存在，这是 #345 之前
  重连拿不到正文的直接原因。permission 一律映射为 `None`。
- 正文 producer 在 daemon 事件入口：`actor::publish_worker_event` →
  `registry::stash_interaction_body` → `hub.put_interaction_content`（入库 + pin +
  记录 `seed → (interaction_id, content_id)`），交互 resolved / expired 时
  `hub.release_interaction_content` 解除 pin。

---

## 4. 配额与失败语义

- pinned 配额：**每 seed ≤ 64 条且 ≤ 4 MiB**（`PINNED_MAX_ENTRIES_PER_SEED` /
  `PINNED_MAX_BYTES_PER_SEED`）。交互正文是几百字节量级，额度只用于防跑飞。
- 超限时 `put_pinned` 返回 `ContentQuotaExceeded`（fail-closed，不静默淘汰）。
  当前落点：事件入口记录 `content_quota_exceeded` 错误日志、**不入库**；客户端对该
  ref 取到 404 → 按契约收掉 modal。
- **已知缺口**：真正「结束挂起回合」的 fail-closed 收尾需要走命令面（发布
  `InteractionResolved` 不会唤醒挂起的引擎），本批未做。pinned 额度按 seed 计且
  交互终结即释放，正常会话不可能触达该分支。
- 冻结 spec 把 `content_quota_exceeded` 列在 `ResetRequired.reason` 里；本契约把它
  当**写路径错误码**用（写失败不是「流需要重同步」）。spec 需要一条小修订，
  尚未提交。

---

## 5. 客户端行为

- 拿到 `ContentValue::Ref { content_ref }` → `GET /ringing/v2/content/{hash}` →
  按 `kind` 渲染 modal。
- **404 必须当「正文不可用」**：本地收掉 modal（可提示一句），不重试风暴。
- 交互 resolved / expired 后即可丢弃正文缓存。

---

## 6. 落地清单

| 面 | 位置 |
|---|---|
| pinned 存储 + 配额 + `get_any` / `unpin` | `crates/qaqh-runtime/src/ringing/content_store.rs` |
| 正文单点序列化 | `crates/qaqh-domain/src/interaction_body.rs` |
| canonical ref = 正文 id | `crates/qaqh-runtime/src/agent/engine_turn.rs::persist_interaction_requests` |
| 入库 + pin + 解除 | `crates/qaqh-runtime/src/ringing/hub.rs`、`registry.rs`、`actor.rs` |
| v2 路由 / v1 硬切 | `crates/qaqh-daemon/src/axum_server/axum_impl/{content.rs,mod.rs}` |
| 客户端 | `crates/qaqh-client/src/client.rs`（`download_content` / `upload_content`） |
| webui gateway proxy | `crates/qaqh-webui-gateway/src/lib.rs` |

## 7. 仍未做（后续）

1. 超配额的 fail-closed 收尾（走命令面结束挂起回合），以及 spec 里
   `content_quota_exceeded` 的定位修订。
2. `GET` 的 range / 分页。
3. `capabilities.timeline = true` 但 v2 timeline 路由不存在（TUI 现在走 v1 timeline）
   ——纯 v2 化的下一个硬缺口，需要 TUI 侧配合切换。
4. 交互正文的跨重启持久化（与「pending interaction 跨重启存活」绑定，属更大改动）。
