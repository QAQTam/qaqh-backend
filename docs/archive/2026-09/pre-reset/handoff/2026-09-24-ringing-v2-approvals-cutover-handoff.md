# Ringing v2 approvals 切换 Handoff（2026-09-24）

状态：**已落地。`/ringing/v1/*` 全部删除，wire 上再无 v1 路由。**

## 1. 这次修的是什么

纯 v2 化最后一条 v1 路由：`GET /ringing/v1/sessions/{seed}/approvals`
（本地浏览器网关的待审批投影）→ `GET /ringing/v2/sessions/{seed}/approvals`，
源从「v1 hub 的 control/tool 快照」换成 **canonical control 投影 + content ref**。

## 2. 复核前提：两个真缺口

1. **permission 详情不在 canonical 世界里。** #345 刻意让 permission「没有正文」
   （详情只在 tool 频道快照 / timeline 卡里）。纯 v2 之后 wire 上不再有 tool 频道
   快照 ⇒ v2 端点（以及 TUI 的授权面板）都拿不到 reason / paths / risk。
   修法：permission 也走 `interaction_body` 的 canonical ref。
2. **答复 id 的 canonical↔wire 对不上。** v2 投影只暴露 canonical
   `call_id` / `interaction_id`（wire id 的单向哈希，不可反推），而运行时的挂起表
   按 wire id 记账 ⇒ 用 v2 的 id 答复会落成 `unknown permission response` /
   `ask_id does not match the active prompt`。修法：运行时对 permission / ask /
   plan 的答复**同时接受两种形态**。

## 3. 改动

| 面 | 改动 |
|---|---|
| daemon 路由 | 删 `GET /ringing/v1/sessions/{seed}/approvals`；新增 `GET /ringing/v2/sessions/{seed}/approvals`（**形状不变**，网关/前端无需改） |
| daemon v2.rs | 新 handler：pending 集合取 canonical control 投影（未 resolved/expired），详情从 `request` 的 content ref 取回并映射回旧 v1 字段名 |
| daemon timeline_api.rs | 删 `handle_pending_approvals` + `pending_approval_payload` 及其单测 |
| qaqh-domain | 新增 `interaction_body::permission_body`（与 wire 字段一一对应的 canonical 正文） |
| qaqh-runtime | `engine_tool` 在请求授权时构造 permission 正文并随 `BatchAdmission` → `TurnState` 传下去；`persist_interaction_requests` 用它算 `request_ref`；`registry::stash_interaction_body` 把它写进 content store |
| qaqh-runtime | `permission_id_matches` / `interaction_id_matches`：permission / ask / plan 的答复都接受 canonical 或 wire id |
| webui gateway | `list_approvals` 改指 v2 路径（challenge/命令面不变） |
| 测试 | `permission_body` 单测；`approvals_v1_route_is_hard_cut`；`v2_pending_interactions_survive_reconnect` 增 approvals 形状断言；新 `interaction_body_permission_content_id`（钉死 ref == 正文 sha256）；`v2-smoke.sh` 加 approvals v1-404 / v2-200 断言 |

### permission 正文的 pin 语义

ask / plan 的正文 `put_pinned`（resolve 时按同一个 key unpin）。permission **不 pin**：
拒绝 / 过期路径没有可挂 unpin 的域事件，pin 会泄漏配额；正文只有几百字节，30min TTL
足够客户端取到。契约由 `interaction_body_permission_content_id` 钉住
（`entry.pinned == false`）。

## 4. 验证

```text
cargo test --workspace -- --test-threads=1              PASS（0 failed）
cargo clippy --workspace --all-targets -- -D warnings   PASS
cargo fmt --all -- --check                              PASS
scripts/v2-smoke.sh <data-root>                         PASS（含 approvals v1-404 / v2-200）
scripts/v2-content-probe.sh <data-root>                 PASS
```

真机 e2e（TUI 用本 rev 重建 + 本仓 daemon）：`e2e-alpha1-basic.sh` 4/4（含 exec 授权
批准，走 canonical id 答复）PASS。

## 5. alpha 未决清单（本次新增/更新）

1. **permission 正文不 pin**：极端容量压力下可能在客户端取到前被淘汰（30min TTL +
   几百字节，实际不可达）。若要做到与 ask/plan 同级的 fail-closed，需要一条
   「权限已终结」的域事件来 unpin。
2. **approvals 的 ask/plan 仍只有 `{id, kind}`**（与旧 v1 端点一致，details 为空）。
   正文现在可从 canonical ref 取到，前端要用的话可以顺手补上。
3. **v2 交互缺 wire call id**（承单流 handoff）：TUI 的授权面板仍靠 timeline 工具卡
   兜底；现在 canonical 正文里有完整详情，可改成直接读正文。
4. TUI v2 e2e harness 仍打 v1（`e2e-v2-*.sh` 7 个脚本）——rev bump 时同批修。

## 6. v1 面

**没有了。** `/ringing/v1/*` 路由全部删除。仍留的 v1 类型
（`RingingSessionBootstrap` / `RingingEventEnvelope` 等）只服务测试与历史断言，
可在后续清理批次里删。
