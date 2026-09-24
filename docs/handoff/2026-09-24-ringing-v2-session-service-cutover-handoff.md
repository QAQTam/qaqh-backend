# 纯 v2：client open/renew/service 切换 Handoff（2026-09-24）

状态：**已落地。** 承接 #345（content）、timeline 切换两步，继续把客户端与 daemon
的 v1 面收掉。

## 1. 这次收掉的 v1 面

| 端点 | 处置 |
|---|---|
| `POST /ringing/v1/clients/open` | **删除**（404）。客户端改走 v2 open |
| `POST /ringing/v1/leases/renew` | **删除**（404）。客户端改走 v2 renew |
| `POST /ringing/v1/service/{method}` | **删除**（404）。客户端 / CLI / webui gateway 改走 `/ringing/v2/service/{method}` |

## 2. 客户端：一个 lease，不再是两条

改动前 `qaqh-client` 有两条并存的身份：`session.rs` 的 v1 lease 与 `v2.rs` 的
`ClientV2SessionState`（`connect_v2_async` = `connect_async` + `open_v2` ⇒ **两个
lease**）。现在：

- `RingingSession::open()` 直接打 `/ringing/v2/clients/open`，一次握手同时填充
  `SessionState`（header 身份）与 `v2_state`（capability / v2 端点）；
- `v2_session` 从 `ClientInner` 搬到 `RingingSession`（唯一身份来源）；
- `renew_once()` 打 `/ringing/v2/leases/renew`；
- `connect_v2_async` 不再额外 open（`connect_async` 本身就是 v2 握手）；
- `query()` / `action()`（typed service RPC）改走 `/ringing/v2/service/{name}`。

## 3. daemon

- 路由：删除 v1 open/renew/service 三条；`/ringing/v2/service/{method}` 注册（复用
  同一 handler，鉴权与 seed 归属语义不变）。
- `command.rs`：删除已死的 `handle_open` / `handle_renew` 与 `JsonResponse`。
- CLI（`qaqh-daemon todo ...`）：改为 v2 open → control `session_attach`（v2 open
  不再接受 `attach_seed`）→ v2 service；顺带修掉 `auto_discover_seed` 之前**不带
  session 头**调 `session.list`（必然 lease_required）的问题。

## 4. webui gateway

- 自身的 daemon 客户端（`daemon.rs`）：open/renew 改 v2（`RingingV2OpenRequest` /
  `RingingV2OpenResponse`）。
- 两个 service proxy 路径改 v2（commands / bootstrap / approvals / events 仍是 v1，
  见 §6）。

## 5. 验证

```text
cargo test --workspace -- --test-threads=1              PASS（0 failed）
cargo clippy --workspace --all-targets -- -D warnings   PASS
cargo fmt --all -- --check                              PASS
scripts/v2-smoke.sh <data-root>                         PASS（全阶段）
scripts/v2-content-probe.sh <data-root>                 PASS（#345 专项）
```

新增 daemon 测试 `v1_open_renew_service_are_hard_cut`（三条路径 404）；
`open_*` / `todo_*` 用例改走 v2（`todo_list_allowed_after_seed_attached` 现在走真实
的 `session_attach` 命令）；`qaqh-client` 的 lease-renew 超时/自愈用例改断言 v2 路径。

**真机（TUI 用本 rev 重建 + 本仓 daemon）**：

| e2e | 结果 |
|---|---|
| `e2e-alpha1-basic.sh` | PASS 4/4（open→命令→工具→回灌全链路） |
| `e2e-new-session.sh` | PASS 4/4 |
| `e2e-history.sh` | PASS 6/6 |

重建后的 TUI 二进制里的路径分布：v2 = `clients/open` / `leases/renew` / `service` /
`content` / `sessions/*/timeline`；v1 只剩 `commands` / `events` / `sessions/*/bootstrap`。

## 6. 仍是 v1 的面（剩余）

| 端点 | 说明 |
|---|---|
| `POST/GET /ringing/v1/commands/{id}` | 命令面（v2 命令面已存在，需带 `seed` + `driver_epoch`） |
| `GET /ringing/v1/sessions/{seed}/bootstrap` | v2 bootstrap 已存在，客户端切过去要改 SessionModel 装配 |
| `GET /ringing/v1/events/{channel}` | v1 三频道 SSE；v2 单流已存在，切过去是 TUI 事件模型改动（最大一块） |
| `GET /ringing/v1/sessions/{seed}/approvals` | v2 无对应端点，需要新设计或并入 v2 bootstrap |

## 7. TUI 仓的连带改动（未提交）

TUI 的 e2e harness 里有多处**直接**打 `/ringing/v1/clients/open`（`e2e-v2-interactions.sh`
`e2e-v2-alacritty.sh` `e2e-v2-faults.sh` `e2e-v2-real-terminal.sh` `e2e-v2-tmux.sh`
`e2e-v2-wezterm.sh` `e2e-lease-expiry.sh`），v1 硬切后这些 harness 会 404——rev bump
时必须同批改成 v2 open + `session_attach`。TUI 的 `app/mod.rs::handle_control` 已有
`#[allow(unreachable_patterns)] _ => {}` 兜底，所以对 v1/v2 两版 `ControlEvent` 都能编译。
