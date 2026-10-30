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

已知预存在失败（与本次无关，HEAD 上同样失败）：`qaqh-permission --lib`、
`ask_user_lifecycle`、`concurrent_read_stress`、`host_direct`、`permission_lifecycle`。

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
