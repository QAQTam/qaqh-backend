# Codex 工具协议对齐规格

> 状态：Draft / 待实现；日期：2026-10-27。
> 范围：QAQ-Harness + 本地依赖 `mutil-ai`；以 `apply_patch` 为首个原生 custom 工具。
> 本文记录设计与已核对事实，不代表已经完成 SDK 改造或真实 OAuth 端点验收。

## 0. 文档导航与规范用语

- [任务列表](tasks.md)：工作包、依赖、产物与完成条件。
- [验收清单](checklist.md)：逐项证据、失败注入和上线门禁。
- [实施契约与跨仓同步单](contract.md)：T01 基线、实施选择、合成 fixtures 和验证证据；不替代本规格。
- MUST / 必须：安全性、协议正确性或上线所必需的约束。
- SHOULD / 应：推荐的默认方案；偏离必须记录原因与验证证据。
- MAY / 可：不影响本期交付的扩展。
- 需求使用 `R01`—`R12`，任务使用 `T01`—`T12`，验收使用 `C01`—`C12` 对应分组。

## 1. 背景、问题与目标

### 1.1 问题分层

本项目已经移植 Codex 的补丁解析与内容匹配，但模型面仍使用 JSON function：

```json
{"patch":"*** Begin Patch\n...\n*** End Patch","dry_run":false}
```

截图 `屏幕截图_20261027_015406.png` 显示两种不同错误：

1. `unknown field input, expected patch or dry_run`：参数反序列化失败，尚未执行补丁引擎。
2. `Failed to find expected lines`：已经进入内容匹配，预期旧内容没有命中。

第一类可以通过原生 custom 接入或受控兼容解决；第二类仍需要正确的源文件内容与上下文。不得将 `no_match` 自动归因于协议，也不得为提高成功率而放宽真实内容匹配。

截图不能证明 DeepSeek 的训练来源、当次上游调用类型或源文件当时的具体内容；这些结论需原始请求、响应与文件快照支持。

### 1.2 目标

- **R01**：清晰分离内部执行契约、模型面工具声明和原生调用身份。
- **R02**：SDK 正式支持 Responses function/custom/namespace，而非依靠 raw JSON 绕过。
- **R03**：完整接入 custom 的流式、终态、截断与去重语义。
- **R04**：原生调用进入权限与调度前必须归一化为内部 canonical 调用。
- **R05**：持久化与下一轮回放保留调用类型、身份和结果配对。
- **R06**：DeepSeek 账号通过 Responses 启用平铺的 `apply_patch` custom 工具。
- **R07**：SIWC GPT 通过 Responses namespace 提供 function/custom 工具。
- **R08**：原生模式下保留预检、pending 和确认能力。
- **R09**：工具说明、提示词与 UI/流式进度反映真实声明。
- **R10**：兼容旧会话、其他 provider，并提供显式回滚。
- **R11**：为协议、安全与恢复路径建立离线测试和可观测性。
- **R12**：以真实 OAuth 双轮探针和分阶段发布作为启用门禁。

### 1.3 非目标

- 不重写现有 patch parser、内容匹配算法或非原子多文件应用语义。
- 不移植完整 Codex agent loop、Code Mode、Responses Lite 或 `additional_tools`。
- 不开启 hosted `tool_search`、hosted MCP 或其他被 SIWC 路由限制的能力。
- 不要求所有 provider 使用 namespace，不更换 DeepSeek/SIWC 登录方式。
- 不冒充 Codex 客户端、不改凭据发送 origin、不自动改用 API key 扣费。
- 不从普通 assistant 正文、Markdown 围栏或思考文本中提取补丁并执行。

## 2. 已确认事实与待验证边界

### 2.1 本仓源码基线

下列行号为本次调研快照；后续实现以符号和实际代码为准。

| 事实 | 源码入口 |
| --- | --- |
| `ApplyPatchArgs` 严格接收 `patch`、`dry_run` | [apply_patch.rs](../../crates/qaqh-file-tools/src/apply_patch.rs):30–38 |
| typed adapter 先反序列化，再执行工具 | [typed.rs](../../crates/qaqh-tool-core/src/tool_api/typed.rs):99–114 |
| 引擎、parser、内容匹配独立于模型协议 | [引擎](../../crates/qaqh-file-tools/src/apply_patch_engine/mod.rs):249；[匹配](../../crates/qaqh-file-tools/src/apply_patch_engine/file_update.rs):47–141 |
| 授权与写冲突检查均读取 `args.patch` | [permission.rs](../../crates/qaqh-permission/src/permission.rs):120–125；[conflict.rs](../../crates/qaqh-permission/src/conflict.rs):47–53 |
| QAQ `ToolDef` 是 function 形态 | [tool_def.rs](../../crates/qaqh-types/src/tool_def.rs):8–24 |
| gate 默认路径将所有工具转成 SDK 的 JSON Schema `ToolSpec`；native opt-in 另走不可变 plan | [responses_sdk.rs](../../crates/qaqh-gate/src/responses_sdk.rs):691–702；[gate.rs](../../crates/qaqh-runtime/src/agent/turn_lap/gate.rs):418–521 |
| DeepSeek 账号执行路径已经允许 Chat 与 Responses | [dto.rs](../../crates/qaqh-config/src/dto.rs):150–166；[gate.rs](../../crates/qaqh-runtime/src/agent/turn_lap/gate.rs):975–1005 |
| SIWC 已使用官方 API base、账号 token、套餐契约收口 | [SIWC 常量](../../crates/qaqh-siwc/src/protocol.rs):23–30；[gate.rs](../../crates/qaqh-runtime/src/agent/turn_lap/gate.rs):899–929 |
| 已有 opaque `ResponseOutputItem` 存储槽位 | [message.rs](../../crates/qaqh-types/src/message.rs):117–124 |

### 2.2 SDK 与 Codex 参照

相邻仓库不是本仓发布产物。实施时必须记录参照 revision，并在依赖发布后更新本仓 pin。下表已按 mutil-ai 0.7.0（commit `9b33d95379a9e69907c8f55381d05a560b458c27`，tag `v0.7.0`）更新；0.6.1 的“声明只有 function、normalizer 只登记 ToolCall”是历史基线，不再代表当前 SDK。

| 事实 | 参照源码 |
| --- | --- |
| SDK 提供 typed function/custom/namespace 声明与 `NativeToolCall` 身份/完成性 | [mutil-ai types.rs](../../../mutil-ai/src/types.rs):774–941 |
| SDK 出站编码 flat custom 与一层 namespace function/custom；其他 wire 拒绝声明 | [openai_responses.rs](../../../mutil-ai/src/adapter/openai_responses.rs):399–532；[normalize.rs](../../../mutil-ai/src/normalize.rs):434–451 |
| SDK `extra_body` 不允许覆盖 `tools`、`input` 等保留字段 | [adapter/mod.rs](../../../mutil-ai/src/adapter/mod.rs):138–207 |
| SDK normalizer 以真实 `call_id` 登记 native pending，按原 kind 回放结果；opaque client item 被拒绝 | [normalize.rs](../../../mutil-ai/src/normalize.rs):679–701, 830–899 |
| Codex `apply_patch` 使用 custom + Lark grammar | [apply_patch_spec.rs](../../../codex/codex-rs/core/src/tools/handlers/apply_patch_spec.rs):9–27；[grammar](../../../codex/codex-rs/core/assets/tools/apply_patch.lark) |
| Codex handler 接收原始 custom input | [handler](../../../codex/codex-rs/core/src/tools/handlers/apply_patch.rs):320–325 |
| 普通 Responses 保留工具原形；Lite 才统一收拢 namespace | [tool_spec.rs](../../../codex/codex-rs/tools/src/tool_spec.rs):82–141 |
| Lite 还会使用 `additional_tools` 输入项 | [client.rs](../../../codex/codex-rs/core/src/client.rs):1016–1030 |
| Codex 路由将 namespace、name、payload 分开保存 | [router.rs](../../../codex/codex-rs/core/src/tools/router.rs):259–304 |

### 2.3 官方文档事实

核对日期：2026-10-27。实施与发布前必须重新确认，并记录文档地址、日期和证据摘要。

- [DeepSeek Codex 集成](https://api-docs.deepseek.com/quick_start/agent_integrations/codex)：模型目录明确使用 `apply_patch_tool_type: freeform`。
- [DeepSeek Responses API](https://api-docs.deepseek.com/guides/responses_api)：支持 `custom_tool_call` / `custom_tool_call_output`、`call_id` 配对和 custom input 流事件；custom 工具仅明确支持名称 `apply_patch`。
- [SIWC preview limitations](https://developers.openai.com/siwc/token-sharing-open-source/preview-limitations.md)：function/custom 工具应分组到 namespace，或通过 `additional_tools` 输入项提供；HTTP 要求 `store:false`、`stream:true` 与完整所需历史。
- [OpenAI function calling](https://developers.openai.com/api/docs/guides/function-calling.md)：区分 function、custom、grammar 与 namespace。

以下不得作为已确认能力：DeepSeek namespace 支持、DeepSeek 是否执行 Lark 约束、任意 GPT 型号的 custom/grammar 支持，以及当前本机 OAuth 账号的实际端点准入。必须通过探针补证据。

## 3. 核心决策与不变量

### 3.1 四个正交维度

1. **Credential**：API key、DeepSeek 账号、ChatGPT 套餐，决定凭据与允许的 origin。
2. **Wire**：Chat、Responses 等，决定可编码的协议。
3. **Input format**：JSON function 或原始文本 custom，决定模型输入形态。
4. **Tool layout**：平铺或 namespace，决定声明组织与调用身份。

namespace 不是 function 的替代品；OAuth 也不会自动让 SDK 学会 custom 编解码。

### 3.2 必须成立的不变量

- 一个内部工具实现；原生模式不能旁路现有权限、串行化、执行、审计和结果投影。
- 原生信息与 canonical 参数是两种视图，不能互相覆盖；补丁文本本身不做语义修复。
- 执行面只接收已完成、可还原、身份合法的调用，不执行半个补丁。
- 每个结果按照原调用的 `kind + call_id` 回传，不能根据工具名猜类型。
- `item_id` 与 `call_id` 分离；不以 `ctc_*` / `fc_*` item 身份替代调用配对身份。
- 同一轮重复帧只形成一次执行；进程崩溃后的不确定结果不承诺分布式 exactly-once。
- 未知 namespace、未知 custom 工具、冲突身份或损坏 payload 必须可见拒绝，不能静默吞掉或降为成功。
- 全部 fallback 必须显式、可观测，不能从未知错误、余额不足、401 自动切 wire/凭据。

## 4. 类型与协议模型（R01、R02）

### 4.1 声明层

工具注册信息应声明可用的模型面表示；gate 按本轮 profile 生成不可变工具计划。不得在多个 adapter 中按字符串工具名维护相互独立的特例表。

概念类型如下；最终 Rust 命名由实施确定，但语义必须保留：

```text
ToolDeclaration = Function { name, description, parameters, strict }
                | Custom { name, description, format: Text | Grammar }
                | Namespace { name, description, tools: Function | Custom }

NativeToolCall = { kind: Function | Custom,
                   namespace: Option<String>, name,
                   call_id, item_id: Option<String>,
                   payload: JsonArguments | RawText,
                   completion, provider_state }

CanonicalToolCall = { call_id, internal_tool_key, args: JSON object }
```

- SDK custom 是客户端执行工具，不得借用“服务端 hosted custom”冒充。
- namespace 只允许本期所需的一层，不输出 namespace 嵌套 namespace。
- 工具地址映射为 `(kind, namespace, name) -> internal key + input adapter`，本轮唯一；冲突在请求发出前拒绝。
- 同一 namespace 内公开的 `name` 也必须唯一，不能靠 kind 区分同时暴露的同名 function/custom；kind 用于保存和识别历史调用，不用于放宽声明冲突。
- 旧 function-only 构造器/数据可提供兼容包装；不得迫使 Chat、Messages、Gemini 隐式接收 custom。
- 不支持的 wire/工具格式组合在发请求前显式报配置或能力错误。

### 4.2 入站适配

仅当本轮工具计划声明了 `apply_patch` custom，才允许以下转换：

```text
custom(namespace?, apply_patch, raw_text)
  -> canonical(internal_key=apply_patch,
               args={patch: raw_text, dry_run: false})
```

转换必须早于授权资源提取、写冲突分组、canonical 调用去重和执行。外部原生 payload 另存，用于诊断和精确回放。

本期不启用通用“修复模型参数”。可选的旧 function `input -> patch` 兼容独立于原生模式，默认关闭；若实现，必须满足：只对已声明的 apply_patch、仅字符串、拒绝 `input` 与 `patch` 同时存在、拒绝未知字段、前置生成 canonical 参数并记录转换。禁止仅给 `ApplyPatchArgs` 添加 serde alias。

## 5. 请求与响应形状

### 5.1 DeepSeek 最小声明示例（R06）

下面为最小格式示例，未展示完整请求与其他 function 工具：

```json
{
  "tools": [
    {
      "type": "custom",
      "name": "apply_patch",
      "description": "Apply a Codex-format patch. Send patch text directly, not JSON."
    }
  ]
}
```

默认采用 flat custom，不以文档未承诺的 namespace 作为启用条件。custom 名称必须保持 `apply_patch`，不得为同一端点引入任意自定义 custom 名称。

### 5.2 SIWC 最小声明示例（R07）

```json
{
  "tools": [
    {
      "type": "namespace",
      "name": "functions",
      "description": "Workspace tools",
      "tools": [
        {
          "type": "custom",
          "name": "apply_patch",
          "description": "Apply a Codex-format patch. Send patch text directly, not JSON."
        }
      ]
    }
  ]
}
```

正式 SIWC 原生声明附上完整 Codex Lark grammar；示例省略 `format` 仅用于说明层级。grammar 必须作为带来源与许可证的固定资源维护，验证与本地 parser 接受的目标格式一致，不能把示意省略号发到服务端。

其他本地 function 工具同样按 SIWC 契约进入 namespace。hosted 工具不混装进本地 namespace；维持现有套餐路由的关闭策略。MCP 若已作为本地 function 执行，必须保留唯一可逆身份，不顺带启用 hosted MCP。

### 5.3 custom 调用与结果

```json
{
  "type": "custom_tool_call",
  "id": "ctc_example",
  "call_id": "call_example",
  "namespace": "functions",
  "name": "apply_patch",
  "input": "*** Begin Patch\n*** Add File: example.txt\n+hello\n*** End Patch\n",
  "status": "completed"
}
```

```json
{
  "type": "custom_tool_call_output",
  "call_id": "call_example",
  "output": "[OK] apply_patch — applied: 1 file(s)"
}
```

- flat 路径不要求 `namespace` 字段；namespaced 路径保留真实 namespace。
- function 调用仍使用 `function_call` / `function_call_output`，错误结果也必须使用原调用类型。
- JSON 字段 `input` 是 API 外层传输字段，不是要求模型把补丁再包装为工具参数对象。

## 6. 流式与执行完成性（R03）

### 6.1 必须支持的输入来源

- `response.output_item.added` / `response.output_item.done` 的 custom item。
- `response.custom_tool_call_input.delta` / `.done`。
- `response.completed` 完整输出；非流式完整响应也使用相同解码逻辑。
- `response.incomplete`、`response.failed`、网络中断与取消。

状态至少包含 `Receiving -> Completed | Invalid | Interrupted`；响应是否结束与某个工具 item 是否完成必须分离。

### 6.2 累计与收口规则

1. 按 response 内 item/output index 累计，并维护真实 `call_id`；跨轮不能复用同一个无作用域索引。
2. custom delta 累计原始文本，不走 JSON 参数解析器；function 仍按原有 JSON 契约解析。
3. done/full input 是快照，不是待追加 delta；不允许 delta + snapshot 拼接导致文本翻倍。
4. 完成帧之间的相同调用按身份合并；终态缺 item 时，可从已经完成的 item 累计记录恢复。
5. 尚未提供的身份字段可由后续权威帧补全；两个非空身份值互相矛盾、已完成 payload 不一致等冲突必须报协议错误，不能取“更长字符串”猜测正确值。
6. 只有完整、完成性可靠、身份合法的 item 才能进入执行队列；仅看到 End Patch 不代表调用已完成。
7. 断流只收到部分补丁时可展示进度，但不得写盘或伪造 `{}`、空补丁、成功结果。
8. 某个 item 已可靠完成但整个 response 后续截断时，是否执行按现有调度/取消策略决定；必须有测试和明确证据，不能与半个 item 混淆。
9. 新连接重试不得自动重新执行结果不确定的写调用；先依据调用记录与审计确认状态。

## 7. 安全准入与执行（R04）

- canonical `args.patch` 是权限、写冲突、执行、统计的共同输入；这些消费者不得各自解释 `input`。
- add/update/delete/move 的源路径与目标路径均必须遵循现有提取与授权规则。
- 同文件串行化、跨工作区审批、符号链接拒绝等现有保护不得削弱。
- 未知 namespace 不通过简单去前缀映射为内部工具；只有当前工具计划中的映射有效。
- 保持现有 patch 首匹配、重复路径校验、行尾保持、非原子部分应用行为。
- 拒绝调用与执行失败均产生真实错误结果；多文件部分应用必须继续报告已生效/未生效路径，禁止自动重发整份 patch。
- 原生接入不得绕过 plan mode、子代理工具限制、工具 allowlist 或工具可执行能力过滤。

## 8. 持久化、配对与历史回放（R05、R10）

### 8.1 存储与投影

- 语义 `ToolUse` 可继续存 canonical 参数；另存或扩展类型以保留 `kind/namespace/call_id/item_id/raw input/completion`。
- 可利用 `ResponseOutputItem` 保存 provider 原生状态，但 SDK 必须正式识别 custom 调用并登记配对。
- 不允许只把 custom 存为 `ProviderItem`，再把结果作为普通 `ToolResult` 交给只认识 function 的 normalizer；这会有 orphan result 被丢弃的风险。
- 回放同一 call_id 只输出一个原生调用；raw custom 已覆盖时不得再合成 function。
- normalizer 的 pending 配对需包含调用类型与身份；未知调用类型/缺失配对必须可见，不允许静默丢结果。

### 8.2 新旧会话与 profile 切换

- 旧记录缺类型时仅按旧版本已知 function 格式解释；不能因当前 profile 已启用 custom 就重标旧调用。
- 同会话允许保留旧 function 与新 custom 的历史事实；结果各自保持原类型。
- 工具计划与 profile/能力快照按轮固定。未完成工具批次期间不切换其编码规则。
- 目标端点若拒绝混合历史，必须明确提示并通过新会话或已完成批次后的受控上下文边界迁移；不得伪造原生历史已执行或静默清空历史。
- 切到不支持 custom 的 wire 时，不发送不可编码的 custom item；只可使用有测试的语义降级策略，或显式要求新会话。
- 存储 schema 如新增字段，必须有缺省/版本兼容测试；恢复、压缩与 timeline 重建不能把调用重新变成可执行的第二份。

## 9. provider 策略与配置（R06、R07、R10）

下列配置名为拟定接口，实施时需在类型、DTO、registry/defaults、导出契约与测试中统一确定，不得宣称已存在。

| 能力 | 建议值域 | 旧配置缺省 |
| --- | --- | --- |
| apply_patch 输入 | `function_json` / `codex_custom` | `function_json` |
| Responses 工具布局 | `flat` / `functions_namespace` | `flat` |
| custom grammar | `none` / `codex_lark` | `none` |

T01 拟定字段分别为 `apply_patch_input_format`、`responses_tool_layout`、`apply_patch_custom_grammar`，仍未接入生产配置。首期不实现 function `input -> patch` 修复；跨 wire 无法编码的 custom 历史显式阻塞，不自动语义降级。存储采用 canonical 调用与可选有版本原生元数据分离，具体字段位置在 T07 核对恢复/压缩链路后确定。详见 [实施契约](contract.md)。

### 9.1 目标 profile

| 路径 | 输入 | 布局 | grammar | 准入 |
| --- | --- | --- | --- | --- |
| DeepSeek 账号（OAuth） | custom（仅 type/name） | flat | 先 text；Lark 单独验证 | R12 探针后启用 |
| SIWC GPT + Responses | custom | functions namespace | 经探针验证的 Codex Lark | R12 探针后启用 |
| 其他 Responses/BYOK | 保持原模式 | 保持原模式 | 不推断 | 显式声明能力 |

- 由显式 provider/profile 能力与路由约束选择，不按模型名称包含某字符串猜测。
- SIWC 即使回滚为 function 输入，也必须保持路由要求的 namespace；回滚不是恢复不合法平铺请求。
- DeepSeek 账号（OAuth）只维护 Responses；Chat 仅保留给 BYOK，不在工具失败后自动切换 wire。
- OAuth 模型工具面只把 `apply_patch` 作为 custom；`confirm_apply` / `apply_patch_preview` 不进入 selected，直接 apply。
- DeepSeek custom 范围只开放 apply_patch；声明按官方文档只发 `type/name`，不发 description/format；namespace 与 grammar 约束效果没有探针前不宣称支持。
- 登录刷新、授权 scope、账号余额、凭据头与 origin 守卫维持既有契约；无额外“启用 freeform 的 OAuth scope”。
- SIWC 保留 `store:false`、`stream:true`、完整 input 历史、instructions/developer、禁字段清理和现有 reasoning 回放清理；不得通过本期功能打开 Responses Lite。

## 10. dry-run 与模型面说明（R08、R09）

### 10.1 预检方案

原生补丁文本没有 `dry_run` 参数，本期采用独立 JSON function **`apply_patch_preview`**：

- 参数只含 `patch: string`，适配到内部 `apply_patch({patch, dry_run:true})`。
- 与 apply_patch 共用前置路径提取、风险/权限策略与预检引擎，不在执行器内部绕过准入后临时改名调用。
- 继续生成现有 pending 与 `pending_id`；`confirm_apply` 沿现有确认执行链路工作。
- 原生 apply_patch 表示真正应用；不得在 raw patch 里增加私有 dry_run 指令。
- 非原生 profile 保留原 `apply_patch({patch,dry_run})`；不同时暴露两个同名异构 apply_patch。
- 新 preview 工具需同步 capabilities、权限资源、统计、工具模式过滤、工具说明与测试；不能只注册函数就上线。
- 预检与确认之间的外部文件变化仍按现有重验证语义处理，不承诺预检结果永远可应用。

### 10.2 提示与 UI

- native 描述明确“原始补丁，不包 JSON”；legacy 描述明确 `patch` / `dry_run`。
- 原生模式把预检说明导向 `apply_patch_preview`，不得继续要求给 custom 工具发送 `dry_run:true`。
- 保留首匹配警告、锚定建议与部分应用重发限制，不宣称格式约束可以保证内容命中。
- UI 进度基于 raw patch 的流式 parser；不能为了行数估计把原文当 JSON 解析。
- timeline 展示内部规范工具名与实际状态；需要时保留 namespace 诊断信息，不泄漏凭据或隐藏协议密文。

## 11. 测试、可观测性与发布（R11、R12）

### 11.1 离线验收

SDK、gate、runtime、安全准入、存储恢复分层覆盖；详细矩阵见 [checklist.md](checklist.md)。至少包含 flat custom、namespace function/custom、混合并行、完整响应、流式重复/缺终态、截断、取消、旧会话和跨工作区写。

协议 fixtures 必须包含能区分 item id/call id 的样例，raw patch 带引号、反斜杠、中文和分段边界，不能只测 ASCII 单文件成功。

### 11.2 可观测性与隐私

- 记录 provider/wire、工具计划版本/摘要、kind、namespace、call_id、完成性、canonical 转换类型、拒绝原因与结果编码类型。
- 监控 unsupported kind、身份冲突、orphan result、重复执行抑制、invalid arguments、parse_error、no_match；这些错误类别不得混成“补丁失败”。
- 传输审计与执行审计用同一调用身份关联；正文与补丁按现有敏感数据策略保存，不为调试新增完整默认日志。
- 不读取或导出 session 内部凭据，不打印 token、Authorization 或账号凭据头；验收报告只保留脱敏证据。

### 11.3 真端点探针

用户明确授权后，分别使用已有 DeepSeek 账号与 SIWC 登录运行两轮探针，记录模型、路由、时间、SDK/本仓 revision 与协议摘要：

1. 让模型产生一个 native apply_patch，仅作用于专用临时测试目录。
2. 经正常准入执行，核对磁盘只应用一次，并发送对应 custom output。
3. 模型正常继续回答，且第二轮请求没有重复调用/错误配对/被丢结果。
4. SIWC 额外验证 namespace 内普通 function 与 custom；DeepSeek 验证平铺 function/custom 混合。
5. 重启恢复再继续一轮；再检查 preview + confirm 路径与显式回滚。

没有真实 OAuth 成功证据时，只能标记“实现完成、端点待验收”，不得默认打开。

### 11.4 发布顺序

1. SDK typed 能力与离线测试先落地并发布/pin。
2. QAQ 接线，保持原有 profile 缺省行为，原生模式 opt-in。
3. 通过离线验收与真实端点门禁后，按已验证路由和模型逐步启用。
4. 保留输入格式回滚开关；不回退凭据，不丢弃历史事实，不重放不确定写操作。

## 12. 交付物与完成定义

- SDK 的声明、调用、流式事件、normalizer、序列化与公开 API 文档更新。
- QAQ 的工具计划、前置适配、安全准入、回放、preview、配置/契约导出与说明更新。
- 固定来源/许可证的 grammar 资源及覆盖 parser 接受边界的测试。
- 脱敏协议 fixtures、自动化结果、真实端点双轮/恢复证据及回滚记录。
- [tasks.md](tasks.md) 每项有产物、验证命令与证据；[checklist.md](checklist.md) 门禁项有可复核结果。

实现合入不等于默认启用；默认启用必须满足 R12。真实账号不可用时，明确保留该阻塞，不以 mock 成功代替。
