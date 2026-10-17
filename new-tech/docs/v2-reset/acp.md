# ACP：QAQH 被调用与调用其他 agent

状态：目标设计。当前 workspace 未有 ACP 实现；不以模型 provider 接入替代 agent 协议。
本能力按 [rollout](rollout.md) 独立验收；保留当前账号型 provider 不代表 ACP 已实现，也不要求先完成 ACP 才发布 v2.1 功能切片。
首发采用 Agent Client Protocol **v1** 的 JSON-RPC/stdio，具体 SDK/schema 在实现时锁定。
官方 v2 RFD 另列未来能力，不据此声称 v1 有可靠游标/PTY writer。

## 1. 两个方向和执行主机

| 方向 | 角色 | 执行与数据所有者 |
|---|---|---|
| 编辑器 → QAQH | acp-server，QAQH 是 agent | 当前 daemon workspace/executor/actor/store |
| QAQH → 外部 agent | acp-client，QAQH 是 client | 默认 daemon 启动外部 agent；本机回调由受限宿主处理 |

首发不把手机设备变成远端 shell，不透过 ACP 假装任意 cwd 在服务器上存在。
本地 `qaqh-daemon acp` stdio 前门由 daemon composition root 装配；可连接现有本地主机服务，
但 session/授权/工具执行仍由同一 application 端口处理，不能建第二套 loop 或消息 writer。
stdio stdout 只有协议，日志走 stderr/diagnostics；包不明指令不能污染 transport。
每连接限定 principal、host、workspace grant；sessionId 不构成跨用户访问权。
路径为协议要求的绝对路径，先验证 workspace/access，再映射内部 workspace ID。

## 2. acp-server 边缘映射

| ACP v1 方法/更新 | 内部用例 |
|---|---|
| initialize | 精确协商协议与已实现 capability，记录 clientInfo |
| authenticate | access/credential flow；不把 provider key 返回 editor |
| session/new | 授权 cwd → application CreateSession + 固定配置/能力集 |
| session/load（声明支持后） | 检查权限 → 读 committed 记录 → 按 ACP 重放展示 |
| session/prompt | typed SubmitInput；ContentBlock 转 provider-neutral parts |
| session/cancel | 取消当前 ACP prompt 对应 turn；停 effect/收割后结果 |
| session/set_mode | 明确 profile/mode mapping + CAS + 下一 turn 生效 |
| session/request_permission | 持久 InteractionOpened → callback → CAS resolve |
| session/update | committed 投影 + 明确临时模型增量 + tool/plan 更新 |

methods 名称以锁定 v1 schema 为准，不向协议塞 catch-all method。
Ringing 外部 command_id 与 ACP JSON-RPC id 不相同；重复 RPC id 不能当稳定幂等业务键。
gateway 在活连接内创建稳定 operation mapping；跨连接重发 prompt 可能是新输入，
除非双方显式协商 QAQH 扩展 command identity，不能承诺 ACP 自带 exactly-once。

ACP prompt response 的 end_turn/cancelled 等 stopReason 从已提交 turn 结论产生，
max token/拒绝/故障按协议与 typed error 分类，不能一律 end_turn。
同时来的第二 prompt 按服务端明确 Busy/排队政策处理，取消不被长 await 阻塞。
ACP toolCallId 来自本 execution 的稳定映射；不能把所有 nested tool 都混成一个 progress 字符串。
pending/in_progress/completed/failed 仅为显示；QAQH denied/cancelled/indeterminate 在内容/错误说明中保留。
背景工具不因 prompt 已结束就显示 completed。

ACP v1 update 没有 QAQH applied cursor 的持久保证。重连用 load/replay；
重放终稿/既成 tool 状态，不续拼旧 live delta、不重新执行工具。
不能把 editor 的重放 UI 或 connection keepalive 当作 writer/driver 权限。
Ringing sessionReplica 不直接消费 ACP 更新；两种适配器共用 committed 读视图，协议映射分别拥有。

capabilities：仅声称实际支持的 image/audio/embeddedContext/loadSession/mcp 等。
传入 mcpServers 是临时 IntegrationSpec 请求，不直接启动 editor 给的 argv/读取 token。
同 extensions/admission 生成 scope binding，尚未授权返回交互/错误；不得全局合并 config。
未保存 buffer 必须选择 editor FS capability 模式并建立 DocumentSnapshotPort 才可使用；
首发 daemon-owned 文件模式不主动调用 editor 写文件，清楚标示分析已保存文件。
不得读 editor buffer、写 daemon disk 却谎称是在同一版本上操作。

## 3. 权限回答不是协议层放行

内部 InteractionHandle 绑定 session、turn、execution、capability、principal、revision。
ACP optionId 映射该 interaction 的具体 policy action；答复先做 CAS commit，executor 再获得批准。
allow_once 只对本意图有效；allow_always 若可表达，则限显式 scope/能力/路径条件，
不能泛化成全局 shell 或所有未来 MCP；无法表达时不提供该 option。
unknown option、旧 revision、断线、cancelled 都不能隐式 allow。
cancel 与回答竞态只有一个已提交结论；到期/吊销也更新 revision 并完成等候者。
密码、OAuth token 不放在 permission reason 或普通表单 answer；登录另走凭据 flow。

## 4. acp-client：外部 agent 是受监督的执行者

agent package/preset 在 extension plan 中声明协议、入口、版本、平台、auth、能力请求。
daemon 注入 DelegationRunner/ToolRunner；agent-features/subagent 只生成明确 DelegateAgent effect，
内部 agent 不依赖 ACP crate。不能把外部 agent 嵌进 provider streaming parser。

启动链路：DelegationIntent commit → grant/sandbox/recipe 准入 → spawn → initialize/authenticate
→ session/new/load（对方能力允许）→ prompt → updates/permissions → 最终 response
→ 保存必要 transcript/result blobs → actor commit DelegationFinished。
connection/remoteSessionId 与 execution/turn generation 一起固定；迟到响应不能污染新 turn。
独立 request/预算/并发/嵌套深度限制；delegation graph 防止无界互相启动。

外部 agent 的自有历史/内部 tool 成功不是 QAQH 本地工具完成证据。
保留 source agent、reported status、remote toolCallId、host；只对本宿主实际执行的 callback 承诺结果。
prompt 结束但外部后台任务未收割时，用 final output 与 background execution 分开表示。
unsupported 方法、schema/版本无交集、auth 不可完成是 typed outcome，不回落为 exec 文本猜解析。

### 4.1 filesystem/terminal 回调

只宣告实现完整的 callback capability：

- fs/read_text_file：workspace grant + content limit + 明确编码/行边界，返回一致文档 snapshot。
- fs/write_text_file：executor 准入 + journal + 文件版本冲突检查；对方协议未给 CAS 时，
  必须限定 workspace 独占编辑或显式捕获当前 hash；不能宣称保护了对方没传的预期版本。
- terminal/create/output/wait_for_exit/kill/release：terminal supervisor 的实例句柄，
  保留 execution identity、输出截断、exit 和 kill+wait；release 不能留下无人收割进程。
- session/request_permission：转内部持久交互；不是回一个 true 就永久开放 host。

有副作用 callback 先经 actor 提交子执行 intent，再执行，完成 callback response 不能替代终态提交。
terminal/output 和 ACP tool updates 只能读取 agent 可见的结果视图；用户接管阶段的原始输入/输出
不在这些视图中，不能因标准方法叫 output 就绕过 Ringing 的密码/人工操作隔离。

不支持一组方法中的必要方法时不声称 terminal=true；不支持回调时可以仍调用完全自持的 agent，
但它的直接 OS 操作仅由进程沙箱/宿主授权约束，ACP callback 权限无法拦截它未发送的调用。
UI 明示执行边界，首发不默认把全部本机 filesystem、环境变量、provider key 交给外部 agent。
按主体绑定它自己的 credential；同项目多个 agent 不共享远端凭据，未提供 login 支持准确报 needs_auth。

## 5. ACP terminal 与 Ringing handoff 的关系

ACP v1 标准 terminal 方法提供 create/output/wait/kill/release，
没有 QAQH 的 byte input、resize、独占控制 generation/ACK 或 sudo 用户接管契约。
editor 宣告 terminal=true 只能说明这些标准方法存在；auth.terminal 用于认证启动，不能当任意 sudo 能力。

因此首发 daemon-owned 执行与 Ringing handoff 保持同一 terminal owner：

1. 普通 ACP 工具终端按标准方法可展示输出。
2. sudo/交互运行需 QAQH handoff 时仍由 actor commit interaction + terminal fence。
3. 若调用端也协商 `_meta` 中 QAQH terminal 扩展，可给短期 resource-bound ticket，
   去原生终端 WS；认证 token 不出现在普通 tool transcript，generation/revoke 语义不降级。
4. 普通 ACP editor 未实现该扩展时，显示 RequiresInteractiveTerminal，
   可让用户打开授权的 QAQH 客户端接管；不能自动往终端塞密码或重跑提升权限命令。
5. [ClientToolHost](client-tools.md) 可提供客户端普通工具；client-owned terminal 模式首发仍不开启。
   未来启用要明确不同 execution host，
   supervisor、回调结果和 workspace buffer 语义一起设计，不能沿用 daemon-owned 保证。

Ringing 扩展只是已协商附加能力，不把标准 ACP terminal 改造成必须懂 QAQH 的私有协议。
ACP update 中的 terminalId 保持标准角色约定；QAQH terminal_id 属自己的 ticket/扩展域，不冒充 client-owned handle。

## 6. 故障、取消与恢复

请求/响应关联表有界；stdout frame/内容/queue 限额，stdio malformed/断流终止连接。
取消发 session/cancel；宽限后强制结束自有 child/process tree 并 wait，再提交 cancelled/failed。
对非自有远端进程只知连接断开时，无法证明已停止，报告 Indeterminate；不伪造 Cancelled。
程序崩溃或断连不自动重发 prompt、tools 或未 ACK 输入；恢复根据 remote load 能力与已提交 evidence。
load 只提供 transcript 也不能证明外部 agent 没做业务副作用；无法对账为人工恢复/新任务。
subprocess 启动后 OS 效果仍可能与会话终态提交失败错开，使用稳定 intent/recovery 分类。

## 7. 验收边界

acp-server 与官方 Test Compatibility Kit/一个真实 editor 对接：
initialize/version、new、prompt 增量到终稿、取消、权限 CAS、load 展示、Busy、连接中断。
acp-client 与一个固定版本开源 ACP agent + 故障 fake peer 对接：
回调读写/终端、权限拒绝/取消、拒绝旧 generation、auth、退出/kill+wait、嵌套限额、断线不重发。
包含未知 capability 不宣告、未保存 buffer 明示、错 cwd 拒绝、无 terminal handoff 扩展的 sudo 阻塞。
stdio 不泄 token/stdout 日志；配合 Ringing 客户端验证接管授权，不用 ACP 假 peer 代替真实 PTY 验收。

资料：[v1 overview](https://agentclientprotocol.com/protocol/v1/overview)、
[initialize](https://agentclientprotocol.com/protocol/v1/initialization)、
[terminals](https://agentclientprotocol.com/protocol/v1/terminals)、
[session setup](https://agentclientprotocol.com/protocol/v1/session-setup)。
