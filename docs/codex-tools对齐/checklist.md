# Codex 工具协议对齐：验收清单

> 状态：C02/C04/C05 的离线子项已验收；C02.08、C05.03、C08、C11/C12 及真实端点仍待验收。日期：2026-10-27。
> 设计：[spec.md](spec.md)；工作包：[tasks.md](tasks.md)。
> 勾选必须附可复核证据。本文不是已实现/已通过报告，不得因文档写完而勾选实现项。

## 0. 验收与证据规则

- 自动化证据记录测试名称、实际命令、revision、退出结果及 fixture/报告位置。
- 真实端点证据记录路由、精确模型标识、时间、两仓 revision、脱敏协议摘要和磁盘核对。
- 失败/阻塞保持未勾选，并给出原因；禁止用“模型看起来会用”替代协议闭环。
- 任何越权、双执行、结果丢失、历史重标或凭据泄漏都阻塞上线。
- C01—C10 是原生 opt-in 的离线门禁；C11—C12 是对应路由默认启用的门禁。

## C01 — 基线、来源与范围（T01）

- [x] C01.01 记录 QAQ、SDK 与 Codex 参照 revision，源码引用可定位。
- [ ] C01.02 官方文档重新核对并记录日期；区分文档支持与真实账号实测。
- [x] C01.03 未将截图归因为已证实的后训练来源，未将 no_match 宣称为 freeform 自动可解。
- [x] C01.04 DeepSeek namespace、grammar 执行效果与具体 GPT 能力均有明确“已验证/未验证”标记。
- [ ] C01.05 冻结输入格式、布局、grammar、存储与错误分类契约，三份文档一致。

证据：[contract.md](contract.md) 第 1—5 节记录 revision、未验证能力和实施契约；spec 1.1 保留错误分层与推断边界。C01.02 仍未全完成（公开长页被截断，需定点补证）；C01.05 仍未全完成（存储字段位置与调度策略待核对，SDK API 已随 0.7.0 发布）。合成 fixtures 的一致性验证不等于 C02—C10 或真实端点验收。

## C02 — typed 声明与身份（T02）

- [x] C02.01 function、custom text、custom grammar、namespace 生成预期 JSON。
- [x] C02.02 namespace 内可混合 function/custom；本期不输出多层 namespace。
- [x] C02.03 `(kind, namespace, name)` 映射唯一，同 namespace 内同名 function/custom 也判声明冲突；在发请求前拒绝。
- [x] C02.04 namespace、name、call_id、item_id、payload 与完成性均不丢失。
- [x] C02.05 custom 是客户端工具，不误走 hosted server tool 分支。
- [x] C02.06 不覆盖 extra_body 的 tools/input；保留字段守卫仍有效。
- [x] C02.07 旧 function 构造器/旧 profile 行为保持；其他 wires 未隐式切原生。
- [ ] C02.08 grammar 资源完整、有来源/revision/许可证归属，本地 parser 接受边界有对照测试。

证据：mutil-ai 0.7.0（commit `9b33d95379a9e69907c8f55381d05a560b458c27`，tag `v0.7.0`，crates.io 已发布）的 `cargo test --test tool_declarations` 4 passed、`--test native_tool_stream` 3 passed；覆盖 flat/namespace 声明、同作用域同名冲突、旧 `ToolSpec` 往返、非 Responses 拒绝与 custom 流快照。`cargo tree -p qaqh-gate -i mutil-ai` 为 0.7.0，CI checkout 同 revision。C02.08 仍未勾：`qaqh-workspace/assets/apply_patch.lark` 有完整资源与归属测试，但 parser 接受边界对照未完成。

## C03 — SSE、终态与故障注入（T03）

- [ ] C03.01 added -> input.delta -> input.done -> item.done -> completed 正常路径不失真。
- [ ] C03.02 仅 item.done 或完整 response 包含调用时也能还原完成调用。
- [ ] C03.03 custom 原文不走 JSON arguments 解析，含引号/反斜杠/中文/空行仍正确。
- [ ] C03.04 UTF-8 与多段传输边界不损坏补丁文本。
- [ ] C03.05 delta + full snapshot 不翻倍；相同重复帧不产生第二份调用。
- [ ] C03.06 允许后续权威帧补全缺失身份；冲突 snapshot 或已知非空 kind/namespace/name/call_id 互相矛盾产生可见协议错误。
- [ ] C03.07 terminal 缺少调用但已收到可靠 item.done 时，调用不因同轮有正文而丢失。
- [ ] C03.08 item id 与 call id 不同仍正确回传配对；不同 response 的 index 不串流。
- [ ] C03.09 未可靠完成的 item 在断流/取消后不进入执行、零文件写入；不伪造空参数或成功结果。已完成 item 按 C03.11 单独验收。
- [ ] C03.10 收到 End Patch 但未可靠完成时仍不执行。
- [ ] C03.11 response 截断时，已完成 item 与未完成 item 按明确策略分别处理。
- [ ] C03.12 网络重试和新 call_id 不自动重做结果不确定的写操作。

证据：待补。

## C04 — SDK 配对与出站回放（T04）

- [x] C04.01 custom 正式登记 normalizer pending，不只保存 opaque item。
- [x] C04.02 function -> function_call_output，custom -> custom_tool_call_output，包括失败结果。
- [x] C04.03 回传真实 call_id，不拿 ctc_/fc_ item id 顶替。
- [ ] C04.04 同名不同 namespace、混合 function/custom 调用结果不交叉匹配。
- [x] C04.05 不存在调用 raw/typed 双份编码或 pending 双登记。
- [x] C04.06 orphan/missing result 可见报告，不能静默丢失后继续宣称成功。
- [x] C04.07 第二轮实际出站 body 可通过 mock server 校验，不仅中间结构正确。
- [x] C04.08 SDK 发布版本/revision 与本仓 pin 一致，没有只在本机 path dependency 下可用。

证据：mutil-ai 0.7.0 的 `cargo test --test native_tool_replay` 4 passed（真实 call_id 配对、跨 wire 拒绝、opaque/typed 双份拒绝、混合历史不重标），`--test endpoint_dispatch native_custom_call_replays_result_in_next_http_body` 1 passed（loopback 第二轮实际 HTTP body 含 `custom_tool_call` / `custom_tool_call_output`），`openai_responses` 单测覆盖 `function_call_output` 与 namespaced function 回填。C04.04 仍未勾：同名不同 namespace 的交叉配对没有独立测试；C04.08 由 Cargo.lock 0.7.0 与 CI pin `9b33d95` 互证。

## C05 — 工具计划与 canonical 化（T05）

- [x] C05.01 本轮计划不可变，可逆映射 namespace/name/kind 至正确内部工具。
- [x] C05.02 native input 映射为 args.patch，dry_run=false；补丁文本逐字保留。
- [ ] C05.03 转换发生在权限、冲突与执行前，非流式/恢复调用不遗漏。
- [x] C05.04 未知 namespace/custom、错误 payload 类型与冲突身份被拒绝。
- [x] C05.05 未通过任意截前缀规则把恶意/未知工具映射为内置工具。
- [x] C05.06 原生 payload 与 canonical 参数分别保存，诊断不伪称模型发送了 canonical JSON。
- [x] C05.07 未仅添加 serde input alias；可选 function 别名兼容默认关闭且拒绝双字段歧义。

证据：`cargo test -p qaqh-workspace --lib model_tool_plan` 10 passed（不可变快照、flat/namespace 可逆映射、native/custom/preview 身份、allowlist 不扩张、冲突与未知项拒绝、grammar 归属）；`cargo test -p qaqh-tool-core --lib native_patch_representation` 1 passed；`cargo test -p qaqh-gate --test codex_protocol_fixtures` 6 passed（声明→流式→canonical 解码）；`cargo test -p qaqh-runtime --lib native_tool_plan` 2 passed（opt-in 映射与默认 legacy）。C05.03 仍未勾：非流式/恢复入口的 canonical 化没有端到端证据。

## C06 — 权限、冲突与引擎回归（T06）

- [ ] C06.01 等价 native/legacy patch 提取相同目标路径与写冲突键。
- [ ] C06.02 Add/Update/Delete/Move 源目标均进入正确授权资源。
- [ ] C06.03 跨工作区写仍需要既有批准，不因 raw input 而返回空资源集合。
- [ ] C06.04 符号链接最终组件/隐藏逃逸等既有拒绝行为不变。
- [ ] C06.05 同文件双补丁、补丁与其他写工具冲突仍串行，不丢更新。
- [ ] C06.06 plan mode、子代理限制、tool allowlist 不被 namespace/custom 旁路。
- [ ] C06.07 权限拒绝与引擎失败均回传正确 kind 的错误结果。
- [ ] C06.08 保留行尾、首匹配、@@ 锚定、End of File、重复路径校验。
- [ ] C06.09 no_match 保持真实失败，不为兼容自动猜旧内容或扩大修改范围。
- [ ] C06.10 多文件部分应用仍报告已生效/未生效；不自动重发整份补丁。
- [ ] C06.11 preview 的路径/风险检查在准入前完成，不通过执行器内部改名绕过授权。

证据：`qaqh-tool-core` 的 `model_tool_plan` 测试覆盖等价 native/legacy/preview canonical 目标、同文件写分组、跨工作区与 Unix 外部符号链接审批；`qaqh-file-tools` 覆盖 preview 后外部改动、一次性丢弃与过期。新工具尚未注册进真实模型调用，plan mode、子代理限制与 allowlist 的跨层验收未完成，故 C06 保持未勾。

## C07 — 持久化、恢复与迁移（T07）

- [ ] C07.01 新记录保留 kind/namespace/call_id/item_id/raw input/completion 或等价有版本信息。
- [ ] C07.02 raw custom 覆盖语义 ToolUse 时不再合成 function_call。
- [ ] C07.03 旧无类型记录按旧 function 解释，不根据当前 profile 重标为 custom。
- [ ] C07.04 旧 function 与新 custom 混合历史的输出类型逐项正确。
- [ ] C07.05 重启恢复后调用结果配对不丢失，历史工具不再次执行。
- [ ] C07.06 压缩/timeline 重建后不出现第二份可执行调用或幽灵工具卡。
- [ ] C07.07 未完成批次中的 profile 切换不改变原调用编码。
- [ ] C07.08 不支持混合历史/目标 wire 时明确阻塞或受控迁移，不静默清空/伪造历史。
- [ ] C07.09 执行前、执行后结果未落盘、结果已落盘三个崩溃窗口有测试与恢复说明。
- [ ] C07.10 不以同轮去重测试宣称跨崩溃 exactly-once。

证据：待补。

## C08 — provider 与 OAuth 契约（T08）

- [ ] C08.01 输入格式、布局、grammar 独立配置，旧配置缺省兼容且 schema/DTO/导出一致。
- [ ] C08.02 DeepSeek Responses：flat apply_patch custom，其他工具为 function；OAuth 不暴露 preview/confirm。
- [ ] C08.03 DeepSeek 不发送未验证 namespace；其他 custom 名称不在允许范围内。
- [ ] C08.04 DeepSeek OAuth 拒绝 Chat；BYOK Chat 保留原 function，不会在失败后自动切 Responses。
- [ ] C08.05 DeepSeek 账号仍用官方 origin 和 x-dsh-auth-token 裸头，未改为 Bearer/API key。
- [ ] C08.06 SIWC function/custom 都按本地工具布局进入 namespace，身份映射可逆。
- [ ] C08.07 SIWC grammar 为完整资源；没有因启 namespace 开启 Lite/additional_tools/tool_search。
- [ ] C08.08 SIWC store=false、stream=true、完整 input、instructions/developer、禁字段和 reasoning 清理仍有效。
- [ ] C08.09 无凭据跨 origin、401 自动换凭据、scope 猜测或客户端冒充。
- [ ] C08.10 其他 BYOK/兼容端点默认不变；能力来自显式配置，不按模型子串猜测。
- [ ] C08.11 SIWC 输入回滚为 function 后仍保持 namespace。

证据：配置字段已落地：`EndpointCompat` 的 `apply_patch_input_format` / `responses_tool_layout` / `apply_patch_custom_grammar`，缺省 `function_json` / `flat` / `none`，`cargo test -p qaqh-config --lib native_tool_compat_fields_load_and_roundtrip` 1 passed；runtime `native_tool_plan_profile` 对 DeepSeek/SIWC 的 wire/layout 组合显式拒绝。C08 仍不勾选：字段尚未进入 `qaqh-config` DTO、`qaqh-config-api` 导出或 UI，C08.01 的 schema/DTO/导出一致性不成立；DeepSeek/SIWC 真实端点、preset 与回滚演练未验收。

## C09 — 预检、说明、UI 与统计（T09）

- [ ] C09.01 native profile 暴露 apply_patch_preview({patch}) function，不把 dry_run 塞进 raw patch。
- [ ] C09.02 preview 复用原引擎返回 pending_id，磁盘无补丁写入。
- [ ] C09.03 confirm_apply 真正应用/丢弃/过期/重复确认行为与原路径一致。
- [ ] C09.04 预检后文件被外部修改时，确认按当前文件重验证，不承诺必然应用。
- [ ] C09.05 legacy profile 仍有原 patch/dry_run，不同时存在同名异构 apply_patch。
- [ ] C09.06 native/legacy 提示词与实际 schema 一致，首匹配与部分应用警告保留。
- [ ] C09.07 preview 同步注册 capabilities、权限、工具模式、统计与冲突规则。
- [ ] C09.08 raw patch 流的行数/进度不经 JSON 解析；diff 与 timeline 正常展示。
- [ ] C09.09 UI/模型可区分传输损坏、参数拒绝、parse_error、no_match 与权限拒绝。

证据：待补。

## C10 — 跨层回归与隐私（T10）

- [ ] C10.01 SDK 与 QAQ 的相关编译、单元、集成测试有实际命令与结果。
- [ ] C10.02 格式检查与项目实际 CI 通过；未以未运行命令冒充验证。
- [ ] C10.03 请求编码 -> SSE -> canonical -> 授权 -> 执行 -> 回放至少有端到端 mock 测试。
- [ ] C10.04 其他 wire、普通 function、tool mode、子代理与 plan mode 回归通过。
- [ ] C10.05 审计可关联 call_id/kind/namespace/完成性/结果类型，错误分类可定位。
- [ ] C10.06 orphan、身份冲突、重复抑制和 invalid arguments 有可观测记录。
- [ ] C10.07 fixtures/日志/报告没有 token、账号凭据头、个人会话或非必要完整 patch。
- [ ] C10.08 未通过真实端点时配置保持 opt-in，不将 mock 作为默认启用依据。

证据：待补。

## C11 — 真实 OAuth 双轮与恢复（T11）

执行前提：用户授权、有效登录、专用临时目录、明确可能消耗额度。不得为填写清单擅自读取凭据文件。

- [ ] C11.01 记录探针授权、范围、时间、精确模型标识、端点和两仓 revision。
- [ ] C11.02 DeepSeek 账号 Responses 产生 flat custom 调用，经正常准入执行成功。
- [ ] C11.03 DeepSeek 第二轮收到 custom_tool_call_output 并继续回答，无丢结果或重复写。
- [ ] C11.04 DeepSeek 同轮 function/custom 混合调用及其配对正确。
- [ ] C11.05 SIWC 接受 namespace 内 function/custom 与完整 grammar 声明。
- [ ] C11.06 SIWC 第二轮正确配对 custom output，并继续回答，无协议 400 或重复写。
- [ ] C11.07 两条路径均以磁盘证据确认补丁只应用一次，而非只相信模型自然语言。
- [ ] C11.08 两条路径重启恢复后继续一轮，历史类型与配对保留。
- [ ] C11.09 两条路径 preview -> confirm 通过；回滚输入格式后仍能合法继续或明确迁移。
- [ ] C11.10 证据脱敏；namespace/grammar/混合历史被拒绝时明确记录能力限制，不静默兜底。

证据：待补。真实账号不可用时填写阻塞原因，不勾选对应项。

## C12 — 发布、默认启用与回滚（T12）

- [ ] C12.01 SDK 发布/pin、QAQ opt-in、指定路由默认启用按阶段交付。
- [ ] C12.02 默认启用严格限制在已完成 C11 的路由/模型，其他 profile 不受影响。
- [ ] C12.03 回滚演练不改凭据/扣费来源，不重标旧 custom 历史、不重放不确定写。
- [ ] C12.04 SIWC 回滚仍合法 namespace；DeepSeek 不做自动 wire 迁移。
- [ ] C12.05 发布说明写明支持矩阵、未验证能力、历史迁移方式与回滚方式。
- [ ] C12.06 所有阻塞安全项已关闭，任务列表与本清单证据可相互定位。

证据：待补。

## 13. 最终结论记录模板

| 项目 | 结果 | 证据/阻塞 |
| --- | --- | --- |
| SDK 离线协议与配对 | 部分通过 | C02.01–07、C04.01–03/05–08、C05.01–02/04–07 已勾；C02.08、C04.04、C05.03 及 T03 截断/重连仍待补 |
| QAQ 安全、回放、preview | 待验收 | 待补 |
| 其他 provider 回归 | 待验收 | 待补 |
| DeepSeek OAuth 真端点 | 待验收 | 待补 |
| SIWC OAuth 真端点 | 待验收 | 待补 |
| 默认启用范围 | 未批准 | 待补 |
| 回滚演练 | 待验收 | 待补 |

验收人/时间：待补。最终结论必须区分“离线实现完成”“真实端点通过”“允许默认启用”。
