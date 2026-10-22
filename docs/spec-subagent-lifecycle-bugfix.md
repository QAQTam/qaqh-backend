# SA-FIX-01：子代理生命周期与工具契约闭环修复

- 日期：2026-10-22
- 类型：一次性交付的 bugfix spec，不是架构重写路线图。
- 工作根：`qaqh-backend/`。
- 执行入口：把同目录 `prompt-subagent-lifecycle-bugfix.md` 全文交给实现模型，并使其可访问本文件。
- 审阅轮次上限：3；第 1 轮做完本文全部 MUST，第 2 轮修审阅缺陷，第 3 轮只作最终收口。不是把功能拆成三轮实现。
- 任务 ID：`SA-FIX-01`，一个聚焦 PR；内部工作项 A～H 不是另开 PR 的前置任务。
- 本文是修复目标，不代表以下缺陷已通过端到端复现或已修复。

## 0. 交付原则与范围

### 0.1 必须交付

恢复以下闭环：

`spawn → canonical spawn edge → task accepted → turn started → answer blob + answer fact → terminal → parent result → unload → followup/resume → wait/list → restart recovery`。

同时修复 queue/steer/interject 的安全点消费、失败回滚、订阅清理和 schema 与实际行为的不一致。18 个相关工具都要保留，不能只让 spawn 返回成功。

不得使用子代理开展本任务；只允许执行隔离测试中必要的确定性 actor fixture，不得向真实模型服务委派任务。不得读取真实用户凭据、历史或在真实会话数据上测试。

### 0.2 不在本次扩展的范围

- 不重写整个 daemon、权限系统、取消系统、所有工具 SDK 或前端。
- 不引入新服务、数据库、任务框架、第三方依赖或新的持久化旁路日志。
- 不调整任务板/消息板业务状态机，不将 task board 改成调度器。
- 不承诺线程可被强杀，不将 timeout 描述为进程隔离或硬杀保证。
- 不更改公开工具参数结构，不删除工具、不移除 agent id/相对路径支持，不以改描述掩盖失效功能。
- 不清理整个仓库的历史 warning；与本次无关的失败独立列出。

### 0.3 仓库约束

先读 `AGENTS-x.md`。其引用的 `docs/ARCHITECTURE.md`、`docs/spec-architecture-convergence.md` 在本次基线中不存在；这是已知事实，不要求实现模型反复寻找或因此停止。若后续工作区已有这些文件，先读并遵守，禁止覆盖。

本次必须遵守：canonical log + per-session blob 为唯一权威；blob 先持久化再追加引用 fact；日志只追加；会话状态显式传参；不新增 session 级 static/TLS/取消旗标；不持 registry 锁做 IO/阻塞发送/join/组件回调；新增公开跨 crate API 使用具名错误；新增参数组用结构体。超过 1500 行的现有文件不得继续变长，需要把本次职责抽到小模块，新文件不超过 1000 行。

Ringing v2 路由、现有字段和语义保持不变。本次允许的兼容方式是添加可缺省字段并保留旧记录读取；不得新增对外事件种类或必填字段。若确需突破冻结边界，报告具体阻塞，不得自行发明不兼容协议。

## 1. 已知基线与定位索引

行号只辅助定位，以符号为准。实现前用红测试核实，不要把源码字符串断言当功能复现。

| 问题 | 入口与根因线索 |
| --- | --- |
| A 答案正文未接通 | `agent/turn_lap/parse.rs::parse_and_ingest` 写 MessageStore；`turn_lap/gate.rs::seal_active_stream_block` 只发旧 timeline；`host_impl.rs::collector_event_from_projection` 只接 canonical AssistantBlockSealed |
| A 引用存储不一致 | `host_impl.rs::download_content` 读 Ringing cache；`qaqh-session/src/canonical/blobs.rs::SessionBlobStore` 才是 canonical 引用的存储 |
| B 安全点漏掉通信 | `host_impl.rs::send_agent_message` 发 as_system=false；`loop_injection.rs::is_injection_command/drain_pending_injections` 只认 as_system=true |
| C 等待目标解析漂移 | `registry.rs::resolve_agent_for_caller` 只查 path；`qaqh-subagent/src/lib.rs::wait_targets_from_listing` 仅字符串匹配；host wait 与快照使用不同解析规则 |
| D 多次执行混用状态 | `registry.rs::subagent_outcomes/armed_collectors/prepare_collector_arm`；collector 完成 message id 仅含 session id；ToolLedger subagent_finished 仅按 child id |
| E 重载丢上下文/配置 | `agent/spawn.rs::spawn_agent` 的 Subagent 分支；`registry.rs::reload_subagent_internal`；`state/lifecycle.rs::create_session_with_session` 初始化空 store |
| F 失败与资源收尾 | `tool_runtime.rs::apply_subagent_spawn_effects` 的早退；`host_impl.rs::subscribe` 轮询线程；`registry.rs::close/finish_for_unload/respawn_dead_agents` |
| G 重启恢复未接线 | `registry.rs::new/ensure_root_metadata`；`agent_graph.rs::load_agent_graph`；`subagent_recovery.rs::scan` |
| H schema/权限可达性 | `qaqh-subagent/src/{lib,task_tools,board_tools}.rs`；`agent/spawn.rs` allowlist；`qaqh-workspace/src/authorization.rs` |

上次可运行验证：`qaqh-subagent --lib` 14/14，`qaqh-tool-core --lib` 71/71，`qaqh-runtime --lib` cargo check 通过。runtime 集成测试缺 `ascii v1.1.0`，session 测试缺 `tempfile v3.27.0`，在线取依赖遇 crates.io DNS 失败。这些是环境限制，不是修复后的验收结果。

## 2. 本轮设计裁决：不要重新讨论这些问题

### 2.1 身份与执行必须分离

- `agent_id/child_session_id` 标识长期存在的 agent；每次初始任务或 followup 都是一次独立 execution。
- **execution_id 使用该次 Trigger 的唯一 message_id**；不得另造第二套随机 run 标识。重复投递同一个 Trigger 只能产生一次执行。
- 执行关联必须能由 child 的 committed `InputAccepted → TurnStarted(input_id) → AssistantBlockSealed(turn_id) → TurnFinished(turn_id)` 重建；不要仅依赖内存 generation。
- input_id 沿用现有 `input_ + ulid_from_text(message_id)` 映射，不另设不一致转换。
- collector 必须按预期 input_id/turn_id 过滤；不得消费同 session 上一次回合、其他 queued followup 的终态或答案。
- 允许 additive `execution_id: Option<...>` 进入 parent 的 SubagentFinished、内部 terminal notification/collector request 等必要结构，均需缺省兼容。相关 canonical 校验、ledger 去重、graph/team/resource 投影同步更新。
- parent ledger 的去重键改为 `(child_session_id, execution_id)`，旧记录缺 execution_id 保持旧幂等规则；不同 execution 的 completed/failed/cancelled 可以共存。不要把多次完成判成同一条冲突记录。
- parent 的 spawn edge 只写一次；后续执行不能再伪造一次 spawn。SubagentFinished 的执行标识是 additive 关联，不删除既有父子边，不改写历史事实。
- 为旧数据缺 execution_id 的处理设明确规则：保留旧终态；新投递全部显式携带 execution_id；缺少可唯一证明的执行关联时返回具名错误，不猜测、不把旧结果套在新任务上。

### 2.2 followup 与 wait 的语义

- 同一 agent 串行执行；忙碌时 followup 进入有界队列，当前任务正常完成后自动运行。可以复用现有命令队列和配额，但不能用无界缓存替代。
- 不要求新建通用调度器；最小实现可为每个 child 维护有限的执行记录及待执行集合。每条记录有 execution_id、input_id、已绑定 turn_id、明确状态。
- 同一 target 的准入、active/pending 标记及卸载决定必须有串行化边界。发送失败要撤销临时状态，不能清掉仍有效的旧结果后假装新任务已启动。
- `list_agents.outcome` 表示最新已准入 execution 的结果：该次未终结时不能携带上一次 outcome。历史结果仍可从 canonical 重建。
- `wait_agent(targets=...)` 在调用开始时固定每个 target 最新已准入 execution。等待期间又来了 followup，不得让本次 wait 偷换目标或永久追赶“最新”。尚无任何 execution 的 root/self 等目标立即报明确错误，不浪费一小时等待不可能的结果。
- wait 必须等待捕获 execution 的终态并返回对应结果，不得因 agent 的历史 outcome 非空立即结束。
- collector 完成消息使用包含 child id 和 execution_id 的稳定 id，reply_to 指向该次 Trigger；重试 id 不变。
- 已开始取消的 execution 不把正文注入父会话，但要记录 cancelled 终态和可查询状态。timeout/failed 不得报告 exit_code=0；流断开、解引用失败不能冒充成功。
- timeout 从执行实际开始时计算，不消耗 queued followup 的等待时长；当前执行 timeout/interrupt 不得将队列中的所有新任务冒充完成。父级取消/关闭按既有子树规则显式取消受影响的 pending/active 项。
- 旧 execution 的 collector 不能覆盖最新结果、关闭正在处理/排队新 execution 的 actor 或清掉新 collector 标记；所有 finalize/unload 使用 execution/generation fencing。

### 2.3 恢复与配置

- 重载 child 必须恢复其持久化上下文，继续使用原 child id/path；禁止把它当新会话打开空 MessageStore。
- 首次 spawn 继承父会话**有效配置**，不是全局默认。subagent 的显式 model/base_url/max_tokens 等设置优先，其余端点/协议/profile 关联设置保持自洽；密钥仅从现有安全配置入口获得，不落 fact、不回传工具输出。
- child 重载时权限来自当前已加载父 actor 的有效会话权限快照，不回落全局高权限。明确区分 actor 生命周期 token 与单次 execution/turn token；取消上一回合后不能让新的 followup 天生处于已取消状态。
- 空工具 allowlist 的既有配置承诺是“全部工具”；自动补 skill 工具不得把空集意外变成“仅 skill”。非空 allowlist 仍只增加三个 skill 工具，不扩权。
- 重启时从 committed facts 重建身份、父子归属、每次 execution/结果；resident 一律 unloaded，不自动重跑已完成模型任务。
- 恢复入口在 daemon/service 初始化或显式 root attach/restore，不在每次 list/send 里悄悄迁移或重写数据；list/send 必须使用已恢复索引。

## 3. 实施工作包（第 1 轮必须全部完成）

### A. 真正接通答案与 blob

1. 在共用的模型响应成功接收/封口路径持久化 Answer blob，再追加 AssistantBlockSealed；包括无工具最终答案，不能只修 subagent 测试特例。
2. 给每个被持久化的 answer block 使用稳定唯一 block_id，关联 canonical turn_id。多个 block 的拼接顺序和最终答案选择需确定：取该 execution 最终模型轮的 Answer blocks，按事实顺序拼接，不回传 reasoning，不把早期工具轮的“我来查看”当最终答案。
3. AssistantBlockSealed 必须先于同 turn 的 TurnFinished 发布；写 blob/fact 失败要可观察地失败，禁止 success + 空答案掩盖错误。合法空答案有明确测试，与采集失败区分。
4. collector 通过 SessionBlobStore + canonical ContentRef 解析正文。正确处理 sha256 前缀、校验摘要；不读 TTL cache、不依赖 HTTP、不增加双存储兜底。
5. 如果 transport 的 download_content 已无其他合法用途，直接替换/删除错误路径，不能保留“先 canonical 失败再缓存猜测”。
6. parent 结果引用如需持久化，先把正文写入 parent 自己的 blob，再写 parent result_ref/communication；child 卸载和传输缓存释放后仍可读取。

### B. 统一安全点消息消费

1. 统一判定 legacy system 注入与显式 inter-agent Queue/Steer/Interject；完整复用正常分发的 validation、communication/input/terminal 落盘，不复制一套漏记事实的分支。
2. 下一个工具 lap 完成、下一次 provider request 之前合并到模型上下文；不得插进 assistant(tool_use) 与 tool_result 中间。
3. 同一安全点优先级保持 Interject > Steer > Queue，保留既有速率限制与待处理顺序；不得把 steer/interject 改成 cancel。
4. Queue/Steer/Interject 在 idle 不新建 turn；V2 结果也保持 queue-only。下一次用户/Trigger 启动的首个 provider request 就应看到已排队内容，不能等第一个工具轮之后才看到。
5. busy followup 的 Trigger 继续按串行队列处理，不当安全点注入。取消后结果不复活父回合。
6. 无 targets wait 在工具内部等待时，也能观察已提交的新 mailbox 通信；不能因为 actor 被 wait 占住而永远等不到落盘。最小推荐落点是工具结果收集/等待循环中的 owner-actor 安全泵：只处理消息准入与 canonical 持久化，不重入 provider、不在工具结果回填前合并模型上下文；wait worker 观察提交后的 watermark。也可复用已有同等能力，但不允许另起 writer、工具线程争抢 lease 或把未持久化的内存入队冒充 mailbox activity。已有未合并上下文的消息应立即返回 activity；InputAccepted 只代表准入，不代表模型已经看见，不能用 pending_count==0 误判。若需要消费水位，仅作可从既有事实/上下文重建的显式运行态，不新增旁路权威日志。

### C. 唯一 resolver 与等待快照

1. 提供一个公共内部解析路径：先在 caller 所属 root 内匹配 canonical agent id，再按 caller AgentPath 解析绝对/相对路径；不跨 root、namespace，不支持 `..`。
2. list prefix 按 prefix 语义，不要把它误改成 exact-target resolver；send/followup/steer/interject/interrupt/wait 共享 exact resolver。
3. wait 解析一次得到 canonical target id、绝对 path、execution_id；返回同一组快照。禁止再用未归一化字符串 filter_map 后静默丢目标。
4. 未知目标明确报错，重复目标按 canonical id 去重，读取失败返回错误，不能 unwrap_or_default 变成“0 个目标已完成”。
5. 保留现有公开 timeout_ms 参数和运行时 1000..=3600000 边界，schema 的 minimum/maximum 与实现一致；timeout 返回对应执行的当前快照。

### D. 可靠复用与执行终态

按 §2.1/2.2 实现完整多次 execution 路径；此项与 A/C/G 一起改，不能留“下一轮支持 followup”。

- 所有 collector event 保留 turn/input 关联，订阅先于 Trigger 准入/发布，不能漏掉快速执行。
- 去掉仅 session id 的结果去重键；ledger 和投影都支持多 execution。
- 列表状态优先看最新执行：queued/pending 不冒充 completed，running 不附旧答案，终态与 outcome 对应同一 execution。不要求新增公开 status enum，用现有值作一致映射并写测试。
- 旧 finalize 只能完成自己的执行；重复 terminal 幂等；不允许两次 parent 注入。
- 队列限额报错明确，不能隐式丢 Trigger；命令已进入队列不等于 canonical 已持久化，report/恢复不能混淆两者。公开成功语义若是 queued，返回 queued 而不是声称模型已开始。

### E. 上下文、权限、工具可达性

修复 §2.3 的上下文/配置/权限/allowlist，保留默认 read+exec+skills。

- 不把通信、task/board 或 spawn 默认授予 child；测试区分“已注册”“已暴露”“获权限准入”“真正执行”。
- Exec/Net 在需用户审批的子代理档位仍拒绝，MCP 仍 host-only，不通过切 SkipPermissions 让测试通过。
- 有限 allowlist 精确生效；空 allowlist 保持全部可用的配置语义，但不绕过权限层。

### F. 回滚、订阅与卸载

1. child actor 创建后到 canonical spawn edge 提交前的任何失败均统一 rollback，包括路径、ledger 获取、lease、append、取消及父会话不匹配。用 guard/单一收尾路径，不逐处手工漏补。
2. 释放本次取得的 quota reservation、collector/执行临时标记、catalog/residency 与 actor；不删除已有有效会话或伪造 canonical spawn。
3. 提交后启动任务失败：持久化对应 failed terminal，再释放 actor；若 terminal 持久化也失败，应明确上报并进入可恢复状态，不声称完全收尾。
4. 为每个订阅设置显式所有权/取消通道；collector drop、terminal、父级关闭、订阅 Closed/Reset 都结束桥线程并可 join。Closed 与暂时 Empty 必须可区分。
5. 不保留 detached 轮询线程；不依赖“下一个事件恰好到来”发现 receiver 已 drop。事件滞后/Reset 从 committed cursor 重读或返回明确 failure，不吞掉结果。
6. registry 生命周期变为两阶段：锁内提取/标记实例和 generation，锁外 signal/send/IO/join，锁内核对 generation 后完成登记；避免 close 与 actor 工具回调互相等锁。
7. wait_until_stopped 超时后不得无条件进入无限 join 并冻结 registry。使用明确的 stopping/reap 状态，保留未停止句柄，不冒充已 join、不重新拉起同一实例；晚到的停止由已有维护入口回收。不新增后台轮询框架。
8. close、父树取消、死 actor 重启都保持父子关系/执行状态一致；respawn 不能丢 supervisor link 或放大执行次数。

### G. 接通恢复，不止写 scanner

1. 从 canonical root graph 重建 catalog、路径、父子归属、最近执行/结果；不只 register root。
2. root 启动/attach 的恢复工作必须在对协作工具开放前完成；IO 在 registry 锁外进行，再原子安装结果。
3. 已有子终态但 parent 缺完成记录：幂等补 parent terminal，并从 durable answer blob 恢复结果；重启两次不重复注入/执行。
4. 崩溃时仍 running 的执行：明确追加 interrupted/failed 恢复终态，不自动重发不可证明幂等的模型任务；pending 数据按实际 committed 接受记录恢复，不凭进程内临时状态杜撰任务。
5. orphan child 没有 committed spawn edge 时不得纳入正常 roster 或自动启动；作可观察的诊断/既有 tombstone 处理。edge 缺 child log、parent mismatch/cycle 明确报完整性错误，不伪造边。
6. 不直接执行 scanner 里“缺 TurnStarted 就补一个”的建议来伪造已经运行；按本节的执行事实规则修 scanner 及 executor。恢复行为必须有实际 service 测试。

### H. 18 工具契约验收与文档

- 覆盖 spawn/list/send/followup/steer/interject/wait/interrupt，以及 5 个 task 和 5 个 board 工具。
- 不要求重写 task/board；通过真实 host 证明现有业务迁移和持久化，补条件校验测试即可。
- task_claim release 的 reason：描述已声明必填，运行时也必须非空；不能默默填 released。action 条件所需字段仍由运行时明确校验；不为此引入 provider 不兼容的 oneOf/anyOf。
- task_update 的 artifact/result/description 引用不能仅验证 64 hex 形状后接受悬空引用：使用已有 canonical blob 能力验证可解析性，跨 session 工件先复制到明确 owner（推荐 root session）再引用。不要新增 blob 上传工具或允许裸哈希代表已存在内容。外部不存在的引用返回明确错误。
- task_close 的 complete/close/cancel 语义、依赖环/所有者/epoch 校验不削弱；task_list 非法状态明确拒绝。
- board_post 字节上限 16 KiB；board_list post_limit 1..=200；非法 topic/thread/task 关联报错；通知失败不回滚已经成功持久化的帖子，返回 notified/skipped。
- schema 描述、数字限制、默认值与运行时一致；保留 deny_unknown_fields。spawn timeout 缺省取设置（出厂 120），显式越界明确报错而非静默 clamp；名字缺省 sub，重复名字明确报错而非覆盖。
- 默认工具集的限制必须在交付报告说明；不要把权限拒绝算成 handler 不存在，也不能把 handler 注册算成子代理可执行。

## 4. 验收矩阵：必须有可失败的行为测试

优先复用 tiny_http 本地 provider fixture、现有 test hooks、临时 SessionManager 和私有 host/collector 实例；没有网络不等于可以 mock 掉被修复的接缝。不得 mock 正文下载为直接返回答案来代替 A 的集成测试。

| ID | 输入/操作 | 必须观察的结果 |
| --- | --- | --- |
| T01 | 本地 provider 产出无工具最终答案 | 真生产 parse 写可解析 Answer fact；wait/list/parent 收到逐字相同正文；terminal 顺序在后 |
| T02 | 多 Answer blocks、Unicode、>64 KiB 正文 | 最终轮正文完整有序，reasoning 不泄漏；释放传输缓存/重开服务后仍能解析 |
| T03 | 缺失/损坏 blob、订阅断开或 Reset | 明确失败或从 cursor 恢复；不返回成功空答案/exit 0 |
| T04 | target 正运行，两个工具 lap 间发送 queue/steer/interject | 下一次 provider request 已含消息；顺序与限额正确；tool_use/result 相邻契约不破坏 |
| T05 | idle target 收 queue/steer/interject，再发用户/Trigger | 原先不新开 turn；首个 request 即可见消息；不延迟到首轮工具之后 |
| T06 | actor 工具中 wait(mailbox)，另一线程投递消息；以及 wait 前已有未消费消息 | 两者均返回 activity；没有 writer 竞争或死锁 |
| T07 | path/id/相对路径指向同 agent，含重复目标 | wait/list 返回同一正确目标和结果；未知/跨 root/namespace 报错；绝非成功空数组 |
| T08 | 第一次 completed，followup blocked，随后 failed，再 followup completed | wait 不返回旧结果；每次 execution 单独终态；第三次成功且没有冲突/漏正文 |
| T09 | busy 时连续 followup、队列满、发送失败 | 顺序执行，重复 Trigger 幂等，超限可恢复错误，失败无残留临时状态 |
| T10 | 旧 collector finalize 与新 followup 并发；wait 期间又发 followup | 不误关闭/覆盖；wait 只等待调用时捕获 execution |
| T11 | 完成/interrupt 后 child 卸载再 followup | 同 id/path；provider request 保留之前任务消息，权限继承父快照、取消状态正确 |
| T12 | 父 session 非全局 profile/权限；child 指定部分覆盖 | endpoint/wire/配置继承一致，reload 不升权，序列化无密钥；empty/nonempty allowlist 正确 |
| T13 | spawn 的路径/ledger/lease/append/start 各故障点 | 无未登记活 actor/未释放 reservation；已提交边有可观察终态，未提交边不伪造 |
| T14 | 正常完成、cancel、timeout、parent close、多次订阅 drop | 订阅有确定停止信号且可 join；不靠长 sleep 猜线程消失；registry 可继续服务其他 session |
| T15 | worker 卡停止、actor 工具并发取 registry、死 parent 重启 | close 不持锁 join、不无限占 registry；未停句柄可追踪；父子关系不丢 |
| T16 | 成功执行并关闭 service，再建新 service | 无手工注入 metadata：list/id 寻址/历史结果恢复；parent attach 后可 followup；不自动重跑 |
| T17 | child terminal 后 parent 未落终态即崩溃，连续恢复两次 | 幂等补事实和结果；不重复消息/任务；running/orphan/缺 log 按 §G 处理 |
| T18 | 任务板真实 host create/claim/release/update/complete/close，重开 store | 状态/owner/epoch/依赖持久化；非法转换、环、缺字段、悬空 content ref 明确拒绝 |
| T19 | 消息板 host 创建/订阅/发帖/过滤/取消订阅，重开 store | 内容持久化；16KiB 和分页边界；不唤醒 idle；运行中通知遵循 T04 |
| T20 | 18 schema 的真实注册+typed 调用 | required/未知字段/范围/默认值一致；受限 child 的未暴露和权限拒绝可区分 |

T01/T04/T08/T16 是不可替代的四个端到端闸口。只有 helper 单测通过、手工 publish fixture 或源码 grep 通过，均不能代替。

新测试不要更改进程全局环境变量，不依赖全局 install_host 的跨测试重装，不要求新增测试必须 --test-threads=1 才通过；通过已有显式上下文/依赖注入或很小的 test hook 构造接缝。本地 provider fixture 必须显式配置测试用端点、无真实密钥。

## 5. 实施顺序与迭代纪律

第 1 轮内部顺序固定：

1. 定义执行关联和恢复折叠的最小数据结构，先补 T01/T04/T08/T16 红测试（编译错误不是红测试证据）。
2. A+B+C 接通生产者、消费者、唯一 resolver；D+E 实现多次执行、恢复上下文/有效配置。
3. F+G 接通回滚/回收/恢复；H 验证协作工具可达性并修小型契约漂移。
4. 跑全矩阵和既有回归，自行修复后一次性交付，不能每写一个 helper 就等用户确认。

不允许为了“3 轮”降低验收。第 1 轮发现范围外问题只记入报告；本文已指定的缺陷不能挪到未来路线图。若确有无法解决的冻结协议冲突或环境阻塞，精确列出受阻条款、失败证据、已完成部分，状态写未完成。

第 2 轮输入是审阅清单：只修清单及其直接回归。第 3 轮：最终验证、兼容/并发/数据恢复收口，不再另开一轮重新设计。

## 6. 验证命令与完成定义

在仓库根运行，先看已有进程避免同时抢 build lock：

```bash
cargo fmt --all -- --check
cargo check -p qaqh-runtime --lib
cargo test -p qaqh-tool-core --lib
cargo test -p qaqh-subagent --lib
cargo test -p qaqh-session --test team_task_board --test team_projection --test message_board --test tool_ledger
cargo test -p qaqh-runtime --lib --test host_direct --test subagent_inprocess --test tool_surface_schema_shape --test tool_ordering_contract
# 若新建独立集成测试，补充其准确命令，不只给出上面的旧测试。
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace -- --test-threads=1
```

离线依赖齐全时可加 --offline；缺依赖时先报告缺什么。DNS 不通不得重复盲目联网。任何因依赖/沙箱/工具超时未跑完的命令都写“未验证”，不能以 cargo check 代替集成测试通过。

完成需同时满足：A～H、T01～T20，既有相关测试无回归；不存在未披露的必做项；协议/旧记录兼容测试通过。若仓库级命令有基线失败，附错误位置及与本次无关的证据，审阅者决定是否接受，不由实现模型自行宣布全绿。

交付时新增/更新 `docs/report-subagent-lifecycle-bugfix.md`，使用以下结构：

1. 结论：完成 / 未完成；不要写“基本完成”。
2. A～H 与 T01～T20 对照：实现符号、测试名、真实运行结果。
3. 四个端到端闸口的红→绿证据；无法取得红证据要明示，不能编造。
4. execution 去重键、collector 过滤、wait 捕获语义、恢复入口、close 锁边界的简述。
5. schema/持久记录 additive 字段与缺省兼容规则；没有新路由/秘密持久化/旁路权威。
6. 所有验证命令的退出码；未跑、被中断、基线 warning/失败分开列。
7. 剩余风险与精确阻塞；任何未完成 MUST 都必须显眼标注。

不要更改本 spec 的验收条款来迁就实现；需要建议变更时仅在报告列出，由审阅者裁决。
