# 计划：移除全会话 driver 席位（第 1 批）

日期：2026-10-26。依据：`docs/analysis-multi-client-communication-2026-10-23.md` §1 与"建议遵守的边界"、`new-tech/docs/v2-reset/protocol.md`（"无全会话 driver seat"、PTY 局部 fence）。配套分析见 2026-10-26 会话审查记录。

**状态（2026-10-26 当日）**：第 1、2、3 批已执行完毕。后端（本仓）与 TUI（qaqh-tui-app）均已修改，均未提交待审阅。
第 1 批验证：`cargo check --workspace --all-targets` 通过（唯一报错为 `sbx-win` 既有平台门控问题，与本批无关）；
`cargo test -p qaqh-daemon -p qaqh-runtime -p qaqh-session -p qaqh-client --lib` 全绿（67/322/21/71），
`--test tool_ledger` 21/21；TUI `cargo check --all-targets` 通过、`cargo test` 591/591。
第 2 批验证：runtime lib 323/323（含新增 targeted-cancel 用例）；`--test {cancel_keeps_tool_results,ask_user_lifecycle,permission_lifecycle,session_lifecycle}`
中 20 项通过；`cancel_mid_batch_keeps_executed_tool_results` 在**未修改的 HEAD 基线（worktree 验证）同样失败**，
为既有环境问题（沙箱内 `sh` 工具零执行），与本批无关；daemon bins 83/83；TUI 592/592（含新增 turn_busy 草稿回填用例）。
第 3 批验证：runtime lib 326/326（含重写后的 lease_store 5 项 + boundary 用例）；daemon bins 83/83（authz 矩阵、
replay/403/401/吊销语义全部按新模型通过）；workspace check 仅余 `sbx-win` 既有平台问题。
`CARGO_INCREMENTAL=0`，测试各只跑一轮。额外小修：`device_registry.rs` 文档链接清理、bootstrap 的 `caller` 绑定简化、
`server.rs` 周期任务不再 clone `app_state`。

目标：连接活跃度与"是否抢到席位"不再作为读写授权条件；普通写请求由服务端按会话串行受理（actor 单队列天然串行）。分批落地，本计划执行第 1 批。

## 批次划分

- **第 1 批（已完成，2026-10-26）**：移除席位强制与全部申请/回收路径；canonical `DriverChanged` 停写、旧日志读取兼容；TUI 同批移除自动 claim、本地只读拦截与席位驱动的自动发送信号。
- **第 2 批（已完成，2026-10-26）**：多写防护，落地语义如下。
  - §6：挂起回合（审批/ask/plan 未决）期间普通用户发送改为显式 `turn_busy` 拒绝（不再 `abort_suspended` 隐式替换）；先应答/关闭交互或显式取消该回合后再发送。v3 邮箱式按会话排队落地后把拒绝升级为延迟受理。TUI 侧：发送被拒时回填**原草稿**（`last_rejected_send` 暂存，composer 为空才回填），错误经 toast 呈现。
  - §7：显式 `turn_id` 的取消不再在 registry 入队时预置取消标记/会话取消表——目标由 actor 校验（`current_turn_id()`），不符即 `cancel_target_mismatch` 拒绝且零副作用（原实现校验失败后仍 seal/remove/reset）。`turn_id: None`（取消当前回合）保留入队预置：回合运行中命令在 lap 边界才被 actor 看到，没有预置就没有回合中取消；其"指向到达时的当前工作"语义由 v3 精确绑定解决。wire 的 `turn_id: Option` 保持不变（TUI `cancel_turn` 依赖）。
- **第 3 批（已完成，2026-10-26）**：lease/attach 解耦，落地语义如下。
  - §2/§3/§4（`lease_store.rs` 重写）：**连接（lease）降级为在线 presence**（短 TTL、renew、过期 GC、吊销断流），不再是读写授权条件；**会话归属挂在稳定身份键名下**（device = `device_id`，admin = 自报 instance id），独立于任何单条连接寿命——重开连接（换 cs）、第二/第三个窗口互不失效，重连后凭原身份直接恢复读写，无需重放 attach；显式 `revoke_device` 摘除该身份下**全部**连接与归属，立即生效。归属无淘汰（跟随设备生命周期），容量治理留后续。SSE 逐事件复查保持（presence + 归属两层，`sse.rs` 同步更新）。
  - §5（`pending_store.rs` + daemon）：回执 owner 从临时 `client_session_id` 换成**稳定身份键**（`ReceiptOwnerScope::IdentityInstanceKey`）；换连接后同一 `command_id` 仍可对账、重复提交返回原回执，不同身份冒用仍被指纹守卫拒绝。对齐 v3 目标去重域 `(principal, session, command_id)`。
  - 契约同步：`tools/qaqh-contract-export/src/coverage.rs` 的 `owner_scope`/`fingerprint_guard` 文案更新（并修正 batch 1 遗留的 `driver_epoch` 指纹描述）；`receipt-policy.json` fixture 内容随之变化，SDK 仓（B 线）需用 `just sdk-export` 重新导出并跑 `sdk-check`。
  - 更新的测试：lease_store 5 项（多连接并存、重连免重放 attach、吊销全摘、身份隔离）、boundary `receipts_reconcile_across_reopen_for_same_identity_but_not_other_identities`、daemon `timeline_live_follows_identity_ownership`（取代 renegotiation 截断用例）、`v2_command_replay_*` 按身份键预置。2026-09-12 系列的「换身份清归属」防御被本批有意推翻（cs 单独泄露无害：请求仍需设备 token，身份绑定校验不变）。
- **wire 清理（最后）**：删除 `capabilities.drive`、bootstrap `driver` 字段、envelope `driver_epoch`、claim/release 响应类型与 domain 变体；随 v3 或 rollout.md 过渡执行，避免破坏已导出契约（B02/B03）。

## 第 1 批文件清单

后端（qaqh-backend-main）：

| 文件 | 改动 |
|---|---|
| `crates/qaqh-runtime/src/ringing/driver_watch.rs` | 删除文件（含 `<data_dir>/ringing-driver-watch.json` 持久化扫描表） |
| `crates/qaqh-runtime/src/ringing/mod.rs` | 去模块声明与再导出 |
| `crates/qaqh-runtime/src/ringing/v2.rs` | 删 hub `driver_state()` |
| `crates/qaqh-runtime/src/agent/loop_dispatch_control.rs` | DriverClaim/DriverRelease 分支改为拒绝回执（`driver_removed`）；删 handle_driver_claim/release、emit_driver_changed、driver_causation_id |
| `crates/qaqh-session/src/canonical/tool_ledger.rs` | 删 claim_driver/release_driver/append_driver_changed/driver_state 与 outcome 枚举、holder/epoch 字段；replay 对旧 DriverChanged fact 保持容忍 |
| `crates/qaqh-session/src/canonical/mod.rs` | 去相应再导出 |
| `crates/qaqh-session/tests/tool_ledger.rs` | 删 `driver_seat_epoch_is_monotonic_and_survives_reopen` |
| `crates/qaqh-daemon/src/axum_server.rs` | 删 reclaim 再导出、driver_watch 构造、全部 driver 测试与 fixture |
| `crates/qaqh-daemon/src/axum_server/axum_impl/mod.rs` | 删 driver 路由、AppState.driver_watch |
| `crates/qaqh-daemon/src/axum_server/axum_impl/v2.rs` | 删 driver/claim、driver/release handler、driver_gated/driver_admission、reclaim 扫描、bootstrap driver 计算（恒 None）、命令信封 driver 身份归一化 |
| `crates/qaqh-daemon/src/axum_server/axum_impl/service_api.rs` | 删 service_driver_rejection |
| `crates/qaqh-daemon/src/axum_server/axum_impl/command.rs` | 指纹去掉 driver_epoch；删对应边界测试 |
| `crates/qaqh-daemon/src/server.rs` | 删周期 reclaim 调用 |
| `crates/qaqh-client/src/v2.rs` | 删 SDK claim_driver/release_driver 与相关测试段 |

保留不动（读取兼容 / 保留清单）：`qaqh-ringing` wire 类型与 domain `ControlCommand::DriverClaim/Release` 变体（已导出契约，最后一批清理）；`FactPayload::DriverChanged`、validation、投影 `ControlDelta::driver_changed`（旧日志重放）；scope/challenge/`caller_lease_bound_to_identity`/`revoke_device`/幂等去重。

TUI（qaqh-tui-app，独立仓，同批落地）：

| 位置 | 改动 |
|---|---|
| `src/app/mod.rs` | 删 bootstrap 后自动 spawn_claim_driver、claim_driver API 包装、driver delta 消费与"席位到手补发草稿"信号、reject_if_v2_read_only 的 driver 分支（保留 reset-pending 只读）、相关测试 |
| `src/app/session.rs` | v2_is_read_only 仅保留 reset-pending 语义；删 v2_is_driver/v2_can_claim |
| `src/app/ringing_v2.rs` | 删 driver 字段、is_driver、apply_driver_state、driver delta 分支 |

## 验证策略（磁盘受限）

- `CARGO_INCREMENTAL=0`；编译检查一次 + 定点测试一轮，不做反复全量构建。
- 后端：`cargo check --workspace --all-targets` → `cargo test -p qaqh-daemon -p qaqh-runtime -p qaqh-session -p qaqh-client --lib --test tool_ledger`。
- TUI：`cargo check --all-targets` → 一次 `cargo test`。
- 未验证项如实记录；多端并发行为留给第 2/3 批联调。
- 验证完成后清理两仓 `target/` 构建缓存回收磁盘（本轮实测磁盘仅剩约 1 GB）。

## 风险与知会

- 删门控后 TUI 与桌面可同时写：挂起回合替换（§6）与无目标取消（§7）的暴露面扩大，属第 2 批范围，落地前不建议多端同时压测写路径。
- contract 文档中 driver claim/release 行（electron-sdk-backend-coverage.md、spec-electron-sdk.md）在 wire 清理批统一修订；本轮 wire 类型不变，导出契约不受影响。
- 两仓均不自动提交，待用户审阅。
