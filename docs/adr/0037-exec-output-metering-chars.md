# ADR-0037 exec 输出计量 tokens→chars：本地 token 估算层整体退役

状态: `已采纳并实施` · 决策日期 2026-11-22 · 决策人 仓库所有者（明确指示："继续移除。我们依靠上游给定字段。当前协议很成熟了，都会下发标准字段给用户"）· 实施: 本批同落

## 背景

1. ADR-0036 移除了 tokenizers 库，但按当时决策保留了 `count_tokens` CJK 启发式
   （exec 截断计量面）与 `estimate_tokens`（`chars/4`）两个本地估算器。
2. 计数的权威事实源本就在上游：provider 响应携带标准 `usage`
   （`UsageInfo.prompt/completion/total_tokens`），session/goal 记账全部消费该字段；
   本地估算只服务于 exec 输出截断这一处，却让"token"这个概念在模型面参数、
   策略 trait、描述符、上下文请求五层扩散。
3. 工具结果字符上限已有先例：`TOOL_MODEL_MAX_CHARS = 24_000`
   （"limit is expressed in Unicode characters"）。

## 决策

1. **exec 输出计量统一 Unicode 字符口径**（与 `TOOL_MODEL_MAX_CHARS` 同律）：
   - 模型面参数 `max_output_tokens` → **`max_output_chars`**
     （exec 100..=200_000 缺省 32_000；exec_session 1..=200_000 缺省 32_000）；
   - `ToolResultFoldPolicy::exec_max_output_tokens` → `exec_max_output_chars`
     （StandardPolicy 缺省 10_000 tokens → 32_000 chars，×3.3 等值换算）；
   - `OutputBudget.exec_max_output_tokens`、`ExecCommandRequest`/
     `WriteStdinRequest.max_output_tokens`、`process_tools::hooks::set_exec_max_output_tokens`
     同批改名；`tool_job` 的 IO 字节上限 = chars × 4（UTF-8 最大宽度）。
2. **本地估算层删除**：`qaqh_types::token` 模块（CJK 启发式）删除；
   `ToolModelPayload.total_tokens` 字段与 `estimate_tokens` 删除
   （内部结构、无 ts 导出、旧持久化数据反序列化时未知键自然忽略）。
3. **上游字段面不动**：`UsageInfo`、`session.usage_totals`、goal 记账、
   gate/profile 的 `max_output_tokens`（LLM 请求参数语义）全部保留。
4. **wire 零变更**：`ToolModelPayload` 无 ts 导出，模型面工具 schema 非
   Ringing 冻结面——本批不触契约槽、不重切金标准、不 bump 版本（内核空间，
   规则 15 不适用）。`truncate.rs` 截断算法保持 70/30 头尾结构，仅计量基座
   从 token 估计改为精确字符数。

## 被否选项

1. **保留 `max_output_tokens` 命名但按字符计数**：语义谎报，宪法 4/5 不允许。
2. **保留启发式仅删 CJK 分支**：估算器失去存在理由（上游 usage 已覆盖记账），
   留壳不留核属兼容性架构。

## 附带发现（与本批无关，待单独归因）

在 HEAD `d3fb7c53` 上即失败的既有用例（worktree 复现，排除本批嫌疑）：
`exec::tests::sbx_auto_session_spec_unauthorized_write_is_enforced`（AUTO-spec
未授权写逃逸——Windows 沙箱强制失效，**安全相关，优先跟进**）、
workspace `authorization::*` ×3、`schema_spot_check::*`、
`execution::exec_progress_sender_reaches_the_exec_handler`。

## 落地清单

- [x] qaqh-types：token.rs 删除、lib.rs 导出、ToolModelPayload/estimate_tokens 删除
- [x] process-tools：truncate.rs char_truncate、hooks/direct/pty/sbx/handler/
      exec_session.tool 字符预算链
- [x] tool-core：context.rs 两 Request、descriptor.rs OutputBudget 改名
- [x] workspace：tool_side_fold trait/policy/32_000、registration.rs 注入点
- [x] agent-runtime：tool_job.rs 字节换算行
- [x] 定向验证：types/tool-core 全绿；process-tools 85/86（唯一失败为上述既有项）；
      workspace 128/133（5 失败均为既有项）
