# Handoff — hub-fact-bus-refactor（阶段 1 + 2 完成；阶段 3 部分完成 + 扫尾）

> 任务来源：`docs/spec/hub-fact-bus-refactor.md`（领取自 `docs/legacy-compat-cleanup-draft.md` H1 条目）。
> 状态：**阶段 1、2 完成；阶段 3：b/c'/c''/e 完成并维持，§4.0.5 副作用迁移完成（a/c 的回滚前置已解除，a/c 现可安全重做），d 待做**。
> 全仓 `cargo check --workspace` 绿；runtime 309 / daemon 65 / subagent 15 测试全绿。
> 事实核查基线：2026-09-27 spec HEAD。

## 已完成（spec §2 对照）

| 条目 | 内容 | 结果 |
|---|---|---|
| 1.1 | `execute_command(state, headers, RingingV2CommandEnvelope) -> (StatusCode, RingingV2CommandAck)` 落在 daemon `axum_impl::command` | ✅ |
| 1.2 | `handle_command_v2` 直调 `execute_command`，删除 v2.rs 的 v1 信封重序列化圈（原 1000-1021） | ✅ |
| 1.3 | `forward_driver_command`（driver claim/release）改为直接构造 v2 信封并直调，删除 v1 信封构造（原 630-653） | ✅ |
| 1.4 | 删除 `command.rs` 的 v1 `handle_command` 与 `ack_response`；`mod.rs:54` re-export 改为 `{command_fingerprint, execute_command}` | ✅ |
| 1.5（部分） | `cargo check --workspace` ✅；`cargo test -p qaqh-daemon --bins` 66 passed ✅ | ⏳ 仅剩手工冒烟未执行 |

## 设计决定（与 spec 的偏差说明）

1. **入口落在 daemon 而非 `RingingHub`**：spec 1.1 字面写"在 RingingHub 上补入口"，但
   `RingingHub`（qaqh-runtime）不持有 leases/pending/service（都在 daemon `AppState`）。
   按"最小 diff"落在 `axum_impl::command::execute_command`，签名与 spec 语义一致
   （收 v2 信封、吐 v2 ack、不含 HTTP body 组装）。阶段 3 抽传输中立核心时再上移。
2. **ack 形状统一为 v2**：原 v2 命令端点 dispatch 路径返回的是 v1 形状 ack（经 v1 handler）。
   现在 execute_command 全路径吐 `RingingV2CommandAck`（多 `existing` 字段，缺省 null/省略）。
   `forward_driver_command` 的响应体随之从 v1 ack 变 v2 ack——消费方只判 `status().is_success()`，无影响。
3. **lease 校验保留在 execute_command 内部**（不变量 4"命令入口"）；无 lease 时返回
   401 ack（code `lease_required`），不再走 v1 的裸 `lease_required_json` 体。
4. **幂等指纹现在计入 driver_epoch**：v1 handler 传 `None`，现传 `envelope.driver_epoch`，
   与 v2 端点 `existing_v2_receipt` 的指纹算法对齐（原两条路径不一致）。
5. `parse_channel` 降级 `#[cfg(test)]`（v1 路径频道解析仅剩单测消费；阶段 3 退役）。

## 文件清单

- `crates/qaqh-daemon/src/axum_server/axum_impl/command.rs` — 重写：v2 ack 构造器
  （`reject_ack`/`accept_ack` 改吐 v2）+ `execute_command`（原 handle_command 逻辑逐支迁移，
  含 test_hooks / SessionClose / Archive·Unarchive·Delete / SessionCreate·Resume·Attach /
  generic worker dispatch）；删 `handle_command`、`ack_response`。
- `crates/qaqh-daemon/src/axum_server/axum_impl/v2.rs` — `handle_command_v2` 尾部直调；
  `forward_driver_command` 构造 `RingingV2CommandEnvelope`；导入删 `RINGING_VERSION`/
  `RingingCommandEnvelope`。
- `crates/qaqh-daemon/src/axum_server/axum_impl/mod.rs` — re-export 更新；qaqh_ringing
  导入换 v2 类型；`parse_channel` 改 test-only re-export。
- `crates/qaqh-daemon/src/axum_server/axum_impl/auth.rs` — `parse_channel` 加 `#[cfg(test)]`。
- `crates/qaqh-daemon/src/axum_server.rs` — 新增 `command_entry_tests` 模块（§5.2 的 5 个
  不变量测试），以及文件清单外无其它改动。

## 验证证据（阶段 3 + 扫尾，最终）

- `cargo check --workspace` → exit 0。
- `cargo test -p qaqh-runtime --lib` → **309 passed / 0 failed**
  （probe_output 两测 `#[cfg(unix)]` 门控，Windows 存量环境失败消除；
  orphan/seal 旧形状测试 ×3 退役）。
- `cargo test -p qaqh-daemon --bins` → 65 passed；`-p qaqh-subagent --lib` → 15 passed。
- 扫尾：pending_store v1 折叠残端（`observe_terminal_event`/`terminal_result`/
  3 测试）删除——fact 链 `observe_projection_events` 为唯一折叠入口；
  typed `existing` replay 能力随之彻底移除（降级已记录于下）。

## 验证证据（阶段 2）

- `cargo test -p qaqh-subagent --lib` → 15 passed；daemon --bins 66 passed（后为 65，
  retired replay 测试 -1）。
- §2.5 grep：`RingingEventEnvelope|EventBatch` 在 daemon/subagent 运行时代码为零；
  `subscribe_channel|hub.subscribe(` 在 daemon/subagent 为零。

## 验证证据（阶段 1）

- `cargo check --workspace` → exit 0（qaqh-daemon 无警告；qaqh-mcp 的 unused variable
  警告为存量，与本改动无关）。
- `cargo test -p qaqh-daemon --bins` → **66 passed / 0 failed**（61 存量 + 5 新增
  `command_entry_tests`）。
- `command_entry_tests`（spec §5.2，5 个不变量）：无 lease 头 401 / 非活跃 lease 401 /
  缺 session_id 400 `missing_session_id` / 同 command_id 同 payload 重放 accepted /
  同 command_id 异 payload 409 `duplicate_command_mismatch`。
- 基线存档（§5.1）：`cargo test -p qaqh-runtime --lib ringing::hub` → **47 passed**（动手前）。

## 不变量自查（spec §1 五条）

1. 因果序 / 2. 游标重放 / 3. 幂等回执 / 5. durable-before-publish：未触碰事件总线与
   pending store 语义，仅迁移执行路径。3 的守门测试在 daemon --bins 内随迁全绿。
4. lease 归属校验仍在命令入口（execute_command 首两段）。

## §4.0.5 副作用迁移（2026-09-28，本阶段唯一实质前置 ✅）

publish 内隐式副作用已全部迁出到**事件产生侧**，publish 现在只保留广播 + journal
语义——阶段 3d（删 publish）不再有任何前置：

- **落点**：actor 桥 `publish_worker_event` 的 `WriterEvent::Ringing` 分支
  （`registry::apply_interaction_side_effects`，worker 事件的唯一汇聚点，
  覆盖主会话与 subagent）；daemon `execute_command` 只转发命令、不产事件，
  respond 分支无副作用可迁（handoff 原"候选落点"据此修正）。
- ① live_interactions 登记/解除：`hub.register_live_interaction` /
  `unregister_live_interaction`（新增 pub 方法，条件删除语义保留）。
- ② 交互正文 pin 释放：`release_interaction_content` 改 pub；actor 桥按
  Resolved / ToolFinished(canonical_interaction_id) 分支调用；orphan_seal
  两处补终态（orphan tool 的 ToolFinished、orphan interaction 的 Dismissed）
  显式补调（原依赖 publish 的 match 触发）。
- 登记时点从「journal append 后」提前到「publish 前」——关闭了原先
  「publish 前 1ms 无登记」的残余窗口，"ask 弹不出"守卫更强。

### 验证证据

- `cargo check --workspace` → exit 0（qaqh-runtime 无警告，仅存量 qaqh-mcp/
  qaqh-sandbox 警告）。
- `cargo test -p qaqh-runtime --lib` → **309 passed**（hub 47 不变量随迁全绿）。
- 交互/权限生命周期集成测试：ask_user_lifecycle 16 / permission_lifecycle 11 /
  plan_review_hook 4 / interaction_request_ledger 1 / interaction_body_content_id 1 /
  interaction_body_permission_content_id 1 / tool_crash_recovery 3 /
  session_inprocess 6 / subagent_inprocess 13 / cancel_keeps_tool_results 3 →
  共 56 passed。
- `cargo test -p qaqh-daemon --bins` → 65 passed；`cargo test -p qaqh-subagent
  --lib` → 15 passed。

### 新发现的存量问题（与本迁移无关，HEAD 基线复现）

- `cancel_keeps_tool_results::cancel_mid_batch_keeps_executed_tool_results`：
  「取消点没有落在工具批中途（无任何工具执行）」前置断言失败；git stash 回
  HEAD 后同样失败——环境相关（取消时序未落进工具批），非本迁移引入。
- `qaqh-runtime --test host_direct` 编译失败：`CollectorBatch` 字段失配
  （现存 `session_id`/`events`，测试用 `channel`/`envelopes`）——阶段 2 改
  `qaqh-subagent/host.rs` 后该集成测试未跟随更新，`--lib`/check 均不覆盖它。
  建议 §5.4 扫尾时一并处理。

## 遗留 / 下一步（按优先级）

1. ~~把 publish 内隐式副作用迁出（spec §4.0.5）~~ **✅ 已完成**（见上节；
   a/c 的回滚前置解除，可按原清单重做：actor.rs 删 `publish_with_causation`、
   orphan_seal 补终态改 fact append）。
2. 副作用迁出后：d（hub.rs 删 `publish/publish_with_causation/subscribe/
   subscribe_channel/replay_*` + journal 事件持久化，**保留** timeline 族与
   snapshot）、§5.4 把退役的 4+3 个测试重挂到 fact 面。
3. 遗留债：orphan_seal 4 类补终态的 fact 补写（§4.0.4）；
   SessionActivityChanged 的 fact 产生侧（活动推送现为查询轮询）；
   typed `existing` replay 的 fact 侧重建（可选）。
4. 手工冒烟（阶段 1/2 均未做）：webui 发命令 → ack → 事件到达。
5. timeline 归属决策（spec §6 独立决策点）：`timeline_hub` 是否并入 fact 总线。

## 风险

- v2 命令端点 401/400 类响应体形状变化（裸 JSON → v2 ack JSON）：TUI/webui 若有对
  错误体形状的硬断言需回归（daemon 测试与既有硬切断言均绿，静态面无引用）。
- `execute_command` 内 `envelope.validate()` 与 v2 handler 的 validate 重复执行一次
  （driver 路径需要；HTTP 路径冗余但便宜），阶段 2 可收敛。
