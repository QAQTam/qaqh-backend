# qaqh-runtime 拆分（2026-10-30）

状态：已完成，**未经编译验证**（本轮按约定不跑 cargo；验证命令见文末）。

## 结果

| crate | 行数 | 职责 |
|---|---:|---|
| `qaqh-runtime` | ≈11.6k | `registry/`（agent 生命周期登记）+ `host_impl`（`qaqh-subagent` 三个宿主 trait 的进程内实现）+ `service/`（`QaqhService` 与强类型 RPC 分发）+ `ringing/service_methods`（方法表 / DTO / dispatch）+ `subagent_*` / `agent_catalog` / `error` |
| `qaqh-agent-runtime` | ≈10.2k | `agent/`（`AgentState`、会话生命周期、系统提示词、工具恢复）+ `session_agent/`（驱动循环）+ `provider_job` / `tool_job` / `quota_ledger` / `activity` |
| `qaqh-ringing-runtime` | ≈5.7k | `v2` / `content_store` / `pending_store` / `lease_store` / `hub` / `projection` / `device_registry` / `compact_mirror` / `operation_failure` / `attachment` + `agent_graph` |

拆分前后都**没有环**：`runtime → {agent-runtime, ringing-runtime}`，两个新 crate 互不依赖，
且都不回指 `qaqh-runtime`（搬运前用引用计数核对过：agent 簇对 core 的 `crate::` 引用为 0，
ringing 簇只有 `QaqhService` 两处，全部落在留在 core 的 `service_methods` 里）。

## 路径兼容（daemon 与测试零改动）

`qaqh-runtime/src/lib.rs` 保留了同名重导出：

```rust
pub use qaqh_agent_runtime::{activity, agent, provider_job, quota_ledger, session_agent, tool_job};
pub(crate) use qaqh_ringing_runtime::agent_graph;
pub use qaqh_agent_runtime::SessionActivityTracker;
pub use qaqh_ringing_runtime::RingingHub;
```

`qaqh-runtime/src/ringing/mod.rs` 变成薄壳：

```rust
pub use qaqh_ringing_runtime::*;
pub mod service_methods;
```

因此下列路径全部不变：

- `qaqh_runtime::ringing::{V2ProjectionHub, RingingLeaseStore, PendingCommandStore,
  DeviceRegistry, Scope, hydrate_attachment_previews, pending_store::RECEIPT_POLICY, ...}`
- `qaqh_runtime::{QaqhService, AgentRegistry, RingingHub, detect_os_info, detect_shell,
  cache_system_path}`
- `qaqh_runtime::{agent::…, session_agent::…, provider_job::…, tool_job::…, quota_ledger::…}`

## 顺带清理

`agent/` 里 10 个零引用模块（约 2.4k 行）随拆分删除：`workspace_audit`、`context`、
`engine_title`、`lifecycle_port`、`compaction_port`、`plugins/`（compact / misc /
dashboard）、`request_context`、`resource_publish`、`stream_continuation`。
`agent/mod.rs` 里仍在描述 `loop_core` / `spawn` / `loop_*` / `actor` 的过期文档同步重写。

## 依赖变化

`qaqh-runtime` 去掉：`qaqh-policy`、`qaqh-sandbox`、`qaqh-skills`、`qaqh-spy`、
`qaqh-title`、`chrono`、`ureq`、`unicode-normalization`（全仓 0 引用）；
`qaqh-gate` / `qaqh-message` 降为 dev-dependencies（仅测试用到）。

> 注：**拆分不等于依赖总数变少**。`qaqh-runtime` 仍有 ~16 个内部依赖（RPC 面本来就宽），
> 收益是重编译半径：改 agent 只重编 10k，改 ringing 只重编 5.7k。

## 验证（待跑）

```bash
cargo check -p qaqh-ringing-runtime -p qaqh-agent-runtime
cargo check -p qaqh-runtime --all-targets
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets
cargo test -p qaqh-agent-runtime -p qaqh-ringing-runtime
cargo test -p qaqh-runtime --lib && cargo test -p qaqh-runtime --tests
```

已知预存在失败（与本次无关，HEAD 上同样失败）：`qaqh-permission --lib`、`host_direct`。
（`ask_user_lifecycle` / `concurrent_read_stress` / `permission_lifecycle` 已随旧状态机
测试一并删除，见下一节。）

## 2026-10-30 P0 死代码清理（拆分后审计）

静态审计（脚本 + 逐条核实，未跑编译）确认以下内容无生产调用者，已删除：

| 位置 | 行数 | 判定依据 |
|---|---:|---|
| `crates/qaqh-title/`（整 crate） | 185 | `cargo metadata` 无任何依赖者；`qaqh_title::` 在 crate 外 0 命中 |
| `qaqh-workspace/src/model_tool_plan.rs` | 687 | 唯一入口 `runtime::build_model_tool_plan` 全仓 0 调用者 |
| `qaqh-workspace/src/tool_api/boundary.rs` | 168 | 自述「尚未接线生产路径」；8 个类型 crate 外 0 引用 |
| `qaqh-runtime/src/subagent_recovery.rs` | 508 | 除 `lib.rs` 声明外 0 引用 |
| `qaqh-runtime/src/test_hooks.rs` | 48 | 同上；编译期长期 `never used` |

`crates/qaqh-import/`（3,611 行，外部 transcript 导入）**保留源码但不参与构建**：
移入 `[workspace] exclude`，接线时移回 `members`。

### P1 · 原生工具计划默认不编译（2026-10-30）

`native-tool-plan` 子系统**只被测试驱动**（生产走 `chat_stream`，永远不带 plan），
现已用 feature 关掉默认编译：

- `qaqh-tool-core`：`[features] native-tool-plan = []`，`pub mod model_tool_plan`
  （885 行）挂 `#[cfg(feature = "native-tool-plan")]`；
- `qaqh-gate`：转发同名 feature，`ModelToolPlan` re-export、
  `chat_stream_responses_with_plan`、`chat_stream_responses_plan`、
  `plan_declarations`、`adapt_native` 全部 gated；
- feature 关闭时 `responses_sdk` 用**不可构造的占位 `ModelToolPlan`**，
  于是 `consume` / `run_sdk_stream` 签名不变（`plan` 恒 `None`），
  `adapt_native` 换成 fail-closed stub——**行为与关闭前完全一致**
  （原生调用没有 plan 一律报错）；
- 测试：`codex_protocol_fixtures` 4 个 plan 用例与 `responses_sdk/tests.rs`
  2 个 plan 用例挂同一 cfg；`qaqh-runtime` 的 dev-dep 打开该 feature
  （`ask_user_lifecycle` 直接驱动输入适配器）。

打开方式：`cargo test -p qaqh-gate --features native-tool-plan`。
接线生产时把 `chat_stream` 换成 `chat_stream_responses_with_plan` 并常开 feature。

未处理的同类问题（P2，见审计报告）：

- `qaqh-lsp` / `qaqh-mcp` 孪生文件树（12 个同名同体函数）、`qaqh-dsaccount` / `qaqh-siwc`
  的 `util.rs`+`loopback.rs` 复制——待抽公共 crate；
- 服务面缺契约 crate：`client/src/service_types.rs` 手抄 siwc/dsaccount 响应，
  服务端 `service_methods.rs` 54 处 `json!` 手拼——待建 `qaqh-service-contract`。

### 2026-10-31 · 单消费者 crate 合并 + 设计债第一批

**合并（成员 37 → 35）**

| 原 crate | 去向 | 理由 |
|---|---|---|
| `qaqh-git`（251 行） | **整体删除** | 先并入 `qaqh-workspace/src/git.rs`，同日确认桌面端从未接线该面板后连同 `git.*` 六个服务方法与 client `Endpoint` 变体一并删除（含 `fs_git::git` 助手与 workspace 的 `git2` 依赖） |
| `qaqh-fs-core`（1,258 行） | `qaqh-file-tools`（`file_cache`/`file_shared`/`file_state`） | 仅被 file-tools 与 workspace 使用，且 file-tools 本就再导出它 |

**设计债（第一批）**

1. `service_methods.rs`：31 对手写 `validate_*_params` / `validate_*_result` 转发函数
   （每加一个方法抄两行）→ `validators!` 宏一次性生成，类型清单即文档。
2. `service_methods::dispatch`：去掉重复的 params 解码。`Command::decode`（`command.rs`
   的 `*Args` 全是本文件 `*Params` 的别名）已做同样严格的反序列化，此前同一份 JSON
   被解两次、方法表被查两次。结果侧校验保留（handler 产出 `Value`，无 DTO 兜底）。
3. `execution.rs`：`execute_with_context` 不再经 `execute_authorized`（ambient 适配器）
   把刚装进线程局部的上下文读回来，改为直接构造 `ToolCallContext` 调
   `execute_authorized_with_context`；`execute_authorized` 降级为 `#[cfg(test)]`。
4. 线程局部/全局态迁移**写成方案**：`docs/plan-tool-runtime-context.md`（S1-S4，
   含验收命令）。这是唯一需要改 `qaqh-permission` 公共 API 的改动，未实施。

### 2026-10-31 · 移除 `qaqh-spy`（判定为失败作品）

`qaqh-spy`（2,709 行）是工作区文件变动审计 + 紧急备份。它的模型面入口
`spy` 工具**早已退役**（`registration.rs` 里只剩一行注释），生产上仅剩两条线：
`exec_audit`（exec 调用前后的双扫归因）与 daemon 启动时的一行 GC。

删除范围：

| 位置 | 行数 | 说明 |
|---|---:|---|
| `crates/qaqh-spy/` | 2,709 | 整 crate（含 e2e 测试） |
| `qaqh-workspace/src/spy_tool.rs` | 273 | 未注册的模型工具入口 |
| `qaqh-workspace/src/exec_audit.rs` | 191 | exec 归因双扫；其唯一产物 `ToolExecResult::attributed_paths` **全仓无人读取**，字段一并删除 |
| `qaqh-daemon/src/main.rs` | 3 | 启动 GC 调用 |

`qaqh-message` 的 `workspace` 上下文注入槽位**保留但注明无生产者**：它是通用的
注入通道（`USER_INJECTION_SOURCES` 与 store 侧测试按名引用），不属于 spy。

## 二轮清扫 · 旧 Loop 状态机（≈9.1k 行）

### 判定依据

`AgentState::new` / `AgentState::init` 在**生产代码里 0 次构造**：拆分后的
`crates/qaqh-runtime/src/**` 只有两处 `crate::agent::state::agent::…` 引用，且都指向
`agent_tool_registrars()`——函数体是 `[ToolRegistrar; 0]` 空数组，生产上的全部作用是
给 `build_tool_manager` 传 `&[]`。其余 `&mut AgentState` 入口只在旧状态机内部或集成
测试里被调用。于是整簇（状态 / 生命周期 / 引擎协议 / 工具对账）可一次删除。

### 删除清单

| 位置 | 行数 | 说明 |
|---|---:|---|
| `agent/state/` | 3,320 | `agent.rs`(1,728) / `lifecycle.rs`(1,113) / `token_calibration.rs`(462) / `mod.rs`(17) |
| `agent/tool_recovery.rs` | 512 | 唯一生产调用点是已死的 `lifecycle::init_session` |
| `agent/engine_session.rs` | 254 | 只保留 `session_effective_config` → `agent/session_config.rs`(43) |
| `agent/util/datetime.rs` | 36 | `epoch_to_date` / `chrono_local_*` 只被旧状态机调用 |
| `agent/util/format.rs` | 145 → 26 | 只留 `canonical_interaction_id`（registry 依赖） |
| `agent/types.rs` | 700 → 271 | 只留 `CancelToken` |
| `agent/prompt.rs` | −9 | 删 `system_prompt_for_mode`（唯一调用点在已死生命周期里） |
| `registry/spawn.rs` | −2 | `agent_tool_registrars()` → `&[]` |
| `qaqh-runtime/tests/` | 4,223 | 9 个只驱动旧状态机的集成测试 + `common/` 夹具 |

`agent/types.rs` 删掉的协议类型：`RingContext` / `SessionBundle` / `StatsCollector` /
`Outcome` / `YieldReason` / `TurnState` / `PendingAsk` / `PendingPlan` /
`PendingTodoActivation` / `AdmittedTool` / `Emitter` / `WriterEvent` / `WorkerCommand` /
`LoopPhase` / `PendingState`。生产者与消费者全在旧状态机里；薄驱动 `session_agent`
用自己的 `AgentControl` / `AgentOutcome`。

### 被删测试

`ask_user_lifecycle`(1,327)、`permission_lifecycle`(859)、`session_lifecycle`(570)、
`restart_prefix_cache`(283)、`mcp_env_block`(245)、`restart_request_body`(189)、
`restart_request_body_responses`(172)、`concurrent_read_stress`(150)、
`tests/common/`(428：`spawn_pipe_loop` 等旧 Loop 夹具)。

理由：它们的主体是 `AgentState::build_context()`（请求体 / 前缀缓存）、旧 permission 与
ask_user 流程、以及 `reconcile_store_in` 的修复注记——生产上已无对应实现，留着只会测
尸体。覆盖应重建在薄驱动上：现存 `session_agent_test.rs`、`session_inprocess.rs`、
`subagent_inprocess.rs`、`ringing_architecture.rs`、`v2_acceptance_matrix.rs`。

`tool_crash_recovery.rs` **保留**：只摘掉 4 条依赖 `reconcile_store_in` 的注记断言
（崩溃窗口、副作用不重放的断言仍在；canonical 账本侧另有
`qaqh-session/tests/recovery_executor.rs` 全量覆盖）。

### 顺带

- `agent_tool_registrars()` 删除——返回空数组的「聚合点」没有聚合任何东西。
- 4 处悬空文档引用同步修正：`agent/prompt.rs`（`AgentState::build_context`）、
  `qaqh-workspace/src/web.rs`（`refresh_tool_defs`）、
  `qaqh-workspace/tests/tool_fold_scope.rs`（`apply_tool_mode` 指向已删文件）、
  `qaqh-session/src/manager.rs` / `qaqh-memwatch/src/lib.rs`。

### 验证（本轮同样未编译）

已做：全部改动文件 `rustfmt --edition 2024 --emit stdout` 解析通过；全仓 grep 确认
`AgentState` / `agent::state` / `tool_recovery` / `engine_session` /
`agent_tool_registrars` 无残留引用（仅历史说明文字）；`git diff --stat` 35 files
+62 / −9,059。
