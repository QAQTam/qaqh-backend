# CodeGraph 热点与工具侧联调 Handoff

> 日期：2026-09-25
> 基线：`2f362e0` / `2.0.0-alpha2`
> 状态：分析完成，未改生产代码
> 范围：`qaqh-runtime`、`qaqh-workspace`、`qaqh-session`、`qaqh-message`、`qaqh-gate`

## 1. 本次结论

本轮使用 CodeGraph 1.6.0 和 Cargo 依赖方向复核当前热点。结论是：

- v2 已经建立了更干净的下半身：canonical facts、projection、ToolLedger、typed tool、Ringing v2 wire。
- 旧运行时上半身仍然存在：`RingingHub`、`TimelineAppender`、重 `TurnEngine` 继续承担 v1 运行时职责。
- 当前不是 crate 环依赖或底层事实源缺失，而是 `qaqh-runtime` 内部新旧两套运行时并存。
- 工具侧联调的第一热点不是 `exec/`，而是
  `engine_tool -> ToolRuntime -> execution -> ToolManager -> ExecTool` 这条执行链。
- 现阶段不应先拆 `hub.rs`。删除旧 owner 比拆分旧文件更能降低实际复杂度。

一句话：

> v2 已解决 wire 和事实源层面的头重脚轻，但尚未完成 runtime ownership 层面的减重。

## 2. CodeGraph 基线

```text
CodeGraph version: 1.6.0
Files:             446
Nodes:             11,988
Edges:             47,251
DB size:           55.55 MB
Index:             up to date
```

原始 call graph 存在 Rust 解析误报，不能直接按调用次数判断架构：

- `MessageStore::clone` 被统计为大量跨文件调用；
- `Attempt::Ok` 被统计为 `qaqh-session -> qaqh-gate` 调用；
- `ToolRuntime::collect` 的 caller 范围被严重放大。

可信度排序：

1. Cargo 依赖方向；
2. CodeGraph import 边；
3. 同一模块内的 `callers/callees/impact`；
4. 原始文件级 call 次数仅作提示。

## 3. 当前热点

以下生产行数已排除同文件内的测试模块。

| 模块 | 生产行数 | 近月 churn | 判断 |
|---|---:|---:|---|
| `runtime/agent/engine_turn.rs` | 1905 | 32 | gate、tool cycle、interaction、compact、persistence 仍集中 |
| `message/store.rs` | 1830 | 17 | turn/context/compact 核心，高扇入 |
| `runtime/ringing/hub.rs` | 1673 | 24 | v1 三频道、journal、content、timeline 聚合 |
| `session/manager.rs` | 1509 | 21 | 生命周期、meta、WAL、seed、文件布局混合 |
| `runtime/registry.rs` | 1439 | 21 | actor、subagent、liveness、quota、shutdown |
| `gate/responses_api.rs` | 1344 | 11 | 单协议适配器，复杂度接近上限 |
| `runtime/timeline.rs` | 1239 | 16 | 独立 TimelineAppender/SSE journal |
| `daemon/axum_impl/v2.rs` | 1192 | 12 | v2 路由与命令处理 |
| `session/canonical/tool_ledger.rs` | 1188 | 13 | canonical 工具账本 |
| `runtime/agent/tool_runtime.rs` | 1115 | 14 | 工具侧核心执行边界 |
| `runtime/agent/engine_tool.rs` | 1087 | 20 | permission、UI、timeline、batch admission |
| `workspace/execution.rs` | 661 | 19 | workspace 工具执行总管线 |

`exec/` 生产代码约 3.6k 行，但按职责拆分为 handler、direct、pipe、shell、display、truncate，边界清晰，不是当前主要结构热点。

## 4. v2 目标与当前现实

v2 原目标要求建立四个 ownership 边界：

1. 持久事实与派生投影；
2. session 状态与连接状态；
3. turn 决策与 IO/工具执行；
4. canonical output 与多消费者投影。

当前状态：

| 目标 | 状态 | 说明 |
|---|---|---|
| canonical fact 是唯一事实源 | 已建立 | `qaqh-session/src/canonical`、`session_fact_v2` |
| projection 可重建 | 已建立 | `qaqh-session/src/projection` |
| Ringing v2 单流 | 已完成 | daemon/client wire 已硬切 |
| typed tool / ToolLedger | 已建立 | `tool_runtime.rs`、`canonical/tool_ledger.rs` |
| SessionActor 唯一 owner | 部分 | `TurnActor` 仍主要是既有 Loop 的 adapter |
| TurnCore 纯状态机 | 部分 | `TurnEngine` 仍承担 compact、interaction、gate、tool cycle |
| timeline 是 canonical projection | 部分 | canonical timeline projection 与旧 `TimelineAppender` 并存 |
| Ringing 只做 wire/projection | 未完成 | `RingingHub` 仍持有三频道、journal、router、content、timeline |

关键代码证据：

- `crates/qaqh-runtime/src/agent/mod.rs`
  - 仍注明主生产 Loop 是 Ringing V1 architecture。
- `crates/qaqh-runtime/src/ringing/hub.rs`
  - 仍聚合三频道 router、journal、projection、timeline、content store。
- `crates/qaqh-runtime/src/ringing/v2.rs`
  - `V2ProjectionHub` 是独立 canonical projection hub。
- `crates/qaqh-runtime/src/timeline.rs`
  - `TimelineAppender` 仍是独立 writer。
- `crates/qaqh-runtime/src/timeline_store.rs`
  - 仍使用 `LegacyWriterFacade`。

## 5. 工具侧联调入口

当前工具执行主链：

```text
ToolEngine::admit_batch
  -> turn_lap::admit::execute_admitted_batch
  -> ToolRuntime::execute_batch
      -> durable ToolIntent
      -> worker spawn
      -> execute_authorized_with_context
          -> ToolManager::prepare_req_with_cancel
          -> ExecTool::run
          -> exec/direct.rs + exec/pipe.rs
          -> ToolManager::finalize_req
      -> canonical ToolFinished
```

关键文件：

```text
crates/qaqh-runtime/src/agent/engine_tool.rs
crates/qaqh-runtime/src/agent/turn_lap/admit.rs
crates/qaqh-runtime/src/agent/tool_runtime.rs
crates/qaqh-workspace/src/execution.rs
crates/qaqh-workspace/src/manager.rs
crates/qaqh-workspace/src/exec/handler.rs
crates/qaqh-workspace/src/exec/direct.rs
```

联调时建议统一 trace 标识：

```text
session_id
turn_id
wire_call_id
canonical_call_id
execution_id
tool_name
```

至少覆盖三个时序点：

1. durable `ToolIntent`；
2. worker spawn 与 handler 进入；
3. canonical `ToolFinished` 或失败终态。

## 6. 建议顺序

### 立即执行

1. 完成工具侧真实联调。
2. 围绕 `ToolRuntime::execute_batch`、`execute_authorized_with_context`、
   `ToolManager::prepare_req_with_cancel` 增加诊断观测。
3. 不重构 `exec/` 目录。
4. 不先拆 `hub.rs`、`timeline.rs`、`engine_turn.rs`。

### 联调收尾后

1. `ToolRuntime` 不再接收 `&ToolEngine`。
   - 改为注入 progress/outcome sink。
   - 消除 runtime 执行边界对 engine 的反向耦合。
2. `execution.rs` 内部拆成 validate / audit / prepare / execute / finalize。
   - 保留一个公开入口，避免扩大接口面。
3. MCP 动态工具完全 typed 后：
   - 移除生产路径的 `LegacyToolAdapter`；
   - 收口 ambient `ToolManager`、mode、sandbox、fold policy 兼容层。

### v2 运行时收口

1. 将生产 timeline 读写切到 canonical timeline projection。
2. 将 `TimelineAppender` 降为纯 materializer，或彻底删除。
3. 删除无消费者的三频道 router/journal/sequencer/outbox。
4. 删除 `RingingHub` 的 v1 聚合职责。
5. 最后再评估文件拆分。

顺序不能反过来。先拆文件只会让旧 owner 看起来更整洁，不会真正减重。

## 7. 未决项

- interaction 跨 daemon 重启持久化仍是 P0。
- permission 正文 pin / 终态 unpin 未决。
- sandbox fallback 是 fail-closed 还是显式降级未裁决。
- `RingingHub` 旧三频道何时可安全删除，需要先证明无生产消费者。
- `TimelineAppender` 与 canonical timeline projection 的最终边界未裁决。
- `ToolRuntime` 的 progress sink 接口形态未定。
- `execution.rs` 是否改名为 `tool_execution.rs` 只是低优先级命名债。

## 8. 接手注意事项

- 先执行 `codegraph sync` / `codegraph status`，确认索引与工作区一致。
- 不要直接采用 CodeGraph 原始 call 次数；先过滤 `clone`、`Default`、枚举成员等误报。
- 工具侧联调优先观察 canonical `ToolIntent` 与 `ToolFinished`，不要从展示文本反推结果。
- 不得恢复 v1 wire 兼容。
- 不得新增第二份可写事实源。
- 不得为了目录整洁先做大规模文件拆分。
