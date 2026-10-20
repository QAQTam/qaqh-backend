# 消息内容单元、撤回与 fork

状态：2026-10-20 校准的 v2.1 → v3.0 目标契约；当前源码的 MessageStore、provider adapters 与客户端尚未迁到这些边界。
当前产品持续变化，实施前按 [能力保留矩阵](preservation.md) 刷新证据，交付边界见 [rollout](rollout.md)。
本文件具体化 [agent](agent.md) 的消息路径与 [protocol](protocol.md) 的消息 DTO。
采用细粒度 transcript item；保留 provider response bundle 的完整语义，不继续以整条 assistant 作为唯一 UI 消息身份。

## 1. 四层标识

| 层 | 身份/序号 | 作用 |
|---|---|---|
| 会话事实 | event_id、durable_seq | 单一提交顺序、重放、applied cursor |
| 展示/内容单元 | message_id、msg_seq | 独立可引用的正文、思考、调用、结果与注入 |
| 供应商响应 | response_id、request_id、attempt_id、provider_part_key | 原始响应/parts/签名/关联/顺序；完整重放 |
| 运行依赖 | turn_id、call_id、execution_id、context_revision | 输入、模型请求、实际执行、上下文版本 |

message_id 是 actor 分配的全局唯一不透明字符串，创建一次不修改；不是 ContentRef、数组下标或时间戳。
msg_seq 是本 session 接纳内容单元的顺序，canonical u64 十进制字符串，**0 起步**。
空 transcript 用 next_msg_seq="0"，last_msg_seq=null；不要把 msg_seq=0 当空 transcript。
durable_seq 仍 1 起步、0 表示空事实日志；两域不可互换。
同一单元的流式更新、finalize、重放、重连不分配新 message_id/msg_seq。
撤回、compact、UI 隐藏不回退 next_msg_seq，不填空洞、不重新编号。
ID/序号在 MessageOpened 提交后才成为公开身份；失败不得向客户端发布未提交身份。
actor 在一个 session commit 内分配并记录，恢复由 facts/持久元数据恢复，不从活跃数组长度猜。

msg_seq 用于同 session 的展示排序；继承到 fork 的身份仍是原 message_id，新 session 有自己的排序域。
命令重试按 command_id/body digest 返回原消息身份；用户再次主动发送相同文字仍是新消息。
相同工具完成回执按 execution identity 去重，不通过“内容相同”判为同一消息。

## 2. 单元粒度与服务器顺序

TranscriptItem 的 kind 为 system、injection、user、reasoning、text、tool_call、tool_result、media。
status 为 opened/completed/incomplete/failed/cancelled；live 正文是 opened 上的易失 overlay。
active/inactive、inherited/frozen 和 visibility 是另外的投影属性，不改写已完成 item 的执行结论。
一条用户输入作为一个 item，内部保留有顺序的文本/附件 parts；供应商输出按语义 part/item 分单元。
每个工具调用独立 item，每个工具结果独立 item；工具进度不产生新消息。
tool_call opened 时 provider call ID/name/参数可能尚不完整：先用 provider_part_key 固定 item 身份，
finalized 时提交完整 call 关联和参数；ToolIntentCommitted 前不可执行，不能用初始空参数抢跑。
一个 provider text/reasoning part 跨多个 delta 仍是同一个 item；不是每个 SSE frame/token 一个 ID。
同一个响应可以产生多个 text/reasoning parts；不能按 kind 把它们重新合并到固定 thinking/chat/tool 区域。

正常例子（数字列是 msg_seq，不是 opaque message_id）：

| msg_seq | kind | 关联 |
|---|---|---|
| 0 | system | 会话初始化配置版本 |
| 1 | injection | 初始化工具/技能说明与能力快照 |
| 2 | user | input / turn A |
| 3 | reasoning | response A / part r0 |
| 4 | text | response A / part t0 |
| 5 | tool_call | response A / call X |
| 6 | tool_call | response A / call Y |
| 7 | tool_result | execution Y，先完成 |
| 8 | tool_result | execution X，后完成 |
| 9 | text | response B / part t0 |

此表是有实际初始化注入的例子；工具 schema/capability snapshot 如果仅作为 provider request 元数据，
不必再伪造一条 injection 文本或把相同工具清单重复放进上下文。序号不规定某个 kind 永远占固定槽位。

顺序由 actor 接纳语义单元开始的顺序确定，既不按 role 分组，也不按完成时间倒排。
timestamp 只展示；服务器发送时间、设备接收时间、日志 timestamp 都不是排序权威。
工具 Y 先完成可以先展示；下一次 provider request 按供应商所需的调用/结果关联与顺序投影，
不能把 UI 的完成顺序直接当模型 API 消息数组顺序。
msg_seq 在 MessageOpened 固定，慢单元晚 finalized 不改变位置。

并发 delta 可以交错：r0 delta → t0 delta → r0 delta；r0 的后续 delta 仍更新同一 ID。
首期 transcript 展示“语义单元开始顺序”，不承诺把全部历史 token 接收顺序逐字重放。
需要过程观察时用 live 观察顺序；若未来要持久逐片交错渲染，须新增 segment/event trace，
不能靠已合并文本、timestamp 或新造每 token msgid 猜回交错过程。

ProviderEvent 必须保留 request/attempt、稳定 provider_part_key、kind、part opened/delta/closed、
供应商原始输出位置与必要关联。不能只交 ContentDelta(String)/ReasoningDelta(String)，
最后 Done 再猜 part 顺序。无流式接口时按供应商返回的明确 parts 顺序合成 opened/closed。
供应商不给开始事件时，首次有可识别 part key 的 delta 是 opened；不能只按类型切换猜 part identity。
adapter 无法保真时返回 UnsupportedContentLayout，不把多个不明边界的签名块合成字符串。

## 3. 展示单元与 provider bundle 分开

provider 原始 role、parts 顺序、签名、encrypted/redacted state、call IDs 与 usage 存入私有 response bundle。
每个 transcript item 保留 response_id/provider_part_key，公共 UI DTO 只暴露获准的可展示内容。
reasoning item 是供应商明确提供的可展示数据；隐藏私有状态不能包装成可见 reasoning。
reasoning 可以定位、折叠或做展示隐藏，不意味着可以修改供应商签名或删掉私有状态后继续请求。
context 的 provider projection 使用 bundle 与工具关联，不能直接遍历 UI items 并拼 role。
当前运行请求固定完整 ContextSnapshot、config/capability snapshot 及 context_revision。
新 UI 结构不得破坏完整 provider message 的保真重放。
上下文等价与缓存前缀稳定分别验收。固定快照内的 system/环境说明、工具 schema/别名映射及顺序应可确定恢复，
同一配置/序列化版本下由 Chat 与 Responses 的实际 HTTP 请求体捕获验证；跨版本变化明确声明，不永久冻结旧 JSON 编码。
新 request/attempt 不改变既有逻辑输入，新增用户轮次不重新排序已提交历史；缓存亲和键不充当 execution 幂等键。
同一 assistant 响应含工具调用时仍保留非空正文；thinking 类别、effort、usage/缓存及工具关联不能在 adapter 迁移中丢失。
工具 call ID/参数增量遵循 [providers](providers.md)，无法执行的上游调用仍保留可见失败，不伪造可恢复执行。

GenUI 围栏是受限的展示内容，不是新模型 role 或执行授权。需保留原文以供历史重放，流式 renderer 可作易失增量投影；
终稿覆盖流式状态，刷新/历史加载与实时生成须一致，且不能因 snapshot/replay 自动再次提交 followUp 或执行工具。
GenUI v0.1/v0.2 的版本分发、具名编译失败、surface local state 与交互白名单见 [protocol](protocol.md)。

内部 MessageOpened 先持久身份/顺序/依赖元数据，live delta 以 after_durable_seq 锚定它。
Closed/Finalized 时先提交正文 blob，再提交 item finalized；流中失败保存 incomplete 与确切已获得内容。
已 finalized item 可独立持久，但其所属 response 不一定已成为可运行上下文。
只有 ResponseCommitted 验证完整 bundle、parts/调用依赖后，相关内容才进入下次模型请求。
ResponseCommitted 是私有提交屏障，不是再创建一条重复 assistant UI 消息。
provider 被取消的 partial 输出可保留展示；不得通过 UI 已有部分文本推断可以继续完整 response。
public finalized 只关闭对应 item 的 stream，不关闭同 response 其他仍打开的 items。

## 4. 展示隐藏、撤回与文件恢复

HideItem 只属于客户端展示偏好，不改变服务器上下文或消息事实。
撤回是 WithdrawInput：用户明确选择一个已提交 user message_id，撤销该输入及依赖它的后续路径。
首期不支持对任意 thinking/text/tool_result 独立删除后保持原后续对话继续运行。
选中 assistant item 时 UI 提供“隐藏此项”或“回退到此响应前”，不能把两者都叫撤回。
编辑旧 user 输入使用同一回退事务，再提交具有新 message_id 的替代输入，记录 replaces_message_id。

WithdrawInput 必须有 command_id、target_message_id、expected_context_revision；
anchor 为目标输入被接受之前的完整 context checkpoint，保留该点有效的 system/injections/config 引用。
排队输入尚无后代时只移出队列并记录撤回；已有后代则移出该输入及所有因果后代，不能只删一个气泡。
新独立 queued inputs 不静默丢弃：冻结为 awaiting_rebase，用户确认后以新的 context_revision 接纳；
依赖被撤回 turn 的 steer/interject/子代理输入随其结算，不迁移到新的 turn。

状态路径：RewindRequested → 封闭旧路径新派发 → 取消/收割或对账 → ContextRewound / RewindFailed。
provider、tools、approval、PTY、feature jobs、subagents 都按确切 ID/generation 处理。
只清 pending maps 或关闭 SSE 不构成完成；远端设备无法证明停止时保持 blocked/indeterminate，
用户显式接受残留风险才可继续，命令不能先成功再让旧任务写新路径。
与目标无关的独立 background jobs 不被悄悄取消；其关联、资源冲突和继续策略在操作摘要中列明。
ContextRewound 原子增加 context_revision、更换 active membership、撤销旧 pending 授权并发布提交结果。
同一事务取消旧 live overlay；已排队/迟到帧、completion 不能进入新 context generation。
残留 outcome 只进入原 execution 的历史/对账，不能污染新 turn。

事实日志 append-only，撤回不会 truncate facts、回退 durable_seq 或改变 log_epoch。
已存在 items 保留稳定 ID，标为 inactive_on_current_path；当前 UI 可折叠被撤回后缀并查看历史。
不因撤回把下一条 msg_seq 改成刚删掉的数字。选择/覆盖快照由 ContextHead 定义，不由 seq 范围猜。
元数据或完整上下文无法解析时返回 CheckpointUnavailable，禁止用摘要/活跃数组猜较早边界。
RewindSession 接受 checkpoint_id/expected_context_revision，复用同一取消/提交事务；
可运行 rewind 只接受 runnable checkpoint，不能绕过 WithdrawInput 的依赖收割去裁剪半条 response。

对话撤回不自动执行文件/Git/远程业务 rollback。
workspace restore 是另一个明确 operation，使用实际 mutation journal/preconditions，返回 restored/conflicted/unsupported。
跨客户端、外部服务、脚本和手工修改不能凭工具消息假定可撤销。
可以在产品里组合“撤回并尝试恢复文件”，但两个结论分别显示；失败不能宣称环境回到过去。

## 5. 任意 item 定位与可运行边界

msg_seq/message_id 的细化解决定位、排序和引用，不保证所有 item 后面都是可运行 checkpoint。
Checkpoint 有 checkpoint_id、context_revision、materialization_digest、完整引用集、boundary_kind、runnable。
运行边界须验证完整 provider bundle、工具调用/结果闭合、私有状态完整以及 required feature 状态。
通常有效位置为用户输入前/后、完整工具批次结算后、完整响应/turn 结束后。
同批多个 call 只完成一个、tool_call 参数流到一半、签名 response 中间不能据界面前缀继续模型。
并行工具全部结果闭合才能达到该批次后边界，不把先完成的工具 ID 当所有工具已结束。
UI query 可提供 anchor 的候选 before/after checkpoints、阻塞原因和执行依赖摘要；不触发业务或创建 checkpoint。
每次请求前持久/引用完整 ContextSnapshot，因此“从某个 assistant item 重新生成”
精确使用该 response 的 pre-request snapshot，而不是截断该 response 的文本再续。

## 6. ForkSession

fork 创建新 session，不是原 session 的 rewind，也不继承执行任务。
fork 本身不取消或修改 source 的运行路径；child 执行与 source 独立，共享 workspace 时仍需资源冲突准入。
请求必须固定 source_session_id、source_context_revision、source_cursor（批次末水位 H）、
anchor_message_id、position=before/after、mode、command_id；不得用移动中的“最新消息”作基准。

| mode | 语义 |
|---|---|
| checkpoint | anchor 对应确切 runnable checkpoint；否则 NotRunnableBoundary，返回候选，不静默挪边界 |
| regenerate | anchor 所属 response 的 pre-request snapshot；新 session/turn/request/attempt 重新请求模型 |
| view_only | 按 anchor 保存已提交的展示前缀；不可 resume、执行工具或自动把 partial 转成完整上下文 |

source_cursor H 用于一致读取，不等于“包含 H 之前的所有 effect 都应再执行”。
fork manifest 固定所选 active path、checkpoint、完整私有 bundle、blobs、feature/config 引用和源身份。
source 中撤回而 inactive 的后代、未来 items、未提交 live 不加入 runnable fork。
view_only 继承的未完成 items 显示 frozen/incomplete，不连接原 live stream 或伪装成 child 正在运行。
新 session 的 log、cursor、context revision、权限评估和运行 IDs 独立。
继承 items 保留原 message_id/origin；新 session 的 msg_seq 按所选前缀顺序分配并记录，
前缀快照发布之后不重新编号；后续新 items 使用全新 message_id 和该 session 的 next_msg_seq。
引用和操作均携 session_id，不能因 item ID 相同跨 session 修改继承历史。
继承 tool call/result 只作为历史上下文；execution 关系保留 origin 标记，不进入 pending 队列或 effect scheduler。
新 call 即使相同工具/参数仍分配新 call/execution identity，不复用源 session 的执行授权。
审批、终端输入 writer、设备 connect ticket、运行子代理、后台作业与 token 不继承。
profile/capability 的确切版本可以引用，但新主体、宿主、权限和当前安全上限必须重新准入。
依赖不可用返回 NeedsConfiguration/UnsupportedFeature；不能静默换 provider 或绕过吊销。

catalog 持久 fork intent → 源 H 的一致物化/引用 pin → 初始化 child facts/blobs/manifest → catalog 激活。
child 仅在完整初始化后可见；命令重试返回同一 child，崩溃恢复对账，不产生两个可见 fork。
child 内容引用拥有独立 retention pin；删除父 session 不得使已创建 child 的 ContentRef 悬空。
fork 不复制真实 workspace：继续用原文件时明确是共享当前 workspace，不是过去文件快照。
隔离目录、Git worktree 或 journal restore 是明确 WorkspaceOperation，另报执行结论。

## 7. Compact、消费和模块归属

compact 是 ContextSnapshot 的派生摘要/选择，不删除原始 item 或覆盖其 ID。
summary 是新 injection item，携 covered_checkpoint/source refs；展示时间与模型放置位置分别定义。
回退到摘要覆盖区内时从原始 facts/blobs 构造确切 checkpoint，禁止带上包含未来信息的摘要。
必要原始内容被清理或 schema 不支持时，明确 CheckpointUnavailable，不伪造可运行 fork。
summary/旧 transcript/私有 provider blobs 的 retention 计入 fork pins 与导入/删除操作。

compact 请求固定 source context_revision；结果应用时以该 revision 为前提，不接受旧结果覆盖期间已变化的上下文。
保留区按实际 turn/checkpoint 计算，不能把注入消息或旧摘要的合成 user 槽位算成真实 turn。
无新可折叠内容是 no-op：不写新摘要、不消耗 message_id/msg_seq、不改变 context_revision。
真实折叠量来自 context 变换结果，不能以 turn 数差推断（折叠一个 turn 同时插入摘要时数量可不变）。
重复压缩避免把旧摘要在历史和 previous-summary 两处重复输入；空摘要明确失败，超大尾部保留合法上下文边界。
过程进度/最终状态见 protocol；这些产品性质保留，不要求继续使用 `[Compacted N turns]` 文本标签恢复。

旧 QAQH 存储切换与外部 transcript 导入是两个用例。外部源的原始 ID/字节/分支/未知块保留来源身份，
不能直接当本会话的执行授权或消息去重键；导入物化分配本会话顺序并保留 origin。
来源解析器提交可验证包，application/session-store 才创建正式会话和引用；默认只读，不因导入成功而提供 runnable fork。
上下文证据不足时只展示缺失/不支持，不继承源工具副作用、审批、PTY 或凭据；并行功能状态见 preservation。

public message.added 是一个 TranscriptItem，字段含 message_id/msg_seq/role/kind/turn_id?/response_id?/
call_id?/execution_id?/origin?/status/visibility/ordered parts；message.finalized 不创建新 item。
message.finalized 携最终 kind/parts 及 call 关联（如适用），允许补全而不替换原 part 身份；
tool.prepared 携 tool_call message_id 以连接完整调用与 execution，不能按名称/气泡位置猜关联。
context.changed 的 rewind 分支携 context_revision、checkpoint_id、active membership materialization ref、
closed_stream_ids、typed reason/target；客户端在完整 batch 原子更新视图与清除旧 live。
大型 membership 使用固定 H 分页物化，没取齐不能推进 applied，必要时按 Ringing reset/snapshot。
context.changed 的 compact 分支不把老 items 删除；fork child 用自己的完整 bootstrap snapshot。
无权查看 reasoning/private state 的主体仍正确应用 durable cursor，不靠占位气泡泄露正文。

| 模块 | 责任 |
|---|---|
| kernel/ids | message/response/checkpoint 身份；序号值 |
| provider-api/event + providers/stream | 稳定 part identity、原始位置、opened/delta/closed、私有 bundle |
| context/items + provider_projection + checkpoints | 纯 items/bundle 投影、依赖闭合、快照与恢复边界 |
| session-api/command + fact + projection | WithdrawInput/RewindSession/ForkSession、item/响应提交与 active path |
| session-store/commit + read + catalog + blob | 分配持久顺序、一致 H、唯一 writer、fork初始化/retention |
| agent/mailbox + reducer + supervisor | 接纳 opened、提交屏障、上下文generation、取消/收割 |
| application/history + forks | 操作用例和跨会话workflow；不读取UI数组推断边界 |
| protocol/messages + sdk/reducer | 独立 item DTO、稳定排序、原子membership变化、跨语言消费 |

不新增第二个 transcript writer，不给每个 delta 写一条永久消息，不把细粒度 UI DTO 直接喂 provider。

## 8. 真实接缝验收

part 3 慢于 part 4 finalize，展示仍按 opened 顺序；交错 delta 不增 ID；provider retries 不拼旧 attempt；
重复命令/receipt 不新增 item；正文 commit 失败不提前 finalize；恢复/compact 不改变原 message_id；
撤回后 seq 继续增加，原 cursor 可重放，旧 completion 不进入新 context；取消无法证明停止不能报成功；
撤回保持文件已产生副作用，恢复文件另报冲突；fork 不启动继承工具/子代理/PTY；
并行 call 部分完成不能 runnable fork，签名响应不允许任意裁剪；源 session 并发新增不越过 H；
parent 删除不使 child blob 悬空；摘要含目标之后信息时不用于早期 checkpoint；
不同 SDK 得到一致 item 顺序、active membership、applied cursor 与 typed boundary errors。
本轮只定义目标，未提供这些消息/撤回/fork 状态机的消费向量；结构 audit 不覆盖其行为。
