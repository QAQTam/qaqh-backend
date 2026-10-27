# Codex 工具对齐：实施契约与跨仓同步单

> T01 实施记录，2026-10-27。设计真源仍为 [spec.md](spec.md)；本文固定实施选择。mutil-ai 0.7.0 提供原生工具 opt-in 表面，但不表示已默认启用或已通过真实端点验收。

## 1. 基线与本轮范围

| 仓库 | 参照 HEAD | 本轮处理 |
| --- | --- | --- |
| qaqh-backend | `1bea479ae1e77a52e34aee9728ddc3ceb22b891d` | 本仓文档、协议 fixtures、一致性测试与 opt-in 接线 |
| mutil-ai | `9b33d95379a9e69907c8f55381d05a560b458c27`（manifest 0.7.0，tag `v0.7.0`） | 已发布 0.7.0；本仓 workspace pin 已同步 |
| Codex | `970111f6cb62cc02fe36a82ea7d71b37a2889c97` | 只读设计参照；本轮不复制 grammar |

基线工作区已有未跟踪的 `docs/adr/`、`docs/codex-tools对齐/`、`docs/spec-unified-agent-runtime.md`；不清理、不覆盖无关文件。HEAD 不能代表未提交内容的版本，后续 SDK 同步时需重新核对实际 API。

## 2. 官方来源复核

复核日期：2026-10-27。使用公开文档抓取，无账号请求、无凭据读取。

| 来源 | 本轮可复核内容 | 证据边界 |
| --- | --- | --- |
| https://api-docs.deepseek.com/guides/responses_api | 列出 custom output item、`response.custom_tool_call_input.delta/done`；终态为 completed/incomplete/failed，无 `[DONE]` | 页面返回内容被工具截断；本轮不声称完整复核该页所有工具限制 |
| https://api-docs.deepseek.com/quick_start/agent_integrations/codex | 模型目录示例含 `apply_patch_tool_type: freeform`、`use_responses_lite: false` | 文档示例不是本机账号探针；页面返回内容被截断 |
| https://developers.openai.com/siwc/token-sharing-open-source/preview-limitations.md | HTTP 要求 store=false、stream=true、完整所需 input；function/custom 使用 namespace 或 additional_tools；禁字段和不支持工具清单 | 本期选 namespace，不引入 additional_tools；不代表具体模型已验收 |
| https://developers.openai.com/api/docs/guides/function-calling.md | function 与 free-text custom 是不同工具表示，结果通过 call_id 关联 | 返回内容被截断；namespace/custom/grammar 细节仍需 SDK 实施阶段补充定点来源证据 |

DeepSeek namespace、grammar 实际约束效果、具体 GPT 型号 custom/grammar 准入、OAuth 双轮和恢复：全部未验证。fixtures 是人工合成的协议目标，不是线上抓包。

## 3. 冻结的语义接口

Rust 符号名以 mutil-ai 0.7.0 的公开 API 为准；以下字段语义与验证规则必须保留。

```text
ToolDeclaration = Function(name, description, parameters, strict)
                | Custom(name, description, Text | Grammar(syntax, definition))
                | Namespace(name, description, [Function | Custom])

NativeCall = {
  kind: Function | Custom,
  namespace: Option<String>, name: String,
  call_id: String, item_id: Option<String>,
  payload: JsonArguments | RawText,
  completion: Receiving | Completed | Invalid | Interrupted,
  provider_state: Option<opaque>
}

CanonicalCall = {call_id, internal_tool_key, args: object}
ToolPlan = immutable declarations + address mappings + profile snapshot + version
```

- SDK 正式建模本地 custom，不能复用 hosted server tool；旧 function 构造器继续可用。
- SDK 负责协议累计、完成性与结果配对；QAQ 不另造一套 custom SSE 修补器。
- 一层 namespace；同一作用域内同名 function/custom 声明也拒绝。地址 `(kind, namespace, name)` 只按当前 ToolPlan 查找。
- item_id 只标识输出 item；结果必须关联真实 call_id。非空身份冲突和完成 payload 冲突报错，不取更长字符串猜测。
- 原生 custom 映射至 `apply_patch({patch: raw_input, dry_run:false})`，逐字保留文本；在权限、冲突、canonical 去重前完成。
- preview function 映射至同一内部实现 `{patch, dry_run:true}`；保留外部地址用于审计与回放。映射与权限能力继承必须发生于准入前。
- 本期不实现旧 function `input -> patch` 修复，不加 serde alias，不从正文提取补丁。

## 4. 配置与存储迁移选择

拟定 EndpointCompat 字段（T08 前不写入生产配置）：

| 字段 | 值域 | 旧配置缺省 |
| --- | --- | --- |
| `apply_patch_input_format` | `function_json` / `codex_custom` | `function_json` |
| `responses_tool_layout` | `flat` / `functions_namespace` | `flat` |
| `apply_patch_custom_grammar` | `none` / `codex_lark` | `none` |

三者独立；grammar 仅在 custom 下合法，namespace 仅用于 Responses。路由硬约束高于普通缺省：SIWC 不能因回滚输入格式而变成 flat；DeepSeek 原生范围仅 apply_patch，flat text，grammar 另验。具体字段落地时同时更新 DTO/schema/导出与测试。

存储选择：保留 canonical ToolUse，新增可选、带 `version:1` 的原生调用元数据，承载 kind/namespace/name/call_id/item_id/raw payload/completion；缺元数据的旧记录仅按旧 function 解释。`ResponseOutputItem` 可保留 provider 扩展，但不是第二份可执行调用，也不能作为 SDK pending 的替代品。最终字段位置在 T07 根据存储/压缩全链路核对后确定，不提前改旧会话 schema。

回放选择：本期不做跨 wire 的自动语义降级；无法编码的 custom 历史显式阻塞并给迁移/新会话提示。当前 profile 不重标历史。未完成批次固定原 ToolPlan。

## 5. 完成性、去重与错误

- delta 是片段，done/full input 是快照；相同完成帧幂等，冲突完成帧失败。
- 只有可靠 Completed 且身份可还原的调用可进入准入；End Patch 本身不是完成证据。
- 首版建议：response 截断/失败/断流时不补执行尚未调度的写调用；保留已完成 item 的事实，不伪造结果。该调度策略须在 T03/T06 验证后才能定为生产行为；用户取消始终阻止新调度。
- 按原生身份去重重复帧；不同 call_id 的同参调用不是重复帧。另设写操作不确定状态，禁止自动重试，不以同参成功注记掩盖未知结果。
- 错误分类至少区分 `unsupported_tool_representation`、`tool_declaration_conflict`、`unknown_tool_address`、`tool_identity_conflict`、`tool_payload_conflict`、`tool_call_incomplete`、`orphan_tool_result`，与既有 invalid_arguments/permission/parse_error/no_match 分开。
- 缺 call_id、身份冲突等无法安全配对的协议错误应可见中止，不能编造 id 回传。身份合法但地址/参数被拒绝时仍按原 kind 返回错误结果。

## 6. fixture 使用与验证边界

`fixtures/tool-protocol.json` 包含请求声明、完成调用、预期 canonical 参数、下一轮 input；custom 样本另含 item 级 SSE 正常事件序列，终态完整 response 单列于 `response`，区分 item_id/call_id，覆盖中文、引号、反斜杠、空行。`request` 是声明片段而非完整 HTTP 请求体，`next_input` 是回放配对子序列而非完整对话历史。

- flat function：旧 profile 基线。
- flat custom + function：DeepSeek 目标形状，无 namespace/grammar 推断。
- namespace function：SIWC 输入回滚形状。
- namespace custom + function：布局与配对目标，text 仅用于独立验证协议层级，**不是可上线的 SIWC grammar profile**。

`crates/qaqh-gate/tests/codex_protocol_fixtures.rs` 只验证合成样本的一致性，不调用生产编解码、不执行补丁。T02—T10 必须将这些目标接入实际 SDK/gate/mock server 测试；本测试通过不能勾选 C02—C10。完整 grammar 及接受边界对照留在 T02。

## 7. 跨仓同步单（T02/T04 已随 0.7.0 发布）

请在 mutil-ai 仓按 T02—T04 实施，并同步：

> 2026-10-27 更新：mutil-ai 0.7.0 已发布（commit
> `9b33d95379a9e69907c8f55381d05a560b458c27`，tag `v0.7.0`，crates.io
> 已发布）。T02 的 typed 声明与 T04 的 normalizer/回放表面可用；T03 的
> 截断、重连与异常帧边界仍未完成。以下条目保留为发布验收清单。

1. typed function/custom/namespace 声明、选择与唯一性验证 API；非 Responses 的显式能力拒绝。
2. 原生调用 kind/namespace/name/call_id/item_id/raw payload/completion API，以及 SSE progress/completed 事件结构。
3. custom delta/done/item/terminal 状态机；快照冲突、终态缺项、断流/取消和重复帧测试。
4. normalizer 正式登记 custom pending；结果按原 kind 编码；raw/typed 不双登记，orphan 可见失败。
5. 完整响应与第二轮实际 HTTP body 的离线测试，旧 function 构造器/历史兼容测试。
6. 实际发布版本/revision、迁移说明、测试命令与结果；不能仅本地 path dependency 成功。

本仓已按 0.7.0 pin 接入显式 opt-in 路径，仍不手拼 extra_body.tools/input、不默认启用、不改 OAuth 登录/凭据头/origin。T01 的 fixture 测试与 T02—T04 的 SDK 正确性是两项不同证据。

## 8. 验证记录

基线为第 1 节 QAQ HEAD，加本轮未提交的文档/fixture/测试文件。验证日期：2026-10-27。

| 实际验证 | 结果 | 范围 |
| --- | --- | --- |
| `TMPDIR=/home/qaqtamsy/Projects/qaqh-backend/target cargo test --offline -j 1 -p qaqh-gate --test codex_protocol_fixtures` | exit 0，4 passed / 0 failed | fixture 矩阵、声明/身份/结果配对、canonical 字节保真、delta/snapshot 一致性；不是生产编解码验收 |
| `rustfmt --edition 2024 --check crates/qaqh-gate/tests/codex_protocol_fixtures.rs` | exit 0 | 仅新增 Rust 测试文件 |
| Python 标准库读取 JSON 并断言四组 canonical/delta/配对 | exit 0 | 补充独立样本核验，不替代 Rust/SDK 测试 |
| `git diff --check` | exit 0 | 只覆盖 Git 跟踪的 diff；本轮新增/原有未跟踪文件不在该命令覆盖范围 |

首次未设置 TMPDIR 的 cargo 测试在依赖 build script 链接阶段遇到 `Cannot create temporary file in ./: Read-only file system`。设置本仓 TMPDIR 后可编译；多次后台构建返回 exit -1 且没有测试结果，不计通过。最终以上述前台 `-j 1` 命令的 exit 0 为准。本节未运行全工作区/完整 gate 回归或账号端点探针；SDK 定向测试见第 11 节。

T01 仍部分完成：公开长页的特定协议细节需补证，存储字段位置及调度策略需后续核对；SDK 公开 API 已随 mutil-ai 0.7.0 发布（commit `9b33d95`）。C01 总门禁未通过。T02/T04 的 SDK 侧已发布，T03 的异常/重连边界仍未完成；QAQ 已接 opt-in gate/runtime，默认配置未改。


## 9. QAQ 工具侧进展与 0.7.0 opt-in 接线

记录日期 2026-10-27。mutil-ai 0.7.0 已发布：commit
`9b33d95379a9e69907c8f55381d05a560b458c27`（tag `v0.7.0`，crates.io
已发布）。QAQ 的 `Cargo.toml` / `Cargo.lock` 已 pin 0.7.0，CI workflow
已同步 checkout 同一 commit。SDK 的 `NativeToolProgress` /
`NativeToolCompleted`、Responses 声明编码与 custom 回放已进入发布版本；
真实 provider/OAuth 端点仍未验收。

- `crates/qaqh-tool-core/src/model_tool_plan.rs`：独立的不可变模型工具计划、声明冲突与地址检查、已完成 native 调用适配，以及分别保留 native/canonical 视图；`qaqh-workspace` 可从实际注册表生成计划，runtime gate 已在显式 opt-in 下调用。调用方仍须先完成工具过滤，SDK 必须保证权威完成性；本模块不解析 SSE、不承担授权。
- 测试使用 canonical 目标进入真实权限提取、写冲突分组：四种文件操作、跨工作区及 Unix 外部符号链接。并不代表模型调用已经经过完整准入。
- `crates/qaqh-file-tools/src/arg_estimate.rs`：独立 raw patch 流式行数估算器，仅用于展示，异常冻结，不推断完成，不接现有 UI。
- `crates/qaqh-file-tools/src/confirm_apply.rs` / `pending.rs`：补充预检后文件改变、一次性丢弃与过期测试，复用现有引擎；preview function 未注册。
- `crates/qaqh-message/src/store.rs`：缺结果的恢复占位改为执行结果未知、可能有副作用，避免无证据断言未执行。runtime canonical/legacy 证据仍可进一步修正占位。
- 未改同参去重生产行为。当前 gate 和 runtime 都存在基于名称/参数的去重；迁移必须以 SDK 真实身份、完成性和测试为前提，不能仅在一处替换，也不能把不确定结果当成功。

验证（均使用 `TMPDIR=/home/qaqtamsy/Projects/qaqh-backend/target`、`--offline -j 1`）：

| 实际命令/范围 | 结果 |
| --- | --- |
| `cargo test -p qaqh-tool-core --quiet` | 79 passed |
| `cargo test -p qaqh-file-tools --quiet` | 197 passed；依赖的 process-tools/workspace 各有既有编译警告 |
| `cargo test -p qaqh-message --quiet` | 70 单元 + 5 集成 passed |
| `cargo test -p qaqh-permission --lib -- --skip root_only_trust_entry_is_not_resolved_to_current_drive_root` | 40 passed，1 filtered |
| `cargo test -p qaqh-tool-core -p qaqh-permission -p qaqh-message -p qaqh-file-tools` | exit 101；permission 的 `root_only_trust_entry_is_not_resolved_to_current_drive_root` 在 Linux 对 `\\` 的断言失败；其余 permission 40 项另行通过 |
| `cargo test -p qaqh-runtime --lib tool_recovery` | 4 passed；SDK 新事件已显式处理，executionless cancel 仍明确“未执行” |
| `cargo clippy -p qaqh-tool-core --all-targets -- -D warnings` | 编译依赖时被未改的 `qaqh-types/src/config.rs:399` 的 needless_borrows lint 阻断 |
| `rustfmt --edition 2024 --check` 针对本次修改的 Rust 文件；`git diff --check` | exit 0；后者不覆盖未跟踪文件 |

T05—T09 均为部分进展，不勾选跨层完成项；SDK 发布不再是阻塞项，真实端点、默认启用、production preview 与完整 T03 异常边界仍需验收。


## 10. Workspace 计划生成并行切片（2026-10-27）

- `ApplyPatchTool::meta()` 注册 native custom/preview 元数据，preview schema 由 `ApplyPatchPreviewArgs` 生成；实际执行器仍只有原 `apply_patch`，没有注册一个可独立执行的 preview。
- `ToolManager::build_model_tool_plan` 从真实 registry 与 `filtered_defs()` 生成独立快照；调用方必须显式传入完成 profile/执行能力过滤后的 canonical 工具名。workspace 二次拒绝非可见项、重复项以及全未知但回退全量的非空 allowlist。空选择保持空，不能当全量。
- namespace 名称及描述、profile 标识与版本保存在不可变计划；动态工具使用完整 canonical 注册名，普通函数别名可逆且冲突在构建时拒绝。补丁的 native 声明来自 descriptor，而不是仅凭工具名推断。
- 原生 patch 会派生 preview 映射；为确保说明中的 pending → confirm 可用，首版计划要求 `confirm_apply` 在已过滤选集内，否则显式拒绝，绝不扩大 allowlist。这是本切片增加的保守准入要求，后续实际 profile 接线需确认产品预期。
- 已接生产 gate/runtime 的显式 opt-in 路径：`gate_request` 在
  `apply_patch_input_format = codex_custom` 且 wire 为 Responses 时构建计划并调用
  `chat_stream_responses_with_plan`；默认 profile 仍为 legacy function。完整
  Codex grammar 已随 `qaqh-workspace` 资源落地，但 grammar 接受边界、动态
  profile 布局、DTO/UI 暴露与安全准入证明仍未完成。`selected_names` 的最终
  能力过滤仍是调用者责任，不能把本计划当作授权凭据。SIWC、DeepSeek 的真实
  请求及默认启用保持阻塞。

验证（TMPDIR 指向本仓 target，`--offline -j 1`）：

| 命令 | 结果 |
| --- | --- |
| `cargo test -p qaqh-workspace --lib model_tool_plan --quiet` | 9 passed；覆盖 flat/namespace、legacy/custom、preview、拒绝冲突与无确认、allowlist、动态身份和不可变快照 |
| `cargo test -p qaqh-tool-core --lib native_patch_representation` | 1 passed |
| `cargo test -p qaqh-workspace -p qaqh-file-tools -p qaqh-tool-core --quiet` | file-tools 197、tool-core 80 passed；workspace 162 passed、3 个 exec 沙箱授权测试失败，exit 101 |
| workspace `authorization::tests::exec_cwd_inside_workspace_keeps_classified_auto_approve -- --exact` | 单独复现失败；该授权代码本轮未修改，不能宣称全量通过 |
| 本轮改动 Rust 文件定点 `rustfmt --check` 与 `git diff --check` | exit 0；后者不覆盖未跟踪文件 |

SDK 工作区可能继续变动，以团队发布的版本及实际 API 为准；本切片不修改 SDK、Codex，也未以新事件空处理掩盖既有 gate 编译阻塞。

## 11. Native apply_patch opt-in 垂直切片（2026-10-27，已随 mutil-ai 0.7.0 发布）

本轮把 T02 的 Responses 声明出口、T05 的 gate/runtime 接线和 T08 的显式能力字段接成了一条可测试的 opt-in 链路；默认 profile 仍保持 legacy function JSON。SDK 侧以 mutil-ai 0.7.0 发布（commit `9b33d95379a9e69907c8f55381d05a560b458c27`，tag `v0.7.0`），本仓 pin 与 CI checkout 均已对齐该 revision；真实端点仍未验收。

GitHub Release 已创建：<https://github.com/QAQTam/mutil-ai/releases/tag/v0.7.0>（2026-10-27，notes 取自 CHANGELOG 0.7.0）。

- SDK：`normalize` 只对 Responses 放行 `tool_declarations`，其余 wire 仍显式拒绝；`to_responses_body` 现在编码 flat custom 与 namespace function/custom。新增测试覆盖声明出站、非 Responses 拒绝、namespaced function 终态回填。
- QAQ 配置：`EndpointCompat` 新增 `apply_patch_input_format`、`responses_tool_layout`、`apply_patch_custom_grammar`，缺省分别为 `function_json` / `flat` / `none`，旧配置零迁移。示例见 `qaqh-config` 的默认配置注释。
- QAQ runtime：`gate_request` 在 `apply_patch_input_format = codex_custom` 且 wire 为 Responses 时，从已过滤的 `tool_defs` 构建不可变 plan，并调用 `chat_stream_responses_with_plan`；否则继续走原 `chat_stream`。缺少 `confirm_apply` 或 plan 构建失败是显式回合错误，不扩大 allowlist。
- grammar：`crates/qaqh-workspace/assets/apply_patch.lark` 从 Codex revision `970111f6cb62cc02fe36a82ea7d71b37a2889c97` 引入，带 Apache-2.0 归属注释；`codex_lark` 只在该资源存在时启用。
- gate：`responses_sdk` 的 native 路径现在保留 provider state，并拒绝 `status != completed` 的历史 raw item；`codex_protocol_fixtures` 的 namespace round-trip 不再 ignore，新增 flat custom 声明/解码测试。

验证（`TMPDIR` 指向本仓 target，`--offline -j 1`）：

| 命令 | 结果 |
| --- | --- |
| `cargo test -p qaqh-gate --test codex_protocol_fixtures` | 6 passed（含 namespace 与 flat custom 的声明→流式→解码） |
| `cargo test -p qaqh-runtime --lib native_tool_plan` | 2 passed（默认 legacy、native 布局/grammar/wire 映射） |
| `cargo test -p qaqh-runtime --lib tool_recovery` | 4 passed（executionless cancel 仍明确“未执行”） |
| `cargo test -p qaqh-workspace --lib model_tool_plan` | 10 passed（含 grammar 资源归属检查） |
| `cargo test -p qaqh-config --lib native_tool_compat_fields_load_and_roundtrip` | 1 passed（三个 opt-in 字段 TOML 往返） |
| `cargo test -p qaqh-types` / `cargo test -p qaqh-config` | 通过 |
| `cd ../mutil-ai && cargo test --offline -j 1` | exit 0（本次复跑；声明、custom 流、namespaced function 回填、replay） |
| `cd ../mutil-ai && cargo test --offline -j 1 --test tool_declarations --test native_tool_replay --test native_tool_stream` | 4 + 4 + 3 passed（本次定向复跑） |
| `cd ../mutil-ai && cargo test --offline -j 1 --test endpoint_dispatch native_custom_call_replays_result_in_next_http_body` | 1 passed（loopback 第二轮实际 HTTP body） |
| `cargo tree --offline --locked -p qaqh-gate -i mutil-ai` | exit 0；`mutil-ai v0.7.0 (/home/.../mutil-ai)`；`Cargo.lock` 同为 0.7.0 |

mutil-ai 0.7.0 已发布（crates.io，commit `9b33d95`，tag `v0.7.0`），本仓 workspace pin 已从 `0.6.1` 同步到 `0.7.0`。仍未完成：真实 OAuth 端点、SIWC/DeepSeek 对 namespace/grammar 的实际接受度、UI 的 raw patch 进度、版本化 native 元数据、默认启用与回滚演练。默认配置没有改变；本切片只能通过显式 `compat` 字段 opt-in。
