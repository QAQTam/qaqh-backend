# Workspace 工具层契约重写（Tool SDK v1） - 实施计划（2026-09-15）

> 状态：**草案待评审**。
>
> 规格来源：
> [`docs/spec/2026-09-15-workspace工具层契约重写-spec.md`](../spec/2026-09-15-workspace工具层契约重写-spec.md)。
>
> 本计划只负责施工顺序、任务拆分和验收。若实现与 spec 冲突，先修改 spec 并重新评审，
> 再继续实施。

## 0. 元信息

| 项 | 值 |
|---|---|
| 计划日期 | 2026-09-15（UTC+8） |
| 涉及仓库 | 后端 `qaqh-backend` |
| 行号/代码基准 | `a72ce0c` 后的当前工作区；实施前必须重新核对行号 |
| 目标 crate | `qaqh-workspace`，外围涉及 `qaqh-types`、`qaqh-runtime`、`qaqh-mcp`、`qaqh-lsp`、`qaqh-subagent` |
| 一句话目标 | 先建立不可绕过的新 Tool SDK 和兼容适配层，再分批迁移所有工具，最终删除 legacy 错误与结果构造路径 |
| 执行原则 | 小步迁移、每批可回滚、每批有静态扫描门禁、不长期保留双实现 |

## 1. 现状与风险

### 1.1 当前实现

- `ToolHandler` 是唯一内置注册单元，参数为 `serde_json::Value`，返回 `ToolResult`。
- 工具 schema 手写在每个 `register()` 中。
- `ToolManager` 负责注册、allowlist、动态工具、inflight 和统计。
- `execute_authorized` 负责授权后执行、fold、审计和结果返回。
- `ToolExecMeta` 生成于 manager，但 runtime 未消费。
- `ToolResult` 同时承担模型、展示、错误和附件投影。

### 1.2 主要施工风险

| ID | 风险 | 影响 | 缓解 |
|---|---|---|---|
| R1 | 一次性改 19 个工具导致行为回归 | 高风险、无法定位 | 先做 adapter，再按批次迁移 |
| R2 | 新 SDK 与旧 runtime 的 ToolResult 漂移 | 模型/UI 表现不一致 | 单一 adapter 出口，禁止工具直构 wire 类型 |
| R3 | schema 与 typed Args 不一致 | 模型调用参数被运行时拒绝 | 新工具必须由 `JsonSchema` 生成 schema |
| R4 | 错误迁移改变 retryable/hint 语义 | 模型行为变化 | 建立旧 code 到新 kind 的显式映射表 |
| R5 | timeline 与 ToolFinished 双通道继续漂移 | TUI 展示不一致 | 所有投影来自同一 `ToolOutcome` |
| R6 | 动态 MCP/LSP 工具无法一次 typed 化 | 新 SDK 不能覆盖全部工具 | 动态工具先使用 JSON output adapter |
| R7 | 迁移期新旧 API 长期共存 | 复杂度固化 | 每批完成后删除对应 legacy 调用点 |
| R8 | Fatal 错误误分类 | 会话被错误终止或错误继续 | fatal 白名单审查 + runtime 测试 |

## 2. 关键决策

| ID | 决策 | 理由 | 放弃方案 |
|---|---|---|---|
| D1 | Tool SDK v1 先落在 `qaqh-workspace::tool_api`，暂不拆新 crate | 降低依赖图和迁移成本；边界稳定后再提取 | 立即新建 `qaqh-tool-sdk`，会扩大一次性改动 |
| D2 | v1 同步执行，不引入 async tool trait | 当前工具以同步为主，异步会放大迁移面 | 全量 async 化 |
| D3 | 保留 `admit -> AuthorizedToolCall -> execute_authorized` | 现有授权链路价值高，问题在 SDK 不在授权 | 重写权限链路 |
| D4 | wire 继续使用 `qaqh-types::ToolResult` | 保证 client/TUI 无需同步大改 | 立即发布 ToolResult v2 |
| D5 | 错误采用 `ToolErrorKind + detail + details + source` | 可机器分派，可诊断，模型信息可控 | 继续开放字符串 code |
| D6 | 增加 `FatalToolError` 与可恢复错误分离 | 避免运行时损坏被当普通工具失败 | 所有失败统一成 ToolResult |
| D7 | 输出使用 `ToolOutcome`，内部再适配到 ToolResult | 分离模型、展示和 metrics 投影 | 继续扩展万能 ToolResult |
| D8 | 进度使用统一 `ToolProgress` | 避免每个工具发明私有 progress JSON | 只保留 exec 专用 progress |
| D9 | 旧工具通过 `LegacyToolAdapter` 接入新入口 | 支持小步迁移和回滚 | 一次性重写全部工具 |
| D10 | `ToolExposure` 类型先落地，Deferred 行为后置 | 先固定扩展边界，不提前实现工具搜索 | 继续只靠 allowlist |
| D11 | `schemars` 作为 v1 schema 唯一生成器 | 防止 typed args 与手写 schema 漂移 | 继续手写 JSON Schema |
| D12 | Tool SDK 稳定前不拆新 crate | 当前首要问题是契约而不是依赖图 | 立即拆 crate 导致迁移面扩大 |

## 3. 阶段总览

```text
P0 基线与门禁
  - 建立行为基线、错误 code 清单、legacy 调用点清单

P1 Tool SDK 核心
  - descriptor / typed args / outcome / error / progress
  - 新 API 单元测试，不接生产工具

P2 Legacy 兼容桥
  - ToolHandler -> LegacyToolAdapter -> ErasedTool
  - ToolManager 改走 ErasedTool 执行入口
  - runtime 保持 ToolResult 行为不变

P3 运行信息与投影收口
  - metrics 进入 outcome / ToolFinished / timeline 适配
  - summary、failure、diff 从同一 outcome 生成

P4 低风险工具迁移
  - read / glob / grep / skills / ask / todo / journal

P5 文件修改工具迁移
  - edit / write / delete / apply_patch / copy_range / confirm_apply

P6 系统工具迁移
  - exec / process / web_fetch / read_image

P7 外部与动态工具迁移
  - subagent / MCP / LSP

P8 Legacy 清理
  - 删除 json_err/json_err_string/prefix 判定/旧 fold 名称表
  - 静态扫描与全量验证
```

## 4. 分阶段任务

### P0 基线与门禁

目标：迁移前固定现状，避免“改完才发现行为变了”。

#### P0-① 建立工具行为矩阵

产物：`docs/report/2026-09-15-workspace工具行为基线-report.md`

每个内置工具记录：

- 注册名称、category、risk、timeout。
- 参数 schema 关键字段。
- 成功结果形态。
- 主要错误 code。
- 是否发送 progress。
- 是否产生 diff、images、continuation。

验收：

- 19 个内置工具全部列出。
- 动态 MCP/LSP 至少记录注册路径和共同字段。
- 所有结论有 `file:line` 证据。

#### P0-② 建立错误 code 清单

产物：spec 附录或基线报告中的映射表：

```text
legacy code -> ToolErrorKind -> retryable -> hint policy
```

重点覆盖：

- `INVALID_INPUT` / `INVALID_ARGUMENTS` / `PARSE_ERROR`
- `NOT_FOUND` / `FILE_NOT_FOUND`
- `NO_MATCH` / `AMBIGUOUS_MATCH` / `HASH_MISMATCH`
- `CANCELLED` / `TIMEOUT`
- `IO_ERROR` / `WRITE_FAILED` / `READ_FAILED`
- `PERMISSION_DENIED`
- `TOOL_ERROR` / `INTERNAL_ERROR`

验收：

- 不存在未映射的活跃错误 code。
- 映射冲突必须先解决再进入 P1。

#### P0-③ 建立 legacy 调用点清单

命令：

```bash
rg -n "json_err\\(|json_err_string\\(|ToolResult::error\\(|ToolResult::error_data\\(|ToolResult::error_with\\(|handler_from_string!|\\[ERROR|\\[PARTIAL" crates/qaqh-workspace/src crates/qaqh-subagent/src
```

产物：按 P4-P7 批次归属的迁移清单。

验收：

- 每个命中点有目标批次。
- 不允许“待定”项。

### P1 Tool SDK 核心

目标：实现 spec 第 5-9 节的新类型，不接生产工具。

#### P1-① 新建 `tool_api` 模块

文件建议：

```text
crates/qaqh-workspace/src/tool_api/
  mod.rs
  descriptor.rs
  context.rs
  error.rs
  output.rs
  progress.rs
  typed.rs
  erased.rs
```

任务：

- 定义 `ToolName`、`ToolDescriptor`、`ToolExposure`、`ToolSource`。
- 定义 `TypedTool`、`ErasedTool`。
- 定义 `ToolError`、`ToolErrorKind`、`FatalToolError`、`ToolExecutionError`。
- 定义 `ToolOutput`、`ToolOutcome`、`ToolDisplay`、`ToolExecutionMetrics`。
  ⚠ 展示结构（`ToolDisplay/Header/Body/Metrics` 与 wire 类型）以
  [`spec/2026-09-18-工具结果展示契约-v1-spec.md`](../spec/2026-09-18-工具结果展示契约-v1-spec.md)
  §3–§4 为准；按 09-17 旧草案实现两字段 `ToolDisplay` 属返工路径。
- `ToolOutcome` 必须含可恢复错误的承载位（`error`）与 `ToolOutputValue`；
  `ToolError` 必须含 `code`。
- 定义 `ToolProgress`。

验收：

- 所有公开类型有文档注释。
- 新增类型不依赖具体工具模块。
- `cargo check -p qaqh-workspace` 通过。

#### P1-② descriptor 校验

实现：

```rust
impl ToolDescriptor {
    pub fn validate(&self) -> Result<(), DescriptorError>;
}
```

覆盖：

- 名称合法且非空。
- description 非空。
- input schema 为 object。
- timeout 非零。
- source/exposure 合法组合。

验收：

- 非法名称、空描述、空 schema、零 timeout 均失败。
- 错误包含具体字段。

#### P1-③ 错误构造器与分类

实现：

- `ToolError::invalid_arguments`
- `ToolError::not_found`
- `ToolError::conflict`
- `ToolError::execution`
- `ToolError::timeout`
- `ToolError::cancelled`
- `ToolError::network`
- `ToolError::unavailable`
- `ToolError::custom(namespace, code, detail)`

实现 fatal 构造器：

- `FatalToolError::invariant`
- `FatalToolError::runtime`
- `FatalToolError::authorization`

验收：

- `source` 不参与 serde。
- `Custom` 无命名空间时校验失败。
- fatal 无法直接转换为普通 ToolResult。

#### P1-④ ToolOutcome 投影

实现：

- typed output -> `ToolOutcome`。
- 默认 JSON text model projection。
- summary 有界。
- metrics 默认填充。

验收：

- 同一个 outcome 可稳定生成模型投影和展示投影。
- 投影测试覆盖空输出、长输出、带 diff、带 image。

#### P1-⑤ 进度类型

实现：

- Text progress。
- Phase progress。
- Content progress。
- Custom progress，subkind 必须非空。

验收：

- 单个 progress 可 serde 往返。
- 接收端无需工具名即可处理通用字段。

### P2 Legacy 兼容桥

目标：让生产执行先走新入口，但行为保持完全一致。

#### P2-① LegacyToolAdapter

实现：

```text
ToolHandler
  -> LegacyToolAdapter
  -> ErasedTool
```

要求：

- 使用旧 handler 的 `key/description/input_schema/risk/category/timeout` 构造 descriptor。
- 调用旧 handler，把 `ToolResult` 适配成 `ToolOutcome`。
- 旧错误 code 暂时映射到 `ToolErrorKind`。
- 不改变模型可见 ToolResult 输出。

验收：

- 适配器对同一输入输出与原路径 byte-equivalent（除内部 metrics）。
- 至少覆盖成功、错误、partial、backgrounded、cancelled。

#### P2-② ToolManager 改走 ErasedTool

修改：

- `handlers` 值类型改为 `Arc<dyn ErasedTool>` 或统一注册项。
- `prepare_req` 不再直接保存 `fn` 指针，改为保存 erased executor。
- allowlist、category、inflight、统计行为不变。

验收：

- 全量现有 `qaqh-workspace` 测试通过。
- `default_registry_exposes_the_formal_tool_vocabulary` 不变。
- `ToolManager::lookup` 行为对旧调用兼容，或在计划内完成调用点迁移。

#### P2-③ 动态工具桥

要求：

- MCP/LSP 工具通过同一 ErasedTool 接口注册。
- schema 与 description 继续来自 server。
- 碰撞和 refresh 语义不变。

验收：

- MCP 投影测试通过。
- LSP 投影测试通过。
- 动态工具执行结果与迁移前一致。

### P3 运行信息与投影收口

目标：解决 `ToolExecMeta` 生成后无消费者的问题，并消除 timeline 自行推导。

#### P3-① metrics 进入 ToolOutcome

迁移：

- manager 生成 elapsed、output bytes、args summary 的策略改为 execution layer 填充。
- `ToolExecMeta` 只作为 wire adapter 的兼容字段。

验收：

- 每个 ToolOutcome 都有 metrics。
- fatal 路径也有可诊断 metrics（至少 elapsed）。

#### P3-② ToolFinished 扩展

在 `qaqh-types::ToolResult` 或 ToolFinished 的兼容扩展区增加：

- `elapsed_ms`
- `output_bytes`
- `effective_tool_name`

要求：

- 旧 client 可忽略。
- 不把 args 正文塞进 wire。

验收：

- serde 向前/向后兼容测试。
- TUI 可通过 client 读取这些字段。

#### P3-③ timeline 单一投影

修改：

- `TimelineTool.summary` 使用 `ToolOutcome.display.summary`；缺失/非法时按
  09-18 契约 §7.1 的 legacy fallback 合成 `"{name} · {state}"`（live 与 rebuild
  共用同一函数）。
- `TimelineTool.failure.code` 使用 `ToolError.code`。
- `diff` 使用 `ToolOutcome.display.diff`，并与 legacy `TimelineTool.diff` 双写。
- timeline 投影函数负责 `project_display(&ToolDisplay) -> TimelineToolDisplay`
  的唯一映射（09-18 契约 §3.4）。

验收：

- H1 对全部既有工具成立（含未迁移工具），不再从 output 第一行构造 summary。
- ToolFinished 与 TimelineTool 的 status/failure 一致。
- display 进快照；旧快照（无 display）与未知 body 变体均有兼容测试。

### P4 低风险工具迁移

范围：

- `read`
- `glob`
- `grep`
- `skills`
- `ask`
- `todo_*`
- `journal`

每工具要求：

1. 定义 `Args` struct + `JsonSchema`。
2. 定义 `Output` struct/enum + `ToolOutput`。
3. 用 `TypedTool` 替换 handler。
4. 删除该工具的 `json_err` / 文本前缀错误。
5. 添加成功、参数错误、执行错误测试。

验收：

- 对应 legacy 调用点清零。
- 输出 byte-level 回归通过；如有意变更，必须在计划中记录。
- 工具 mode 过滤和权限 category 不变。

### P5 文件修改工具迁移

范围：

- `edit`
- `write`
- `delete`
- `apply_patch`
- `copy_range`
- `confirm_apply`

重点：

- dry-run pending 语义不变。
- file hash/stale 检测不变。
- diff 只进入 display projection。
- `NO_MATCH` / `AMBIGUOUS_MATCH` / `HASH_MISMATCH` 等迁移为结构化错误。

验收：

- 所有 apply_patch fixture 通过。
- edit 事务、冲突、hash 链测试通过。
- diff 不出现在模型投影。
- timeline diff 正常。

### P6 系统工具迁移

范围：

- `exec`
- `process`
- `web_fetch`
- `read_image`

重点：

- exec progress 使用 ToolProgress。
- backgrounded 不再被错误映射为失败。
- process 的 check/wait/write/kill 错误分类明确。
- web_fetch 网络错误与响应过大分类明确。
- read_image 的 image blocks 进入 ToolOutcome。

验收：

- exec pipe、background、cancel、timeout 测试通过。
- process registry 测试通过。
- web_fetch 超限测试通过。
- read_image 图片投影测试通过。

### P7 外部与动态工具迁移

范围：

- `spawn_subagent`
- MCP per-server 工具
- MCP aggregate tool
- LSP aggregate tool

要求：

- 外部工具仍可使用 JSON output adapter。
- 错误必须映射到 ToolErrorKind。
- 动态工具 refresh 不破坏 inflight 调用。
- subagent process registry 与 process 工具保持一致。

验收：

- MCP call path、resources、lifecycle 测试通过。
- LSP tool 测试通过。
- subagent in-process 测试通过。

### P8 Legacy 清理

目标：删除迁移期兼容面，完成门禁。

任务：

- 删除 `json_err` 和 `json_err_string`。
- 删除 `handler_from_string!`。
- 删除 `[ERROR]` / `[PARTIAL]` 前缀判定。
- 删除 `tool_side_fold` 的工具名称硬编码预算，改由 descriptor 提供。
- 将 `ToolResult::error/error_data/error_with` 降为 wire adapter 内部 API 或 `pub(crate)`。
- 删除无消费者的 `ToolExecMeta` 旧路径。
- 更新 README 和开发文档。

验收命令：

```bash
rg -n "json_err\\(|json_err_string\\(|handler_from_string!|\\[ERROR|\\[PARTIAL" crates/qaqh-workspace/src crates/qaqh-subagent/src
```

期望：仅测试 fixture 或明确兼容层允许命中；生产路径为零。

## 5. 测试矩阵

| 层 | 用例 | 类型 |
|---|---|---|
| tool_api | descriptor 校验 | 单测 |
| tool_api | typed args 反序列化 | 单测 |
| tool_api | error kind / fatal 分离 | 单测 |
| tool_api | outcome 三投影 | 单测 |
| tool_api | progress serde | 单测 |
| workspace | legacy adapter 等价 | 单测 |
| workspace | 每工具成功/参数错/执行错 | 单测 |
| workspace | 动态注册与碰撞 | 集成 |
| runtime | ToolFinished 与 Timeline 一致 | 集成 |
| runtime | fatal 不进入普通 tool result | 集成 |
| runtime | metrics 到达 client 投影 | 集成 |
| client/TUI | 新字段向后兼容 | 后续 TUI 接入验收 |

## 6. 每阶段统一质量门禁

每阶段结束必须执行：

```bash
cargo fmt --all
cargo check --workspace
cargo test -p qaqh-workspace
cargo test -p qaqh-runtime
cargo clippy --workspace --all-targets
git diff --check
```

阶段提交必须包含：

1. 本阶段迁移清单。
2. legacy 调用点减少数量。
3. 行为不变证据或有意变更说明。
4. 剩余风险和下一阶段入口条件。

## 7. 完成定义

Tool SDK v1 重构完成必须同时满足：

1. 所有内置工具使用 typed args 和 descriptor。
2. 所有工具错误进入结构化 taxonomy。
3. 不存在文本前缀控制流。
4. `ToolExecMeta` 不再生成后丢弃。
5. timeline、ToolFinished 和 audit 来自同一 canonical outcome。
6. 动态 MCP/LSP 工具遵守同一错误和输出适配契约。
7. `qaqh-workspace`、`qaqh-runtime` 全量测试通过。
8. 全 workspace clippy 通过。
9. 新工具接入文档完成，禁止项有静态扫描命令。

## 8. 排期建议

| 阶段 | 内容 | 估算 | 出口 |
|---|---|---|---|
| P0 | 基线、错误映射、调用点清单 | 0.5-1 人日 | 三份清单可执行 |
| P1 | Tool SDK 核心 | 1-2 人日 | 新类型测试通过 |
| P2 | Legacy adapter + ToolManager | 1-2 人日 | 全量行为不变 |
| P3 | metrics 与 timeline 收口 | 1-2 人日 | 双通道一致 |
| P4 | 低风险工具 | 1-2 人日 | 批次 legacy 清零 |
| P5 | 文件修改工具 | 2-3 人日 | fixture 全绿 |
| P6 | 系统工具 | 2-4 人日 | exec/process 回归全绿 |
| P7 | subagent/MCP/LSP | 1-3 人日 | 动态工具测试全绿 |
| P8 | legacy 清理与文档 | 1 人日 | 完成定义全满足 |

## 9. 关联文档

- Spec:
  [`docs/spec/2026-09-15-workspace工具层契约重写-spec.md`](../spec/2026-09-15-workspace工具层契约重写-spec.md)
- 现有 exec/process 评审：
  [`docs/report/2026-09-12-exec与process工具设计评审-report.md`](../report/2026-09-12-exec与process工具设计评审-report.md)
- Codex exec 对照：
  [`docs/report/2026-09-13-codex-exec设计对照与修订-report.md`](../report/2026-09-13-codex-exec设计对照与修订-report.md)
