# 纯 v2：命令面切换 Handoff（2026-09-24）

状态：**已落地。** 承接 #345（content）、timeline、open/renew/service 三步。

## 1. 这次收掉的 v1 面

| 端点 | 处置 |
|---|---|
| `POST /ringing/v1/commands/{channel}` | **删除**（404）。客户端 / webui gateway 改走 v2 |
| `GET /ringing/v1/commands/{command_id}` | **删除**（404）。同上 |

`handle_command` / `command_fingerprint` 保留为 **v2 handler 的内部委托**（
`handle_command_v2` 归一成 v1 信封后调用它）；v1 的 `handle_command_status` 已删
（v2 status 直接读 pending store）。

## 2. 客户端

- `send_command` / `command_status` 改为委托既有 v2 实现
  （`send_command_v2_typed` / `command_status_v2_typed`）+ `.into_v1()`，
  **公开签名与返回类型不变**——既有壳层（TUI）零改动；
- 要读 typed `existing` / terminal `result` 的调用方直接用
  `send_command_v2_typed` / `command_status_v2_typed`；
- `CommandOptions.driver_epoch` 现在真正生效（v1 信封没有该字段，之前是空转）：
  带 epoch 会走 `stale_driver_epoch` 门，不带即 opt-out（交互应答就该不带）。

**顺带修掉一个潜伏 bug**：`send_command_v2_typed` 之前**没有发
`X-QAQH-Client-Session-Id` 头**（daemon 的 lease 判定只看 header，信封里的
`client_session_id` 不参与鉴权），所以这条路径一直会稳定拿 401
`lease_required`。它此前只是「平行 API、无人使用」，命令面切过来才暴露。

## 3. webui gateway

- `attach_seed` / approval / command 三个代理改为构造
  `RingingV2CommandEnvelope` 并发 v2；命令状态查询改 v2；
- 浏览器侧提交的仍是网关自己的 v1 形状信封（网关自有 API），转发前转成 v2 信封，
  身份字段照旧由网关覆写（不可信输入不入 wire）。

## 4. 验证

```text
cargo test --workspace -- --test-threads=1              PASS（0 failed）
cargo clippy --workspace --all-targets -- -D warnings   PASS
cargo fmt --all -- --check                              PASS
scripts/v2-smoke.sh <data-root>                         PASS（全阶段）
scripts/v2-content-probe.sh <data-root>                 PASS（#345 专项）
```

- 新增 daemon 硬切断言：`v1_open_renew_service_are_hard_cut` 扩到
  `POST /ringing/v1/commands/control` 与 `GET /ringing/v1/commands/{id}`；
- `session_attach_grants_seed_ownership_without_actor_side_effects` 改走
  `RingingV2CommandEnvelope` + v2 路径。

**真机（TUI 用本 rev 重建 + 本仓 daemon）**：

| e2e | 结果 |
|---|---|
| `e2e-new-session.sh`（session_create 走 v2 命令面） | PASS 4/4 |
| `e2e-alpha1-basic.sh`（conversation/tool 命令 + 工具链） | PASS 4/4 |
| `e2e-history.sh` | PASS 6/6 |

> 第一次跑 new-session / alpha1 全红（TUI 卡在「正在创建会话…」），根因就是上面那条
> 缺 header 的潜伏 bug；补头后全绿。这条值得记住：v2 平行 API 在被真正接上主路径
> 之前，不能假设它是通的。

重建后 TUI 二进制里的路径分布：

```text
v2 = clients/open · leases/renew · commands · service · content · sessions/*/timeline
v1 = events/{channel} · sessions/*/bootstrap        ← 只剩这两条
```

## 5. 仍是 v1 的面（剩余）

| 端点 | 说明 |
|---|---|
| `GET /ringing/v1/events/{channel}` | v1 三频道 SSE；v2 单流已存在。切过去要改 TUI 的事件模型（现在是「v1 域事件驱动」，v2 是 `ClientV2Payload` 增量），**最大一块** |
| `GET /ringing/v1/sessions/{seed}/bootstrap` | v2 bootstrap 已存在；切过去要改 SessionModel 装配（control/conversation/tool 三频道快照形态不同） |
| `GET /ringing/v1/sessions/{seed}/approvals` | v2 无对应端点，需设计（并入 v2 bootstrap 或新端点） |

## 6. TUI 仓的连带改动（未提交）

- e2e harness 里多处直接打 v1（open / commands / timeline），rev bump 时同批改 v2；
- `handle_control` 的 `#[allow(unreachable_patterns)] _ => {}` 兜底让两版 `ControlEvent`
  都能编译，rev bump 少一个卡点。
