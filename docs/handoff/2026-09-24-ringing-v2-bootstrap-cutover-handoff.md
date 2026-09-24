# Ringing v2 bootstrap 切换 Handoff（2026-09-24）

状态：**后端已落地（v1 路由硬切 + client 走 v2）；TUI 侧改了 2 个文件（未提交，需一次
rev bump 才能吃到）。**

## 1. 这次修的是什么

纯 v2 化剩下的权威快照入口：`GET /ringing/v1/sessions/{seed}/bootstrap` 返回的是 **v1 领域
state**（`qaqh_domain::state::{ControlState, ConversationState, ToolState}`，`state` 是
`serde_json::Value`）。v2 侧早就有 `GET /ringing/v2/sessions/{seed}/bootstrap`，返回的是
canonical 三频道 **typed 投影**（`control` / `conversation` / `tool`）。

本次把 bootstrap 也硬切到 v2：**删 v1 路由**，client `bootstrap()` 改走 v2 并换返回类型。

## 2. 复核前提（重要）

按「先验证前提再动手」的纪律，先核了两件事：

1. **v1 bootstrap 与 v2 bootstrap 不是同一份数据。** v1 读 `state.hub`（daemon 侧 v1 事件总线
   的领域投影），v2 读 `state.v2_hub`（canonical fact 的可重建投影）。两者**字段形状不同**，
   不是「换个路径就行」。
2. **TUI 的 bootstrap 装配依赖 v1 领域词汇**：`conversation.usage/usage_totals/context_limit/
   model`、`control.activity/dashboard_snapshot`、`tool.pending_permission`。这些在 v2 投影里
   **部分不存在**（`dashboard_snapshot` / `usage_totals` / `context_limit` 都没有）。

因此这不是「机械换路径」，而是一次 **client API 签名变更**（`RingingSessionBootstrap` →
`ClientV2Bootstrap`），TUI 消费点必须同批改。降级项见 §5。

## 3. 改动

| 面 | 改动 |
|---|---|
| daemon 路由 | 删 `GET /ringing/v1/sessions/{seed}/bootstrap`（404）；v2 路由不变 |
| daemon handler | 删 `handle_bootstrap`（v1 领域快照组装）；`handle_pending_approvals` 保留 |
| qaqh-client | `Client::bootstrap()` 改走 `bootstrap_v2()`，返回 `ClientV2Bootstrap`；新增 v2 词汇/上下文的 re-export（`ClientV2ActivityState`、`ClientV2ConversationContextKind`） |
| webui gateway | bootstrap proxy 路径 `v1 → v2`（lease 本就是 v2，无需改鉴权） |
| 测试 | 新增 `bootstrap_v1_route_is_hard_cut`；`v2-smoke.sh` 加「v1 bootstrap 404」断言 |
| TUI（未提交） | `ActionResult::Bootstrap` / `App::bootstrap` 换 `ClientV2Bootstrap`；装配臂改读 typed 三频道 |

### TUI 装配臂的映射

| 目标字段 | 来源 |
|---|---|
| `sess.activity` | `control.state.activity`（v2 词汇 idle/running/interrupted → 领域 idle/working/disconnected） |
| `sess.conversation` | `conversation.state.context` 里**最新** assistant block 的 `model`/`usage`（其余留默认） |
| 挂起权限 | `control.state.interactions` 里 `kind == permission` 的 `call_id` |
| dashboard | v2 不携带 → 无条件回退 `session.dashboard` 拉取 |
| usage/model/context_limit | bootstrap 只做首刷；后续由 v1 SSE `UsageUpdated` 补全 |

## 4. 验证

```text
cargo test --workspace -- --test-threads=1              PASS（0 failed）
cargo clippy --workspace --all-targets -- -D warnings   PASS
cargo fmt --all -- --check                              PASS
scripts/v2-smoke.sh <data-root>                         PASS（含新增 v1 bootstrap 404）
scripts/v2-content-probe.sh <data-root>                 PASS（#345 专项）
```

**真机（TUI 用本 rev 重建 + 本仓 daemon）**：

| e2e | 结果 |
|---|---|
| `e2e-alpha1-basic.sh` | PASS（4/4） |
| `e2e-new-session.sh` | PASS（4/4） |
| `e2e-history.sh` | PASS（6/6） |

## 5. alpha 降级 / 未决

1. **`dashboard_snapshot` 不再来自 bootstrap**：v2 control 投影没有该字段，TUI 每次
   bootstrap 都重拉 `session.dashboard`。可接受，但多一次 RPC；若要省，需把 dashboard 并入
   v2 control 投影。
2. **`usage_totals` / `context_limit` 从 bootstrap 消失**：v2 投影不聚合这两项。TUI 靠
   `UsageUpdated` 事件补；冷启动到首个 usage 事件之间状态栏可能空。归入 v2 模型迁移。
3. **activity 词汇收窄**：v2 只有 idle/running/interrupted，`waiting_user` 改由挂起交互面板
   判定（TUI `is_waiting_user` 本就以面板为准）。
4. **TUI v2 e2e harness 仍打 v1**：`e2e-v2-interactions.sh` 等脚本直接打
   `/ringing/v1/clients/open`（#349 硬切后已 404）。这批 harness 必须同批改成 v2 open +
   `session_attach`，与本次无关但属同一收口。
5. **`RingingSessionBootstrap` 类型仍在**（`qaqh-ringing` + `qaqh-client` re-export）：v1 SSE
   仍产出领域投影，`hub.rs` 的产出方往返测试仍用它。等 SSE 单流迁完一并删。

## 6. 仍是 v1 的面（纯 v2 化的剩余清单）

| 端点 | 现状 |
|---|---|
| `GET /ringing/v1/events/{channel}` | v1 三频道 SSE（v2 单流已存在）——**最大一块** |
| `GET /ringing/v1/sessions/{seed}/approvals` | 只读审批查询，v2 无对应端点（需设计：并入 bootstrap 或新端点） |

⇒ 只剩这两条（+ 上面第 4 条的 harness 收口）。

## 7. TUI 侧要做的（未提交，需要一次 rev bump）

本次已改 `src/app/mod.rs` / `src/app/session.rs`（**未提交**，TUI 仓另有他人在飞的
`src/terminal/agent.rs`、`src/ui/v2/fullscreen.rs` 改动，不要一并提交）。rev bump 时：

1. 带上这两个文件的改动；
2. 重建锚点 + bump `scripts/ci-linux.sh` 的 `QAQH_BACKEND_REV` + 给后端 rev 打 annotated
   tag（沿用 `tui-ringing-v2-*` 命名，旧 tag 不动）；
3. 同批把 §5.4 的 v2 e2e harness 从 v1 open 改成 v2 open + `session_attach`。

本次验证时用临时 `.cargo/config.toml` 把 `qaqh-client`/`qaqh-config-api` 指到本仓工作树，
测完已还原。
