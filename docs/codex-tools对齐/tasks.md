# Codex 工具协议对齐：任务列表

> 状态：SDK 0.7.0 已发布；T02/T04 主体离线完成，T03 部分完成；T05—T09 为 opt-in 部分进展；T11/T12 待验收。日期：2026-10-27。所有复选框表示实现工作，不表示本次文档编写进度。
> 设计真源：[spec.md](spec.md)；验收真源：[checklist.md](checklist.md)。
> 本任务包含相邻 `mutil-ai` 仓库改造；不能仅在 QAQ gate 手拼请求完成交付。

## 0. 执行约定

- 按依赖顺序实施，任何安全门禁失败不得进入默认启用阶段。
- T11/T12 的真端点结果与启用许可按路由分别记录；一条路由受阻不代表另一条已经通过验收的路由也必须禁用，但不得勾选尚未通过的总项。
- 每项完成时记录：revision、实际修改路径、实际执行命令、结果与证据位置。
- 测试命令是下列推荐入口，不是已经执行的结果；新增 test filter 以最终实现为准。
- 不提交凭据、个人会话原文或完整敏感 patch；网络探针需用户明确授权。
- 如调整工具名、配置字段或存储模型，同时更新三份文档及对应 fixtures，不保留第二套真源。

## 1. 依赖图与交付阶段

```text
T01 基线与协议契约
 └─ T02 SDK typed 声明/身份
     ├─ T03 SDK SSE/完成性
     └─ T04 SDK 配对/回放
          └─ T05 QAQ 工具计划与前置适配（同时依赖 T03）
              ├─ T06 权限/冲突/执行（含 preview 路径）
              ├─ T07 持久化/恢复/旧历史
              └─ T08 provider 配置与路由策略
                   └─ T09 preview 与提示/UI（同时依赖 T06、T07）
                        └─ T10 跨层离线回归（依赖 T02—T09）
                            └─ T11 OAuth 真端点验收
                                └─ T12 发布与回滚
```

T02—T04 属 SDK 阶段；T05—T10 属 QAQ opt-in 阶段；T11—T12 才决定默认启用。

## 2. 工作包

### T01 — 固化基线与协议契约

关联需求：R01、R06、R07、R12。依赖：无。

- [ ] 记录本仓、`mutil-ai`、Codex 参照 revision，复核 spec 第 2 节源码事实。
- [ ] 重新核对 DeepSeek Responses、Codex 集成、SIWC preview limitations 文档。
- [x] 固化 flat function、flat custom、namespace function/custom 的脱敏请求/响应 fixtures（人工合成目标，非线上抓包）。
- [x] fixture 明确区分 `item_id`、`call_id`、namespace、payload 和完成性。
- [ ] 冻结配置字段、typed 声明接口、错误分类与历史版本迁移方案。
- [x] 将 DeepSeek namespace/grammar 效果、具体 GPT 型号能力列为未证实项，不做猜测性默认。

产物：接口说明、来源/revision 记录、脱敏 fixture 集合与能力矩阵。

完成条件：C01 全通过；没有把文档承诺误标为本机 OAuth 实测。

证据：见 [contract.md](contract.md) 的基线、公开来源复核边界及跨仓同步单；`fixtures/tool-protocol.json` 为四组合成目标，`crates/qaqh-gate/tests/codex_protocol_fixtures.rs` 为样本一致性测试。T01 尚未全通过：官方长页部分截断、存储字段位置与调度策略仍待核对；SDK 具体公开 API 已随 0.7.0 发布；不勾选总门禁。测试结果见 contract 第 8 节。

### T02 — SDK 正式建模 function/custom/namespace

关联需求：R01、R02。依赖：T01。

入口：`../mutil-ai/src/types.rs`、`normalize.rs`、`adapter/openai_responses.rs` 及公开构造器。

- [x] 为声明增加 custom text/grammar 与一层 namespace；兼容旧 function 构造器。
- [x] 调用模型显式保存 kind、namespace/name、call_id/item_id、raw input 与完成性。
- [x] 分离客户端 custom 工具与 hosted server tool，不复用错误的 hosted 语义。
- [x] Responses 正确编码声明；不通过 `extra_body.tools` / `input` 绕过保留字段。
- [x] 工具选择、身份校验与唯一性校验覆盖 namespace 与 custom；同 namespace 的同名 function/custom 声明必须拒绝。
- [x] 不支持这些表示的 Chat/Messages/Gemini 路径显式拒绝或仅使用已声明的 function 投影。
- [x] 建立 grammar 资源来源与 Apache-2.0 归属说明；完整语法不得用示意文本替代。

产物：SDK typed API、序列化 fixtures、能力错误测试、公开文档。

完成条件：C02 通过；旧 function 用户无需隐式升级 wire。

证据：mutil-ai 0.7.0（commit `9b33d95379a9e69907c8f55381d05a560b458c27`，tag `v0.7.0`，crates.io 已发布）提供 `ToolDeclaration` / `ScopedToolDeclaration` / `CustomToolFormat` / `NativeToolCall` 与 Responses 声明编码；`cargo test --test tool_declarations` 4 passed（声明形状、作用域唯一性、旧 `ToolSpec` 往返、非 Responses 拒绝），`cargo test --test native_tool_stream` 3 passed。QAQ workspace pin 与 CI checkout 已对齐 0.7.0。grammar 资源在 `qaqh-workspace/assets/apply_patch.lark` 落地并带 Apache-2.0 归属，但 grammar 接受边界测试仍缺，C02.08 保持未勾。

### T03 — SDK custom 流与完成性状态机

关联需求：R03。依赖：T02。

- [ ] 解码 custom output_item added/done、input delta/done、完整 response 输出。
- [ ] 原始文本累计与 JSON arguments 累计分离；UTF-8、转义符和中文分段不失真。
- [ ] done/full input 快照与 delta 合并不翻倍；冲突快照可见报错。
- [ ] item_id/call_id、response/index 作用域正确；不同调用不串缓冲。
- [ ] 已完成 item 在 completed 缺席时仍可恢复，且不会因同时存在正文而丢失。
- [ ] 半个补丁即使带完整结束标记，也不能在缺乏可靠完成性时进入执行。
- [ ] 明确 incomplete/failed/断流/取消下已完成 item 与未完成 item 的不同处理。
- [ ] 重复帧、身份变化、completed/item.done 内容不一致都有失败注入测试。

产物：状态机、SSE fixtures、完整/截断/冲突测试。

完成条件：C03 通过；无 partial text -> JSON Null -> 假 missing-field 错误链。

证据：待补。

### T04 — SDK normalizer 与结果回放闭环

关联需求：R05、R10。依赖：T02；与 T03 联合验证。

- [x] custom 调用正式登记 pending，与结果正确匹配；不只保存在 opaque ProviderItem。
- [x] function 输出 function_call_output，custom 输出 custom_tool_call_output，包括错误结果。
- [x] 原始协议状态与语义调用协同，不双重编码、不双重登记。
- [ ] 同名不同 namespace、不同 kind 的身份不碰撞。
- [x] orphan/missing result 不静默丢弃；提供可定位的报告/错误。
- [x] 旧 function 与新 custom 历史混合回放有测试，不按当前工具名重标旧 kind。
- [x] 其他 wire 的降级/拒绝策略明确，不把 custom raw patch 当成可执行正文。
- [x] 发布 SDK 兼容版本，记录实际版本/revision；QAQ 升级 pin 后再依赖新 API。

产物：正常化/回放实现、迁移测试、SDK 版本与 pin 证据。

完成条件：C04 通过；至少离线复现“调用 -> 结果 -> 下一轮”闭环。

证据：mutil-ai 0.7.0 的 `normalize` 以真实 `call_id` 登记 native pending，按原 kind 回放 `custom_tool_call_output` / `function_call_output`；`cargo test --test native_tool_replay` 4 passed（真实 call_id 配对、跨 wire 拒绝、opaque/typed 双份拒绝、混合历史不重标），`cargo test --test endpoint_dispatch native_custom_call_replays_result_in_next_http_body` 1 passed（loopback 第二轮实际 HTTP body）。Cargo.toml/Cargo.lock pin 0.7.0，CI workflow checkout `9b33d95`。同名不同 namespace 的交叉配对与完整 T03 截断/重连边界仍未验收。

### T05 — QAQ 工具计划与 canonical 入站适配

关联需求：R01、R02、R04。依赖：T03、T04。

入口：`qaqh-tool-core` descriptor、`qaqh-types` ToolDef、workspace manager、gate 与 runtime parse。

- [x] 工具注册声明模型面能力；内部仍只有一个 apply_patch 执行实现。
- [x] 按 profile 生成本轮不可变工具计划，建立 `(kind, namespace, name)` 唯一映射。
- [x] gate 使用 typed SDK 表示，不在 provider 分支重复维护工具特例表。
- [x] native apply_patch 映射为 canonical `{patch: raw_input, dry_run:false}`，原生事实另存。
- [ ] canonical 化早于权限、冲突识别、canonical 去重与执行；覆盖非流式/恢复入口。
- [x] 拒绝未知 namespace/custom 工具、冲突声明和错误 payload 类型。
- [x] 如实施 function `input -> patch` 兼容，默认关闭并按 spec 4.2 严格验证；否则明确保持不支持。

产物：工具计划、typed 投影与前置适配、映射冲突测试。

完成条件：C05 通过；不存在只加 serde alias 的实现。

部分进展（已接 opt-in，不能勾选全部完成项）：`qaqh-tool-core/src/model_tool_plan.rs` 提供 SDK 无关的不可变计划、地址校验、native/canonical 双视图及前置转换；`qaqh-workspace/src/model_tool_plan.rs` 可基于实际 registry 与调用方提供的最终过滤选集生成快照；runtime `gate_request` 在显式 opt-in 下调用 `chat_stream_responses_with_plan`，默认 profile 仍为 legacy function。非流式/恢复入口、profile 能力过滤与 SDK 流式完成性边界仍需验收，不能默认启用原生工具。验证与阻塞见 [contract.md](contract.md) 第 9—11 节。

### T06 — 权限、串行化与执行不变量

关联需求：R04、R08。依赖：T05；preview 对外注册前必须完成对应准入。

入口：`qaqh-permission`、workspace authorization、runtime admit/dispatch、file tools。

- [ ] native 与 legacy 的等价 patch 产生一致授权资源和写冲突键。
- [ ] add/update/delete/move 源目标、跨工作区路径、词法归一与符号链接场景均覆盖。
- [ ] 同文件写仍串行；未知 namespace 不通过截前缀绕过 allowlist。
- [ ] plan mode、子代理限制、工具模式过滤仍生效。
- [ ] apply_patch_preview 的输入适配与目标提取在准入前完成，共用既有风险/权限策略。
- [ ] 保留首匹配、行尾、重复路径、no_match 与非原子部分应用语义。
- [ ] 权限拒绝、执行错误、部分应用继续返回对应原生调用类型的真实结果。
- [ ] 记录不确定执行状态，不以新 call_id 自动重发写操作。

产物：资源提取与调度回归、越权负例、引擎行为回归。

完成条件：C06 全通过；任何资源漏提取或审批旁路均为阻塞。

部分进展（未完成跨层准入）：model_tool_plan 测试验证等价 custom/preview/legacy canonical 目标、同文件分组、跨工作区与外部符号链接审批；file-tools 覆盖 preview 后外部改动、丢弃和过期。新工具尚未注册或进入真实模型调用，plan mode/allowlist 等仍需接线后验收。验证见 [contract.md](contract.md) 第 9 节。

### T07 — QAQ 持久化、回放、恢复与旧会话

关联需求：R05、R10。依赖：T05、T04。

入口：ContentBlock/ToolCall、gate responses 投影、message store、恢复与压缩路径。

- [ ] 保存 native kind/namespace/raw input/completion，或提供等价的有版本协议记录。
- [ ] 回放去重同时认识 function/custom；raw item 已覆盖时不合成第二份调用。
- [ ] SDK pending 登记仍存在，结果不会变成 orphan；原生结果类型不靠工具名推断。
- [ ] 旧缺字段记录仅按旧 function 契约解释；添加向后兼容读取测试。
- [ ] 混合历史、重启恢复、timeline 重建、压缩后仍保留配对，不重新执行历史工具。
- [ ] 批次未完成期间 profile 切换不改变编码；目标不接受历史时提供明确迁移提示。
- [ ] 检查崩溃窗口：执行前、执行后结果未落盘、结果已落盘；不宣称跨崩溃 exactly-once。

产物：存储迁移、回放 fixtures、恢复与崩溃窗口测试。

完成条件：C07 通过；无历史重标、双回放或静默清空。

部分进展：缺结果的恢复占位不再断言“未执行”，改为结果未知并要求先核实；有测试覆盖重建与再次恢复。原生 metadata、SDK 配对和跨 wire 回放仍未实现，见 [contract.md](contract.md) 第 9 节。

### T08 — provider 能力配置与 OAuth 路由

关联需求：R06、R07、R10。依赖：T05；与 T06、T07 联合验证。

入口：EndpointCompat、DTO、registry、provider_for、ResponsesCompat、SDK 请求策略。

- [x] 落地输入格式、工具布局、grammar 三个独立能力，缺省兼容旧 profile。
- [ ] DeepSeek Responses 原生预置：flat apply_patch custom，其他工具 function；OAuth 过滤 preview/confirm。
- [ ] DeepSeek OAuth 只保留 Responses；Chat 仅 BYOK 保留，不在失败后自动切换。
- [ ] SIWC 本地 function/custom 进入 namespace；不误开 Lite/additional_tools/hosted tool_search。
- [ ] SIWC 原生 grammar 使用完整验证资源；模型能力必须来自明确配置/验证记录。
- [ ] 保留账号 token 刷新、凭据头、origin 守卫与不回退 API key 的纪律。
- [ ] 保留 SIWC store/stream/instructions、禁字段与 reasoning 回放清理。
- [ ] 配置、DTO、导出契约、前端可消费类型与文档同步；修正 DeepSeek 仅 Chat 的过时注释。
- [ ] 错误组合在发请求前拒绝；已启用路径能单独回滚输入格式，不破坏 SIWC namespace。

产物：typed 配置、provider presets、schema/导出与配置测试。

完成条件：C08 通过；未验证端点不被全局默认启用。

证据：`EndpointCompat` 已落地 `apply_patch_input_format` / `responses_tool_layout` / `apply_patch_custom_grammar` 三个独立字段，缺省 `function_json` / `flat` / `none`，旧配置零迁移；`cargo test -p qaqh-config --lib native_tool_compat_fields_load_and_roundtrip` 1 passed。runtime 在显式 opt-in 下映射到 plan，DeepSeek/SIWC 的 wire/layout 约束在 `native_tool_plan_profile` 中显式拒绝。字段尚未进入 `qaqh-config` DTO、`qaqh-config-api` 导出或 UI，故 C08.01 不能勾选；DeepSeek/SIWC 真实端点与 preset 仍未验收。

### T09 — preview、提示词与 UI/统计

关联需求：R08、R09。依赖：T06、T07、T08。

- [ ] 注册 JSON function `apply_patch_preview({patch})`，映射至原内部 dry-run 执行路径。
- [ ] 返回现有 pending_id；confirm_apply 不另建引擎，不绕过既有批准与重验证语义。
- [ ] native profile 说明预检走 preview；legacy 保留原 dry_run 参数。
- [ ] 更新 native/legacy 描述、系统工具说明和工具模式过滤，防止模型收到互相冲突的提示。
- [ ] 预检新增工具同步 capabilities、资源提取、统计和冲突处理。
- [ ] raw patch 流进度复用 streaming parser；行数估计、diff、timeline 不依赖 JSON 包装。
- [ ] 保留首匹配与部分应用提示；UI 对“传输损坏/参数拒绝/解析失败/no_match”有真实分类。

产物：preview 工具闭环、模型面说明、UI/统计回归。

完成条件：C09 通过；preview -> pending -> confirm 的结果与磁盘变化一致。

部分进展：`RawPatchLineEstimator` 可直接增量统计 raw patch，不执行或判断完成；尚未接 UI，也未注册 preview 或修改模型提示。见 [contract.md](contract.md) 第 9 节。

### T10 — 跨层离线回归与可观测性

关联需求：R11。依赖：T02—T09。

- [ ] 完成 checklist C01—C10 的请求、SSE、准入、回放、恢复矩阵。
- [ ] 使用 mock server 检查第二轮出站实际 body，不仅检查中间类型。
- [ ] 为重复帧、终态遗漏、身份冲突、UTF-8 分段、断流和取消注入故障。
- [ ] 为混合 function/custom、namespace 同名工具、旧会话与 profile 切换补集成测试。
- [ ] 监控 protocol kind、转换、完成性、orphan/duplicate 与错误分类；审计日志不泄露凭据。
- [ ] 回归其他 wires、tool allowlist、plan mode 与旧 function 默认 profile。
- [ ] 按受影响范围运行格式、编译、单元/集成测试，记录实际命令与结果。

建议验证入口：

```bash
cargo test --manifest-path ../mutil-ai/Cargo.toml
cargo test -p qaqh-types -p qaqh-tool-core -p qaqh-permission
cargo test -p qaqh-file-tools -p qaqh-workspace
cargo test -p qaqh-gate -p qaqh-runtime
cargo fmt --all -- --check
```

SDK 的格式检查在 SDK 仓库单独执行；若仓库有额外约定，以项目实际 CI 为准。上述命令不构成“已通过”的声明。

产物：离线报告、fixture 回归结果、脱敏监控/审计样本。

完成条件：C10 全通过；真实端点未跑时仍保持 opt-in。

证据：待补。

### T11 — 两条 OAuth 路径真实验收

关联需求：R12。依赖：T10；执行前需用户授权与可用账号。

- [ ] 单独申请并记录真实账号探针授权；使用专用临时目录，说明可能消耗额度。
- [ ] DeepSeek 账号 Responses：flat function/custom 混合，产生补丁、执行、回传结果、继续回答。
- [ ] SIWC GPT Responses：namespace function/custom，grammar 声明、执行与第二轮结果配对。
- [ ] 两条路径均验证实际磁盘只应用一次，以及重启恢复后的继续对话。
- [ ] 验证 native preview -> confirm 与输入格式回滚；SIWC 回滚后仍使用 namespace。
- [ ] 记录模型精确标识、端点、时间、两仓 revision、脱敏请求/事件/结果摘要。
- [ ] 对拒绝 namespace、grammar、混合历史等情况明确收口，不静默重试写操作。

产物：每条路由独立的通过/阻塞记录与脱敏证据。

完成条件：C11 通过；账号不可用只能记阻塞，不以 mock 代替。

证据：待补。

### T12 — 分阶段启用与回滚演练

关联需求：R10、R11、R12。依赖：T11。

- [ ] SDK 发布/pin、QAQ opt-in、已验证路由逐步启用分阶段交付。
- [ ] 只为通过验收的路由/模型设默认；其他 provider 缺省不变。
- [ ] 更新使用说明、支持范围、已知限制与升级/历史迁移提示。
- [ ] 演练回滚：输入格式恢复 function，旧 custom 历史不重标，SIWC namespace 保留。
- [ ] 监控 invalid arguments、协议错误、orphan、重复执行与 no_match，区分回归类型。
- [ ] 将测试命令、证据链接、未解决风险填回本文件与 checklist；阻塞项不能勾选。

产物：发布记录、回滚记录、支持矩阵、最终验收报告。

完成条件：C12 通过；安全与历史配对没有未解释的失败项。

证据：待补。

## 3. 合入与默认启用门禁

| 门禁 | 必需完成 | 未满足时 |
| --- | --- | --- |
| SDK API 可消费 | T01—T04 | 不提交依赖未发布或未对齐 pin 的 API 的默认路径 |
| QAQ 原生模式可 opt-in | T05—T10 | 不允许用户进入未完成协议闭环 |
| 特定路由可默认启用 | T11—T12 | 保留 opt-in/禁用状态并标记阻塞 |

## 4. 需求与验收映射

| 任务 | 需求 | 验收分组 |
| --- | --- | --- |
| T01 | R01、R06、R07、R12 | C01 |
| T02 | R01、R02 | C02 |
| T03 | R03 | C03 |
| T04 | R05、R10 | C04 |
| T05 | R01、R02、R04 | C05 |
| T06 | R04、R08 | C06 |
| T07 | R05、R10 | C07 |
| T08 | R06、R07、R10 | C08 |
| T09 | R08、R09 | C09 |
| T10 | R11 | C10 |
| T11 | R12 | C11 |
| T12 | R10、R11、R12 | C12 |
