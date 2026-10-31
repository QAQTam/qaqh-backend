# 会话消息、流式展示与工具闭环故障报告及修复方案

日期：2026-10-31。状态：排查完成；工作包 A（A1–A5）已实施并通过现有测试，B 的后端与 Electron 补载代码已在工作区，C/D 未实施。第 9 节记录实施改动。

基线：backend `8e640b8a` 加当前工作区，以及相邻 `../qaqh-electron`、`../qaqh-sdk` 的实际源码。三处工作区均有既有修改；实现时须保留并衔接这些修改，不能用 HEAD 覆盖工作区。

## 1. 目标与本次约束

交付目标是让新建会话完成以下全过程：正文能恢复、生成时能实时显示、工具成功或失败后均能交回模型、审批后的剩余工具能继续执行、异常不会变成无解释的永久等待。

本轮采用以下约束：

- provider 下发的工具调用 ID 原样保存、原样回传。取消仅为满足内部格式而执行的 call ID 哈希转换。
- 不以兼容旧版会话为交付目标。不建设旧 ID 反向映射、在线迁移、旧执行器 fallback 或双版本运行链。
- 不自动删除用户历史数据。缺少新契约能力的旧会话明确标记为不支持继续执行，引导新建会话；历史读取或离线导出不作为本轮交付前置。
- 保留现有 Ringing v2 单流和单会话决策所有者，使用现有事实、blob、投影及订阅设施。
- 暂不新增回归测试，不执行全 workspace test/check 或反复 typecheck。验收由用户实机完成，按第 8 节逐项记录结果。
- 实现需要生成协议类型及产出可运行安装包时，执行对应生成和构建步骤；这些是交付产物，不额外扩张成长时间检查流程。

本文是消息与执行闭环的专项方案，不将另一个“Ringing 命令面统一”工程作为修复前置。其他文档中与本次旧会话兼容、测试交付要求冲突的部分，以以上用户约束为准；不因此改变配置和账号数据的处理规则。

## 2. 结论与证据等级

| 编号 | 问题 | 证据及当前状态 | 优先级 |
| --- | --- | --- | --- |
| F01 | 切 session 后 assistant/工具正文变成“正文加载中…” | 后端返回正文引用，前端需要额外下载；当前 Electron 工作区已有未提交的补载代码，之前的针对性测试通过，用户安装包是否包含该代码未确认 | P0 |
| F02 | 正文、思考只有完整块结束后才显示 | 后端接收流式片段但未向展示通道发布；v2 对话契约和前端 reducer 没有对应的生成中更新链路，源码确认 | P0 |
| F03 | 工具结束后 loop 退出，下一轮模型请求不发送 | 实机会话日志确认，provider ID 与返回结果 ID 不同；临时读工具探针也复现相同错误 | P0 |
| F04 | 同一模型响应中，审批后的后续工具没有继续调度 | 实机该轮声明两个 exec，仅第一项进入执行；源码在首次审批处提前返回，响应后没有恢复剩余列表的调度位置 | P0 |
| F05 | 崩溃后恢复再次失败，仍可能对外暴露可用状态 | 实机日志出现 reconciliation 证据缺失；构造器记录恢复错误后继续初始化为 Ready，源码确认 | P0 |
| F06 | 新建无绑定工作区会话的 exec 在沙箱准备时失败 | 指定会话真实输出为默认工作目录不存在，exit_code=-1；目录存在性和初始化责任需一并修复 | P0 |
| F07 | 大 completion 投递失败后可能永久等待 | 源码确认：固定预留预算、超限返回错误、调用方忽略 deliver 错误后退出；不是此次已指定会话的直接原因 | P1 |
| F08 | 快照与实时事件覆盖顺序可能导致内容回退 | 激活时存在宿主 watch/bootstrap 与 renderer loadBootstrap 两条路径；作为流式接线必须核对的时序风险，尚未独立复现 | 随 F02 收口 |
| F09 | Electron 指定 cwd 后，新会话仍落到默认 workspace | 前端参数正常传递；分配阶段不再写初始 metadata，但创世阶段仍回读该 metadata，丢失请求 cwd；源码和实际会话创世记录确认 | P0 |
| F10 | config.toml 关闭 sandbox 后 exec 仍受只读挂载限制 | 配置确实为 false，工具上下文却使用硬编码 true；实机日志确认 linux_bubblewrap 的 read_only_file_system | P0 |
| F11 | 活跃会话修改工作区/执行配置后，工具端口未同步 | workspace.set 写事实后发送 AgentReloadConfig，该命令仅更新 provider_spec，未更新工具 policy/workspace；源码确认 | P0 |

F01 的补载和 F02 的流式发布解决不同阶段的问题。修复正文下载不会自动恢复流式；只补实时事件也不会让历史正文引用自行解析。

### 2.1 指定会话的实际失败链

会话：`01a19073-7475-74db-a653-f35ebead0fa4`。时间为北京时间。

1. 13:39:50，模型响应声明两个 exec。
2. provider 原始第一项 ID 是 `call_00_MLH98vXviYLqLG233K9o1920`，内部声明改为 `call_4MS35H295AA2HFR738NMM0EN9P`。
3. 第一项审批通过后开始执行；exec 返回沙箱准备失败，默认工作根不存在。`status:"completed"` 表示调用结束，不能据此判定成功；该结果的 `exit_code` 是 `-1`。
4. `ToolFinished` 和工具上下文消息已落盘为 fact 45、46；返回上下文使用转换后的 ID，assistant 上下文保留 provider 原始 ID。
5. 13:39:53，准备下一轮输入时，完整调用/结果序列校验抛出 `context checkpoint is invalid: unmatched tool result`。错误向上退出 driver，下一轮模型请求未发出。
6. worker 随后重启，恢复事实又因 `reconciled:true` 缺少证据被拒绝。

因此，这次不是“工具结果没有回到后端”，也不能描述为“exec 命令成功执行”。结果返回、结果持久化、模型继续、前端正文显示是四个不同环节。

日志证据：`~/.config/qaqh/qaqh-daemon.log` 的此次排查位置约为 30–34 行；日志可能滚动，不能把行号当长期定位。会话事实位于对应 `sessions/<session_id>/committed-batches/batches.jsonl`。

### 2.2 源码定位

| 责任 | 位置 |
| --- | --- |
| 模型流片段只进入本地缓存，工具进度被忽略 | [provider_job.rs](../crates/qaqh-agent-runtime/src/provider_job.rs)，`run_spec` |
| 原始模型消息持久化与 call ID 转换分离 | 同上，`prepare_response_commit_facts`、`canonical_tool_call_id` |
| 结果使用内部 call ID、准备下一轮、审批后调度 | [agent.rs](../crates/qaqh-agent-runtime/src/session_agent/agent.rs)，`tool_result_context_fact`、`settle_tool`、`next_round`、`start_declared_tools`、`control` |
| 调用/结果字符串匹配及整个历史的 seen_calls | [context_projection.rs](../crates/qaqh-session/src/context_projection.rs)，`request_messages`、`validate_complete_sequence` |
| call ID 格式和 reconciliation 证据校验 | [validation.rs](../crates/qaqh-session/src/session_fact_v2/validation.rs) |
| 恢复失败后的启动行为 | [agent.rs](../crates/qaqh-agent-runtime/src/session_agent/agent.rs)，`new`、`recover_open_work`；[host.rs](../crates/qaqh-agent-runtime/src/session_agent/host.rs) |
| 现有累计状态式实时事件及单流发布机制 | [v2.rs](../crates/qaqh-ringing-runtime/src/v2.rs)，compact replaceable 发布与订阅 |
| 正文引用、最终完整块 | [conversation.rs](../crates/qaqh-session/src/projection/conversation.rs)；[projection_event.rs](../crates/qaqh-session/src/session_fact_v2/projection_event.rs) |
| 默认工作根只计算路径 | [platform.rs](../crates/qaqh-types/src/platform.rs)，`default_session_workspace_dir`；[service.rs](../crates/qaqh-runtime/src/service.rs)，`session_canonical_cwd` |
| completion 预算与错误被忽略 | [agent.rs](../crates/qaqh-agent-runtime/src/session_agent/agent.rs)，`dispatch_task`；[mailbox.rs](../crates/qaqh-agentloop-core/src/mailbox.rs)；[supervisor.rs](../crates/qaqh-agentloop-core/src/supervisor.rs) |
| 前端正文回填 | [content-loader.ts](../../qaqh-electron/webui/src/session/content-loader.ts)、[reducer.ts](../../qaqh-electron/webui/src/session/reducer.ts)、[store.ts](../../qaqh-electron/webui/src/session/store.ts) |
| Electron 下载与事件转发 | [ringing-client.ts](../../qaqh-electron/src/main/ringing-client.ts)、[ipc-handlers.ts](../../qaqh-electron/src/main/ipc-handlers.ts)、[electron.ts](../../qaqh-electron/webui/src/lib/transport/electron.ts) |

## 3. Git 追溯：设计起点与此次回归分开认定

| 日期（北京时间） | Commit | 实际引入内容 |
| --- | --- | --- |
| 09-20 00:56 | `d649ef77` | 字段契约首次要求 ToolCallId 使用 `call_<ULID>` |
| 09-20 13:51 | `9c422429` | Rust 类型和校验器落实该约束 |
| 09-23 10:06 | `b2b07cf5` | 旧 ToolRuntime 增加 `canonical_call_id(wire_call_id)`，内部账本以哈希 ID 记账；模型结果链仍持有原始 ID |
| 10-30 18:11 | `42e62f4a` | 删除 Timeline 和旧执行链，新增 thin loop/provider job；旧逐片段发布路径随之删除 |
| 10-30 20:39 | `7c0c5f60` | 拆除分支引入新的 `canonical_tool_call_id`，新结果构造使用内部 ID，却没有还原 provider ID |
| 10-31 01:32 | `68ff2767` | main 侧新 crate 中也引入上述转换；当前 provider_job 的普通 blame 主要指向该提交 |
| 10-31 11:24 | `13e0e6ec` | 合并并保留新链路；不是内部 ID 设计的首次来源 |

结论：ID 规范是 9 月的旧设计；此次回归来自新执行链混用内部记账身份与 provider 协议身份。只看昨天的文件名或今天搬家后的 blame，会遗漏设计起点。

## 4. 目标契约

### 4.1 工具调用 ID 保留 provider 原值

`ToolCallId` 表示 provider 给出的调用 ID，作为不透明字符串保存。校验保留非空、合法传输及资源预算等必要约束，移除 `call_` 前缀和 ULID 形状要求；不能再为了适配账本格式改写 ID。

所有模型协议输出引用原始调用 ID。对 Responses 应引用 `call_id`，不能误用输出 item 的 `id`。原始 provider 输出中的关联字段也必须保持一致。

内部定位复用既有会话、回合、轮次、attempt 及 execution/event 身份，不再生成一个替代 provider ID 的独立 call ID。需要跨轮唯一定位时使用带作用域的键，例如 `(session_id, turn_id, round, attempt, call_id)`。

必须同时调整以下假设，不能仅删除转换函数：

- 一个模型响应中的调用 ID 不得重复；不同轮次或响应允许出现相同 provider ID。
- `validate_complete_sequence` 的去重/匹配按调用所在响应及待完成集合判断，不能用跨整个会话的全局 seen_calls 拒绝合法后续调用。
- commit state、工具账本、active/pending 表、审批授权、结果提交和恢复均引用同一个带作用域调用；协议需要携带的轮次/响应归属必须持久化，不能只存在进程内。
- 审批响应必须指向具体 interaction 和调用作用域，不能因 provider ID 重用而批准另一次执行。
- `execution_id` 保持一次具体执行的身份，`event_id` 保持事实/事件身份。它们不替换模型请求中的 call ID。
- 同步更新所有受影响的 Rust/TypeScript DTO、解码器、消费者与协议文档。

### 4.2 生成中状态与持久事实

模型上下文仍以 committed response、tool result 和终态事实为准；生成中的正文/思考/参数只更新展示状态，不能用于提前执行工具。

现有 v2 单流补齐以下运行时更新，名称在实现时统一到既有类型命名规则：

| 更新 | 必要内容 | 语义 |
| --- | --- | --- |
| assistant block progress | turn、round、attempt、稳定 block ID、正文/思考类别、revision、累计文本值 | replaceable，后一个版本完整替换同一生成中块 |
| tool input progress | 所属响应作用域、稳定 provisional key、已收到的原始 call ID/名称、revision、累计参数 | replaceable，仅展示；参数未完整校验时不可执行 |
| attempt 终止/清理 | attempt 身份、原因、相关块身份 | 明确取消、失败、被替代；防止旧帧复活 |
| sealed/finished | 最终正文引用、完整模型输出及可靠终态 | 延续持久事实语义，覆盖生成中展示并释放运行时状态 |

replaceable 事件携带累计状态，不把可以被合并/丢弃的片段当成可直接追加的 delta。每个身份的 revision 单调递增；旧版本、重复帧和旧 attempt 均不可覆盖新状态。可靠 cursor 与运行时 revision 分开维护。

block 身份在开始生成时分配，最终封口复用或显式关联该身份；不能流式显示一份、封口后再追加另一份。provider 回调不提供 block ID 时，按响应作用域和输出片段顺序分配稳定身份，并在最终结果中携带对齐信息。

### 4.3 资源边界

- 合并连续片段后发布，可先以约 30–50 ms 的发布间隔实机调节；不能每 token 重建整份会话或写一条可靠事实。
- 生成中状态只保存活动 attempt 所需内容，封口、取消、失败、session 卸载时释放；hub 仅保留每个稳定身份的最新版本。
- 大累计文本达到事件帧预算时，使用内容引用发布最新快照，避免放大单帧上限或无限扩容。外置是合并后的快照，不是每 token 写盘。
- 前端先直接渲染已收到的正文。最终封口引用尚未读回时保留已有文本，避免“正在输出”突然退回“正文加载中…”。
- 结果大小限制和流式发布节流不能静默截掉最终模型上下文或用户正文。

## 5. 修复工作包

### A. 恢复工具执行闭环（先实施）

#### A1. 原始 call ID 与作用域

移除 `canonical_tool_call_id` 及相应哈希调用，修改 schema 校验和调用键作用域。检查声明、工具端口、审批、结果、恢复及模型协议转换全过程。使用真实 provider 返回的 ID 实机运行，确保“收到的 ID = 发回的 ID”。

结果成功、命令退出非零、沙箱准备失败、拒绝审批、取消和超时都应得到明确处理。可确定的失败结果应交回模型继续决策；执行是否真正停止仍不确定时，保留 indeterminate 语义，不能伪装成功或自动重执行。

#### A2. 保存完整工具调度列表

模型声明全部工具后，owner 保存完整列表和处理位置。遇到 ask/permission 可暂停，但响应后必须继续剩余列表；拒绝一项也不能遗忘后续项。可以保留现有有界并发，审批按明确顺序处理，无需另建调度框架。

“还有未执行的声明”也算未完成工作。下一轮模型请求只能在该响应所有调用均有匹配结果、没有待审批/待回答、没有活动执行时启动，不能只判断 `active_tool_identities.is_empty()`。

若调度中途出错，已启动调用继续被 owner 追踪；未启动调用明确结束或保留待处理身份，不能丢弃列表后直接开下一轮。

#### A3. 默认工作目录建立后才可执行

无绑定工作区时，在会话运行环境初始化中建立默认工作目录，并把同一个实际路径传给模型说明、ToolCallContext、沙箱 writable roots 和进程 cwd。已有用户工作区不存在时应明确报错，不应悄悄新建一个同名空项目掩盖问题。

目录创建失败必须暴露为明确的会话/工具错误。不得将 `status:completed` 单独映射为成功；结合工具终态、exit_code 和错误字段展示。

#### A4. 恢复事实和启动状态

先区分“有真实完成证据的结果恢复”与“前进程已丢失、执行结果未知”。只有前者可以声明 reconciliation，并附实际证据引用；后者使用真实的中断/不确定终态及 recovery 引用，不伪造结果或证据。

恢复批次还须为未完成调用提供合法的上下文闭合，使 TurnInterrupted 和后续请求能够通过序列校验。新增作用域后保证闭合引用原始 provider ID，多个未完成工具也逐项处理。

恢复失败时构造/启动明确失败或进入 Blocked，禁止仅打印日志后继续 Ready；错误要到达会话状态和前端。driver 异常退出同样必须结束“无解释等待”，不能把日志中的错误自动算成业务已恢复。

本项覆盖按新契约写出的会话发生崩溃后的恢复；不承担此前已写坏会话的在线修补。

#### A5. completion 可靠到达

移除 `let _ = sup.deliver(...)` 的静默失败。超预算 completion 在进入 mailbox 前外置大正文，发送轻量引用及必要执行证据，仍由 session owner 提交事实。blob 写入与 journal writer 权限分开，worker 不获得事实写入权。

交付失败必须保留结果所有权并进入可观测处理；可重试的投递与不可恢复的 owner 关闭应区别处理，不能无限空转。actor 在业务结果持久化后才 acknowledge completion；下一轮 dispatch 失败不能把已持久化结果重新当作未完成执行。

### B. 完成历史正文与切换恢复

以 Electron 工作区现有 `ContentLoader` 为基础收口，不重复建设下载缓存。

1. bootstrap 和 sealed/tool-finished 引用通过 `readContentText` → 固定 IPC → SDK downloadContent 下载，回填 user、assistant、思考、工具参数和输出。
2. 对应后端内容端点保留 `sha256:` 引用处理、归属授权及持久 session blob 回落；缓存失效不等于正文丢失。
3. 缓存按不可变引用去重，并限制并发和占用；失败显示明确状态，有重新加载入口，不能永久停在加载占位。
4. 回填同时核对当前 ref、session 激活代次和块身份，旧请求不能覆盖已替换内容，后台卸载后不能被迟到下载重新填满。
5. 同步更新 answer 派生文本与实际 step，避免正文已下载但最终答案仍显示占位。
6. 将现有未提交补载代码实际打入安装包，核对实机版本；源码存在不算发布完成。

### C. 恢复流式发布与渲染

#### C1. 后端接通 provider 片段

`run_spec` 将 ContentDelta、ReasoningDelta、ToolCallProgress/可支持的原生参数进度送入有界的运行时展示通道。与执行结果 completion 分开，展示拥塞可以合并进度，但不能阻塞或丢掉终态结果。

这些更新经现有 v2 hub 形成第 4.2 节的 replaceable 事件。最后的可靠 sealed/tool/turn 事实仍由 owner 提交；成功、失败、取消、重试都完成活动块清理及部分内容呈现。

#### C2. 订阅与快照对齐

由宿主/SDK 管理一次有水位的 bootstrap 和流续接，renderer 消费这次基线与后续事件。梳理当前重复 bootstrap 路径，避免晚到的旧快照覆盖已经收到的进度或可靠事实。

bootstrap 应能恢复已提交正文和当前活动输出；可以复用 hub 的最新 replaceable 状态，不新增第二套 transcript。切回会话、重连时恢复最新累计值，而不是重放所有 token。

若 renderer 仍需主动重拉，必须带有明确的快照水位、请求代次和进度 revision 合并规则。单靠“最后一个 HTTP 响应胜出”不能保证正确。

#### C3. SDK、Electron 和 UI 同步

同步新协议类型与解析分支，确认 SDK、Electron 主进程和 preload 不筛掉新事件。renderer reducer 支持生成中的块更新，维护块顺序、流式指示、思考状态和工具参数状态；Markdown 在最终封口前也实际刷新。

高频更新只触碰对应块，不每帧扫描全部历史、增加结构版本并重建整棵消息树。最终 seal 将既有块转为完成状态，正文相同不闪烁、不重复，后续工作出现时保留已有分段顺序。

### D. 新契约边界与交付

明确新会话的契约版本/能力标记，集中在创建和打开入口校验，不能运行到工具结果阶段才发现不支持。使用现有 schema/version/caps 设施完成必要标记，不为此次修复另立一个网络大版本。

新旧客户端组合不支持新能力时给出明确状态，不假装已连接且能继续运行。旧会话只标记不支持执行并保留原始数据；本次实机验收统一新建会话。

同一批功能交付包含 backend、SDK、Electron；各工作包可以分提交实现，不能只发布消费者或只发布生产者就宣布修复完成。修改相关文档和生成类型，清理被替代的转换函数及旧分支。

## 6. 实施顺序与范围控制

| 顺序 | 工作内容 | 可实机观察的完成条件 |
| --- | --- | --- |
| 1 | A1 + A2 + A3，必要的 DTO/消费者同步 | 新会话真实 exec 能运行；成功和失败结果原 ID 回传；多工具审批后继续，模型给出后续回答 |
| 2 | A4 + A5 | driver/恢复/大结果不再无解释卡住；新契约会话异常重开有明确结果 |
| 3 | B | 完成会话切走再切回，正文、思考、工具输入输出恢复；失败不是永久占位 |
| 4 | C1 + C2 + C3 | 最终回答之前已经显示正文/思考；切换和重连恢复当前输出；工具参数可见且不提前执行 |
| 5 | D，构建并交付同步版本 | 安装包具备新能力；不支持的会话/客户端明确拒绝执行 |

必要范围包含会话事实类型及校验、context projection/commit state、provider/tool jobs、session agent/host、v2 hub、会话工作根初始化、SDK 类型和消费、Electron IPC 和 reducer/store。不会仅改一个 ID 函数就结束，也不扩张到恢复 Timeline、替换整个执行框架或统一全部管理 API。

## 7. 既有验证如何解释

此前已执行的针对性验证，仅作为本次排查证据：

- Electron 正文补载、卸载与 IPC 下载等相关测试共 10 项通过；其中名为 streaming 的既有测试主要检查封口后的分段，不能证明逐片段输出正常。
- 后端原有 `session_agent_tool_loop_and_multi_round` 通过，其 provider fixture 使用符合内部格式的调用 ID，未覆盖此次 ID 转换路径。
- 临时复制该读工具用例并换成真实 provider ID 格式，复现 `Err(Commit(Context(InvalidCheckpoint("unmatched tool result"))))`；临时文件已删除。

这些不是修复后的验收结果。本轮文档编写未新增测试，也未运行 build/check/test。

## 8. 实机验收单（由用户执行）

使用本批 backend、SDK、Electron 同步构建的版本，统一新建会话。每项记录会话 ID、时间、可见结果及对应日志；不记录账号密钥。无需以冗长自动检查代替下列真实行为。

| 场景 | 操作 | 通过标准 |
| --- | --- | --- |
| 默认工作根 | 不选项目，新建会话，让 agent 执行简单只读 exec | 默认目录存在，沙箱准备成功，agent 根据真实结果继续回答 |
| 工具正常完成 | 让模型执行一个能成功的简单命令 | 相应结果交回模型，实际发起下一轮请求并给出后续回答 |
| 工具确定失败 | 执行一个明确退出非零的命令 | 展示真实失败；失败结果交回模型，agent 能继续解释或处理 |
| provider ID | 对照同一次模型响应和后续模型输入 | 调用原 ID 与返回 ID 相同；没有内部哈希 ID 替换；Responses 的 item id/call_id 不混用 |
| 同轮多工具 | 让同一个模型响应产生至少两个 exec，首项需要审批 | 批准后执行剩余项；所有结果齐全才发下一轮；拒绝首项也不丢后续项 |
| 长回答与思考 | 要求较长的分段说明；使用提供可展示思考的模型 | 最终结束前可见多次内容更新；提供思考时实时显示；结束不复制出第二份 |
| 生成中切 session | 输出未结束时切走，再切回 | 恢复该会话当前累计输出并继续更新；其他会话不串内容 |
| 完成后消息恢复 | 完成含工具的会话，切走再切回或重开应用 | 用户输入、正文、思考、工具参数和输出可读，不永久停在加载占位 |
| 重连 | 生成中暂断模型/本地连接后恢复，或触发宿主重新订阅 | 有明确连接状态；恢复最新内容，无重复追加和旧快照覆盖 |
| 取消/重试 | 生成中取消，再发新请求；可再覆盖 provider 自动重试 | 旧 attempt 的迟到帧不复活旧输出，新请求持续工作 |
| 大工具结果 | 让工具产生超过原 256 KiB completion 预算的结果 | 不永久等待；最终正文有可读引用，模型收到符合既有上下文预算的结果或明确限制信息 |
| 新契约异常恢复 | 在专门会话执行中结束 daemon，再重启 | 不伪造工具成功；新会话事实恢复或明确 Blocked，不出现 reconciliation 字段错误后假 Ready |
| 旧会话边界 | 打开不符合新契约的旧会话 | 明确不支持继续执行，能新建会话；原数据不被自动删除或静默改写 |

前三项和同轮审批续跑未通过前，不将“能实时显示文字”当成整项修复完成。已生成正文、工具结果和可靠终态各自正确到达，才算消息与执行闭环交付。


## 9. 实施记录（工作包 A）

本轮只实施了 A1–A5，未新增回归测试；验证方式为既有测试 + 全 workspace `cargo check --all-targets`。

### A1 调用 ID 原值

- 删除 `provider_job::canonical_tool_call_id`；`ToolCallDeclared` 与 `start_declared_tools` 直接使用 provider 给出的 `id`。结果上下文、审批授权、工具账本因此引用同一个字符串。
- `validation.rs` 新增 `validate_tool_call_id`（非空、≤256 字节、无控制符/空白），替换 call_id 与 source_call_id 处的 `call_<ULID>` 形状校验；subagent 内部的 `parent_call_id` 仍保持内部前缀。
- `context_projection::validate_complete_sequence` 的重名判定改为按单个 assistant 响应去重；跨回合复用同一 provider ID 不再被判 "duplicate tool call"。
- `commit_state` 的工具账本改成 `(TurnId, ToolCallId)` 键；恢复完成事实按 call_id 唯一解引用。

### A2 完整调度列表

- 新增 `DeclaredTool` 队列 `SessionAgent::declared_tools`。`start_declared_tools` 现在只入队，`advance_declared_tools` 逐项推进。
- `ask` 与权限请求只挂起列表（剩余项留在队列里），`ResolveInteraction`、`ToolPermissionRespond`（批准与拒绝）后继续推进。
- `next_round` 与新增的 `settle_round_progress` 都以"队列为空、无活动执行、无待决交互"为前提；`finish_turn` 清空队列。
- 同时修正审批应答的消费顺序：先确认挂起的交互确实是该调用，再移除权限记录，避免 ask 场景下误删。

### A3 默认工作目录

- `session_canonical_cwd` 改为返回 `Result`：默认工作根按需创建，用户自选工作区不存在时明确报错；`session.create`/`session.new` 在建会话阶段即失败。
- 新增 `platform::is_managed_session_workspace`：数据根 `workspace/` 下的目录由 daemon 负责创建，其余路径必须已存在。
- `registry::spawn` 的主会话与子会话装配改用同一解析（原先无绑定工作区时错误地落到会话存储目录）；系统提示中的 workspace 也改为该实际路径，不再使用 daemon 进程 cwd。

### A4 恢复

- 未完成工具恢复不再写 `reconciled: true`；改为 `reconciled: false` + `recovery_ref`，并保持 indeterminate/cancelled 语义。
- 恢复批次为每个未完成调用补一条 `ToolResult` 上下文消息，使 `TurnInterrupted` 能通过完整序列校验；`SessionRecovered.actions` 同步带上 `ToolFinished`。
- `SessionAgent::new` 在恢复失败时进入 `Blocked` 并跳过排队输入，不再打印日志后继续 Ready。

### A5 completion 投递

- `dispatch_task` 的后台观察任务不再 `let _ = sup.deliver(...)`。
- `Supervisor::resize_reservation`：按实际 completion 大小重新预留通道容量后投递；超过 completion 通道 4 MiB 帧上限时记录明确错误，结果仍由 supervisor 持有而不是丢弃。

## 10. 实施记录（工作包 C：生成中发布）

### C1 后端接通 provider 片段

- `ConversationDelta` 新增两个 ephemeral replaceable 变体：`AssistantBlockProgress`（累计正文/思考 + 稳定 `block_id`）与 `ToolInputProgress`（累计原始参数文本）。二者都不是事实，不进入 canonical log。
- `provider_job::run_spec` 现在接收 `ProgressStream`：ContentDelta / 可展示的 ReasoningDelta / ToolCallProgress / NativeToolProgress 以 40 ms 窗口合并为累计快照发布，首帧立即发布；无论成功、失败还是取消，结束后都会补发一次完整累计值。
- 新增 `progress` 模块：`ProgressSink` / `AttemptBlocks` / `ProgressStream`。`AttemptBlocks` 在开始生成时分配 reasoning 与 answer 两个 block 身份，`prepare_response_commit_facts` 封口时复用同一身份，因此流式文本与最终块是同一块，不会出现"流一份、封口再追加一份"。
- `V2ProjectionHub` 新增 `publish_assistant_progress` / `publish_tool_input_progress` / `clear_*`，按 block/call 身份写入 `replaceables`（每个身份单调 revision，重连只重放最新值），并复用既有单流 ephemeral 通道。
- `AgentRegistry` 装配 `V2ProgressSink`（hub + 会话目录）：主会话与子会话都在传 `v2_hub` 时接线；sink 失败只记 debug，不影响生成。尝试结束、回合收尾、重试替代时清理活动身份。

### C3 SDK / Electron / UI

- `qaqh-sdk` 与 `qaqh-electron` 的生成类型补上两个变体（SDK 只改 `ConversationDelta.ts` 并同步 `manifest.json` 的文件哈希与 `contract_hash`；未整目录重生成，避免把 SDK 尚未跟进的 timeline 删除等无关漂移一并带入）。
- renderer reducer 支持生成中块：同一 `block_id` 的后续帧只替换该块，封口事件复用同一 id 直接转完成态；工具参数帧只更新展示，状态保持 `pending`，不触发任何执行。
- 流式帧跳过 `recomputeDerived` 的结构版本递增和待补载扫描，避免每帧重建整棵消息树。

### 验证

- backend：`cargo check --workspace --all-targets` 无错误；`qaqh-session` / `qaqh-ringing-runtime`（54 项）/ `session_agent_test` / `host_direct` / `v2_acceptance_matrix` 与 `thin_loop_seam_probe` 22 项全通过。
- `qaqh-sdk`：typecheck + 36 项测试通过（含契约 manifest 哈希校验）。
- `qaqh-electron`：`tsc --noEmit` 与 81 项脚本测试通过。

### 未实施

- B：正文补载后端端点与 Electron `ContentLoader` 已存在，本轮未改动；打包与实机核对未做。
- C2 的重复 bootstrap 路径合并未动：现有宿主 watch/bootstrap 两条路径保持原样，仅依赖 replaceable 重放提供最新累计值。
- D：契约能力标记未加；不符合新契约的旧会话目前不会被拒绝，只会在工具结果阶段暴露 ID 不一致。
- 显示侧累计文本仍受 64 KiB 上限约束，超过后实时显示停在已收部分，封口事件才给出全文（未做成外置快照）。

## 11. 补充排查：指定 cwd 丢失与 sandbox 开关失效

本节记录上述实施之后新增的实机反馈，以本次补充排查时的磁盘源码为准。第 9、10 节原有实施记录保留；其“默认目录已经创建”不代表指定 cwd 已能正确传递。本次仅补充报告，不修改业务代码，不新增或运行测试/check，也不修改用户配置。

### 11.1 F09：cwd 在后端创世前丢失

已核对的参数流：

```text
Electron submitStartupSession(options.cwd)
  → createSessionWithCwd({ cwd })
  → transport.rpc("session.create", { cwd, ... })
  → Electron service_rpc / SDK 转发 params
  → backend SessionCreateParams.cwd
  → allocate_session(args.cwd)
  → create_new_session：仅建目录与 identity，cwd 只用于工作区分组
  → session_canonical_cwd：回读 journal/index 中尚未存在的 cwd
  → default_session_workspace_dir
  → SessionCreated.cwd 写入默认目录
  → worker 按这个默认目录执行
```

断点不是 Electron 的目录选择或 IPC 丢字段。`create_new_session` 已不再持久化初始 metadata；`session.create` 和 `session.new` 随后仍通过 `session_canonical_cwd` 从 metadata/index 回读 cwd。分组中的 workspace_id 不能自动补回运行目录，因此可能出现侧栏归属于项目、实际工具却在默认目录执行的情况。

Git 起点是 `b4d4b0e6`（2026-10-31 00:59，北京时间），该提交为保证 SessionCreated 是第一事实，删除了分配阶段写入 `meta.cwd` 的代码，但调用方未改为直接传递请求 cwd。禁止恢复创世前写 metadata 来掩盖断链；正确的修复是在运行时创世入口接住请求参数。

修复要求：

1. 为创建流程解析一次实际 cwd：显式 cwd 优先，只有未指定才生成受管理的默认目录。显式路径的存在性和规范化在创建阶段处理，非法路径明确拒绝，不能降级到默认目录。
2. 将解析后的路径直接写入首个 SessionCreated；不要在创世前从尚未创建的会话 metadata 中回读请求值。
3. 从已提交事实装配 worker，模型提示、ToolCallContext.workspace_root、exec 默认 cwd 和 sandbox writable roots 使用同一个路径。
4. `session.create`、`session.new`/控制通道创建、子会话创建等入口使用一致的参数传递规则，不能只修 Electron 的首选 RPC 后让 fallback 继续丢参。
5. 侧栏归组与实际工作目录分别定义；移入某个分组本身不隐式切换 cwd。用户明确选目录创建时，归组和运行目录都应反映该选择。

源码：`webui/src/tabs/store.ts::createSessionWithCwd`（Electron）；backend `service/handlers/session.rs` 创建分支、`qaqh-session/src/manager.rs::create_new_session`、`service.rs::session_canonical_cwd`。

### 11.2 F10：配置读取正常，执行上下文硬编码开启沙箱

已只读取非敏感配置项，确认当前用户配置为 `[sandbox] enabled = false`；配置解析器也确实将其读入 `Config.sandbox.enabled`。

但 `ToolPolicySnapshot` 没有携带这个开关，`WorkspaceToolPort::prepare` 中实际构造如下：

```rust
SandboxSpec::for_permission(
    policy.workspace_root.clone(),
    policy.permission_level,
    true,
)
```

第三个参数就是 sandbox_enabled。`SandboxSpec::for_permission(..., false)` 已提供禁用路径，但新执行链永远传 true，所以配置关闭不影响 exec。

这里的 `SandboxMode::Main/Subagent` 是调用角色与准入策略，不是操作系统沙箱开关；不能通过切换 Main/Subagent 代替传递 enabled。提高 permission_level 也不会自动禁用文件系统沙箱；当前 for_permission 在开关为 true 时仍保留 writable roots。

Git 起点是 `42e62f4a`（2026-10-30 18:11），新 tool_job 引入了该硬编码。旧 `engine_tool` 调用使用的是 `ctx.agent.config.sandbox.enabled`；此次属于重构丢失配置接线。

修复要求：

1. 在 session assembly 从有效配置和实际 workspace 解析完整 SandboxSpec，作为工具执行策略快照的一部分传给 ToolPort；工具准备阶段消费这个值，移除硬编码 true 和重复推导。
2. role、permission_level、操作系统 sandbox.enabled 分别保留各自含义。关闭 OS 沙箱不自动等同于跳过工具准入或审批。
3. tool intent 的 sandbox_spec_hash 必须对应本次真正使用的 spec，替换当前固定字符串的哈希；日志与实际执行不能报告不同策略。
4. trusted roots 仅扩展已启用沙箱的可写范围，不能意外重新启用显式关闭的沙箱。
5. disabled 时 exec 的普通管道与 PTY 路径都应消费同一 disabled spec，不能有某一后端重新默认开启的旁路。

源码：`qaqh-config/src/config.rs` sandbox 解析、`session_agent/assembly.rs::tool_policy`、`tool_job.rs::ToolPolicySnapshot/prepare`、`qaqh-policy/src/lib.rs::SandboxSpec::for_permission`、`qaqh-process-tools/src/exec/handler.rs`。

### 11.3 两个问题叠加后的实机表现

只读错误对应会话 `01a190bb-930a-7423-a938-2d4fe3281f8b`。其 SessionCreated.cwd 为：

```text
/home/qaqtamsy/.config/qaqh/workspace/01a190bb-930a-7423-a938-2d4fe3281f8b
```

daemon 日志中的 `sandbox_denial` 明确标记 `backend:linux_bubblewrap`、`reason:read_only_file_system`。相关调用 ID 是 `call_00_W5GGcPZW2W0oNRjZpFeP5318` 和 `call_00_XEQ1XcH6Boh7rAddu2EB6302`，输出显示在 `/home/qaqtamsy/Projects/qaqh-backend` 下创建临时文件、touch 失败。

解释链为：实际 writable root 错落到默认目录，同时 sandbox 被强制开启；命令中即便 cd 到项目，也只改变进程工作目录，不改变 sandbox 的挂载授权，所以能读项目却不能写。当前日志能确认本次拒绝来自 bubblewrap，不能把它当成宿主磁盘已经整体只读的证据。

仅创建默认目录解决的是原 F06 的“沙箱准备找不到目录”，不会解决 F09/F10。仅让命令自行 cd 或提高审批权限，也不能完成这两项修复。

### 11.4 F11：重载只改模型，未改工具环境

`workspace.set` 会持久化 cwd，随后发送 AgentReloadConfig；当前 registry 将其转换为 `reload_provider`，owner 只替换 provider_spec。已有 `WorkspaceToolPort` 的 workspace_root、sandbox spec 和 permission snapshot 不随之更新。

因此，即使用户后来把 cwd 修正到项目，仍可能出现列表已变、exec 环境没变的第二处断链。配置关闭沙箱也不能仅凭 reload 命令成功认定已在活跃会话生效。

修复采用完整的执行配置重载：owner 接收有效 provider、工具执行策略及实际工作根，在明确的调度边界更新。下一次工具派发使用新快照；已经启动的工具继续使用派发时的策略，不能宣称被事后切换。更新 cwd 后还要同步下一轮模型的工作目录说明；会话写入成功和运行环境应用成功均须有明确结果。

### 11.5 增补实施顺序与实机验收

F09、F10、F11 应与工具闭环一起优先修复，不能等流式 UI 完成后再处理。依次完成“创建参数直达创世事实 → sandbox 有效配置进入工具快照 → 活跃 owner 完整重载”，再交付同步版本。

| 场景 | 操作 | 通过标准 |
| --- | --- | --- |
| 显式项目目录启动 | Electron 选择已有项目，新建会话后查看模型说明并运行 pwd | SessionCreated.cwd、模型工作根、默认 exec cwd 都是所选项目，无默认 workspace 回退 |
| 不指定目录启动 | 新建无绑定工作区会话后运行 pwd | 默认目录已创建且与事实、工具执行一致 |
| 关闭沙箱 | sandbox.enabled=false，新建会话，在用户指定的可写测试目录创建并删除一个标记文件 | 不启用 bubblewrap；文件操作受宿主真实权限约束，执行和输出一致 |
| 开启沙箱 | sandbox.enabled=true，以选定项目为工作根，在项目内及明确未授权的另一个目录各试一次写入 | 项目内可写，未授权范围被拒绝；cwd 变化不扩大 writable roots |
| 活跃会话切目录 | 调用 workspace.set，下一次 exec 运行 pwd | metadata、模型后续说明、工具工作根和启用沙箱时的 writable roots 一致更新 |
| 活跃会话改开关 | 通过配置保存/重载切换 sandbox.enabled，再派发新 exec | 新工具使用更新后的策略，已有运行中进程仍按原策略完成；不需要靠重建会话才能掩盖重载遗漏 |

以上新增验收仍由用户实机执行，不新增回归测试或长时间 check。

### 11.6 实施记录（F09 / F10 / F11，2026-10-31）

第 11 节的三条断链已按 11.5 的顺序修复；第 11 节之前的工作包（A/C/B/D）未在本轮改动范围内，一行未动。

**F09 创建参数直达创世事实**

- `service.rs::session_canonical_cwd` 增加 `requested` 参数：显式请求优先，且只解析一次。显式路径必须存在、必须是目录，否则整次创建失败，不再降级到默认目录；默认目录仍由后端创建。
- `session.create` 与 `session.new` 两处都传入请求中的 `cwd`；创世事实、worker 工作根、模型提示、ToolCallContext 与沙箱可写根因此同源（worker 经 `meta::read` 重放 `SessionCreated` 取 cwd，不读第二份真值）。
- 子会话：`AgentRegistry::spawn_subagent_in_workspace` + `SubagentSpawnOptions.workspace` + `SubagentHost::spawn_subagent` 把显式 workspace 直接写进子会话创世事实；原 `spawn_subagent` 保留为 `None` 的兼容入口，测试调用点未改。
- Windows：`canonicalize` 的 verbatim 前缀在入库前剥离（`normalize_workspace_path`），避免 `\\?\C:\...` 进入事实、提示与 UI。

**F10 有效配置进入工具快照**

- `ToolPolicySnapshot` 新增 `sandbox_spec`；`assembly::tool_policy` 从 `config.sandbox.enabled` + workspace + permission level 解析完整 spec，`spawn`/`reload` 共用。
- `WorkspaceToolPort::prepare` 直接消费快照里的 spec，删除硬编码 `true` 与二次推导；disabled 时管道与 PTY 两条路径拿到同一个 disabled spec。
- `SandboxSpec::with_trusted_roots` 在 `enabled == false` 时不再扩展可写根；`ToolPolicySnapshot::trust_dir` 与端口 `trust_dir` 同步维护可写根，信任目录不会把已关闭的沙箱重新打开。
- 审批挑战（`authorization.rs`）改为沿用上下文里真实 spec，不再用 `for_permission(..., true)` 重建。
- `ToolIntent.sandbox_spec_hash` 改为 `WorkspaceToolPort::sandbox_spec_hash()`：对当前 spec 的规范化 JSON 取 sha256，替换固定 `b"sandbox_spec"` 占位；日志、账本与执行看到同一份策略。

**F11 活跃 owner 完整重载**

- `AgentControl::ReloadProvider` 携带 `policy: Option<Box<ToolPolicySnapshot>>`；`AgentHost::reload_provider(spec, policy)` 一并投递。
- `messaging.rs` 在投递前同时解析 provider 与工具策略（effective config + 当前 workspace + 会话角色；子代理用 Subagent 角色），解析失败返回带原因的错误，不再折叠成笼统的 "unavailable"。
- actor 在控制边界 `update_policy`：下一次派发使用新快照，已在执行的工具保持派发时的策略；工作根变化时追加一条 `ContextMessageOrigin::Feature` 系统消息，下一轮模型能看到新的工作目录（frozen prefix 不再被改写）。

**验证**

- `cargo check --workspace --all-targets` 通过。
- `cargo test -p qaqh-policy --lib`（18）、`-p qaqh-agent-runtime --lib`（14）、`-p qaqh-workspace --lib`（57）、`-p qaqh-runtime --lib`（136）全部通过。
- 未执行 daemon 启停、Electron 联调与 11.5 的实机验收；那 6 条仍按原样由用户执行。

**已知未覆盖**

- 旧 ambient 执行链（`qaqh-workspace/src/execution.rs::execute_with_ambient`、`authorization::legacy_tool_call_context`）里的 `true` 仍在；生产工具链经 `WorkspaceToolPort` 不经过它，本轮未改动以免与 11 节之前的工作包冲突。
- 冻结前缀里的旧工作目录不会被重写，只由上述 Feature 消息补充当前目录；这是保持前缀可缓存的有意取舍。
