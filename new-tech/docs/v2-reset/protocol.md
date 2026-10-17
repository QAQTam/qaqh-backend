# Ringing：v2.1 → v3.0 目标前后端契约

状态：目标协议，尚未接入 daemon / SDK。产品版本不等于协议版本。
保留 Ringing 名称；新 wire major **3**，`schema = "qaqh.Ringing"`，路由 **`/api/v3`**。
它在独立验收后替换当前 Ringing v2，不用可选字段掩盖语义变化。上一轮 `/api/v2` / `qaqh.api/2` 提案由本文替代。
v2.1 发布不以 major 3 一次性落地为前提；内部切片可保持 v2，跨 major 过渡与旧路由退出遵循 [rollout](rollout.md)。
产品能力基线见 [preservation](preservation.md)，GenUI v0.2 与本 wire major 无版本联动。
只支持明确协商的 major；minor 可新增非关键可选字段。未知可靠 change 必须 UnsupportedSchema，不能跳过后推进 cursor。

## 1. 四条流

| 流 | 数据 | 恢复和背压 |
|---|---|---|
| session committed | 消息终稿、工具/交互/turn 状态、命令结论 | 持久，至少一次；从 applied cursor 重放 |
| session live | 模型文本/思考/工具参数增量、运行进度 | 易失，按 stream epoch/seq；缺块替换或重置 |
| diagnostics | 延迟/重试/健康、派生统计 | 独立流、保留窗口、明确 gap；不推进会话 cursor |
| terminal I/O | PTY 字节、resize、input ACK、输出 offset | 独立双向 WS、有界 buffer；输入不跨重连自动重放 |

committed/live 可共用一条 SSE；durable 优先，各队列有界。live 溢出只丢 live 并 reset。
诊断单独 SSE，PTY 单独 WS，不能阻塞审批结论和工具终态。
HTTP 管命令/查询/内容/票据，SSE 管单向订阅，WS 用于真正双向的终端。

host 扩展安装另有持久 operation feed，复用 committed envelope/快照/applied cursor 规则，
使用 `(operation_id, operation_log_epoch, operation_seq)` 独立域，不能推进 session durable_seq。
它归 extensions registry workflow，不新造空 session 或交给 agent writer；下载进度仍可丢。
详细命令/DTO/状态见 [生态契约](ecosystem.md) 与 `contracts/extensions/`。
ACP v1 是另外的边缘协议，不具备这里的 applied cursor 或 terminal input fence，见 [ACP](acp.md)。

没有全局 seq，每个序号有域、重置条件和持久承诺。timestamp 只展示，不排序。消息单元及撤回/fork 的具体语义见 [messages](messages.md)。

| 字段 | 序号域 |
|---|---|
| durable_seq | `(session_id, log_epoch)` 事实序号，重启不变，1 起步，0 是空日志 |
| from_seq/to_seq | 一次完整 committed batch 范围，不跨未提交边界 |
| projection_version | reducer 版本，不兼容变化必须 snapshot reset |
| live_seq | `(stream_id, live_epoch)`，1 起步；epoch 对应一次生成尝试 |
| observation_seq | `(diagnostics_scope, observation_epoch)`，保留窗口可缺失 |
| control_generation | terminal 独占输入 fence，接管/归还/断连/吊销均递增 |
| input_seq | 当前 terminal socket + generation，1 起步，重连不复用 |
| output_offset | `(terminal_id, io_epoch)` 的字节偏移，不是字符数 |

JSON 的 u64 seq/revision/offset 用规范十进制字符串：`0` 或 `[1-9][0-9]*`，上限 u64。
时长/尺寸为有界 number，timestamp 为 UTC RFC3339，JS 不用 Number 承载无限增长计数器。
log_epoch 只在日志身份更换/离线导入切换时改变；server_instance_id 变化不使 durable cursor 失效。

以下 committed changes 是首期公共注册表；所有 ID 为不透明非空字符串，revision 遵循上述十进制规则，
可选字段用缺省或 null 的具体 DTO 定义，不接受任意 string method/payload。持久内部 fact schema 与这里分开。

| change tag | 必需业务字段 |
|---|---|
| command.status | command_id、status；终态携 typed result/error |
| turn.started | turn_id、input_message_id、config_revision |
| turn.finished | turn_id、terminal、reason? |
| message.added | message_id、msg_seq、turn_id?、response_id?、role、kind、call_id?/execution_id?、origin?、visibility、ordered parts、status |
| message.finalized | message_id、kind、status、ordered parts/content_ref、call_id?、closed_stream_ids |
| tool.prepared | execution_id、call_id、message_id、turn_id、name、arguments_ref、display |
| tool.status | execution_id、status、interaction_id?、process_id? |
| tool.finished | execution_id、status、result_ref、display、metrics、closed_stream_ids |
| interaction.opened | interaction_id、revision、kind、target、typed form、deadline? |
| interaction.resolved | interaction_id、revision、outcome、resolving_command_id |
| terminal.control | terminal_id、execution_id、host_id、state、control_generation、controller_id? |
| usage.observed | request_id、purpose、usage、source、completeness |
| session.selection | selection_revision、profile_id、profile_revision、effective_from |
| context.changed | context_revision、checkpoint_id、typed compact/rewind reason、active membership ref、closed_stream_ids、summary_ref? |
| feature.changed | feature_id、feature_version、registered typed public state |

message_id 标识独立 TranscriptItem，msg_seq 为 session 接纳顺序（0 起步），不是 durable_seq。
thinking/text/工具调用/结果分单元；保留 response_id 关联供应商完整响应，消息排序不按 finalization 时间。
message.finalized 只更新原单元，原始 provider bundle/私有状态的完成由内部屏障验证。

message parts 的公共 union：text/ref、reasoning/ref（能力及权限允许时）、image/ref/media_type、
tool_call/call_id/execution_id、tool_result/execution_id/result_ref。provider 私有签名等保存在内部消息，
不将 secret 或供应商私有对象透传 UI。工具状态 union：prepared/awaiting_user/running/backgrounded/
succeeded/failed/partial/cancelled/denied/indeterminate，backgrounded 是运行态。
交互 kind 为 permission/ask/terminal_handoff；permission 答案有明确 allow_once/deny 等枚举，
ask 为 typed question_id/answer union，terminal_handoff 只能 claim/decline，不把密码当 ask 答案。

command 首期 union：SubmitInput(message parts)、CancelTurn(turn_id)、InvokeTool(typed call)、
AnswerInteraction(interaction_id/revision/typed answer)、UpdateSelection(selection revision/profile revision)、
ClaimTerminal(interaction_id/revision/terminal_id)、ReturnTerminal(terminal_id/control_generation/share selection)、
CancelExecution(execution_id)、ChangeFeature(typed feature delta)、
WithdrawInput(message_id/context revision)、RewindSession(checkpoint_id/context revision)、
ForkSession(source session/context revision/cursor/anchor/position/mode)。详细边界与操作状态见 messages.md。
不得用任意命令调用内部 IO。ForkSession 走 application/catalog 持久 workflow，原 session 仍处理并发 commands；
去重域使用 principal/source session/command_id，回执引用唯一 child session，不让源 actor 成为第二个 child writer。

## 2. 消费单位：完整提交批次

一个存储 batch 投影成一个 committed envelope，一个 fact 产生的多个 changes 全部包含。
客户端原子应用整批，取消 projection_index 逐条确认。正文转 ContentRef；单帧目标上限 256 KiB。
超限/投影不可用返回 ResetRequired，取分页 snapshot，禁止只确认一个分片。

```json
{
  "schema": "qaqh.Ringing", "major": 3, "kind": "committed",
  "session_id": "s1", "log_epoch": "l1", "projection_version": 1,
  "from_seq": "41", "to_seq": "44", "cursor": "opaque-cursor",
  "causation_id": "c1", "committed_at": "2026-10-10T08:00:00Z",
  "changes": [
    {"type": "tool.finished", "execution_id": "e1", "status": "succeeded", "result_ref": "sha256:..."},
    {"type": "message.finalized", "message_id": "m2", "kind": "text", "status": "completed", "content_ref": "sha256:...", "closed_stream_ids": ["stream-m2"]}
  ]
}
```

cursor 逻辑值 `(session_id, log_epoch, to_seq, projection_version)`，wire 为不透明 token，不含凭据。
server 采用持久材料编码/校验，重启不换材料；授权独立检查，cursor 不授予资源读取权限。
范围从 `applied_seq + 1` 接上；只支持 commit 边界续传，中部 cursor 为 InvalidCursor。
完整重复 batch 幂等忽略；范围重叠但非完整重复是协议错误，必须 reset。
空 changes 也发送并推进水位，不以频道过滤制造假 gap。因权限隐藏的事实可无 change；权限视图变化 reset。

SDK 分开 received_cursor / applied_cursor，只有 reducer 提交成功才推进 applied。
UI 使用 SDK 已提交状态；自管 reducer 时显式 ack_applied，不在接到帧时抢先确认。
本地持久状态与 cursor 原子提交；只存 cursor、丢状态时必须 snapshot。
延迟加载正文不阻塞应用 ContentRef，失败须展示可重试错误；不得虚构内容。
网络不保证 exactly-once，消费以 batch 水位幂等。

## 3. snapshot、follow 和 live

POST /sessions/{id}/subscriptions 返回订阅 id、身份/资源绑定的短期单次 connect ticket、snapshot manifest 和 cursor。
分页 snapshot 固定同一 revision；缺页不能推进 applied，pin 超时为 SnapshotExpired。
建立订阅时先注册 follow reader，捕获持久水位 H，读 H 的 snapshot，再补 H 之后 batch，最后转 live。
H 到 follow 之间新增的提交从日志补读，不依赖广播队列始终保留。
断连不改变身份；resume 带 applied cursor 建新订阅。

GET /subscriptions/{id}/events 建 SSE。浏览器用 ticket 或 SDK fetch SSE，不把持久 Bearer 放 URL。
ticket 只限制建流，过期不清会话、不取消 turn、不配对；重放已使用 ticket 失败。
access 吊销主动断流，空闲时也生效；发送前核对当前 grant generation。
仅 committed SSE 帧设置 id=cursor，live/control 无 id。显式 resume cursor 与 Last-Event-ID 冲突则拒绝。

ResetRequired：log_replaced/projection_changed/cursor_invalid/replay_unavailable/consumer_overflow/access_view_changed。
reset 不是 snapshot：清 speculative live，重新取完整 snapshot，提交成功才替换 applied。
不得把 reset 提示水位直接当 applied。daemon 重启但日志未变，正常 replay。

live 必填 session_id/turn_id/request_id/stream_id/live_epoch/live_seq/after_durable_seq/mode/payload。
mode=append 需要连续 seq，replace 是该 stream 完整当前值。
先应用 after_durable_seq 再显示依赖 live；缓冲有界，等不到锚点就丢弃。
gap 后停止追加，GET /sessions/{id}/live/{stream_id} 取易失 replace snapshot，Gone 则清 speculative。
live snapshot 来自 subscriptions 有界缓存，携同 epoch/current seq，不触发 provider 或补事实。
message.finalized/工具终态覆盖并关闭对应 live；晚到旧帧不得复活终稿。
模型重试使用新 request_id/live_epoch，禁止拼接不同尝试的增量。

## 4. 命令与幂等

POST /sessions/{id}/commands 为 typed union，公共字段 command_id/type/target/expected_revision?/payload。
target 精确到 turn/interaction/execution，不用“当前 turn”。无全会话 driver seat。
普通提交按邮箱排序；满队列/忙状态明确返回 QueueFull/TurnBusy 或受限排队。
配置按对应资源 revision CAS，交互按 interaction revision CAS，取消绑定 turn_id。

状态 accepted → running → succeeded/failed/cancelled/indeterminate，终态不可变。
CommandAccepted fact 持久化后才 ACK accepted；完成来自 terminal fact。
同步拒绝给具名 error，不产生 accepted；接受之后失败必须有终态。
去重域 `(principal_id, session_id, command_id)` 并核对 body digest；同 id 异体为 IdempotencyConflict。
已存在返回相同 receipt/status，不触发第二次执行。receipt 含 command_id/accepted cursor/status/typed result 或 error。
幂等记录与会话 facts 同寿命；删会话保留最小 tombstone，拒绝重放，不复用 session_id。
OS 外部副作用按恢复类别对账，command 去重不等于执行 exactly-once。

## 5. 工具到前端

1. provider 正文/参数增量走 live，解析完成才提交 ToolIntent，引用持久参数 blob。
2. 提交后 executor 准入，复核 host 安全上限和授权。
3. NeedInteraction effect 让 actor 提交 InteractionOpened，UI 显示 pending。
4. 首个合格答案 CAS 提交 InteractionResolved；executor 只消费已持久答案，其余端 AlreadyResolved。
5. 进度走 live，ProcessStarted/后台关联/终端交接/退出和工具终态走 facts。
6. executor 返回 ToolOutcome，actor 原子提交 ToolFinished + 模型可见 tool result message + 命令/交互结论。
7. committed 更新工具卡片和消息；模型从 committed context 获取结果，不读 UI 卡片。

工具展示用公共 typed DTO，不暴露内部 trait 或 argv 密钥。后台启动确认不等于完成，
execution_id 关联后续 exit；前端不能以“没有进度”猜成功/失败。

## 6. 终端接管

首期 terminal 域是 daemon 主机上的 PTY/ConPTY。客户端普通工具由 [client-tools](client-tools.md) 的独立通道接入，
不能用该协议隐式启动客户端 shell/sudo；client-owned PTY 需另行完成宿主、输入 fence 和恢复验收。
绑定 terminal_id/execution_id/owner/host_id/io_epoch；owner 为闭合 union：
`session {session_id,turn_id}` 或 `extension_operation {operation_id}`。
会话执行使用前者；安装 recipe 使用后者，不能伪造 session。UI 显示主机、命令、用途、权限范围。
接管交互/控制结论由对应 actor/workflow 唯一提交；ticket 与 scope 鉴权按 owner 检查。
两种 owner 共用 generation、独占 writer、断连 fence、输入不重发及敏感 I/O 隔离规则。
process-tools 声明 interactive=never/allowed/required；PTY、sandbox、elevation 为独立能力。
可能交互的调用从一开始分配 PTY；不承诺把已启动 pipe 进程无损转换。

sudo 正常通过终端读密码，非交互时可能直接失败，不能匹配 password 文本自动重跑。
参见 [sudo 官方排障](https://github.com/sudo-project/sudo/blob/main/docs/TROUBLESHOOTING.md)。
提权由 typed 执行意图/明确等待信号提出；泛化提示检测只能建议接管。
不可重放的失败要新 execution、重新审批，禁止自动再执行命令。
no-new-privileges/AppContainer 等可能不允许提权，应返回 ElevationUnsupported；
离开 sandbox 需要独立授权和新 intent，不能输入密码就静默突破执行边界。

### 6.1 控制状态与 fence

agent_controlled → awaiting_user → user_controlled → returning → agent_controlled/exited/cancelled/lost。
InteractionOpened(kind=terminal_handoff) 引用 terminal/execution，不含密码字段。
request 含 reason/命令摘要/host/能力/显式等待超时策略。
ClaimTerminal 经交互 CAS，停止 agent stdin/resize，递增 control_generation；
TerminalControlChanged 持久后才开用户输入。claim 期间进程退出就关闭交互，不建 writer。
terminal 是唯一输入执行者，在同一串行队列逐帧检查 fence 和 access。
独占的是 PTY stdin/resize，不是给全会话恢复 30 秒在线 lease。

ReturnTerminal 先关闭用户输入队列，完成或明确中止已接受写入，递增 fence，持久归还结论。
屏障完成后 agent 才写/继续；归还不等于成功，exit 决定 ToolOutcome。
Ctrl-C 是 PTY 字节，CancelExecution 是 kill+wait 命令，语义不同；一次 execution 仅一个 ToolFinished。

actor 持交互结论，terminal 持 OS/IO fence，不设第二份业务 pending。
交接用 operation_id/staged fence：quiesce 旧 writer → actor commit → enable 新 writer。
commit 失败禁写；actor 崩溃禁写等待恢复，恢复对账前不启用。
daemon 死亡通常丢 PTY，记 Lost/Indeterminate，不自动重跑 sudo；detached supervisor 需另有持久对账设计。

### 6.2 WS 数据面

POST /terminals/{id}/connections 给单次 ticket，绑定 principal/execution/generation/read-write/io_epoch。
GET /terminals/{id}/io Upgrade，子协议 qaqh.terminal.1；验证 Origin，远端 TLS/WSS。
ticket 过期只限制建连；吊销实时关 socket/禁写，持久 token 不进 URL/日志/模型。
WS 支持二进制消息，参见 [RFC 6455](https://www.rfc-editor.org/rfc/rfc6455.html)。

text 控制：ready/resize/input_ack/output_ack/gap/exit/revoked，不含 raw 输入。
binary 每 message 为 32-byte network-order header + payload：
version:u8, kind:u8, flags:u16, generation:u64, io_seq:u64, byte_offset:u64, payload_len:u32。
version=1，flags=0，kind=1 output/2 input，payload_len 等于实际长度且不超过 64 KiB。
ready 给 terminal_id/io_epoch/generation；input offset=0，io_seq=input_seq。
output io_seq 是连接帧序号，offset 是 PTY 字节起点。UTF-8/ANSI 可跨消息，由 streaming decoder/emulator 保存解析状态。

input ACK 含 input_seq/bytes_written/outcome=written|partial|rejected|ambiguous。
ACK 只证明交给 PTY 的字节数，不证明执行；部分写不谎报全成，ambiguous 禁写并提示核对。
同连接重复 seq 只返回缓存 ACK，不再 write；跳号拒绝。
控制连接断开立即 fence；重 claim 新 generation，不自动补发未 ACK 按键。
断连后 actor 为仍存活 execution 提交新的 terminal_handoff interaction；重 claim 针对新 revision。
等待策略是明确的执行 timeout/cancel，不能用在线租约把会话授权一起删除。
human 控制期默认仅 controller 可读终端输出，其他观察权限须明确授予。

output 使用有界 ring buffer/byte ACK window；请求起点低于 retained_start 则 gap。
不承诺任意历史 replay；gap 明确标记丢失并清屏/重建视图，不把 ANSI 尾片当完整 screen snapshot。
慢端断开后 PTY drain 继续，有界丢弃与统计。业务结果采集独立限额/截断标记；
重要证据持久失败必须报错误，不能借 live 可丢而静默丢工具结果。

### 6.3 人类输入与模型上下文

raw 用户输入只写目标 PTY，不写 facts/blob/诊断/请求日志/模型，不能靠正则判断密码。
整个 human 区间 raw 输出默认也不进模型、不持久 replay、不广播；不靠 ECHO 状态证明安全。
启用用户 writer 前先完成输出 tee 停止屏障，关闭模型/日志/诊断订阅该 raw 区间的通道；
归还时 drain 并清理 human 区间缓冲后才恢复 agent tee，不能由异步晚到输出穿过边界。
恢复只给结构化退出状态和明确可分享结果；用户选择分享正文才 put_blob + fact 引用。
不自动把 human transcript 拼成 tool result。应用不保证目标程序/OS 不自行记录，宿主策略须单独约束。
命令仍运行时使用同 execution；参数/权限变化则新 execution。
手机可只观察/批准，缺 terminal_write 时由支持终端的客户端接管。
无 controller 时 AwaitingUser 或明确超时/取消，不能让模型替用户输入密码。

## 7. 诊断

DTO 在 protocol，collector/index 在 diagnostics。scope=host（admin）或 session（资源 grant）。
帧含 scope/observation_epoch/observation_seq/observed_at/type/correlation/payload。
correlation 可带 session/turn/request/execution/span，不携正文/凭据/raw PTY。
派生查询含 source_cursor/window/sample_count/coverage/source/as_of，未知 rate=null。
usage live 按 request_id 替换，最终业务 usage 仅提交一次；重连不累加。
logical call/execution attempt/denied/cancelled/indeterminate 分开，后台启动不算成功。

off/basic/trace 只控制附加采集，off 不关闭 ToolFinished/UsageObserved/安全审计。
trace 默认仍只元数据；敏感请求正文导出为独立受控操作。队列满/采集失败不阻塞 loop。
丢弃发送带范围 gap 或 DiagnosticsResetRequired；epoch 换了不能静默从 1 续接。
查询明确 UTC 时间窗/时区，返回 disabled/retention/dropped/健康，不用全零冒充未采集。
memwatch/LSP/provider/executor 通过显式 sink，诊断不能触发业务状态。
观测端口由生产者契约层定义：agent-api 的 ObservationSink、provider-api 的 ProviderObserver、
tool-api 的 ExecutionObserver；diagnostics 提供这些接口的适配实现，daemon 注入。
provider-api/tool-api 不依赖 agent-api 或 diagnostics，避免为共享一个 sink 形成循环依赖。

## 8. 路由与错误

| 路由（/api/v3） | 契约 |
|---|---|
| GET /capabilities | wire/projection 版本、限额、每 host PTY/elevation/sandbox |
| POST /devices/pair；DELETE /devices/{id} | 稳定身份/吊销，无 lease open |
| GET/POST /sessions；GET /sessions/{id} | catalog 查询/创建，创建 request 去重 |
| POST /sessions/{id}/commands；GET /sessions/{id}/commands/{command_id} | typed 命令/持久 receipt |
| POST /sessions/{id}/subscriptions；GET /subscriptions/{id}/events | snapshot + committed/live SSE |
| GET /snapshots/{id}/pages/{page}；GET /sessions/{id}/live/{stream_id} | pinned snapshot/易失 replace |
| GET /sessions/{id}/content/{content_id} | 会话授权、range、digest；禁止按 hash 跨会话越权 |
| GET/PUT /host-config；GET/POST /profiles | 配置资源 CAS，UI 偏好无路由 |
| /provider-accounts 下的 typed 登录/状态/取消/校验/退出用例；模型目录查询 | 目标资源族；精确 DTO/路由在该切片锁定，不能遗漏现行 siwc/dsaccount 能力，见 providers.md |
| POST /terminals/{id}/connections；GET /terminals/{id}/io | ticket/受控 WS |
| GET /diagnostics/status；GET /diagnostics/query；POST /diagnostics/subscriptions | 范围、窗口、SSE ticket |
| GET /diagnostics/subscriptions/{id}/events | 诊断帧/gap/reset/health |

terminal claim/return 和 interaction answer 都走 typed 命令，不设 string service RPC。
error envelope：code/message/request_id/retryable/details，details 按 code 区分 DTO。
401 身份、403 越权、409 CAS/幂等/fence、410 资源/窗口、413 超限、429 容量、503 存储/不可用。
message 不含密码/token；SDK 保留 code，UI 不匹配错误字符串。

## 9. 客户端模块与当前消费差距

SDK 产品族与 ClientToolHost port 的边界见 [client-tools](client-tools.md)；Rust qaqh-client 目标改名 qaqh-sdk。
新 SDK SessionReplica 拥有 committed reducer/applied cursor；LiveOverlay 独立存 speculative；
DiagnosticsClient 维护观测窗口；TerminalConnection 只管理 WS/字节/控制 fence，不写 SessionReplica。
桌面/TUI/mobile 渲染同一 DTO，但 capabilities 决定是否支持终端输入，UI tab 激活不授予执行授权。
Tauri host 转发 replica 已提交状态或整批变更，保留提交边界；renderer 若异步应用则由它 ack，不能抢先推进宿主 cursor。

2026-10-10 通过 CodeGraph 检查同级桌面源码发现：SessionStore.activate 仍 transport.attach；
connectHost 同时接 timeline 和 projection，applyProjection 只读 stream_key/payload；
reducer.applyEntry 用 timeline_seq:number，text_delta/tool_progress 做字符串追加；types.Seq=number。
这些消费路径须一起切换，单改服务器 envelope 会保留多套水位和文本追加规则。
旧宿主 timeline/projection 双订阅、activeSeed、RPC catch-all 和按页面激活 attach 均从新 SDK 路径删除。
目前没有移动端源码验证，不声称移动端已适配。桌面仓未在本轮改动。

以上桌面差距是 2026-10-10 的取样，不是当前前端完整清单；2026-10-15 的账号、模型、消息渲染与 GenUI 增量见保留矩阵。

### 9.1 GenUI 与 renderer 的保留契约

保留 `qaqh-ui` v0.1 与 v0.2 的版本分发（v0.2 默认）和受限 TSX/JSONL 语义；v0.2 的 A2UI 对齐是渲染实现，不等于执行任意 JS。
保持流式部分编译/终稿 seal、children 前向引用、具名错误回退和纯文本回退，不以旧的最终 Markdown 渲染替代流式能力。
surface state 属于客户端局部状态；正文/历史可重建展示，不在 daemon 建第二份 UI 权威，不把页面状态当 session facts。
followUp 只通过现有用户输入与授权链提交选定字段；copy/openLink 受白名单限制；渲染/replay 不自动触发动作。
模型输出不可信：保持组件/表达式/动作目录、无任意 JS/导入/网络权限、链接安全校验，不能借新 DTO 绕过现行安全边界。
完整传输正文以支持刷新和历史重放；live 显示不得推进 durable cursor，final 后迟到 patch 不得复活旧 surface。

## 10. 验收

真实接缝：snapshot/follow 并发提交、重复幂等、gap 不推进、重启 cursor 稳定、projection reset、final 后 live 不复活；
诊断 gap 不影响 committed、双端 claim 单 writer、旧 fence input 不写、ACK 丢失重连不补键；
human transcript 不进上下文/日志、Return/exit 并发单终态、cancel kill+wait、不支持提权明确拒绝。
离线契约检查只证明 trace 消费规则，不代替真实 SSE/WS/PTY/平台/三端验收。
配套离线消费模型与场景在 tools/v2-architecture/protocol_check.py / protocol-vectors.json，
用于提前检查序号、应用屏障和终端重连语义，落地后应让真实 SDK 跑同一组场景。
