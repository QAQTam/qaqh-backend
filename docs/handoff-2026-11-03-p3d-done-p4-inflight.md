# Handoff — 2026-11-03:P3-D 已提交(78b77876),P4 刚起步

## 0. 使命与standing directives(不可变)

- **目标(goal-mode)**:"按照既定计划以及文档把全部工作安排妥当并且写测试;且时刻注意硬件资源占用问题后,帮我直接完成全部的工作"——按 docs/report-01/02/03 三份报告完成整个架构简化程序,写测试,自主推进。
- **用户既定决策**:旧数据不要了(无迁移);alpha 不做确定性兼容;你直接全改,前端我自然有办法。
- **宪法(AGENTS.md)要点**:#7 开工查 disk/mem/cpu 并预告卡顿;#8 工具失败不得声称完工;#9 RingingV2 协议与命令面是最高准则,禁止转换层懒政;#10 不许每改一次跑全仓测试(磁盘);#11 `rg -r` == `--replace` 陷阱;#12 sed 改完必须 rg 验证;#13 用 todo 工具;#14 新库用 find-docs。
- **资源现状(本次开工时)**:磁盘 86G 空闲 / 内存可用 ~4.5G / 8 核。cargo check --workspace --tests 约 2-4 分钟,可行;**不要跑 `cargo test --workspace`**(用户两次拒绝过),只跑逐 crate 定向测试。

## 1. 已完成(全部已提交,按序)

| commit | 内容 |
|---|---|
| 2685453d | RingingV2 契约独立 + SQLite 存储基座 |
| ec744693 | SessionState 业务表写读往返 |
| 7a87f41a | 会话 owner 接管工具生命周期 + E09/E10 + effects 消费 |
| 439c423f | owner 驱动的新回合循环(证明链外) |
| 3e05a105 | registry 切 SessionLoop,SSE/状态单权威落 owner |
| **78b77876** | **P3-D:旧存储/证明链/投影面全量退役(140 文件,-34342 行)** |

P3-D 具体内容(78b77876):
- 删除:qaqh-agentloop-core 整 crate、qaqh-session canonical/projection/session_commit/commit_state/context_projection/meta/store/wire/team 旧存储、ringing-runtime v2.rs/agent_graph.rs、agent-runtime activity.rs/liveness.rs/session_agent 旧文件。
- SQLite 单权威:sessions 表补 `model_selection`/`custom_tools_json`/`skills_json`/`frozen_annotation` 列(schema v2,SCHEMA_VERSION=2,旧库直接删);SessionRow/SessionListRow 全列 + turn_count;`session_row_with_turn_count()` 供 SessionMeta 组装。
- 创建流:`handlers/session.rs` 的 `create_business_row()`(CreateRowInput)一次落库,无 meta.json 中间态;`SessionManager::allocate_session()` 无参返回 `Result<String,String>`(`ses_{ulid}`);materialize_canonical_session* 全删。
- cwd 读取唯一口:`registry::spawn::session_cwd(owners, session_id) -> Option<String>`(read_blocking + session_row);`ensure_session_workspace(owners, ...)`;fs_git/plan/stats/service.list_sessions 全切 owners。
- stats::activity 直查 tool_calls 终态表(status: finished/failed/cancelled/backgrounded)。
- registry 不再持有 SessionManager(`AgentRegistry::new(owners)`);OwnerDirectory 有 `set_ephemeral()`。
- pending_store 删掉 causation index/error_code_of(回执终态在派发时直写)。
- contract-export 切契约类型(SessionBootstrap/SessionState/SessionEvent/SessionEventFrame),index.ts 无泛型别名层;fixture 名 `event-frame-interaction.json`。
- qaqh-client 导出 `contract_events`/`contract_state`(v2.rs);ClientV2Event = SessionEventFrame,cursor = `"epoch:seq"` 字符串。
- 集成测试:remote_fs_allowlist 种子改走业务行(service.owners().db().write_blocking + store::create_session);v2_public_api 匹配 SessionEvent 变体;删 tool_crash_recovery/v2_acceptance_matrix/e2e_seed。
- **验证状态:全仓 `cargo check --workspace --tests` 0 错误 0 警告;定向测试全绿**:qaqh-session 12+36(db)、agent-runtime 36、ringing-runtime 9、runtime lib 49 + 集成 13、client 68、contract-export 6、daemon 31、types/ringing/domain/workspace ~240。

## 2. 当前进行到哪(P4 刚开工,0 行代码改动)

P4 = report-03 执行:**三命令入口统一 + 四决策**。刚读完 report-03 §4-8,关键结论:

1. **DomainCommand 删除**(report-03 §8.1):根枚举在 qaqh-domain/src/command.rs,全仓无生产者/消费者,只有定义/channel()/From 实现/导出。直接删 + 删导出 + 删 ringing 的 `From<DomainCommand>` + 改过时分层注释。
2. **三入口现状**:① service 链(method+JSON → Command::decode → SessionCommand → 再包 RingingCommand);② Unified 链(daemon axum_impl/unified.rs 直调 typed handler);③ 频道链(RingingCommand 经信封)。**创建会话有 4 个表示**(ControlCommand::SessionCreate / session.new+SessionNewArgs / SessionCreateParams+独立实现 / UnifiedCommand::SessionCreate)→ 合并为一个实现+一个返回 DTO。注意 `create_session_unified()` 仍重装 SessionNewArgs 调 create_new(),SessionCommand::SessionCreate 是另一份实现(现在两者都已重写为 create_business_row,合并点已就绪)。
3. **四决策**(report-03 行 155):
   - `PublishResourceChanged` 退出 UI 命令目录(**摘要称已做,需验证**——在 qaqh-domain/ringing 里 rg 确认是否仍在公共命令枚举);
   - 全局 ops(provider/配置/生命周期)由各自 handler 处理,不进会话 actor;
   - skills/settings 的 revision CAS:SDK 声称 expected_revision 但 SkillsOperationArgs 丢弃它 → 在统一契约定义一次、owner 写边界执行;
   - DomainCommand 删除。
4. **字段缩减违规**(report-03 §5.2):registry messaging.rs ConversationSendMessage 分支只取 text/attachments,丢弃 message_id(input_id 已传)、input_purpose(已映射)、as_system(已传)、**inter_agent、subagent_terminal**。qaqh-subagent/src/lib.rs:1464 填写了 QueueOnly+inter_agent+终态通知——需完整到达 SessionLoop 或明确删除。**现状核对:messaging.rs 已映射 purpose/as_system/attachments/message_id,但 inter_agent 和 subagent_terminal 仍被 `..` 忽略**。
5. **声明无实现的缺口**(report-03 §5.4):METHODS 有 provider.upsert/delete 但 decode 不收 provider 前缀;UnifiedCommand::ProviderModelsDiscover 返回 unsupported;ToolInvoke 在公共命令但 registry 无分支;UnifiedPrecondition::SettingsRevision 契约里有但 dispatcher 不消费。→ 有需求则实现,没需求从目录删除。
6. **完成标准**(§8):同一操作一份字段真源+一个实现;不能 `..` 省略宣称支持;创建返回值/归属确定;revision 在写边界检查;导出不同时保留两代命令目录。

## 3. 下一步(按序执行)

1. **P4-a(小)**:删 DomainCommand(qaqh-domain/src/command.rs 根枚举 + channel() + qaqh-ringing From + lib 导出 + 过时注释)。rg 确认 PublishResourceChanged 是否仍在 UI 目录,在则退出。
2. **P4-b**:定唯一命令目录。现实落点:以 `UnifiedCommand`(qaqh-ringing,UnifiedCommandEnvelope+UnifiedTarget+UnifiedPrecondition)为唯一业务根,把 RingingCommand 会话内业务 payload 并入;service decoder(SERVICE_METHODS + Command::decode)与 SDK ActionRequest 同义变体删除,消费者直用契约类型。全局 ops(provider/config/lifecycle)独立 handler。**注意 report-03 §6.1:不要新建第三个命令 crate**。
3. **P4-c**:创建合并——一个创建实现(create_business_row 已是),一个返回 DTO(session_id + 归属/lease 行为要定);三入口全部指向它。
4. **P4-d**:registry messaging 去字段缩减——SessionLoop 直接消费契约 payload;inter_agent/subagent_terminal 补齐或按报告声明删除;QueueInput lane 按 input_purpose 选。
5. **P4-e**:skills/settings revision CAS 到 owner 写边界;§5.4 缺口逐项实现或删声明。
6. **P5 收尾**:stale 注释/死代码清理(rg `session_fact_v2` 旧类型残留、`// 投影`、activity 事实名匹配等),`cargo check --workspace --tests` 清零,定向测试组(同 §1 列表),commit。
7. 每步做完跑受影响 crate 的 check+test,**不跑全仓测试**。

## 4. 关键 API 速查(本会话固化的事实)

- `qaqh_session::db::store`:`NewSession`(含 model_selection/custom_tools/skills/frozen_annotation/ephemeral)、`create_session(conn,&NewSession)`、`session_row(conn,id)->Option<SessionRow>`、`session_row_with_turn_count(conn,id)->Option<(SessionRow,i64)>`、`list_sessions(conn,include_archived,limit)->Vec<SessionListRow>`、`SessionColumnPatch`(model_selection: Option<SettingValue<ModelSelection>>,Use 落 JSON/Inherit 置 NULL)、`update_session_columns(conn:&mut Connection,...)`。
- `OwnerDirectory`:`create_session(NewSession) async`、`ensure_session(id,parent,ephemeral)` sync、`set_ephemeral(id,bool)`、`session_parent/children/run_state`、`db()->&Arc<Db>`;`Db::write/read(async)+write_blocking/read_blocking(sync)`;read_blocking 闭包返回 rusqlite::Result(Ok 类型即返回 T,不加嵌套 Result)。
- `SessionLoop::new("SES-ID", owner, port, ProviderRequestSpec, workspace_root)`;`send_input(LoopInput{input_id,text,purpose,as_system,attachments})`、`cancel_turn(Option<&str>)`、`respond_interaction/respond_permission/abandon/reload_provider`。
- 测试造会话:`SessionManager::new_for_test(dir, active)`(现在会自建目录)+ `allocate_session()` + `owners.db().write_blocking(create_session)` 或 owners.create_session(...).await。
- 会话 owner 泵/广播:`owner.subscribe() -> broadcast<SessionEventFrame>`;SSE cursor `"epoch:seq"`;gap→resync_required。
- 契约:qaqh-ringing-contract{commands,events,fact,state,team,ulid};`generate_ulid()/causation_for_command()` 在 contract::ulid;SessionEvent `#[serde(tag="type", rename_all="snake_case")]`;SessionEventFrame{seq, event(flatten)};ContentValue tag=kind/content=data。
- Id 类型构造:`SessionId::new(text)`、`InteractionId::new(text)`、`AgentPath::parse_absolute(text)`、`MessageId::new(text)`。

## 5. 陷阱(踩过的)

- rusqlite 多闭包共享 `&mut values/sets` 会 E0499——要么一个闭包包揽,要么 if-let 内联;`session_meta.rs` 读取链用 `read_blocking(...).ok()? ?;`(store helper 已消除类型标注需求)。
- `qaqh-runtime` 无 rusqlite 直接依赖——闭包里别写 `rusqlite::Error` 字面类型,用 helper fn 或 `_`。
- write_blocking 闭包是 `move`,&str 捕获要在闭包外 `.to_string()`。
- `Option<Result<Option<T>>>.transpose().map(...).flatten()` 处理 Use/Inherit 双层 Option。
- schema v2 与 store.rs 列必须同步加(本次 model_selection 差点只加了一半)。
- 用户拒绝全仓 cargo test;check --workspace --tests 可以。

## 6. 环境注意

- 工作目录 /home/qaqtamsy/Projects/qaqh-backend-v3,分支 v3,remote 无(本地提交)。
- 本会话上下文已耗尽额度,新会话直接从 §3 P4-a 开干,先 `git log --oneline -6` 确认 78b77876 在顶。
