# ADR 2026-10-27：OAuth 路由的 Responses 原生 freeform apply_patch

- 状态：Accepted（设计冻结；分阶段实施）
- 日期：2026-10-27
- 范围：DeepSeek 账号、ChatGPT 套餐（SIWC）两条 OAuth 路由；BYOK 路由不变
- 相关：[Codex 工具协议对齐 spec](../codex-tools对齐/spec.md)、[实施契约](../codex-tools对齐/contract.md)、[验收清单](../codex-tools对齐/checklist.md)

## 1. 背景与已核对事实

### 1.1 DeepSeek Responses

2026-10-27 抓取 `https://api-docs.deepseek.com/zh-cn/guides/responses_api` 与
`https://api-docs.deepseek.com/zh-cn/api/create-response`：

- `tools` 支持 `function`；`custom` **仅支持** `{"type":"custom","name":"apply_patch"}`，
  其他 custom 名称返回 400；`web_search` 仅 `deepseek-v4-pro` 支持。
- `input` 支持 `function_call` / `function_call_output` / `custom_tool_call` /
  `custom_tool_call_output`，并明确 `custom_tool_call` 与 `custom_tool_call_output`
  配合 `apply_patch` custom 工具使用，含 `call_id` 配对校验。
- 流式事件包含 `response.custom_tool_call_input.delta` / `.done`。
- 输出 item schema 列出 `custom_tool_call` 的 `id/status/call_id/name`，但**没有
  `input`**；`input` 只在 `custom_tool_call_input.delta` / `.done` 里。因此 SDK 不能把
  completed item 的 `input` 当必填，必须回退到 delta/done 累积的 snapshot。
- `store` 恒为 `false`；`previous_response_id` / `conversation` 不支持；
  `parallel_tool_calls` 忽略（始终开启）；`include` / `prompt_cache_key` 不支持；
  `reasoning.summary` / `encrypted_content` 不支持。
- 两页均**没有出现** `namespace` 或 `grammar`。

DeepSeek 的 Codex 集成页（`https://api-docs.deepseek.com/zh-cn/quick_start/agent_integrations/codex`）
在模型目录中明确使用 `"apply_patch_tool_type": "freeform"`、`"use_responses_lite": false`，
`wire_api = "responses"`。这说明 DeepSeek 把 Codex 的 freeform `apply_patch` 当作一等路径；
该页使用 API key，OAuth 账号仍需真实探针。

### 1.2 SIWC

SIWC preview limitations 要求本地 function/custom 工具进入 namespace（或使用
`additional_tools`）。本仓已选择 namespace，不引入 `additional_tools`。ChatGPT 套餐
路由已经强制 Responses，且 `store:false`、`stream:true`、完整 input 历史、禁字段清理
必须保持。

### 1.3 Codex

本地 Codex `970111f6cb62cc02fe36a82ea7d71b37a2889c97`：

- `create_apply_patch_freeform_tool` 生成 `custom` + `format: {type:"grammar",
  syntax:"lark", definition: ...}`，描述为
  “The `apply_patch` tool can be used to edit files. This is a FREEFORM tool, so do not
  wrap the patch in JSON.”
  （`codex-rs/core/src/tools/handlers/apply_patch_spec.rs:9-27`）
- Handler 只接受 `ToolPayload::Custom { input }`，用 `parse_patch` + `verify_apply_patch_args`
  解析/校验，错误前缀为 `apply_patch verification failed: ...`
  （`codex-rs/core/src/tools/handlers/apply_patch.rs:320-378`）。
- 成功输出为 `Success. Updated the following files:` + `A/M/D path`
  （`codex-rs/apply-patch/src/lib.rs:830`）。
- 流式进度使用 `StreamingPatchParser` 增量解析，`PatchApplyUpdated` 事件 500ms 缓冲
  （`apply_patch.rs:110-146`）。
- 普通 Responses 使用 flat 工具；只有 Responses Lite 才统一收进 namespace
  （`codex-rs/tools/src/tool_spec.rs:82-142`）。

### 1.4 QAQH 现状

- native 工具计划已存在：`crates/qaqh-runtime/src/agent/turn_lap/gate.rs:418-521`。
- DeepSeek 强制 flat + text：`gate.rs:471-483`。
- SIWC 强制 namespace：`gate.rs:484-493`。
- workspace 计划把 `apply_patch` 映射为 custom，并派生 `apply_patch_preview`
  （`crates/qaqh-workspace/src/model_tool_plan.rs:157-192`）。
- 系统提示仍要求 `apply_patch` 使用 `dry_run`（`crates/qaqh-runtime/src/agent/prompts/backend_prompt.md:38`），
  与 native custom 没有 `dry_run` 的事实冲突。
- 模型面成功文案是 `[OK] apply_patch — applied: ...`
  （`crates/qaqh-file-tools/src/apply_patch.rs:259-270`），不是 Codex 文案。
- `apply_patch_preview` 只在计划里合成，注册表/能力/统计/权限未正式接线。
- 引擎是 Codex 的同步移植（`crates/qaqh-file-tools/src/apply_patch_engine/`），
  grammar 资源与 Codex 一致，但缺少 Codex 测试套件的逐条对照。

## 2. 决策

### D1：OAuth 只保留 Responses

- `CredentialSource::DeepseekAccount` 只允许 `Wire::Responses`。
- `CredentialSource::ChatgptPlan` 已经只允许 Responses。
- `Wire::Chat` 只保留给 `CredentialSource::ApiKey`（BYOK）及其他 provider。
- 不在工具失败后自动切 wire；不回退 API key。

### D2：DeepSeek 使用 flat custom apply_patch，不使用 namespace

- `responses_tool_layout = flat`。
- `apply_patch_input_format = codex_custom`。
- `apply_patch_custom_grammar` 先 `none`；真实探针确认后切 `codex_lark`。
- 其他工具继续 function；不把 `write` / `edit` 改成 custom。

### D3：SIWC 使用 functions namespace

- `responses_tool_layout = functions_namespace`。
- `apply_patch` 是 namespace 内的 custom；其他工具是 namespace 内的 function。
- `apply_patch_custom_grammar = codex_lark`。
- 保留 `store:false`、`stream:true`、完整 input、instructions/developer、禁字段清理。

### D4：引擎以现有 Codex 移植为载体，逐模块对照而不是再造一套

QAQH 现有 `crates/qaqh-file-tools/src/apply_patch_engine/` 已经是 Codex
`codex-apply-patch` 的同步移植。P1 不引入第二套并行引擎，而是：

- 以 Codex `970111f6` 的 `parser.rs` / `streaming_parser.rs` / `seek_sequence.rs` /
  `text_file.rs` / `file_update.rs` 为参照，逐模块对照现有移植；
- 差异过大的模块直接按 Codex 源替换（保留 QAQH 的同步 I/O 适配），不做“看着差不多”
  的局部修补；
- 保留 QAQH 的 `apply_patch_engine/mod.rs` 作为 I/O 与安全适配层：
  - 路径词法归一化、workspace 边界、symlink 最终组件拒绝；
  - 跨工作区审批后的落盘；
  - dry-run、partial application、journal、file_state；
  - 同步 `std::fs`，不引入 `codex-exec-server`、`PathUri`、tokio；
- 每个对照/替换的模块带 Codex revision 与 Apache-2.0 归属；
- 建立 Codex conformance suite：把 Codex 的 parser / file_update / streaming_parser
  测试用例逐条跑 QAQH 引擎，接受/拒绝与落盘结果必须一致。

不复制 Codex 的 sandbox/symlink 策略；只复制解析、匹配、行尾保持与错误语义。

### D5：模型面按 Codex 对齐

- 工具名保持 `apply_patch`；类型 custom；输入 raw text。
- 默认带 Codex 的 Lark grammar（DeepSeek 探针后决定；SIWC 直接启用）。
- 描述以 Codex 原文为第一段，QAQH 的首匹配/部分应用警告放在其后。
- native profile 的系统提示必须写：
  - “Use `apply_patch` to edit files.”
  - “This is a FREEFORM tool, so do not wrap the patch in JSON.”
  - “NEVER try `applypatch` or `apply-patch`.”
  - “Do not re-read files after apply_patch; the tool call fails if it didn't work.”
  - “Matching takes the FIRST hit; disambiguate with extra lines or `@@ <context line>`.”
- native profile 不再提 `dry_run`。
- 模型面成功文案改为 Codex 的
  `Success. Updated the following files:\nA/M/D path`；结构化字段继续供 UI 使用。
- 解析/校验错误以 `apply_patch verification failed: ...` 开头，后面附 QAQH 的
  `code` / `hint`。

### D6：Preview 改为可选

- native `apply_patch` 默认直接 apply，与 Codex 一致。
- `apply_patch_preview` 是 QAQH 扩展：有 `confirm_apply` 时暴露；没有时不得让整个
  native plan 失败。
- 高风险场景（跨工作区、删除、超大补丁）可以走 preview/confirm，但不作为每次补丁
  的强制两步。
- 不再把 `dry_run` 塞进 raw patch。

### D7：OAuth 直接 native，BYOK 保留显式 opt-in

- DeepSeek 账号与 ChatGPT 套餐不再读取 `apply_patch_input_format` 决定是否 native：
  两条 OAuth 路由直接构造 native profile。
- DeepSeek 固定 flat + text；SIWC 固定 functions namespace + Codex Lark。
- OAuth 的模型工具面只把 `apply_patch` 作为 custom；`confirm_apply` /
  `apply_patch_preview` 在构造 selected 列表时被过滤，避免 Codex 风格的直接 apply
  被 QAQH preview 两步打断。
- BYOK 默认仍为 `function_json`；显式 `codex_custom` 才能用 custom，并保留 preview。
- 真实 OAuth 探针只决定 grammar/description 等字段是否被端点接受，不再决定是否
  启用 native。

### D8：流式进度按 Codex

- 复用 `StreamingPatchParser`，增加增量 hunk / 文件变更出口。
- 行数预览使用 `stats_including_pending_line`：`+` / `-` 前缀到达即 +1；
  `edit` 的 `LineTally` 同样计入当前未完成行。`\n` 只负责提交/去重，不是唯一触发。
- 以 500ms 缓冲发 `PatchApplyUpdated` 类事件；UI 不解析 JSON 参数。
- 行数估算与进度共用同一个 parser，不维护第二套解析器。

## 3. 配置矩阵

| 路由 | Wire | Layout | apply_patch | grammar | 其他工具 |
|---|---|---|---|---|---|
| DeepSeek 账号（OAuth） | Responses only | flat | custom | 先 none；探针后 codex_lark | function |
| ChatGPT 套餐 / SIWC | Responses only | functions_namespace | custom | codex_lark | function（namespace 内） |
| OpenAI API key | 不变 | 不变 | 默认 function_json；显式 opt-in custom | 不推断 | 不变 |
| DeepSeek API key | Chat 或 Responses | 不变 | 默认 function_json | 不推断 | 不变 |

DeepSeek profile 额外建议：

- `responses_send_include = false`（DeepSeek 不支持 include）
- 不发 `prompt_cache_key`
- `web_search` 仅 `deepseek-v4-pro` 按需开启
- `store` 恒为 false（现有行为）
- `reasoning.summary` 不作为能力承诺

## 4. 迁移阶段

### P0：OAuth 去 Chat

- `qaqh-config` DTO 拒绝 `deepseek_account + chat`。
- runtime `apply_deepseek_account` 只接受 Responses。
- 修掉 `CredentialSource::DeepseekAccount` 的过时注释。
- 更新 spec/checklist/tasks 的支持矩阵。
- 验收：保存期与请求期都拒绝 Chat；Responses 路径不变。

### P1：Codex 引擎对照与 conformance

- 以现有 `apply_patch_engine` 为基线，逐模块对照 Codex `970111f6` 的纯解析/匹配实现。
- 差异过大的模块按 Codex 源替换；`mod.rs` 保持 I/O/安全适配。
- 移植 Codex parser / file_update / streaming_parser 测试用例，逐条对照接受/拒绝与
  落盘结果。
- 验收：conformance suite 全绿；现有 workspace/file-tools 测试不回归。

### P2：工具声明、Prompt、输出、Preview

- native 声明使用 Codex 描述 + grammar。
- prompt 增加 native 分支；legacy 保留 `dry_run`。
- `model_text` 改 Codex 文案；结构化字段不变。
- preview 改为可选；移除 `PreviewConfirmationUnavailable` 硬错误。
- 验收：prompt snapshot、wire-shape golden、preview/confirm 回归。

### P3：DeepSeek 真实探针

- 工具声明：最小 custom vs description vs text vs grammar。
- completed item：`response.output_item.done` / `response.completed` 是否带 `input`；
  若缺失，SDK 必须回退到 `custom_tool_call_input.done` 的 snapshot。
- 同轮 function/custom 混合、第二轮输出配对、重启恢复、preview/confirm、回滚。
- 验收：C11 DeepSeek 项。

### P4：SIWC 真实探针

- namespace 内 function/custom、grammar、第二轮配对、重启恢复。
- 验收：C11 SIWC 项。

### P5：OAuth 固定 native 与回滚边界

- OAuth 路由已经固定 native；探针失败时只回退字段/grammar，不回退到 Chat。
- BYOK 的 `function_json` 路径不变；OAuth 不提供运行时自动切换。
- 不重标旧历史、不重放不确定写。
- 更新支持矩阵、发布说明、回滚记录。

## 5. 风险与探针

| 风险 | 影响 | 处理 |
|---|---|---|
| DeepSeek completed custom item 缺 `input` | SDK 报 `tool_payload_conflict`，整轮失败 | 已按文档回退到 delta/done 累积 snapshot；真实账号仍需复验 |
| DeepSeek 不接受 grammar | 400 或 grammar 被忽略 | 先 text；探针后决定；不在运行时静默降级 |
| DeepSeek 不接受 description/format | 400 | 已对 DeepSeek 发 `type/name`-only 最小声明；真实账号复验 |
| namespace 被 DeepSeek 忽略/拒绝 | 400 或工具不可见 | 不做 DeepSeek namespace |
| 混合历史回放失败 | 第二轮 orphan/重复执行 | C07/C11 覆盖；按原 kind 回放 |
| preview 硬要求 | 与 Codex 体验不一致 | 改为可选；高风险策略单独保留 |
| 复制 Codex 安全策略 | 削弱 QAQH symlink/跨工作区保护 | 只复制解析/匹配，不复制 sandbox |

## 6. 不做的事

- 不给 DeepSeek 引入 namespace。
- 不为 OAuth 继续维护 Chat。
- 不引入 Responses Lite、`additional_tools`、hosted `tool_search`。
- 不按模型名子串推断能力。
- 不复制 Codex 的 sandbox / symlink / multi-environment 策略。
- 不把 raw patch 塞进 JSON，也不加 `input -> patch` serde alias。
- 不从普通 assistant 正文/Markdown 中提取补丁执行。

## 7. 验收门禁

- C02.08 grammar 资源与接受边界。
- C03 custom 流式完成性、截断、重连。
- C05.03 非流式/恢复入口的 canonical 化。
- C06 权限、冲突、allowlist、plan mode。
- C07 native 元数据、回放、旧会话。
- C09 preview 与 UI/统计。
- C11 DeepSeek/SIWC 真实 OAuth 双轮。
- C12 默认启用与回滚演练。
