# #345 pending interaction 正文（纯 v2 content 面）Handoff（2026-09-24）

状态：**已落地并实机验证。** 契约见
`docs/plan/2026-09-24-345-content-contract-plan.md`。

## 1. 这次修的是什么

`ask` / `plan` 的 modal 正文原先只走「当前进程内广播的 v1 域事件」：客户端一断线
重连（或换新 client session）就只剩 bootstrap 里的交互身份，弹不出 modal。
本次把正文做成 **canonical ref + content store** 的展示面旁路，客户端按 ref 取正文。

**最关键的一条复核发现（和 issue 描述不同）**：bootstrap 的
`RingingV2PendingInteraction` **压根没有 `request` 字段**——不是「ref 取不到正文」，
而是「ref 不在 wire 上」。所以本次同时补了 wire 字段，否则端点实现了也拿不到 ref。

## 2. 落地内容

| 面 | 改动 |
|---|---|
| 正文序列化 | 新增 `qaqh_domain::interaction_body`（ask/plan 正文的**唯一**构造点） |
| canonical ref | `persist_interaction_requests` 用正文 `sha256` 作为 `request_ref`（ask/plan）；permission 保持身份摘要 |
| content store | 新增 pinned 条目（不吃 TTL / 不被容量淘汰）、按 seed 的 pinned 配额、`get_any`、`unpin` |
| 入库与释放 | `actor::publish_worker_event` → `registry::stash_interaction_body` → `hub.put_interaction_content`（入库 + pin）；交互 resolved/expired 时 `release_interaction_content` 解除 pin |
| bootstrap wire | `RingingV2PendingInteraction.request: Option<RingingV2ContentValue>`；permission → `None` |
| v2 端点 | `GET /ringing/v2/content/{content_id}`（**无 seed 参数**，按条目 seed 校验归属）、`POST /ringing/v2/content`（multipart） |
| v1 硬切 | `/ringing/v1/content/*` 删除（404）；`qaqh-client`、`qaqh-webui-gateway` 改指 v2 |

id 归一：canonical `ContentRef` 必须是 `sha256:<64 hex>`（schema 校验），content store
条目 id 是裸 hex ⇒ 端点接受两种形态（strip 前缀后查 store）。

## 3. 验证证据

```text
cargo test --workspace -- --test-threads=1              PASS（0 failed）
cargo clippy --workspace --all-targets -- -D warnings   PASS
cargo fmt --all -- --check                              PASS
scripts/v2-smoke.sh <data-root>                         PASS（全阶段）
scripts/v2-content-probe.sh <data-root>                 PASS（#345 专项，见下）
```

`scripts/v2-content-probe.sh`（真实 daemon + 真实引擎 + fake provider 下发 `ask`）：

```text
pending interaction: {..., "kind": "ask",
  "request": {"kind":"ref","data":{"content_ref":"sha256:05679730…180b2"}}}
  [✓] ① bootstrap 带 ContentValue::Ref
  [✓] ② 用 canonical ref 取回 ask 正文 — {"kind":"ask","mode":"single",
        "questions":[{"id":"q1","question":"Proceed with the #345 content probe?",…}]}
  [✓] ③ 裸 hex 形态同样可取
  [✓] ④ 第二个 client session 重建 modal 正文 — ref_match=True status=200
  [✓] ⑤ v1 content 路由已硬切 — status=404
RESULT: PASS
```

TUI 侧（真实 PTY，用本仓 daemon）：`MODE=ask` / `MODE=plan`
`e2e-v2-interactions.sh`、`e2e-alpha1-basic.sh` 全 PASS——bootstrap 新增字段对既有
壳层是可选字段，未破坏。

单测：
`crates/qaqh-runtime/tests/interaction_body_content_id.rs` 把
「canonical `request_ref` == 正文 content_id」钉死（两处各自序列化是最容易漂移、
且没有编译期错误的点）；`crates/qaqh-runtime/src/ringing/content_store.rs` 覆盖
pin / 配额 / unpin；daemon 覆盖 v2 路由 + 归属 403 + v1 硬切 404。

## 4. alpha 未决（本次未做）

1. **超配额的 fail-closed 收尾**：现在超配额只记错误日志 + 不入库（客户端 404 →
   收掉 modal）。真正结束挂起回合要走命令面（发布 `InteractionResolved` 不会唤醒
   挂起的引擎）。配额 64 条 / 4 MiB per seed，正常会话触不到。
2. **spec 小修订**：`content_quota_exceeded` 现在被当写路径错误码用，冻结 spec 把它
   列在 `ResetRequired.reason` 里。
3. `GET /ringing/v2/content` 的 range / 分页。
4. **`capabilities.timeline = true` 但 v2 timeline 路由不存在**——纯 v2 化的下一个
   硬缺口（TUI 目前走 v1 `/ringing/v1/sessions/{seed}/timeline`）。需要 TUI 侧配合
   切换，属跨仓改动。
5. 交互正文跨重启持久化：与「pending interaction 跨重启存活」绑定（今天重启后
   `seal_orphan_channel_state` 会把交互收尾成 `Dismissed`），属更大改动。
6. 其余沿用上一份 handoff 的 alpha 清单（#336 / #339 挂起 / P6 B / driver 侧四项 /
   fence 轮转 / interaction kind 拼写）。V2-C3 replaceable producer 已补，见
   `2026-09-24-ringing-v2-alpha-closure-handoff.md` §2.7；V2-V1 cursor 映射已随
   v1 硬切作废，见 `2026-09-24-tool-outcome-p2-v1-cursor-closure-handoff.md` §1。

## 5. 接手注意

- **不要**在别处再写一份 ask/plan 正文的 `serde_json::json!`：ref 与 store 的
  content_id 必须由 `qaqh_domain::interaction_body` 单点产出，否则 ref 解析不到
  且没有编译期错误。
- permission 的 `request` 恒为 `None`（客户端不得取 content）；它的详情在 tool
  频道快照 / timeline 卡里。
- `qaqh-client` 现在有两个「内容值」别名：`ClientV2ContentValue`（SSE control
  delta，canonical 类型）与 `ClientV2PendingContentValue`（bootstrap，wire 类型），
  JSON 形态一致、类型不同（wire 层不依赖 session crate）。
